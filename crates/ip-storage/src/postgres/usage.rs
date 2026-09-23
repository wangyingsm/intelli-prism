use async_trait::async_trait;
use ip_core::{ApiId, ModelName, TenantId, Timestamp, Tokens, TraceId, TurnId, UserId};
use sqlx::Row;
use sqlx::postgres::PgRow;

use super::PostgresStore;
use crate::codec::{latency, served as served_of, served_name, token_count};
use crate::error::{Entity, StorageError};
use crate::usage::{NewUsage, Usage, UsageRowId, UsageStore};

#[async_trait]
impl UsageStore for PostgresStore {
    async fn record_usage(&self, usage: NewUsage) -> Result<Usage, StorageError> {
        let row = sqlx::query(
            "INSERT INTO usage (trace_id, turn_id, tenant_id, user_id, api_id, model, \
             input_tokens, output_tokens, served, latency_ms, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
             RETURNING row_id, created_at",
        )
        .bind(usage.trace.to_hex())
        .bind(usage.turn.as_ref().map(TurnId::as_str))
        .bind(usage.tenant.as_str())
        .bind(usage.user.as_str())
        .bind(usage.api.as_str())
        .bind(usage.model.as_ref().map(ModelName::as_str))
        .bind(i64::from(usage.tokens.input.get()))
        .bind(i64::from(usage.tokens.output.get()))
        .bind(served_name(usage.served))
        .bind(i64::from(usage.latency.millis()))
        .bind(Timestamp::now().unix_seconds())
        .fetch_one(&self.pool)
        .await
        .map_err(StorageError::backend)?;

        Ok(Usage {
            row_id: UsageRowId::new(row.get("row_id")),
            trace: usage.trace,
            turn: usage.turn,
            tenant: usage.tenant,
            user: usage.user,
            api: usage.api,
            model: usage.model,
            tokens: usage.tokens,
            served: usage.served,
            latency: usage.latency,
            created_at: Timestamp::from_unix_seconds(row.get("created_at"))?,
        })
    }

    async fn usage(&self, row_id: UsageRowId) -> Result<Option<Usage>, StorageError> {
        let row = sqlx::query(
            "SELECT row_id, trace_id, turn_id, tenant_id, user_id, api_id, model, \
             input_tokens, output_tokens, served, latency_ms, created_at \
             FROM usage WHERE row_id = $1",
        )
        .bind(row_id.get())
        .fetch_optional(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        row.map(recorded).transpose()
    }
}

/// Rebuilds a usage row, refusing one whose stored value its own type will not take.
pub(super) fn recorded(row: PgRow) -> Result<Usage, StorageError> {
    let turn: Option<String> = row.get("turn_id");
    let model: Option<String> = row.get("model");
    let source: String = row.get("served");
    Ok(Usage {
        row_id: UsageRowId::new(row.get("row_id")),
        trace: TraceId::from_hex(row.get("trace_id")).map_err(|error| StorageError::Malformed {
            entity: Entity::Usage,
            detail: error.to_string(),
        })?,
        turn: turn.as_deref().map(TurnId::new).transpose()?,
        tenant: TenantId::new(row.get("tenant_id"))?,
        user: UserId::new(row.get("user_id"))?,
        api: ApiId::new(row.get("api_id"))?,
        model: model.as_deref().map(ModelName::new).transpose()?,
        tokens: Tokens::new(
            token_count(row.get("input_tokens"))?,
            token_count(row.get("output_tokens"))?,
        ),
        served: served_of(&source)?,
        latency: latency(row.get("latency_ms"))?,
        created_at: Timestamp::from_unix_seconds(row.get("created_at"))?,
    })
}
