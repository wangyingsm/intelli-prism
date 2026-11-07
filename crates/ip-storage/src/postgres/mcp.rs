use async_trait::async_trait;
use ip_core::Timestamp;
use ip_mcp::{McpEndpoint, McpId, McpName, McpServer, McpToken};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};

use super::PostgresStore;
use crate::audit::{By, NewAudit};
use crate::codec::{mcp_auth, mcp_refused};
use crate::error::{Entity, StorageError};
use crate::list::Page;
use crate::mcp::{McpStore, StoredMcp};

#[async_trait]
impl McpStore for PostgresStore {
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
             WHERE created_at > $1 ORDER BY created_at DESC, row_id DESC LIMIT $2 OFFSET $3",
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
    connection: &mut PgConnection,
    server: McpServer,
    by: &By,
) -> Result<StoredMcp, StorageError> {
    let before = read_mcp(connection, &server.id).await?;
    // Removed and written rather than updated in place, so a replacement carries no column of
    // what it replaced -- a credential dropped for none would otherwise be left behind.
    sqlx::query("DELETE FROM mcps WHERE mcp_id = $1")
        .bind(server.id.as_str())
        .execute(&mut *connection)
        .await
        .map_err(StorageError::backend)?;

    let created_at = Timestamp::now();
    sqlx::query(
        "INSERT INTO mcps (mcp_id, name, endpoint, auth_kind, auth_token, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6)",
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
async fn removing(connection: &mut PgConnection, id: &McpId, by: &By) -> Result<(), StorageError> {
    let before = read_mcp(connection, id).await?;
    let removed = sqlx::query("DELETE FROM mcps WHERE mcp_id = $1")
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
    connection: &mut PgConnection,
    id: &McpId,
) -> Result<Option<StoredMcp>, StorageError> {
    let row = sqlx::query(
        "SELECT mcp_id, name, endpoint, auth_kind, auth_token, created_at FROM mcps \
         WHERE mcp_id = $1",
    )
    .bind(id.as_str())
    .fetch_optional(&mut *connection)
    .await
    .map_err(StorageError::backend)?;
    row.map(stored).transpose()
}

/// Rebuilds a server, refusing a row whose stored value its own type will not take.
pub(super) fn stored(row: PgRow) -> Result<StoredMcp, StorageError> {
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
