use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http::header::{
    CONNECTION, HOST, HeaderName, PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, TE, TRAILER,
    TRANSFER_ENCODING, UPGRADE,
};
use http::uri::{Authority as UriAuthority, Scheme};
use http::{HeaderMap, Request, Response, Uri};
use http_body_util::BodyExt;
use ip_auth::Authority;
use ip_core::{Capability, CapabilityScope, Endpoint, Protocol, RouteKey};

use crate::body::{GatewayBody, from_bytes};
use crate::error::{GatewayError, GatewayErrorKind};
use crate::processor::{BodyProcessor, HeaderProcessor, ProcessorChain};
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
}

/// The request dataflow: the stages of `DESIGN.md` run in order.
///
/// Authentication happens before a request reaches here, so the flow starts at the
/// request header chain and carries the identity it was handed.
pub struct Gateway {
    table: RoutingTable,
    processors: ProcessorChain,
    upstream: Arc<dyn Upstream>,
}

impl Gateway {
    /// Builds the flow around a routing table, its plugin chains and an upstream.
    pub fn new(
        table: RoutingTable,
        processors: ProcessorChain,
        upstream: Arc<dyn Upstream>,
    ) -> Self {
        Self {
            table,
            processors,
            upstream,
        }
    }

    /// The rules this flow routes by.
    pub fn table(&self) -> &RoutingTable {
        &self.table
    }

    /// Carries one request through every stage, in the order the types allow.
    pub async fn handle(
        &self,
        context: RequestContext,
        request: Request<GatewayBody>,
    ) -> Result<Response<GatewayBody>, GatewayError> {
        Flow::received(context, request)
            .process_headers(self.processors.request_headers())
            .await?
            .authorize(&self.table)?
            .process_body(&self.processors)
            .await?
            .forward(self.upstream.as_ref())
            .await?
            .process_response_headers(self.processors.response_headers())
            .await?
            .process_response_body(&self.processors)
            .await?
            .into_response()
    }
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
/// # use ip_core::{Grants, Protocol, Role, TenantId, UserId};
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
/// #     }
/// # }
/// # async fn run(table: &RoutingTable, chain: &ProcessorChain) {
/// let request = Request::builder().uri("/x").body(empty()).unwrap();
/// let flow = Flow::received(context(), request);
/// let flow = flow.process_headers(chain.request_headers()).await.unwrap();
/// let flow = flow.authorize(table).unwrap();
/// # }
/// ```
///
/// Skipping one does not compile:
///
/// ```compile_fail
/// # use ip_gateway::{Flow, ProcessorChain, RoutingTable, RequestContext};
/// # use ip_gateway::body::empty;
/// # use ip_auth::{Authority, Identity};
/// # use ip_core::{Grants, Protocol, Role, TenantId, UserId};
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
/// #     }
/// # }
/// # async fn run(table: &RoutingTable, chain: &ProcessorChain) {
/// let request = Request::builder().uri("/x").body(empty()).unwrap();
/// let flow = Flow::received(context(), request);
/// // authorize belongs to Flow<HeadersProcessed>, not Flow<Received>.
/// let flow = flow.authorize(table).unwrap();
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

    /// Runs the request header chain.
    pub async fn process_headers(
        mut self,
        chain: &[Arc<dyn HeaderProcessor>],
    ) -> Result<Flow<HeadersProcessed>, GatewayError> {
        run_headers(HeadersProcessed::NAME, chain, self.held.headers_mut()).await?;
        Ok(Flow {
            context: self.context,
            held: self.held,
        })
    }
}

impl Flow<HeadersProcessed> {
    /// Resolves the route and checks the caller may use it.
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
            *self.held.request.body_mut() = from_bytes(bytes);
        }
        Ok(Flow {
            context: self.context,
            held: self.held,
        })
    }
}

impl Flow<BodyProcessed> {
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
        let body = run_body(
            ResponseBodyProcessed::NAME,
            processors.response_body(),
            self.held.body_mut(),
        )
        .await?;
        if let Some(bytes) = body {
            *self.held.body_mut() = from_bytes(bytes);
        }
        Ok(Flow {
            context: self.context,
            held: self.held,
        })
    }
}

impl Flow<ResponseBodyProcessed> {
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

/// Runs a body chain, or hands back `None` when the body may pass through untouched.
///
/// Whether it passes is read from the chain itself, so a body can only be buffered when
/// something is actually there to rewrite it.
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
        let processors = ProcessorChain::new();
        let flow = flow
            .process_headers(processors.request_headers())
            .await
            .unwrap();
        assert_eq!(flow.stage(), StageName::HeaderProcess);
        let flow = flow.authorize(&table()).unwrap();
        assert_eq!(flow.stage(), StageName::Authorization);
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
