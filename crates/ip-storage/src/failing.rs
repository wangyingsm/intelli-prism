//! A store that fails when it is told to, for the arms a working one never reaches.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use ip_agents::AgentId;
use ip_core::{
    Checksum, Counted, Grant, Grants, LimitScope, ModelName, NewPluginRule, PassphraseHash, Period,
    PluginKind, PluginOrder, PluginRule, RouteKey, RouteRule, TenantId, Timestamp, UserId,
};
use ip_mcp::{McpId, McpServer};

use crate::agent::{AgentOwner, AgentStore, NewAgent, StoredAgent};
use crate::audit::{AuditFilter, AuditStore, Audited, By};
use crate::error::{StorageError, ToldToFail};
use crate::limit::{Limit, LimitStore, NewLimit};
use crate::list::{ListStore, Listed, Page};
use crate::mcp::{McpStore, StoredMcp};
use crate::model::{Membership, NewTenant, NewUser, Tenant, User};
use crate::plugin::{NewPlugin, Plugin, PluginOwner, PluginRecord, PluginRuleStore, PluginStore};
use crate::price::{NewPrice, PriceStore, Priced};
use crate::revision::{RevisionStore, RuleRevision, RuleSet};
use crate::route::RouteStore;
use crate::store::{Backend, GrantStore, MembershipStore, TenantStore, UserStore};
use crate::usage::{NewUsage, SpentArms, Usage, UsageFilter, UsageRowId, UsageStore};

/// A store that answers like the one it wraps until the calls it was given run out, and
/// refuses every call after that.
///
/// A handler's "the store could not answer" arm cannot be reached by a working database, and
/// a real one cannot be relied on to fail at the call a test means. This one fails exactly
/// where it is told to.
pub struct FailingStore {
    inner: Arc<dyn Backend>,
    answers_left: AtomicUsize,
}

impl FailingStore {
    /// A store that answers everything, until it is told otherwise.
    pub fn new(inner: Arc<dyn Backend>) -> Self {
        Self {
            inner,
            answers_left: AtomicUsize::new(usize::MAX),
        }
    }

    /// Answers `calls` more times, then refuses everything. Nothing is refused before that.
    pub fn answer_only(&self, calls: usize) {
        self.answers_left.store(calls, Ordering::SeqCst);
    }

    /// Refuses every call from here on.
    pub fn fail_now(&self) {
        self.answer_only(0);
    }

    /// Answers everything again, however many calls were left.
    pub fn answer_again(&self) {
        self.answers_left.store(usize::MAX, Ordering::SeqCst);
    }

    /// Takes one of the answers left, or refuses when there are none.
    fn answering(&self) -> Result<(), StorageError> {
        let taken = self
            .answers_left
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                (left > 0).then(|| left.saturating_sub(1))
            });
        match taken {
            Ok(_) => Ok(()),
            Err(_) => Err(StorageError::backend(ToldToFail)),
        }
    }
}

#[async_trait]
impl TenantStore for FailingStore {
    async fn create_tenant(&self, new: NewTenant, by: &By) -> Result<Tenant, StorageError> {
        self.answering()?;
        self.inner.create_tenant(new, by).await
    }

    async fn tenant(&self, id: &TenantId) -> Result<Option<Tenant>, StorageError> {
        self.answering()?;
        self.inner.tenant(id).await
    }

    async fn delete_tenant(&self, id: &TenantId, by: &By) -> Result<(), StorageError> {
        self.answering()?;
        self.inner.delete_tenant(id, by).await
    }
}

#[async_trait]
impl UserStore for FailingStore {
    async fn create_user(&self, new: NewUser, by: &By) -> Result<User, StorageError> {
        self.answering()?;
        self.inner.create_user(new, by).await
    }

    async fn user(&self, id: &UserId) -> Result<Option<User>, StorageError> {
        self.answering()?;
        self.inner.user(id).await
    }

    async fn set_passphrase(
        &self,
        id: &UserId,
        passphrase: &PassphraseHash,
        by: &By,
    ) -> Result<(), StorageError> {
        self.answering()?;
        self.inner.set_passphrase(id, passphrase, by).await
    }

