use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::Frame;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full};
use tokio::sync::mpsc;

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

/// A body fed frame by frame from a channel, so a task can stream a body it is still producing.
pub(crate) struct ChannelBody {
    receiver: mpsc::Receiver<Result<Frame<Bytes>, BoxError>>,
}

impl ChannelBody {
    /// Streams whatever the channel's sender produces, ending when the sender is dropped.
    pub(crate) fn new(receiver: mpsc::Receiver<Result<Frame<Bytes>, BoxError>>) -> Self {
        Self { receiver }
    }
}

impl http_body::Body for ChannelBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.receiver.poll_recv(cx)
    }
}
