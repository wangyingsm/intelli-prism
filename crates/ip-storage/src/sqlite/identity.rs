use async_trait::async_trait;
use ip_core::{
    ApiId, Capability, CapabilityScope, Grant, Grants, PassphraseHash, TenantId, Timestamp, TnKey,
    UserId,
};
use sqlx::Row;

use super::SqliteStore;
use crate::error::{Entity, StorageError, is_unique_violation};
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
