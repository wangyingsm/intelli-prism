use async_trait::async_trait;
use ip_core::{
    Checksum, NewPluginRule, PluginKind, PluginOrder, PluginRule, PluginScope, TenantId, Timestamp,
};

use crate::error::StorageError;

/// Surrogate primary key of a stored plugin row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PluginRowId(i64);

impl PluginRowId {
    /// Wraps a key the backend assigned.
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    /// The key as the backend stores it.
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// What is known about a stored plugin without reading its wasm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRecord {
    /// Primary key every other row references it by.
    pub row_id: PluginRowId,
    /// The sha256 of the wasm, which is how the plugin is named.
    pub checksum: Checksum,
    /// Where in the dataflow it runs.
    pub kind: PluginKind,
    /// How large the wasm is, in bytes.
    pub size: usize,
    /// When it was first stored.
    pub created_at: Timestamp,
}

/// A stored plugin together with its wasm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plugin {
    /// What is known about it without the wasm.
    pub record: PluginRecord,
    /// The module itself.
    pub wasm: Vec<u8>,
}

/// Whose a stored plugin is: the global chain's, or one tenant's.
///
/// Wasm is stored once however many own it, so a plugin is shown to, and placed by, only the
/// chains that own it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PluginOwner {
    /// The chain every flow runs, which the system administrator manages.
    Global,
    /// One tenant's chain.
    Tenant(TenantId),
}

impl PluginOwner {
    /// The tenant that owns the plugin, when a tenant does.
    pub fn tenant(&self) -> Option<&TenantId> {
        match self {
            Self::Global => None,
            Self::Tenant(tenant) => Some(tenant),
        }
    }

    /// The chain a rule in this scope belongs to, which must own the plugin it places.
    pub fn of_scope(scope: &PluginScope) -> Self {
        match scope.tenant() {
            Some(tenant) => Self::Tenant(tenant.clone()),
            None => Self::Global,
        }
    }
}

/// Wasm about to be stored, which is named by its own checksum once written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPlugin {
    /// Where in the dataflow it runs.
    pub kind: PluginKind,
    /// The module itself.
    pub wasm: Vec<u8>,
    /// Who stores it, and so who may see and place it.
    pub owner: PluginOwner,
}

/// A plugin stored for one owner, and since when that owner has held it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginUploaded {
    /// What is stored under the wasm's checksum.
    pub plugin: PluginRecord,
    /// When the owner first stored it, which is not when anyone else did.
    pub owned_at: Timestamp,
}

/// One owner's hold on a plugin that ended, and whether the wasm went with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginDisowned {
    /// The plugin that was held.
    pub plugin: PluginRecord,
    /// Whether nobody owned it any more, so its wasm was removed.
    pub swept: bool,
}

/// Holds the wasm of every plugin, addressed by the checksum of its own bytes.
#[async_trait]
pub trait PluginStore: Send + Sync {
    /// Stores wasm under the checksum of its bytes and records its owner, in one transaction.
    /// Storing the same wasm again only adds an owner, because the name is derived from the
    /// content; storing it as another kind is refused.
    async fn put_plugin(&self, new: NewPlugin) -> Result<PluginRecord, StorageError>;

    /// Reads one plugin, wasm included.
    async fn plugin(&self, checksum: &Checksum) -> Result<Option<Plugin>, StorageError>;

    /// Every stored plugin whoever owns it, without reading any wasm. The api lists by owner.
    async fn plugins(&self) -> Result<Vec<PluginRecord>, StorageError>;

    /// Ends one owner's hold on a plugin, refusing while a rule in its chain still runs it,
    /// and removes the wasm once nobody owns it. Reports whether the wasm went.
    async fn disown_plugin(
        &self,
        checksum: &Checksum,
        owner: &PluginOwner,
    ) -> Result<bool, StorageError>;
}

/// Places stored plugins in chains, and reads back which run for whom.
#[async_trait]
pub trait PluginRuleStore: Send + Sync {
    /// Places a stored plugin, taking its kind from the plugin itself. Refuses a plugin the
    /// rule's chain does not own, and an order its tenant already uses for that kind.
    async fn put_rule(&self, rule: NewPluginRule) -> Result<PluginRule, StorageError>;

    /// Removes the rule at one kind and order, in a tenant or in the global chain.
    async fn remove_rule(
        &self,
        tenant: Option<&TenantId>,
        kind: PluginKind,
        order: PluginOrder,
    ) -> Result<(), StorageError>;

    /// Every rule that could join a chain inside this tenant, the global ones included.
    async fn rules_for_tenant(&self, tenant: &TenantId) -> Result<Vec<PluginRule>, StorageError>;

