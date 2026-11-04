use async_trait::async_trait;
use ip_core::{Grant, Grants, PassphraseHash, TenantId, Timestamp, UserId};
use sqlx::{PgConnection, Row};

use super::PostgresStore;
use crate::audit::{By, NewAudit};
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

/// Creates a tenant and records it, both on the one connection its caller commits.
async fn adding_tenant(
    connection: &mut PgConnection,
    new: NewTenant,
    by: &By,
) -> Result<Tenant, StorageError> {
    let added = insert_tenant(&mut *connection, new).await?;
    super::audit::record(connection, NewAudit::created(by.clone(), &added)).await?;
    Ok(added)
}

/// Deletes a tenant and records the deletion, on the one connection its caller commits.
async fn dropping_tenant(
    connection: &mut PgConnection,
    id: &TenantId,
    by: &By,
) -> Result<(), StorageError> {
    let before = read_tenant(&mut *connection, id).await?;
    let deleted = sqlx::query("DELETE FROM tenants WHERE id = $1")
        .bind(id.as_str())
        .execute(&mut *connection)
        .await
        .map_err(StorageError::backend)?
        .rows_affected();
    if deleted == 0 {
        return Err(StorageError::NotFound {
            entity: Entity::Tenant,
            id: id.to_string(),
        });
    }
    // A row was deleted, so something was there to read a moment ago in the same
    // transaction; an absent before here would mean the two disagree.
    let Some(before) = before else {
        return Err(StorageError::Malformed {
            entity: Entity::Tenant,
            detail: "a tenant was deleted that nothing could read".to_owned(),
        });
    };
    super::audit::record(connection, NewAudit::removed(by.clone(), &before)).await?;
    Ok(())
}

/// One tenant, over whichever connection it is given. Read inside the transaction that is
/// about to change it, so the before it records is the before that went.
pub(super) async fn read_tenant(
    connection: &mut PgConnection,
    id: &TenantId,
) -> Result<Option<Tenant>, StorageError> {
    let Some(row) = sqlx::query("SELECT row_id, key, created_at FROM tenants WHERE id = $1")
        .bind(id.as_str())
        .fetch_optional(&mut *connection)
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

/// Creates an account and records it, both on the one connection its caller commits.
///
/// Apart from the method so that every way out of it is a `?` the caller can roll back,
/// rather than a return that leaves a transaction open.
async fn adding(
    connection: &mut PgConnection,
    new: NewUser,
    by: &By,
) -> Result<User, StorageError> {
    let added = insert_user(&mut *connection, new).await?;
    super::audit::record(connection, NewAudit::created(by.clone(), &added)).await?;
    Ok(added)
}

/// Replaces an account's passphrase and records that it was replaced, on the one
/// connection its caller commits.
async fn replacing(
    connection: &mut PgConnection,
    id: &UserId,
    passphrase: &PassphraseHash,
    by: &By,
) -> Result<(), StorageError> {
    let before = read_user(&mut *connection, id).await?;
    let updated = sqlx::query("UPDATE users SET passphrase = $1 WHERE id = $2")
        .bind(passphrase.as_str())
        .bind(id.as_str())
        .execute(&mut *connection)
        .await
        .map_err(StorageError::backend)?
        .rows_affected();
    if updated == 0 {
        return Err(StorageError::NotFound {
            entity: Entity::User,
            id: id.to_string(),
        });
    }
    // A row was updated, so something was there to read a moment ago in the same
    // transaction; an absent before here would mean the two disagree.
    let Some(before) = before else {
        return Err(StorageError::Malformed {
            entity: Entity::User,
            detail: "a passphrase was replaced on an account nothing could read".to_owned(),
        });
    };
    // Before and after are the same shape on purpose: the verifier is not in it, so what
    // the trail says is that the account was changed here, not what it was changed to.
    super::audit::record(
        connection,
        NewAudit::changed(by.clone(), Some(&before), &before),
    )
    .await?;
    Ok(())
}

/// Deletes an account and records the deletion, on the one connection its caller commits.
async fn dropping(connection: &mut PgConnection, id: &UserId, by: &By) -> Result<(), StorageError> {
    let before = read_user(&mut *connection, id).await?;
    let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(id.as_str())
        .execute(&mut *connection)
        .await
        .map_err(StorageError::backend)?
        .rows_affected();
    if deleted == 0 {
        return Err(StorageError::NotFound {
            entity: Entity::User,
            id: id.to_string(),
        });
    }
    // A row was deleted, so something was there to read a moment ago in the same
    // transaction; an absent before here would mean the two disagree.
    let Some(before) = before else {
        return Err(StorageError::Malformed {
            entity: Entity::User,
            detail: "an account was deleted that nothing could read".to_owned(),
        });
    };
    super::audit::record(connection, NewAudit::removed(by.clone(), &before)).await?;
    Ok(())
}

/// One account, over whichever connection it is given. Read inside the transaction that
/// is about to change it, so the before it records is the before that was replaced.
pub(super) async fn read_user(
    connection: &mut PgConnection,
    id: &UserId,
) -> Result<Option<User>, StorageError> {
    let Some(row) =
        sqlx::query("SELECT row_id, passphrase, kind, created_at FROM users WHERE id = $1")
            .bind(id.as_str())
            .fetch_optional(&mut *connection)
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
        "INSERT INTO memberships (tenant_row_id, user_row_id, standing, created_at) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (tenant_row_id, user_row_id) DO UPDATE SET standing = excluded.standing",
    )
    .bind(tenant_row_id.get())
    .bind(user_row_id.get())
    .bind(standing_name(membership.standing))
    .bind(Timestamp::now().unix_seconds())
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
    async fn create_tenant(&self, new: NewTenant, by: &By) -> Result<Tenant, StorageError> {
        // One transaction, so a tenant that lands without its record is not a state this
        // store can be left in, and neither is a record whose tenant rolled back.
        let mut transaction = self.pool.begin().await.map_err(StorageError::backend)?;
        // Rolled back by hand rather than by dropping it: a transaction left to its
        // `Drop` keeps the connection's locks until the pool gets round to it, and a
        // scratch schema being torn down behind it waits for ever.
        match adding_tenant(&mut transaction, new, by).await {
            Ok(added) => transaction
                .commit()
                .await
                .map_err(StorageError::backend)
                .map(|()| added),
            Err(error) => {
                let _ = transaction.rollback().await;
                Err(error)
            }
        }
    }

    async fn tenant(&self, id: &TenantId) -> Result<Option<Tenant>, StorageError> {
        read_tenant(&mut *self.connection().await?, id).await
    }

    async fn delete_tenant(&self, id: &TenantId, by: &By) -> Result<(), StorageError> {
        let mut transaction = self.pool.begin().await.map_err(StorageError::backend)?;
        match dropping_tenant(&mut transaction, id, by).await {
            Ok(()) => transaction.commit().await.map_err(StorageError::backend),
            Err(error) => {
                let _ = transaction.rollback().await;
                Err(error)
            }
        }
    }
}

