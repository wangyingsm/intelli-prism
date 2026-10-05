//! The typestate machine: a request as it moves from one stage to the next.

use futures_util::stream::{FuturesUnordered, StreamExt};
use http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use http::{HeaderName, Request, Response};
use ip_cache::{CacheKey, Ttl};
use ip_core::{
    Capability, CapabilityScope, Difficulty, HEDGE_MOST, Hedge, Replica, RouteKey, TurnId,
};
use std::sync::Arc;
use std::time::SystemTime;
use tracing::{Span, field};

use super::RequestContext;
use super::headers::{aim, authority_of, is_event_stream, rewrite, set_length, strip_hop_by_hop};
use super::run::{collect, run_body, run_headers};
use crate::body::{GatewayBody, from_bytes};
use crate::cache::{CachedResponse, Freshness, freshness, response_key};
use crate::dispatch::{Dispatcher, Dispatching, may_hold, may_race};
use crate::error::{GatewayError, GatewayErrorKind};
use crate::processor::{HeaderProcessor, ProcessorChain};
use crate::sse::stream_chunks;
use crate::stage::{
    Answered, Authorized, BodyProcessed, Forwarded, HeadersProcessed, Received,
    ResponseBodyProcessed, ResponseHeadersProcessed, Routed, Stage, StageName,
};
use crate::table::{Resolution, RoutingTable, request_key};
use crate::turns::{Opening, TurnAffinity, Verdict};
use crate::upstream::Upstream;

/// The request header naming the difficulty class the caller asks to be served at.
const DIFFICULTY: HeaderName = HeaderName::from_static("x-ip-difficulty");

/// A request part way through the dataflow, with the stage it reached in its type.
///
/// Each transition is implemented only on the stage before it, so a step cannot be
/// skipped or reordered: the call that would do so does not compile.
///
/// Stages run in order:
///
/// ```
/// # use ip_gateway::{Flow, ProcessorChain, RoutingTable, RequestContext};
/// # use ip_gateway::body::empty;
/// # use ip_auth::{Authority, Identity};
/// # use ip_core::{Grants, Protocol, Role, TenantId, TraceId, UserId};
/// # use http::Request;
/// # fn context() -> RequestContext {
/// #     RequestContext {
/// #         authority: Authority::new(
/// #             Identity {
/// #                 user: UserId::new("alice").unwrap(),
/// #                 tenant: TenantId::new("acme").unwrap(),
/// #                 role: Role::Member,
/// #             },
/// #             Grants::new(),
/// #         ),
/// #         protocol: Protocol::Http,
/// #         listen: "127.0.0.1:8080".parse().unwrap(),
/// #         trace: TraceId::generate().unwrap(),
/// #         turn: None,
/// #     }
/// # }
/// # async fn run(table: &RoutingTable, chain: &ProcessorChain) {
/// let request = Request::builder().uri("/x").body(empty()).unwrap();
/// let flow = Flow::received(context(), request);
/// let flow = flow.authorize(table).unwrap();
/// let flow = flow.process_headers(chain.request_headers()).await.unwrap();
/// # }
/// ```
///
/// Running a plugin before authorization does not compile:
///
/// ```compile_fail
/// # use ip_gateway::{Flow, ProcessorChain, RoutingTable, RequestContext};
/// # use ip_gateway::body::empty;
/// # use ip_auth::{Authority, Identity};
/// # use ip_core::{Grants, Protocol, Role, TenantId, TraceId, UserId};
/// # use http::Request;
/// # fn context() -> RequestContext {
/// #     RequestContext {
/// #         authority: Authority::new(
/// #             Identity {
/// #                 user: UserId::new("alice").unwrap(),
/// #                 tenant: TenantId::new("acme").unwrap(),
/// #                 role: Role::Member,
/// #             },
/// #             Grants::new(),
/// #         ),
/// #         protocol: Protocol::Http,
/// #         listen: "127.0.0.1:8080".parse().unwrap(),
/// #         trace: TraceId::generate().unwrap(),
/// #         turn: None,
/// #     }
/// # }
/// # async fn run(table: &RoutingTable, chain: &ProcessorChain) {
/// let request = Request::builder().uri("/x").body(empty()).unwrap();
/// let flow = Flow::received(context(), request);
/// // process_headers belongs to Flow<Authorized>, so no plugin runs before authorization.
/// let flow = flow.process_headers(chain.request_headers()).await.unwrap();
/// # }
/// ```
pub struct Flow<S: Stage> {
    context: RequestContext,
    held: S::Held,
}

