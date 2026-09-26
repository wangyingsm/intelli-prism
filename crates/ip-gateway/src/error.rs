use http::StatusCode;
use ip_cache::CacheError;
use ip_config::ConfigError;
use ip_core::{Allowance, Capability, CoreError, Counted, Period, Protocol, RouteKey, Timestamp};
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

    /// What the caller may be told about why. A plugin's deliberate refusal supplies one, and
    /// so does a limit the caller itself holds; every other failure stays in the log.
    pub fn public_reason(&self) -> Option<String> {
        match &self.kind {
            GatewayErrorKind::Processor(ProcessorError::Refused { reason }) => Some(reason.clone()),
            GatewayErrorKind::Spent {
                counted,
                period,
                allowance,
                ..
            } => Some(format!(
                "{} per {} is spent: {allowance} allowed",
                counted.name(),
                period.name()
            )),
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
            GatewayErrorKind::Spent { .. } => StatusCode::TOO_MANY_REQUESTS,
            GatewayErrorKind::Unmeasured { .. } => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    /// How many seconds the caller waits before what it ran out of is allowed again.
    pub fn retry_after(&self) -> Option<u64> {
        let GatewayErrorKind::Spent { ends, .. } = &self.kind else {
            return None;
        };
        let left = ends.unix_seconds() - Timestamp::now().unix_seconds();
        Some(u64::try_from(left).unwrap_or(0).max(1))
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

    /// A limit on what the caller may spend is used up for this stretch.
    #[error("{} per {} is spent, and allows {allowance} until {ends}", counted.name(), period.name())]
    Spent {
        /// What ran out.
        counted: Counted,
        /// Over what stretch.
        period: Period,
        /// How much that stretch allows.
        allowance: Allowance,
        /// When the next stretch begins.
        ends: Timestamp,
    },

    /// A quota could not be read, so the request is refused rather than spent unmeasured.
    #[error("what this request would spend could not be counted: {detail}")]
    Unmeasured {
        /// Why it could not be.
        detail: String,
    },
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

#[cfg(test)]
mod tests {
    use ip_core::{AbsPath, Endpoint, Host, Port};

    use super::*;

    fn status_of(kind: GatewayErrorKind) -> StatusCode {
        GatewayError::new(StageName::Route, kind).status()
    }

    fn key() -> RouteKey {
        RouteKey::new(Endpoint::new(
            Protocol::Http,
            Host::new("gateway.local").unwrap(),
            Port::new(8080).unwrap(),
            AbsPath::new("/nowhere").unwrap(),
        ))
    }

    #[test]
    fn every_kind_answers_with_the_status_it_is_meant_to() {
        let cases = [
            (GatewayErrorKind::Unauthenticated, StatusCode::UNAUTHORIZED),
            (
                GatewayErrorKind::Forbidden {
                    capability: Capability::ApiAccess,
                },
                StatusCode::FORBIDDEN,
            ),
            (
                GatewayErrorKind::NoRoute { key: key() },
                StatusCode::NOT_FOUND,
            ),
            (
                GatewayErrorKind::ProtocolNotServed {
                    protocol: Protocol::Ws,
                },
                StatusCode::NOT_IMPLEMENTED,
            ),
            (
                GatewayErrorKind::Malformed {
                    detail: "no host".to_owned(),
                },
                StatusCode::BAD_REQUEST,
            ),
            (
                GatewayErrorKind::Value(CoreError::ZeroPort),
                StatusCode::BAD_REQUEST,
            ),
            (
                GatewayErrorKind::Processor(ProcessorError::Refused {
                    reason: "no".to_owned(),
                }),
                StatusCode::FORBIDDEN,
            ),
            (
                GatewayErrorKind::Processor(ProcessorError::failed("trapped")),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                GatewayErrorKind::Upstream(UpstreamError::new("refused")),
                StatusCode::BAD_GATEWAY,
            ),
            (
                GatewayErrorKind::Cache(CacheError::ZeroTtl),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (kind, status) in cases {
            let described = kind.to_string();
            assert_eq!(status_of(kind), status, "{described}");
        }
    }

    #[test]
    fn only_a_deliberate_refusal_tells_the_caller_why() {
        let refused = GatewayError::new(
            StageName::BodyProcess,
            GatewayErrorKind::Processor(ProcessorError::Refused {
                reason: "too long".to_owned(),
            }),
        );
        assert_eq!(refused.public_reason().as_deref(), Some("too long"));
        let failed = GatewayError::new(
            StageName::BodyProcess,
            GatewayErrorKind::Processor(ProcessorError::failed("trapped")),
        );
        assert_eq!(failed.public_reason(), None);
    }
}
