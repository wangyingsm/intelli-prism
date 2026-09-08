use ip_config::ConfigError;
use ip_storage::StorageError;

/// Every way the routing table can fail to be built.
#[derive(Debug, thiserror::Error)]
pub enum RouteError {
    /// A configured upstream does not describe a usable rule.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// The stored rules could not be read.
    #[error(transparent)]
    Storage(#[from] StorageError),
}
