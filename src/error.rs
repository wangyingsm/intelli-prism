use std::io;
use std::net::SocketAddr;

use ip_config::ConfigError;
use ip_gateway::{RouteError, UpstreamError};
use ip_storage::StorageError;

/// Every way the server can fail before it is listening.
#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    /// The configuration file could not be loaded.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// The storage backend could not be opened.
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// The routing table could not be built.
    #[error(transparent)]
    Routing(#[from] RouteError),

    /// The upstream client could not be built.
    #[error(transparent)]
    Upstream(#[from] UpstreamError),

    /// The configured backend is not compiled into this build.
    #[error("this build has no {backend} backend; rebuild with its feature enabled")]
    UnsupportedBackend {
        /// The backend the configuration asked for.
        backend: &'static str,
    },

    /// The listener could not take the configured address.
    #[error("cannot listen on {address}")]
    Bind {
        /// The address that was refused.
        address: SocketAddr,
        #[source]
        source: io::Error,
    },

    /// The server stopped with an error.
    #[error("server failed")]
    Serve(#[source] io::Error),
}
