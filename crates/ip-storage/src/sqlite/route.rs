use async_trait::async_trait;
use ip_core::{
    ApiId, Difficulty, Endpoint, Hedge, HedgeDelay, RouteKey, RouteRule, RouteTarget, Strategy,
    Timestamp, Weight, Weighted,
};
use sqlx::{Row, SqliteConnection};

use super::SqliteStore;
use crate::audit::{By, NewAudit};
use crate::codec::{dialect, dialect_name};
use crate::error::{Entity, StorageError};
use crate::route::RouteStore;

#[async_trait]
impl RouteStore for SqliteStore {
    async fn put_route(&self, rule: RouteRule, by: &By) -> Result<(), StorageError> {
        // One transaction, already, for the rule and its endpoints; the record joins them,
        // so a rule that lands without its record is not a state this store can be left in.
        let mut transaction = self.pool.begin().await.map_err(StorageError::backend)?;
        // Rolled back by hand rather than by dropping it: a transaction left to its
        // `Drop` keeps the connection's locks until the pool gets round to it, and a
        // scratch schema being torn down behind it waits for ever.
        match setting(&mut transaction, rule, by).await {
            Ok(()) => transaction.commit().await.map_err(StorageError::backend),
            Err(error) => {
                let _ = transaction.rollback().await;
                Err(error)
            }
        }
    }

    async fn remove_route(&self, key: &RouteKey, by: &By) -> Result<(), StorageError> {
        let mut transaction = self.pool.begin().await.map_err(StorageError::backend)?;
        match removing(&mut transaction, key, by).await {
            Ok(()) => transaction.commit().await.map_err(StorageError::backend),
            Err(error) => {
                let _ = transaction.rollback().await;
                Err(error)
            }
        }
    }

    async fn route(&self, key: &RouteKey) -> Result<Option<RouteRule>, StorageError> {
        read_route(&mut *self.connection().await?, key).await
    }

    async fn routes(&self) -> Result<Vec<RouteRule>, StorageError> {
        read_routes(&mut *self.connection().await?).await
    }
}

/// Records a rule and the change, both on the one connection its caller commits.
///
/// Apart from the method so that every way out of it is a `?` the caller can roll back,
/// rather than a return that leaves a transaction open.
async fn setting(
    connection: &mut SqliteConnection,
    rule: RouteRule,
    by: &By,
) -> Result<(), StorageError> {
    let before = read_route(connection, &rule.key).await?;
    let key = rule.key.endpoint();
    let route_row_id: i64 = sqlx::query_scalar(
        "INSERT INTO routes \
         (api_id, key_protocol, key_host, key_port, key_path, strategy, hedge, hedge_delay, \
         caller_speaks, upstream_speaks, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (key_protocol, key_host, key_port, key_path) DO UPDATE SET \
         api_id = excluded.api_id, strategy = excluded.strategy, hedge = excluded.hedge, \
         hedge_delay = excluded.hedge_delay, \
         caller_speaks = excluded.caller_speaks, \
         upstream_speaks = excluded.upstream_speaks \
         RETURNING row_id",
    )
    .bind(rule.api.as_str())
    .bind(key.protocol.name())
    .bind(key.host.as_str())
    .bind(i64::from(key.port.get()))
    .bind(key.path.as_str())
    .bind(rule.strategy.name())
    .bind(i64::from(rule.hedge.get()))
    .bind(i64::from(rule.hedge_delay.get()))
    .bind(dialect_name(rule.caller_speaks))
    .bind(dialect_name(rule.upstream_speaks))
    .bind(Timestamp::now().unix_seconds())
    .fetch_one(&mut *connection)
    .await
    .map_err(StorageError::backend)?;

    sqlx::query("DELETE FROM route_targets WHERE route_row_id = ?")
        .bind(route_row_id)
        .execute(&mut *connection)
        .await
        .map_err(StorageError::backend)?;

    for (position, weighted) in rule.target.weighted().iter().enumerate() {
        let endpoint = &weighted.endpoint;
        sqlx::query(
            "INSERT INTO route_targets \
             (route_row_id, position, protocol, host, port, path, weight, serves) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(route_row_id)
        .bind(i64::try_from(position).unwrap_or(i64::MAX))
        .bind(endpoint.protocol.name())
        .bind(endpoint.host.as_str())
        .bind(i64::from(endpoint.port.get()))
        .bind(endpoint.path.as_str())
        .bind(i64::from(weighted.weight.get()))
        .bind(weighted.serves.map(Difficulty::name))
        .execute(&mut *connection)
        .await
        .map_err(StorageError::backend)?;
    }

    super::audit::record(
        connection,
        NewAudit::changed(by.clone(), before.as_ref(), &rule),
    )
    .await?;
    Ok(())
}

/// Removes a rule and records the removal, on the one connection its caller commits.
async fn removing(
    connection: &mut SqliteConnection,
    key: &RouteKey,
    by: &By,
) -> Result<(), StorageError> {
    let before = read_route(connection, key).await?;
    let endpoint = key.endpoint();
    let deleted = sqlx::query(
        "DELETE FROM routes WHERE key_protocol = ? AND key_host = ? \
         AND key_port = ? AND key_path = ?",
    )
    .bind(endpoint.protocol.name())
    .bind(endpoint.host.as_str())
    .bind(i64::from(endpoint.port.get()))
    .bind(endpoint.path.as_str())
    .execute(&mut *connection)
    .await
    .map_err(StorageError::backend)?
    .rows_affected();
    if deleted == 0 {
        return Err(StorageError::NotFound {
            entity: Entity::Route,
            id: key.to_string(),
        });
    }
    // A row was deleted, so something was there to read a moment ago in the same
    // transaction; an absent before here would mean the two disagree.
    let Some(before) = before else {
        return Err(StorageError::Malformed {
            entity: Entity::Route,
            detail: "a route was removed that nothing could read".to_owned(),
        });
    };
    super::audit::record(connection, NewAudit::removed(by.clone(), &before)).await?;
    Ok(())
}

/// The rule under one key, over whichever connection it is given. Read inside the
/// transaction that is about to change it, so the before it records is what was replaced.
pub(super) async fn read_route(
    connection: &mut SqliteConnection,
    key: &RouteKey,
) -> Result<Option<RouteRule>, StorageError> {
    let endpoint = key.endpoint();
    let rows = sqlx::query(
        "SELECT r.api_id, r.strategy, r.hedge, r.hedge_delay, r.caller_speaks, \
         r.upstream_speaks, t.protocol, t.host, t.port, \
         t.path, t.weight, \
         t.serves \
         FROM routes r \
         JOIN route_targets t ON t.route_row_id = r.row_id \
         WHERE r.key_protocol = ? AND r.key_host = ? AND r.key_port = ? AND r.key_path = ? \
         ORDER BY t.position",
    )
    .bind(endpoint.protocol.name())
    .bind(endpoint.host.as_str())
    .bind(i64::from(endpoint.port.get()))
    .bind(endpoint.path.as_str())
    .fetch_all(&mut *connection)
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
        target: RouteTarget::from_weighted(targets)?,
        strategy: strategy_of(first)?,
        hedge: hedge_of(first)?,
        hedge_delay: hedge_delay_of(first)?,
        caller_speaks: dialect(first.get("caller_speaks"))?,
        upstream_speaks: dialect(first.get("upstream_speaks"))?,
    }))
}

