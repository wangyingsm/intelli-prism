use async_trait::async_trait;
use ip_core::{ApiId, Endpoint, RouteKey, RouteRule, RouteTarget, Timestamp};
use sqlx::{Row, SqliteConnection};

use super::SqliteStore;
use crate::error::{Entity, StorageError};
use crate::route::RouteStore;

#[async_trait]
impl RouteStore for SqliteStore {
    async fn put_route(&self, rule: RouteRule) -> Result<(), StorageError> {
        let key = rule.key.endpoint();
        let mut transaction = self.pool.begin().await.map_err(StorageError::backend)?;
        let route_row_id: i64 = sqlx::query_scalar(
            "INSERT INTO routes (api_id, key_protocol, key_host, key_port, key_path, created_at) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT (key_protocol, key_host, key_port, key_path) DO UPDATE SET \
             api_id = excluded.api_id \
             RETURNING row_id",
        )
        .bind(rule.api.as_str())
        .bind(key.protocol.name())
        .bind(key.host.as_str())
        .bind(i64::from(key.port.get()))
        .bind(key.path.as_str())
        .bind(Timestamp::now().unix_seconds())
        .fetch_one(&mut *transaction)
        .await
        .map_err(StorageError::backend)?;

        sqlx::query("DELETE FROM route_targets WHERE route_row_id = ?")
            .bind(route_row_id)
            .execute(&mut *transaction)
            .await
            .map_err(StorageError::backend)?;

        for (position, endpoint) in rule.target.endpoints().iter().enumerate() {
            sqlx::query(
                "INSERT INTO route_targets (route_row_id, position, protocol, host, port, path) \
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(route_row_id)
            .bind(i64::try_from(position).unwrap_or(i64::MAX))
            .bind(endpoint.protocol.name())
            .bind(endpoint.host.as_str())
            .bind(i64::from(endpoint.port.get()))
            .bind(endpoint.path.as_str())
            .execute(&mut *transaction)
            .await
            .map_err(StorageError::backend)?;
        }

        transaction.commit().await.map_err(StorageError::backend)
    }

    async fn remove_route(&self, key: &RouteKey) -> Result<(), StorageError> {
        let endpoint = key.endpoint();
        let deleted = sqlx::query(
            "DELETE FROM routes WHERE key_protocol = ? AND key_host = ? \
             AND key_port = ? AND key_path = ?",
        )
        .bind(endpoint.protocol.name())
        .bind(endpoint.host.as_str())
        .bind(i64::from(endpoint.port.get()))
        .bind(endpoint.path.as_str())
        .execute(&self.pool)
        .await
        .map_err(StorageError::backend)?
        .rows_affected();
        if deleted == 0 {
            return Err(StorageError::NotFound {
                entity: Entity::Route,
                id: key.to_string(),
            });
        }
        Ok(())
    }

    async fn route(&self, key: &RouteKey) -> Result<Option<RouteRule>, StorageError> {
        let endpoint = key.endpoint();
        let rows = sqlx::query(
            "SELECT r.api_id, t.protocol, t.host, t.port, t.path FROM routes r \
             JOIN route_targets t ON t.route_row_id = r.row_id \
             WHERE r.key_protocol = ? AND r.key_host = ? AND r.key_port = ? AND r.key_path = ? \
             ORDER BY t.position",
        )
        .bind(endpoint.protocol.name())
        .bind(endpoint.host.as_str())
        .bind(i64::from(endpoint.port.get()))
        .bind(endpoint.path.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        let Some(first) = rows.first() else {
            return Ok(None);
        };
        let api = ApiId::new(first.get("api_id"))?;
        let targets = rows.iter().map(target_of).collect::<Result<Vec<_>, _>>()?;
        Ok(Some(RouteRule {
            api,
            key: key.clone(),
            target: RouteTarget::from_endpoints(targets)?,
        }))
    }

    async fn routes(&self) -> Result<Vec<RouteRule>, StorageError> {
        read_routes(&mut *self.connection().await?).await
    }
}

/// Every stored routing rule, over whichever connection it is given.
pub(super) async fn read_routes(
    connection: &mut SqliteConnection,
) -> Result<Vec<RouteRule>, StorageError> {
    let rows = sqlx::query(
        "SELECT r.row_id, r.api_id, r.key_protocol, r.key_host, r.key_port, r.key_path, \
         t.protocol, t.host, t.port, t.path FROM routes r \
         JOIN route_targets t ON t.route_row_id = r.row_id \
         ORDER BY r.key_host, r.key_path, r.key_protocol, r.key_port, t.position",
    )
    .fetch_all(&mut *connection)
    .await
    .map_err(StorageError::backend)?;

    let mut rules: Vec<RouteRule> = Vec::new();
    let mut current: Option<i64> = None;
    for row in &rows {
        let route_row_id: i64 = row.get("row_id");
        let endpoint = target_of(row)?;
        if current == Some(route_row_id)
            && let Some(rule) = rules.last_mut()
        {
            let mut endpoints = rule.target.endpoints().to_vec();
            endpoints.push(endpoint);
            rule.target = RouteTarget::from_endpoints(endpoints)?;
            continue;
        }
        current = Some(route_row_id);
        rules.push(RouteRule {
            api: ApiId::new(row.get("api_id"))?,
            key: RouteKey::new(Endpoint::from_parts(
                row.get("key_protocol"),
                row.get("key_host"),
                row.get("key_port"),
                row.get("key_path"),
            )?),
            target: RouteTarget::new(endpoint),
        });
    }
    Ok(rules)
}

fn target_of(row: &sqlx::sqlite::SqliteRow) -> Result<Endpoint, StorageError> {
    Ok(Endpoint::from_parts(
        row.get("protocol"),
        row.get("host"),
        row.get("port"),
        row.get("path"),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::fixture::*;
    use crate::suite::route::rule;

    /// Writes a route around the driver, so a row the code could not have written is read back.
    async fn broken_route(store: &SqliteStore, key_protocol: &str, protocol: &str, port: i64) {
        let route_row_id: i64 = sqlx::query_scalar(
            "INSERT INTO routes (api_id, key_protocol, key_host, key_port, key_path) \
             VALUES ('anthropic', ?, 'gateway.local', 443, '/v1') RETURNING row_id",
        )
        .bind(key_protocol)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO route_targets (route_row_id, position, protocol, host, port, path) \
             VALUES (?, 0, ?, 'api.example.com', ?, '/v1')",
        )
        .bind(route_row_id)
        .bind(protocol)
        .bind(port)
        .execute(&store.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn removing_a_route_takes_its_endpoints_with_it() {
        let store = store().await;
        let rule = rule("gateway.local", "/v1", "one.example.com");
        store.put_route(rule.clone()).await.unwrap();
        store.remove_route(&rule.key).await.unwrap();
        let orphans: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM route_targets")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(orphans, 0);
    }

    #[tokio::test]
    async fn a_stored_protocol_the_code_does_not_know_is_refused() {
        let store = store().await;
        broken_route(&store, "gopher", "https", 443).await;
        assert!(matches!(
            store.routes().await,
            Err(StorageError::Value(
                ip_core::CoreError::UnknownProtocol { .. }
            ))
        ));
    }

    #[tokio::test]
    async fn a_stored_port_of_zero_is_refused_on_read() {
        let store = store().await;
        broken_route(&store, "https", "https", 0).await;
        assert!(matches!(
            store.routes().await,
            Err(StorageError::Value(ip_core::CoreError::ZeroPort))
        ));
    }
}
