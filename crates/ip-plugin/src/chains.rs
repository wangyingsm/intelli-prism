use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use ip_auth::Identity;
use ip_core::{ApiId, Checksum, PluginKind, PluginRule, PluginScope, TenantId};
use ip_gateway::{ChainSource, ProcessorChain};
use ip_storage::{PluginRuleStore, PluginStore};

use crate::adapter::{WasmBodyProcessor, WasmHeaderProcessor};
use crate::error::ChainError;
use crate::host::PluginHost;

/// A loaded plugin, wrapped for the one chain its rule places it in.
enum Slot {
    RequestHeader(Arc<WasmHeaderProcessor>),
    RequestBody(Arc<WasmBodyProcessor>),
    ResponseHeader(Arc<WasmHeaderProcessor>),
    ResponseBody(Arc<WasmBodyProcessor>),
    ResponseChunk(Arc<WasmBodyProcessor>),
}

/// One rule in force, with the processor that carries it out.
struct Placed {
    rule: PluginRule,
    slot: Slot,
}

impl Placed {
    fn new(rule: PluginRule, host: &Arc<PluginHost>) -> Self {
        let checksum = *rule.checksum();
        let order = rule.order();
        let header = || Arc::new(WasmHeaderProcessor::new(Arc::clone(host), checksum, order));
        let body = || Arc::new(WasmBodyProcessor::new(Arc::clone(host), checksum, order));
        let slot = match rule.kind() {
            PluginKind::ReqHeader => Slot::RequestHeader(header()),
            PluginKind::ReqBody => Slot::RequestBody(body()),
            PluginKind::RespHeader => Slot::ResponseHeader(header()),
            PluginKind::RespBody => Slot::ResponseBody(body()),
            PluginKind::RespChunk => Slot::ResponseChunk(body()),
        };
        Self { rule, slot }
    }

    fn join(&self, chain: ProcessorChain) -> ProcessorChain {
        match &self.slot {
            Slot::RequestHeader(processor) => chain.with_request_header(processor.clone()),
            Slot::RequestBody(processor) => chain.with_request_body(processor.clone()),
            Slot::ResponseHeader(processor) => chain.with_response_header(processor.clone()),
            Slot::ResponseBody(processor) => chain.with_response_body(processor.clone()),
            Slot::ResponseChunk(processor) => chain.with_response_chunk(processor.clone()),
        }
    }
}

/// Every plugin rule in force, built once at startup, so picking a request's chains is a
/// filter over lists already in memory and never loads or compiles anything.
pub struct PluginChains {
    global: Vec<Placed>,
    tenants: HashMap<TenantId, Vec<Placed>>,
}

impl PluginChains {
    /// Compiles every plugin a rule names, once however many rules name it, and files the
    /// rules by tenant. A global plugin that will not load stops startup; a tenant plugin
    /// that will not load is logged and left out, together with its rules.
    pub async fn load(
        host: Arc<PluginHost>,
        plugins: &dyn PluginStore,
        rules: &dyn PluginRuleStore,
    ) -> Result<Self, ChainError> {
        Self::from_rules(host, plugins, rules.rules().await?).await
    }

    /// Builds the chains from rules already in hand, as a reload does when it has them from
    /// the cache rather than from the database.
    pub async fn from_rules(
        host: Arc<PluginHost>,
        plugins: &dyn PluginStore,
        rules: Vec<PluginRule>,
    ) -> Result<Self, ChainError> {
        let mut by_plugin: BTreeMap<Checksum, Vec<PluginRule>> = BTreeMap::new();
        for rule in rules {
            by_plugin.entry(*rule.checksum()).or_default().push(rule);
        }
        let mut chains = Self {
            global: Vec::new(),
            tenants: HashMap::new(),
        };
        for (checksum, placed) in by_plugin {
            if let Some(detail) = load_failure(&host, plugins, &checksum).await? {
                if placed
                    .iter()
                    .any(|rule| rule.scope() == &PluginScope::Global)
                {
                    return Err(ChainError::GlobalPlugin { checksum, detail });
                }
                for rule in &placed {
                    let tenant = rule
                        .scope()
                        .tenant()
                        .map_or(String::new(), ToString::to_string);
                    tracing::error!(%checksum, %tenant, %detail, "left out a tenant plugin that will not load");
                }
                continue;
            }
            for rule in placed {
                let entry = Placed::new(rule, &host);
                match entry.rule.scope().tenant().cloned() {
                    Some(tenant) => chains.tenants.entry(tenant).or_default().push(entry),
                    None => chains.global.push(entry),
                }
            }
        }
        Ok(chains)
    }

    /// How many rules are in force.
    pub fn len(&self) -> usize {
        self.global.len() + self.tenants.values().map(Vec::len).sum::<usize>()
    }

