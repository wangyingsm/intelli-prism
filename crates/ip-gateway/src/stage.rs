use std::fmt;

use http::{Request, Response};

use ip_core::{Decider, Difficulty, Hedge, Replica};

use crate::body::GatewayBody;
use crate::filter::Filtered;
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
    /// The difficulty class the request named, which narrows the dispatch.
    ///
    /// Read once the request header chain has run — a processor that sets the header is
    /// honoured, which is how an operator plugs in a classifier of their own — and before
    /// any limit is counted, so a value the gateway cannot read is refused unbilled.
    pub class: Option<Difficulty>,
    /// What the filter took out of the body, which the answer reports and the ledger
    /// counts. Settled at the body stage and carried from there, because the filter runs
    /// where the body is in hand and the answer is built two stages later.
    pub filtered: Filtered,
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
    type Held = Routed;
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

/// What an upstream answered, and which endpoint gave it.
pub struct Answered {
    /// The answer itself.
    pub response: Response<GatewayBody>,
    /// The endpoint the request was dispatched to.
    pub served: Replica,
    /// How many endpoints the request was raced across; one is what happened when it was not
    /// raced at all.
    pub raced: Hedge,
    /// What it was dispatched at and which level of the margin said so. The record's
    /// `dispatched_class` and `decided_by`, and the reason the context speaks for the
    /// class after forward rather than the header reading alone.
    pub dispatched: Dispatched,
}

/// What a request was dispatched at, and which level of the margin named it.
///
/// The two are one value because they are settled together, at the single point of
/// precedence, and a row carrying a class no level named — or a level that named no
/// class — would be a row nobody can count. Held so that such a pair cannot be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dispatched(Option<(Difficulty, Decider)>);

impl Dispatched {
    /// Nothing named a class, which is how every request went before any of this.
    pub const NONE: Self = Self(None);

    /// The class `decided_by` named.
    #[must_use]
    pub const fn at(class: Difficulty, decided_by: Decider) -> Self {
        Self(Some((class, decided_by)))
    }

    /// The class the request was dispatched at, when one was named.
    #[must_use]
    pub const fn class(self) -> Option<Difficulty> {
        match self.0 {
            Some((class, _)) => Some(class),
            None => None,
        }
    }

    /// The level that named it, when one did.
    #[must_use]
    pub const fn decided_by(self) -> Option<Decider> {
        match self.0 {
            Some((_, decided_by)) => Some(decided_by),
            None => None,
        }
    }
}

/// The upstream answered.
pub struct Forwarded;

impl Stage for Forwarded {
    type Held = Answered;
    const NAME: StageName = StageName::Route;
}

/// The response header chain has run.
pub struct ResponseHeadersProcessed;

impl Stage for ResponseHeadersProcessed {
    type Held = Answered;
    const NAME: StageName = StageName::ResponseHeaderProcess;
}

/// The response body chain has run, or was skipped because there is none.
pub struct ResponseBodyProcessed;

impl Stage for ResponseBodyProcessed {
    type Held = Answered;
    const NAME: StageName = StageName::ResponseBodyProcess;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_stage_names_itself() {
        for (stage, name) in [
            (StageName::Receive, "receive"),
            (StageName::Authentication, "authentication"),
            (StageName::HeaderRead, "header read"),
            (StageName::HeaderProcess, "header process"),
            (StageName::Authorization, "authorization"),
            (StageName::BodyRead, "body read"),
            (StageName::BodyProcess, "body process"),
            (StageName::Route, "route"),
            (StageName::ResponseHeaderRead, "response header read"),
            (StageName::ResponseHeaderProcess, "response header process"),
            (StageName::ResponseBodyRead, "response body read"),
            (StageName::ResponseBodyProcess, "response body process"),
            (StageName::Send, "send"),
        ] {
            assert_eq!(stage.as_str(), name);
            assert_eq!(stage.to_string(), name);
        }
    }

    #[test]
    fn each_stage_type_carries_its_own_name() {
        assert_eq!(Received::NAME, StageName::Receive);
        assert_eq!(HeadersProcessed::NAME, StageName::HeaderProcess);
        assert_eq!(Authorized::NAME, StageName::Authorization);
        assert_eq!(BodyProcessed::NAME, StageName::BodyProcess);
        assert_eq!(Forwarded::NAME, StageName::Route);
        assert_eq!(
            ResponseHeadersProcessed::NAME,
            StageName::ResponseHeaderProcess
        );
        assert_eq!(ResponseBodyProcessed::NAME, StageName::ResponseBodyProcess);
    }
}