impl<S: Stage> Flow<S> {
    /// Who the caller is, and where the request arrived.
    pub fn context(&self) -> &RequestContext {
        &self.context
    }

    /// The stage this flow has reached.
    pub fn stage(&self) -> StageName {
        S::NAME
    }
}

impl Flow<Received> {
    /// Takes a request that has been authenticated but not yet processed.
    pub fn received(context: RequestContext, request: Request<GatewayBody>) -> Self {
        Self {
            context,
            held: request,
        }
    }

    /// Resolves the route and checks the caller may use it, before any plugin runs.
    pub fn authorize(self, table: &RoutingTable) -> Result<Flow<Authorized>, GatewayError> {
        let key = key_of(&self.context, &self.held)?;
        let resolution = resolve(table, &key)?;
        allow(&self.context, &resolution)?;
        Ok(Flow {
            context: self.context,
            held: Routed {
                request: self.held,
                resolution,
                class: None,
            },
        })
    }
}

impl Flow<Authorized> {
    /// Where the request is going, and the rule that sent it there.
    pub fn resolution(&self) -> &Resolution {
        &self.held.resolution
    }

    /// Runs the request header chain, then reads the difficulty class the request names.
    ///
    /// The read follows the chain on purpose: a header processor that sets
    /// `x-ip-difficulty` is honoured, which is how an operator plugs in a classifier of
    /// their own before this gateway grows one. It also sits before the admission check, so
    /// a request carrying a class the gateway cannot read is refused without having been
    /// counted against any limit.
    pub async fn process_headers(
        mut self,
        chain: &[Arc<dyn HeaderProcessor>],
    ) -> Result<Flow<HeadersProcessed>, GatewayError> {
        run_headers(
            HeadersProcessed::NAME,
            chain,
            self.held.request.headers_mut(),
        )
        .await?;
        let class = asked_class(&self.held.request)?;
        self.held.class = class;
        Ok(Flow {
            context: self.context,
            held: self.held,
        })
    }
}

impl Flow<HeadersProcessed> {
    /// Runs the request body chain, or leaves the body untouched when there is none.
    pub async fn process_body(
        mut self,
        processors: &ProcessorChain,
    ) -> Result<Flow<BodyProcessed>, GatewayError> {
        let body = run_body(
            BodyProcessed::NAME,
            processors.request_body(),
            self.held.request.body_mut(),
        )
        .await?;
        if let Some(bytes) = body {
            set_length(self.held.request.headers_mut(), bytes.len());
            *self.held.request.body_mut() = from_bytes(bytes);
        }
        Ok(Flow {
            context: self.context,
            held: self.held,
        })
    }
}

impl Flow<BodyProcessed> {
    /// Names this request in the response cache, buffering the body it is keyed by so the
    /// request can still be forwarded.
    pub async fn response_key(&mut self) -> Result<CacheKey, GatewayError> {
        let route = key_of(&self.context, &self.held.request)?;
        let body = std::mem::replace(self.held.request.body_mut(), crate::body::empty());
        let bytes = collect(StageName::BodyRead, body).await?;
        *self.held.request.body_mut() = from_bytes(bytes.clone());
        response_key(&route, &self.context.authority.identity().tenant, &bytes)
            .map_err(|error| GatewayError::new(BodyProcessed::NAME, error.into()))
    }

