//! Speaking REST to one qdrant.

use std::time::Duration;

use bytes::Bytes;
use http::header;
use http::{HeaderValue, Method, Request, StatusCode};
use http_body_util::{BodyExt, Full, Limited};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use ip_config::Secret;
use ip_retrieve::{CollectionName, RetrieverEndpoint, VectorName};

use crate::error::QdrantError;
use crate::look::{Collection, Distance, Found, Query};

/// How much of one answer this will read before refusing the rest.
///
/// A look-up answers at most the retriever's own ceiling of hits, each carrying a payload, so
/// this bounds a store that answers a query with a corpus.
const MAX_ANSWER_BYTES: usize = 8 * 1024 * 1024;

/// How long a connection may take to establish.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The header qdrant takes its key in.
const API_KEY: &str = "api-key";

/// The pooling http client this crate talks over.
type Pooled = Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>;

/// How a client reaches one qdrant, and how patient it is.
///
/// A composite rather than three parameters, and the one place the credential lives.
#[derive(Debug, Clone)]
pub struct QdrantSettings {
    /// Where the store answers, as the retriever that named it spelled it.
    pub endpoint: RetrieverEndpoint,
    /// The key every call presents, and none where the store requires none.
    pub key: Option<Secret>,
    /// How long one call may take before the store is given up on.
    pub timeout: Duration,
}

/// A client bound to one qdrant.
///
/// One client per retriever, holding a connection pool. The collection is a caller's argument
/// here, unlike a plane in `ip-drsg`: a tenant's retriever names its own collection and the
/// client is built for that retriever, so what bounds a look-up is the row rather than the
/// client.
#[derive(Debug)]
pub struct QdrantClient {
    /// Where it calls, what it presents, and how long it waits.
    settings: QdrantSettings,
    /// The pool every call goes over.
    http: Pooled,
}

impl QdrantClient {
    /// A client over the configured endpoint and credential.
    #[must_use]
    pub fn new(settings: QdrantSettings) -> Self {
        let http = pooled(settings.timeout);
        Self { settings, http }
    }

    /// What a collection says about the vectors it holds.
    ///
    /// This is the call that makes a width mismatch answerable at registration rather than as an
    /// empty result later: qdrant reports the size of its own vectors, which a plane does not.
    pub async fn collection(
        &self,
        collection: &CollectionName,
        vector: Option<&VectorName>,
    ) -> Result<Collection, QdrantError> {
        let answer = self
            .send(
                Method::GET,
                &format!("/collections/{}", collection.as_str()),
                None,
                collection,
            )
            .await?;
        let vectors = &answer["result"]["config"]["params"]["vectors"];
        // A collection with one unnamed vector holds `{size, distance}` here; one with several
        // holds a map of them, so the named vector picks an entry and the unnamed one is the
        // whole object.
        let described = match vector {
            Some(name) => &vectors[name.as_str()],
            None => vectors,
        };
        let width = described["size"]
            .as_u64()
            .and_then(|width| u32::try_from(width).ok())
            .ok_or_else(|| QdrantError::Malformed {
                detail: match vector {
                    Some(name) => format!("no vector named {name} with a width"),
                    None => "the collection reports no vector width".to_owned(),
                },
            })?;
        Ok(Collection {
            width,
            metric: described["distance"].as_str().and_then(Distance::named),
        })
    }

