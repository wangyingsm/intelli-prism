use async_trait::async_trait;
use ip_core::{
    ApiId, Counted, Decider, Difficulty, Hedge, ModelName, Replica, Served, TenantId, Timestamp,
    Tokens, TraceId, TurnId, UserId,
};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use super::SqliteStore;
use crate::codec::{
    decided_by, decided_by_name, latency, served as served_of, served_name, token_count,
};
use crate::error::{Entity, StorageError};
use crate::usage::{Decided, NewUsage, SpentArms, Usage, UsageFilter, UsageRowId, UsageStore};

#[async_trait]
impl UsageStore for SqliteStore {
    async fn record_usage(&self, usage: NewUsage) -> Result<Usage, StorageError> {
        let row = sqlx::query(
            "INSERT INTO usage (trace_id, turn_id, tenant_id, user_id, api_id, model, \
             input_tokens, output_tokens, served, served_host, served_port, latency_ms, \
             created_at, raced_across, dispatched_class, decided_by) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
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
        .bind(usage.dispatched_class.map(Difficulty::name))
        .bind(decided_by_name(usage.decided_by))
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
            dispatched_class: usage.dispatched_class,
            decided_by: usage.decided_by,
            latency: usage.latency,
            created_at: Timestamp::from_unix_seconds(row.get("created_at"))?,
        })
    }

    async fn usage(&self, row_id: UsageRowId) -> Result<Option<Usage>, StorageError> {
        let row = sqlx::query(
            "SELECT row_id, trace_id, turn_id, tenant_id, user_id, api_id, model, \
             input_tokens, output_tokens, served, served_host, served_port, latency_ms, \
             created_at, raced_across, dispatched_class, decided_by FROM usage WHERE row_id = ?",
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
                "SELECT COALESCE(SUM(input_tokens + output_tokens), 0) FROM usage \
                 WHERE created_at >= ?1 \
                 AND (?2 IS NULL OR tenant_id = ?2) \
                 AND (?3 IS NULL OR user_id = ?3) \
                 AND (?4 IS NULL OR api_id = ?4)",
            ),
            Counted::Requests => sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM usage \
                 WHERE created_at >= ?1 \
                 AND (?2 IS NULL OR tenant_id = ?2) \
                 AND (?3 IS NULL OR user_id = ?3) \
                 AND (?4 IS NULL OR api_id = ?4)",
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

    /// How many requests the rows matching `filter` answered since `moment`, and how many
    /// arms went out for them. `sent` takes its sum where `served` is the upstream: a
    /// cached answer carries an arms count of one without having sent anything, and every
    /// row — the cache's included — is an answered request.
    async fn sent_upstream(
        &self,
        filter: &UsageFilter,
        moment: Option<Timestamp>,
    ) -> Result<SpentArms, StorageError> {
        let row = sqlx::query(
            "SELECT COUNT(*), \
               COALESCE(SUM(CASE WHEN served = ?5 THEN raced_across ELSE 0 END), 0), \
               COUNT(decided_by) FILTER (WHERE decided_by = ?6), \
               COUNT(decided_by) FILTER (WHERE decided_by = ?7), \
               COUNT(decided_by) FILTER (WHERE decided_by = ?8), \
               COUNT(decided_by) FILTER (WHERE decided_by = ?9), \
               COUNT(*) FILTER (WHERE decided_by IS NULL) \
               FROM usage \
             WHERE created_at >= ?1 \
               AND (?2 IS NULL OR tenant_id = ?2) \
               AND (?3 IS NULL OR user_id = ?3) \
               AND (?4 IS NULL OR api_id = ?4)",
        )
        .bind(moment.map_or(i64::MIN, |moment| moment.unix_seconds()))
        .bind(filter.tenant.as_ref().map(TenantId::as_str))
        .bind(filter.user.as_ref().map(UserId::as_str))
        .bind(filter.api.as_ref().map(ApiId::as_str))
        .bind(served_name(Served::Upstream))
        .bind(decided_by_name(Some(Decider::Request)))
        .bind(decided_by_name(Some(Decider::Turn)))
        .bind(decided_by_name(Some(Decider::Classifier)))
        .bind(decided_by_name(Some(Decider::Arbiter)))
        .fetch_one(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        let answered: i64 = row.get(0);
        let sent: i64 = row.get(1);
        let counted = |at: usize| u64::try_from(row.get::<i64, _>(at)).unwrap_or(0);
        Ok(SpentArms {
            answered: u64::try_from(answered).unwrap_or(0),
            sent: u64::try_from(sent).unwrap_or(0),
            // Columns two to six, in the order the levels were bound above.
            decided: Decided {
                request: counted(2),
                turn: counted(3),
                classifier: counted(4),
                arbiter: counted(5),
                undecided: counted(6),
            },
        })
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

/// Which endpoint answered, when the row names one. A row naming half of one is refused: a
/// host without a port is no endpoint.
fn served_by(row: &SqliteRow) -> Result<Option<Replica>, StorageError> {
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
fn raced_across(row: &SqliteRow) -> Result<Hedge, StorageError> {
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

/// The class a stored row was dispatched at, refusing a name this code does not carry.
fn dispatched_class(row: &SqliteRow) -> Result<Option<Difficulty>, StorageError> {
    let named: Option<String> = row.get("dispatched_class");
    Ok(named.map(|name| Difficulty::named(&name)).transpose()?)
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
        served_by: served_by(&row)?,
        raced_across: raced_across(&row)?,
        dispatched_class: dispatched_class(&row)?,
        decided_by: decided_by(row.get("decided_by"))?,
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
    const REFUSED: [&str; 7] = [
        "UPDATE usage SET served = 'somewhere' WHERE row_id = ?",
        "UPDATE usage SET trace_id = 'not-a-trace' WHERE row_id = ?",
        "UPDATE usage SET input_tokens = 9223372036854775807 WHERE row_id = ?",
        "UPDATE usage SET latency_ms = -1 WHERE row_id = ?",
        "UPDATE usage SET served_port = NULL WHERE row_id = ?",
        "UPDATE usage SET served_host = NULL WHERE row_id = ?",
        "UPDATE usage SET raced_across = 9 WHERE row_id = ?",
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

    /// A count the schema's own check will not store, written around the driver, is refused by
    /// the schema itself, before any read ever sees it.
    #[tokio::test]
    async fn a_count_the_schema_will_not_store_is_refused_around_the_driver() {
        let store = store().await;
        let row_id = store.record_usage(spent()).await.unwrap().row_id;
        assert!(
            sqlx::query("UPDATE usage SET raced_across = 0 WHERE row_id = ?")
                .bind(row_id.get())
                .execute(&store.pool)
                .await
                .is_err()
        );
    }

    /// A row written around the driver, holding no count at all, reads back as one endpoint.
    #[tokio::test]
    async fn a_row_holding_no_count_reads_back_as_one() {
        let store = store().await;
        sqlx::query(
            "INSERT INTO usage (trace_id, tenant_id, user_id, api_id, input_tokens, \
             output_tokens, served, latency_ms, created_at) \
             VALUES ('11111111111111111111111111111111', 'acme', 'alice', 'anthropic', 0, 0, \
             'upstream', 1, 0)",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        let row_id: i64 = sqlx::query_scalar("SELECT row_id FROM usage WHERE trace_id = ?")
            .bind("11111111111111111111111111111111")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        let read = store.usage(UsageRowId::new(row_id)).await.unwrap().unwrap();
        assert_eq!(read.raced_across, Hedge::NONE);
    }

    /// A class this code does not carry, written around the driver into a row's dispatch,
    /// is refused on the single read.
    #[tokio::test]
    async fn a_stored_class_this_code_does_not_carry_is_refused_on_read() {
        let store = store().await;
        let row_id = store.record_usage(spent()).await.unwrap().row_id;
        sqlx::query("UPDATE usage SET dispatched_class = 'impossible' WHERE row_id = ?")
            .bind(row_id.get())
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(matches!(
            store.usage(row_id).await,
            Err(StorageError::Value(
                ip_core::CoreError::UnknownDifficulty { .. }
            ))
        ));
    }

    /// A level this code does not carry, written around the driver into a row, is
    /// refused on the single read.
    #[tokio::test]
    async fn a_stored_decider_this_code_does_not_carry_is_refused_on_read() {
        let store = store().await;
        let row_id = store.record_usage(spent()).await.unwrap().row_id;
        sqlx::query("UPDATE usage SET decided_by = 'impossible' WHERE row_id = ?")
            .bind(row_id.get())
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(matches!(
            store.usage(row_id).await,
            Err(StorageError::Value(
                ip_core::CoreError::UnknownDecider { .. }
            ))
        ));
    }

    /// A listed row carrying the same impossible level is refused the same way, which is
    /// where a column read and then discarded would go unnoticed.
    #[tokio::test]
    async fn a_listed_decider_this_code_does_not_carry_is_refused_on_read() {
        use crate::list::ListStore;
        let store = store().await;
        let row_id = store.record_usage(spent()).await.unwrap().row_id;
        sqlx::query("UPDATE usage SET decided_by = 'impossible' WHERE row_id = ?")
            .bind(row_id.get())
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(matches!(
            store
                .list_usage(
                    &crate::usage::UsageFilter::default(),
                    crate::Page::default()
                )
                .await,
            Err(StorageError::Value(
                ip_core::CoreError::UnknownDecider { .. }
            ))
        ));
    }

    /// A listed row carrying the same impossible name is refused the same way.
    #[tokio::test]
    async fn a_listed_class_this_code_does_not_carry_is_refused_on_read() {
        use crate::list::ListStore;
        let store = store().await;
        let row_id = store.record_usage(spent()).await.unwrap().row_id;
        sqlx::query("UPDATE usage SET dispatched_class = 'impossible' WHERE row_id = ?")
            .bind(row_id.get())
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(matches!(
            store
                .list_usage(
                    &crate::usage::UsageFilter::default(),
                    crate::Page::default()
                )
                .await,
            Err(StorageError::Value(
                ip_core::CoreError::UnknownDifficulty { .. }
            ))
        ));
    }
}