    /// Sends the request to the endpoint the dispatcher chose, or races it across the arms the
    /// rule allows.
    ///
    /// Each arm is counted against its endpoint until the upstream answers, so what the
    /// dispatcher knows about its load is what a request arriving now would actually wait behind.
    ///
    /// A request belongs to a turn either because `x-ip-turn` named one or because §429
    /// recognised it from the conversation's opening. A named turn is known before the body
    /// is, so a marked request never waits on its body; recognising one asks the body, and a
    /// body that may not be held is not read at all — such a request streams through exactly
    /// as it did before any of this, and is simply not sticky.
    pub async fn forward(
        self,
        upstream: &dyn Upstream,
        dispatcher: &Dispatcher,
        turns: Option<&TurnAffinity>,
    ) -> Result<Flow<Forwarded>, GatewayError> {
        let Routed {
            request,
            resolution,
            class,
        } = self.held;
        let raced = may_race(request.headers(), resolution.rule().hedge);
        let unmarked = turns.is_some() && self.context.turn.is_none();
        let mut request = request;
        let mut opening = None;
        // The body is held here when the turn's opening has to be read out of it: only for
        // an unmarked request, and only when a memory is configured that would read it. A
        // race makes its own buffer below — for a request whose opening was read here that
        // is a second collect over bytes already in memory, cheap for the reasons `race`
        // gives.
        if unmarked && may_hold(request.headers()) {
            let (parts, body) = request.into_parts();
            let bytes = collect(StageName::BodyRead, body).await?;
            opening = Opening::read(&bytes);
            request = Request::from_parts(parts, from_bytes(bytes));
        }
        let taken = taken_turn(self.context.turn.as_ref(), opening, class);
        let landed = remembered(turns, &self.context, taken.as_ref()).await;
        let dispatching =
            Dispatching::new(&resolution, class).landed_on(landed.as_ref().map(|v| &v.endpoint));
        if raced == Hedge::NONE {
            let chosen = dispatcher.choose(&dispatching);
            let served = Replica::of(chosen.endpoint());
            Span::current().record("served", field::display(&served));
            let request = rewrite(request, chosen.endpoint())?;
            let mut response = upstream
                .send(request)
                .await
                .map_err(|error| GatewayError::new(Forwarded::NAME, error.into()))?;
            strip_hop_by_hop(response.headers_mut());
            remember(turns, &self.context, taken.as_ref(), &served);
            return Ok(Flow {
                context: self.context,
                // Not raced, so one: the default the row carries anyway.
                held: Answered {
                    response,
                    served,
                    raced: Hedge::default(),
                },
            });
        }
        let outcome = race(self.context, upstream, dispatcher, request, &dispatching).await?;
        remember(
            turns,
            &outcome.context,
            taken.as_ref(),
            &outcome.held.served,
        );
        Ok(outcome)
    }
}

/// Which turn a request takes part in, and what that turn's verdict is to leave behind:
/// the caller's own word on the matter wins wherever it was said, and the conversation
/// speaks for itself where it was not. A request neither is on its own, and nothing of it
/// is kept.
fn taken_turn(
    marked: Option<&TurnId>,
    opening: Option<Opening>,
    class: Option<Difficulty>,
) -> Option<Taken> {
    let id = match (marked, &opening) {
        (Some(marked), _) => marked.clone(),
        (None, Some(opening)) => opening.anchor.clone(),
        (None, None) => return None,
    };
    Some(Taken {
        id,
        class,
        messages: opening.map(|opening| opening.messages),
    })
}

/// What one request settles about the turn it is in: which turn, and what to leave in its
/// verdict. `messages` is none when the body was never read; a verdict that holds no count
/// trusts the next request and compares nothing.
struct Taken {
    id: TurnId,
    class: Option<Difficulty>,
    messages: Option<usize>,
}

/// Where this turn landed, if there was one to remember: one read of the memory before the
/// choosing, since after the choosing the request is on its way and no decision is left
/// that could know a better endpoint than the one it is already at.
async fn remembered(
    turns: Option<&TurnAffinity>,
    context: &RequestContext,
    taken: Option<&Taken>,
) -> Option<Verdict> {
    let (turns, taken) = (turns?, taken?);
    let verdict = turns
        .landed(context.authority.identity(), &taken.id)
        .await?;
    // A history that has shrunk under the turn's landing is a compaction, and a compaction
    // starts a turn rather than continuing one.
    verdict.continues(taken.messages).then_some(verdict)
}

/// Keeps where this request was answered as where its turn lands from now on, which the next
/// request of the turn reads back. The write travels the way the usage row does, off the path
/// the answer takes; a memory that will not hold costs the landing and nothing else.
fn remember(
    turns: Option<&TurnAffinity>,
    context: &RequestContext,
    taken: Option<&Taken>,
    served: &Replica,
) {
    if let (Some(turns), Some(taken)) = (turns, taken) {
        turns.keep(
            context.authority.identity(),
            &taken.id,
            &Verdict {
                class: taken.class,
                messages: taken.messages,
                endpoint: served.clone(),
            },
        );
    }
}

