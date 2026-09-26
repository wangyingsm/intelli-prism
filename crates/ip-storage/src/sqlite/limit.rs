use async_trait::async_trait;
use ip_core::{Allowance, Counted, LimitScope, Period, Timestamp};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection};

use super::SqliteStore;
use crate::error::{Entity, StorageError};
use crate::limit::{Limit, LimitStore, NewLimit};

#[async_trait]
impl LimitStore for SqliteStore {
    async fn put_limit(&self, limit: NewLimit) -> Result<Limit, StorageError> {
        let mut connection = self.connection().await?;
        let tenant = super::identity::tenant_row_id(&mut connection, &limit.scope.tenant).await?;
        let user = match &limit.scope.user {
            Some(user) => Some(super::identity::user_row_id(&mut connection, user).await?),
            None => None,
        };
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

        Ok(Limit {
            scope: limit.scope,
            counted: limit.counted,
            period: limit.period,
            allowance: limit.allowance,
            created_at,
        })
    }

    async fn remove_limit(
        &self,
        scope: &LimitScope,
        counted: Counted,
        period: Period,
    ) -> Result<(), StorageError> {
        let mut connection = self.connection().await?;
        let tenant = super::identity::tenant_row_id(&mut connection, &scope.tenant).await?;
        let user = match &scope.user {
            Some(user) => Some(super::identity::user_row_id(&mut connection, user).await?),
            None => None,
        };
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
        Ok(())
    }

    async fn limits(&self) -> Result<Vec<Limit>, StorageError> {
        read_limits(&mut *self.connection().await?).await
    }
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
