//! Keeping every node's copy of the rules in step with storage.
//!
//! Storage is the truth. After every change to the rules, the whole set is published in the
//! cache under the revision storage stamped it with, and every node follows that publication.
//! Storage and cache cannot commit together, so a publish that fails is healed later: each
//! node compares the two revisions on a timer and publishes again when the cache is behind.

use std::sync::Arc;
use std::time::Duration;

use ip_gateway::{Gateway, RoutingTable};
use ip_plugin::{PluginChains, PluginHost};
use tokio::sync::watch;

use ip_cache::{CacheBackend, CacheKey, CacheLevel};
use ip_core::{Checksum, PluginKind, PluginOrder, PluginRule, PluginScope, RouteRule};
use ip_storage::RuleSet;
use ip_storage::{Backend, RuleRevision};
use tokio::task::JoinHandle;

use crate::error::FeedError;

/// How often, at the least, a node checks that the cache has not fallen behind storage.
const HEAL_EVERY: Duration = Duration::from_secs(30);

/// The most a node adds to that, so nodes started together do not check together.
const HEAL_JITTER: Duration = Duration::from_secs(10);

/// The most a node waits before acting on a change, so a change never sends every node to
/// rebuild at the same instant.
const RELOAD_SPREAD: Duration = Duration::from_secs(2);

/// How long a node waits before trying a reload that failed again.
const RETRY_FIRST: Duration = Duration::from_secs(1);

/// The longest it waits between those attempts.
const RETRY_MOST: Duration = Duration::from_secs(60);

/// The rules as the cache carries them, without the revision the publication already holds.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Snapshot {
    routes: Vec<RouteRule>,
    rules: Vec<Placed>,
}

/// One plugin rule on the wire, rebuilt through its own checks when read back.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Placed {
    checksum: Checksum,
    kind: PluginKind,
    order: PluginOrder,
    scope: PluginScope,
}

/// Publishes the rules storage holds, and reads back what was published.
pub struct RuleFeed {
    store: Arc<dyn Backend>,
    cache: Arc<dyn CacheBackend>,
    topic: CacheKey,
}

impl RuleFeed {
    /// A feed from this store into this cache.
    pub fn new(store: Arc<dyn Backend>, cache: Arc<dyn CacheBackend>) -> Result<Self, FeedError> {
        Ok(Self {
            store,
            cache,
            topic: CacheKey::new(CacheLevel::System, "rules")?,
        })
    }

    /// Publishes the rules as storage holds them now, and the revision they stand at. A cache
    /// holding a newer revision keeps it.
    pub async fn publish(&self) -> Result<RuleRevision, FeedError> {
        let set = self.store.rule_set().await?;
        let snapshot = Snapshot {
            routes: set.routes,
            rules: set.rules.iter().map(Placed::from).collect(),
        };
        let encoded = serde_json::to_vec(&snapshot).map_err(FeedError::Encode)?;
        self.cache
            .publish(&self.topic, set.revision.get(), &encoded)
            .await?;
        Ok(set.revision)
    }

    /// Publishes after a change the caller has already committed, which stands whether or
    /// not this lands: a failure is logged and left for healing.
    pub async fn after_change(&self) {
        if let Err(error) = self.publish().await {
            tracing::warn!(%error, "could not publish the rules; healing will");
        }
    }

    /// The rules published now, or none when nothing has been.
    pub async fn published(&self) -> Result<Option<RuleSet>, FeedError> {
        let Some(published) = self.cache.published(&self.topic).await? else {
            return Ok(None);
        };
        let snapshot: Snapshot =
            serde_json::from_slice(&published.value).map_err(FeedError::Decode)?;
        let rules = snapshot
            .rules
            .into_iter()
            .map(PluginRule::try_from)
            .collect::<Result<_, _>>()?;
        Ok(Some(RuleSet {
            revision: RuleRevision::new(published.revision),
            routes: snapshot.routes,
            rules,
        }))
    }