    /// Every rule held.
    async fn rules(&self) -> Result<Vec<PluginRule>, StorageError>;
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::error::Entity;

    #[derive(Default)]
    struct MemoryPlugins {
        held: Mutex<Vec<Plugin>>,
        owners: Mutex<Vec<(Checksum, PluginOwner)>>,
        next_row_id: Mutex<i64>,
        rules: Mutex<Vec<PluginRule>>,
    }

    #[async_trait]
    impl PluginStore for MemoryPlugins {
        async fn put_plugin(&self, new: NewPlugin) -> Result<PluginRecord, StorageError> {
            let checksum = Checksum::of(&new.wasm);
            let mut owners = self.owners.lock().unwrap();
            if !owners.contains(&(checksum, new.owner.clone())) {
                owners.push((checksum, new.owner.clone()));
            }
            let mut held = self.held.lock().unwrap();
            if let Some(plugin) = held
                .iter()
                .find(|plugin| plugin.record.checksum == checksum)
            {
                if plugin.record.kind != new.kind {
                    return Err(StorageError::Conflict {
                        entity: Entity::Plugin,
                        id: format!("{checksum} as a {} plugin", plugin.record.kind),
                    });
                }
                return Ok(plugin.record.clone());
            }
            let mut next = self.next_row_id.lock().unwrap();
            *next += 1;
            let record = PluginRecord {
                row_id: PluginRowId::new(*next),
                checksum,
                kind: new.kind,
                size: new.wasm.len(),
                created_at: Timestamp::now(),
            };
            held.push(Plugin {
                record: record.clone(),
                wasm: new.wasm,
            });
            Ok(record)
        }

        async fn plugin(&self, checksum: &Checksum) -> Result<Option<Plugin>, StorageError> {
            Ok(self
                .held
                .lock()
                .unwrap()
                .iter()
                .find(|plugin| &plugin.record.checksum == checksum)
                .cloned())
        }

        async fn plugins(&self) -> Result<Vec<PluginRecord>, StorageError> {
            Ok(self
                .held
                .lock()
                .unwrap()
                .iter()
                .map(|plugin| plugin.record.clone())
                .collect())
        }

        async fn disown_plugin(
            &self,
            checksum: &Checksum,
            owner: &PluginOwner,
        ) -> Result<bool, StorageError> {
            if self.rules.lock().unwrap().iter().any(|rule| {
                rule.checksum() == checksum && &PluginOwner::of_scope(rule.scope()) == owner
            }) {
                return Err(StorageError::InUse {
                    entity: Entity::Plugin,
                    id: checksum.to_string(),
                });
            }
            let mut owners = self.owners.lock().unwrap();
            let before = owners.len();
            owners.retain(|held| held != &(*checksum, owner.clone()));
            if owners.len() == before {
                return Err(StorageError::NotFound {
                    entity: Entity::Plugin,
                    id: checksum.to_string(),
                });
            }
            if owners.iter().any(|(held, _)| held == checksum) {
                return Ok(false);
            }
            self.held
                .lock()
                .unwrap()
                .retain(|plugin| &plugin.record.checksum != checksum);
            Ok(true)
        }
    }

    #[async_trait]
    impl PluginRuleStore for MemoryPlugins {
        async fn put_rule(&self, rule: NewPluginRule) -> Result<PluginRule, StorageError> {
            let owner = PluginOwner::of_scope(rule.scope());
            let owned = self
                .owners
                .lock()
                .unwrap()
                .contains(&(*rule.checksum(), owner));
            let kind = self
                .held
                .lock()
                .unwrap()
                .iter()
                .find(|plugin| owned && &plugin.record.checksum == rule.checksum())
                .map(|plugin| plugin.record.kind)
                .ok_or_else(|| StorageError::NotFound {
                    entity: Entity::Plugin,
                    id: rule.checksum().to_string(),
                })?;
            let rule = rule.with_kind(kind);
            let mut rules = self.rules.lock().unwrap();
            if rules.iter().any(|held| {
                held.scope().tenant() == rule.scope().tenant()
                    && held.kind() == rule.kind()
                    && held.order() == rule.order()
            }) {
                return Err(StorageError::Conflict {
                    entity: Entity::PluginRule,
                    id: format!("{} {}", rule.kind(), rule.order()),
                });
            }
            rules.push(rule.clone());
            Ok(rule)
        }

