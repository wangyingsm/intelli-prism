use async_trait::async_trait;
use ip_core::{Grant, Grants, PassphraseHash, TenantId, Timestamp, UserId};
use sqlx::{PgConnection, Row};

use super::PostgresStore;
use crate::codec::{
    account_kind, account_kind_name, capability, capability_name, scope, standing, standing_name,
    tenant_key,
};
use crate::error::{Entity, StorageError, is_unique_violation};
use crate::model::{Membership, NewTenant, NewUser, Tenant, TenantRowId, User, UserRowId};
use crate::store::{GrantStore, MembershipStore, TenantStore, UserStore};
use crate::transaction::IdentityDialect;
use crate::transaction::member_add::{MemberAddBegun, MemberAddTransactional, MemberAddTxn};
use crate::transaction::member_remove::{
    MemberRemoveBegun, MemberRemoveTransactional, MemberRemoveTxn,
};
use crate::transaction::user_create::{UserCreateBegun, UserCreateTransactional, UserCreateTxn};

/// Writes a tenant row over whichever connection it is given, so a transaction and the pool
/// run the same statement.
pub(super) async fn insert_tenant(
    connection: &mut PgConnection,
    new: NewTenant,
) -> Result<Tenant, StorageError> {
    let created_at = Timestamp::now();
    let result = sqlx::query(
        "INSERT INTO tenants (id, key, created_at) VALUES ($1, $2, $3) RETURNING row_id",
    )
    .bind(new.id.as_str())
    .bind(new.key.as_bytes().as_slice())
    .bind(created_at.unix_seconds())
    .fetch_one(connection)
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

/// Writes a user row over whichever connection it is given.
pub(super) async fn insert_user(
    connection: &mut PgConnection,
    new: NewUser,
) -> Result<User, StorageError> {
    let created_at = Timestamp::now();
    let result = sqlx::query(
        "INSERT INTO users (id, passphrase, kind, created_at) VALUES ($1, $2, $3, $4) \
         RETURNING row_id",
    )
    .bind(new.id.as_str())
    .bind(new.passphrase.as_str())
    .bind(account_kind_name(new.kind))
    .bind(created_at.unix_seconds())
    .fetch_one(connection)
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

/// Attaches a user to a tenant over whichever connection it is given.
pub(super) async fn insert_membership(
    connection: &mut PgConnection,
    membership: Membership,
) -> Result<(), StorageError> {
    let tenant_row_id = tenant_row_id(&mut *connection, &membership.tenant).await?;
    let user_row_id = user_row_id(&mut *connection, &membership.user).await?;
    sqlx::query(
        "INSERT INTO memberships (tenant_row_id, user_row_id, standing) VALUES ($1, $2, $3) \
         ON CONFLICT (tenant_row_id, user_row_id) DO UPDATE SET standing = excluded.standing",
    )
    .bind(tenant_row_id.get())
    .bind(user_row_id.get())
    .bind(standing_name(membership.standing))
    .execute(connection)
    .await
    .map_err(StorageError::backend)?;
    Ok(())
}

/// Detaches a user from a tenant over whichever connection it is given, handing back the
/// attachment that was there.
pub(super) async fn delete_membership(
    connection: &mut PgConnection,
    user: &UserId,
    tenant: &TenantId,
) -> Result<Membership, StorageError> {
    let row = sqlx::query(
        "DELETE FROM memberships \
         WHERE tenant_row_id = (SELECT row_id FROM tenants WHERE id = $1) \
         AND user_row_id = (SELECT row_id FROM users WHERE id = $2) \
         RETURNING standing",
    )
    .bind(tenant.as_str())
    .bind(user.as_str())
    .fetch_optional(connection)
    .await
    .map_err(StorageError::backend)?
    .ok_or_else(|| StorageError::NotFound {
        entity: Entity::Membership,
        id: format!("{user}@{tenant}"),
    })?;
    Ok(Membership {
        user: user.clone(),
        tenant: tenant.clone(),
        standing: standing(row.get("standing"))?,
    })
}

/// Revokes every grant one user holds inside one tenant over whichever connection it is
/// given, reporting how many it held.
pub(super) async fn delete_grants_in_tenant(
    connection: &mut PgConnection,
    user: &UserId,
    tenant: &TenantId,
) -> Result<u64, StorageError> {
    let deleted = sqlx::query(
        "DELETE FROM grants \
         WHERE user_row_id = (SELECT row_id FROM users WHERE id = $1) \
         AND tenant_row_id = (SELECT row_id FROM tenants WHERE id = $2)",
    )
    .bind(user.as_str())
    .bind(tenant.as_str())
    .execute(connection)
    .await
    .map_err(StorageError::backend)?
    .rows_affected();
    Ok(deleted)
}

/// The primary key of a tenant row, or a report that there is none.
pub(super) async fn tenant_row_id(
    connection: &mut PgConnection,
    id: &TenantId,
) -> Result<TenantRowId, StorageError> {
    sqlx::query("SELECT row_id FROM tenants WHERE id = $1")
        .bind(id.as_str())
        .fetch_optional(connection)
        .await
        .map_err(StorageError::backend)?
        .map(|row| TenantRowId::new(row.get("row_id")))
        .ok_or_else(|| StorageError::NotFound {
            entity: Entity::Tenant,
            id: id.to_string(),
        })
}

/// The primary key of a user row, or a report that there is none.
pub(super) async fn user_row_id(
    connection: &mut PgConnection,
    id: &UserId,
) -> Result<UserRowId, StorageError> {
    sqlx::query("SELECT row_id FROM users WHERE id = $1")
        .bind(id.as_str())
        .fetch_optional(connection)
        .await
        .map_err(StorageError::backend)?
        .map(|row| UserRowId::new(row.get("row_id")))
        .ok_or_else(|| StorageError::NotFound {
            entity: Entity::User,
            id: id.to_string(),
        })
}

#[async_trait]
impl IdentityDialect for sqlx::Postgres {
    async fn insert_tenant(
        connection: &mut PgConnection,
        new: NewTenant,
    ) -> Result<Tenant, StorageError> {
        insert_tenant(connection, new).await
    }

    async fn insert_user(
        connection: &mut PgConnection,
        new: NewUser,
    ) -> Result<User, StorageError> {
        insert_user(connection, new).await
    }

    async fn insert_membership(
        connection: &mut PgConnection,
        membership: Membership,
    ) -> Result<(), StorageError> {
        insert_membership(connection, membership).await
    }

    async fn delete_membership(
        connection: &mut PgConnection,
        user: &UserId,
        tenant: &TenantId,
    ) -> Result<Membership, StorageError> {
        delete_membership(connection, user, tenant).await
    }

    async fn delete_grants_in_tenant(
        connection: &mut PgConnection,
        user: &UserId,
        tenant: &TenantId,
    ) -> Result<u64, StorageError> {
        delete_grants_in_tenant(connection, user, tenant).await
    }
}

impl UserCreateTransactional for PostgresStore {
    type Db = sqlx::Postgres;

    async fn begin_user_create(
        &self,
    ) -> Result<UserCreateTxn<Self::Db, UserCreateBegun>, StorageError> {
        let inner = self.pool.begin().await.map_err(StorageError::backend)?;
        Ok(UserCreateTxn::new(inner))
    }
}

impl MemberRemoveTransactional for PostgresStore {
    type Db = sqlx::Postgres;

    async fn begin_member_remove(
        &self,
    ) -> Result<MemberRemoveTxn<Self::Db, MemberRemoveBegun>, StorageError> {
        let inner = self.pool.begin().await.map_err(StorageError::backend)?;
        Ok(MemberRemoveTxn::new(inner))
    }
}

impl MemberAddTransactional for PostgresStore {
    type Db = sqlx::Postgres;

    async fn begin_member_add(
        &self,
    ) -> Result<MemberAddTxn<Self::Db, MemberAddBegun>, StorageError> {
        let inner = self.pool.begin().await.map_err(StorageError::backend)?;
        Ok(MemberAddTxn::new(inner))
    }
}

#[async_trait]
impl TenantStore for PostgresStore {
    async fn create_tenant(&self, new: NewTenant) -> Result<Tenant, StorageError> {
        insert_tenant(&mut *self.connection().await?, new).await
    }

    async fn tenant(&self, id: &TenantId) -> Result<Option<Tenant>, StorageError> {
        let Some(row) = sqlx::query("SELECT row_id, key, created_at FROM tenants WHERE id = $1")
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
        let deleted = sqlx::query("DELETE FROM tenants WHERE id = $1")
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
impl UserStore for PostgresStore {
    async fn create_user(&self, new: NewUser) -> Result<User, StorageError> {
        insert_user(&mut *self.connection().await?, new).await
    }

    async fn user(&self, id: &UserId) -> Result<Option<User>, StorageError> {
        let Some(row) =
            sqlx::query("SELECT row_id, passphrase, kind, created_at FROM users WHERE id = $1")
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
        let updated = sqlx::query("UPDATE users SET passphrase = $1 WHERE id = $2")
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
        let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
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
impl MembershipStore for PostgresStore {
    async fn attach(&self, membership: Membership) -> Result<(), StorageError> {
        insert_membership(&mut *self.connection().await?, membership).await
    }

    async fn detach(&self, user: &UserId, tenant: &TenantId) -> Result<(), StorageError> {
        let deleted = sqlx::query(
            "DELETE FROM memberships \
             WHERE tenant_row_id = (SELECT row_id FROM tenants WHERE id = $1) \
             AND user_row_id = (SELECT row_id FROM users WHERE id = $2)",
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
             WHERE t.id = $1 AND u.id = $2",
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
             WHERE u.id = $1 ORDER BY t.id",
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
             WHERE t.id = $1 ORDER BY u.id",
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
impl GrantStore for PostgresStore {
    async fn grant(&self, grant: &Grant) -> Result<(), StorageError> {
        let user_row_id = self.user_row_id(grant.scope().user()).await?;
        let tenant_row_id = match grant.scope().tenant() {
            Some(tenant) => Some(self.tenant_row_id(tenant).await?.get()),
            None => None,
        };
        let result = sqlx::query(
            "INSERT INTO grants (user_row_id, tenant_row_id, api_id, capability) \
             VALUES ($1, $2, $3, $4)",
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
            "DELETE FROM grants WHERE user_row_id = $1 \
             AND tenant_row_id IS NOT DISTINCT FROM $2 \
             AND api_id IS NOT DISTINCT FROM $3 AND capability = $4",
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
             WHERE u.id = $1",
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
             WHERE u.id = $1 AND t.id = $2",
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
    use crate::postgres::scratch;
    use crate::suite::fixture::*;

    #[tokio::test]
    async fn a_capability_the_code_does_not_know_is_reported_as_malformed() {
        let Some(store) = scratch::store().await else {
            return;
        };
        let (_, user) = tenant_with_user(&store).await;
        sqlx::query("INSERT INTO grants (user_row_id, capability) VALUES ($1, 'wat')")
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
        let Some(store) = scratch::store().await else {
            return;
        };
        let (_, user) = tenant_with_user(&store).await;
        sqlx::query(
            "INSERT INTO grants (user_row_id, api_id, capability) VALUES ($1, 'chat', 'api_access')",
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
