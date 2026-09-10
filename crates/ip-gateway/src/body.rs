use bytes::Bytes;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full};

/// Any error a body stream can fail with.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The body type carried through the dataflow, streamed rather than buffered.
///
/// Unsync because a hyper or axum body is `Send` but not `Sync`, and a body is only
/// ever owned by the one task carrying its request.
pub type GatewayBody = UnsyncBoxBody<Bytes, BoxError>;

/// Wraps bytes already in hand as a body.
pub fn from_bytes(bytes: Bytes) -> GatewayBody {
    Full::new(bytes)
        .map_err(|never| match never {})
        .boxed_unsync()
}

/// An empty body.
pub fn empty() -> GatewayBody {
    from_bytes(Bytes::new())
}
