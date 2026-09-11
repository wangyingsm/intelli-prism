use async_trait::async_trait;
use ip_core::{
    ApiId, Capability, CapabilityScope, Grant, Grants, PassphraseHash, TenantId, Timestamp, TnKey,
    UserId,
};
use sqlx::Row;

use super::{SqliteStore, is_unique_violation};
use crate::error::{Entity, StorageError};
use crate::model::{
    AccountKind, Membership, NewTenant, NewUser, Standing, Tenant, TenantRowId, User, UserRowId,
};
use crate::store::{GrantStore, MembershipStore, TenantStore, UserStore};

fn tenant_key(bytes: Vec<u8>) -> Result<TnKey, StorageError> {
    let len = bytes.len();
    let bytes: [u8; ip_core::key::TN_KEY_BYTES] =
        bytes.try_into().map_err(|_| StorageError::Malformed {
            entity: Entity::Tenant,
            detail: format!("key is {len} bytes"),
        })?;
    Ok(TnKey::from(bytes))
}

fn account_kind_name(kind: AccountKind) -> &'static str {
    match kind {
        AccountKind::SystemAdministrator => "sysadmin",
        AccountKind::Regular => "regular",
    }
}

fn account_kind(name: &str) -> Result<AccountKind, StorageError> {
    match name {
        "sysadmin" => Ok(AccountKind::SystemAdministrator),
        "regular" => Ok(AccountKind::Regular),
        other => Err(StorageError::Malformed {
            entity: Entity::User,
            detail: format!("unknown account kind {other:?}"),
        }),
    }
}

fn standing_name(standing: Standing) -> &'static str {
    match standing {
        Standing::Owner => "owner",
        Standing::Member => "member",
    }
}

fn standing(name: &str) -> Result<Standing, StorageError> {
    match name {
        "owner" => Ok(Standing::Owner),
        "member" => Ok(Standing::Member),
        other => Err(StorageError::Malformed {
            entity: Entity::Membership,
            detail: format!("unknown standing {other:?}"),
        }),
    }
}

fn capability_name(capability: Capability) -> &'static str {
    match capability {
        Capability::TenantMgr => "tenant_mgr",
        Capability::UserMgr => "user_mgr",
        Capability::ApiAccess => "api_access",
        Capability::ApiAdvMgr => "api_adv_mgr",
        Capability::LimitMgr => "limit_mgr",
        Capability::SysAgent => "sys_agent",
        Capability::Observer => "observer",
    }
}

fn capability(name: &str) -> Result<Capability, StorageError> {
    match name {
        "tenant_mgr" => Ok(Capability::TenantMgr),
        "user_mgr" => Ok(Capability::UserMgr),
        "api_access" => Ok(Capability::ApiAccess),
        "api_adv_mgr" => Ok(Capability::ApiAdvMgr),
        "limit_mgr" => Ok(Capability::LimitMgr),
        "sys_agent" => Ok(Capability::SysAgent),
        "observer" => Ok(Capability::Observer),
        other => Err(StorageError::Malformed {
            entity: Entity::Grant,
            detail: format!("unknown capability {other:?}"),
        }),
    }
}

/// Rebuilds the scope a grant row was written from, so a nonsensical row is rejected on read.
fn scope(
    user: &UserId,
    tenant: Option<String>,
    api: Option<String>,
) -> Result<CapabilityScope, StorageError> {
    let user = user.clone();
    Ok(match (tenant, api) {
        (None, None) => CapabilityScope::User { user },
        (Some(tenant), None) => CapabilityScope::Tenant {
            user,
            tenant: TenantId::new(&tenant)?,
        },
        (Some(tenant), Some(api)) => CapabilityScope::Api {
            user,
            tenant: TenantId::new(&tenant)?,
            api: ApiId::new(&api)?,
        },
        (None, Some(api)) => {
            return Err(StorageError::Malformed {
                entity: Entity::Grant,
                detail: format!("api {api} is scoped to no tenant"),
            });
        }
    })
}