    /// The cache the rules are published in, for a test that publishes something this feed
    /// would never write.
    #[cfg(test)]
    pub fn cache(&self) -> &Arc<dyn CacheBackend> {
        &self.cache
    }

    /// Follows the published revision, which moves on whenever the rules do.
    pub async fn follow(&self) -> Result<watch::Receiver<u64>, FeedError> {
        Ok(self.cache.follow(&self.topic).await?)
    }

    /// Publishes again when the cache holds nothing or is behind storage, reporting whether it
    /// was.
    pub async fn heal(&self) -> Result<bool, FeedError> {
        let stored = self.store.rule_revision().await?;
        let published = self.cache.published(&self.topic).await?;
        if published.is_some_and(|published| published.revision >= stored.get()) {
            return Ok(false);
        }
        self.publish().await?;
        Ok(true)
    }

    /// Heals on a jittered timer for as long as the server runs.
    pub fn keep_healing(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(HEAL_EVERY + jitter(HEAL_JITTER)).await;
                match self.heal().await {
                    Ok(true) => {
                        tracing::warn!("the cached rules were behind storage; published them again")
                    }
                    Ok(false) => {}
                    Err(error) => tracing::warn!(%error, "could not check the cached rules"),
                }
            }
        })
    }
}

/// A span between nothing and `most`, drawn afresh each time.
pub fn jitter(most: Duration) -> Duration {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        return most / 2;
    }
    let most = u64::try_from(most.as_millis()).unwrap_or(u64::MAX).max(1);
    Duration::from_millis(u64::from_le_bytes(bytes) % most)
}

/// Keeps this node's routing table and plugin chains in step with what is published.
///
/// The rules are built before anything is replaced, so a set that cannot be built leaves the
/// node serving the one it already has.
pub struct Reloader {
    feed: Arc<RuleFeed>,
    store: Arc<dyn Backend>,
    host: Arc<PluginHost>,
    gateway: Arc<Gateway>,
    configured: Arc<[RouteRule]>,
}

impl Reloader {
    /// A reloader that puts what `feed` publishes in force in `gateway`.
    pub fn new(
        feed: Arc<RuleFeed>,
        store: Arc<dyn Backend>,
        host: Arc<PluginHost>,
        gateway: Arc<Gateway>,
        configured: Arc<[RouteRule]>,
    ) -> Self {
        Self {
            feed,
            store,
            host,
            gateway,
            configured,
        }
    }

    /// Builds the rules published now and puts them in force, reporting the revision that is
    /// now serving. Answers nothing when nothing is published yet.
    pub async fn reload(&self) -> Result<Option<RuleRevision>, FeedError> {
        let Some(set) = self.feed.published().await? else {
            return Ok(None);
        };
        let table = RoutingTable::assemble(&self.configured, set.routes)?;
        let chains =
            PluginChains::from_rules(Arc::clone(&self.host), self.store.as_ref(), set.rules)
                .await?;
        self.gateway.replace(table, Arc::new(chains));
        Ok(Some(set.revision))
    }

    /// Follows the published rules from `applied`, reloading whenever they move past it, for
    /// as long as the server runs.
    pub fn keep_following(self: Arc<Self>, applied: RuleRevision) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut following = match self.feed.follow().await {
                Ok(following) => following,
                Err(error) => {
                    tracing::error!(%error, "could not follow the rules; this node keeps the rules it started with");
                    return;
                }
            };
            let mut applied = applied;
            while following.changed().await.is_ok() {
                if *following.borrow_and_update() <= applied.get() {
                    continue;
                }
                tokio::time::sleep(jitter(RELOAD_SPREAD)).await;
                applied = self.reload_until_it_lands(applied, &mut following).await;
            }
        })
    }

    /// Reloads, waiting longer after each failure, until it lands or the feed is gone.
    async fn reload_until_it_lands(
        &self,
        applied: RuleRevision,
        following: &mut watch::Receiver<u64>,
    ) -> RuleRevision {
        let mut wait = RETRY_FIRST;
        loop {
            match self.reload().await {
                Ok(Some(revision)) => {
                    tracing::info!(%revision, "the rules this node serves moved on");
                    return revision;
                }
                Ok(None) => return applied,
                Err(error) => {
                    tracing::error!(%error, "could not put the published rules in force; keeping the ones in hand");
                    tokio::select! {
                        () = tokio::time::sleep(wait) => {}
                        changed = following.changed() => {
                            if changed.is_err() {
                                return applied;
                            }
                        }
                    }
                    wait = (wait * 2).min(RETRY_MOST);
                }
            }
        }
    }
}

