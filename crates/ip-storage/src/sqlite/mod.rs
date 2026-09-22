mod identity;
mod list;
mod plugin;
mod revision;
mod route;

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use ip_core::{TenantId, UserId};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

use crate::error::StorageError;
use crate::model::{TenantRowId, UserRowId};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONNECTIONS: u32 = 8;

/// The standalone backend: one local sqlite file.
#[derive(Debug, Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Opens the database at `path`, creating and migrating it when it is not there yet.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let options = SqliteConnectOptions::new()
            .filename(path.as_ref())
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(BUSY_TIMEOUT);
        Self::connect(options, MAX_CONNECTIONS).await
    }

    /// Opens a private database that lives only as long as the store, for tests and dry runs.
    pub async fn in_memory() -> Result<Self, StorageError> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .map_err(StorageError::backend)?
            .foreign_keys(true);
        Self::connect(options, 1).await
    }

    async fn connect(
        options: SqliteConnectOptions,
        max_connections: u32,
    ) -> Result<Self, StorageError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(options)
            .await
            .map_err(StorageError::backend)?;
        sqlx::migrate!("./migrations/sqlite")
            .run(&pool)
            .await
            .map_err(StorageError::backend)?;
        Ok(Self { pool })
    }

    /// Closes every pooled connection.
    pub async fn close(&self) {
        self.pool.close().await;
    }

    async fn tenant_row_id(&self, id: &TenantId) -> Result<TenantRowId, StorageError> {
        identity::tenant_row_id(&mut *self.connection().await?, id).await
    }

    async fn user_row_id(&self, id: &UserId) -> Result<UserRowId, StorageError> {
        identity::user_row_id(&mut *self.connection().await?, id).await
    }

    /// One connection out of the pool, for the writes a transaction also runs.
    async fn connection(&self) -> Result<sqlx::pool::PoolConnection<sqlx::Sqlite>, StorageError> {
        self.pool.acquire().await.map_err(StorageError::backend)
    }
}

/// The store the sqlite only tests build from, and the records every backend test shares.
#[cfg(test)]
mod fixture {
    pub(crate) use crate::suite::fixture::*;

    use super::SqliteStore;

    pub(crate) async fn store() -> SqliteStore {
        SqliteStore::in_memory().await.unwrap()
    }
}

/// The shared backend tests, each against a private in-memory database.
#[cfg(test)]
mod suite {
    async fn open() -> Option<super::SqliteStore> {
        Some(super::SqliteStore::in_memory().await.unwrap())
    }

    crate::suite::backend_suite!(crate::sqlite::suite::open);
}
