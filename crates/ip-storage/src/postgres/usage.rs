use async_trait::async_trait;
use ip_core::{
    ApiId, Counted, Hedge, ModelName, Replica, TenantId, Timestamp, Tokens, TraceId, TurnId, UserId,
};
use sqlx::Row;
use sqlx::postgres::PgRow;

use super::PostgresStore;
use crate::codec::{latency, served as served_of, served_name, token_count};
use crate::error::{Entity, StorageError};
use crate::usage::{NewUsage, Usage, UsageFilter, UsageRowId, UsageStore};

#[async_trait]
impl UsageStore for PostgresStore {
    async fn record_usage(&self, usage: NewUsage) -> Result<Usage, StorageError> {
        let row = sqlx::query(
            "INSERT INTO usage (trace_id, turn_id, tenant_id, user_id, api_id, model, \
             input_tokens, output_tokens, served, served_host, served_port, latency_ms, \
             created_at, raced_across) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) \
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
        .bind(
            usage
                .served_by
                .as_ref()
                .map(|replica| replica.host.as_str().to_owned()),
        )
        .bind(
            usage
                .served_by
                .as_ref()
                .map(|replica| i64::from(replica.port.get())),
        )
        .bind(i64::from(usage.latency.millis()))
        .bind(Timestamp::now().unix_seconds())
        .bind(i64::from(usage.raced_across.get()))
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
            served_by: usage.served_by,
            raced_across: usage.raced_across,
            latency: usage.latency,
            created_at: Timestamp::from_unix_seconds(row.get("created_at"))?,
        })
    }

    async fn usage(&self, row_id: UsageRowId) -> Result<Option<Usage>, StorageError> {
        let row = sqlx::query(
            "SELECT row_id, trace_id, turn_id, tenant_id, user_id, api_id, model, \
             input_tokens, output_tokens, served, served_host, served_port, latency_ms, \
             created_at, raced_across FROM usage WHERE row_id = $1",
        )
        .bind(row_id.get())
        .fetch_optional(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        row.map(recorded).transpose()
    }

    async fn spent(
        &self,
        filter: &UsageFilter,
        counted: Counted,
        moment: Timestamp,
    ) -> Result<u64, StorageError> {
        let query = match counted {
            Counted::Tokens => sqlx::query_scalar::<_, i64>(
                "SELECT COALESCE(SUM(input_tokens + output_tokens), 0)::BIGINT FROM usage \
                 WHERE created_at >= $1 \
                 AND ($2::TEXT IS NULL OR tenant_id = $2) \
                 AND ($3::TEXT IS NULL OR user_id = $3) \
                 AND ($4::TEXT IS NULL OR api_id = $4)",
            ),
            Counted::Requests => sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM usage \
                 WHERE created_at >= $1 \
                 AND ($2::TEXT IS NULL OR tenant_id = $2) \
                 AND ($3::TEXT IS NULL OR user_id = $3) \
                 AND ($4::TEXT IS NULL OR api_id = $4)",
            ),
        };
        let spent = query
            .bind(moment.unix_seconds())
            .bind(filter.tenant.as_ref().map(TenantId::as_str))
            .bind(filter.user.as_ref().map(UserId::as_str))
            .bind(filter.api.as_ref().map(ApiId::as_str))
            .fetch_one(&self.pool)
            .await
            .map_err(StorageError::backend)?;
        Ok(u64::try_from(spent).unwrap_or(0))
    }

    async fn sweep_usage(&self, moment: Timestamp) -> Result<u64, StorageError> {
        let swept = sqlx::query("DELETE FROM usage WHERE created_at < $1")
            .bind(moment.unix_seconds())
            .execute(&self.pool)
            .await
            .map_err(StorageError::backend)?
            .rows_affected();
        Ok(swept)
    }
}

/// Which endpoint answered, when the row names one. A row naming half of one is refused: a
/// host without a port is no endpoint.
fn served_by(row: &PgRow) -> Result<Option<Replica>, StorageError> {
    let host: Option<String> = row.get("served_host");
    let port: Option<i64> = row.get("served_port");
    match (host, port) {
        (Some(host), Some(port)) => Ok(Some(Replica {
            host: ip_core::Host::new(&host)?,
            port: ip_core::Port::try_from(port)?,
        })),
        (None, None) => Ok(None),
        (host, port) => Err(StorageError::Malformed {
            entity: Entity::Usage,
            detail: format!("{host:?} and {port:?} are half an endpoint"),
        }),
    }
}

/// How many endpoints one request was raced across, as the row holds it.
///
/// A stored count the type refuses is a row the code cannot have written.
fn raced_across(row: &PgRow) -> Result<Hedge, StorageError> {
    let stored: i64 = row.get("raced_across");
    let across = u8::try_from(stored).map_err(|_| StorageError::Malformed {
        entity: Entity::Usage,
        detail: format!("{stored} is not a count of endpoints"),
    })?;
    Hedge::new(across).map_err(|_| StorageError::Malformed {
        entity: Entity::Usage,
        detail: format!("{stored} is not a count of endpoints"),
    })
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
        served_by: served_by(&row)?,
        raced_across: raced_across(&row)?,
        latency: latency(row.get("latency_ms"))?,
        created_at: Timestamp::from_unix_seconds(row.get("created_at"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postgres::scratch;

    /// A row written around the driver, holding no count at all, reads back as one endpoint, the
    /// column's default.
    #[tokio::test]
    async fn a_row_holding_no_count_reads_back_as_one() {
        let Some(store) = scratch::store().await else {
            return;
        };
        sqlx::query(
            "INSERT INTO usage (trace_id, tenant_id, user_id, api_id, input_tokens, \
             output_tokens, served, latency_ms, created_at) \
             VALUES ('11111111111111111111111111111111', 'acme', 'alice', 'anthropic', 0, 0, \
             'upstream', 1, 0)",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        let row_id: i64 = sqlx::query_scalar("SELECT row_id FROM usage WHERE trace_id = $1")
            .bind("11111111111111111111111111111111")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        let read = store.usage(UsageRowId::new(row_id)).await.unwrap().unwrap();
        assert_eq!(read.raced_across, Hedge::NONE);
    }
}