/// The difficulty class this request names, refusing a value that is not one.
///
/// A caller that said what it needs is believed, and answering 400 beats sending the request
/// somewhere the caller did not ask for. Read at the header stage, not inside `forward`: the
/// chain may have set the header, and a name that cannot be read costs no limit count.
fn asked_class(request: &Request<GatewayBody>) -> Result<Option<Difficulty>, GatewayError> {
    let Some(value) = request.headers().get(&DIFFICULTY) else {
        return Ok(None);
    };
    let named = value.to_str().map_err(|_| {
        GatewayError::new(
            HeadersProcessed::NAME,
            GatewayErrorKind::Malformed {
                detail: format!("{DIFFICULTY} is not readable as text"),
            },
        )
    })?;
    Difficulty::named(named)
        .map(Some)
        .map_err(|error| GatewayError::new(HeadersProcessed::NAME, error.into()))
}

impl Flow<Forwarded> {
    /// Runs the response header chain.
    pub async fn process_response_headers(
        mut self,
        chain: &[Arc<dyn HeaderProcessor>],
    ) -> Result<Flow<ResponseHeadersProcessed>, GatewayError> {
        run_headers(
            ResponseHeadersProcessed::NAME,
            chain,
            self.held.response.headers_mut(),
        )
        .await?;
        Ok(Flow {
            context: self.context,
            held: self.held,
        })
    }
}

impl Flow<ResponseHeadersProcessed> {
    /// Runs the response body chain, or leaves the body untouched when there is none.
    pub async fn process_response_body(
        mut self,
        processors: &ProcessorChain,
    ) -> Result<Flow<ResponseBodyProcessed>, GatewayError> {
        if is_event_stream(self.held.response.headers()) && !processors.response_chunk().is_empty()
        {
            self.held.response.headers_mut().remove(CONTENT_LENGTH);
            let body = std::mem::replace(self.held.response.body_mut(), crate::body::empty());
            *self.held.response.body_mut() =
                stream_chunks(body, processors.response_chunk().to_vec());
            return Ok(Flow {
                context: self.context,
                held: self.held,
            });
        }
        let body = run_body(
            ResponseBodyProcessed::NAME,
            processors.response_body(),
            self.held.response.body_mut(),
        )
        .await?;
        if let Some(bytes) = body {
            set_length(self.held.response.headers_mut(), bytes.len());
            *self.held.response.body_mut() = from_bytes(bytes);
        }
        Ok(Flow {
            context: self.context,
            held: self.held,
        })
    }
}

impl Flow<ResponseBodyProcessed> {
    /// Which endpoint answered.
    pub fn served(&self) -> &Replica {
        &self.held.served
    }

    /// How many endpoints the request was raced across; one is what it was when it was not
    /// raced at all.
    pub fn raced(&self) -> Hedge {
        self.held.raced
    }

    /// The answer to keep and how long to keep it, buffering the body so it can still be sent,
    /// or nothing when this response may not be kept at all.
    ///
    /// The response's own expiration headers outrank `default`, which stands when it names none.
    pub async fn cacheable(
        &mut self,
        default: Ttl,
    ) -> Result<Option<(CachedResponse, Ttl)>, GatewayError> {
        // A stream is answered as it arrives and an error is not an answer to repeat.
        if !self.held.response.status().is_success()
            || is_event_stream(self.held.response.headers())
        {
            return Ok(None);
        }
        let ttl = match freshness(self.held.response.headers(), SystemTime::now()) {
            Freshness::For(ttl) => ttl,
            Freshness::Unsaid => default,
            Freshness::Never => return Ok(None),
        };
        let body = std::mem::replace(self.held.response.body_mut(), crate::body::empty());
        let bytes = collect(StageName::ResponseBodyRead, body).await?;
        *self.held.response.body_mut() = from_bytes(bytes.clone());
        let content_type = self.held.response.headers().get(CONTENT_TYPE).cloned();
        Ok(Some((CachedResponse::new(content_type, bytes), ttl)))
    }

    /// Hands the response back to the kernel.
    pub fn into_response(self) -> Result<Response<GatewayBody>, GatewayError> {
        Ok(self.held.response)
    }
}

/// Sends the arm's request and comes back with its index and its outcome.
///
/// The future carries only the armed request and the arm's index, never the arm itself, so a
/// losing arm can still be abandoned once its future is dropped.
async fn send_arm(
    at: usize,
    upstream: &dyn Upstream,
    sent: Request<GatewayBody>,
) -> (usize, Result<Response<GatewayBody>, GatewayError>) {
    (
        at,
        upstream
            .send(sent)
            .await
            .map_err(|error| GatewayError::new(Forwarded::NAME, error.into())),
    )
}

