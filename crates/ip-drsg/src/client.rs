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
    use super::*;
    use crate::testing::{
        Replies as Answers, STUB_TOKEN as TOKEN, Seen, SilentPlane, StubPlane, stub_settings,
    };

    /// Kept as a pair, so each test below reads as it did when the stub lived in this file.
    async fn client(answers: Answers) -> (DrsgClient, StubPlane) {
        let stub = StubPlane::start(answers).await;
        (stub.client(), stub)
    }

    fn only(stub: &StubPlane) -> Seen {
        stub.only()
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
        let client = SilentPlane::start().await.client();
        assert!(matches!(
            client.query(serde_json::json!({})).await,
            Err(DrsgError::Unreachable { .. })
        ));
    }

    /// What a graph's own tests will drive the stub with: one prepared answer per method, so a
    /// sequence of calls can be exercised without a stub per step.
    #[tokio::test]
    async fn a_scripted_store_answers_each_method_with_what_was_prepared_for_it() {
        let stub = StubPlane::start(Answers::scripted([
            ("plane.query", serde_json::json!([{"id": 1}])),
            ("node.delete", serde_json::json!({"deleted": true})),
        ]))
        .await;
        let client = stub.client();
        assert_eq!(
            client
                .query(serde_json::json!({"source": "ScanAll"}))
                .await
                .expect("the script answers a read"),
            serde_json::json!([{"id": 1}])
        );
        assert!(
            client
                .node_delete("a-key")
                .await
                .expect("the script answers a delete")
        );
        assert!(
            !client
                .edge_delete(7)
                .await
                .expect("a method nothing prepared still answers"),
            "an unscripted method answered as though it had done something"
        );
        assert_eq!(stub.calls("plane.query").len(), 1);
        assert_eq!(stub.calls("node.create").len(), 0);
    }

    /// Two calls in flight must not read each other's answer, which is what the id is for.
    #[tokio::test]
    async fn each_call_carries_an_id_of_its_own() {
        let (client, record) = client(Answers::Echo).await;
        client.query(serde_json::json!({})).await.unwrap();
        client.query(serde_json::json!({})).await.unwrap();
        assert_eq!(record.seen().len(), 2);
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

        let settings = format!("{:?}", stub_settings("127.0.0.1:1".parse().unwrap()));
        assert!(!settings.contains(TOKEN), "the settings printed the token");
    }
}
