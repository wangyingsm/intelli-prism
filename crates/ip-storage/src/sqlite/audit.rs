use async_trait::async_trait;
use ip_core::{TenantId, Timestamp, UserId};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection};

use super::SqliteStore;
use crate::audit::{AuditFilter, AuditStore, Audited, NewAudit};
use crate::error::{Entity, StorageError};
use crate::list::Page;

/// Records one change, over the same connection the change itself was written on, and
/// says when it was recorded.
///
/// Takes a connection rather than the store so that it can only be called from inside a
/// transaction that is also writing the thing being described.
pub(super) async fn record(
    connection: &mut SqliteConnection,
    audit: NewAudit,
) -> Result<Timestamp, StorageError> {
    let created_at = Timestamp::now();
    sqlx::query(
        "INSERT INTO audit \
         (actor, through, entity, entity_id, tenant, before, after, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
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
impl AuditStore for SqliteStore {
    async fn audits(&self, filter: &AuditFilter, page: Page) -> Result<Vec<Audited>, StorageError> {
        let rows = sqlx::query(
            "SELECT actor, through, entity, entity_id, tenant, before, after, created_at \
             FROM audit \
             WHERE created_at > ?1 \
             AND (?2 IS NULL OR tenant = ?2) \
             AND (?3 IS NULL OR actor = ?3) \
             AND (?4 IS NULL OR entity = ?4) \
             ORDER BY created_at DESC, row_id DESC LIMIT ?5 OFFSET ?6",
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
fn recorded(row: SqliteRow) -> Result<Audited, StorageError> {
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

#[cfg(test)]
mod tests {
    use crate::limit::LimitStore;
    use crate::plugin::{PluginRuleStore, PluginStore};
    use crate::price::PriceStore;
    use crate::route::RouteStore;
    use crate::sqlite::fixture::*;
    use crate::suite::limit::{by, monthly};
    use crate::suite::price::priced_at;

    /// The other half of the pair: a change whose record cannot be written does not
    /// happen either.
    ///
    /// `FailingStore` wraps whole methods and so cannot reach inside one; taking the
    /// table away is the only fault that makes the second write of the transaction fail
    /// while the first succeeds, which is exactly the case the transaction exists for.
    #[tokio::test]
    async fn a_change_whose_record_cannot_be_written_is_rolled_back() {
        let store = store().await;
        tenant_with_user(&store).await;
        sqlx::query("DROP TABLE audit")
            .execute(&store.pool)
            .await
            .unwrap();

        assert!(
            store.put_limit(monthly(1_000), &by()).await.is_err(),
            "a limit was set with nothing to record it"
        );
        assert!(
            store.limits().await.unwrap().is_empty(),
            "the limit survived a transaction that could not record it"
        );
    }

    /// Held per family rather than once: the transaction is written out again in each
    /// one, so it is each one that can be written without it.
    #[tokio::test]
    async fn a_price_whose_record_cannot_be_written_is_rolled_back() {
        let store = store().await;
        sqlx::query("DROP TABLE audit")
            .execute(&store.pool)
            .await
            .unwrap();

        assert!(
            store
                .put_price(priced_at(3_000_000, 15_000_000), &crate::suite::price::by())
                .await
                .is_err(),
            "a price was set with nothing to record it"
        );
        assert!(
            store.prices().await.unwrap().is_empty(),
            "the price survived a transaction that could not record it"
        );
    }

    #[tokio::test]
    async fn a_route_whose_record_cannot_be_written_is_rolled_back() {
        let store = store().await;
        sqlx::query("DROP TABLE audit")
            .execute(&store.pool)
            .await
            .unwrap();

        assert!(
            store
                .put_route(
                    crate::suite::route::rule("gateway.local", "/v1", "api.example.com"),
                    &crate::suite::route::by(),
                )
                .await
                .is_err(),
            "a rule was stored with nothing to record it"
        );
        assert!(
            store.routes().await.unwrap().is_empty(),
            "the rule survived a transaction that could not record it"
        );
    }

    /// The typestate flow will not compile without its recording step, so this proves the
    /// weaker thing the others do: the step runs inside the transaction it belongs to.
    #[tokio::test]
    async fn a_plugin_whose_record_cannot_be_written_is_rolled_back() {
        let store = store().await;
        sqlx::query("DROP TABLE audit")
            .execute(&store.pool)
            .await
            .unwrap();

        assert!(
            store
                .put_plugin(
                    crate::suite::plugin::plugin(ip_core::PluginKind::ReqBody, b"module"),
                    &crate::suite::plugin::by(),
                )
                .await
                .is_err(),
            "wasm was stored with nothing to record it"
        );
        assert!(
            store.plugins().await.unwrap().is_empty(),
            "the plugin survived a transaction that could not record it"
        );
    }

    #[tokio::test]
    async fn a_rule_whose_record_cannot_be_written_is_rolled_back() {
        let store = store().await;
        let stored = store
            .put_plugin(
                crate::suite::plugin::plugin(ip_core::PluginKind::ReqBody, b"module"),
                &crate::suite::plugin::by(),
            )
            .await
            .unwrap();
        sqlx::query("DROP TABLE audit")
            .execute(&store.pool)
            .await
            .unwrap();

        assert!(
            store
                .put_rule(
                    ip_core::NewPluginRule::new(
                        stored.checksum,
                        ip_core::PluginOrder::new(10),
                        ip_core::PluginScope::Global,
                    )
                    .unwrap(),
                    &crate::suite::plugin::rules_by(),
                )
                .await
                .is_err(),
            "a rule was placed with nothing to record it"
        );
        assert!(
            store.rules().await.unwrap().is_empty(),
            "the rule survived a transaction that could not record it"
        );
    }
}