/// Races one request across the arms the dispatcher chose and keeps the first acceptable
/// answer.
///
/// The arms are polled together on the task the flow already runs on: they stay inside the
/// request's span, and cancelling a losing arm is a matter of dropping the future it is in.
async fn race(
    context: RequestContext,
    upstream: &dyn Upstream,
    dispatcher: &Dispatcher,
    request: Request<GatewayBody>,
    dispatching: &Dispatching<'_>,
) -> Result<Flow<Forwarded>, GatewayError> {
    // The body leaves once per arm, so it is held in hand once for the whole race; collecting
    // what is already bytes is cheap.
    let (mut parts, body) = request.into_parts();
    let buffered = collect(StageName::BodyRead, body).await?;
    // The same work on every arm, so it runs once, on the parts every arm is aimed from.
    strip_hop_by_hop(&mut parts.headers);
    let mut arms = dispatcher.choose_many(dispatching, dispatching.resolution().rule().hedge);
    let mut racing = FuturesUnordered::new();
    for (at, arm) in arms.iter().enumerate() {
        if at + 1 == arms.len() {
            continue;
        }
        let sent = aim(parts.clone(), from_bytes(buffered.clone()), arm.endpoint())?;
        racing.push(send_arm(at, upstream, sent));
    }
    // The last arm takes the parts that were already in hand, so it needs no clone.
    let last = arms.len() - 1;
    let sent = aim(parts, from_bytes(buffered.clone()), arms[last].endpoint())?;
    racing.push(send_arm(last, upstream, sent));
    let mut answered = vec![false; arms.len()];
    let mut winner: Option<(usize, Response<GatewayBody>)> = None;
    let mut first_failure: Option<(usize, Result<Response<GatewayBody>, GatewayError>)> = None;
    while let Some((at, outcome)) = racing.next().await {
        answered[at] = true;
        if answers_acceptably(&outcome) {
            winner = Some((at, outcome.expect("an acceptable answer is a response")));
            break;
        }
        if first_failure.is_none() {
            first_failure = Some((at, outcome));
        }
    }
    for (at, arm) in arms.iter_mut().enumerate() {
        if !answered[at] {
            arm.abandon();
        }
    }
    let (at, response) = match (winner, first_failure) {
        (Some((at, response)), _) => (at, response),
        (None, Some((at, Ok(response)))) => (at, response),
        (None, Some((_, Err(error)))) => return Err(error),
        (None, None) => unreachable!("every arm answered once"),
    };
    let endpoint = arms[at].endpoint();
    drop(racing);
    let served = Replica::of(endpoint);
    Span::current().record("served", field::display(&served));
    // The number the row will say is what left the gateway, not what the rule asked: the
    // dispatcher never chooses past the rule's hedge, which a count of endpoints already means.
    let raced = Hedge::new(
        u8::try_from(arms.len())
            .unwrap_or(HEDGE_MOST)
            .clamp(1, HEDGE_MOST),
    )
    .unwrap();
    let mut response = response;
    strip_hop_by_hop(response.headers_mut());
    Ok(Flow {
        context,
        held: Answered {
            response,
            served,
            raced,
        },
    })
}

/// Whether one arm's answer may win a race.
///
/// A transport error and a 5xx both say the endpoint cannot serve the request, and while one
/// arm answered badly the request may still be racing on its other arms.
fn answers_acceptably(outcome: &Result<Response<GatewayBody>, GatewayError>) -> bool {
    outcome
        .as_ref()
        .is_ok_and(|response| !response.status().is_server_error())
}

fn key_of(
    context: &RequestContext,
    request: &Request<GatewayBody>,
) -> Result<RouteKey, GatewayError> {
    let host = authority_of(request, context.listen);
    request_key(
        context.protocol,
        &host,
        context.listen,
        request.uri().path(),
    )
    .map_err(|error| GatewayError::new(StageName::HeaderRead, error.into()))
}

fn resolve(table: &RoutingTable, key: &RouteKey) -> Result<Resolution, GatewayError> {
    let resolution = table.resolve(key).ok_or_else(|| {
        GatewayError::new(
            Authorized::NAME,
            GatewayErrorKind::NoRoute { key: key.clone() },
        )
    })?;
    if let Some(target) = resolution
        .targets()
        .iter()
        .find(|target| !target.endpoint.protocol.is_forwarded())
    {
        return Err(GatewayError::new(
            Authorized::NAME,
            GatewayErrorKind::ProtocolNotServed {
                protocol: target.endpoint.protocol,
            },
        ));
    }
    Ok(resolution)
}

