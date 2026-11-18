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
use ip_retrieve::NodeLabel;

use crate::error::DrsgError;
use crate::look;
use crate::look::{Across, Apart, Around, Grouped, Hit, Hop, Nearest, Path, Placed};
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

    /// Runs one graph algorithm over the nodes carrying a label, and the edges of one type among
    /// them.
    ///
    /// Both arguments are required rather than optional, although the store defaults each: the
    /// label is what narrows an algorithm to one graph, since a plane holds every deployment's,
    /// and an unnamed edge type means *every* type, which an edge that is not a dependency turns
    /// into a cycle that is not one.
    pub async fn algo(
        &self,
        algo: &str,
        label: &str,
        edge_type: &str,
        extra: serde_json::Value,
    ) -> Result<serde_json::Value, DrsgError> {
        let mut params = serde_json::json!({ "edge_type": edge_type });
        if let (Some(params), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
            params.extend(extra.clone());
        }
        self.run(algo, Some(label), params).await
    }

    /// What is nearest a query vector, as scored node records.
    ///
    /// **An empty answer is three things this client cannot tell apart**: a plane holding no
    /// embeddings of that `(label, property)`, embeddings of a different width than the query,
    /// and nothing near enough to answer with. The store's own doc says so, and it reports no
    /// width to compare against — which is why a retriever's width is measured when it is
    /// registered rather than asked for here.
    pub async fn search(&self, asked: &Nearest) -> Result<Vec<Hit>, DrsgError> {
        let mut params = serde_json::json!({
            "plane": self.settings.plane.as_str(),
            "property": asked.property.as_str(),
            "query": asked.query,
            "k": asked.hits,
        });
        if let Some(params) = params.as_object_mut() {
            if let Some(label) = &asked.label {
                params.insert("label".to_owned(), serde_json::json!(label.as_str()));
            }
            if let Some(metric) = asked.metric {
                params.insert("metric".to_owned(), serde_json::json!(metric.as_str()));
            }
        }
        look::rows(self.call("plane.search", params).await?, "a search")
    }

    /// What one hop reaches from a node, each hop carrying the node it reached and the edge it
    /// crossed.
    ///
    /// Hydrated, so a hop is a record rather than a pair of ids: what a look-up is for is text a
    /// model can read, and ids would mean one call per hop to find any. Lean with it, so a node's
    /// embedding crosses the wire as a marker rather than as its floats.
    pub async fn neighbors(&self, asked: &Around) -> Result<Vec<Hop>, DrsgError> {
        let mut params = serde_json::json!({
            "plane": self.settings.plane.as_str(),
            "id": asked.node,
            "direction": asked.direction.as_str(),
            "hydrate": true,
            "lean": true,
        });
        if let (Some(params), Some(kind)) = (params.as_object_mut(), &asked.edge_type) {
            params.insert("type".to_owned(), serde_json::json!(kind));
        }
        look::rows(self.call("plane.neighbors", params).await?, "a walk")
    }

    /// Which nodes matter most, by how much of the graph leads to them.
    pub async fn rank(&self, asked: &Across) -> Result<Vec<Placed>, DrsgError> {
        self.placed("pagerank", asked, "a ranking").await
    }

    /// Which groups the nodes fall into.
    pub async fn cluster(&self, asked: &Across) -> Result<Vec<Grouped>, DrsgError> {
        let answer = self
            .run(
                "louvain",
                asked.label.as_ref().map(NodeLabel::as_str),
                serde_json::json!({ "limit": asked.limit }),
            )
            .await?;
        look::rows(answer["results"].clone(), "a clustering")
    }

    /// How two nodes are connected, and nothing where nothing connects them.
    ///
    /// The absence is an answer rather than a refusal: two documents with no path between them is
    /// what a model asked about and is worth being told.
    pub async fn relate(&self, asked: &Apart) -> Result<Option<Path>, DrsgError> {
        let answer = self
            .run(
                "shortest_path",
                None,
                serde_json::json!({
                    "src": asked.from,
                    "dst": asked.to,
                    "dir": asked.direction.as_str(),
                }),
            )
            .await?;
        match answer.get("path") {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(path) => serde_json::from_value(path.clone())
                .map(Some)
                .map_err(|error| DrsgError::Malformed {
                    detail: format!("a path holds something else: {error}"),
                }),
        }
    }

    /// One algorithm answering `{id, score}` rows, which is pagerank and would be degree.
    async fn placed(
        &self,
        algo: &str,
        asked: &Across,
        promised: &str,
    ) -> Result<Vec<Placed>, DrsgError> {
        let answer = self
            .run(
                algo,
                asked.label.as_ref().map(NodeLabel::as_str),
                serde_json::json!({ "limit": asked.limit }),
            )
            .await?;
        look::rows(answer["results"].clone(), promised)
    }

    /// Carries one `plane.algo` call, naming the plane and the algorithm last.
    ///
    /// Last so that neither is something a caller's own parameters can replace, and the label
    /// optional because only this crate's own graph methods have a label they must always send.
    async fn run(
        &self,
        algo: &str,
        label: Option<&str>,
        extra: serde_json::Value,
    ) -> Result<serde_json::Value, DrsgError> {
        let mut params = extra;
        if let Some(params) = params.as_object_mut() {
            if let Some(label) = label {
                params.insert("label".to_owned(), serde_json::json!(label));
            }
            params.insert("algo".to_owned(), serde_json::json!(algo));
            params.insert(
                "plane".to_owned(),
                serde_json::json!(self.settings.plane.as_str()),
            );
        }
        self.call("plane.algo", params).await
    }

    /// Runs one openCypher-subset statement against the plane, with its parameters bound.
    ///
    /// What this answers that [`algo`](Self::algo) and [`query`](Self::query) cannot: a path of
    /// more than one hop. The store's own traversal method expands a single hop, and its
    /// algorithms are ranked or ordered answers rather than reachable sets, so *which nodes does
    /// this one reach* is a pattern — `-[:TYPE*1..n]->` — and nothing else. The upper bound is not
    /// optional: the store refuses an unbounded `*`.
    ///
    /// The store gates this behind write access even for a statement that only reads, which costs
    /// this deployment nothing: writing a graph creates nodes and edges through the same token.
    ///
    /// A statement that *projects* — `RETURN b.name AS name` — answers a table, as
    /// `{"columns": [...], "rows": [[...]]}`. One that returns a bound node answers the induced
    /// subgraph instead, so a caller that wants rows projects.
    pub async fn cypher(
        &self,
        query: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, DrsgError> {
        self.call(
            "plane.cypher",
            serde_json::json!({
                "plane": self.settings.plane.as_str(),
                "query": query,
                "params": params,
            }),
        )
        .await
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
    use ip_retrieve::VectorProperty;

    use crate::look::{Direction, Metric};
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

    /// The two things an algorithm must always be told, since the store's own defaults for them
    /// are the whole plane and every edge type.
    #[tokio::test]
    async fn an_algorithm_names_the_label_it_runs_over_and_the_edge_it_follows() {
        let (client, record) = client(Answers::Echo).await;
        client
            .algo(
                "toposort",
                "Graph/acme/triage",
                "FEEDS",
                serde_json::json!({}),
            )
            .await
            .expect("the stub answers");
        let seen = only(&record);
        assert_eq!(seen.method, "plane.algo");
        assert_eq!(seen.params["algo"], "toposort");
        assert_eq!(seen.params["label"], "Graph/acme/triage");
        assert_eq!(seen.params["edge_type"], "FEEDS");
        assert_eq!(seen.params["plane"], "graphs");
    }

    #[tokio::test]
    async fn what_one_algorithm_needs_and_another_does_not_rides_beside_the_rest() {
        let (client, record) = client(Answers::Echo).await;
        client
            .algo(
                "cycles",
                "Graph/acme/triage",
                "FEEDS",
                serde_json::json!({"limit": 1, "dir": "out"}),
            )
            .await
            .expect("the stub answers");
        let seen = only(&record);
        assert_eq!(seen.params["limit"], 1);
        assert_eq!(seen.params["dir"], "out");
        assert_eq!(seen.params["algo"], "cycles", "the extra overwrote a name");
    }

    /// The statement and its parameters travel apart, which is the point of binding them.
    #[tokio::test]
    async fn a_statement_carries_its_parameters_beside_it_rather_than_inside_it() {
        let (client, record) = client(Answers::Echo).await;
        client
            .cypher(
                "MATCH (a:Node {name: $name})-[:FEEDS*1..256]->(b:Node) RETURN b.name AS name",
                serde_json::json!({"name": "intake"}),
            )
            .await
            .expect("the stub answers");
        let seen = only(&record);
        assert_eq!(seen.method, "plane.cypher");
        assert_eq!(seen.params["plane"], "graphs");
        assert_eq!(seen.params["params"]["name"], "intake");
        assert!(
            seen.params["query"]
                .as_str()
                .is_some_and(|query| query.contains("FEEDS*1..256")),
            "the statement did not cross the wire as it was written"
        );
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

    /// What a plane is asked for a search: the plane it is bound to, where the embeddings are,
    /// the query as a vector and how many hits — and not a metric nobody named.
    #[tokio::test]
    async fn a_search_names_the_plane_the_property_and_the_query() {
        let (client, record) = client(Answers::Echo).await;
        let _ = client.search(&nearest(None, None)).await;
        let seen = only(&record);
        assert_eq!(seen.method, "plane.search");
        assert_eq!(seen.params["plane"], "graphs");
        assert_eq!(seen.params["property"], "embedding");
        assert_eq!(seen.params["query"], serde_json::json!([0.5, 0.25]));
        assert_eq!(seen.params["k"], 4);
        assert!(seen.params.get("label").is_none(), "{:?}", seen.params);
        assert!(seen.params.get("metric").is_none(), "{:?}", seen.params);
    }

    /// Both are sent when they are given, and the metric is worth a test of its own: the store
    /// assumes cosine for an absent one rather than reading the index's.
    #[tokio::test]
    async fn a_search_scoped_to_a_label_or_compared_another_way_says_so() {
        let (client, record) = client(Answers::Echo).await;
        let _ = client
            .search(&nearest(Some("Document"), Some(Metric::Dot)))
            .await;
        let seen = only(&record);
        assert_eq!(seen.params["label"], "Document");
        assert_eq!(seen.params["metric"], "dot");
    }

    #[tokio::test]
    async fn a_search_answers_the_rows_the_store_scored() {
        let stub = StubPlane::start(Answers::scripted([(
            "plane.search",
            serde_json::json!([
                {"id": 7, "external_key": "doc:leave", "labels": ["Document"],
                 "properties": {"title": "Leave"}, "score": 0.9},
                {"id": 8, "labels": ["Document"], "properties": {}},
            ]),
        )]))
        .await;

        let hits = stub
            .client()
            .search(&nearest(None, None))
            .await
            .expect("the store answered rows");

        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].key.as_deref(), Some("doc:leave"));
        assert_eq!(hits[0].properties["title"], "Leave");
        assert_eq!(hits[1].score, None);
    }

    /// The store's own answer to a search is a list; an object where one belongs is a store this
    /// client is not speaking to, and is not read as "nothing found".
    #[tokio::test]
    async fn a_search_answered_with_something_else_is_not_an_empty_result() {
        let stub = StubPlane::start(Answers::scripted([(
            "plane.search",
            serde_json::json!({"hits": []}),
        )]))
        .await;
        assert!(matches!(
            stub.client().search(&nearest(None, None)).await,
            Err(DrsgError::Malformed { .. })
        ));
    }

    /// Hydrated and lean, which is what makes a hop readable without a call per hop and keeps an
    /// embedding out of the answer.
    #[tokio::test]
    async fn a_walk_asks_for_records_rather_than_ids_and_for_markers_rather_than_floats() {
        let (client, record) = client(Answers::Echo).await;
        let _ = client
            .neighbors(&Around {
                node: 7,
                direction: Direction::Both,
                edge_type: Some("CITES".to_owned()),
            })
            .await;
        let seen = only(&record);
        assert_eq!(seen.method, "plane.neighbors");
        assert_eq!(seen.params["plane"], "graphs");
        assert_eq!(seen.params["id"], 7);
        assert_eq!(seen.params["direction"], "both");
        assert_eq!(seen.params["type"], "CITES");
        assert_eq!(seen.params["hydrate"], true);
        assert_eq!(seen.params["lean"], true);
    }

    #[tokio::test]
    async fn a_walk_that_names_no_edge_type_follows_every_type() {
        let (client, record) = client(Answers::Echo).await;
        let _ = client
            .neighbors(&Around {
                node: 7,
                direction: Direction::default(),
                edge_type: None,
            })
            .await;
        let seen = only(&record);
        assert_eq!(seen.params["direction"], "out");
        assert!(seen.params.get("type").is_none(), "{:?}", seen.params);
    }

    #[tokio::test]
    async fn a_walk_answers_what_is_joined_and_what_joins_it() {
        let stub = StubPlane::start(Answers::scripted([(
            "plane.neighbors",
            serde_json::json!([{
                "node": {"id": 9, "labels": ["Document"], "properties": {"title": "Pay"}},
                "edge": {"id": 4, "type": "CITES", "properties": {}},
            }]),
        )]))
        .await;

        let hops = stub
            .client()
            .neighbors(&Around {
                node: 7,
                direction: Direction::Out,
                edge_type: None,
            })
            .await
            .expect("the store answered hops");

        assert_eq!(hops[0].node.properties["title"], "Pay");
        assert_eq!(hops[0].edge.kind.as_deref(), Some("CITES"));
    }

    /// The three algorithm-backed verbs are one method with different parameters, and each names
    /// the algorithm it is.
    #[tokio::test]
    async fn ranking_and_clustering_name_their_algorithm_their_label_and_their_limit() {
        let across = Across {
            label: Some(NodeLabel::new("Document").expect("a label")),
            limit: 6,
        };

        let (ranking, ranked) = client(Answers::Echo).await;
        let _ = ranking.rank(&across).await;
        let (clustering, clustered) = client(Answers::Echo).await;
        let _ = clustering.cluster(&across).await;

        for (seen, algo) in [(only(&ranked), "pagerank"), (only(&clustered), "louvain")] {
            assert_eq!(seen.method, "plane.algo");
            assert_eq!(seen.params["algo"], algo);
            assert_eq!(seen.params["plane"], "graphs");
            assert_eq!(seen.params["label"], "Document");
            assert_eq!(seen.params["limit"], 6);
            assert!(
                seen.params.get("edge_type").is_none(),
                "neither narrows to an edge type: {:?}",
                seen.params
            );
        }
    }

    /// A plane holding one corpus is ranked whole, and the label is then absent rather than
    /// empty: an empty one would scope to a label of that name.
    #[tokio::test]
    async fn an_unscoped_ranking_names_no_label_at_all() {
        let (client, record) = client(Answers::Echo).await;
        let _ = client
            .rank(&Across {
                label: None,
                limit: 3,
            })
            .await;
        assert!(
            only(&record).params.get("label").is_none(),
            "an unscoped ranking named a label"
        );
    }

    #[tokio::test]
    async fn a_ranking_answers_what_scored_and_a_clustering_what_grouped() {
        let stub = StubPlane::start(Answers::scripted([(
            "plane.algo",
            serde_json::json!({
                "algo": "pagerank",
                "results": [{"id": 7, "score": 0.42}],
                "count": 1,
            }),
        )]))
        .await;
        let across = Across {
            label: None,
            limit: 3,
        };
        let placed = stub
            .client()
            .rank(&across)
            .await
            .expect("the store answered a ranking");
        assert_eq!(placed[0].id, 7);
        assert!((placed[0].score - 0.42).abs() < f64::EPSILON);

        let grouping = StubPlane::start(Answers::scripted([(
            "plane.algo",
            serde_json::json!({
                "algo": "louvain",
                "results": [{"id": 7, "community": 3}, {"id": 8, "community": 3}],
                "count": 1,
            }),
        )]))
        .await;
        let grouped = grouping
            .client()
            .cluster(&across)
            .await
            .expect("the store answered a clustering");
        assert_eq!(grouped[1].group, 3);
    }

    /// A path takes two nodes and a direction and no label: the store's own shortest path is over
    /// the nodes it is given rather than over a scope.
    #[tokio::test]
    async fn a_relate_names_both_ends_and_which_way_an_edge_may_be_crossed() {
        let (client, record) = client(Answers::Echo).await;
        let _ = client
            .relate(&Apart {
                from: 7,
                to: 9,
                direction: Direction::Both,
            })
            .await;
        let seen = only(&record);
        assert_eq!(seen.params["algo"], "shortest_path");
        assert_eq!(seen.params["plane"], "graphs");
        assert_eq!(seen.params["src"], 7);
        assert_eq!(seen.params["dst"], 9);
        assert_eq!(seen.params["dir"], "both");
        assert!(seen.params.get("label").is_none(), "{:?}", seen.params);
    }

    #[tokio::test]
    async fn a_relate_answers_the_ids_along_the_way() {
        let stub = StubPlane::start(Answers::scripted([(
            "plane.algo",
            serde_json::json!({
                "algo": "shortest_path",
                "found": true,
                "path": {"nodes": [7, 8, 9], "edges": [4, 5], "cost": 2.0},
            }),
        )]))
        .await;

        let path = stub
            .client()
            .relate(&apart())
            .await
            .expect("the store answered a path")
            .expect("one was found");

        assert_eq!(path.nodes, [7, 8, 9]);
        assert_eq!(path.edges, [4, 5]);
    }

    /// Nothing connecting them is an answer a model asked for, not a refusal.
    #[tokio::test]
    async fn two_nodes_nothing_connects_are_answered_with_nothing() {
        let stub = StubPlane::start(Answers::scripted([(
            "plane.algo",
            serde_json::json!({"algo": "shortest_path", "found": false, "path": null}),
        )]))
        .await;
        assert_eq!(
            stub.client()
                .relate(&apart())
                .await
                .expect("the store answered"),
            None
        );
    }

    #[tokio::test]
    async fn a_path_that_is_not_one_is_refused_rather_than_read() {
        let stub = StubPlane::start(Answers::scripted([(
            "plane.algo",
            serde_json::json!({"found": true, "path": {"nodes": "everything"}}),
        )]))
        .await;
        assert!(matches!(
            stub.client().relate(&apart()).await,
            Err(DrsgError::Malformed { .. })
        ));
    }

    /// The plane and the algorithm are named last, so what one algorithm needs cannot replace
    /// what every call means.
    #[tokio::test]
    async fn a_callers_own_parameters_cannot_replace_the_plane_or_the_algorithm() {
        let (client, record) = client(Answers::Echo).await;
        let _ = client
            .algo(
                "cycles",
                "Graph/acme/triage",
                "FEEDS",
                serde_json::json!({"plane": "somebody-elses", "algo": "pagerank"}),
            )
            .await;
        let seen = only(&record);
        assert_eq!(seen.params["plane"], "graphs");
        assert_eq!(seen.params["algo"], "cycles");
    }

    /// A query vector over the plane's own property, as a look-up sends one.
    fn nearest(label: Option<&str>, metric: Option<Metric>) -> Nearest {
        Nearest {
            property: VectorProperty::new("embedding").expect("a property"),
            label: label.map(|label| NodeLabel::new(label).expect("a label")),
            query: vec![0.5, 0.25],
            hits: 4,
            metric,
        }
    }

    fn apart() -> Apart {
        Apart {
            from: 7,
            to: 9,
            direction: Direction::Out,
        }
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
