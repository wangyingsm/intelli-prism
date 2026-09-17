use http::StatusCode;
use ip_cache::CacheError;
use ip_config::ConfigError;
use ip_core::{Capability, CoreError, Protocol, RouteKey};
use ip_storage::StorageError;

use crate::stage::StageName;

/// Every way the routing table can fail to be built.
#[derive(Debug, thiserror::Error)]
pub enum RouteError {
    /// A configured upstream does not describe a usable rule.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// The stored rules could not be read.
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// A rule claims a path the gateway keeps for its own endpoints.
    #[error(
        "{path} is inside {}, which the gateway keeps for itself",
        ip_core::RESERVED_PATH_PREFIX
    )]
    ReservedPath {
        /// The path the rule tried to claim.
        path: ip_core::AbsPath,
    },
}

/// A request that did not make it through the dataflow, and how far it got.
#[derive(Debug, thiserror::Error)]
#[error("{stage}: {kind}")]
pub struct GatewayError {
    /// Where the request stopped.
    stage: StageName,
    /// Why it stopped.
    kind: GatewayErrorKind,
}

impl GatewayError {
    /// Records a failure at a stage.
    pub fn new(stage: StageName, kind: GatewayErrorKind) -> Self {
        Self { stage, kind }
    }

    /// Where the request stopped.
    pub fn stage(&self) -> StageName {
        self.stage
    }

    /// Why it stopped.
    pub fn kind(&self) -> &GatewayErrorKind {
        &self.kind
    }

    /// What the caller may be told about why. Only a plugin's deliberate refusal supplies
    /// one; every other failure stays in the log.
    pub fn public_reason(&self) -> Option<&str> {
        match &self.kind {
            GatewayErrorKind::Processor(ProcessorError::Refused { reason }) => Some(reason),
            _ => None,
        }
    }

    /// The status the caller is told.
    pub fn status(&self) -> StatusCode {
        match self.kind {
            GatewayErrorKind::Unauthenticated => StatusCode::UNAUTHORIZED,
            GatewayErrorKind::Forbidden { .. } => StatusCode::FORBIDDEN,
            GatewayErrorKind::NoRoute { .. } => StatusCode::NOT_FOUND,
            GatewayErrorKind::ProtocolNotServed { .. } => StatusCode::NOT_IMPLEMENTED,
            GatewayErrorKind::Malformed { .. } | GatewayErrorKind::Value(_) => {
                StatusCode::BAD_REQUEST
            }
            GatewayErrorKind::Processor(ProcessorError::Refused { .. }) => StatusCode::FORBIDDEN,
            GatewayErrorKind::Processor(ProcessorError::Failed { .. }) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
            GatewayErrorKind::Upstream(_) => StatusCode::BAD_GATEWAY,
            GatewayErrorKind::Cache(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// Why a request did not make it through the dataflow.
#[derive(Debug, thiserror::Error)]
pub enum GatewayErrorKind {
    /// The caller never established who it is.
    #[error("the caller is not authenticated")]
    Unauthenticated,

    /// The caller is known but holds no such capability here.
    #[error("the caller does not hold {capability:?}")]
    Forbidden {
        /// What the caller would have needed.
        capability: Capability,
    },

    /// No rule carries this request.
    #[error("no route for {key}")]
    NoRoute {
        /// What was looked up.
        key: RouteKey,
    },

    /// A rule matched, but this build does not carry its protocol.
    #[error("{protocol} is not carried by this build")]
    ProtocolNotServed {
        /// The protocol the rule names.
        protocol: Protocol,
    },

    /// The request itself could not be read.
    #[error("{detail}")]
    Malformed {
        /// What was wrong with it.
        detail: String,
    },

    /// A value in the request failed its own validation.
    #[error(transparent)]
    Value(#[from] CoreError),

    /// A plugin refused the request.
    #[error(transparent)]
    Processor(#[from] ProcessorError),

    /// The upstream refused or never answered.
    #[error(transparent)]
    Upstream(#[from] UpstreamError),

    /// The cache could not be read or written.
    #[error(transparent)]
    Cache(#[from] CacheError),
}

/// A plugin stopping the request it was given.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProcessorError {
    /// The plugin failed. The caller learns only that the request could not be carried.
    #[error("{detail}")]
    Failed {
        /// What went wrong, for the log.
        detail: String,
    },
    /// The plugin refused the request on purpose, and its reason goes back to the caller.
    #[error("refused: {reason}")]
    Refused {
        /// Why the plugin refused, as the caller will read it.
        reason: String,
    },
}

impl ProcessorError {
    /// A plugin that failed.
    pub fn failed(detail: impl Into<String>) -> Self {
        Self::Failed {
            detail: detail.into(),
        }
    }

    /// A plugin that refused the request on purpose.
    pub fn refused(reason: impl Into<String>) -> Self {
        Self::Refused {
            reason: reason.into(),
        }
    }
}

/// The upstream refused or never answered.
#[derive(Debug, thiserror::Error)]
#[error("{detail}")]
pub struct UpstreamError {
    /// What went wrong reaching the upstream.
    pub detail: String,
}

impl UpstreamError {
    /// Reports a failure to reach the upstream.
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}
