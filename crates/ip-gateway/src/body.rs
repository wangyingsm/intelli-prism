use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};

/// Any error a body stream can fail with.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The body type carried through the dataflow, streamed rather than buffered.
pub type GatewayBody = BoxBody<Bytes, BoxError>;

/// Wraps bytes already in hand as a body.
pub fn from_bytes(bytes: Bytes) -> GatewayBody {
    Full::new(bytes).map_err(|never| match never {}).boxed()
}

/// An empty body.
pub fn empty() -> GatewayBody {
    from_bytes(Bytes::new())
}
