use std::net::SocketAddr;
use std::sync::Arc;

use arc_swap::ArcSwap;
use std::time::SystemTime;

use bytes::Bytes;
use http::header::{
    CONNECTION, HOST, HeaderName, PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, TE, TRAILER,
    TRANSFER_ENCODING, UPGRADE,
};
use http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use http::uri::{Authority as UriAuthority, Scheme};
use http::{HeaderMap, HeaderValue, Request, Response, Uri};
use http_body_util::BodyExt;
use ip_auth::Authority;
use ip_cache::{CacheKey, Ttl};
use ip_core::{Capability, CapabilityScope, Endpoint, Protocol, RouteKey, TraceId, TurnId};

use crate::body::{GatewayBody, from_bytes};
use crate::cache::{CachedResponse, Freshness, ResponseCache, freshness, response_key};
use crate::error::{GatewayError, GatewayErrorKind};
use crate::processor::{BodyProcessor, ChainSource, FixedChains, HeaderProcessor, ProcessorChain};
use crate::sse::stream_chunks;
use crate::stage::{
    Authorized, BodyProcessed, Forwarded, HeadersProcessed, Received, ResponseBodyProcessed,
    ResponseHeadersProcessed, Routed, Stage, StageName,
};
use crate::table::{Resolution, RoutingTable, request_key};
use crate::upstream::Upstream;

/// What the dataflow needs about a request beyond the request itself.
pub struct RequestContext {
    /// Who the caller is, with everything granted to it.
    pub authority: Authority,
    /// The scheme the request arrived over.
    pub protocol: Protocol,
    /// Where the gateway listens, supplying the port a `Host` header omits.
    pub listen: SocketAddr,
    /// What this request is followed by, drawn at the edge and never taken from a caller.
    pub trace: TraceId,
    /// The chat turn it belongs to, when the caller marked one.
    pub turn: Option<TurnId>,
}

/// The request dataflow: the stages of `DESIGN.md` run in order.
///
/// Authentication happens before a request reaches here, so the flow starts at
/// authorization and carries the identity it was handed.
pub struct Gateway {
    /// What a request is routed and processed by, replaced whole when the rules change.
    routing: ArcSwap<Routing>,
    upstream: Arc<dyn Upstream>,
    responses: Option<ResponseCache>,
}

/// The rules in force: the table a request is routed by and the chains it is processed by.
///
/// The two are held together and replaced together, so no request is ever routed by one set
/// of rules and processed by another.
struct Routing {
    table: Arc<RoutingTable>,
    chains: Arc<dyn ChainSource>,
}

impl Gateway {
    /// Builds the flow around a routing table, one plugin chain every request runs, and an
    /// upstream.
    pub fn new(
        table: RoutingTable,
        processors: ProcessorChain,
        upstream: Arc<dyn Upstream>,
    ) -> Self {
        Self::with_chains(table, Arc::new(FixedChains::new(processors)), upstream)
    }

    /// Builds the flow around a routing table, a source of each request's plugin chains,
    /// and an upstream.
    pub fn with_chains(
        table: RoutingTable,
        chains: Arc<dyn ChainSource>,
        upstream: Arc<dyn Upstream>,
    ) -> Self {
        Self {
            routing: ArcSwap::from_pointee(Routing {
                table: Arc::new(table),
                chains,
            }),
            upstream,
            responses: None,
        }
    }

    /// Puts a new table and new chains in force, for every request that starts after this.
    ///
    /// Readers never wait for this: a request in flight finishes on the rules it started
    /// with, and the next one picks up the new ones.
    pub fn replace(&self, table: RoutingTable, chains: Arc<dyn ChainSource>) {
        self.routing.store(Arc::new(Routing {
            table: Arc::new(table),
            chains,
        }));
    }

    /// Answers a request the cache already holds an answer for out of `responses`.
    pub fn caching(mut self, responses: ResponseCache) -> Self {
        self.responses = Some(responses);
        self
    }

    /// The rules this flow routes by, as they stand now.
    pub fn table(&self) -> Arc<RoutingTable> {
        Arc::clone(&self.routing.load().table)
    }

