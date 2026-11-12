//! A cluster member that is not one, for tests of the code that dispatches to a member.
//!
//! Behind the `testing` feature, so nothing that ships carries it.
//!
//! It answers a real event stream over a real loopback socket rather than standing in for the
//! client, because what a test of a dispatch has to assert is what crossed the wire: the secret
//! presented, the work serialized, and an event reassembled out of the chunks it arrived in. A
//! double that replaced the client would be a double of the thing under test.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use http::{Request, Response, StatusCode, header};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use ip_agents::{AgentId, ChosenModel};
use ip_config::Secret;
use ip_core::route::ClusterAddress;
use ip_core::{Money, TenantId, TokenCount, Tokens, UserId};
use ip_graph::{GraphId, GraphOwner, GraphRunId, NodeName, NodeRan};
use ip_run::{Requested, RunEvent, Spend, Stopped, TurnNumber};

use crate::client::{ClusterClient, ClusterSettings, MAX_EVENT_BYTES};
use crate::dispatch::Dispatch;
use crate::event::NodeEvent;

/// The secret a stub requires, and the thing no message of its own may ever carry.
pub const STUB_SECRET: &str = "cluster-live-6b20d8e4a1f3";

/// How long a dispatch to a stub may go quiet, which is a local socket and so is short.
const STUB_IDLE: Duration = Duration::from_millis(300);

/// How long a slow stub waits before each event, which is under [`STUB_IDLE`] while the three of
/// them together are well over it.
const SLOW_GAP: Duration = Duration::from_millis(200);

/// How big a chunk a stub sends, chosen to fall inside an event rather than between two.
const CHUNK_BYTES: usize = 17;

/// What a stub saw of one dispatch.
#[derive(Debug, Clone)]
pub struct Seen {
    /// The path it was asked on.
    pub path: String,
    /// The work it was handed.
    pub work: serde_json::Value,
    /// What was presented, if anything.
    pub authorization: Option<String>,
}

/// Everything a stub saw, in the order it saw it.
type Record = Arc<Mutex<Vec<Seen>>>;

/// The body a stub answers with: its chunks, each after however long it waits.
type Chunks = Vec<(Duration, Bytes)>;

/// The streaming body every stub answers over.
type StubBody = StreamBody<BoxStream<'static, Result<Frame<Bytes>, Infallible>>>;

/// How a stub answers. Every one of these is a member somebody really runs.
#[derive(Debug, Clone, Copy)]
pub enum Answers {
    /// The node ran: two events and an answer, in chunks that fall inside events.
    Ran,
    /// The same, an event at a time, slowly enough that no one clock covers the whole run.
    Slowly,
    /// It would not run the node, and says so instead of answering.
    Refused,
    /// It says one thing and then stops, as a member that died mid-run leaves it.
    CutOff,
    /// It sends an event whose data is not one.
    Garbage,
    /// It opens an event and goes on filling it, never ending it.
    Unending,
    /// It answers 401, as a member holding a different secret does.
    Unauthorized,
}

/// One node's work, as a test that dispatches one builds it.
///
/// # Panics
///
/// If its own names are not ones, which is the fixture failing rather than the code.
#[must_use]
pub fn stub_dispatch() -> Dispatch {
    Dispatch {
        run: GraphRunId::generate().expect("the fixture's own run id"),
        node: NodeName::new("triage").expect("the fixture's own node name"),
        graph: GraphId::new("support").expect("the fixture's own graph id"),
        owner: GraphOwner::Tenant(TenantId::new("acme").expect("the fixture's own tenant id")),
        tenant: TenantId::new("acme").expect("the fixture's own tenant id"),
        user: UserId::new("dana").expect("the fixture's own user id"),
        agent: AgentId::new("summarise").expect("the fixture's own agent id"),
        requested: Requested {
            model: Some(
                ChosenModel::try_from("gpt-4o".to_owned()).expect("the fixture's own model"),
            ),
            api: None,
        },
        input: "{\"intake\":\"41 crates missing\"}".to_owned(),
    }
}

/// What a stub worker says about a node that ran, from first word to last.
#[must_use]
pub fn stub_stream() -> Vec<NodeEvent> {
    vec![
        NodeEvent::Said(RunEvent::Turn {
            number: TurnNumber::FIRST,
            model: None,
        }),
        NodeEvent::Said(RunEvent::Text {
            text: "Looking it up.".to_owned(),
        }),
        NodeEvent::Answered(stub_answer()),
    ]
}

/// What a stub worker says a node came to.
#[must_use]
pub fn stub_answer() -> NodeRan {
    NodeRan {
        answer: "41 crates in bay 3.".to_owned(),
        stopped: Stopped::Answered,
        spend: Spend::new(
            Tokens::new(TokenCount::new(120), TokenCount::new(34)),
            Money::from_micros(5_600),
        ),
        traces: Vec::new(),
    }
}

/// What a stub worker says when it will not run a node.
pub const STUB_REFUSAL: &str = "no template named summarise here";

/// A stub cluster member, listening for as long as the process runs.
pub struct StubWorker {
    address: SocketAddr,
    record: Record,
}

