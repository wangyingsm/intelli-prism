//! A collection that is not one, for tests of the code that talks to a collection.
//!
//! Behind the `testing` feature, so nothing that ships carries it.
//!
//! It answers real http over a real loopback socket rather than standing in for the client,
//! because what a test has to assert is the body that crossed the wire: a filter that reached
//! the store carrying a collection, or a look-up sent to the interface qdrant removed, are
//! exactly the mistakes a double of the client would hide.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::{Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use ip_config::Secret;
use ip_retrieve::RetrieverEndpoint;

use crate::client::{QdrantClient, QdrantSettings};

/// The key a stub requires, and the thing no message of its own may ever carry.
pub const STUB_KEY: &str = "qd-live-9f2c4a7b1e";

/// How long a call to a stub may take, which is a local socket and so is short.
const STUB_TIMEOUT: Duration = Duration::from_millis(300);

/// An answer longer than the client will read, by one byte.
const PAST_THE_CEILING: usize = 8 * 1024 * 1024 + 1;

/// What a stub saw of one call.
#[derive(Debug, Clone)]
pub struct Seen {
    /// Which method was used.
    pub method: String,
    /// What was asked for.
    pub path: String,
    /// What it was asked with, and null for a call with no body.
    pub body: serde_json::Value,
    /// What was presented, if anything.
    pub api_key: Option<String>,
}

/// Everything a stub saw, in the order it saw it.
type Record = Arc<Mutex<Vec<Seen>>>;

/// How a stub answers. Every one of these is a store somebody really runs.
#[derive(Debug, Clone)]
pub enum Answers {
    /// With a collection describing its vectors as given.
    Collection {
        /// What sits at `result.config.params.vectors`: one description, or a map of named ones.
        vectors: serde_json::Value,
    },
    /// With two hits where qdrant puts them, at `result.points`.
    Points,
    /// With hits at `result`, which is the shape of the interface qdrant removed.
    Flat,
    /// With a 404, as a store without that collection does.
    Absent,
    /// With a 400 and its own words, as a store handed a vector of the wrong width does.
    Wrong,
    /// With a 500 and nothing to say.
    Silent,
    /// With a 401, as a store with a different key does.
    Unauthorized,
    /// With a body that is not json at all.
    Garbage,
    /// With more than the client will read.
    Enormous,
}

/// A stub qdrant, listening for as long as the process runs.
pub struct StubQdrant {
    address: SocketAddr,
    record: Record,
}

impl StubQdrant {
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
                let answers = answers.clone();
                tokio::spawn(async move {
                    let _ = http1::Builder::new()
                        .serve_connection(
                            TokioIo::new(stream),
                            service_fn(move |request| {
                                answer(Arc::clone(&kept), answers.clone(), request)
                            }),
                        )
                        .await;
                });
            }
        });
        Self { address, record }
    }

    /// Where it is listening.
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// A client bound to this stub, with the stub's own credential.
    #[must_use]
    pub fn client(&self) -> QdrantClient {
        QdrantClient::new(stub_settings(self.address))
    }

    /// Everything it saw, in the order it saw it.
    ///
    /// # Panics
    ///
    /// If a previous panic poisoned the record.
    #[must_use]
    pub fn seen(&self) -> Vec<Seen> {
        self.record
            .lock()
            .expect("the record is not poisoned")
            .clone()
    }

    /// The one call it saw.
    ///
    /// # Panics
    ///
    /// If it saw any other number of calls, saying how many, since a test that reads "the" call
    /// has assumed there was one.
    #[must_use]
    pub fn only(&self) -> Seen {
        let seen = self.seen();
        assert_eq!(seen.len(), 1, "one call was expected");
        seen.into_iter().next().expect("a record of one holds one")
    }
}

/// A socket that accepts a connection and answers nothing, so a client's own clock is what ends
/// the call.
pub struct SilentQdrant {
    address: SocketAddr,
}

impl SilentQdrant {
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

    /// A client bound to this silence.
    #[must_use]
    pub fn client(&self) -> QdrantClient {
        QdrantClient::new(stub_settings(self.address))
    }
}

/// How a client reaches a stub: its address, the stub's key and a short clock.
///
/// # Panics
///
/// If the address will not make a url, which is the fixture failing rather than the code.
#[must_use]
pub fn stub_settings(address: SocketAddr) -> QdrantSettings {
    QdrantSettings {
        endpoint: RetrieverEndpoint::new(&format!("http://{address}"))
            .expect("the fixture's own endpoint is one"),
        key: Some(Secret::from(STUB_KEY)),
        timeout: STUB_TIMEOUT,
    }
}

async fn answer(
    record: Record,
    answers: Answers,
    request: Request<hyper::body::Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let api_key = request
        .headers()
        .get("api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let method = request.method().to_string();
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
            method,
            path,
            body: serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
            api_key,
        });

    let built = Response::builder();
    Ok(match answers {
        Answers::Collection { vectors } => json_body(
            built,
            &serde_json::json!({
                "status": "ok", "time": 0.001,
                "result": {"status": "green", "config": {"params": {"vectors": vectors}}},
            }),
        ),
        Answers::Points => json_body(
            built,
            &serde_json::json!({
                "status": "ok", "time": 0.002,
                "result": {"points": [
                    {"id": 42, "version": 3, "score": 0.75, "payload": {"title": "Leave"}},
                    {"id": "6b3f-7a", "version": 1, "score": 0.5, "payload": {"title": "Pay"}},
                ]},
            }),
        ),
        Answers::Flat => json_body(
            built,
            &serde_json::json!({
                "status": "ok", "time": 0.002,
                "result": [{"id": 42, "score": 0.75, "payload": {}}],
            }),
        ),
        Answers::Absent => refusal(
            built,
            StatusCode::NOT_FOUND,
            "Not found: Collection `handbook` doesn't exist!",
        ),
        Answers::Wrong => refusal(
            built,
            StatusCode::BAD_REQUEST,
            "Wrong input: Vector dimension error: expected dim: 384, got 3",
        ),
        Answers::Silent => built
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Full::new(Bytes::new()))
            .expect("the fixture's own response builds"),
        Answers::Unauthorized => built
            .status(StatusCode::UNAUTHORIZED)
            .body(Full::new(Bytes::new()))
            .expect("the fixture's own response builds"),
        Answers::Garbage => built
            .header(header::CONTENT_TYPE, "text/html")
            .body(Full::new(Bytes::from("<html>a proxy, not a store</html>")))
            .expect("the fixture's own response builds"),
        Answers::Enormous => json_body(
            built,
            &serde_json::json!({"result": {"points": [{"id": 1, "score": 1.0,
                "payload": {"text": "x".repeat(PAST_THE_CEILING)}}]}}),
        ),
    })
}

/// A refusal in qdrant's own envelope, whose `status` is an object where an answer's is the word
/// `ok`.
fn refusal(
    built: http::response::Builder,
    status: StatusCode,
    said: &str,
) -> Response<Full<Bytes>> {
    json_body(
        built.status(status),
        &serde_json::json!({"status": {"error": said}, "time": 0.000_041}),
    )
}

fn json_body(built: http::response::Builder, payload: &serde_json::Value) -> Response<Full<Bytes>> {
    built
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(payload.to_string())))
        .expect("the fixture's own response builds")
}