#[async_trait]
impl TenantStore for SqliteStore {
    async fn create_tenant(&self, new: NewTenant) -> Result<Tenant, StorageError> {
        let created_at = Timestamp::now();
        let result = sqlx::query(
            "INSERT INTO tenants (id, key, created_at) VALUES (?, ?, ?) RETURNING row_id",
        )
        .bind(new.id.as_str())
        .bind(new.key.as_bytes().as_slice())
        .bind(created_at.unix_seconds())
        .fetch_one(&self.pool)
        .await;
        match result {
            Ok(row) => Ok(Tenant {
                row_id: TenantRowId::new(row.get("row_id")),
                id: new.id,
                key: new.key,
                created_at,
            }),
            Err(error) if is_unique_violation(&error) => Err(StorageError::Conflict {
                entity: Entity::Tenant,
                id: new.id.to_string(),
            }),
            Err(error) => Err(StorageError::backend(error)),
        }
    }

    async fn tenant(&self, id: &TenantId) -> Result<Option<Tenant>, StorageError> {
        let Some(row) = sqlx::query("SELECT row_id, key, created_at FROM tenants WHERE id = ?")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(StorageError::backend)?
        else {
            return Ok(None);
        };
        Ok(Some(Tenant {
            row_id: TenantRowId::new(row.get("row_id")),
            id: id.clone(),
            key: tenant_key(row.get("key"))?,
            created_at: Timestamp::from_unix_seconds(row.get("created_at"))?,
        }))
    }

    async fn delete_tenant(&self, id: &TenantId) -> Result<(), StorageError> {
        let deleted = sqlx::query("DELETE FROM tenants WHERE id = ?")
            .bind(id.as_str())
            .execute(&self.pool)
            .await
            .map_err(StorageError::backend)?
            .rows_affected();
        if deleted == 0 {
            return Err(StorageError::NotFound {
                entity: Entity::Tenant,
                id: id.to_string(),
            });
        }
        Ok(())
    }
}

#[async_trait]
impl UserStore for SqliteStore {
    async fn create_user(&self, new: NewUser) -> Result<User, StorageError> {
        let created_at = Timestamp::now();
        let result = sqlx::query(
            "INSERT INTO users (id, passphrase, kind, created_at) VALUES (?, ?, ?, ?) RETURNING row_id",
        )
        .bind(new.id.as_str())
        .bind(new.passphrase.as_str())
        .bind(account_kind_name(new.kind))
        .bind(created_at.unix_seconds())
        .fetch_one(&self.pool)
        .await;
        match result {
            Ok(row) => Ok(User {
                row_id: UserRowId::new(row.get("row_id")),
                id: new.id,
                passphrase: new.passphrase,
                kind: new.kind,
                created_at,
            }),
            Err(error) if is_unique_violation(&error) => Err(StorageError::Conflict {
                entity: Entity::User,
                id: new.id.to_string(),
            }),
            Err(error) => Err(StorageError::backend(error)),
        }
    }

    async fn user(&self, id: &UserId) -> Result<Option<User>, StorageError> {
        let Some(row) =
            sqlx::query("SELECT row_id, passphrase, kind, created_at FROM users WHERE id = ?")
                .bind(id.as_str())
                .fetch_optional(&self.pool)
                .await
                .map_err(StorageError::backend)?
        else {
            return Ok(None);
        };
        Ok(Some(User {
            row_id: UserRowId::new(row.get("row_id")),
            id: id.clone(),
            passphrase: PassphraseHash::new(row.get("passphrase"))?,
            kind: account_kind(row.get("kind"))?,
            created_at: Timestamp::from_unix_seconds(row.get("created_at"))?,
        }))
    }

    async fn set_passphrase(
        &self,
        id: &UserId,
        passphrase: &PassphraseHash,
    ) -> Result<(), StorageError> {
        let updated = sqlx::query("UPDATE users SET passphrase = ? WHERE id = ?")
            .bind(passphrase.as_str())
            .bind(id.as_str())
            .execute(&self.pool)
            .await
            .map_err(StorageError::backend)?
            .rows_affected();
        if updated == 0 {
            return Err(StorageError::NotFound {
                entity: Entity::User,
                id: id.to_string(),
            });
        }
        Ok(())
    }