impl StubWorker {
    /// Starts a stub answering as `answers` says, on a loopback port it picked itself.
    ///
    /// # Panics
    ///
    /// If its own socket will not bind, which is the fixture failing rather than the code.
    pub async fn start(answers: Answers) -> Self {
        let record: Record = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the fixture's own socket binds");
        let address = listener
            .local_addr()
            .expect("a bound socket has an address");
        let kept = Arc::clone(&record);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let kept = Arc::clone(&kept);
                tokio::spawn(async move {
                    let _ = http1::Builder::new()
                        .serve_connection(
                            TokioIo::new(stream),
                            service_fn(move |request| answer(Arc::clone(&kept), answers, request)),
                        )
                        .await;
                });
            }
        });
        Self { address, record }
    }

    /// Where it is listening, as a member of a cluster names itself.
    ///
    /// # Panics
    ///
    /// If a loopback socket is not an address, which is the fixture failing rather than the code.
    pub fn address(&self) -> ClusterAddress {
        ClusterAddress::of(self.address)
            .expect("a loopback address is one")
            .expect("a loopback address names an interface")
    }

    /// A client holding this cluster's own secret and a short clock.
    #[must_use]
    pub fn client(&self) -> ClusterClient {
        ClusterClient::new(stub_settings())
    }

    /// Everything it saw, in the order it saw it.
    ///
    /// # Panics
    ///
    /// If a previous panic poisoned the record.
    pub fn seen(&self) -> Vec<Seen> {
        self.record
            .lock()
            .expect("the record is not poisoned")
            .clone()
    }

    /// The one dispatch it saw.
    ///
    /// # Panics
    ///
    /// If it saw any other number, saying how many, since a test that reads "the" dispatch has
    /// assumed there was one.
    pub fn only(&self) -> Seen {
        let seen = self.seen();
        assert_eq!(seen.len(), 1, "one dispatch was expected");
        seen.into_iter().next().expect("a record of one holds one")
    }
}

/// A socket that accepts a connection and answers nothing, so a client's own clock is what ends
/// the dispatch.
pub struct SilentMember {
    address: SocketAddr,
}

impl SilentMember {
    /// Starts one, on a loopback port it picked itself.
    ///
    /// # Panics
    ///
    /// If its own socket will not bind.
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the fixture's own socket binds");
        let address = listener
            .local_addr()
            .expect("a bound socket has an address");
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        Self { address }
    }

    /// Where it is listening.
    ///
    /// # Panics
    ///
    /// If a loopback socket is not an address.
    pub fn address(&self) -> ClusterAddress {
        ClusterAddress::of(self.address)
            .expect("a loopback address is one")
            .expect("a loopback address names an interface")
    }

    /// A client bound to this silence.
    #[must_use]
    pub fn client(&self) -> ClusterClient {
        ClusterClient::new(stub_settings())
    }
}

/// An address nothing is listening on, which is what a member that died looks like from here.
///
/// Bound and let go, so the port was real and is now nobody's.
///
/// # Panics
///
/// If its own socket will not bind, or a loopback socket is not an address.
pub async fn vacant_address() -> ClusterAddress {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the fixture's own socket binds");
    let address = listener
        .local_addr()
        .expect("a bound socket has an address");
    drop(listener);
    ClusterAddress::of(address)
        .expect("a loopback address is one")
        .expect("a loopback address names an interface")
}

/// How a client reaches a stub: this cluster's secret and a short clock.
#[must_use]
pub fn stub_settings() -> ClusterSettings {
    ClusterSettings {
        secret: Secret::from(STUB_SECRET),
        idle: STUB_IDLE,
    }
}

async fn answer(
    record: Record,
    answers: Answers,
    request: Request<hyper::body::Incoming>,
) -> Result<Response<StubBody>, Infallible> {
    let authorization = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let path = request.uri().path().to_owned();
    let body = request
        .into_body()
        .collect()
        .await
        .map(|collected| collected.to_bytes())
        .unwrap_or_default();
    record
        .lock()
        .expect("the record is not poisoned")
        .push(Seen {
            path,
            work: serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
            authorization,
        });

    let built = Response::builder();
    Ok(match answers {
        Answers::Unauthorized => streamed(built.status(StatusCode::UNAUTHORIZED), Vec::new()),
        _ => streamed(built, chunks(answers)),
    })
}

/// What one answer sends, and how long it waits before each piece of it.
fn chunks(answers: Answers) -> Chunks {
    match answers {
        Answers::Ran => split(&written(&stub_stream())),
        Answers::Slowly => stub_stream()
            .iter()
            .map(|event| (SLOW_GAP, Bytes::from(written(std::slice::from_ref(event)))))
            .collect(),
        Answers::Refused => split(&written(&[NodeEvent::Refused(STUB_REFUSAL.to_owned())])),
        Answers::CutOff => split(&written(&stub_stream()[..1])),
        Answers::Garbage => vec![(
            Duration::ZERO,
            Bytes::from_static(b"event: said\ndata: {\"nope\":1}\n\n"),
        )],
        Answers::Unending => vec![(
            Duration::ZERO,
            Bytes::from(format!("data: {}", "x".repeat(MAX_EVENT_BYTES + 1))),
        )],
        Answers::Unauthorized => Vec::new(),
    }
}

/// Events as a stream carries them: the name for a person, the whole event for a reader.
fn written(events: &[NodeEvent]) -> String {
    events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {}\n\n",
                event.name(),
                serde_json::to_string(event).expect("the fixture's own event writes down")
            )
        })
        .collect()
}

/// A stream cut into chunks that fall inside events, since that is how one really arrives.
fn split(text: &str) -> Chunks {
    text.as_bytes()
        .chunks(CHUNK_BYTES)
        .map(|chunk| (Duration::ZERO, Bytes::copy_from_slice(chunk)))
        .collect()
}

fn streamed(built: http::response::Builder, chunks: Chunks) -> Response<StubBody> {
    let stream = futures_util::stream::unfold(chunks.into_iter(), |mut left| async move {
        let (wait, chunk) = left.next()?;
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        Some((Ok(Frame::data(chunk)), left))
    });
    built
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(StreamBody::new(stream.boxed()))
        .expect("the fixture's own response builds")
}
