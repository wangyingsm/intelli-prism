//! A store that is not one, for tests of the code that talks to a store.
//!
//! Behind the `testing` feature, so nothing that ships carries it.
//!
//! It answers real JSON-RPC over a real loopback socket rather than standing in for the client,
//! because what a test of graph code has to assert is the body that crossed the wire: a filter
//! missing from a read plan is the whole of this milestone's isolation risk, and a double that
//! replaced the client would be a double of the thing under test.

use std::collections::BTreeMap;
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

use crate::client::{DrsgClient, DrsgSettings};
use crate::plane::{DrsgEndpoint, PlaneName};

/// The credential a stub requires, and the thing no message of its own may ever carry.
pub const STUB_TOKEN: &str = "drsg-live-4f91ca7b22e8";

/// How long a call to a stub may take, which is a local socket and so is short.
const STUB_TIMEOUT: Duration = Duration::from_millis(300);

/// What a stub saw of one call.
#[derive(Debug, Clone)]
pub struct Seen {
    /// The method that was asked for.
    pub method: String,
    /// What it was asked with.
    pub params: serde_json::Value,
    /// What was presented, if anything.
    pub authorization: Option<String>,
}

/// Everything a stub saw, in the order it saw it.
type Record = Arc<Mutex<Vec<Seen>>>;

/// How a stub answers. Every one of these is a store somebody really runs.
#[derive(Debug, Clone)]
pub enum Replies {
    /// With the call itself, so a test can see what crossed the wire.
    Echo,
    /// With a JSON-RPC error object.
    Failure,
    /// With a 401, as a store with a different token does.
    Unauthorized,
    /// With a body that is not JSON-RPC at all.
    Garbage,
    /// With an answer to a call nobody made.
    WrongId,
    /// With a 502, as a proxy in front of a store that is down does.
    Gateway,
    /// Saying a node was deleted.
    Deleted,
    /// Saying nothing was there to delete.
    Absent,
    /// With a prepared result per method, which is what a test of several calls in a row needs.
    ///
    /// A method nothing was prepared for is answered with an empty object rather than an error, so
    /// a test scripts only the answers it reads and the writes it ignores still succeed.
    Scripted(BTreeMap<String, serde_json::Value>),
}

impl Replies {
    /// A script, from method name to the result that method answers with.
    pub fn scripted<I, K>(answers: I) -> Self
    where
        I: IntoIterator<Item = (K, serde_json::Value)>,
        K: Into<String>,
    {
        Self::Scripted(
            answers
                .into_iter()
                .map(|(method, result)| (method.into(), result))
                .collect(),
        )
    }
}

/// A stub plane, listening for as long as the process runs.
pub struct StubPlane {
    address: SocketAddr,
    record: Record,
}

impl StubPlane {
    /// Starts a stub answering as `replies` says, on a loopback port it picked itself.
    ///
    /// # Panics
    ///
    /// If its own socket will not bind, which is the fixture failing rather than the code.
    pub async fn start(replies: Replies) -> Self {
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
                let replies = replies.clone();
                tokio::spawn(async move {
                    let _ = http1::Builder::new()
                        .serve_connection(
                            TokioIo::new(stream),
                            service_fn(move |request| {
                                answer(Arc::clone(&kept), replies.clone(), request)
                            }),
                        )
                        .await;
                });
            }
        });
        Self { address, record }
    }

    /// Where it is listening.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// A client bound to this stub, with the stub's own credential.
    pub fn client(&self) -> DrsgClient {
        DrsgClient::new(stub_settings(self.address))
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

    /// What one method was asked with, once per call, in the order the calls arrived.
    ///
    /// # Panics
    ///
    /// If a previous panic poisoned the record.
    pub fn calls(&self, method: &str) -> Vec<serde_json::Value> {
        self.seen()
            .into_iter()
            .filter(|seen| seen.method == method)
            .map(|seen| seen.params)
            .collect()
    }

    /// The one call it saw.
    ///
    /// # Panics
    ///
    /// If it saw any other number of calls, saying how many, since a test that reads "the" call
    /// has assumed there was one.
    pub fn only(&self) -> Seen {
        let seen = self.seen();
        assert_eq!(seen.len(), 1, "one call was expected");
        seen.into_iter().next().expect("a record of one holds one")
    }
}

/// A socket that accepts a connection and answers nothing, so a client's own clock is what ends
/// the call.
pub struct SilentPlane {
    address: SocketAddr,
}

impl SilentPlane {
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
    pub fn client(&self) -> DrsgClient {
        DrsgClient::new(stub_settings(self.address))
    }
}

/// How a client reaches a stub: its address, the stub's credential and a short clock.
///
/// # Panics
///
/// If the address will not make a url, which is the fixture failing rather than the code.
pub fn stub_settings(address: SocketAddr) -> DrsgSettings {
    DrsgSettings {
        endpoint: DrsgEndpoint::new(&format!("http://{address}/rpc"))
            .expect("the fixture's own endpoint is one"),
        token: Secret::from(STUB_TOKEN),
        plane: PlaneName::new("graphs").expect("the fixture's own plane name is one"),
        timeout: STUB_TIMEOUT,
    }
}

async fn answer(
    record: Record,
    replies: Replies,
    request: Request<hyper::body::Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let authorization = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = request
        .into_body()
        .collect()
        .await
        .map(|collected| collected.to_bytes())
        .unwrap_or_default();
    let call: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    let id = call["id"].as_u64().unwrap_or(0);
    let method = call["method"].as_str().unwrap_or_default().to_owned();
    record
        .lock()
        .expect("the record is not poisoned")
        .push(Seen {
            method: method.clone(),
            params: call["params"].clone(),
            authorization,
        });

    let built = Response::builder();
    Ok(match replies {
        Replies::Echo => json_body(
            built,
            &serde_json::json!({
                "jsonrpc": "2.0", "id": id, "result": {"echoed": call["params"].clone()},
            }),
        ),
        Replies::Failure => json_body(
            built,
            &serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32602, "message": "no plane named graphs"},
            }),
        ),
        Replies::Unauthorized => built
            .status(StatusCode::UNAUTHORIZED)
            .body(Full::new(Bytes::new()))
            .expect("the fixture's own response builds"),
        Replies::Garbage => json_body(built, &serde_json::json!({"ok": true})),
        Replies::WrongId => json_body(
            built,
            &serde_json::json!({
                "jsonrpc": "2.0", "id": id + 100, "result": {},
            }),
        ),
        Replies::Gateway => built
            .status(StatusCode::BAD_GATEWAY)
            .body(Full::new(Bytes::new()))
            .expect("the fixture's own response builds"),
        Replies::Deleted => json_body(
            built,
            &serde_json::json!({
                "jsonrpc": "2.0", "id": id, "result": {"deleted": true, "id": 42},
            }),
        ),
        Replies::Absent => json_body(
            built,
            &serde_json::json!({
                "jsonrpc": "2.0", "id": id, "result": {"deleted": false},
            }),
        ),
        Replies::Scripted(answers) => json_body(
            built,
            &serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "result": answers.get(&method).cloned().unwrap_or_else(|| serde_json::json!({})),
            }),
        ),
    })
}

fn json_body(built: http::response::Builder, payload: &serde_json::Value) -> Response<Full<Bytes>> {
    built
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(payload.to_string())))
        .expect("the fixture's own response builds")
}