    /// Carries one request through every stage, in the order the types allow.
    pub async fn handle(
        &self,
        context: RequestContext,
        request: Request<GatewayBody>,
    ) -> Result<Response<GatewayBody>, GatewayError> {
        // One load, so the request is routed and processed by the same rules throughout.
        let routing = self.routing.load();
        let authorized = Flow::received(context, request).authorize(&routing.table)?;
        let chains = routing.chains.chains_for(
            authorized.context().authority.identity(),
            &authorized.resolution().rule().api,
        );
        let mut processed = authorized
            .process_headers(chains.request_headers())
            .await?
            .process_body(&chains)
            .await?;
        let Some(responses) = &self.responses else {
            return processed
                .forward(self.upstream.as_ref())
                .await?
                .process_response_headers(chains.response_headers())
                .await?
                .process_response_body(&chains)
                .await?
                .into_response();
        };
        // A hit answers here, spending no tokens and running no response processor, as designed.
        let key = processed.response_key().await?;
        match responses.get(&key).await {
            Ok(Some(hit)) => return Ok(answer_with(hit)),
            Ok(None) => {}
            // A cache that cannot answer costs a round trip upstream, never the request itself.
            Err(error) => tracing::warn!(%error, "could not read the response cache"),
        }
        let mut done = processed
            .forward(self.upstream.as_ref())
            .await?
            .process_response_headers(chains.response_headers())
            .await?
            .process_response_body(&chains)
            .await?;
        if let Some((answer, ttl)) = done.cacheable(responses.ttl()).await?
            && let Err(error) = responses.put(&key, &answer, ttl).await
        {
            tracing::warn!(%error, "could not keep a response in the cache");
        }
        done.into_response()
    }
}

/// Answers from what the cache held, without any stage after the request body running.
fn answer_with(hit: CachedResponse) -> Response<GatewayBody> {
    let mut response = Response::new(from_bytes(hit.body().clone()));
    if let Some(content_type) = hit.content_type() {
        response
            .headers_mut()
            .insert(CONTENT_TYPE, content_type.clone());
    }
    set_length(response.headers_mut(), hit.body().len());
    response
}

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
            },
        })
    }
}

impl Flow<Authorized> {
    /// Where the request is going, and the rule that sent it there.
    pub fn resolution(&self) -> &Resolution {
        &self.held.resolution
    }

    /// Runs the request header chain.
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

    /// Sends the request to the endpoint the rule chose.
    pub async fn forward(self, upstream: &dyn Upstream) -> Result<Flow<Forwarded>, GatewayError> {
        let Routed {
            request,
            resolution,
        } = self.held;
        let request = rewrite(request, resolution.primary())?;
        let mut response = upstream
            .send(request)
            .await
            .map_err(|error| GatewayError::new(Forwarded::NAME, error.into()))?;
        strip_hop_by_hop(response.headers_mut());
        Ok(Flow {
            context: self.context,
            held: response,
        })
    }
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
            self.held.headers_mut(),
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
        if is_event_stream(self.held.headers()) && !processors.response_chunk().is_empty() {
            self.held.headers_mut().remove(CONTENT_LENGTH);
            let body = std::mem::replace(self.held.body_mut(), crate::body::empty());
            *self.held.body_mut() = stream_chunks(body, processors.response_chunk().to_vec());
            return Ok(Flow {
                context: self.context,
                held: self.held,
            });
        }
        let body = run_body(
            ResponseBodyProcessed::NAME,
            processors.response_body(),
            self.held.body_mut(),
        )
        .await?;
        if let Some(bytes) = body {
            set_length(self.held.headers_mut(), bytes.len());
            *self.held.body_mut() = from_bytes(bytes);
        }
        Ok(Flow {
            context: self.context,
            held: self.held,
        })
    }
}

impl Flow<ResponseBodyProcessed> {
    /// The answer to keep and how long to keep it, buffering the body so it can still be sent,
    /// or nothing when this response may not be kept at all.
    ///
    /// The response's own expiration headers outrank `default`, which stands when it names none.
    pub async fn cacheable(
        &mut self,
        default: Ttl,
    ) -> Result<Option<(CachedResponse, Ttl)>, GatewayError> {
        // A stream is answered as it arrives and an error is not an answer to repeat.
        if !self.held.status().is_success() || is_event_stream(self.held.headers()) {
            return Ok(None);
        }
        let ttl = match freshness(self.held.headers(), SystemTime::now()) {
            Freshness::For(ttl) => ttl,
            Freshness::Unsaid => default,
            Freshness::Never => return Ok(None),
        };
        let body = std::mem::replace(self.held.body_mut(), crate::body::empty());
        let bytes = collect(StageName::ResponseBodyRead, body).await?;
        *self.held.body_mut() = from_bytes(bytes.clone());
        let content_type = self.held.headers().get(CONTENT_TYPE).cloned();
        Ok(Some((CachedResponse::new(content_type, bytes), ttl)))
    }