fn allow(context: &RequestContext, resolution: &Resolution) -> Result<(), GatewayError> {
    let identity = context.authority.identity();
    let scope = CapabilityScope::Api {
        user: identity.user.clone(),
        tenant: identity.tenant.clone(),
        api: resolution.rule().api.clone(),
    };
    if context.authority.allows(Capability::ApiAccess, &scope) {
        return Ok(());
    }
    Err(GatewayError::new(
        Authorized::NAME,
        GatewayErrorKind::Forbidden {
            capability: Capability::ApiAccess,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::super::Gateway;
    use super::super::fixtures::*;
    use super::*;
    use crate::processor::ProcessorChain;
    use crate::stage::StageName;
    use crate::table::RoutingTable;
    use bytes::Bytes;
    use http::header::{CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST};
    use http::{HeaderValue, StatusCode};
    use ip_core::Protocol;
    use std::time::Duration;

    #[tokio::test]
    async fn a_routed_request_reaches_the_target_with_its_remainder_and_query() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(table(), ProcessorChain::new(), upstream.clone());
        let response = gateway
            .handle(
                context(granted()),
                request("/anthropic/messages?beta=1", ""),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await, "pong");
        assert_eq!(
            upstream.last().uri,
            "https://api.example.com:443/v1/messages?beta=1"
        );
    }

    #[tokio::test]
    async fn a_path_no_rule_carries_is_not_found() {
        let gateway = Gateway::new(
            table(),
            ProcessorChain::new(),
            FakeUpstream::answering("pong"),
        );
        let error = gateway
            .handle(context(granted()), request("/elsewhere", ""))
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::NOT_FOUND);
        assert_eq!(error.stage(), StageName::Authorization);
    }

    #[tokio::test]
    async fn a_caller_holding_no_api_access_is_refused() {
        let gateway = Gateway::new(
            table(),
            ProcessorChain::new(),
            FakeUpstream::answering("pong"),
        );
        let error = gateway
            .handle(context(ungranted()), request("/anthropic", ""))
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::FORBIDDEN);
        assert!(matches!(
            error.kind(),
            GatewayErrorKind::Forbidden {
                capability: Capability::ApiAccess
            }
        ));
    }

    #[tokio::test]
    async fn a_protocol_this_build_does_not_carry_is_not_implemented() {
        let table = RoutingTable::from_rules(vec![rule(endpoint(
            Protocol::Ws,
            "api.example.com",
            443,
            "/v1",
        ))]);
        let gateway = Gateway::new(
            table,
            ProcessorChain::new(),
            FakeUpstream::answering("pong"),
        );
        let error = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn an_upstream_that_will_not_answer_is_a_bad_gateway() {
        let gateway = Gateway::new(table(), ProcessorChain::new(), FakeUpstream::failing());
        let error = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(error.stage(), StageName::Route);
    }

    #[tokio::test]
    async fn each_stage_names_itself_as_the_flow_advances() {
        let request = request("/anthropic", "");
        let flow = Flow::received(context(granted()), request);
        assert_eq!(flow.stage(), StageName::Receive);
        let flow = flow.authorize(&table()).unwrap();
        assert_eq!(flow.stage(), StageName::Authorization);
        let processors = ProcessorChain::new();
        let flow = flow
            .process_headers(processors.request_headers())
            .await
            .unwrap();
        assert_eq!(flow.stage(), StageName::HeaderProcess);
        let flow = flow.process_body(&processors).await.unwrap();
        assert_eq!(flow.stage(), StageName::BodyProcess);
        let upstream = FakeUpstream::answering("pong");
        let (dispatcher, _folding) = Dispatcher::new();
        let flow = flow
            .forward(upstream.as_ref(), &dispatcher, None)
            .await
            .unwrap();
        assert_eq!(flow.stage(), StageName::Route);
        let flow = flow
            .process_response_headers(processors.response_headers())
            .await
            .unwrap();
        assert_eq!(flow.stage(), StageName::ResponseHeaderProcess);
        let flow = flow.process_response_body(&processors).await.unwrap();
        assert_eq!(flow.stage(), StageName::ResponseBodyProcess);
        assert_eq!(body_of(flow.into_response().unwrap()).await, "pong");
    }

    #[tokio::test]
    async fn a_request_naming_a_class_reaches_an_endpoint_that_serves_it() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(
            ceiled_table(&[
                ("silent.example.com", None),
                ("hard.example.com", Some(ip_core::Difficulty::Hard)),
            ]),
            ProcessorChain::new(),
            upstream.clone(),
        );
        let mut hard = request("/anthropic/messages", "ask");
        hard.headers_mut()
            .insert(DIFFICULTY, HeaderValue::from_static("hard"));
        let response = gateway.handle(context(granted()), hard).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let sent = upstream.seen.lock().unwrap();
        assert_eq!(sent.len(), 1);
        // The silent endpoint serves hard too, and lost to the one that declared it.
        assert_eq!(sent[0].host, "hard.example.com:443");
    }

    #[tokio::test]
    async fn a_request_naming_a_value_that_is_not_a_class_reaches_no_endpoint() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(table(), ProcessorChain::new(), upstream.clone());
        let mut bogus = request("/anthropic/messages", "ask");
        bogus
            .headers_mut()
            .insert(DIFFICULTY, HeaderValue::from_static("epic"));
        let error = gateway.handle(context(granted()), bogus).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert!(upstream.seen.lock().unwrap().is_empty());
    }

    /// What one host in a race answers with.
    fn behaviour(delay: Duration, status: StatusCode, body: &'static str, fail: bool) -> Behaviour {
        Behaviour {
            delay,
            status,
            body,
            fail,
        }
    }

    #[tokio::test]
    async fn a_rule_that_does_not_hedge_sends_one_request() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(
            replicated_table(ip_core::Strategy::RoundRobin),
            ProcessorChain::new(),
            upstream.clone(),
        );
        let response = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let sent_to: Vec<String> = upstream
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|seen| seen.host.clone())
            .collect();
        assert_eq!(sent_to, ["one.example.com:443".to_owned()]);
    }

    #[tokio::test]
    async fn a_hedged_rule_races_its_arms_and_the_faster_one_is_recorded() {
        let upstream = FakeUpstream::answering_each(&[
            (
                "one.example.com:443",
                behaviour(
                    Duration::from_millis(300),
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "slow",
                    false,
                ),
            ),
            (
                "two.example.com:443",
                behaviour(Duration::from_millis(20), StatusCode::OK, "fast", false),
            ),
        ]);
        let table = hedged_replicated_table(
            ip_core::Strategy::RoundRobin,
            ip_core::Hedge::new(2).unwrap(),
        );
        let (dispatcher, _folding) = Dispatcher::new();
        let mut request = request("/anthropic/messages", "ask");
        request
            .headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from_static("3"));
        // Rides in, and must not ride on.
        request
            .headers_mut()
            .insert(CONNECTION, HeaderValue::from_static("keep-alive"));
        let flow = Flow::received(context(granted()), request)
            .authorize(&table)
            .unwrap();
        let processors = ProcessorChain::new();
        let flow = flow
            .process_headers(processors.request_headers())
            .await
            .unwrap()
            .process_body(&processors)
            .await
            .unwrap();
        let flow = flow
            .forward(upstream.as_ref(), &dispatcher, None)
            .await
            .unwrap();
        let flow = flow
            .process_response_headers(processors.response_headers())
            .await
            .unwrap()
            .process_response_body(&processors)
            .await
            .unwrap();
        assert_eq!(flow.served().host.as_str(), "two.example.com");
        assert_eq!(body_of(flow.into_response().unwrap()).await, "fast");
        let seen = upstream.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "a hedged rule sent one request per arm");
        let won = seen
            .iter()
            .find(|seen| seen.host == "two.example.com:443")
            .expect("the winning arm reached the upstream");
        assert_eq!(won.body, Bytes::from_static(b"ask"));
        assert_eq!(won.headers.get(CONTENT_LENGTH).unwrap(), "3");
        assert_eq!(won.headers.get(HOST).unwrap(), "two.example.com:443");
        assert!(won.headers.get(CONNECTION).is_none());
    }

    #[tokio::test]
    async fn an_arm_that_fails_fast_does_not_win_a_race() {
        let upstream = FakeUpstream::answering_each(&[
            (
                "one.example.com:443",
                behaviour(
                    Duration::from_millis(5),
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "boom",
                    false,
                ),
            ),
            (
                "two.example.com:443",
                behaviour(
                    Duration::from_millis(300),
                    StatusCode::OK,
                    "slow-pong",
                    false,
                ),
            ),
        ]);
        let gateway = Gateway::new(
            hedged_replicated_table(
                ip_core::Strategy::RoundRobin,
                ip_core::Hedge::new(2).unwrap(),
            ),
            ProcessorChain::new(),
            upstream.clone(),
        );
        let response = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await, "slow-pong");
        assert_eq!(upstream.seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn an_arm_that_drops_off_does_not_end_a_race() {
        let upstream = FakeUpstream::answering_each(&[
            (
                "one.example.com:443",
                behaviour(Duration::from_millis(5), StatusCode::OK, "", true),
            ),
            (
                "two.example.com:443",
                behaviour(
                    Duration::from_millis(200),
                    StatusCode::OK,
                    "slow-pong",
                    false,
                ),
            ),
        ]);
        let gateway = Gateway::new(
            hedged_replicated_table(
                ip_core::Strategy::RoundRobin,
                ip_core::Hedge::new(2).unwrap(),
            ),
            ProcessorChain::new(),
            upstream,
        );
        let response = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await, "slow-pong");
    }

    #[tokio::test]
    async fn a_race_lost_by_every_arm_answers_with_the_first_failure() {
        let upstream = FakeUpstream::answering_each(&[
            (
                "one.example.com:443",
                behaviour(
                    Duration::from_millis(5),
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "boom",
                    false,
                ),
            ),
            (
                "two.example.com:443",
                behaviour(
                    Duration::from_millis(1_000),
                    StatusCode::SERVICE_UNAVAILABLE,
                    "away",
                    false,
                ),
            ),
        ]);
        let gateway = Gateway::new(
            hedged_replicated_table(
                ip_core::Strategy::RoundRobin,
                ip_core::Hedge::new(2).unwrap(),
            ),
            ProcessorChain::new(),
            upstream,
        );
        let response = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body_of(response).await, "boom");
    }

    #[tokio::test]
    async fn a_race_lost_to_every_arm_is_a_bad_gateway() {
        let upstream = FakeUpstream::answering_each(&[
            (
                "one.example.com:443",
                behaviour(Duration::ZERO, StatusCode::OK, "", true),
            ),
            (
                "two.example.com:443",
                behaviour(Duration::ZERO, StatusCode::OK, "", true),
            ),
        ]);
        let gateway = Gateway::new(
            hedged_replicated_table(
                ip_core::Strategy::RoundRobin,
                ip_core::Hedge::new(2).unwrap(),
            ),
            ProcessorChain::new(),
            upstream,
        );
        let error = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(error.stage(), StageName::Route);
    }

    #[tokio::test]
    async fn an_arm_that_lost_a_race_teaches_the_dispatcher_nothing() {
        let (dispatcher, mut folding) = Dispatcher::new();
        let upstream = FakeUpstream::answering_each(&[
            (
                "one.example.com:443",
                behaviour(Duration::from_millis(300), StatusCode::OK, "slow", false),
            ),
            (
                "two.example.com:443",
                behaviour(Duration::from_millis(20), StatusCode::OK, "fast", false),
            ),
        ]);
        let gateway = Gateway::new(
            hedged_replicated_table(
                ip_core::Strategy::RoundRobin,
                ip_core::Hedge::new(2).unwrap(),
            ),
            ProcessorChain::new(),
            upstream,
        )
        .dispatching(Arc::new(dispatcher));
        let response = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        assert_eq!(body_of(response).await, "fast");
        // Only the winner's answer reached the folder; the arm that lost was abandoned
        // and reports no latency of its own.
        assert_eq!(folding.fold().await, 1);
    }

    #[tokio::test]
    async fn a_request_the_decision_will_not_race_reaches_only_one_arm() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(
            hedged_replicated_table(
                ip_core::Strategy::RoundRobin,
                ip_core::Hedge::new(3).unwrap(),
            ),
            ProcessorChain::new(),
            upstream.clone(),
        );
        let mut oversized = request("/anthropic/messages", "ask");
        oversized
            .headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from_static("5000000"));
        gateway.handle(context(granted()), oversized).await.unwrap();
        let mut multipart = request("/anthropic/messages", "ask");
        multipart.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_static("multipart/form-data; boundary=here"),
        );
        gateway.handle(context(granted()), multipart).await.unwrap();
        let sent: usize = upstream.seen.lock().unwrap().len();
        assert_eq!(sent, 2, "a refused race went out more than once");
    }
}
