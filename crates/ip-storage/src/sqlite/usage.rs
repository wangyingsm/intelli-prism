use async_trait::async_trait;
use ip_core::{ApiId, ModelName, TenantId, Timestamp, Tokens, TraceId, TurnId, UserId};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use super::SqliteStore;
use crate::codec::{latency, served as served_of, served_name, token_count};
use crate::error::{Entity, StorageError};
use crate::usage::{NewUsage, Usage, UsageRowId, UsageStore};

#[async_trait]
impl UsageStore for SqliteStore {
    async fn record_usage(&self, usage: NewUsage) -> Result<Usage, StorageError> {
        let row = sqlx::query(
            "INSERT INTO usage (trace_id, turn_id, tenant_id, user_id, api_id, model, \
             input_tokens, output_tokens, served, latency_ms, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
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
             FROM usage WHERE row_id = ?",
        )
        .bind(row_id.get())
        .fetch_optional(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        row.map(recorded).transpose()
    }

    async fn sweep_usage(&self, moment: Timestamp) -> Result<u64, StorageError> {
        let swept = sqlx::query("DELETE FROM usage WHERE created_at < ?")
            .bind(moment.unix_seconds())
            .execute(&self.pool)
            .await
            .map_err(StorageError::backend)?
            .rows_affected();
        Ok(swept)
    }
}

/// Rebuilds a usage row, refusing one whose stored value its own type will not take.
pub(super) fn recorded(row: SqliteRow) -> Result<Usage, StorageError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::fixture::*;
    use crate::suite::usage::spent;

    /// Records a request, then writes over one column with something the code cannot produce.
    async fn written_over(store: &SqliteStore, update: &'static str) -> UsageRowId {
        let recorded = store.record_usage(spent()).await.unwrap();
        sqlx::query(update)
            .bind(recorded.row_id.get())
            .execute(&store.pool)
            .await
            .unwrap();
        recorded.row_id
    }

    /// Every column whose stored value its own type can refuse, and a value that it refuses.
    const REFUSED: [&str; 4] = [
        "UPDATE usage SET served = 'somewhere' WHERE row_id = ?",
        "UPDATE usage SET trace_id = 'not-a-trace' WHERE row_id = ?",
        "UPDATE usage SET input_tokens = 9223372036854775807 WHERE row_id = ?",
        "UPDATE usage SET latency_ms = -1 WHERE row_id = ?",
    ];

    #[tokio::test]
    async fn a_row_no_type_of_ours_will_take_is_reported_as_malformed() {
        let store = store().await;
        for update in REFUSED {
            let row_id = written_over(&store, update).await;
            assert!(
                matches!(
                    store.usage(row_id).await,
                    Err(StorageError::Malformed {
                        entity: Entity::Usage,
                        ..
                    })
                ),
                "{update} was read back"
            );
        }
    }
}
