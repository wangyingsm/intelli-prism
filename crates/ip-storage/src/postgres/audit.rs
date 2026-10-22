use async_trait::async_trait;
use ip_core::{TenantId, Timestamp, UserId};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};

use super::PostgresStore;
use crate::audit::{AuditFilter, AuditStore, Audited, NewAudit};
use crate::error::{Entity, StorageError};
use crate::list::Page;

/// Records one change, over the same connection the change itself was written on, and
/// says when it was recorded.
///
/// Takes a connection rather than the store so that it can only be called from inside a
/// transaction that is also writing the thing being described.
pub(super) async fn record(
    connection: &mut PgConnection,
    audit: NewAudit,
) -> Result<Timestamp, StorageError> {
    let created_at = Timestamp::now();
    sqlx::query(
        "INSERT INTO audit \
         (actor, through, entity, entity_id, tenant, before, after, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(audit.by.actor.as_str())
    .bind(audit.by.through)
    .bind(audit.entity.name())
    .bind(&audit.entity_id)
    .bind(audit.tenant.as_ref().map(TenantId::as_str))
    .bind(written(audit.before.as_ref())?)
    .bind(written(audit.after.as_ref())?)
    .bind(created_at.unix_seconds())
    .execute(&mut *connection)
    .await
    .map_err(StorageError::backend)?;
    Ok(created_at)
}

/// One payload as the column holds it, and nothing where there is nothing to hold.
fn written(payload: Option<&serde_json::Value>) -> Result<Option<String>, StorageError> {
    payload
        .map(|payload| {
            serde_json::to_string(payload).map_err(|error| StorageError::Malformed {
                entity: Entity::Audit,
                detail: format!("an audited shape will not serialize: {error}"),
            })
        })
        .transpose()
}

#[async_trait]
impl AuditStore for PostgresStore {
    async fn audits(&self, filter: &AuditFilter, page: Page) -> Result<Vec<Audited>, StorageError> {
        let rows = sqlx::query(
            "SELECT actor, through, entity, entity_id, tenant, before, after, created_at \
             FROM audit \
             WHERE created_at > $1 \
             AND ($2::TEXT IS NULL OR tenant = $2) \
             AND ($3::TEXT IS NULL OR actor = $3) \
             AND ($4::TEXT IS NULL OR entity = $4) \
             ORDER BY created_at DESC, row_id DESC LIMIT $5 OFFSET $6",
        )
        .bind(page.after_seconds())
        .bind(filter.tenant.as_ref().map(TenantId::as_str))
        .bind(filter.actor.as_ref().map(UserId::as_str))
        .bind(filter.entity.map(Entity::name))
        .bind(i64::from(page.limit()))
        .bind(i64::from(page.offset()))
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        rows.into_iter().map(recorded).collect()
    }
}

/// Rebuilds a recorded change, refusing a row whose stored value its own type will not take.
fn recorded(row: PgRow) -> Result<Audited, StorageError> {
    let entity: String = row.get("entity");
    let tenant: Option<String> = row.get("tenant");
    Ok(Audited {
        actor: UserId::new(row.get("actor"))?,
        through: row.get("through"),
        entity: Entity::named(&entity)?,
        entity_id: row.get("entity_id"),
        tenant: tenant.as_deref().map(TenantId::new).transpose()?,
        before: read(row.get("before"))?,
        after: read(row.get("after"))?,
        created_at: Timestamp::from_unix_seconds(row.get("created_at"))?,
    })
}

/// One stored payload, refusing text that is not the json it was written as.
fn read(payload: Option<String>) -> Result<Option<serde_json::Value>, StorageError> {
    payload
        .map(|payload| {
            serde_json::from_str(&payload).map_err(|error| StorageError::Malformed {
                entity: Entity::Audit,
                detail: format!("an audited shape is not json: {error}"),
            })
        })
        .transpose()
}