    async fn delete_user(&self, id: &UserId, by: &By) -> Result<(), StorageError> {
        self.answering()?;
        self.inner.delete_user(id, by).await
    }
}

#[async_trait]
impl MembershipStore for FailingStore {
    async fn attach(&self, membership: Membership, by: &By) -> Result<(), StorageError> {
        self.answering()?;
        self.inner.attach(membership, by).await
    }

    async fn membership(
        &self,
        user: &UserId,
        tenant: &TenantId,
    ) -> Result<Option<Membership>, StorageError> {
        self.answering()?;
        self.inner.membership(user, tenant).await
    }

    async fn memberships_of_user(&self, user: &UserId) -> Result<Vec<Membership>, StorageError> {
        self.answering()?;
        self.inner.memberships_of_user(user).await
    }

    async fn members_of_tenant(&self, tenant: &TenantId) -> Result<Vec<Membership>, StorageError> {
        self.answering()?;
        self.inner.members_of_tenant(tenant).await
    }
}

#[async_trait]
impl GrantStore for FailingStore {
    async fn grant(&self, grant: &Grant, by: &By) -> Result<(), StorageError> {
        self.answering()?;
        self.inner.grant(grant, by).await
    }

    async fn revoke(&self, grant: &Grant, by: &By) -> Result<(), StorageError> {
        self.answering()?;
        self.inner.revoke(grant, by).await
    }

    async fn grants_of(&self, user: &UserId) -> Result<Grants, StorageError> {
        self.answering()?;
        self.inner.grants_of(user).await
    }

    async fn grants_in_tenant(
        &self,
        user: &UserId,
        tenant: &TenantId,
    ) -> Result<Grants, StorageError> {
        self.answering()?;
        self.inner.grants_in_tenant(user, tenant).await
    }
}

#[async_trait]
impl RouteStore for FailingStore {
    async fn put_route(&self, rule: RouteRule, by: &By) -> Result<(), StorageError> {
        self.answering()?;
        self.inner.put_route(rule, by).await
    }

    async fn remove_route(&self, key: &RouteKey, by: &By) -> Result<(), StorageError> {
        self.answering()?;
        self.inner.remove_route(key, by).await
    }

    async fn route(&self, key: &RouteKey) -> Result<Option<RouteRule>, StorageError> {
        self.answering()?;
        self.inner.route(key).await
    }

    async fn routes(&self) -> Result<Vec<RouteRule>, StorageError> {
        self.answering()?;
        self.inner.routes().await
    }
}

#[async_trait]
impl PluginStore for FailingStore {
    async fn put_plugin(&self, new: NewPlugin, by: &By) -> Result<PluginRecord, StorageError> {
        self.answering()?;
        self.inner.put_plugin(new, by).await
    }

    async fn plugin(&self, checksum: &Checksum) -> Result<Option<Plugin>, StorageError> {
        self.answering()?;
        self.inner.plugin(checksum).await
    }

    async fn plugins(&self) -> Result<Vec<PluginRecord>, StorageError> {
        self.answering()?;
        self.inner.plugins().await
    }

    async fn disown_plugin(
        &self,
        checksum: &Checksum,
        owner: &PluginOwner,
        by: &By,
    ) -> Result<bool, StorageError> {
        self.answering()?;
        self.inner.disown_plugin(checksum, owner, by).await
    }
}

#[async_trait]
impl PluginRuleStore for FailingStore {
    async fn put_rule(&self, rule: NewPluginRule, by: &By) -> Result<PluginRule, StorageError> {
        self.answering()?;
        self.inner.put_rule(rule, by).await
    }

    async fn remove_rule(
        &self,
        tenant: Option<&TenantId>,
        kind: PluginKind,
        order: PluginOrder,
        by: &By,
    ) -> Result<(), StorageError> {
        self.answering()?;
        self.inner.remove_rule(tenant, kind, order, by).await
    }

    async fn rules_for_tenant(&self, tenant: &TenantId) -> Result<Vec<PluginRule>, StorageError> {
        self.answering()?;
        self.inner.rules_for_tenant(tenant).await
    }

    async fn rules(&self) -> Result<Vec<PluginRule>, StorageError> {
        self.answering()?;
        self.inner.rules().await
    }
}

