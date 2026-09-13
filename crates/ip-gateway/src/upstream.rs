use async_trait::async_trait;
use http::{Request, Response};

use crate::body::GatewayBody;
use crate::error::UpstreamError;

/// Carries a request to the upstream it was routed to.
#[async_trait]
pub trait Upstream: Send + Sync {
    /// Sends the request on and hands back what came off the wire.
    async fn send(
        &self,
        request: Request<GatewayBody>,
    ) -> Result<Response<GatewayBody>, UpstreamError>;
}