impl From<&PluginRule> for Placed {
    fn from(rule: &PluginRule) -> Self {
        Self {
            checksum: *rule.checksum(),
            kind: rule.kind(),
            order: rule.order(),
            scope: rule.scope().clone(),
        }
    }
}

impl TryFrom<Placed> for PluginRule {
    type Error = FeedError;

    fn try_from(placed: Placed) -> Result<Self, Self::Error> {
        Ok(PluginRule::new(
            placed.checksum,
            placed.kind,
            placed.order,
            placed.scope,
        )?)
    }
}

#[cfg(test)]
mod tests {
    use ip_cache::{Publication, SledCache};
    use ip_core::{
        AbsPath, ApiId, Endpoint, Host, NewPluginRule, Port, Protocol, RouteKey, RouteTarget,
        TenantId, TnKey,
    };
    use ip_storage::{
        NewPlugin, NewTenant, PluginOwner, PluginRuleStore, PluginStore, RevisionStore, RouteStore,
        SqliteStore, TenantStore,
    };

    use super::*;

    fn route(path: &str) -> RouteRule {
        let endpoint = |host: &str| {
            Endpoint::new(
                Protocol::Https,
                Host::new(host).unwrap(),
                Port::new(443).unwrap(),
                AbsPath::new(path).unwrap(),
            )
        };
        RouteRule {
            api: ApiId::new("chat").unwrap(),
            key: RouteKey::new(endpoint("gateway.local")),
            target: RouteTarget::new(endpoint("api.example.com")),
        }
    }

    /// A feed over a store holding one route and one plugin rule in acme, and the store.
    async fn feed() -> (RuleFeed, Arc<SqliteStore>, Arc<SledCache>) {
        let store = Arc::new(SqliteStore::in_memory().await.unwrap());
        let acme = TenantId::new("acme").unwrap();
        store
            .create_tenant(NewTenant {
                id: acme.clone(),
                key: TnKey::generate().unwrap(),
            })
            .await
            .unwrap();
        store.put_route(route("/v1")).await.unwrap();
        let plugin = store
            .put_plugin(NewPlugin {
                kind: PluginKind::RespBody,
                wasm: b"module".to_vec(),
                owner: PluginOwner::Tenant(acme.clone()),
            })
            .await
            .unwrap();
        let scope = PluginScope::Tenant {
            tenant: acme,
            user: None,
            api: None,
        };
        store
            .put_rule(NewPluginRule::new(plugin.checksum, PluginOrder::new(100), scope).unwrap())
            .await
            .unwrap();
        let cache = Arc::new(SledCache::temporary().unwrap());
        let feed = RuleFeed::new(
            Arc::clone(&store) as Arc<dyn Backend>,
            Arc::clone(&cache) as Arc<dyn CacheBackend>,
        )
        .unwrap();
        (feed, store, cache)
    }

    #[tokio::test]
    async fn what_is_published_is_the_rules_as_storage_holds_them() {
        let (feed, store, _) = feed().await;
        let revision = feed.publish().await.unwrap();
        assert_eq!(revision, store.rule_revision().await.unwrap());
        assert_eq!(
            feed.published().await.unwrap(),
            Some(store.rule_set().await.unwrap())
        );
    }