    /// What is nearest a query vector, with each hit's payload.
    ///
    /// **One interface, and `/points/search` is not it.** Qdrant's current api answers a look-up
    /// at `POST /collections/{c}/points/query` and the hits are at `result.points[]` rather than
    /// at `result[]`; `search`, `recommend` and `discover` were dropped from its `OpenAPI` spec,
    /// and its own JS client dropped them at 1.19.0.
    ///
    /// The payload is asked for and the vector is not: the payload is the text a model reads,
    /// and the embedding is what this deployment has no reason to carry back.
    pub async fn query(&self, asked: &Query) -> Result<Vec<Found>, QdrantError> {
        let mut body = serde_json::json!({
            "query": asked.query,
            "limit": asked.hits,
            "with_payload": true,
            "with_vector": false,
        });
        if let Some(body) = body.as_object_mut() {
            if let Some(vector) = &asked.vector {
                body.insert("using".to_owned(), serde_json::json!(vector.as_str()));
            }
            match &asked.filter {
                Some(filter) if !filter.is_empty() => {
                    let written =
                        serde_json::to_value(filter).map_err(|error| QdrantError::Malformed {
                            detail: format!("the filter will not serialize: {error}"),
                        })?;
                    body.insert("filter".to_owned(), written);
                }
                _ => {}
            }
        }

        let answer = self
            .send(
                Method::POST,
                &format!("/collections/{}/points/query", asked.collection.as_str()),
                Some(body),
                &asked.collection,
            )
            .await?;
        let points = &answer["result"]["points"];
        let Some(listed) = points.as_array() else {
            return Err(QdrantError::Malformed {
                detail: "a look-up answers its hits at result.points".to_owned(),
            });
        };
        listed
            .iter()
            .map(|point| {
                serde_json::from_value(point.clone()).map_err(|error| QdrantError::Malformed {
                    detail: format!("a hit holds something else: {error}"),
                })
            })
            .collect()
    }

    /// Carries one call to the store and reads what came back.
    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
        collection: &CollectionName,
    ) -> Result<serde_json::Value, QdrantError> {
        let payload = match &body {
            Some(body) => serde_json::to_vec(body).map_err(|error| QdrantError::Malformed {
                detail: format!("the call will not serialize: {error}"),
            })?,
            None => Vec::new(),
        };
        // A collection name is a key — ascii letters, digits and `-_.` — so it needs no escaping
        // to sit in a path, and its own type is what guarantees that.
        let uri = format!(
            "{}{path}",
            self.settings.endpoint.as_str().trim_end_matches('/')
        );
        let mut built = Request::builder()
            .method(method)
            .uri(&uri)
            .header(header::ACCEPT, "application/json");
        if body.is_some() {
            built = built.header(header::CONTENT_TYPE, "application/json");
        }
        if let Some(key) = &self.settings.key {
            let presented =
                HeaderValue::from_str(key.expose()).map_err(|_| QdrantError::Unauthorized)?;
            built = built.header(API_KEY, presented);
        }
        let request = built
            .body(Full::new(Bytes::from(payload)))
            .map_err(|error| QdrantError::Unreachable {
                detail: error.to_string(),
            })?;

        let answered = tokio::time::timeout(self.settings.timeout, self.http.request(request))
            .await
            .map_err(|_| QdrantError::Unreachable {
                detail: format!(
                    "no answer within {} seconds",
                    self.settings.timeout.as_secs()
                ),
            })?
            .map_err(|error| QdrantError::Unreachable {
                detail: error.to_string(),
            })?;

        let status = answered.status();
        let read = Limited::new(answered.into_body(), MAX_ANSWER_BYTES)
            .collect()
            .await
            .map_err(|_| QdrantError::Unreachable {
                detail: format!("the answer is longer than {MAX_ANSWER_BYTES} bytes"),
            })?
            .to_bytes();

        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(QdrantError::Unauthorized);
        }
        if status == StatusCode::NOT_FOUND {
            return Err(QdrantError::Absent {
                collection: collection.as_str().to_owned(),
            });
        }
        if !status.is_success() {
            return Err(QdrantError::Refused {
                status: status.as_u16(),
                message: said(&read).unwrap_or_else(|| format!("the store answered {status}")),
            });
        }

        serde_json::from_slice(&read).map_err(|error| QdrantError::Malformed {
            detail: error.to_string(),
        })
    }
}