    async fn delete_user(&self, id: &UserId) -> Result<(), StorageError> {
        let deleted = sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(id.as_str())
            .execute(&self.pool)
            .await
            .map_err(StorageError::backend)?
            .rows_affected();
        if deleted == 0 {
            return Err(StorageError::NotFound {
                entity: Entity::User,
                id: id.to_string(),
            });
        }
        Ok(())
    }
}

#[async_trait]
impl MembershipStore for SqliteStore {
    async fn attach(&self, membership: Membership) -> Result<(), StorageError> {
        let tenant_row_id = self.tenant_row_id(&membership.tenant).await?;
        let user_row_id = self.user_row_id(&membership.user).await?;
        sqlx::query(
            "INSERT INTO memberships (tenant_row_id, user_row_id, standing) VALUES (?, ?, ?) \
             ON CONFLICT (tenant_row_id, user_row_id) DO UPDATE SET standing = excluded.standing",
        )
        .bind(tenant_row_id.get())
        .bind(user_row_id.get())
        .bind(standing_name(membership.standing))
        .execute(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        Ok(())
    }

    async fn detach(&self, user: &UserId, tenant: &TenantId) -> Result<(), StorageError> {
        let deleted = sqlx::query(
            "DELETE FROM memberships WHERE tenant_row_id = (SELECT row_id FROM tenants WHERE id = ?) \
             AND user_row_id = (SELECT row_id FROM users WHERE id = ?)",
        )
        .bind(tenant.as_str())
        .bind(user.as_str())
        .execute(&self.pool)
        .await
        .map_err(StorageError::backend)?
        .rows_affected();
        if deleted == 0 {
            return Err(StorageError::NotFound {
                entity: Entity::Membership,
                id: format!("{user}@{tenant}"),
            });
        }
        Ok(())
    }

    async fn membership(
        &self,
        user: &UserId,
        tenant: &TenantId,
    ) -> Result<Option<Membership>, StorageError> {
        let Some(row) = sqlx::query(
            "SELECT m.standing FROM memberships m \
             JOIN tenants t ON t.row_id = m.tenant_row_id \
             JOIN users u ON u.row_id = m.user_row_id \
             WHERE t.id = ? AND u.id = ?",
        )
        .bind(tenant.as_str())
        .bind(user.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(StorageError::backend)?
        else {
            return Ok(None);
        };
        Ok(Some(Membership {
            user: user.clone(),
            tenant: tenant.clone(),
            standing: standing(row.get("standing"))?,
        }))
    }

    async fn memberships_of_user(&self, user: &UserId) -> Result<Vec<Membership>, StorageError> {
        let rows = sqlx::query(
            "SELECT t.id AS tenant_id, m.standing FROM memberships m \
             JOIN tenants t ON t.row_id = m.tenant_row_id \
             JOIN users u ON u.row_id = m.user_row_id \
             WHERE u.id = ? ORDER BY t.id",
        )
        .bind(user.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        rows.into_iter()
            .map(|row| {
                Ok(Membership {
                    user: user.clone(),
                    tenant: TenantId::new(row.get("tenant_id"))?,
                    standing: standing(row.get("standing"))?,
                })
            })
            .collect()
    }

    async fn members_of_tenant(&self, tenant: &TenantId) -> Result<Vec<Membership>, StorageError> {
        let rows = sqlx::query(
            "SELECT u.id AS user_id, m.standing FROM memberships m \
             JOIN tenants t ON t.row_id = m.tenant_row_id \
             JOIN users u ON u.row_id = m.user_row_id \
             WHERE t.id = ? ORDER BY u.id",
        )
        .bind(tenant.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        rows.into_iter()
            .map(|row| {
                Ok(Membership {
                    user: UserId::new(row.get("user_id"))?,
                    tenant: tenant.clone(),
                    standing: standing(row.get("standing"))?,
                })
            })
            .collect()
    }
}

#[async_trait]
impl GrantStore for SqliteStore {
    async fn grant(&self, grant: &Grant) -> Result<(), StorageError> {
        let user_row_id = self.user_row_id(grant.scope().user()).await?;
        let tenant_row_id = match grant.scope().tenant() {
            Some(tenant) => Some(self.tenant_row_id(tenant).await?.get()),
            None => None,
        };
        let result = sqlx::query(
            "INSERT INTO grants (user_row_id, tenant_row_id, api_id, capability) VALUES (?, ?, ?, ?)",
        )
        .bind(user_row_id.get())
        .bind(tenant_row_id)
        .bind(grant.scope().api().map(|api| api.as_str()))
        .bind(capability_name(grant.capability()))
        .execute(&self.pool)
        .await;
        match result {
            Ok(_) => Ok(()),
            Err(error) if is_unique_violation(&error) => Ok(()),
            Err(error) => Err(StorageError::backend(error)),
        }
    }

    async fn revoke(&self, grant: &Grant) -> Result<(), StorageError> {
        let user_row_id = self.user_row_id(grant.scope().user()).await?;
        let tenant_row_id = match grant.scope().tenant() {
            Some(tenant) => Some(self.tenant_row_id(tenant).await?.get()),
            None => None,
        };
        let deleted = sqlx::query(
            "DELETE FROM grants WHERE user_row_id = ? AND tenant_row_id IS ? \
             AND api_id IS ? AND capability = ?",
        )
        .bind(user_row_id.get())
        .bind(tenant_row_id)
        .bind(grant.scope().api().map(|api| api.as_str()))
        .bind(capability_name(grant.capability()))
        .execute(&self.pool)
        .await
        .map_err(StorageError::backend)?
        .rows_affected();
        if deleted == 0 {
            return Err(StorageError::NotFound {
                entity: Entity::Grant,
                id: format!("{:?}", grant.capability()),
            });
        }
        Ok(())
    }

    async fn grants_of(&self, user: &UserId) -> Result<Grants, StorageError> {
        let rows = sqlx::query(
            "SELECT t.id AS tenant_id, g.api_id, g.capability FROM grants g \
             JOIN users u ON u.row_id = g.user_row_id \
             LEFT JOIN tenants t ON t.row_id = g.tenant_row_id \
             WHERE u.id = ?",
        )
        .bind(user.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        rows.into_iter()
            .map(|row| {
                let scope = scope(user, row.get("tenant_id"), row.get("api_id"))?;
                Ok(Grant::new(capability(row.get("capability"))?, scope)?)
            })
            .collect()
    }

    async fn grants_in_tenant(
        &self,
        user: &UserId,
        tenant: &TenantId,
    ) -> Result<Grants, StorageError> {
        let rows = sqlx::query(
            "SELECT t.id AS tenant_id, g.api_id, g.capability FROM grants g \
             JOIN users u ON u.row_id = g.user_row_id \
             JOIN tenants t ON t.row_id = g.tenant_row_id \
             WHERE u.id = ? AND t.id = ?",
        )
        .bind(user.as_str())
        .bind(tenant.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        rows.into_iter()
            .map(|row| {
                let scope = scope(user, row.get("tenant_id"), row.get("api_id"))?;
                Ok(Grant::new(capability(row.get("capability"))?, scope)?)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::fixture::*;

    fn api_grant(user: &UserId, tenant: &TenantId, capability: Capability) -> Grant {
        Grant::new(
            capability,
            CapabilityScope::Api {
                user: user.clone(),
                tenant: tenant.clone(),
                api: ApiId::new("chat").unwrap(),
            },
        )
        .unwrap()
    }

    #[tokio::test]
    async fn a_fresh_database_is_migrated_and_empty() {
        let store = store().await;
        assert_eq!(store.tenant(&tenant_id()).await.unwrap(), None);
        assert_eq!(store.user(&user_id()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_tenant_round_trips_with_its_key_intact() {
        let store = store().await;
        let created = store.create_tenant(new_tenant()).await.unwrap();
        let read = store.tenant(&tenant_id()).await.unwrap().unwrap();
        assert_eq!(read.row_id, created.row_id);
        assert_eq!(read.key, created.key);
        assert_eq!(read.created_at, created.created_at);
    }

    #[tokio::test]
    async fn the_primary_key_is_an_integer_the_backend_assigns() {
        let store = store().await;
        let first = store.create_tenant(new_tenant()).await.unwrap();
        let second = store
            .create_tenant(NewTenant {
                id: TenantId::new("globex").unwrap(),
                key: TnKey::generate().unwrap(),
            })
            .await
            .unwrap();
        assert_eq!(first.row_id, TenantRowId::new(1));
        assert_eq!(second.row_id, TenantRowId::new(2));
    }

    #[tokio::test]
    async fn a_repeated_tenant_id_conflicts() {
        let store = store().await;
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
    async fn a_repeated_user_id_conflicts() {
        let store = store().await;
        store.create_user(new_user()).await.unwrap();
        assert!(matches!(
            store.create_user(new_user()).await,
            Err(StorageError::Conflict {
                entity: Entity::User,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_user_round_trips_and_its_passphrase_can_be_replaced() {
        let store = store().await;
        let created = store.create_user(new_user()).await.unwrap();
        assert_eq!(store.user(&user_id()).await.unwrap(), Some(created));
        let replacement =
            PassphraseHash::new("$argon2id$v=19$m=19456,t=2,p=1$b3RoZXI$b3RoZXJoYXNo").unwrap();
        store
            .set_passphrase(&user_id(), &replacement)
            .await
            .unwrap();
        assert_eq!(
            store.user(&user_id()).await.unwrap().unwrap().passphrase,
            replacement
        );
    }

    #[tokio::test]
    async fn the_account_kind_survives_a_round_trip() {
        let store = store().await;
        store
            .create_user(NewUser {
                id: user_id(),
                passphrase: hash(),
                kind: AccountKind::SystemAdministrator,
            })
            .await
            .unwrap();
        assert_eq!(
            store.user(&user_id()).await.unwrap().unwrap().kind,
            AccountKind::SystemAdministrator
        );
    }

    #[tokio::test]
    async fn deleting_what_is_absent_reports_it_missing() {
        let store = store().await;
        assert!(matches!(
            store.delete_tenant(&tenant_id()).await,
            Err(StorageError::NotFound {
                entity: Entity::Tenant,
                ..
            })
        ));
        assert!(matches!(
            store.set_passphrase(&user_id(), &hash()).await,
            Err(StorageError::NotFound {
                entity: Entity::User,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn attaching_twice_replaces_the_standing() {
        let store = store().await;
        tenant_with_user(&store).await;
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
        let held = store.memberships_of_user(&user_id()).await.unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].standing, Standing::Member);
    }

    #[tokio::test]
    async fn attaching_to_a_tenant_that_is_not_there_reports_it_missing() {
        let store = store().await;
        store.create_user(new_user()).await.unwrap();
        assert!(matches!(
            store
                .attach(Membership {
                    user: user_id(),
                    tenant: tenant_id(),
                    standing: Standing::Member,
                })
                .await,
            Err(StorageError::NotFound {
                entity: Entity::Tenant,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_tenant_is_listed_from_both_sides() {
        let store = store().await;
        tenant_with_user(&store).await;
        store
            .attach(Membership {
                user: user_id(),
                tenant: tenant_id(),
                standing: Standing::Owner,
            })
            .await
            .unwrap();
        assert_eq!(
            store.memberships_of_user(&user_id()).await.unwrap().len(),
            1
        );
        assert_eq!(
            store.members_of_tenant(&tenant_id()).await.unwrap()[0].user,
            user_id()
        );
        assert_eq!(
            store
                .membership(&user_id(), &tenant_id())
                .await
                .unwrap()
                .unwrap()
                .standing,
            Standing::Owner
        );
    }

    #[tokio::test]
    async fn deleting_a_tenant_takes_its_memberships_and_grants_with_it() {
        let store = store().await;
        tenant_with_user(&store).await;
        store
            .attach(Membership {
                user: user_id(),
                tenant: tenant_id(),
                standing: Standing::Member,
            })
            .await
            .unwrap();
        store
            .grant(&api_grant(&user_id(), &tenant_id(), Capability::ApiAccess))
            .await
            .unwrap();
        store.delete_tenant(&tenant_id()).await.unwrap();
        assert!(
            store
                .memberships_of_user(&user_id())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(store.grants_of(&user_id()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn granting_the_same_capability_twice_changes_nothing() {
        let store = store().await;
        tenant_with_user(&store).await;
        let grant = api_grant(&user_id(), &tenant_id(), Capability::ApiAccess);
        store.grant(&grant).await.unwrap();
        store.grant(&grant).await.unwrap();
        assert_eq!(store.grants_of(&user_id()).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn every_scope_shape_round_trips() {
        let store = store().await;
        tenant_with_user(&store).await;
        let user_scoped = Grant::new(
            Capability::TenantMgr,
            CapabilityScope::User { user: user_id() },
        )
        .unwrap();
        let tenant_scoped = Grant::new(
            Capability::UserMgr,
            CapabilityScope::Tenant {
                user: user_id(),
                tenant: tenant_id(),
            },
        )
        .unwrap();
        let api_scoped = api_grant(&user_id(), &tenant_id(), Capability::ApiAccess);
        for grant in [&user_scoped, &tenant_scoped, &api_scoped] {
            store.grant(grant).await.unwrap();
        }
        let held = store.grants_of(&user_id()).await.unwrap();
        assert_eq!(held.len(), 3);
        assert!(held.holds(Capability::TenantMgr, user_scoped.scope()));
        assert!(held.holds(Capability::UserMgr, tenant_scoped.scope()));
        assert!(held.holds(Capability::ApiAccess, api_scoped.scope()));
    }

    #[tokio::test]
    async fn grants_in_a_tenant_exclude_the_user_scoped_ones() {
        let store = store().await;
        tenant_with_user(&store).await;
        store
            .grant(
                &Grant::new(
                    Capability::TenantMgr,
                    CapabilityScope::User { user: user_id() },
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let api_scoped = api_grant(&user_id(), &tenant_id(), Capability::ApiAccess);
        store.grant(&api_scoped).await.unwrap();
        let held = store
            .grants_in_tenant(&user_id(), &tenant_id())
            .await
            .unwrap();
        assert_eq!(held.len(), 1);
        assert!(held.holds(Capability::ApiAccess, api_scoped.scope()));
    }

    #[tokio::test]
    async fn revoking_what_is_not_held_reports_it_missing() {
        let store = store().await;
        tenant_with_user(&store).await;
        assert!(matches!(
            store
                .revoke(&api_grant(&user_id(), &tenant_id(), Capability::ApiAccess))
                .await,
            Err(StorageError::NotFound {
                entity: Entity::Grant,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn revoking_removes_only_the_named_grant() {
        let store = store().await;
        tenant_with_user(&store).await;
        let access = api_grant(&user_id(), &tenant_id(), Capability::ApiAccess);
        let limits = api_grant(&user_id(), &tenant_id(), Capability::LimitMgr);
        store.grant(&access).await.unwrap();
        store.grant(&limits).await.unwrap();
        store.revoke(&limits).await.unwrap();
        let held = store.grants_of(&user_id()).await.unwrap();
        assert_eq!(held.len(), 1);
        assert!(held.holds(Capability::ApiAccess, access.scope()));
    }

    #[tokio::test]
    async fn a_capability_the_code_does_not_know_is_reported_as_malformed() {
        let store = store().await;
        let (_, user) = tenant_with_user(&store).await;
        sqlx::query("INSERT INTO grants (user_row_id, capability) VALUES (?, 'wat')")
            .bind(user.row_id.get())
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(matches!(
            store.grants_of(&user_id()).await,
            Err(StorageError::Malformed {
                entity: Entity::Grant,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn an_api_grant_with_no_tenant_is_reported_as_malformed() {
        let store = store().await;
        let (_, user) = tenant_with_user(&store).await;
        sqlx::query(
            "INSERT INTO grants (user_row_id, api_id, capability) VALUES (?, 'chat', 'api_access')",
        )
        .bind(user.row_id.get())
        .execute(&store.pool)
        .await
        .unwrap();
        assert!(matches!(
            store.grants_of(&user_id()).await,
            Err(StorageError::Malformed {
                entity: Entity::Grant,
                ..
            })
        ));
    }
}
