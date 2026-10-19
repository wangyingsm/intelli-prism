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
    Protocol, Role, RouteKey, RouteRule, RouteTarget, TenantId, TraceId, TurnId, UserId,
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

/// What one host answers with when a race reaches it: how long it keeps the caller waiting,
/// with what it answers, or that it does not answer at all.
#[derive(Clone)]
pub(super) struct Behaviour {
    pub(super) delay: std::time::Duration,
    pub(super) status: StatusCode,
    pub(super) body: &'static str,
    pub(super) fail: bool,
}

/// Records what it was sent, and answers with what it was built with.
pub(super) struct FakeUpstream {
    pub(super) seen: Mutex<Vec<Seen>>,
    status: StatusCode,
    body: Bytes,
    headers: Vec<(&'static str, &'static str)>,
    fail: bool,
    behaviours: Mutex<Vec<(String, Behaviour)>>,
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
            behaviours: Mutex::new(Vec::new()),
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
            behaviours: Mutex::new(Vec::new()),
        })
    }

    pub(super) fn failing() -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            status: StatusCode::OK,
            body: Bytes::new(),
            headers: Vec::new(),
            fail: true,
            behaviours: Mutex::new(Vec::new()),
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
            behaviours: Mutex::new(Vec::new()),
        })
    }

    /// Answers per endpoint: a host named here answers on its own timing, the rest as built.
    pub(super) fn answering_each(named: &[(&str, Behaviour)]) -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            status: StatusCode::OK,
            body: Bytes::new(),
            headers: Vec::new(),
            fail: false,
            behaviours: Mutex::new(
                named
                    .iter()
                    .map(|(host, behaviour)| (String::from(*host), behaviour.clone()))
                    .collect(),
            ),
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
        let behaviour = self
            .behaviours
            .lock()
            .unwrap()
            .iter()
            .find(|(named, _)| *named == host)
            .map(|(_, behaviour)| behaviour.clone());
        self.seen.lock().unwrap().push(Seen {
            uri,
            host,
            headers,
            body,
        });
        let (status, answered) = match behaviour {
            Some(behaviour) if behaviour.fail => {
                return Err(crate::error::UpstreamError::new("connection refused"));
            }
            Some(behaviour) => {
                if !behaviour.delay.is_zero() {
                    tokio::time::sleep(behaviour.delay).await;
                }
                (behaviour.status, Bytes::from(behaviour.body.to_owned()))
            }
            None => (self.status, self.body.clone()),
        };
        let mut response = Response::builder().status(status);
        for (name, value) in &self.headers {
            response = response.header(*name, *value);
        }
        Ok(response.body(from_bytes(answered)).unwrap())
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
        strategy: ip_core::Strategy::default(),
        hedge: ip_core::Hedge::default(),
        hedge_delay: ip_core::HedgeDelay::default(),
        caller_speaks: None,
        upstream_speaks: None,
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

/// A table whose one rule bridges two families, or neither when both are the same.
pub(super) fn bridging_table(caller: ip_core::Dialect, upstream: ip_core::Dialect) -> RoutingTable {
    let mut rule = rule(endpoint(Protocol::Https, "api.example.com", 443, "/v1"));
    rule.caller_speaks = Some(caller);
    rule.upstream_speaks = Some(upstream);
    RoutingTable::from_rules(vec![rule])
}

/// A table whose one rule stands behind two endpoints, dispatched to by `strategy`.
pub(super) fn replicated_table(strategy: ip_core::Strategy) -> RoutingTable {
    let mut rule = rule(endpoint(Protocol::Https, "one.example.com", 443, "/v1"));
    rule.strategy = strategy;
    rule.target = RouteTarget::from_endpoints(vec![
        endpoint(Protocol::Https, "one.example.com", 443, "/v1"),
        endpoint(Protocol::Https, "two.example.com", 443, "/v1"),
    ])
    .unwrap();
    RoutingTable::from_rules(vec![rule])
}

/// A judge that answers a fixed class for every text handed to it, counting the calls
/// and remembering the last one — the flow tests must not need an artefact, and "was the
/// turn classified once?" is a question about counts, not about weights.
#[derive(Debug, Default)]
pub struct CountingClassifier {
    /// The class every text gets; `None` is a judge that never commits.
    pub answer: std::sync::Mutex<Option<ip_core::Difficulty>>,
    calls: std::sync::atomic::AtomicUsize,
    /// The last text handed over, for the leak assertions.
    pub last: std::sync::Mutex<String>,
}