/// What the store said about a refusal, in its own words.
///
/// Qdrant's `status` is the word `ok` on an answer and `{error: …}` on a refusal, so a refusal's
/// reason is read out of the object and a body that is not one leaves the status to speak.
fn said(body: &[u8]) -> Option<String> {
    let answer: serde_json::Value = serde_json::from_slice(body).ok()?;
    answer["status"]["error"]
        .as_str()
        .map(ToOwned::to_owned)
        .filter(|said| !said.is_empty())
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
    use crate::look::{Filter, PointId};
    use crate::testing::{Answers, STUB_KEY as KEY, Seen, SilentQdrant, StubQdrant, stub_settings};

    fn collection() -> CollectionName {
        CollectionName::new("handbook").expect("a collection name")
    }

    fn vector(name: &str) -> VectorName {
        VectorName::new(name).expect("a vector name")
    }

    fn asked(filter: Option<Filter>) -> Query {
        Query {
            collection: collection(),
            vector: None,
            query: vec![0.5, 0.25],
            hits: 4,
            filter,
        }
    }

    async fn client(answers: Answers) -> (QdrantClient, StubQdrant) {
        let stub = StubQdrant::start(answers).await;
        (stub.client(), stub)
    }

    fn only(stub: &StubQdrant) -> Seen {
        stub.only()
    }

    /// The width is what registration compares against an embedder's own, so it is read from
    /// where qdrant actually reports it.
    #[tokio::test]
    async fn a_collection_reports_the_width_and_the_metric_of_its_vectors() {
        let (client, record) = client(Answers::Collection {
            vectors: serde_json::json!({"size": 384, "distance": "Cosine"}),
        })
        .await;

        let described = client
            .collection(&collection(), None)
            .await
            .expect("the store described its collection");

        assert_eq!(described.width, 384);
        assert_eq!(described.metric, Some(Distance::Cosine));
        let seen = only(&record);
        assert_eq!(seen.method, "GET");
        assert_eq!(seen.path, "/collections/handbook");
    }

    /// A collection with more than one vector describes each under its name, and the retriever
    /// says which it searches.
    #[tokio::test]
    async fn a_named_vector_is_described_under_its_own_name() {
        let (client, _) = client(Answers::Collection {
            vectors: serde_json::json!({
                "title": {"size": 128, "distance": "Dot"},
                "body": {"size": 384, "distance": "Euclid"},
            }),
        })
        .await;

        let described = client
            .collection(&collection(), Some(&vector("body")))
            .await
            .expect("the store described the named vector");

        assert_eq!(described.width, 384);
        assert_eq!(described.metric, Some(Distance::Euclid));
    }

    #[tokio::test]
    async fn a_vector_the_collection_does_not_have_is_said_rather_than_guessed() {
        let (client, _) = client(Answers::Collection {
            vectors: serde_json::json!({"body": {"size": 384, "distance": "Cosine"}}),
        })
        .await;

        match client
            .collection(&collection(), Some(&vector("title")))
            .await
        {
            Err(QdrantError::Malformed { detail }) => assert!(detail.contains("title"), "{detail}"),
            other => panic!("a missing vector was read as {other:?}"),
        }
    }

    /// A metric this build has not heard of is not a reason to refuse a registration, because
    /// nothing here acts on the metric.
    #[tokio::test]
    async fn a_metric_this_build_does_not_know_is_still_a_width() {
        let (client, _) = client(Answers::Collection {
            vectors: serde_json::json!({"size": 384, "distance": "Hamming"}),
        })
        .await;

        let described = client
            .collection(&collection(), None)
            .await
            .expect("an unknown metric is not a refusal");
        assert_eq!(described.width, 384);
        assert_eq!(described.metric, None);
    }

    /// The whole of the third finding: one interface, and the hits are nested.
    #[tokio::test]
    async fn a_look_up_goes_to_points_query_and_reads_its_hits_out_of_result_points() {
        let (client, record) = client(Answers::Points).await;

        let found = client
            .query(&asked(None))
            .await
            .expect("the store answered points");

        let seen = only(&record);
        assert_eq!(seen.method, "POST");
        assert_eq!(
            seen.path, "/collections/handbook/points/query",
            "`/points/search` was removed from qdrant's api, and from its own client at 1.19.0"
        );
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].id, PointId::Number(42));
        assert_eq!(found[0].payload["title"], "Leave");
        assert_eq!(found[1].id, PointId::Text("6b3f-7a".to_owned()));
    }

    /// Hits at `result[]` are the shape of an api that no longer exists, and reading them as none
    /// would be a wrong answer rather than a refusal.
    #[tokio::test]
    async fn an_answer_shaped_like_the_api_that_was_removed_is_refused() {
        let (client, _) = client(Answers::Flat).await;
        match client.query(&asked(None)).await {
            Err(QdrantError::Malformed { detail }) => {
                assert!(detail.contains("result.points"), "{detail}");
            }
            other => panic!("a flat result was read as {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_look_up_asks_for_the_payload_and_not_for_the_vector() {
        let (client, record) = client(Answers::Points).await;
        let _ = client.query(&asked(None)).await;
        let seen = only(&record);
        assert_eq!(seen.body["query"], serde_json::json!([0.5, 0.25]));
        assert_eq!(seen.body["limit"], 4);
        assert_eq!(seen.body["with_payload"], true);
        assert_eq!(seen.body["with_vector"], false);
        assert!(seen.body.get("filter").is_none(), "{:?}", seen.body);
        assert!(seen.body.get("using").is_none(), "{:?}", seen.body);
    }

    #[tokio::test]
    async fn a_retriever_that_names_a_vector_says_which_one_to_use() {
        let (client, record) = client(Answers::Points).await;
        let _ = client
            .query(&Query {
                vector: Some(vector("body")),
                ..asked(None)
            })
            .await;
        assert_eq!(only(&record).body["using"], "body");
    }

    /// The conditions cross the wire as they were written, and nothing beside them does.
    #[tokio::test]
    async fn a_filter_crosses_the_wire_as_the_model_wrote_it() {
        let (client, record) = client(Answers::Points).await;
        let _ = client
            .query(&asked(Some(Filter {
                must: vec![serde_json::json!({"key": "team", "match": {"value": "ops"}})],
                ..Filter::default()
            })))
            .await;
        let seen = only(&record);
        assert_eq!(seen.body["filter"]["must"][0]["key"], "team");
        assert_eq!(seen.body["filter"]["must"][0]["match"]["value"], "ops");
        assert_eq!(
            seen.body["collection"],
            serde_json::Value::Null,
            "the collection travels in the path and nowhere else"
        );
    }

    #[tokio::test]
    async fn a_filter_that_narrows_nothing_is_not_sent_at_all() {
        let (client, record) = client(Answers::Points).await;
        let _ = client.query(&asked(Some(Filter::default()))).await;
        assert!(only(&record).body.get("filter").is_none());
    }

    #[tokio::test]
    async fn a_key_is_presented_in_the_header_qdrant_takes_it_in() {
        let (client, record) = client(Answers::Points).await;
        let _ = client.query(&asked(None)).await;
        assert_eq!(only(&record).api_key.as_deref(), Some(KEY));
    }

    /// A qdrant that requires no key is a deployment this gateway must still be able to read.
    #[tokio::test]
    async fn a_store_that_requires_no_key_is_sent_none() {
        let stub = StubQdrant::start(Answers::Points).await;
        let mut settings = stub_settings(stub.address());
        settings.key = None;
        let client = QdrantClient::new(settings);

        let _ = client.query(&asked(None)).await;

        assert_eq!(stub.only().api_key, None);
    }

    #[tokio::test]
    async fn a_collection_that_is_not_there_is_named_rather_than_read_as_empty() {
        let (client, _) = client(Answers::Absent).await;
        assert_eq!(
            client.query(&asked(None)).await,
            Err(QdrantError::Absent {
                collection: "handbook".to_owned()
            })
        );
        assert_eq!(
            client.collection(&collection(), None).await,
            Err(QdrantError::Absent {
                collection: "handbook".to_owned()
            })
        );
    }

    /// What a width mismatch actually looks like from qdrant: a 400 in its own words, which is
    /// worth carrying rather than flattening into "the store refused".
    #[tokio::test]
    async fn a_store_that_declines_the_call_says_why_and_that_is_kept() {
        let (client, _) = client(Answers::Wrong).await;
        match client.query(&asked(None)).await {
            Err(QdrantError::Refused { status, message }) => {
                assert_eq!(status, 400);
                assert!(message.contains("dimension"), "{message}");
            }
            other => panic!("a 400 was read as {other:?}"),
        }
    }

    /// A refusal with no words of its own still says what happened.
    #[tokio::test]
    async fn a_refusal_that_says_nothing_is_reported_by_its_status() {
        let (client, _) = client(Answers::Silent).await;
        match client.query(&asked(None)).await {
            Err(QdrantError::Refused { status, message }) => {
                assert_eq!(status, 500);
                assert!(message.contains("500"), "{message}");
            }
            other => panic!("a wordless 500 was read as {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_store_that_will_not_take_the_key_is_reported_as_that() {
        let (client, _) = client(Answers::Unauthorized).await;
        assert_eq!(
            client.query(&asked(None)).await,
            Err(QdrantError::Unauthorized)
        );
    }

    #[tokio::test]
    async fn an_answer_that_is_not_json_is_not_an_answer() {
        let (client, _) = client(Answers::Garbage).await;
        assert!(matches!(
            client.query(&asked(None)).await,
            Err(QdrantError::Malformed { .. })
        ));
    }

    /// A store answering with a corpus is a store this gateway will not read, because what it
    /// answers with lands in a prompt.
    ///
    /// The one test here given a long clock: eight mebibytes take longer to write than a stub's
    /// own 300 milliseconds allow when a test runs per core, and what failed then would be the
    /// clock rather than the ceiling.
    #[tokio::test]
    async fn an_answer_past_the_ceiling_is_refused_rather_than_read() {
        let stub = StubQdrant::start(Answers::Enormous).await;
        let mut settings = stub_settings(stub.address());
        settings.timeout = Duration::from_secs(30);
        let client = QdrantClient::new(settings);

        match client.query(&asked(None)).await {
            Err(QdrantError::Unreachable { detail }) => {
                assert!(detail.contains("longer than"), "{detail}");
            }
            other => panic!("an answer past the ceiling was read as {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_store_that_never_answers_is_given_up_on() {
        let client = SilentQdrant::start().await.client();
        assert!(matches!(
            client.query(&asked(None)).await,
            Err(QdrantError::Unreachable { .. })
        ));
    }

    /// The `Secret` contract, asserted here because this is the crate that presents it.
    #[tokio::test]
    async fn the_credential_never_appears_in_a_debug_or_in_a_refusal() {
        let (client, _) = client(Answers::Wrong).await;
        let printed = format!("{client:?}");
        assert!(!printed.contains(KEY), "the client printed its own key");

        let refusal = client.query(&asked(None)).await;
        assert!(
            !format!("{refusal:?}").contains(KEY),
            "a refusal carried it"
        );
        let Err(error) = refusal else {
            panic!("the store was supposed to refuse");
        };
        assert!(!error.to_string().contains(KEY), "a message carried it");
    }

    /// An endpoint an operator wrote with a trailing slash names the same store as one without.
    #[tokio::test]
    async fn a_path_is_joined_to_the_endpoint_however_it_was_written() {
        let stub = StubQdrant::start(Answers::Points).await;
        let mut settings = stub_settings(stub.address());
        settings.endpoint =
            RetrieverEndpoint::new(&format!("http://{}/", stub.address())).expect("an endpoint");
        let client = QdrantClient::new(settings);

        let _ = client.query(&asked(None)).await;

        assert_eq!(stub.only().path, "/collections/handbook/points/query");
    }
}