    #[tokio::test]
    async fn healing_fills_an_empty_cache_and_then_rests() {
        let (feed, _, _) = feed().await;
        assert!(feed.heal().await.unwrap());
        assert!(!feed.heal().await.unwrap());
    }

    #[tokio::test]
    async fn healing_catches_up_a_cache_a_failed_publish_left_behind() {
        let (feed, store, _) = feed().await;
        feed.publish().await.unwrap();
        // A change whose publish never happened, as when the cache was down.
        store.put_route(route("/v2")).await.unwrap();
        let behind = feed.published().await.unwrap().unwrap();
        assert!(behind.revision < store.rule_revision().await.unwrap());

        assert!(feed.heal().await.unwrap());
        let healed = feed.published().await.unwrap().unwrap();
        assert_eq!(healed.revision, store.rule_revision().await.unwrap());
        assert_eq!(healed.routes.len(), 2);
    }

    #[tokio::test]
    async fn a_rule_its_own_type_refuses_does_not_come_back_through_the_cache() {
        let (feed, _, cache) = feed().await;
        let refused = serde_json::json!({
            "routes": [],
            "rules": [{
                "checksum": Checksum::of(b"module"),
                "kind": "resp_body",
                "order": 10,
                "scope": {"Tenant": {"tenant": "acme", "user": null, "api": null}},
            }],
        });
        let topic = CacheKey::new(CacheLevel::System, "rules").unwrap();
        cache
            .publish(&topic, 99, refused.to_string().as_bytes())
            .await
            .unwrap();
        assert!(matches!(feed.published().await, Err(FeedError::Rule(_))));
    }

    /// A node whose gateway serves whatever the reloader puts in force, over a store and a
    /// cache of its own.
    async fn node() -> (crate::state::AppState, Arc<SqliteStore>) {
        let store = Arc::new(SqliteStore::in_memory().await.unwrap());
        let state = crate::routes::harness::state_over_shared(Arc::clone(&store));
        (state, store)
    }

