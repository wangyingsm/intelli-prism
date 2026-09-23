//! What every span of one request carries, and the span each stage opens.

use ip_core::{ApiId, TenantId, TraceId, TurnId, UserId};

use super::RequestContext;

/// What every span of one request is read by.
pub(super) struct Followed {
    pub(super) trace: TraceId,
    pub(super) turn: Option<TurnId>,
    pub(super) tenant: TenantId,
    pub(super) user: UserId,
    pub(super) api: Option<ApiId>,
}

impl Followed {
    /// What the request arrived with, before its route is known.
    pub(super) fn of(context: &RequestContext) -> Self {
        let identity = context.authority.identity();
        Self {
            trace: context.trace,
            turn: context.turn.clone(),
            tenant: identity.tenant.clone(),
            user: identity.user.clone(),
            api: None,
        }
    }

    /// The same, once the route says which api is serving.
    pub(super) fn serving(self, api: ApiId) -> Self {
        Self {
            api: Some(api),
            ..self
        }
    }
}

/// The stages `DESIGN.md` names, which are the spans one request opens.
pub(super) const INGRESS_REQUEST: &str = "ingress.request";
pub(super) const EGRESS_REQUEST: &str = "egress.request";
pub(super) const INGRESS_RESPONSE: &str = "ingress.response";
pub(super) const EGRESS_RESPONSE: &str = "egress.response";

/// One stage's span, carrying what every trace is read by. `api` is empty until the route is
/// resolved, and `hit` until the answer is known to have come from the cache or not.
macro_rules! stage_span {
    ($name:expr, $followed:expr) => {
        tracing::info_span!(
            $name,
            trace = %$followed.trace,
            turn = $followed.turn.as_ref().map(tracing::field::display),
            tenant = %$followed.tenant,
            user = %$followed.user,
            api = $followed.api.as_ref().map(tracing::field::display),
            hit = tracing::field::Empty,
        )
    };
}

pub(super) use stage_span;