#[async_trait]
impl UserStore for PostgresStore {
    async fn create_user(&self, new: NewUser, by: &By) -> Result<User, StorageError> {
        // One transaction, so an account that lands without its record is not a state
        // this store can be left in, and neither is a record whose account rolled back.
        let mut transaction = self.pool.begin().await.map_err(StorageError::backend)?;
        // Rolled back by hand rather than by dropping it: a transaction left to its
        // `Drop` keeps the connection's locks until the pool gets round to it, and a
        // scratch schema being torn down behind it waits for ever.
        match adding(&mut transaction, new, by).await {
            Ok(added) => transaction
                .commit()
                .await
                .map_err(StorageError::backend)
                .map(|()| added),
            Err(error) => {
                let _ = transaction.rollback().await;
                Err(error)
            }
        }
    }

    async fn user(&self, id: &UserId) -> Result<Option<User>, StorageError> {
        read_user(&mut *self.connection().await?, id).await
    }

    async fn set_passphrase(
        &self,
        id: &UserId,
        passphrase: &PassphraseHash,
        by: &By,
    ) -> Result<(), StorageError> {
        let mut transaction = self.pool.begin().await.map_err(StorageError::backend)?;
        match replacing(&mut transaction, id, passphrase, by).await {
            Ok(()) => transaction.commit().await.map_err(StorageError::backend),
            Err(error) => {
                let _ = transaction.rollback().await;
                Err(error)
            }
        }
    }

    async fn delete_user(&self, id: &UserId, by: &By) -> Result<(), StorageError> {
        let mut transaction = self.pool.begin().await.map_err(StorageError::backend)?;
        match dropping(&mut transaction, id, by).await {
            Ok(()) => transaction.commit().await.map_err(StorageError::backend),
            Err(error) => {
                let _ = transaction.rollback().await;
                Err(error)
            }
        }
    }
}

#[async_trait]
impl MembershipStore for PostgresStore {
    async fn attach(&self, membership: Membership) -> Result<(), StorageError> {
        insert_membership(&mut *self.connection().await?, membership).await
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
            "INSERT INTO grants (user_row_id, tenant_row_id, api_id, capability, created_at) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(user_row_id.get())
        .bind(tenant_row_id)
        .bind(grant.scope().api().map(|api| api.as_str()))
        .bind(capability_name(grant.capability()))
        .bind(Timestamp::now().unix_seconds())
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
        sqlx::query(
            "INSERT INTO grants (user_row_id, capability, created_at) VALUES ($1, 'wat', 0)",
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

    #[tokio::test]
    async fn an_api_grant_with_no_tenant_is_reported_as_malformed() {
        let Some(store) = scratch::store().await else {
            return;
        };
        let (_, user) = tenant_with_user(&store).await;
        sqlx::query(
            "INSERT INTO grants (user_row_id, api_id, capability, created_at) \
             VALUES ($1, 'chat', 'api_access', 0)",
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
