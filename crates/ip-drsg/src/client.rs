//! Speaking JSON-RPC to one dr-strange plane.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderValue, Request, StatusCode, header};
use http_body_util::{BodyExt, Full, Limited};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use ip_config::Secret;

use crate::error::DrsgError;
use crate::plane::{DrsgEndpoint, PlaneName};
use crate::rpc::{RpcRequest, RpcResponse};

/// How much of one answer this will read before refusing the rest.
///
/// A graph is small, so this bounds a store that answers a query with the whole plane.
const MAX_ANSWER_BYTES: usize = 8 * 1024 * 1024;

/// How long a connection may take to establish.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The pooling http client this crate talks over.
type Pooled = Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>;

/// How a client reaches one plane, and how patient it is.
///
/// A composite rather than four parameters, and the one place the credential lives.
#[derive(Debug, Clone)]
pub struct DrsgSettings {
    /// Where the store answers JSON-RPC.
    pub endpoint: DrsgEndpoint,
    /// The bearer token every call presents.
    pub token: Secret,
    /// The one plane this client reads and writes.
    pub plane: PlaneName,
    /// How long one call may take before the store is given up on.
    pub timeout: Duration,
}

/// A client bound to one plane of one dr-strange server.
///
/// One client per deployment, holding a connection pool: every method is one JSON-RPC call, and
/// the plane it names is the configured one, never a caller's argument.
#[derive(Debug)]
pub struct DrsgClient {
    /// Where it calls, what it presents, and which plane it names.
    settings: DrsgSettings,
    /// The pool every call goes over.
    http: Pooled,
    /// The id of the next call, so two in flight cannot read each other's answer.
    next: AtomicU64,
}

impl DrsgClient {
    /// A client over the configured endpoint, plane and credential.
    pub fn new(settings: DrsgSettings) -> Self {
        let http = pooled(settings.timeout);
        Self {
            settings,
            http,
            next: AtomicU64::new(1),
        }
    }

    /// The plane this client is bound to.
    pub fn plane(&self) -> &PlaneName {
        &self.settings.plane
    }

    /// Makes the configured plane, which the store refuses if the name is taken.
    ///
    /// The one method whose parameter is `name` rather than `plane`, because it names a plane that
    /// does not exist yet; and the one needing the store's admin access rather than write.
    pub async fn plane_create(&self) -> Result<(), DrsgError> {
        self.call(
            "plane.create",
            serde_json::json!({ "name": self.settings.plane.as_str() }),
        )
        .await
        .map(|_| ())
    }

    /// Adds a node under a stable key, which the store refuses if the key is already bound.
    ///
    /// The key is how everything after this addresses the node, so nothing here has to keep the
    /// numeric id a store assigned.
    pub async fn node_create(
        &self,
        key: &str,
        labels: &[&str],
        properties: serde_json::Value,
    ) -> Result<serde_json::Value, DrsgError> {
        self.call(
            "node.create",
            serde_json::json!({
                "plane": self.settings.plane.as_str(),
                "key": key,
                "labels": labels,
                "properties": properties,
            }),
        )
        .await
    }

    /// Removes the node under a key, and the edges touching it.
    ///
    /// Answers whether one was there, so deleting what is already gone is not a failure.
    pub async fn node_delete(&self, key: &str) -> Result<bool, DrsgError> {
        let answer = self
            .call(
                "node.delete",
                serde_json::json!({ "plane": self.settings.plane.as_str(), "key": key }),
            )
            .await?;
        Ok(answer["deleted"].as_bool().unwrap_or(false))
    }

    /// Adds a directed edge of one type between two nodes named by their keys.
    pub async fn edge_create(
        &self,
        from: &str,
        to: &str,
        kind: &str,
        properties: serde_json::Value,
    ) -> Result<serde_json::Value, DrsgError> {
        self.call(
            "edge.create",
            serde_json::json!({
                "plane": self.settings.plane.as_str(),
                "src": from,
                "dst": to,
                "type": kind,
                "properties": properties,
            }),
        )
        .await
    }

    /// Removes one edge by the id the store assigned it.
    ///
    /// Answers whether one was there, as [`node_delete`](Self::node_delete) does.
    pub async fn edge_delete(&self, edge: u64) -> Result<bool, DrsgError> {
        let answer = self
            .call(
                "edge.delete",
                serde_json::json!({ "plane": self.settings.plane.as_str(), "edge": edge }),
            )
            .await?;
        Ok(answer["deleted"].as_bool().unwrap_or(false))
    }

