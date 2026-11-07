use async_trait::async_trait;
use ip_core::Timestamp;
use ip_mcp::{McpEndpoint, McpId, McpName, McpServer, McpToken};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection};

use super::SqliteStore;
use crate::audit::{By, NewAudit};
use crate::codec::{mcp_auth, mcp_refused};
use crate::error::{Entity, StorageError};
use crate::list::Page;
use crate::mcp::{McpStore, StoredMcp};

#[async_trait]
impl McpStore for SqliteStore {
    async fn put_mcp(&self, server: McpServer, by: &By) -> Result<StoredMcp, StorageError> {
        // One transaction, so a server that lands without its record is not a state this store
        // can be left in, and neither is a record whose server rolled back.
        let mut transaction = self.pool.begin().await.map_err(StorageError::backend)?;
        match writing(&mut transaction, server, by).await {
            Ok(written) => transaction
                .commit()
                .await
                .map_err(StorageError::backend)
                .map(|()| written),
            Err(error) => {
                let _ = transaction.rollback().await;
                Err(error)
            }
        }
    }

    async fn remove_mcp(&self, id: &McpId, by: &By) -> Result<(), StorageError> {
        let mut transaction = self.pool.begin().await.map_err(StorageError::backend)?;
        match removing(&mut transaction, id, by).await {
            Ok(()) => transaction.commit().await.map_err(StorageError::backend),
            Err(error) => {
                let _ = transaction.rollback().await;
                Err(error)
            }
        }
    }

    async fn mcp(&self, id: &McpId) -> Result<Option<StoredMcp>, StorageError> {
        read_mcp(&mut *self.connection().await?, id).await
    }

    async fn mcps(&self, page: Page) -> Result<Vec<StoredMcp>, StorageError> {
        let rows = sqlx::query(
            "SELECT mcp_id, name, endpoint, auth_kind, auth_token, created_at FROM mcps \
             WHERE created_at > ?1 ORDER BY created_at DESC, row_id DESC LIMIT ?2 OFFSET ?3",
        )
        .bind(page.after_seconds())
        .bind(i64::from(page.limit()))
        .bind(i64::from(page.offset()))
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        rows.into_iter().map(stored).collect()
    }
}