    /// Hands the response back to the kernel.
    pub fn into_response(self) -> Result<Response<GatewayBody>, GatewayError> {
        Ok(self.held)
    }
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
        .find(|target| !target.protocol.is_forwarded())
    {
        return Err(GatewayError::new(
            Authorized::NAME,
            GatewayErrorKind::ProtocolNotServed {
                protocol: target.protocol,
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

/// Points the request at the endpoint it was routed to, carrying its query across.
fn rewrite(
    request: Request<GatewayBody>,
    target: &Endpoint,
) -> Result<Request<GatewayBody>, GatewayError> {
    let query = request.uri().query().map(ToOwned::to_owned);
    let (mut parts, body) = request.into_parts();
    parts.uri = target_uri(target, query.as_deref()).map_err(|detail| {
        GatewayError::new(Forwarded::NAME, GatewayErrorKind::Malformed { detail })
    })?;
    let host = format!("{}:{}", target.host, target.port);
    let host = host.parse().map_err(|_| {
        GatewayError::new(
            Forwarded::NAME,
            GatewayErrorKind::Malformed {
                detail: format!("{host} is not a host header"),
            },
        )
    })?;
    strip_hop_by_hop(&mut parts.headers);
    parts.headers.insert(HOST, host);
    Ok(Request::from_parts(parts, body))
}

/// Headers that describe one connection, which a proxy must not pass on to the next.
const HOP_BY_HOP: [HeaderName; 8] = [
    CONNECTION,
    HeaderName::from_static("keep-alive"),
    HeaderName::from_static("proxy-connection"),
    PROXY_AUTHENTICATE,
    PROXY_AUTHORIZATION,
    TRAILER,
    TRANSFER_ENCODING,
    UPGRADE,
];

/// Drops every hop by hop header, including any the `Connection` header names.
/// `te: trailers` survives, because grpc needs it end to end.
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let listed: Vec<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in listed.iter().chain(HOP_BY_HOP.iter()) {
        headers.remove(name);
    }
    let trailers_only = headers
        .get_all(TE)
        .iter()
        .all(|value| value.as_bytes().eq_ignore_ascii_case(b"trailers"));
    if !trailers_only {
        headers.remove(TE);
    }
}

/// The authority a request names, which the `Host` header carries over http 1.1
/// and the uri carries over http 2.
fn authority_of(request: &Request<GatewayBody>, listen: SocketAddr) -> String {
    if let Some(authority) = request.uri().authority() {
        return authority.to_string();
    }
    request
        .headers()
        .get(HOST)
        .and_then(|host| host.to_str().ok())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| listen.to_string())
}

fn target_uri(target: &Endpoint, query: Option<&str>) -> Result<Uri, String> {
    let scheme = Scheme::try_from(target.protocol.name())
        .map_err(|_| format!("{} is not a uri scheme", target.protocol))?;
    let authority = format!("{}:{}", target.host, target.port);
    let authority = UriAuthority::try_from(authority.as_str())
        .map_err(|_| format!("{authority} is not a uri authority"))?;
    let path = match query {
        Some(query) => format!("{}?{}", target.path, query),
        None => target.path.to_string(),
    };
    Uri::builder()
        .scheme(scheme)
        .authority(authority)
        .path_and_query(path)
        .build()
        .map_err(|error| error.to_string())
}

async fn run_headers(
    stage: StageName,
    chain: &[Arc<dyn HeaderProcessor>],
    headers: &mut HeaderMap,
) -> Result<(), GatewayError> {
    for processor in chain {
        processor
            .process(headers)
            .await
            .map_err(|error| GatewayError::new(stage, error.into()))?;
    }
    Ok(())
}

/// Whether a response is a stream of server sent events.
fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
}

/// Makes `Content-Length` match a body a plugin rewrote, so the next hop reads all of it.
fn set_length(headers: &mut HeaderMap, length: usize) {
    headers.insert(CONTENT_LENGTH, HeaderValue::from(length));
}

/// Runs a body chain, or hands back `None` when the body may pass through untouched.
///
/// Whether it passes is read from the chain itself, so a body can only be buffered when
/// something is actually there to rewrite it.
/// Reads a whole body, reporting a stream that failed part way as the stage that read it.
async fn collect(stage: StageName, body: GatewayBody) -> Result<Bytes, GatewayError> {
    Ok(body
        .collect()
        .await
        .map_err(|error| {
            GatewayError::new(
                stage,
                GatewayErrorKind::Malformed {
                    detail: error.to_string(),
                },
            )
        })?
        .to_bytes())
}

async fn run_body(
    stage: StageName,
    chain: &[Arc<dyn BodyProcessor>],
    body: &mut GatewayBody,
) -> Result<Option<Bytes>, GatewayError> {
    if chain.is_empty() {
        return Ok(None);
    }
    let read_stage = match stage {
        StageName::BodyProcess => StageName::BodyRead,
        _ => StageName::ResponseBodyRead,
    };
    let taken = std::mem::replace(body, crate::body::empty());
    let mut bytes = taken
        .collect()
        .await
        .map_err(|error| {
            GatewayError::new(
                read_stage,
                GatewayErrorKind::Malformed {
                    detail: error.to_string(),
                },
            )
        })?
        .to_bytes();
    for processor in chain {
        bytes = processor
            .process(bytes)
            .await
            .map_err(|error| GatewayError::new(stage, error.into()))?;
    }
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use bytes::Bytes;
    use http::{HeaderName, HeaderValue, StatusCode};
    use ip_auth::Identity;
    use ip_core::{
        AbsPath, ApiId, Capability, Grant, Grants, Host, Port, Role, RouteKey, RouteRule,
        RouteTarget, TenantId, UserId,
    };

    use super::*;
    use crate::body::from_bytes;
    use crate::error::ProcessorError;
    use ip_cache::Ttl;
    use ip_core::PluginOrder;

    /// Records what it was sent, and answers with what it was built with.
    struct FakeUpstream {
        seen: Mutex<Vec<Seen>>,
        status: StatusCode,
        body: Bytes,
        headers: Vec<(&'static str, &'static str)>,
        fail: bool,
    }

    struct Seen {
        uri: String,
        host: String,
        headers: HeaderMap,
        body: Bytes,
    }

    impl FakeUpstream {
        fn answering(body: &str) -> Arc<Self> {
            Arc::new(Self {
                seen: Mutex::new(Vec::new()),
                status: StatusCode::OK,
                body: Bytes::from(body.to_owned()),
                headers: Vec::new(),
                fail: false,
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                seen: Mutex::new(Vec::new()),
                status: StatusCode::OK,
                body: Bytes::new(),
                headers: Vec::new(),
                fail: true,
            })
        }

        fn answering_with_headers(
            body: &str,
            headers: Vec<(&'static str, &'static str)>,
        ) -> Arc<Self> {
            Arc::new(Self {
                seen: Mutex::new(Vec::new()),
                status: StatusCode::OK,
                body: Bytes::from(body.to_owned()),
                headers,
                fail: false,
            })
        }

        fn last(&self) -> Seen {
            self.seen.lock().unwrap().pop().expect("nothing was sent")
        }
    }

    #[async_trait::async_trait]
    impl Upstream for FakeUpstream {
        async fn send(
            &self,
            request: Request<GatewayBody>,
        ) -> Result<Response<GatewayBody>, crate::error::UpstreamError> {
            if self.fail {
                return Err(crate::error::UpstreamError::new("connection refused"));
            }
            let uri = request.uri().to_string();
            let host = request
                .headers()
                .get(HOST)
                .map(|value| value.to_str().unwrap().to_owned())
                .unwrap_or_default();
            let headers = request.headers().clone();
            let body = request.into_body().collect().await.unwrap().to_bytes();
            self.seen.lock().unwrap().push(Seen {
                uri,
                host,
                headers,
                body,
            });
            let mut response = Response::builder().status(self.status);
            for (name, value) in &self.headers {
                response = response.header(*name, *value);
            }
            Ok(response.body(from_bytes(self.body.clone())).unwrap())
        }
    }

    /// Appends a marker so the order plugins ran in is visible in the output.
    struct Marker {
        order: PluginOrder,
        mark: &'static str,
    }

    impl Marker {
        fn at(order: u8, mark: &'static str) -> Arc<Self> {
            Arc::new(Self {
                order: PluginOrder::new(order),
                mark,
            })
        }
    }

    #[async_trait::async_trait]
    impl BodyProcessor for Marker {
        fn order(&self) -> PluginOrder {
            self.order
        }

        async fn process(&self, body: Bytes) -> Result<Bytes, ProcessorError> {
            let mut out = body.to_vec();
            out.extend_from_slice(self.mark.as_bytes());
            Ok(Bytes::from(out))
        }
    }

    struct SetHeader {
        name: HeaderName,
        value: &'static str,
    }

    impl SetHeader {
        fn new(name: &'static str, value: &'static str) -> Arc<Self> {
            Arc::new(Self {
                name: HeaderName::from_static(name),
                value,
            })
        }
    }

    #[async_trait::async_trait]
    impl HeaderProcessor for SetHeader {
        fn order(&self) -> PluginOrder {
            PluginOrder::new(10)
        }

        async fn process(&self, headers: &mut HeaderMap) -> Result<(), ProcessorError> {
            headers.insert(self.name.clone(), HeaderValue::from_static(self.value));
            Ok(())
        }
    }

    /// Stops every request with the error it was built with.
    struct Stop(ProcessorError);

    #[async_trait::async_trait]
    impl HeaderProcessor for Stop {
        fn order(&self) -> PluginOrder {
            PluginOrder::new(200)
        }

        async fn process(&self, _: &mut HeaderMap) -> Result<(), ProcessorError> {
            Err(self.0.clone())
        }
    }

    fn user() -> UserId {
        UserId::new("alice").unwrap()
    }

    fn tenant() -> TenantId {
        TenantId::new("acme").unwrap()
    }

    fn api() -> ApiId {
        ApiId::new("anthropic").unwrap()
    }

    fn endpoint(protocol: Protocol, host: &str, port: u16, path: &str) -> Endpoint {
        Endpoint::new(
            protocol,
            Host::new(host).unwrap(),
            Port::new(port).unwrap(),
            AbsPath::new(path).unwrap(),
        )
    }

    fn rule(target: Endpoint) -> RouteRule {
        RouteRule {
            api: api(),
            key: RouteKey::new(endpoint(
                Protocol::Http,
                "gateway.local",
                8080,
                "/anthropic",
            )),
            target: RouteTarget::new(target),
        }
    }

    fn table() -> RoutingTable {
        RoutingTable::from_rules(vec![rule(endpoint(
            Protocol::Https,
            "api.example.com",
            443,
            "/v1",
        ))])
    }

    fn granted() -> Authority {
        let scope = CapabilityScope::Api {
            user: user(),
            tenant: tenant(),
            api: api(),
        };
        let grants: Grants = [Grant::new(Capability::ApiAccess, scope).unwrap()]
            .into_iter()
            .collect();
        Authority::new(identity(), grants)
    }

    fn ungranted() -> Authority {
        Authority::new(identity(), Grants::new())
    }

    fn identity() -> Identity {
        Identity {
            user: user(),
            tenant: tenant(),
            role: Role::Member,
        }
    }

    fn context(authority: Authority) -> RequestContext {
        RequestContext {
            authority,
            protocol: Protocol::Http,
            listen: "127.0.0.1:8080".parse().unwrap(),
            trace: TraceId::generate().unwrap(),
            turn: None,
        }
    }

    fn request(path_and_query: &str, body: &str) -> Request<GatewayBody> {
        Request::builder()
            .uri(path_and_query)
            .header(HOST, "gateway.local:8080")
            .body(from_bytes(Bytes::from(body.to_owned())))
            .unwrap()
    }

    async fn body_of(response: Response<GatewayBody>) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// A gateway that answers repeated requests out of a cache of its own.
    fn caching_gateway(upstream: Arc<FakeUpstream>) -> Gateway {
        let cache = Arc::new(ip_cache::SledCache::temporary().unwrap());
        Gateway::new(table(), ProcessorChain::new(), upstream)
            .caching(ResponseCache::new(cache, Ttl::seconds(60).unwrap()))
    }

    /// Another tenant asking the same route the same thing.
    fn other_tenant() -> Authority {
        let tenant = TenantId::new("other").unwrap();
        let identity = Identity {
            user: user(),
            tenant: tenant.clone(),
            role: Role::Member,
        };
        let scope = CapabilityScope::Api {
            user: user(),
            tenant,
            api: api(),
        };
        let grants: Grants = [Grant::new(Capability::ApiAccess, scope).unwrap()]
            .into_iter()
            .collect();
        Authority::new(identity, grants)
    }

    #[tokio::test]
    async fn a_repeated_request_is_answered_without_the_upstream() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = caching_gateway(upstream.clone());
        let first = gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(body_of(first).await, "pong");
        let second = gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(body_of(second).await, "pong");
        assert_eq!(
            upstream.seen.lock().unwrap().len(),
            1,
            "the upstream was called twice"
        );
    }

    #[tokio::test]
    async fn a_response_that_refuses_to_be_stored_is_not_kept() {
        let upstream =
            FakeUpstream::answering_with_headers("pong", vec![("cache-control", "no-store")]);
        let gateway = caching_gateway(upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(
            upstream.seen.lock().unwrap().len(),
            2,
            "an upstream that said no-store was cached anyway"
        );
    }

    #[tokio::test]
    async fn a_max_age_shorter_than_the_default_expires_the_answer_sooner() {
        let upstream =
            FakeUpstream::answering_with_headers("pong", vec![("cache-control", "max-age=1")]);
        let cache = Arc::new(ip_cache::SledCache::temporary().unwrap());
        let gateway = Gateway::new(table(), ProcessorChain::new(), upstream.clone())
            .caching(ResponseCache::new(cache, Ttl::seconds(600).unwrap()));
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(upstream.seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_max_age_longer_than_the_default_keeps_the_answer_longer() {
        let upstream =
            FakeUpstream::answering_with_headers("pong", vec![("cache-control", "max-age=600")]);
        let cache = Arc::new(ip_cache::SledCache::temporary().unwrap());
        let gateway = Gateway::new(table(), ProcessorChain::new(), upstream.clone())
            .caching(ResponseCache::new(cache, Ttl::seconds(1).unwrap()));
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(upstream.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_response_header_plugin_may_still_decide_what_is_kept() {
        let upstream = FakeUpstream::answering("pong");
        let cache = Arc::new(ip_cache::SledCache::temporary().unwrap());
        let processors =
            ProcessorChain::new().with_response_header(SetHeader::new("cache-control", "no-store"));
        let gateway = Gateway::new(table(), processors, upstream.clone())
            .caching(ResponseCache::new(cache, Ttl::seconds(600).unwrap()));
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(upstream.seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_hit_runs_no_response_processor() {
        let upstream = FakeUpstream::answering("pong");
        let cache = Arc::new(ip_cache::SledCache::temporary().unwrap());
        let processors =
            ProcessorChain::new().with_response_header(SetHeader::new("x-processed", "yes"));
        let gateway = Gateway::new(table(), processors, upstream.clone())
            .caching(ResponseCache::new(cache, Ttl::seconds(60).unwrap()));
        let first = gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(first.headers()["x-processed"], "yes");
        let hit = gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert!(
            hit.headers().get("x-processed").is_none(),
            "a hit ran the response header chain"
        );
    }

    #[tokio::test]
    async fn a_cached_answer_carries_the_content_type_it_was_kept_with() {
        let upstream =
            FakeUpstream::answering_with_headers("{}", vec![("content-type", "application/json")]);
        let gateway = caching_gateway(upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        let hit = gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(hit.headers()[CONTENT_TYPE], "application/json");
        assert_eq!(hit.headers()[CONTENT_LENGTH], "2");
        assert_eq!(body_of(hit).await, "{}");
    }

    #[tokio::test]
    async fn another_body_is_not_answered_from_the_cache() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = caching_gateway(upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        gateway
            .handle(context(granted()), request("/anthropic", "ask again"))
            .await
            .unwrap();
        assert_eq!(upstream.seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn another_tenant_is_not_answered_from_the_cache() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = caching_gateway(upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        gateway
            .handle(context(other_tenant()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(
            upstream.seen.lock().unwrap().len(),
            2,
            "one tenant read another's answer"
        );
    }

    #[tokio::test]
    async fn the_request_still_reaches_the_upstream_whole_when_it_is_keyed() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = caching_gateway(upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(upstream.last().body, Bytes::from_static(b"ask"));
    }

    #[tokio::test]
    async fn a_response_that_failed_is_not_kept() {
        let upstream = Arc::new(FakeUpstream {
            seen: Mutex::new(Vec::new()),
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: Bytes::from_static(b"boom"),
            headers: Vec::new(),
            fail: false,
        });
        let gateway = caching_gateway(upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(upstream.seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn an_event_stream_is_not_kept() {
        let upstream = FakeUpstream::answering_with_headers(
            "data: one\n\n",
            vec![("content-type", "text/event-stream")],
        );
        let gateway = caching_gateway(upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(upstream.seen.lock().unwrap().len(), 2);
    }

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
    async fn the_host_header_is_rewritten_to_the_target() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(table(), ProcessorChain::new(), upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap();
        assert_eq!(upstream.last().host, "api.example.com:443");
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
    async fn a_request_header_plugin_rewrites_what_goes_upstream() {
        let upstream = FakeUpstream::answering("pong");
        let processors =
            ProcessorChain::new().with_request_header(SetHeader::new("x-ip-trace", "yes"));
        let gateway = Gateway::new(table(), processors, upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap();
        assert_eq!(
            upstream.last().headers.get("x-ip-trace").unwrap(),
            HeaderValue::from_static("yes")
        );
    }

    #[tokio::test]
    async fn hop_by_hop_headers_never_reach_the_upstream() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(table(), ProcessorChain::new(), upstream.clone());
        let mut request = request("/anthropic", "");
        for (name, value) in [
            ("connection", "keep-alive, x-private"),
            ("keep-alive", "timeout=5"),
            ("x-private", "for this hop only"),
            ("upgrade", "websocket"),
            ("transfer-encoding", "chunked"),
            ("proxy-authorization", "Basic c2VjcmV0"),
            ("te", "gzip"),
            ("x-keep", "end to end"),
        ] {
            request.headers_mut().insert(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        gateway.handle(context(granted()), request).await.unwrap();
        let sent = upstream.last().headers;
        for name in [
            "connection",
            "keep-alive",
            "x-private",
            "upgrade",
            "transfer-encoding",
            "proxy-authorization",
            "te",
        ] {
            assert!(sent.get(name).is_none(), "{name} reached the upstream");
        }
        assert_eq!(sent.get("x-keep").unwrap(), "end to end");
    }

    #[tokio::test]
    async fn te_trailers_is_carried_for_grpc() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(table(), ProcessorChain::new(), upstream.clone());
        let mut request = request("/anthropic", "");
        request
            .headers_mut()
            .insert(TE, HeaderValue::from_static("trailers"));
        gateway.handle(context(granted()), request).await.unwrap();
        assert_eq!(upstream.last().headers.get(TE).unwrap(), "trailers");
    }

    #[tokio::test]
    async fn hop_by_hop_headers_from_the_upstream_never_reach_the_caller() {
        let upstream = FakeUpstream::answering_with_headers(
            "pong",
            vec![
                ("connection", "close"),
                ("keep-alive", "timeout=5"),
                ("x-upstream-keep", "1"),
            ],
        );
        let gateway = Gateway::new(table(), ProcessorChain::new(), upstream);
        let response = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap();
        assert!(response.headers().get("connection").is_none());
        assert!(response.headers().get("keep-alive").is_none());
        assert_eq!(response.headers().get("x-upstream-keep").unwrap(), "1");
    }

    /// Counts the requests that reach it, and passes each on untouched.
    #[derive(Default)]
    struct Counter(AtomicUsize);

    #[async_trait::async_trait]
    impl HeaderProcessor for Counter {
        fn order(&self) -> PluginOrder {
            PluginOrder::new(100)
        }

        async fn process(&self, _: &mut HeaderMap) -> Result<(), ProcessorError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_request_that_fails_authorization_never_reaches_a_plugin() {
        for (authority, path, status) in [
            (ungranted(), "/anthropic", StatusCode::FORBIDDEN),
            (granted(), "/elsewhere", StatusCode::NOT_FOUND),
        ] {
            let counter = Arc::new(Counter::default());
            let processors = ProcessorChain::new().with_request_header(counter.clone());
            let gateway = Gateway::new(table(), processors, FakeUpstream::answering("pong"));
            let error = gateway
                .handle(context(authority), request(path, ""))
                .await
                .unwrap_err();
            assert_eq!(error.status(), status);
            assert_eq!(
                counter.0.load(Ordering::Relaxed),
                0,
                "a plugin ran for {path}"
            );
        }
    }

    /// Marks one user's responses, and remembers every request it was asked about.
    struct PerUser {
        marked: UserId,
        asked: Mutex<Vec<(UserId, ApiId)>>,
    }

    impl PerUser {
        fn marking(user: &str) -> Arc<Self> {
            Arc::new(Self {
                marked: UserId::new(user).unwrap(),
                asked: Mutex::new(Vec::new()),
            })
        }
    }

    impl ChainSource for PerUser {
        fn chains_for(&self, identity: &Identity, api: &ApiId) -> Arc<ProcessorChain> {
            self.asked
                .lock()
                .unwrap()
                .push((identity.user.clone(), api.clone()));
            let chain = if identity.user == self.marked {
                ProcessorChain::new().with_response_body(Marker::at(10, "-marked"))
            } else {
                ProcessorChain::new()
            };
            Arc::new(chain)
        }
    }

    #[tokio::test]
    async fn each_request_runs_the_chains_its_source_picks() {
        for (marked, expected) in [("alice", "pong-marked"), ("bob", "pong")] {
            let gateway = Gateway::with_chains(
                table(),
                PerUser::marking(marked),
                FakeUpstream::answering("pong"),
            );
            let response = gateway
                .handle(context(granted()), request("/anthropic", ""))
                .await
                .unwrap();
            assert_eq!(
                body_of(response).await,
                expected,
                "chains marked for {marked}"
            );
        }
    }

    #[tokio::test]
    async fn the_source_is_asked_about_the_caller_and_the_routed_api() {
        let source = PerUser::marking("alice");
        let gateway =
            Gateway::with_chains(table(), source.clone(), FakeUpstream::answering("pong"));
        gateway
            .handle(context(granted()), request("/anthropic/messages", ""))
            .await
            .unwrap();
        assert_eq!(source.asked.lock().unwrap().as_slice(), [(user(), api())]);
    }

    #[tokio::test]
    async fn a_forbidden_request_never_asks_for_chains() {
        let source = PerUser::marking("alice");
        let gateway =
            Gateway::with_chains(table(), source.clone(), FakeUpstream::answering("pong"));
        let error = gateway
            .handle(context(ungranted()), request("/anthropic", ""))
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::FORBIDDEN);
        assert!(source.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failing_plugin_stops_the_flow_at_its_stage() {
        let processors = ProcessorChain::new()
            .with_request_header(Arc::new(Stop(ProcessorError::failed("plugin crashed"))));
        let gateway = Gateway::new(table(), processors, FakeUpstream::answering("pong"));
        let error = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap_err();
        assert_eq!(error.stage(), StageName::HeaderProcess);
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error.public_reason(), None);
    }

    #[tokio::test]
    async fn a_refusing_plugin_is_forbidden_with_its_reason() {
        let upstream = FakeUpstream::answering("pong");
        let processors = ProcessorChain::new()
            .with_request_header(Arc::new(Stop(ProcessorError::refused("blocked by policy"))));
        let gateway = Gateway::new(table(), processors, upstream.clone());
        let error = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::FORBIDDEN);
        assert_eq!(error.public_reason(), Some("blocked by policy"));
        assert!(upstream.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_request_body_plugin_transforms_what_goes_upstream() {
        let upstream = FakeUpstream::answering("pong");
        let processors = ProcessorChain::new().with_request_body(Marker::at(10, "-marked"));
        let gateway = Gateway::new(table(), processors, upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", "payload"))
            .await
            .unwrap();
        assert_eq!(upstream.last().body, Bytes::from("payload-marked"));
    }

    #[tokio::test]
    async fn a_chunk_plugin_does_not_force_the_body_to_be_buffered() {
        let processors = ProcessorChain::new().with_response_chunk(Marker::at(10, "-chunk"));
        assert!(processors.passes_response_body());
        let gateway = Gateway::new(table(), processors, FakeUpstream::answering("pong"));
        let response = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap();
        assert_eq!(body_of(response).await, "pong");
    }

    #[tokio::test]
    async fn a_rewritten_request_body_carries_its_new_length() {
        let upstream = FakeUpstream::answering("pong");
        let processors = ProcessorChain::new().with_request_body(Marker::at(10, "-marked"));
        let gateway = Gateway::new(table(), processors, upstream.clone());
        let mut request = request("/anthropic", "payload");
        request
            .headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from_static("7"));
        gateway.handle(context(granted()), request).await.unwrap();
        let sent = upstream.last();
        assert_eq!(sent.body, Bytes::from("payload-marked"));
        assert_eq!(sent.headers.get(CONTENT_LENGTH).unwrap(), "14");
    }

    #[tokio::test]
    async fn a_rewritten_response_body_carries_its_new_length() {
        let upstream = FakeUpstream::answering_with_headers("pong", vec![("content-length", "4")]);
        let processors = ProcessorChain::new().with_response_body(Marker::at(10, "-seen"));
        let gateway = Gateway::new(table(), processors, upstream);
        let response = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap();
        assert_eq!(response.headers().get(CONTENT_LENGTH).unwrap(), "9");
        assert_eq!(body_of(response).await, "pong-seen");
    }

    #[tokio::test]
    async fn an_untouched_body_keeps_its_length() {
        let upstream = FakeUpstream::answering_with_headers("pong", vec![("content-length", "4")]);
        let gateway = Gateway::new(table(), ProcessorChain::new(), upstream);
        let response = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap();
        assert_eq!(response.headers().get(CONTENT_LENGTH).unwrap(), "4");
    }

    #[tokio::test]
    async fn an_event_stream_runs_each_event_through_the_chunk_chain() {
        let upstream = FakeUpstream::answering_with_headers(
            "data: a\n\ndata: b\n\n",
            vec![
                ("content-type", "text/event-stream; charset=utf-8"),
                ("content-length", "18"),
            ],
        );
        let processors = ProcessorChain::new().with_response_chunk(Marker::at(10, "-seen"));
        let gateway = Gateway::new(table(), processors, upstream);
        let response = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap();
        assert!(response.headers().get(CONTENT_LENGTH).is_none());
        assert_eq!(body_of(response).await, "data: a\n\n-seendata: b\n\n-seen");
    }

    #[tokio::test]
    async fn a_response_that_is_not_an_event_stream_skips_the_chunk_chain() {
        let upstream = FakeUpstream::answering_with_headers(
            "pong",
            vec![("content-type", "application/json")],
        );
        let processors = ProcessorChain::new().with_response_chunk(Marker::at(10, "-seen"));
        let gateway = Gateway::new(table(), processors, upstream);
        let response = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap();
        assert_eq!(body_of(response).await, "pong");
    }

    #[tokio::test]
    async fn an_event_stream_with_no_chunk_plugins_passes_untouched() {
        let upstream = FakeUpstream::answering_with_headers(
            "data: a\n\n",
            vec![
                ("content-type", "text/event-stream"),
                ("content-length", "9"),
            ],
        );
        let gateway = Gateway::new(table(), ProcessorChain::new(), upstream);
        let response = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap();
        assert_eq!(response.headers().get(CONTENT_LENGTH).unwrap(), "9");
        assert_eq!(body_of(response).await, "data: a\n\n");
    }

    #[tokio::test]
    async fn with_no_body_plugin_the_body_is_never_read() {
        let upstream = FakeUpstream::answering("pong");
        let processors = ProcessorChain::new();
        assert!(processors.passes_request_body());
        assert!(processors.passes_response_body());
        let gateway = Gateway::new(table(), processors, upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", "payload"))
            .await
            .unwrap();
        assert_eq!(upstream.last().body, Bytes::from("payload"));
    }

    #[tokio::test]
    async fn a_response_body_plugin_transforms_what_goes_back() {
        let processors = ProcessorChain::new().with_response_body(Marker::at(10, "-seen"));
        let gateway = Gateway::new(table(), processors, FakeUpstream::answering("pong"));
        let response = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap();
        assert_eq!(body_of(response).await, "pong-seen");
    }

    #[tokio::test]
    async fn plugins_run_highest_order_first() {
        let upstream = FakeUpstream::answering("pong");
        let processors = ProcessorChain::new()
            .with_request_body(Marker::at(10, "-low"))
            .with_request_body(Marker::at(200, "-high"));
        let gateway = Gateway::new(table(), processors, upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", "body"))
            .await
            .unwrap();
        assert_eq!(upstream.last().body, Bytes::from("body-high-low"));
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
        let flow = flow.forward(upstream.as_ref()).await.unwrap();
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
}
