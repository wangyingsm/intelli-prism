//! A store that fails when it is told to, for the arms a working one never reaches.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use ip_core::{
    Checksum, Counted, Grant, Grants, LimitScope, NewPluginRule, PassphraseHash, Period,
    PluginKind, PluginOrder, PluginRule, RouteKey, RouteRule, TenantId, Timestamp, UserId,
};

use crate::error::{StorageError, ToldToFail};
use crate::limit::{Limit, LimitStore, NewLimit};
use crate::list::{ListStore, Listed, Page};
use crate::model::{Membership, NewTenant, NewUser, Tenant, User};
use crate::plugin::{NewPlugin, Plugin, PluginOwner, PluginRecord, PluginRuleStore, PluginStore};
use crate::revision::{RevisionStore, RuleRevision, RuleSet};
use crate::route::RouteStore;
use crate::store::{Backend, GrantStore, MembershipStore, TenantStore, UserStore};
use crate::usage::{NewUsage, Usage, UsageFilter, UsageRowId, UsageStore};

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
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
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
    async fn create_tenant(&self, new: NewTenant) -> Result<Tenant, StorageError> {
        self.answering()?;
        self.inner.create_tenant(new).await
    }

    async fn tenant(&self, id: &TenantId) -> Result<Option<Tenant>, StorageError> {
        self.answering()?;
        self.inner.tenant(id).await
    }

    async fn delete_tenant(&self, id: &TenantId) -> Result<(), StorageError> {
        self.answering()?;
        self.inner.delete_tenant(id).await
    }
}

#[async_trait]
impl UserStore for FailingStore {
    async fn create_user(&self, new: NewUser) -> Result<User, StorageError> {
        self.answering()?;
        self.inner.create_user(new).await
    }

    async fn user(&self, id: &UserId) -> Result<Option<User>, StorageError> {
        self.answering()?;
        self.inner.user(id).await
    }

    async fn set_passphrase(
        &self,
        id: &UserId,
        passphrase: &PassphraseHash,
    ) -> Result<(), StorageError> {
        self.answering()?;
        self.inner.set_passphrase(id, passphrase).await
    }

    async fn delete_user(&self, id: &UserId) -> Result<(), StorageError> {
        self.answering()?;
        self.inner.delete_user(id).await
    }
}

#[async_trait]
impl MembershipStore for FailingStore {
    async fn attach(&self, membership: Membership) -> Result<(), StorageError> {
        self.answering()?;
        self.inner.attach(membership).await
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
    async fn grant(&self, grant: &Grant) -> Result<(), StorageError> {
        self.answering()?;
        self.inner.grant(grant).await
    }

    async fn revoke(&self, grant: &Grant) -> Result<(), StorageError> {
        self.answering()?;
        self.inner.revoke(grant).await
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
    async fn put_route(&self, rule: RouteRule) -> Result<(), StorageError> {
        self.answering()?;
        self.inner.put_route(rule).await
    }

    async fn remove_route(&self, key: &RouteKey) -> Result<(), StorageError> {
        self.answering()?;
        self.inner.remove_route(key).await
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
    async fn put_plugin(&self, new: NewPlugin) -> Result<PluginRecord, StorageError> {
        self.answering()?;
        self.inner.put_plugin(new).await
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
    ) -> Result<bool, StorageError> {
        self.answering()?;
        self.inner.disown_plugin(checksum, owner).await
    }
}

#[async_trait]
impl PluginRuleStore for FailingStore {
    async fn put_rule(&self, rule: NewPluginRule) -> Result<PluginRule, StorageError> {
        self.answering()?;
        self.inner.put_rule(rule).await
    }

    async fn remove_rule(
        &self,
        tenant: Option<&TenantId>,
        kind: PluginKind,
        order: PluginOrder,
    ) -> Result<(), StorageError> {
        self.answering()?;
        self.inner.remove_rule(tenant, kind, order).await
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
}

#[async_trait]
impl LimitStore for FailingStore {
    async fn put_limit(&self, limit: NewLimit) -> Result<Limit, StorageError> {
        self.answering()?;
        self.inner.put_limit(limit).await
    }

    async fn remove_limit(
        &self,
        scope: &LimitScope,
        counted: Counted,
        period: Period,
    ) -> Result<(), StorageError> {
        self.answering()?;
        self.inner.remove_limit(scope, counted, period).await
    }

    async fn limits(&self) -> Result<Vec<Limit>, StorageError> {
        self.answering()?;
        self.inner.limits().await
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
        store.create_tenant(tenant).await.unwrap();
        assert!(
            store
                .tenant(&TenantId::new("acme").unwrap())
                .await
                .unwrap()
                .is_some()
        );
    }
}