        async fn remove_rule(
            &self,
            tenant: Option<&TenantId>,
            kind: PluginKind,
            order: PluginOrder,
        ) -> Result<(), StorageError> {
            let mut rules = self.rules.lock().unwrap();
            let before = rules.len();
            rules.retain(|held| {
                !(held.scope().tenant() == tenant && held.kind() == kind && held.order() == order)
            });
            if rules.len() == before {
                return Err(StorageError::NotFound {
                    entity: Entity::PluginRule,
                    id: format!("{kind} {order}"),
                });
            }
            Ok(())
        }

        async fn rules_for_tenant(
            &self,
            tenant: &TenantId,
        ) -> Result<Vec<PluginRule>, StorageError> {
            Ok(self
                .rules
                .lock()
                .unwrap()
                .iter()
                .filter(|held| held.scope().tenant().is_none_or(|owner| owner == tenant))
                .cloned()
                .collect())
        }

        async fn rules(&self) -> Result<Vec<PluginRule>, StorageError> {
            Ok(self.rules.lock().unwrap().clone())
        }
    }

    fn store() -> Arc<dyn PluginStore> {
        Arc::new(MemoryPlugins::default())
    }

    fn both() -> Arc<MemoryPlugins> {
        Arc::new(MemoryPlugins::default())
    }

    fn acme() -> TenantId {
        TenantId::new("acme").unwrap()
    }

    fn tenant_wide(tenant: TenantId) -> ip_core::PluginScope {
        ip_core::PluginScope::Tenant {
            tenant,
            user: None,
            api: None,
        }
    }

    fn rule(checksum: Checksum, order: u8, scope: ip_core::PluginScope) -> NewPluginRule {
        NewPluginRule::new(checksum, PluginOrder::new(order), scope).unwrap()
    }

    /// Wasm acme stores, which is the chain most of these tests place in.
    fn wasm(body: &[u8]) -> NewPlugin {
        owned_by(body, PluginOwner::Tenant(acme()))
    }

    fn owned_by(body: &[u8], owner: PluginOwner) -> NewPlugin {
        NewPlugin {
            kind: PluginKind::ReqBody,
            wasm: body.to_vec(),
            owner,
        }
    }