#[async_trait]
impl ListStore for FailingStore {
    async fn list_members(
        &self,
        tenant: &TenantId,
        page: Page,
    ) -> Result<Vec<Listed<Membership>>, StorageError> {
        self.answering()?;
        self.inner.list_members(tenant, page).await
    }

    async fn list_tenant_grants(
        &self,
        user: &UserId,
        tenant: &TenantId,
        page: Page,
    ) -> Result<Vec<Listed<Grant>>, StorageError> {
        self.answering()?;
        self.inner.list_tenant_grants(user, tenant, page).await
    }

    async fn list_account_grants(
        &self,
        user: &UserId,
        page: Page,
    ) -> Result<Vec<Listed<Grant>>, StorageError> {
        self.answering()?;
        self.inner.list_account_grants(user, page).await
    }

    async fn list_routes(&self, page: Page) -> Result<Vec<Listed<RouteRule>>, StorageError> {
        self.answering()?;
        self.inner.list_routes(page).await
    }

    async fn list_plugins(
        &self,
        owner: &PluginOwner,
        page: Page,
    ) -> Result<Vec<Listed<PluginRecord>>, StorageError> {
        self.answering()?;
        self.inner.list_plugins(owner, page).await
    }

    async fn list_rules(
        &self,
        tenant: Option<&TenantId>,
        page: Page,
    ) -> Result<Vec<Listed<PluginRule>>, StorageError> {
        self.answering()?;
        self.inner.list_rules(tenant, page).await
    }

    async fn list_limits(
        &self,
        tenant: Option<&TenantId>,
        page: Page,
    ) -> Result<Vec<Limit>, StorageError> {
        self.answering()?;
        self.inner.list_limits(tenant, page).await
    }

    async fn list_usage(
        &self,
        filter: &UsageFilter,
        page: Page,
    ) -> Result<Vec<Usage>, StorageError> {
        self.answering()?;
        self.inner.list_usage(filter, page).await
    }
}

#[async_trait]
impl RevisionStore for FailingStore {
    async fn rule_revision(&self) -> Result<RuleRevision, StorageError> {
        self.answering()?;
        self.inner.rule_revision().await
    }

    async fn rule_set(&self) -> Result<RuleSet, StorageError> {
        self.answering()?;
        self.inner.rule_set().await
    }
}

#[async_trait]
impl UsageStore for FailingStore {
    async fn record_usage(&self, usage: NewUsage) -> Result<Usage, StorageError> {
        self.answering()?;
        self.inner.record_usage(usage).await
    }

    async fn usage(&self, row_id: UsageRowId) -> Result<Option<Usage>, StorageError> {
        self.answering()?;
        self.inner.usage(row_id).await
    }

    async fn sweep_usage(&self, moment: Timestamp) -> Result<u64, StorageError> {
        self.answering()?;
        self.inner.sweep_usage(moment).await
    }

    async fn spent(
        &self,
        filter: &UsageFilter,
        counted: Counted,
        moment: Timestamp,
    ) -> Result<u64, StorageError> {
        self.answering()?;
        self.inner.spent(filter, counted, moment).await
    }

    async fn sent_upstream(
        &self,
        filter: &UsageFilter,
        moment: Option<Timestamp>,
    ) -> Result<SpentArms, StorageError> {
        self.answering()?;
        self.inner.sent_upstream(filter, moment).await
    }
}

#[async_trait]
impl AuditStore for FailingStore {
    async fn audits(&self, filter: &AuditFilter, page: Page) -> Result<Vec<Audited>, StorageError> {
        self.answering()?;
        self.inner.audits(filter, page).await
    }
}

#[async_trait]
impl LimitStore for FailingStore {
    async fn put_limit(&self, limit: NewLimit, by: &By) -> Result<Limit, StorageError> {
        self.answering()?;
        self.inner.put_limit(limit, by).await
    }

    async fn remove_limit(
        &self,
        scope: &LimitScope,
        counted: Counted,
        period: Period,
        by: &By,
    ) -> Result<(), StorageError> {
        self.answering()?;
        self.inner.remove_limit(scope, counted, period, by).await
    }

    async fn limits(&self) -> Result<Vec<Limit>, StorageError> {
        self.answering()?;
        self.inner.limits().await
    }
}

