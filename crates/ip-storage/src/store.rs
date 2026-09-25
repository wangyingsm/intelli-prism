use async_trait::async_trait;
use ip_core::{Grant, Grants, PassphraseHash, TenantId, UserId};

use crate::error::StorageError;
use crate::limit::LimitStore;
use crate::list::ListStore;
use crate::model::{Membership, NewTenant, NewUser, Tenant, User};
use crate::plugin::{PluginRuleStore, PluginStore};
use crate::revision::RevisionStore;
use crate::route::RouteStore;
use crate::usage::UsageStore;

/// Reads and writes tenants.
#[async_trait]
pub trait TenantStore: Send + Sync {
    /// Creates a tenant, or reports a conflict when the id is taken.
    async fn create_tenant(&self, new: NewTenant) -> Result<Tenant, StorageError>;

    /// Reads one tenant.
    async fn tenant(&self, id: &TenantId) -> Result<Option<Tenant>, StorageError>;

    /// Deletes a tenant, or reports it missing.
    async fn delete_tenant(&self, id: &TenantId) -> Result<(), StorageError>;
}

/// Reads and writes users.
#[async_trait]
pub trait UserStore: Send + Sync {
    /// Creates a user, or reports a conflict when the id is taken.
    async fn create_user(&self, new: NewUser) -> Result<User, StorageError>;

    /// Reads one user.
    async fn user(&self, id: &UserId) -> Result<Option<User>, StorageError>;

    /// Replaces a user's passphrase verifier.
    async fn set_passphrase(
        &self,
        id: &UserId,
        passphrase: &PassphraseHash,
    ) -> Result<(), StorageError>;

    /// Deletes a user, or reports it missing.
    async fn delete_user(&self, id: &UserId) -> Result<(), StorageError>;
}

/// Reads and writes the many to many attachment of users to tenants.
///
/// Detaching is not here: it must revoke what the user held inside the tenant in the same
/// transaction, so it is `MemberRemoveTransactional`'s alone.
#[async_trait]
pub trait MembershipStore: Send + Sync {
    /// Attaches a user to a tenant, replacing any standing it already had there.
    async fn attach(&self, membership: Membership) -> Result<(), StorageError>;

    /// Reads one user's standing inside one tenant.
    async fn membership(
        &self,
        user: &UserId,
        tenant: &TenantId,
    ) -> Result<Option<Membership>, StorageError>;

    /// Every tenant one user is attached to.
    async fn memberships_of_user(&self, user: &UserId) -> Result<Vec<Membership>, StorageError>;

    /// Every user attached to one tenant.
    async fn members_of_tenant(&self, tenant: &TenantId) -> Result<Vec<Membership>, StorageError>;
}

/// Reads and writes explicit capability grants.
#[async_trait]
pub trait GrantStore: Send + Sync {
    /// Records a grant. Granting what is already held changes nothing.
    async fn grant(&self, grant: &Grant) -> Result<(), StorageError>;

    /// Removes a grant, or reports it missing.
    async fn revoke(&self, grant: &Grant) -> Result<(), StorageError>;

    /// Every grant one user holds, at every scope.
    async fn grants_of(&self, user: &UserId) -> Result<Grants, StorageError>;

    /// Every grant one user holds inside one tenant.
    async fn grants_in_tenant(
        &self,
        user: &UserId,
        tenant: &TenantId,
    ) -> Result<Grants, StorageError>;
}

/// Everything the identity layer needs from storage, behind one object.
pub trait Storage: TenantStore + UserStore + MembershipStore + GrantStore {}

impl<T> Storage for T where T: TenantStore + UserStore + MembershipStore + GrantStore {}

/// Everything a storage backend provides, behind one object.
pub trait Backend:
    Storage
    + RouteStore
    + PluginStore
    + PluginRuleStore
    + ListStore
    + RevisionStore
    + UsageStore
    + LimitStore
{
}