/// Every stored routing rule, over whichever connection it is given.
pub(super) async fn read_routes(
    connection: &mut SqliteConnection,
) -> Result<Vec<RouteRule>, StorageError> {
    let rows = sqlx::query(
        "SELECT r.row_id, r.api_id, r.key_protocol, r.key_host, r.key_port, r.key_path, \
         r.strategy, r.hedge, r.hedge_delay, r.caller_speaks, r.upstream_speaks, \
         t.protocol, t.host, t.port, t.path, t.weight, t.serves FROM routes r \
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
            let mut weighted = rule.target.weighted().to_vec();
            weighted.push(endpoint);
            rule.target = RouteTarget::from_weighted(weighted)?;
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
            target: RouteTarget::from_weighted(vec![endpoint])?,
            strategy: strategy_of(row)?,
            hedge: hedge_of(row)?,
            hedge_delay: hedge_delay_of(row)?,
            caller_speaks: dialect(row.get("caller_speaks"))?,
            upstream_speaks: dialect(row.get("upstream_speaks"))?,
        });
    }
    Ok(rules)
}

fn target_of(row: &sqlx::sqlite::SqliteRow) -> Result<Weighted, StorageError> {
    let endpoint = Endpoint::from_parts(
        row.get("protocol"),
        row.get("host"),
        row.get("port"),
        row.get("path"),
    )?;
    let weight: i64 = row.get("weight");
    let weight = u32::try_from(weight)
        .map(Weight::new)
        .map_err(|_| StorageError::Malformed {
            entity: Entity::Route,
            detail: format!("{weight} is not a share"),
        })?;
    Ok(Weighted::serving(endpoint, weight, serves_of(row)?))
}

/// The hardest class a stored endpoint serves, refusing a name this code does not carry.
fn serves_of(row: &sqlx::sqlite::SqliteRow) -> Result<Option<Difficulty>, StorageError> {
    let named: Option<String> = row.get("serves");
    Ok(named.map(|name| Difficulty::named(&name)).transpose()?)
}

/// How a stored rule spreads its requests, refusing a strategy this code does not carry.
fn strategy_of(row: &sqlx::sqlite::SqliteRow) -> Result<Strategy, StorageError> {
    let named: String = row.get("strategy");
    Ok(Strategy::named(&named)?)
}

/// How many endpoints a stored rule races across, refusing a count this code will not race.
fn hedge_of(row: &sqlx::sqlite::SqliteRow) -> Result<Hedge, StorageError> {
    let stored: i64 = row.get("hedge");
    let across = u8::try_from(stored).map_err(|_| StorageError::Malformed {
        entity: Entity::Route,
        detail: format!("{stored} is not a hedge"),
    })?;
    Ok(Hedge::new(across)?)
}

