use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http::header::HOST;
use http::uri::{Authority as UriAuthority, Scheme};
use http::{HeaderMap, Request, Response, Uri};
use http_body_util::BodyExt;
use ip_auth::Authority;
use ip_core::{Capability, CapabilityScope, Endpoint, Protocol, RouteKey};

use crate::body::{GatewayBody, from_bytes};
use crate::error::{GatewayError, GatewayErrorKind};
use crate::processor::{BodyProcessor, HeaderProcessor, ProcessorChain};
use crate::stage::Stage;
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

    /// Carries one request through every stage.
    pub async fn handle(
        &self,
        context: RequestContext,
        mut request: Request<GatewayBody>,
    ) -> Result<Response<GatewayBody>, GatewayError> {
        let key = self.key_of(&context, &request)?;

        run_headers(
            Stage::HeaderProcess,
            self.processors.request_headers(),
            request.headers_mut(),
        )
        .await?;

        let resolution = self.authorize(&context, &key)?;

        let body = run_body(
            Stage::BodyProcess,
            self.processors.request_body(),
            request.body_mut(),
            self.processors.passes_request_body(),
        )
        .await?;

        let forwarded = self.forward_request(request, body, resolution.primary())?;
        let mut response = self
            .upstream
            .send(forwarded)
            .await
            .map_err(|error| GatewayError::new(Stage::Route, error.into()))?;

        run_headers(
            Stage::ResponseHeaderProcess,
            self.processors.response_headers(),
            response.headers_mut(),
        )
        .await?;

        let body = run_body(
            Stage::ResponseBodyProcess,
            self.processors.response_body(),
            response.body_mut(),
            self.processors.passes_response_body(),
        )
        .await?;

        Ok(match body {
            Some(bytes) => {
                let (parts, _) = response.into_parts();
                Response::from_parts(parts, from_bytes(bytes))
            }
            None => response,
        })
    }

    fn key_of(
        &self,
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
        .map_err(|error| GatewayError::new(Stage::HeaderRead, error.into()))
    }

    fn authorize(
        &self,
        context: &RequestContext,
        key: &RouteKey,
    ) -> Result<Resolution, GatewayError> {
        let resolution = self.table.resolve(key).ok_or_else(|| {
            GatewayError::new(
                Stage::Authorization,
                GatewayErrorKind::NoRoute { key: key.clone() },
            )
        })?;
        if let Some(target) = resolution
            .targets()
            .iter()
            .find(|target| !target.protocol.is_forwarded())
        {
            return Err(GatewayError::new(
                Stage::Authorization,
                GatewayErrorKind::ProtocolNotServed {
                    protocol: target.protocol,
                },
            ));
        }
        let identity = context.authority.identity();
        let scope = CapabilityScope::Api {
            user: identity.user.clone(),
            tenant: identity.tenant.clone(),
            api: resolution.rule().api.clone(),
        };
        if !context.authority.allows(Capability::ApiAccess, &scope) {
            return Err(GatewayError::new(
                Stage::Authorization,
                GatewayErrorKind::Forbidden {
                    capability: Capability::ApiAccess,
                },
            ));
        }
        Ok(resolution)
    }

    fn forward_request(
        &self,
        request: Request<GatewayBody>,
        body: Option<Bytes>,
        target: &Endpoint,
    ) -> Result<Request<GatewayBody>, GatewayError> {
        let query = request.uri().query().map(ToOwned::to_owned);
        let (mut parts, original) = request.into_parts();
        parts.uri = target_uri(target, query.as_deref()).map_err(|detail| {
            GatewayError::new(Stage::Route, GatewayErrorKind::Malformed { detail })
        })?;
        let host = format!("{}:{}", target.host, target.port);
        let host = host.parse().map_err(|_| {
            GatewayError::new(
                Stage::Route,
                GatewayErrorKind::Malformed {
                    detail: format!("{host} is not a host header"),
                },
            )
        })?;
        parts.headers.insert(HOST, host);
        Ok(Request::from_parts(
            parts,
            body.map_or(original, from_bytes),
        ))
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
    stage: Stage,
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
async fn run_body(
    stage: Stage,
    chain: &[Arc<dyn BodyProcessor>],
    body: &mut GatewayBody,
    passes: bool,
) -> Result<Option<Bytes>, GatewayError> {
    if passes {
        return Ok(None);
    }
    let read_stage = match stage {
        Stage::BodyProcess => Stage::BodyRead,
        _ => Stage::ResponseBodyRead,
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
    use crate::processor::{ProcessorError, ProcessorOrder};

    /// Records what it was sent, and answers with what it was built with.
    struct FakeUpstream {
        seen: Mutex<Vec<Seen>>,
        status: StatusCode,
        body: Bytes,
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
                fail: false,
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                seen: Mutex::new(Vec::new()),
                status: StatusCode::OK,
                body: Bytes::new(),
                fail: true,
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
        ) -> Result<Response<GatewayBody>, crate::upstream::UpstreamError> {
            if self.fail {
                return Err(crate::upstream::UpstreamError::new("connection refused"));
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
            Ok(Response::builder()
                .status(self.status)
                .body(from_bytes(self.body.clone()))
                .unwrap())
        }
    }

    /// Appends a marker so the order plugins ran in is visible in the output.
    struct Marker {
        order: ProcessorOrder,
        mark: &'static str,
    }

    impl Marker {
        fn at(order: u8, mark: &'static str) -> Arc<Self> {
            Arc::new(Self {
                order: ProcessorOrder::new(order),
                mark,
            })
        }
    }

    #[async_trait::async_trait]
    impl BodyProcessor for Marker {
        fn order(&self) -> ProcessorOrder {
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
        fn order(&self) -> ProcessorOrder {
            ProcessorOrder::new(10)
        }

        async fn process(&self, headers: &mut HeaderMap) -> Result<(), ProcessorError> {
            headers.insert(self.name.clone(), HeaderValue::from_static(self.value));
            Ok(())
        }
    }

    struct Refusing;

    #[async_trait::async_trait]
    impl HeaderProcessor for Refusing {
        fn order(&self) -> ProcessorOrder {
            ProcessorOrder::new(200)
        }

        async fn process(&self, _: &mut HeaderMap) -> Result<(), ProcessorError> {
            Err(ProcessorError::new("refused by policy"))
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
        assert_eq!(error.stage(), Stage::Authorization);
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
        assert_eq!(error.stage(), Stage::Route);
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
    async fn a_refusing_plugin_stops_the_flow_at_its_stage() {
        let processors = ProcessorChain::new().with_request_header(Arc::new(Refusing));
        let gateway = Gateway::new(table(), processors, FakeUpstream::answering("pong"));
        let error = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap_err();
        assert_eq!(error.stage(), Stage::HeaderProcess);
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
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

    #[test]
    fn the_reserved_order_range_is_the_first_sixty_four() {
        assert!(ProcessorOrder::new(0).is_primary());
        assert!(ProcessorOrder::new(63).is_primary());
        assert!(!ProcessorOrder::new(64).is_primary());
        assert!(!ProcessorOrder::new(255).is_primary());
    }
}
