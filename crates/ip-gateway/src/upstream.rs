use async_trait::async_trait;
use http::{Request, Response};

use crate::body::GatewayBody;

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

/// Carries a request to the upstream it was routed to.
#[async_trait]
pub trait Upstream: Send + Sync {
    /// Sends the request on and hands back what came off the wire.
    async fn send(
        &self,
        request: Request<GatewayBody>,
    ) -> Result<Response<GatewayBody>, UpstreamError>;
}
