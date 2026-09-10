use std::fmt;

use http::{Request, Response};

use crate::body::GatewayBody;
use crate::table::Resolution;

/// Where in the dataflow a request had got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StageName {
    /// The kernel handed the connection over.
    Receive,
    /// The caller's signature was checked.
    Authentication,
    /// The request headers were read.
    HeaderRead,
    /// The request header plugin chain ran.
    HeaderProcess,
    /// The caller's capabilities were checked.
    Authorization,
    /// The request body was read.
    BodyRead,
    /// The request body plugin chain ran.
    BodyProcess,
    /// The request was sent upstream.
    Route,
    /// The response headers were read.
    ResponseHeaderRead,
    /// The response header plugin chain ran.
    ResponseHeaderProcess,
    /// The response body was read.
    ResponseBodyRead,
    /// The response body plugin chain ran.
    ResponseBodyProcess,
    /// The response went back to the kernel.
    Send,
}

impl StageName {
    /// The stage's name, as it appears in logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Receive => "receive",
            Self::Authentication => "authentication",
            Self::HeaderRead => "header read",
            Self::HeaderProcess => "header process",
            Self::Authorization => "authorization",
            Self::BodyRead => "body read",
            Self::BodyProcess => "body process",
            Self::Route => "route",
            Self::ResponseHeaderRead => "response header read",
            Self::ResponseHeaderProcess => "response header process",
            Self::ResponseBodyRead => "response body read",
            Self::ResponseBodyProcess => "response body process",
            Self::Send => "send",
        }
    }
}

impl fmt::Display for StageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One stage of the dataflow, as a type.
///
/// A `Flow` is generic over this, and a transition is only implemented on the stage it
/// leaves, so the order of `DESIGN.md` is what the compiler checks rather than what the
/// call site remembers.
pub trait Stage {
    /// What the flow holds while it is at this stage.
    type Held;

    /// The stage's name, for errors and logs.
    const NAME: StageName;
}

/// A request and the rule that carries it.
pub struct Routed {
    /// The request as it stands.
    pub request: Request<GatewayBody>,
    /// Where it is going, and the rule that said so.
    pub resolution: Resolution,
}

/// The request has arrived and nothing has touched it.
pub struct Received;

impl Stage for Received {
    type Held = Request<GatewayBody>;
    const NAME: StageName = StageName::Receive;
}

/// The request header chain has run.
pub struct HeadersProcessed;

impl Stage for HeadersProcessed {
    type Held = Request<GatewayBody>;
    const NAME: StageName = StageName::HeaderProcess;
}

/// The caller holds the capability this route needs, and the route is resolved.
pub struct Authorized;

impl Stage for Authorized {
    type Held = Routed;
    const NAME: StageName = StageName::Authorization;
}

/// The request body chain has run, or was skipped because there is none.
pub struct BodyProcessed;

impl Stage for BodyProcessed {
    type Held = Routed;
    const NAME: StageName = StageName::BodyProcess;
}

/// The upstream answered.
pub struct Forwarded;

impl Stage for Forwarded {
    type Held = Response<GatewayBody>;
    const NAME: StageName = StageName::Route;
}

/// The response header chain has run.
pub struct ResponseHeadersProcessed;

impl Stage for ResponseHeadersProcessed {
    type Held = Response<GatewayBody>;
    const NAME: StageName = StageName::ResponseHeaderProcess;
}

/// The response body chain has run, or was skipped because there is none.
pub struct ResponseBodyProcessed;

impl Stage for ResponseBodyProcessed {
    type Held = Response<GatewayBody>;
    const NAME: StageName = StageName::ResponseBodyProcess;
}
