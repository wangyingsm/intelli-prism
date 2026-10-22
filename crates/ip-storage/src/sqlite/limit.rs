use async_trait::async_trait;
use ip_core::{Allowance, Counted, LimitScope, Period, Timestamp};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection};

use super::SqliteStore;
use crate::audit::{By, NewAudit};
use crate::error::{Entity, StorageError};
use crate::limit::{Limit, LimitStore, NewLimit};

#[async_trait]
impl LimitStore for SqliteStore {
    async fn put_limit(&self, limit: NewLimit, by: &By) -> Result<Limit, StorageError> {
        // One transaction, so a limit that lands without its record is not a state this
        // store can be left in, and neither is a record whose limit rolled back.
        let mut transaction = self.pool.begin().await.map_err(StorageError::backend)?;
        // Rolled back by hand rather than by dropping it: a transaction left to its
        // `Drop` keeps the connection's locks until the pool gets round to it, and a
        // scratch schema being torn down behind it waits for ever.
        match setting(&mut transaction, limit, by).await {
            Ok(set) => transaction
                .commit()
                .await
                .map_err(StorageError::backend)
                .map(|()| set),
            Err(error) => {
                let _ = transaction.rollback().await;
                Err(error)
            }
        }
    }

    async fn remove_limit(
        &self,
        scope: &LimitScope,
        counted: Counted,
        period: Period,
        by: &By,
    ) -> Result<(), StorageError> {
        let mut transaction = self.pool.begin().await.map_err(StorageError::backend)?;
        match removing(&mut transaction, scope, counted, period, by).await {
            Ok(()) => transaction.commit().await.map_err(StorageError::backend),
            Err(error) => {
                let _ = transaction.rollback().await;
                Err(error)
            }
        }
    }

    async fn limits(&self) -> Result<Vec<Limit>, StorageError> {
        read_limits(&mut *self.connection().await?).await
    }
}

/// Sets a limit and records the change, both on the one connection its caller commits.
///
/// Apart from the method so that every way out of it is a `?` the caller can roll back,
/// rather than a return that leaves a transaction open.
async fn setting(
    connection: &mut SqliteConnection,
    limit: NewLimit,
    by: &By,
) -> Result<Limit, StorageError> {
    let tenant = super::identity::tenant_row_id(connection, &limit.scope.tenant).await?;
    let user = match &limit.scope.user {
        Some(user) => Some(super::identity::user_row_id(connection, user).await?),
        None => None,
    };
    let before = read_limit(connection, &limit.scope, limit.counted, limit.period).await?;
    let created_at = Timestamp::now();
    sqlx::query(
        "INSERT INTO limits \
         (tenant_row_id, user_row_id, api_id, counted, period, allowance, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (tenant_row_id, IFNULL(user_row_id, 0), IFNULL(api_id, ''), \
         counted, period) DO UPDATE SET allowance = excluded.allowance",
    )
    .bind(tenant.get())
    .bind(user.map(|user| user.get()))
    .bind(limit.scope.api.as_ref().map(ip_core::ApiId::as_str))
    .bind(limit.counted.name())
    .bind(limit.period.name())
    .bind(i64::try_from(limit.allowance.get()).unwrap_or(i64::MAX))
    .bind(created_at.unix_seconds())
    .execute(&mut *connection)
    .await
    .map_err(StorageError::backend)?;

    let set = Limit {
        scope: limit.scope,
        counted: limit.counted,
        period: limit.period,
        allowance: limit.allowance,
        created_at,
    };
    super::audit::record(
        connection,
        NewAudit::changed(by.clone(), before.as_ref(), &set),
    )
    .await?;
    Ok(set)
}

/// Removes a limit and records the removal, on the one connection its caller commits.
async fn removing(
    connection: &mut SqliteConnection,
    scope: &LimitScope,
    counted: Counted,
    period: Period,
    by: &By,
) -> Result<(), StorageError> {
    let tenant = super::identity::tenant_row_id(connection, &scope.tenant).await?;
    let user = match &scope.user {
        Some(user) => Some(super::identity::user_row_id(connection, user).await?),
        None => None,
    };
    let before = read_limit(connection, scope, counted, period).await?;
    let removed = sqlx::query(
        "DELETE FROM limits WHERE tenant_row_id = ? AND IFNULL(user_row_id, 0) = ? \
         AND IFNULL(api_id, '') = ? AND counted = ? AND period = ?",
    )
    .bind(tenant.get())
    .bind(user.map_or(0, |user| user.get()))
    .bind(scope.api.as_ref().map_or("", ip_core::ApiId::as_str))
    .bind(counted.name())
    .bind(period.name())
    .execute(&mut *connection)
    .await
    .map_err(StorageError::backend)?
    .rows_affected();
    if removed == 0 {
        return Err(StorageError::NotFound {
            entity: Entity::Limit,
            id: format!("{scope} {} per {}", counted.name(), period.name()),
        });
    }
    // A row was deleted, so something was there to read a moment ago in the same
    // transaction; an absent before here would mean the two disagree.
    let Some(before) = before else {
        return Err(StorageError::Malformed {
            entity: Entity::Limit,
            detail: "a limit was removed that nothing could read".to_owned(),
        });
    };
    super::audit::record(connection, NewAudit::removed(by.clone(), &before)).await
}