#[async_trait]
impl PriceStore for FailingStore {
    async fn put_price(&self, price: NewPrice, by: &By) -> Result<Priced, StorageError> {
        self.answering()?;
        self.inner.put_price(price, by).await
    }

    async fn remove_price(&self, model: &ModelName, by: &By) -> Result<(), StorageError> {
        self.answering()?;
        self.inner.remove_price(model, by).await
    }

    async fn prices(&self) -> Result<Vec<Priced>, StorageError> {
        self.answering()?;
        self.inner.prices().await
    }
}

#[async_trait]
impl AgentStore for FailingStore {
    async fn put_agent(&self, agent: NewAgent, by: &By) -> Result<StoredAgent, StorageError> {
        self.answering()?;
        self.inner.put_agent(agent, by).await
    }

    async fn remove_agent(
        &self,
        owner: &AgentOwner,
        id: &AgentId,
        by: &By,
    ) -> Result<(), StorageError> {
        self.answering()?;
        self.inner.remove_agent(owner, id, by).await
    }

    async fn agent(
        &self,
        owner: &AgentOwner,
        id: &AgentId,
    ) -> Result<Option<StoredAgent>, StorageError> {
        self.answering()?;
        self.inner.agent(owner, id).await
    }

    async fn agents(
        &self,
        owner: &AgentOwner,
        page: Page,
    ) -> Result<Vec<StoredAgent>, StorageError> {
        self.answering()?;
        self.inner.agents(owner, page).await
    }
}

#[async_trait]
impl McpStore for FailingStore {
    async fn put_mcp(&self, server: McpServer, by: &By) -> Result<StoredMcp, StorageError> {
        self.answering()?;
        self.inner.put_mcp(server, by).await
    }

    async fn remove_mcp(&self, id: &McpId, by: &By) -> Result<(), StorageError> {
        self.answering()?;
        self.inner.remove_mcp(id, by).await
    }

    async fn mcp(&self, id: &McpId) -> Result<Option<StoredMcp>, StorageError> {
        self.answering()?;
        self.inner.mcp(id).await
    }

    async fn mcps(&self, page: Page) -> Result<Vec<StoredMcp>, StorageError> {
        self.answering()?;
        self.inner.mcps(page).await
    }
}

#[cfg(all(test, feature = "standalone-storage"))]
mod tests {
    use crate::sqlite::SqliteStore;

    use super::*;

    async fn failing() -> FailingStore {
        let store = SqliteStore::in_memory().await.unwrap();
        FailingStore::new(Arc::new(store))
    }

    fn told_to_fail(error: &StorageError) -> bool {
        matches!(error, StorageError::Backend(source) if source.is::<ToldToFail>())
    }

    #[tokio::test]
    async fn a_store_answers_until_it_is_told_to_stop() {
        let store = failing().await;
        assert!(store.rule_revision().await.is_ok());

        store.fail_now();
        let refused = store.rule_revision().await.unwrap_err();
        assert!(told_to_fail(&refused), "{refused:?}");
    }

    #[tokio::test]
    async fn the_call_it_fails_on_is_the_one_it_was_given() {
        let store = failing().await;
        store.answer_only(2);
        assert!(store.rule_revision().await.is_ok());
        assert!(store.routes().await.is_ok());
        assert!(told_to_fail(&store.routes().await.unwrap_err()));
    }

    #[tokio::test]
    async fn a_store_told_to_answer_again_does() {
        let store = failing().await;
        store.fail_now();
        store.rule_revision().await.unwrap_err();
        store.answer_again();
        assert!(store.rule_revision().await.is_ok());
    }

    #[tokio::test]
    async fn what_it_wraps_still_answers_through_it() {
        let store = failing().await;
        let tenant = crate::model::NewTenant {
            id: TenantId::new("acme").unwrap(),
            key: ip_core::TnKey::generate().unwrap(),
        };
        store
            .create_tenant(tenant, &crate::suite::identity::tenants_by())
            .await
            .unwrap();
        assert!(
            store
                .tenant(&TenantId::new("acme").unwrap())
                .await
                .unwrap()
                .is_some()
        );
    }
}
