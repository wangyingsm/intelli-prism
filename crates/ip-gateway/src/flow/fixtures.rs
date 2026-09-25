//! What the dataflow's tests are built out of: an upstream that answers, plugins that mark
//! what they touched, and one tenant's request.

use bytes::Bytes;
use http::header::HOST;
use http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode};
use http_body_util::BodyExt;
use ip_auth::{Authority, Identity};
use ip_cache::Ttl;
use ip_core::{
    AbsPath, ApiId, Capability, CapabilityScope, Endpoint, Grant, Grants, Host, PluginOrder, Port,
    Protocol, Role, RouteKey, RouteRule, RouteTarget, TenantId, TraceId, UserId,
};
use std::sync::{Arc, Mutex};

use super::{Gateway, RequestContext};
use crate::body::{GatewayBody, from_bytes};
use crate::cache::ResponseCache;
use crate::error::ProcessorError;
use crate::processor::{BodyProcessor, HeaderProcessor, ProcessorChain};
use crate::table::RoutingTable;
use crate::upstream::Upstream;
use ip_storage::{NewUsage, StorageError, Usage, UsageRowId, UsageStore};

/// Records what it was sent, and answers with what it was built with.
pub(super) struct FakeUpstream {
    pub(super) seen: Mutex<Vec<Seen>>,
    status: StatusCode,
    body: Bytes,
    headers: Vec<(&'static str, &'static str)>,
    fail: bool,
}

pub(super) struct Seen {
    pub(super) uri: String,
    pub(super) host: String,
    pub(super) headers: HeaderMap,
    pub(super) body: Bytes,
}