impl CountingClassifier {
    /// A judge that names `class` for everything and says how often it was asked.
    #[must_use]
    pub fn naming(class: ip_core::Difficulty) -> Self {
        Self {
            answer: std::sync::Mutex::new(Some(class)),
            calls: std::sync::atomic::AtomicUsize::new(0),
            last: std::sync::Mutex::new(String::new()),
        }
    }

    /// A judge that refuses everything: the shape of an error or a sub-margin answer,
    /// which the flow must read as "unclassified" and move on.
    #[must_use]
    pub fn silent() -> Self {
        Self::default()
    }

    /// How many times it has been asked.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl crate::Classifier for CountingClassifier {
    fn classify(&self, text: &str) -> Option<ip_core::Difficulty> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *self.last.lock().unwrap() = text.to_owned();
        *self.answer.lock().unwrap()
    }
}

/// A table whose one rule stands behind the hosts named, each capped at the class named
/// alongside it; an endpoint that serves a class serves every class below it.
pub(super) fn capped_table(behind: &[(&str, ip_core::Difficulty)]) -> RoutingTable {
    let mut rule = rule(endpoint(
        Protocol::Https,
        behind.first().expect("a rule stands behind something").0,
        443,
        "/v1",
    ));
    rule.target = RouteTarget::from_weighted(
        behind
            .iter()
            .map(|(host, ceiling)| ip_core::Weighted {
                endpoint: endpoint(Protocol::Https, host, 443, "/v1"),
                serves: Some(*ceiling),
                ..ip_core::Weighted::new(endpoint(Protocol::Https, host, 443, "/v1"))
            })
            .collect(),
    )
    .unwrap();
    RoutingTable::from_rules(vec![rule])
}

/// A table whose one rule stands behind two endpoints, dispatched to by `strategy`, and raced
/// across `hedge` of them.
pub(super) fn hedged_replicated_table(
    strategy: ip_core::Strategy,
    hedge: ip_core::Hedge,
) -> RoutingTable {
    let mut rule = rule(endpoint(Protocol::Https, "one.example.com", 443, "/v1"));
    rule.strategy = strategy;
    rule.hedge = hedge;
    rule.target = RouteTarget::from_endpoints(vec![
        endpoint(Protocol::Https, "one.example.com", 443, "/v1"),
        endpoint(Protocol::Https, "two.example.com", 443, "/v1"),
    ])
    .unwrap();
    RoutingTable::from_rules(vec![rule])
}

/// A table whose one rule stands behind the hosts named, raced `hedge` of them at a time.
pub(super) fn hedged_table(
    strategy: ip_core::Strategy,
    hedge: ip_core::Hedge,
    behind: &[&str],
) -> RoutingTable {
    let mut rule = rule(endpoint(
        Protocol::Https,
        behind.first().expect("a rule stands behind something"),
        443,
        "/v1",
    ));
    rule.strategy = strategy;
    rule.hedge = hedge;
    rule.target = RouteTarget::from_endpoints(
        behind
            .iter()
            .map(|host| endpoint(Protocol::Https, host, 443, "/v1"))
            .collect(),
    )
    .unwrap();
    RoutingTable::from_rules(vec![rule])
}

/// A table whose one rule stands behind two endpoints and waits `delay_millis` between
/// the arms of a raced request.
pub(super) fn delayed_table(delay_millis: u16) -> RoutingTable {
    let mut rule = rule(endpoint(Protocol::Https, "one.example.com", 443, "/v1"));
    rule.hedge = ip_core::Hedge::new(2).unwrap();
    rule.hedge_delay = ip_core::HedgeDelay::new(delay_millis);
    rule.target = RouteTarget::from_endpoints(vec![
        endpoint(Protocol::Https, "one.example.com", 443, "/v1"),
        endpoint(Protocol::Https, "two.example.com", 443, "/v1"),
    ])
    .unwrap();
    RoutingTable::from_rules(vec![rule])
}

/// A table whose one rule stands behind the hosts named, each with the ceiling given.
pub(super) fn ceiled_table(behind: &[(&str, Option<ip_core::Difficulty>)]) -> RoutingTable {
    let mut rule = rule(endpoint(
        Protocol::Https,
        behind.first().expect("a rule stands behind something").0,
        443,
        "/v1",
    ));
    rule.target = RouteTarget::from_weighted(
        behind
            .iter()
            .map(|(host, serves)| {
                ip_core::Weighted::serving(
                    endpoint(Protocol::Https, host, 443, "/v1"),
                    ip_core::Weight::default(),
                    *serves,
                )
            })
            .collect(),
    )
    .unwrap();
    RoutingTable::from_rules(vec![rule])
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
        // Unclassified until forward says otherwise, which is where the class is read from.
    }
}

