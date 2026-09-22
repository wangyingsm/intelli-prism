use std::io;
use std::net::SocketAddr;

use ip_cache::CacheError;
use ip_config::ConfigError;
use ip_gateway::{RouteError, UpstreamError};
use ip_plugin::{ChainError, PluginError};
use ip_storage::StorageError;

/// Every way the server can fail before it is listening.
#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    /// The command line did not say what to do.
    #[error("{detail}")]
    Usage {
        /// What was wrong with it.
        detail: String,
    },

    /// A passphrase or an identity was refused.
    #[error(transparent)]
    Auth(#[from] ip_auth::AuthError),

    /// The configuration file could not be loaded.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// The storage backend could not be opened.
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// The cache backend could not be opened.
    #[error(transparent)]
    Cache(#[from] CacheError),

    /// The routing table could not be built.
    #[error(transparent)]
    Routing(#[from] RouteError),

    /// The upstream client could not be built.
    #[error(transparent)]
    Upstream(#[from] UpstreamError),

    /// The plugin engine could not be started.
    #[error(transparent)]
    Plugin(#[from] PluginError),

    /// The plugin chains could not be built from the stored rules.
    #[error(transparent)]
    Chains(#[from] ChainError),

    /// Publishing the rules could not be set up.
    #[error(transparent)]
    Feed(#[from] FeedError),

    /// The configured backend is not compiled into this build.
    #[error("this build has no {backend} backend; rebuild with the `{feature}` feature")]
    UnsupportedBackend {
        /// The backend the configuration asked for.
        backend: &'static str,
        /// The cargo feature that compiles it in.
        feature: &'static str,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_backend_names_the_feature_that_brings_it() {
        let error = StartupError::UnsupportedBackend {
            backend: "postgres",
            feature: "fast-storage",
        };
        assert_eq!(
            error.to_string(),
            "this build has no postgres backend; rebuild with the `fast-storage` feature"
        );
    }
}

/// Every way publishing the rules for the other nodes can fail.
#[derive(Debug, thiserror::Error)]
pub enum FeedError {
    /// Storage could not say what the rules are.
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// The cache could not carry them.
    #[error(transparent)]
    Cache(#[from] CacheError),

    /// The rules could not be written out for the cache.
    #[error("the rules could not be encoded: {0}")]
    Encode(#[source] serde_json::Error),

    /// What the cache carries could not be read back as rules.
    #[error("the published rules could not be decoded: {0}")]
    Decode(#[source] serde_json::Error),

    /// A published rule is one its own type refuses.
    #[error(transparent)]
    Rule(#[from] ip_core::CoreError),
}
