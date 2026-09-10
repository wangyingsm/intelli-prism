use http::StatusCode;
use ip_config::ConfigError;
use ip_core::{Capability, CoreError, Protocol, RouteKey};
use ip_storage::StorageError;

use crate::processor::ProcessorError;
use crate::stage::Stage;
use crate::upstream::UpstreamError;

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

/// A request that did not make it through the dataflow, and how far it got.
#[derive(Debug, thiserror::Error)]
#[error("{stage}: {kind}")]
pub struct GatewayError {
    /// Where the request stopped.
    stage: Stage,
    /// Why it stopped.
    kind: GatewayErrorKind,
}

impl GatewayError {
    /// Records a failure at a stage.
    pub fn new(stage: Stage, kind: GatewayErrorKind) -> Self {
        Self { stage, kind }
    }

    /// Where the request stopped.
    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// Why it stopped.
    pub fn kind(&self) -> &GatewayErrorKind {
        &self.kind
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
            GatewayErrorKind::Processor(_) => StatusCode::INTERNAL_SERVER_ERROR,
            GatewayErrorKind::Upstream(_) => StatusCode::BAD_GATEWAY,
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
}