    /// Whether no rule is in force.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Reads a plugin's wasm and compiles it, handing back why it will not load, if it will not.
async fn load_failure(
    host: &PluginHost,
    plugins: &dyn PluginStore,
    checksum: &Checksum,
) -> Result<Option<String>, ChainError> {
    let Some(plugin) = plugins.plugin(checksum).await? else {
        return Ok(Some("it is not in the plugin store".to_owned()));
    };
    Ok(host
        .load(checksum, &plugin.wasm)
        .err()
        .map(|error| error.to_string()))
}

impl ChainSource for PluginChains {
    fn chains_for(&self, identity: &Identity, api: &ApiId) -> Arc<ProcessorChain> {
        let tenant_rules = self
            .tenants
            .get(&identity.tenant)
            .map_or(&[][..], Vec::as_slice);
        let chain = self
            .global
            .iter()
            .chain(tenant_rules)
            .filter(|placed| {
                placed
                    .rule
                    .applies_to(&identity.tenant, &identity.user, api)
            })
            .fold(ProcessorChain::new(), |chain, placed| placed.join(chain));
        Arc::new(chain)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use ip_core::{NewPluginRule, PassphraseHash, PluginOrder, Role, TnKey, UserId};
    use ip_storage::{
        AccountKind, NewPlugin, NewTenant, NewUser, PluginOwner, SqliteStore, TenantStore,
        UserStore,
    };

    use super::*;
    use crate::testing::*;

    /// A module that compiles but keeps to no abi, so it will never load.
    const BROKEN: &str = r#"(module (memory (export "memory") 1))"#;

    async fn store() -> SqliteStore {
        let store = SqliteStore::in_memory().await.unwrap();
        for tenant in ["acme", "globex"] {
            store
                .create_tenant(NewTenant {
                    id: TenantId::new(tenant).unwrap(),
                    key: TnKey::generate().unwrap(),
                })
                .await
                .unwrap();
        }
        for user in ["alice", "bob"] {
            store
                .create_user(NewUser {
                    id: UserId::new(user).unwrap(),
                    passphrase: PassphraseHash::new("$argon2id$v=19$m=8,t=1,p=1$c2FsdA$aGFzaA")
                        .unwrap(),
                    kind: AccountKind::Regular,
                })
                .await
                .unwrap();
        }
        store
    }

    /// The `!`-appending guest, made distinct by a marker so each stores as its own plugin.
    fn distinct(marker: &str) -> String {
        guest(&format!(r#"{BANG} (data (i32.const 8) "{marker}")"#))
    }

    /// Stores wasm for the global chain and both tenants, so any test can place it anywhere.
    async fn stored(store: &SqliteStore, kind: PluginKind, text: &str) -> Checksum {
        let owners = [
            PluginOwner::Global,
            PluginOwner::Tenant(TenantId::new("acme").unwrap()),
            PluginOwner::Tenant(TenantId::new("globex").unwrap()),
        ];
        let mut checksum = None;
        for owner in owners {
            let record = store
                .put_plugin(NewPlugin {
                    kind,
                    wasm: wat::parse_str(text).unwrap(),
                    owner,
                })
                .await
                .unwrap();
            checksum = Some(record.checksum);
        }
        checksum.expect("stored for at least one owner")
    }

    async fn place(store: &SqliteStore, checksum: Checksum, order: u8, scope: PluginScope) {
        store
            .put_rule(NewPluginRule::new(checksum, PluginOrder::new(order), scope).unwrap())
            .await
            .unwrap();
    }

    fn tenant_wide(tenant: &str) -> PluginScope {
        PluginScope::Tenant {
            tenant: TenantId::new(tenant).unwrap(),
            user: None,
            api: None,
        }
    }

    fn identity(user: &str, tenant: &str) -> Identity {
        Identity {
            user: UserId::new(user).unwrap(),
            tenant: TenantId::new(tenant).unwrap(),
            role: Role::Member,
        }
    }

    fn anthropic() -> ApiId {
        ApiId::new("anthropic").unwrap()
    }

    async fn load_from(store: &SqliteStore) -> (Arc<PluginHost>, Result<PluginChains, ChainError>) {
        let host = Arc::new(host());
        let chains = PluginChains::load(Arc::clone(&host), store, store).await;
        (host, chains)
    }

    #[tokio::test]
    async fn a_tenant_rule_joins_only_its_tenants_chains() {
        let store = store().await;
        let plugin = stored(&store, PluginKind::RespBody, &distinct("a")).await;
        place(&store, plugin, 100, tenant_wide("acme")).await;
        let (_, chains) = load_from(&store).await;
        let chains = chains.unwrap();
        assert_eq!(
            chains
                .chains_for(&identity("alice", "acme"), &anthropic())
                .response_body()
                .len(),
            1
        );
        assert!(
            chains
                .chains_for(&identity("alice", "globex"), &anthropic())
                .response_body()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_global_rule_joins_every_chain() {
        let store = store().await;
        let plugin = stored(&store, PluginKind::ReqBody, &distinct("g")).await;
        place(&store, plugin, 10, PluginScope::Global).await;
        let (_, chains) = load_from(&store).await;
        let chains = chains.unwrap();
        for tenant in ["acme", "globex"] {
            assert_eq!(
                chains
                    .chains_for(&identity("bob", tenant), &anthropic())
                    .request_body()
                    .len(),
                1,
                "{tenant}"
            );
        }
    }

    #[tokio::test]
    async fn a_rule_narrowed_to_a_user_or_an_api_only_joins_matching_chains() {
        let store = store().await;
        let for_bob = stored(&store, PluginKind::ReqBody, &distinct("u")).await;
        let for_internal = stored(&store, PluginKind::ReqBody, &distinct("i")).await;
        place(
            &store,
            for_bob,
            100,
            PluginScope::Tenant {
                tenant: TenantId::new("acme").unwrap(),
                user: Some(UserId::new("bob").unwrap()),
                api: None,
            },
        )
        .await;
        place(
            &store,
            for_internal,
            101,
            PluginScope::Tenant {
                tenant: TenantId::new("acme").unwrap(),
                user: None,
                api: Some(ApiId::new("internal").unwrap()),
            },
        )
        .await;
        let (_, chains) = load_from(&store).await;
        let chains = chains.unwrap();
        assert!(
            chains
                .chains_for(&identity("alice", "acme"), &anthropic())
                .request_body()
                .is_empty()
        );
        assert_eq!(
            chains
                .chains_for(&identity("bob", "acme"), &anthropic())
                .request_body()
                .len(),
            1
        );
        assert_eq!(
            chains
                .chains_for(&identity("alice", "acme"), &ApiId::new("internal").unwrap())
                .request_body()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn each_kind_lands_in_its_own_chain() {
        let store = store().await;
        for (order, kind) in [
            PluginKind::ReqHeader,
            PluginKind::ReqBody,
            PluginKind::RespHeader,
            PluginKind::RespBody,
            PluginKind::RespChunk,
        ]
        .into_iter()
        .enumerate()
        {
            let plugin = stored(&store, kind, &distinct(kind.name())).await;
            place(&store, plugin, 100 + order as u8, tenant_wide("acme")).await;
        }
        let (_, chains) = load_from(&store).await;
        let chain = chains
            .unwrap()
            .chains_for(&identity("alice", "acme"), &anthropic());
        assert_eq!(chain.request_headers().len(), 1);
        assert_eq!(chain.request_body().len(), 1);
        assert_eq!(chain.response_headers().len(), 1);
        assert_eq!(chain.response_body().len(), 1);
        assert_eq!(chain.response_chunk().len(), 1);
    }

    #[tokio::test]
    async fn a_tenant_plugin_that_will_not_load_is_left_out() {
        let store = store().await;
        let broken = stored(&store, PluginKind::ReqBody, BROKEN).await;
        let working = stored(&store, PluginKind::RespBody, &distinct("w")).await;
        place(&store, broken, 100, tenant_wide("acme")).await;
        place(&store, working, 101, tenant_wide("acme")).await;
        let (_, chains) = load_from(&store).await;
        let chains = chains.unwrap();
        assert_eq!(chains.len(), 1);
        let chain = chains.chains_for(&identity("alice", "acme"), &anthropic());
        assert!(chain.request_body().is_empty());
        assert_eq!(chain.response_body().len(), 1);
    }

    #[tokio::test]
    async fn a_global_plugin_that_will_not_load_stops_startup() {
        let store = store().await;
        let broken = stored(&store, PluginKind::ReqBody, BROKEN).await;
        place(&store, broken, 10, PluginScope::Global).await;
        let (_, chains) = load_from(&store).await;
        assert!(matches!(
            chains,
            Err(ChainError::GlobalPlugin { checksum, .. }) if checksum == broken
        ));
    }

    #[tokio::test]
    async fn a_plugin_named_by_many_rules_compiles_once() {
        let store = store().await;
        let shared = stored(&store, PluginKind::RespBody, &distinct("s")).await;
        place(&store, shared, 100, tenant_wide("acme")).await;
        place(&store, shared, 100, tenant_wide("globex")).await;
        place(&store, shared, 10, PluginScope::Global).await;
        let (host, chains) = load_from(&store).await;
        assert_eq!(chains.unwrap().len(), 3);
        assert_eq!(host.len(), 1);
    }

    #[tokio::test]
    async fn a_built_chain_runs_real_wasm() {
        let store = store().await;
        let plugin = stored(&store, PluginKind::RespBody, &distinct("r")).await;
        place(&store, plugin, 100, tenant_wide("acme")).await;
        let (_, chains) = load_from(&store).await;
        let chain = chains
            .unwrap()
            .chains_for(&identity("alice", "acme"), &anthropic());
        assert_eq!(
            chain.response_body()[0]
                .process(Bytes::from_static(b"pong"))
                .await,
            Ok(Bytes::from_static(b"pong!"))
        );
    }
}