/// How long a stored rule waits before its next arm, refusing a wait this code cannot hold.
fn hedge_delay_of(row: &sqlx::sqlite::SqliteRow) -> Result<HedgeDelay, StorageError> {
    let stored: i64 = row.get("hedge_delay");
    let millis = u16::try_from(stored).map_err(|_| StorageError::Malformed {
        entity: Entity::Route,
        detail: format!("{stored} is not a hedge delay"),
    })?;
    Ok(HedgeDelay::new(millis))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::fixture::*;
    use crate::suite::route::{by, rule};

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
        store.put_route(rule.clone(), &by()).await.unwrap();
        store.remove_route(&rule.key, &by()).await.unwrap();
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

    /// A hedge wider than this code races, written around the driver.
    #[tokio::test]
    async fn a_stored_hedge_this_gateway_will_not_race_is_refused_on_read() {
        let store = store().await;
        store
            .put_route(rule("gateway.local", "/v1", "api.example.com"), &by())
            .await
            .unwrap();
        sqlx::query("UPDATE routes SET hedge = 99")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(matches!(
            store.routes().await,
            Err(StorageError::Value(ip_core::CoreError::HedgeTooWide { .. }))
        ));
    }

    /// A hedge of nothing never reaches the read: the schema itself refuses to hold one.
    #[tokio::test]
    async fn a_hedge_of_nothing_cannot_be_written_even_around_the_driver() {
        let store = store().await;
        store
            .put_route(rule("gateway.local", "/v1", "api.example.com"), &by())
            .await
            .unwrap();
        assert!(
            sqlx::query("UPDATE routes SET hedge = 0")
                .execute(&store.pool)
                .await
                .is_err()
        );
    }

    /// A wait past the width this code holds, written around the driver into a rule's delay.
    #[tokio::test]
    async fn a_stored_hedge_delay_this_code_cannot_hold_is_refused_on_read() {
        let store = store().await;
        store
            .put_route(rule("gateway.local", "/v1", "api.example.com"), &by())
            .await
            .unwrap();
        sqlx::query("UPDATE routes SET hedge_delay = 70000")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(matches!(
            store
                .route(&rule("gateway.local", "/v1", "api.example.com").key)
                .await,
            Err(StorageError::Malformed {
                entity: Entity::Route,
                ..
            })
        ));
    }

    /// A family this code does not carry, written around the driver into a rule, is
    /// refused on the single read and on the listing both.
    #[tokio::test]
    async fn a_stored_dialect_this_code_does_not_carry_is_refused_on_read() {
        use crate::list::ListStore;
        let store = store().await;
        store
            .put_route(rule("gateway.local", "/v1", "api.example.com"), &by())
            .await
            .unwrap();
        sqlx::query("UPDATE routes SET upstream_speaks = 'cohere'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(matches!(
            store
                .route(&rule("gateway.local", "/v1", "api.example.com").key)
                .await,
            Err(StorageError::Value(
                ip_core::CoreError::UnknownDialect { .. }
            ))
        ));
        assert!(matches!(
            store.list_routes(crate::Page::default()).await,
            Err(StorageError::Value(
                ip_core::CoreError::UnknownDialect { .. }
            ))
        ));
    }

    /// A listed rule carrying the same impossible wait is refused the same way.
    #[tokio::test]
    async fn a_listed_hedge_delay_this_code_cannot_hold_is_refused_on_read() {
        use crate::list::ListStore;
        let store = store().await;
        store
            .put_route(rule("gateway.local", "/v1", "api.example.com"), &by())
            .await
            .unwrap();
        sqlx::query("UPDATE routes SET hedge_delay = 70000")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(matches!(
            store.list_routes(crate::Page::default()).await,
            Err(StorageError::Malformed {
                entity: Entity::Route,
                ..
            })
        ));
    }

    /// A class no endpoint serves, written around the driver into an endpoint's ceiling.
    #[tokio::test]
    async fn a_stored_class_this_code_does_not_carry_is_refused_on_read() {
        let store = store().await;
        store
            .put_route(rule("gateway.local", "/v1", "api.example.com"), &by())
            .await
            .unwrap();
        sqlx::query("UPDATE route_targets SET serves = 'impossible'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(matches!(
            store.routes().await,
            Err(StorageError::Value(
                ip_core::CoreError::UnknownDifficulty { .. }
            ))
        ));
    }

    /// A listed row carrying the same impossible name is refused the same way.
    #[tokio::test]
    async fn a_listed_class_this_code_does_not_carry_is_refused_on_read() {
        use crate::list::ListStore;
        let store = store().await;
        store
            .put_route(rule("gateway.local", "/v1", "api.example.com"), &by())
            .await
            .unwrap();
        sqlx::query("UPDATE route_targets SET serves = 'impossible'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(matches!(
            store.list_routes(crate::Page::default()).await,
            Err(StorageError::Value(
                ip_core::CoreError::UnknownDifficulty { .. }
            ))
        ));
    }
}