/// Writes a server and records the change, both on the one connection its caller commits.
///
/// Apart from the method so that every way out of it is a `?` the caller can roll back, rather
/// than a return that leaves a transaction open.
async fn writing(
    connection: &mut SqliteConnection,
    server: McpServer,
    by: &By,
) -> Result<StoredMcp, StorageError> {
    let before = read_mcp(connection, &server.id).await?;
    // Removed and written rather than updated in place, so a replacement carries no column of
    // what it replaced -- a credential dropped for none would otherwise be left behind.
    sqlx::query("DELETE FROM mcps WHERE mcp_id = ?")
        .bind(server.id.as_str())
        .execute(&mut *connection)
        .await
        .map_err(StorageError::backend)?;

    let created_at = Timestamp::now();
    sqlx::query(
        "INSERT INTO mcps (mcp_id, name, endpoint, auth_kind, auth_token, created_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(server.id.as_str())
    .bind(server.name.as_str())
    .bind(server.endpoint.as_str())
    .bind(server.auth.name())
    .bind(server.auth.token().map(McpToken::expose))
    .bind(created_at.unix_seconds())
    .execute(&mut *connection)
    .await
    .map_err(StorageError::backend)?;

    let written = StoredMcp { server, created_at };
    super::audit::record(
        connection,
        NewAudit::changed(by.clone(), before.as_ref(), &written),
    )
    .await?;
    Ok(written)
}

/// Removes a server and records the removal, on the one connection its caller commits.
async fn removing(
    connection: &mut SqliteConnection,
    id: &McpId,
    by: &By,
) -> Result<(), StorageError> {
    let before = read_mcp(connection, id).await?;
    let removed = sqlx::query("DELETE FROM mcps WHERE mcp_id = ?")
        .bind(id.as_str())
        .execute(&mut *connection)
        .await
        .map_err(StorageError::backend)?
        .rows_affected();
    if removed == 0 {
        return Err(StorageError::NotFound {
            entity: Entity::Mcp,
            id: id.as_str().to_owned(),
        });
    }
    // A row was deleted, so something was there to read a moment ago in the same transaction;
    // an absent before here would mean the two disagree.
    let Some(before) = before else {
        return Err(StorageError::Malformed {
            entity: Entity::Mcp,
            detail: "a server was removed that nothing could read".to_owned(),
        });
    };
    super::audit::record(connection, NewAudit::removed(by.clone(), &before)).await?;
    Ok(())
}

/// One server, and nothing where none is stored under the id. Read inside the transaction that is
/// about to change it, so the before it records is the before that was replaced.
async fn read_mcp(
    connection: &mut SqliteConnection,
    id: &McpId,
) -> Result<Option<StoredMcp>, StorageError> {
    let row = sqlx::query(
        "SELECT mcp_id, name, endpoint, auth_kind, auth_token, created_at FROM mcps \
         WHERE mcp_id = ?",
    )
    .bind(id.as_str())
    .fetch_optional(&mut *connection)
    .await
    .map_err(StorageError::backend)?;
    row.map(stored).transpose()
}

/// Rebuilds a server, refusing a row whose stored value its own type will not take.
pub(super) fn stored(row: SqliteRow) -> Result<StoredMcp, StorageError> {
    Ok(StoredMcp {
        server: McpServer {
            id: McpId::new(row.get::<String, _>("mcp_id").as_str()).map_err(mcp_refused)?,
            name: McpName::new(row.get::<String, _>("name").as_str()).map_err(mcp_refused)?,
            endpoint: McpEndpoint::new(row.get::<String, _>("endpoint").as_str())
                .map_err(mcp_refused)?,
            auth: mcp_auth(
                row.get::<String, _>("auth_kind").as_str(),
                row.get("auth_token"),
            )?,
        },
        created_at: Timestamp::from_unix_seconds(row.get("created_at"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::fixture::*;
    use crate::suite::mcp::{bearer, by, server};

    /// Writes a server, then writes over one column with something the code cannot produce.
    async fn written_over(store: &SqliteStore, written: McpServer, update: &'static str) {
        store.put_mcp(written, &by()).await.unwrap();
        sqlx::query(update).execute(&store.pool).await.unwrap();
    }

    /// Every column whose stored value its own type can refuse, of a server requiring nothing.
    ///
    /// The last two are the pairing: `auth_kind` and `auth_token` carry no CHECK holding them
    /// to each other, which is what leaves this refusal reachable from a row at all.
    const REFUSED: [&str; 5] = [
        "UPDATE mcps SET mcp_id = 'not an id'",
        "UPDATE mcps SET name = ' untrimmed'",
        "UPDATE mcps SET endpoint = 'ftp://warehouse.test'",
        "UPDATE mcps SET auth_kind = 'mtls'",
        "UPDATE mcps SET auth_token = 'left behind'",
    ];

    /// The same, of a server that does require a credential.
    const REFUSED_WITH_A_CREDENTIAL: [&str; 2] = [
        "UPDATE mcps SET auth_token = NULL",
        "UPDATE mcps SET auth_token = ''",
    ];

    /// Asserted through the listing rather than a single read, because one of these columns is
    /// the id a single read looks the row up by: a refused id would simply not be found.
    #[tokio::test]
    async fn a_row_no_type_of_ours_will_take_is_refused_on_the_way_back() {
        for (written, update) in REFUSED
            .iter()
            .map(|update| (server("warehouse", "Warehouse"), *update))
            .chain(
                REFUSED_WITH_A_CREDENTIAL
                    .iter()
                    .map(|update| (bearer("warehouse", "Warehouse", "wh-live-9f2c"), *update)),
            )
        {
            let store = store().await;
            written_over(&store, written, update).await;
            let listed = store.mcps(Page::default()).await;
            assert!(
                matches!(
                    listed,
                    Err(StorageError::Malformed {
                        entity: Entity::Mcp,
                        ..
                    })
                ),
                "{update} was read back as {listed:?}"
            );
        }
    }

    /// A kind no credential of ours is says so, rather than reporting the pairing it also fails.
    #[tokio::test]
    async fn a_kind_of_credential_this_code_does_not_carry_names_itself() {
        let store = store().await;
        written_over(
            &store,
            server("warehouse", "Warehouse"),
            "UPDATE mcps SET auth_kind = 'mtls'",
        )
        .await;

        let error = store.mcps(Page::default()).await.unwrap_err();
        assert!(
            error.to_string().contains("MCP server"),
            "{error} must name what it was reading"
        );
        assert!(
            matches!(&error, StorageError::Malformed { detail, .. } if detail.contains("mtls")),
            "{error:?}"
        );
    }

    /// A single read rebuilds through the same types, so it refuses the same rows.
    #[tokio::test]
    async fn a_single_read_refuses_what_a_listing_refuses() {
        let store = store().await;
        written_over(
            &store,
            server("warehouse", "Warehouse"),
            "UPDATE mcps SET auth_token = 'left behind'",
        )
        .await;

        let read = store.mcp(&McpId::new("warehouse").unwrap()).await;
        assert!(
            matches!(
                read,
                Err(StorageError::Malformed {
                    entity: Entity::Mcp,
                    ..
                })
            ),
            "{read:?}"
        );
    }

    /// The one column that is neither text nor null, and the only value it holds that the
    /// timestamp type will not take. A second before the epoch is a time, and is not refused.
    #[tokio::test]
    async fn a_row_timed_past_what_a_timestamp_holds_is_refused_on_the_way_back() {
        let store = store().await;
        written_over(
            &store,
            server("warehouse", "Warehouse"),
            "UPDATE mcps SET created_at = 9223372036854775807",
        )
        .await;

        let read = store.mcp(&McpId::new("warehouse").unwrap()).await;
        assert!(matches!(read, Err(StorageError::Value(_))), "{read:?}");
    }
}