/// The one limit a scope holds of this thing over this stretch, and nothing where it
/// holds none. Read inside the transaction that is about to change it, so the before it
/// records is the before that was replaced.
pub(super) async fn read_limit(
    connection: &mut SqliteConnection,
    scope: &LimitScope,
    counted: Counted,
    period: Period,
) -> Result<Option<Limit>, StorageError> {
    let row = sqlx::query(
        "SELECT t.id AS tenant_id, u.id AS user_id, l.api_id, l.counted, l.period, \
             l.allowance, l.created_at FROM limits l \
             JOIN tenants t ON t.row_id = l.tenant_row_id \
             LEFT JOIN users u ON u.row_id = l.user_row_id \
             WHERE t.id = ? AND IFNULL(u.id, '') = ? AND IFNULL(l.api_id, '') = ? \
             AND l.counted = ? AND l.period = ?",
    )
    .bind(scope.tenant.as_str())
    .bind(scope.user.as_ref().map_or("", ip_core::UserId::as_str))
    .bind(scope.api.as_ref().map_or("", ip_core::ApiId::as_str))
    .bind(counted.name())
    .bind(period.name())
    .fetch_optional(&mut *connection)
    .await
    .map_err(StorageError::backend)?;
    row.map(set).transpose()
}

/// Every limit held, over whichever connection it is given.
pub(super) async fn read_limits(
    connection: &mut SqliteConnection,
) -> Result<Vec<Limit>, StorageError> {
    let rows = sqlx::query(
        "SELECT t.id AS tenant_id, u.id AS user_id, l.api_id, l.counted, l.period, \
             l.allowance, l.created_at FROM limits l \
             JOIN tenants t ON t.row_id = l.tenant_row_id \
             LEFT JOIN users u ON u.row_id = l.user_row_id \
             ORDER BY l.created_at DESC, l.row_id DESC",
    )
    .fetch_all(&mut *connection)
    .await
    .map_err(StorageError::backend)?;
    rows.into_iter().map(set).collect()
}

/// Rebuilds a limit, refusing a row whose stored value its own type will not take.
pub(super) fn set(row: SqliteRow) -> Result<Limit, StorageError> {
    let user: Option<String> = row.get("user_id");
    let api: Option<String> = row.get("api_id");
    let counted: String = row.get("counted");
    let period: String = row.get("period");
    let allowance: i64 = row.get("allowance");
    Ok(Limit {
        scope: LimitScope {
            tenant: ip_core::TenantId::new(row.get("tenant_id"))?,
            user: user.as_deref().map(ip_core::UserId::new).transpose()?,
            api: api.as_deref().map(ip_core::ApiId::new).transpose()?,
        },
        counted: Counted::named(&counted)?,
        period: Period::named(&period)?,
        allowance: Allowance::new(u64::try_from(allowance).map_err(|_| {
            StorageError::Malformed {
                entity: Entity::Limit,
                detail: format!("{allowance} is not an allowance"),
            }
        })?)?,
        created_at: Timestamp::from_unix_seconds(row.get("created_at"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::fixture::*;
    use crate::suite::limit::monthly;

    /// Sets a limit, then writes over one column with something the code cannot produce.
    async fn written_over(store: &SqliteStore, update: &'static str) {
        tenant_with_user(store).await;
        store
            .put_limit(monthly(1_000), &crate::suite::limit::by())
            .await
            .unwrap();
        sqlx::query(update).execute(&store.pool).await.unwrap();
    }

    /// Every column whose stored value its own type can refuse, and a value that it refuses.
    const REFUSED: [&str; 2] = [
        "UPDATE limits SET counted = 'dollars'",
        "UPDATE limits SET period = 'fortnight'",
    ];

    #[tokio::test]
    async fn a_row_no_type_of_ours_will_take_is_reported_as_malformed() {
        for update in REFUSED {
            let store = store().await;
            written_over(&store, update).await;
            assert!(
                matches!(store.limits().await, Err(StorageError::Value(_))),
                "{update} was read back as {:?}",
                store.limits().await
            );
        }
    }
}