impl<T> Backend for T where
    T: Storage
        + RouteStore
        + PluginStore
        + PluginRuleStore
        + ListStore
        + RevisionStore
        + UsageStore
        + LimitStore
{
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::{Arc, Mutex};

    use ip_core::{ApiId, Capability, CapabilityScope, PassphraseHash, Timestamp, TnKey};

    use super::*;
    use crate::error::Entity;
    use crate::model::{AccountKind, Standing, TenantRowId, UserRowId};

    #[derive(Default)]
    struct MemoryStore {
        tenants: Mutex<HashMap<TenantId, Tenant>>,
        users: Mutex<HashMap<UserId, User>>,
        memberships: Mutex<Vec<Membership>>,
        grants: Mutex<Vec<Grant>>,
        next_row_id: AtomicI64,
    }

    impl MemoryStore {
        fn next_row_id(&self) -> i64 {
            self.next_row_id.fetch_add(1, Ordering::Relaxed) + 1
        }
    }

    #[async_trait]
    impl TenantStore for MemoryStore {
        async fn create_tenant(&self, new: NewTenant) -> Result<Tenant, StorageError> {
            let mut tenants = self.tenants.lock().unwrap();
            if tenants.contains_key(&new.id) {
                return Err(StorageError::Conflict {
                    entity: Entity::Tenant,
                    id: new.id.to_string(),
                });
            }
            let tenant = Tenant {
                row_id: TenantRowId::new(self.next_row_id()),
                id: new.id.clone(),
                key: new.key,
                created_at: Timestamp::now(),
            };
            tenants.insert(new.id, tenant.clone());
            Ok(tenant)
        }

        async fn tenant(&self, id: &TenantId) -> Result<Option<Tenant>, StorageError> {
            Ok(self.tenants.lock().unwrap().get(id).cloned())
        }

        async fn delete_tenant(&self, id: &TenantId) -> Result<(), StorageError> {
            self.tenants
                .lock()
                .unwrap()
                .remove(id)
                .map(|_| ())
                .ok_or_else(|| StorageError::NotFound {
                    entity: Entity::Tenant,
                    id: id.to_string(),
                })
        }
    }

    #[async_trait]
    impl UserStore for MemoryStore {
        async fn create_user(&self, new: NewUser) -> Result<User, StorageError> {
            let mut users = self.users.lock().unwrap();
            if users.contains_key(&new.id) {
                return Err(StorageError::Conflict {
                    entity: Entity::User,
                    id: new.id.to_string(),
                });
            }
            let user = User {
                row_id: UserRowId::new(self.next_row_id()),
                id: new.id.clone(),
                passphrase: new.passphrase,
                kind: new.kind,
                created_at: Timestamp::now(),
            };
            users.insert(new.id, user.clone());
            Ok(user)
        }

        async fn user(&self, id: &UserId) -> Result<Option<User>, StorageError> {
            Ok(self.users.lock().unwrap().get(id).cloned())
        }

        async fn set_passphrase(
            &self,
            id: &UserId,
            passphrase: &PassphraseHash,
        ) -> Result<(), StorageError> {
            match self.users.lock().unwrap().get_mut(id) {
                Some(user) => {
                    user.passphrase = passphrase.clone();
                    Ok(())
                }
                None => Err(StorageError::NotFound {
                    entity: Entity::User,
                    id: id.to_string(),
                }),
            }
        }

        async fn delete_user(&self, id: &UserId) -> Result<(), StorageError> {
            self.users
                .lock()
                .unwrap()
                .remove(id)
                .map(|_| ())
                .ok_or_else(|| StorageError::NotFound {
                    entity: Entity::User,
                    id: id.to_string(),
                })
        }
    }

    #[async_trait]
    impl MembershipStore for MemoryStore {
        async fn attach(&self, membership: Membership) -> Result<(), StorageError> {
            let mut memberships = self.memberships.lock().unwrap();
            memberships
                .retain(|held| held.user != membership.user || held.tenant != membership.tenant);
            memberships.push(membership);
            Ok(())
        }

        async fn membership(
            &self,
            user: &UserId,
            tenant: &TenantId,
        ) -> Result<Option<Membership>, StorageError> {
            Ok(self
                .memberships
                .lock()
                .unwrap()
                .iter()
                .find(|held| &held.user == user && &held.tenant == tenant)
                .cloned())
        }

        async fn memberships_of_user(
            &self,
            user: &UserId,
        ) -> Result<Vec<Membership>, StorageError> {
            Ok(self
                .memberships
                .lock()
                .unwrap()
                .iter()
                .filter(|held| &held.user == user)
                .cloned()
                .collect())
        }

        async fn members_of_tenant(
            &self,
            tenant: &TenantId,
        ) -> Result<Vec<Membership>, StorageError> {
            Ok(self
                .memberships
                .lock()
                .unwrap()
                .iter()
                .filter(|held| &held.tenant == tenant)
                .cloned()
                .collect())
        }
    }

    #[async_trait]
    impl GrantStore for MemoryStore {
        async fn grant(&self, grant: &Grant) -> Result<(), StorageError> {
            let mut grants = self.grants.lock().unwrap();
            if !grants.contains(grant) {
                grants.push(grant.clone());
            }
            Ok(())
        }

        async fn revoke(&self, grant: &Grant) -> Result<(), StorageError> {
            let mut grants = self.grants.lock().unwrap();
            let before = grants.len();
            grants.retain(|held| held != grant);
            if grants.len() == before {
                return Err(StorageError::NotFound {
                    entity: Entity::Grant,
                    id: format!("{:?}", grant.capability()),
                });
            }
            Ok(())
        }

        async fn grants_of(&self, user: &UserId) -> Result<Grants, StorageError> {
            Ok(self
                .grants
                .lock()
                .unwrap()
                .iter()
                .filter(|held| held.scope().user() == user)
                .cloned()
                .collect())
        }

        async fn grants_in_tenant(
            &self,
            user: &UserId,
            tenant: &TenantId,
        ) -> Result<Grants, StorageError> {
            Ok(self
                .grants
                .lock()
                .unwrap()
                .iter()
                .filter(|held| held.scope().user() == user && held.scope().tenant() == Some(tenant))
                .cloned()
                .collect())
        }
    }

    fn store() -> Arc<dyn Storage> {
        Arc::new(MemoryStore::default())
    }

    fn user_id() -> UserId {
        UserId::new("alice").unwrap()
    }

    fn tenant_id() -> TenantId {
        TenantId::new("acme").unwrap()
    }

    fn new_user() -> NewUser {
        NewUser {
            id: user_id(),
            passphrase: PassphraseHash::new("$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA")
                .unwrap(),
            kind: AccountKind::Regular,
        }
    }

    fn new_tenant() -> NewTenant {
        NewTenant {
            id: tenant_id(),
            key: TnKey::generate().unwrap(),
        }
    }

    #[tokio::test]
    async fn the_whole_surface_is_reachable_through_one_trait_object() {
        let store = store();
        let tenant = store.create_tenant(new_tenant()).await.unwrap();
        let user = store.create_user(new_user()).await.unwrap();
        store
            .attach(Membership {
                user: user.id.clone(),
                tenant: tenant.id.clone(),
                standing: Standing::Owner,
            })
            .await
            .unwrap();
        let grant = Grant::new(
            Capability::ApiAccess,
            CapabilityScope::Api {
                user: user.id.clone(),
                tenant: tenant.id.clone(),
                api: ApiId::new("chat").unwrap(),
            },
        )
        .unwrap();
        store.grant(&grant).await.unwrap();

        assert_eq!(
            store.tenant(&tenant.id).await.unwrap(),
            Some(tenant.clone())
        );
        assert_eq!(store.user(&user.id).await.unwrap(), Some(user.clone()));
        assert_eq!(
            store
                .membership(&user.id, &tenant.id)
                .await
                .unwrap()
                .map(|held| held.standing),
            Some(Standing::Owner)
        );
        assert!(
            store
                .grants_in_tenant(&user.id, &tenant.id)
                .await
                .unwrap()
                .holds(Capability::ApiAccess, grant.scope())
        );
    }

    #[tokio::test]
    async fn every_created_row_gets_its_own_primary_key() {
        let store = store();
        let tenant = store.create_tenant(new_tenant()).await.unwrap();
        let user = store.create_user(new_user()).await.unwrap();
        assert_eq!(tenant.row_id, TenantRowId::new(1));
        assert_eq!(user.row_id, UserRowId::new(2));
    }

    #[tokio::test]
    async fn creating_the_same_tenant_twice_conflicts() {
        let store = store();
        store.create_tenant(new_tenant()).await.unwrap();
        assert!(matches!(
            store.create_tenant(new_tenant()).await,
            Err(StorageError::Conflict {
                entity: Entity::Tenant,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn deleting_what_is_absent_reports_it_missing() {
        let store = store();
        assert!(matches!(
            store.delete_user(&user_id()).await,
            Err(StorageError::NotFound {
                entity: Entity::User,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn attaching_twice_replaces_the_standing() {
        let store = store();
        for standing in [Standing::Owner, Standing::Member] {
            store
                .attach(Membership {
                    user: user_id(),
                    tenant: tenant_id(),
                    standing,
                })
                .await
                .unwrap();
        }
        assert_eq!(
            store.memberships_of_user(&user_id()).await.unwrap().len(),
            1
        );
        assert_eq!(
            store
                .membership(&user_id(), &tenant_id())
                .await
                .unwrap()
                .unwrap()
                .standing,
            Standing::Member
        );
    }
}