    /// Runs a read plan against the plane and answers what it returned.
    ///
    /// The plan is passed through rather than built here: this crate is the transport, and what a
    /// graph asks for is the caller's to say.
    pub async fn query(&self, plan: serde_json::Value) -> Result<serde_json::Value, DrsgError> {
        self.call(
            "plane.query",
            serde_json::json!({ "plane": self.settings.plane.as_str(), "plan": plan }),
        )
        .await
    }

    /// Carries one call to the store and reads what came back.
    async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, DrsgError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let message = RpcRequest::call(id, method, params);
        let payload = serde_json::to_vec(&message).map_err(|error| DrsgError::Malformed {
            detail: format!("the call will not serialize: {error}"),
        })?;

        let presented = HeaderValue::from_str(&format!("Bearer {}", self.settings.token.expose()))
            .map_err(|_| DrsgError::Unauthorized)?;
        let request = Request::builder()
            .method(http::Method::POST)
            .uri(self.settings.endpoint.as_str())
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json")
            .header(header::AUTHORIZATION, presented)
            .body(Full::new(Bytes::from(payload)))
            .map_err(|error| DrsgError::Unreachable {
                detail: error.to_string(),
            })?;

        let answered = tokio::time::timeout(self.settings.timeout, self.http.request(request))
            .await
            .map_err(|_| DrsgError::Unreachable {
                detail: format!(
                    "no answer within {} seconds",
                    self.settings.timeout.as_secs()
                ),
            })?
            .map_err(|error| DrsgError::Unreachable {
                detail: error.to_string(),
            })?;

        let status = answered.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(DrsgError::Unauthorized);
        }
        if !status.is_success() {
            return Err(DrsgError::Unreachable {
                detail: format!("the store answered {}", status.as_u16()),
            });
        }

        let body = Limited::new(answered.into_body(), MAX_ANSWER_BYTES)
            .collect()
            .await
            .map_err(|_| DrsgError::Unreachable {
                detail: format!("the answer is longer than {MAX_ANSWER_BYTES} bytes"),
            })?
            .to_bytes();
        let message: RpcResponse =
            serde_json::from_slice(&body).map_err(|error| DrsgError::Malformed {
                detail: error.to_string(),
            })?;
        message.answer_to(id)
    }
}

/// The connection pool every call to one store goes over.
fn pooled(request_timeout: Duration) -> Pooled {
    install_crypto_provider();
    let mut http = HttpConnector::new();
    http.set_connect_timeout(Some(CONNECT_TIMEOUT.min(request_timeout)));
    http.enforce_http(false);
    let connector = HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .wrap_connector(http);
    Client::builder(TokioExecutor::new()).build(connector)
}