    #[tokio::test]
    async fn the_surface_is_reachable_through_one_trait_object() {
        let store = store();
        let record = store.put_plugin(wasm(b"\0asm\x01\0\0\0")).await.unwrap();
        let plugin = store.plugin(&record.checksum).await.unwrap().unwrap();
        assert_eq!(plugin.wasm, b"\0asm\x01\0\0\0");
        assert_eq!(store.plugins().await.unwrap(), vec![record.clone()]);
        let owner = PluginOwner::Tenant(acme());
        assert!(store.disown_plugin(&record.checksum, &owner).await.unwrap());
        assert_eq!(store.plugin(&record.checksum).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_plugin_is_named_by_the_checksum_of_its_own_bytes() {
        let store = store();
        let record = store.put_plugin(wasm(b"module")).await.unwrap();
        assert_eq!(record.checksum, Checksum::of(b"module"));
        assert!(record.checksum.matches(b"module"));
        assert_eq!(record.size, 6);
    }

    #[tokio::test]
    async fn storing_the_same_wasm_twice_changes_nothing() {
        let store = store();
        let first = store.put_plugin(wasm(b"module")).await.unwrap();
        let second = store.put_plugin(wasm(b"module")).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(store.plugins().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn different_wasm_is_a_different_plugin() {
        let store = store();
        store.put_plugin(wasm(b"one")).await.unwrap();
        store.put_plugin(wasm(b"two")).await.unwrap();
        assert_eq!(store.plugins().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn listing_does_not_carry_the_wasm() {
        let store = store();
        let record = store.put_plugin(wasm(b"module")).await.unwrap();
        let listed = store.plugins().await.unwrap();
        assert_eq!(listed, vec![record]);
        assert_eq!(listed[0].size, 6);
    }

    #[tokio::test]
    async fn the_rule_surface_is_reachable_through_one_trait_object() {
        let held = both();
        let record = held.put_plugin(wasm(b"module")).await.unwrap();
        let rules: Arc<dyn PluginRuleStore> = held;
        let placed = rules
            .put_rule(rule(record.checksum, 100, tenant_wide(acme())))
            .await
            .unwrap();
        assert_eq!(rules.rules().await.unwrap(), vec![placed.clone()]);
        rules
            .remove_rule(Some(&acme()), placed.kind(), placed.order())
            .await
            .unwrap();
        assert!(rules.rules().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_rule_takes_its_kind_from_the_stored_plugin() {
        let held = both();
        let record = held
            .put_plugin(NewPlugin {
                kind: PluginKind::RespHeader,
                wasm: b"module".to_vec(),
                owner: PluginOwner::Tenant(acme()),
            })
            .await
            .unwrap();
        let placed = held
            .put_rule(rule(record.checksum, 100, tenant_wide(acme())))
            .await
            .unwrap();
        assert_eq!(placed.kind(), PluginKind::RespHeader);
    }

    #[tokio::test]
    async fn a_rule_naming_a_plugin_that_is_not_stored_is_refused() {
        let held = both();
        assert!(matches!(
            held.put_rule(rule(Checksum::of(b"absent"), 100, tenant_wide(acme())))
                .await,
            Err(StorageError::NotFound {
                entity: Entity::Plugin,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn an_order_a_tenant_already_uses_for_that_kind_is_refused() {
        let held = both();
        let first = held.put_plugin(wasm(b"one")).await.unwrap();
        let second = held.put_plugin(wasm(b"two")).await.unwrap();
        held.put_rule(rule(first.checksum, 100, tenant_wide(acme())))
            .await
            .unwrap();
        let user_scoped = ip_core::PluginScope::Tenant {
            tenant: acme(),
            user: Some(ip_core::UserId::new("bob").unwrap()),
            api: None,
        };
        assert!(matches!(
            held.put_rule(rule(second.checksum, 100, user_scoped)).await,
            Err(StorageError::Conflict {
                entity: Entity::PluginRule,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn another_tenant_may_reuse_the_order() {
        let held = both();
        let record = held.put_plugin(wasm(b"module")).await.unwrap();
        held.put_rule(rule(record.checksum, 100, tenant_wide(acme())))
            .await
            .unwrap();
        let globex = TenantId::new("globex").unwrap();
        held.put_plugin(owned_by(b"module", PluginOwner::Tenant(globex.clone())))
            .await
            .unwrap();
        assert!(
            held.put_rule(rule(record.checksum, 100, tenant_wide(globex)))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_tenant_sees_its_own_rules_and_the_global_ones() {
        let held = both();
        let record = held.put_plugin(wasm(b"module")).await.unwrap();
        let globex = TenantId::new("globex").unwrap();
        for owner in [PluginOwner::Global, PluginOwner::Tenant(globex.clone())] {
            held.put_plugin(owned_by(b"module", owner)).await.unwrap();
        }
        held.put_rule(rule(record.checksum, 10, ip_core::PluginScope::Global))
            .await
            .unwrap();
        held.put_rule(rule(record.checksum, 100, tenant_wide(acme())))
            .await
            .unwrap();
        held.put_rule(rule(record.checksum, 100, tenant_wide(globex)))
            .await
            .unwrap();
        let seen = held.rules_for_tenant(&acme()).await.unwrap();
        assert_eq!(seen.len(), 2);
        assert!(
            seen.iter()
                .all(|rule| rule.scope().tenant().is_none_or(|t| t == &acme()))
        );
    }

    #[tokio::test]
    async fn a_plugin_a_rule_still_uses_is_not_removed() {
        let held = both();
        let record = held.put_plugin(wasm(b"module")).await.unwrap();
        held.put_rule(rule(record.checksum, 100, tenant_wide(acme())))
            .await
            .unwrap();
        assert!(matches!(
            held.disown_plugin(&record.checksum, &PluginOwner::Tenant(acme()))
                .await,
            Err(StorageError::InUse {
                entity: Entity::Plugin,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_chain_places_only_the_plugins_it_owns() {
        let held = both();
        let record = held.put_plugin(wasm(b"module")).await.unwrap();
        let globex = tenant_wide(TenantId::new("globex").unwrap());
        for (order, scope) in [(10, ip_core::PluginScope::Global), (100, globex)] {
            assert!(matches!(
                held.put_rule(rule(record.checksum, order, scope)).await,
                Err(StorageError::NotFound {
                    entity: Entity::Plugin,
                    ..
                })
            ));
        }
    }

    #[tokio::test]
    async fn removing_a_rule_that_is_absent_reports_it_missing() {
        let held = both();
        assert!(matches!(
            held.remove_rule(Some(&acme()), PluginKind::ReqBody, PluginOrder::new(100))
                .await,
            Err(StorageError::NotFound {
                entity: Entity::PluginRule,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn removing_what_is_absent_reports_it_missing() {
        let store = store();
        assert!(matches!(
            store
                .disown_plugin(&Checksum::of(b"absent"), &PluginOwner::Global)
                .await,
            Err(StorageError::NotFound {
                entity: Entity::Plugin,
                ..
            })
        ));
    }
}