/// The context of a request that marked a chat turn.
pub(super) fn in_turn(authority: Authority, turn: &str) -> RequestContext {
    RequestContext {
        turn: Some(TurnId::new(turn).unwrap()),
        ..context(authority)
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

/// An encoder a test can state the meaning of.
///
/// A text means whichever vector's phrase it contains, and nothing when it contains
/// none — so a test says in one line which two questions mean the same thing, instead
/// of depending on what some real model happens to think of them.
#[derive(Debug, Default)]
pub(super) struct FakeEmbedder {
    meanings: Vec<(String, Vec<f32>)>,
}

impl FakeEmbedder {
    /// Any text holding `phrase` means `vector`. Earliest match wins.
    pub(super) fn saying(mut self, phrase: &str, vector: &[f32]) -> Self {
        self.meanings.push((phrase.to_owned(), vector.to_vec()));
        self
    }
}

impl crate::embedder::Embedder for FakeEmbedder {
    fn embed(&self, text: &str) -> Option<ip_embed::Embedding> {
        let (_, vector) = self
            .meanings
            .iter()
            .find(|(phrase, _)| text.contains(phrase.as_str()))?;
        ip_embed::Embedding::new(vector).ok()
    }
}

/// An encoder that never answers in time, for the arm where the deadline decides.
///
/// It sleeps well past the deadline rather than a hair over it, so what the test
/// measures is the deadline firing and never a race against the clock on a loaded
/// box. Only a few times over, though: a blocking task cannot be cancelled, so the
/// test's runtime waits out this whole sleep on its way down however early the
/// timeout returned, and the multiple is what that costs the suite.
#[derive(Debug)]
pub(super) struct SlowEmbedder;

impl crate::embedder::Embedder for SlowEmbedder {
    fn embed(&self, _text: &str) -> Option<ip_embed::Embedding> {
        std::thread::sleep(crate::embedder::EMBED_DEADLINE * 4);
        ip_embed::Embedding::new(&[1.0, 0.0, 0.0]).ok()
    }
}

/// The semantic level at a stated threshold, over a cache of its own.
pub(super) fn recognising(nearness: f32) -> Arc<crate::semantic::SemanticCache> {
    Arc::new(crate::semantic::SemanticCache::new(
        Arc::new(ip_cache::SledCache::temporary().unwrap()),
        crate::semantic::SemanticSettings {
            nearness: ip_embed::Nearness::new(nearness).unwrap(),
            ttl: Ttl::seconds(60).unwrap(),
            width: ip_embed::Width::new(3).unwrap(),
            capacity: ip_embed::Capacity::new(64).unwrap(),
            model: "fake-model".to_owned(),
        },
    ))
}

/// One question, written the way a client writes one.
pub(super) fn asking(question: &str) -> String {
    serde_json::json!({"messages": [{"role": "user", "content": question}]}).to_string()
}

/// The same question with an answer already in the body, which is no longer one
/// question however near its words are to another's.
pub(super) fn asking_again(question: &str) -> String {
    serde_json::json!({"messages": [
        {"role": "user", "content": question},
        {"role": "assistant", "content": "an earlier answer"},
        {"role": "user", "content": question},
    ]})
    .to_string()
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
            served_by: usage.served_by,
            raced_across: usage.raced_across,
            dispatched_class: usage.dispatched_class,
            decided_by: usage.decided_by,
            redacted: usage.redacted,
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

    async fn sent_upstream(
        &self,
        _: &ip_storage::UsageFilter,
        _: Option<ip_core::Timestamp>,
    ) -> Result<ip_storage::SpentArms, StorageError> {
        Ok(ip_storage::SpentArms {
            decided: ip_storage::Decided::default(),
            answered: 0,
            sent: 0,
        })
    }
}

/// An upstream that answers a stream of events, one frame each. Frames are bytes
/// rather than text because a real one may split a character in half.
pub(super) struct Streaming(pub(super) &'static [&'static [u8]]);

#[async_trait::async_trait]
impl Upstream for Streaming {
    async fn send(
        &self,
        _: Request<GatewayBody>,
    ) -> Result<Response<GatewayBody>, crate::error::UpstreamError> {
        let (sender, receiver) = tokio::sync::mpsc::channel(self.0.len() + 1);
        for part in self.0 {
            sender
                .send(Ok(http_body::Frame::data(Bytes::from_static(part))))
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
