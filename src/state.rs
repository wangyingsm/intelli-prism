use std::net::SocketAddr;
use std::sync::Arc;

use ip_auth::RequestVerifier;
use ip_config::{Config, StorageConfig};
use ip_gateway::{Gateway, HyperUpstream, ProcessorChain, RoutingTable};
use ip_storage::{SqliteStore, Storage};

use crate::error::StartupError;

/// What every handler is given.
#[derive(Clone)]
pub struct AppState {
    verifier: RequestVerifier,
    gateway: Arc<Gateway>,
    listen: SocketAddr,
}

impl AppState {
    /// Opens the configured backend, builds the routing table and the upstream client.
    pub async fn open(config: &Config) -> Result<Self, StartupError> {
        let store = open_store(&config.storage).await?;
        let table = RoutingTable::load(config, store.as_ref()).await?;
        let upstream = Arc::new(HyperUpstream::new()?);
        Ok(Self {
            verifier: RequestVerifier::new(store as Arc<dyn Storage>),
            gateway: Arc::new(Gateway::new(table, ProcessorChain::new(), upstream)),
            listen: config.server.listen,
        })
    }

    /// Builds state over a store and gateway that are already built.
    #[cfg(test)]
    pub fn with_parts(store: Arc<dyn Storage>, gateway: Gateway, listen: SocketAddr) -> Self {
        Self {
            verifier: RequestVerifier::new(store),
            gateway: Arc::new(gateway),
            listen,
        }
    }

    /// Checks request signatures against the store.
    pub fn verifier(&self) -> &RequestVerifier {
        &self.verifier
    }

    /// Carries proxied requests through the dataflow.
    pub fn gateway(&self) -> &Gateway {
        &self.gateway
    }

    /// Where the gateway listens, supplying the port a `Host` header omits.
    pub fn listen(&self) -> SocketAddr {
        self.listen
    }
}

async fn open_store(config: &StorageConfig) -> Result<Arc<SqliteStore>, StartupError> {
    match config {
        StorageConfig::Sqlite { path } => Ok(Arc::new(SqliteStore::open(path).await?)),
        StorageConfig::Postgres { .. } => Err(StartupError::UnsupportedBackend {
            backend: "postgres",
        }),
    }
}

#[cfg(test)]
mod tests {
    use ip_config::Config;

    use super::*;

    fn config(storage: &str) -> Config {
        Config::parse(&format!(
            r#"
[server]
listen = "127.0.0.1:8080"

{storage}

[auth.jwt]
issuer = "intelli-prism"
secret = "0123456789abcdef0123456789abcdef"
"#
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn a_backend_this_build_does_not_carry_stops_startup() {
        let config = config("[storage]\nbackend = \"postgres\"\nurl = \"postgres://ip@db/ip\"");
        assert!(matches!(
            AppState::open(&config).await,
            Err(StartupError::UnsupportedBackend {
                backend: "postgres"
            })
        ));
    }

    #[tokio::test]
    async fn opening_the_sqlite_backend_builds_a_routing_table() {
        let path = std::env::temp_dir().join(format!("ip-state-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let config = config(&format!(
            "[storage]\nbackend = \"sqlite\"\npath = {:?}",
            path.display().to_string()
        ));
        let state = AppState::open(&config).await.unwrap();
        assert_eq!(state.listen(), config.server.listen);
        assert!(state.gateway().table().is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
