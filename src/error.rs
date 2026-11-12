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

    /// The classifier's artefact could not be got, or is not the one it claims.
    #[cfg(feature = "classifier")]
    #[error("{detail}")]
    Artefact {
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

    /// The configured graph store could not be named, although the configuration took it.
    ///
    /// A startup failure rather than a graph refusal: `ip-config` and `ip-drsg` apply the same
    /// rules to the same three values, written out twice because the config crate cannot depend
    /// on the client's newtypes, and this is where the two disagreeing shows up.
    #[error(transparent)]
    GraphStore(#[from] ip_drsg::DrsgError),

    /// The configured backend is not compiled into this build.
    #[allow(
        dead_code,
        reason = "every construction site is behind a `cfg(not(feature))`, so a build \
                  carrying every backend constructs it nowhere"
    )]
    #[error("this build has no {backend} backend; rebuild with the `{feature}` feature")]
    UnsupportedBackend {
        /// The backend the configuration asked for.
        backend: &'static str,
        /// The cargo feature that compiles it in.
        feature: &'static str,
    },

    /// The difficulty classifier could not be opened — an artefact that is not there,
    /// a runtime that will not load, a taxonomy that does not match. Startup is the
    /// only honest place to say so: a gateway configured to classify that cannot must
    /// not stand up and serve unclassified. The classifier build feature is the door.
    #[cfg(feature = "classifier")]
    #[error("the difficulty classifier cannot open: {reason}")]
    Classifier {
        /// What the loader refused with.
        reason: String,
    },

    /// The embedding model could not be opened — an artefact that is not there, a
    /// runtime that will not load, a graph whose answer is not the shape its manifest
    /// promised. Startup is the only honest place to say so: a gateway configured to
    /// answer by meaning that cannot must not stand up with the level silently off,
    /// because a level that never hits looks exactly like one nobody's traffic suits.
    #[cfg(feature = "semantic")]
    #[error("the embedding model cannot open: {reason}")]
    Embedder {
        /// What the loader refused with.
        reason: String,
    },

    /// A deployment advertises a cluster address but has no secret for its peers to present.
    ///
    /// A refusal rather than a warning, because the alternative is a machine its own cluster can
    /// reach and cannot authenticate to: every node dispatched to it would be refused, and every
    /// graph run would quietly fall back to the one machine that read the graph. The other way
    /// round is not an error — a member can be dispatched *to* without dispatching.
    #[error(
        "`[cluster] advertise` names an address but `[cluster] secret` is unset: \
         a member of a cluster needs the secret its peers present"
    )]
    ClusterWithoutSecret,

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

    /// The routing table could not be built from the published rules.
    #[error(transparent)]
    Table(#[from] RouteError),

    /// The plugin chains could not be built from the published rules.
    #[error(transparent)]
    Chains(#[from] ChainError),

    /// A published rule is one its own type refuses.
    #[error(transparent)]
    Rule(#[from] ip_core::CoreError),
}