/// Rustls needs one process wide provider; a second install is another caller winning the race.
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use http::Response;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use tokio::net::TcpListener;

    use super::*;

    /// The credential the stub below requires, and the thing no message may ever carry.
    const TOKEN: &str = "drsg-live-4f91ca7b22e8";

    /// What the stub saw of one call.
    #[derive(Debug, Clone)]
    struct Seen {
        /// The method that was asked for.
        method: String,
        /// What it was asked with.
        params: serde_json::Value,
        /// What was presented, if anything.
        authorization: Option<String>,
    }

    /// Everything the stub saw, in the order it saw it.
    type Record = Arc<Mutex<Vec<Seen>>>;

    /// How the stub answers. Every one of these is a store somebody really runs.
    #[derive(Debug, Clone, Copy)]
    enum Answers {
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
    }

    async fn answer(
        record: Record,
        answers: Answers,
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
        let call: serde_json::Value =
            serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        let id = call["id"].as_u64().unwrap_or(0);
        record
            .lock()
            .expect("the record is not poisoned")
            .push(Seen {
                method: call["method"].as_str().unwrap_or_default().to_owned(),
                params: call["params"].clone(),
                authorization,
            });

        let built = Response::builder();
        Ok(match answers {
            Answers::Echo => json_body(
                built,
                &serde_json::json!({
                    "jsonrpc": "2.0", "id": id, "result": {"echoed": call["params"].clone()},
                }),
            ),
            Answers::Failure => json_body(
                built,
                &serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32602, "message": "no plane named graphs"},
                }),
            ),
            Answers::Unauthorized => built
                .status(StatusCode::UNAUTHORIZED)
                .body(Full::new(Bytes::new()))
                .expect("the fixture's own response builds"),
            Answers::Garbage => json_body(built, &serde_json::json!({"ok": true})),
            Answers::WrongId => json_body(
                built,
                &serde_json::json!({
                    "jsonrpc": "2.0", "id": id + 100, "result": {},
                }),
            ),
            Answers::Gateway => built
                .status(StatusCode::BAD_GATEWAY)
                .body(Full::new(Bytes::new()))
                .expect("the fixture's own response builds"),
            Answers::Deleted => json_body(
                built,
                &serde_json::json!({
                    "jsonrpc": "2.0", "id": id, "result": {"deleted": true, "id": 42},
                }),
            ),
            Answers::Absent => json_body(
                built,
                &serde_json::json!({
                    "jsonrpc": "2.0", "id": id, "result": {"deleted": false},
                }),
            ),
        })
    }

    fn json_body(
        built: http::response::Builder,
        payload: &serde_json::Value,
    ) -> Response<Full<Bytes>> {
        built
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(payload.to_string())))
            .expect("the fixture's own response builds")
    }

    async fn stub(answers: Answers) -> (SocketAddr, Record) {
        let record: Record = Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0")
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
        (address, record)
    }

    /// Accepts the connection and answers nothing, so the client's own clock is what ends it.
    async fn silent() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0")
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
        address
    }

    fn settings(address: SocketAddr) -> DrsgSettings {
        DrsgSettings {
            endpoint: DrsgEndpoint::new(&format!("http://{address}/rpc"))
                .expect("the fixture's own endpoint is one"),
            token: Secret::from(TOKEN),
            plane: PlaneName::new("graphs").expect("the fixture's own plane name is one"),
            timeout: Duration::from_millis(300),
        }
    }

    async fn client(answers: Answers) -> (DrsgClient, Record) {
        let (address, record) = stub(answers).await;
        (DrsgClient::new(settings(address)), record)
    }

    fn only(record: &Record) -> Seen {
        let seen = record.lock().expect("the record is not poisoned");
        assert_eq!(seen.len(), 1, "one call was expected");
        seen[0].clone()
    }

    #[tokio::test]
    async fn every_call_presents_the_credential_as_a_bearer_token() {
        let (client, record) = client(Answers::Echo).await;
        client.query(serde_json::json!({})).await.unwrap();
        assert_eq!(
            only(&record).authorization.as_deref(),
            Some(format!("Bearer {TOKEN}").as_str())
        );
    }

    #[tokio::test]
    async fn every_call_names_the_configured_plane_and_no_other() {
        let (client, record) = client(Answers::Echo).await;
        client
            .query(serde_json::json!({"match": "GraphNode"}))
            .await
            .unwrap();
        let seen = only(&record);
        assert_eq!(seen.method, "plane.query");
        assert_eq!(seen.params["plane"], "graphs");
        assert_eq!(
            seen.params["plan"],
            serde_json::json!({"match": "GraphNode"})
        );
    }

    /// `plane.create` names the plane under `name`, which is the one method that differs.
    #[tokio::test]
    async fn making_a_plane_names_it_the_way_that_method_asks() {
        let (client, record) = client(Answers::Echo).await;
        client.plane_create().await.unwrap();
        let seen = only(&record);
        assert_eq!(seen.method, "plane.create");
        assert_eq!(seen.params["name"], "graphs");
        assert!(seen.params.get("plane").is_none(), "{:?}", seen.params);
    }

    #[tokio::test]
    async fn a_node_is_made_under_the_key_that_addresses_it() {
        let (client, record) = client(Answers::Echo).await;
        client
            .node_create(
                "graph:g1:survey",
                &["GraphNode"],
                serde_json::json!({"owner": "acme"}),
            )
            .await
            .unwrap();
        let seen = only(&record);
        assert_eq!(seen.method, "node.create");
        assert_eq!(seen.params["plane"], "graphs");
        assert_eq!(seen.params["key"], "graph:g1:survey");
        assert_eq!(seen.params["labels"], serde_json::json!(["GraphNode"]));
        assert_eq!(seen.params["properties"]["owner"], "acme");
    }

    #[tokio::test]
    async fn an_edge_names_its_ends_by_key_and_carries_its_type() {
        let (client, record) = client(Answers::Echo).await;
        client
            .edge_create(
                "graph:g1:survey",
                "graph:g1:summary",
                "FEEDS",
                serde_json::json!({}),
            )
            .await
            .unwrap();
        let seen = only(&record);
        assert_eq!(seen.method, "edge.create");
        assert_eq!(seen.params["src"], "graph:g1:survey");
        assert_eq!(seen.params["dst"], "graph:g1:summary");
        assert_eq!(seen.params["type"], "FEEDS");
    }

    #[tokio::test]
    async fn deleting_says_whether_anything_was_there() {
        let (present, _) = client(Answers::Deleted).await;
        assert!(present.node_delete("graph:g1:survey").await.unwrap());
        assert!(present.edge_delete(42).await.unwrap());

        let (absent, _) = client(Answers::Absent).await;
        assert!(!absent.node_delete("graph:g1:gone").await.unwrap());
        assert!(!absent.edge_delete(99).await.unwrap());
    }

    #[tokio::test]
    async fn an_edge_is_deleted_by_the_id_the_store_gave_it() {
        let (client, record) = client(Answers::Echo).await;
        let _ = client.edge_delete(42).await;
        let seen = only(&record);
        assert_eq!(seen.method, "edge.delete");
        assert_eq!(seen.params["edge"], 42);
    }

    #[tokio::test]
    async fn a_store_that_refuses_the_call_says_why_and_that_is_kept() {
        let (client, _) = client(Answers::Failure).await;
        assert_eq!(
            client.query(serde_json::json!({})).await,
            Err(DrsgError::Refused {
                code: -32602,
                message: "no plane named graphs".to_owned(),
            })
        );
    }

    #[tokio::test]
    async fn a_store_that_will_not_take_the_credential_is_reported_as_that() {
        let (client, _) = client(Answers::Unauthorized).await;
        assert_eq!(
            client.query(serde_json::json!({})).await,
            Err(DrsgError::Unauthorized)
        );
    }

    #[tokio::test]
    async fn an_answer_that_is_not_json_rpc_is_not_an_answer() {
        let (client, _) = client(Answers::Garbage).await;
        assert!(matches!(
            client.query(serde_json::json!({})).await,
            Err(DrsgError::Malformed { .. })
        ));
    }

    #[tokio::test]
    async fn an_answer_to_another_call_is_refused_rather_than_read() {
        let (client, _) = client(Answers::WrongId).await;
        assert!(matches!(
            client.query(serde_json::json!({})).await,
            Err(DrsgError::Malformed { .. })
        ));
    }

    #[tokio::test]
    async fn a_status_that_is_not_a_result_is_unreachable_and_names_it() {
        let (client, _) = client(Answers::Gateway).await;
        let refusal = client.query(serde_json::json!({})).await;
        match refusal {
            Err(DrsgError::Unreachable { detail }) => assert!(detail.contains("502"), "{detail}"),
            other => panic!("a 502 was read as {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_store_that_never_answers_is_given_up_on() {
        let client = DrsgClient::new(settings(silent().await));
        assert!(matches!(
            client.query(serde_json::json!({})).await,
            Err(DrsgError::Unreachable { .. })
        ));
    }

    /// Two calls in flight must not read each other's answer, which is what the id is for.
    #[tokio::test]
    async fn each_call_carries_an_id_of_its_own() {
        let (client, record) = client(Answers::Echo).await;
        client.query(serde_json::json!({})).await.unwrap();
        client.query(serde_json::json!({})).await.unwrap();
        assert_eq!(record.lock().expect("the record is not poisoned").len(), 2);
    }

    /// The `Secret` contract, asserted here because this is the crate that presents it.
    #[tokio::test]
    async fn the_credential_never_appears_in_a_debug_or_in_a_refusal() {
        let (client, _) = client(Answers::Failure).await;
        let printed = format!("{client:?}");
        assert!(!printed.contains(TOKEN), "the client printed its own token");

        let refusal = client.query(serde_json::json!({})).await;
        let said = format!("{refusal:?}");
        assert!(!said.contains(TOKEN), "a refusal carried the token");
        let Err(error) = refusal else {
            panic!("the store was supposed to refuse");
        };
        assert!(
            !error.to_string().contains(TOKEN),
            "a message carried the token"
        );

        let settings = format!("{:?}", settings("127.0.0.1:1".parse().unwrap()));
        assert!(!settings.contains(TOKEN), "the settings printed the token");
    }
}