    /// Waits for the gateway to serve `rules`, or gives up.
    async fn serves(state: &crate::state::AppState, rules: usize) -> bool {
        tokio::time::timeout(Duration::from_secs(20), async {
            while state.gateway().table().len() != rules {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .is_ok()
    }

    #[tokio::test]
    async fn what_is_published_becomes_what_this_node_serves() {
        let (state, store) = node().await;
        assert!(state.gateway().table().is_empty());
        store.put_route(route("/v1")).await.unwrap();
        state.feed().publish().await.unwrap();

        let revision = state.reload_rules().await.unwrap().unwrap();
        assert_eq!(revision, store.rule_revision().await.unwrap());
        assert_eq!(state.gateway().table().len(), 1);
    }

    #[tokio::test]
    async fn rules_that_cannot_be_built_leave_the_ones_in_hand_serving() {
        let (state, store) = node().await;
        store.put_route(route("/v1")).await.unwrap();
        state.feed().publish().await.unwrap();
        state.reload_rules().await.unwrap();

        // A rule on the gateway's own prefix, which the table refuses and the api never stores.
        let refused = serde_json::json!({
            "routes": [{
                "api": "chat",
                "key": {"protocol": "https", "host": "gateway.local", "port": 443, "path": "/_ip/own"},
                "target": [{"protocol": "https", "host": "api.example.com", "port": 443, "path": "/v1"}],
            }],
            "rules": [],
        });
        let topic = CacheKey::new(CacheLevel::System, "rules").unwrap();
        state
            .feed()
            .cache()
            .publish(&topic, 9_999, refused.to_string().as_bytes())
            .await
            .unwrap();

        assert!(matches!(
            state.reload_rules().await,
            Err(FeedError::Table(_))
        ));
        assert_eq!(state.gateway().table().len(), 1, "the node stopped serving");
    }

    #[tokio::test]
    async fn a_global_plugin_that_will_not_load_leaves_the_rules_in_hand_serving() {
        let (state, store) = node().await;
        store.put_route(route("/v1")).await.unwrap();
        state.feed().publish().await.unwrap();
        state.reload_rules().await.unwrap();

        let plugin = store
            .put_plugin(NewPlugin {
                kind: PluginKind::ReqBody,
                wasm: b"not wasm at all".to_vec(),
                owner: PluginOwner::Global,
            })
            .await
            .unwrap();
        store
            .put_rule(
                NewPluginRule::new(plugin.checksum, PluginOrder::new(10), PluginScope::Global)
                    .unwrap(),
            )
            .await
            .unwrap();
        state.feed().publish().await.unwrap();

        assert!(matches!(
            state.reload_rules().await,
            Err(FeedError::Chains(_))
        ));
        assert_eq!(state.gateway().table().len(), 1, "the node stopped serving");
    }

    /// A route on the gateway's own prefix, which the table refuses and the api never stores.
    fn refused_snapshot() -> String {
        serde_json::json!({
            "routes": [{
                "api": "chat",
                "key": {"protocol": "https", "host": "gateway.local", "port": 443, "path": "/_ip/own"},
                "target": [{"protocol": "https", "host": "api.example.com", "port": 443, "path": "/v1"}],
            }],
            "rules": [],
        })
        .to_string()
    }

    #[tokio::test]
    async fn a_set_that_cannot_be_built_is_tried_again_until_one_that_can_arrives() {
        let (state, store) = node().await;
        let topic = CacheKey::new(CacheLevel::System, "rules").unwrap();
        let ahead = store.rule_revision().await.unwrap().get() + 1;
        state
            .feed()
            .cache()
            .publish(&topic, ahead, refused_snapshot().as_bytes())
            .await
            .unwrap();

        let following = state.keep_rules_in_step();
        for path in ["/v1", "/v2"] {
            store.put_route(route(path)).await.unwrap();
        }
        assert!(
            store.rule_revision().await.unwrap().get() > ahead,
            "the rules must move past the set that cannot be built"
        );
        state.feed().publish().await.unwrap();

        assert!(serves(&state, 2).await, "the node never caught up");
        following.abort();
    }

    #[tokio::test]
    async fn following_puts_a_change_in_force_of_its_own_accord() {
        let (state, store) = node().await;
        let following = state.keep_rules_in_step();
        store.put_route(route("/v1")).await.unwrap();
        state.feed().publish().await.unwrap();
        assert!(
            serves(&state, 1).await,
            "the change never reached the table"
        );

        for path in ["/v2", "/v3"] {
            store.put_route(route(path)).await.unwrap();
            state.feed().publish().await.unwrap();
        }
        assert!(serves(&state, 3).await, "the newest change never landed");
        following.abort();
    }

    #[test]
    fn jitter_stays_between_nothing_and_its_most() {
        for _ in 0..1_000 {
            assert!(jitter(Duration::from_secs(10)) < Duration::from_secs(10));
        }
        assert_eq!(jitter(Duration::ZERO), Duration::ZERO);
    }

    #[tokio::test]
    async fn a_cache_that_cannot_take_the_rules_leaves_healing_to_try_again() {
        let (_, store, _) = feed().await;
        let told = Arc::new(ip_cache::FailingCache::new(Arc::new(
            SledCache::temporary().unwrap(),
        )));
        let feed = RuleFeed::new(
            Arc::clone(&store) as Arc<dyn Backend>,
            Arc::clone(&told) as Arc<dyn CacheBackend>,
        )
        .unwrap();
        told.fail_now();

        assert!(matches!(feed.publish().await, Err(FeedError::Cache(_))));
        assert!(matches!(feed.heal().await, Err(FeedError::Cache(_))));
        // A change stands whether or not it is published: this only logs.
        feed.after_change().await;

        told.answer_again();
        assert!(feed.heal().await.unwrap(), "healing never caught up");
    }
}
