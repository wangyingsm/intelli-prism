use std::sync::Arc;

use ip_auth::RequestVerifier;
use ip_config::{Config, StorageConfig};
use ip_storage::{SqliteStore, Storage};

use crate::error::StartupError;

/// What every handler is given.
#[derive(Clone)]
pub struct AppState {
    verifier: RequestVerifier,
}

impl AppState {
    /// Opens the configured backend and builds the state around it.
    pub async fn open(config: &Config) -> Result<Self, StartupError> {
        let store = open_store(&config.storage).await?;
        Ok(Self {
            verifier: RequestVerifier::new(store),
        })
    }

    /// Builds state over a store that is already open.
    #[cfg(test)]
    pub fn with_store(store: Arc<dyn Storage>) -> Self {
        Self {
            verifier: RequestVerifier::new(store),
        }
    }

    /// Checks request signatures against the store.
    pub fn verifier(&self) -> &RequestVerifier {
        &self.verifier
    }
}

async fn open_store(config: &StorageConfig) -> Result<Arc<dyn Storage>, StartupError> {
    match config {
        StorageConfig::Sqlite { path } => Ok(Arc::new(SqliteStore::open(path).await?)),
        StorageConfig::Postgres { .. } => Err(StartupError::UnsupportedBackend {
            backend: "postgres",
        }),
    }
}