impl FakeUpstream {
    pub(super) fn answering(body: &str) -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            status: StatusCode::OK,
            body: Bytes::from(body.to_owned()),
            headers: Vec::new(),
            fail: false,
        })
    }

    /// Answers with a status of its own, so what is kept can be told from what is not.
    pub(super) fn answering_with(status: StatusCode, body: &str) -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            status,
            body: Bytes::from(body.to_owned()),
            headers: Vec::new(),
            fail: false,
        })
    }

    pub(super) fn failing() -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            status: StatusCode::OK,
            body: Bytes::new(),
            headers: Vec::new(),
            fail: true,
        })
    }

    pub(super) fn answering_with_headers(
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

    pub(super) fn last(&self) -> Seen {
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
pub(super) struct Marker {
    order: PluginOrder,
    mark: &'static str,
}

impl Marker {
    pub(super) fn at(order: u8, mark: &'static str) -> Arc<Self> {
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

pub(super) struct SetHeader {
    name: HeaderName,
    value: &'static str,
}

impl SetHeader {
    pub(super) fn new(name: &'static str, value: &'static str) -> Arc<Self> {
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
pub(super) struct Stop(pub(super) ProcessorError);

#[async_trait::async_trait]
impl HeaderProcessor for Stop {
    fn order(&self) -> PluginOrder {
        PluginOrder::new(200)
    }

    async fn process(&self, _: &mut HeaderMap) -> Result<(), ProcessorError> {
        Err(self.0.clone())
    }
}

pub(super) fn user() -> UserId {
    UserId::new("alice").unwrap()
}

pub(super) fn tenant() -> TenantId {
    TenantId::new("acme").unwrap()
}

pub(super) fn api() -> ApiId {
    ApiId::new("anthropic").unwrap()
}

pub(super) fn endpoint(protocol: Protocol, host: &str, port: u16, path: &str) -> Endpoint {
    Endpoint::new(
        protocol,
        Host::new(host).unwrap(),
        Port::new(port).unwrap(),
        AbsPath::new(path).unwrap(),
    )
}

pub(super) fn rule(target: Endpoint) -> RouteRule {
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

pub(super) fn table() -> RoutingTable {
    RoutingTable::from_rules(vec![rule(endpoint(
        Protocol::Https,
        "api.example.com",
        443,
        "/v1",
    ))])
}

pub(super) fn granted() -> Authority {
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

pub(super) fn ungranted() -> Authority {
    Authority::new(identity(), Grants::new())
}

pub(super) fn identity() -> Identity {
    Identity {
        user: user(),
        tenant: tenant(),
        role: Role::Member,
    }
}

pub(super) fn context(authority: Authority) -> RequestContext {
    RequestContext {
        authority,
        protocol: Protocol::Http,
        listen: "127.0.0.1:8080".parse().unwrap(),
        trace: TraceId::generate().unwrap(),
        turn: None,
    }
}

pub(super) fn request(path_and_query: &str, body: &str) -> Request<GatewayBody> {
    Request::builder()
        .uri(path_and_query)
        .header(HOST, "gateway.local:8080")
        .body(from_bytes(Bytes::from(body.to_owned())))
        .unwrap()
}

pub(super) async fn body_of(response: Response<GatewayBody>) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// A gateway that answers repeated requests out of a cache of its own.
pub(super) fn caching_gateway(upstream: Arc<FakeUpstream>) -> Gateway {
    let cache = Arc::new(ip_cache::SledCache::temporary().unwrap());
    Gateway::new(table(), ProcessorChain::new(), upstream)
        .caching(ResponseCache::new(cache, Ttl::seconds(60).unwrap()))
}

/// Another tenant asking the same route the same thing.
pub(super) fn other_tenant() -> Authority {
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

/// Keeps every usage row it is given, so a test can wait for one.
pub(super) struct Ledger {
    written: tokio::sync::mpsc::UnboundedSender<Usage>,
    next_row: std::sync::atomic::AtomicI64,
}

impl Ledger {
    /// A store and the end a test reads what was recorded from.
    pub(super) fn new() -> (Arc<Self>, tokio::sync::mpsc::UnboundedReceiver<Usage>) {
        let (written, read) = tokio::sync::mpsc::unbounded_channel();
        (
            Arc::new(Self {
                written,
                next_row: std::sync::atomic::AtomicI64::new(1),
            }),
            read,
        )
    }
}

#[async_trait::async_trait]
impl UsageStore for Ledger {
    async fn record_usage(&self, usage: NewUsage) -> Result<Usage, StorageError> {
        let row_id = UsageRowId::new(
            self.next_row
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        );
        let stored = Usage {
            row_id,
            trace: usage.trace,
            turn: usage.turn,
            tenant: usage.tenant,
            user: usage.user,
            api: usage.api,
            model: usage.model,
            tokens: usage.tokens,
            served: usage.served,
            latency: usage.latency,
            created_at: ip_core::Timestamp::now(),
        };
        let _ = self.written.send(stored.clone());
        Ok(stored)
    }

    async fn usage(&self, _: UsageRowId) -> Result<Option<Usage>, StorageError> {
        Ok(None)
    }

    async fn sweep_usage(&self, _: ip_core::Timestamp) -> Result<u64, StorageError> {
        Ok(0)
    }

    async fn spent(
        &self,
        _: &ip_storage::UsageFilter,
        _: ip_core::Counted,
        _: ip_core::Timestamp,
    ) -> Result<u64, StorageError> {
        Ok(0)
    }
}

/// An upstream that answers a stream of events, one frame each.
pub(super) struct Streaming(pub(super) &'static [&'static str]);

#[async_trait::async_trait]
impl Upstream for Streaming {
    async fn send(
        &self,
        _: Request<GatewayBody>,
    ) -> Result<Response<GatewayBody>, crate::error::UpstreamError> {
        let (sender, receiver) = tokio::sync::mpsc::channel(self.0.len() + 1);
        for part in self.0 {
            sender
                .send(Ok(http_body::Frame::data(Bytes::from_static(
                    part.as_bytes(),
                ))))
                .await
                .unwrap();
        }
        drop(sender);
        Ok(Response::builder()
            .header(http::header::CONTENT_TYPE, "text/event-stream")
            .body(http_body_util::BodyExt::boxed_unsync(
                crate::body::ChannelBody::new(receiver),
            ))
            .unwrap())
    }
}
