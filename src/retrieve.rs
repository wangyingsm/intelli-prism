//! Reaching whichever backend a retriever names.
//!
//! The two backends are two crates that know nothing of each other — `ip-drsg` speaks JSON-RPC to
//! a plane, `ip-qdrant` REST to a collection — and this is the one place that holds either. The
//! split is the one [`ip_gateway::Embedder`] already makes: the crates carry the wire, and the
//! binary is where a stored row becomes a client.

use std::sync::Arc;
use std::time::Duration;

use ip_config::Secret;
use ip_drsg::{DrsgClient, DrsgEndpoint, DrsgError, DrsgSettings, PlaneSource};
use ip_qdrant::{CollectionSource, QdrantClient, QdrantError, QdrantSettings};
use ip_retrieve::{Retrieval, Retriever, RetrieverAuth, Source};
use ip_run::Held;
use ip_storage::{AgentOwner, RetrieverOwner};

use crate::error::Unanswered;

/// How long one call to a retriever's own backend may take.
///
/// Longer than a graph store's, because this one is an operator's own machine reached while they
/// wait for a `PUT` rather than a store this deployment was configured with and has already
/// spoken to.
const BACKEND_TIMEOUT: Duration = Duration::from_secs(10);

/// A client for whichever backend a retriever names, holding what a look-up of it needs.
///
/// Each variant is the crate's own source type — the client with the retriever's scope beside it,
/// which is what [`Retrieval`] is implemented on — so nothing downstream has to match on
/// [`Source`] again, and what a run dispatches through is built here and nowhere else.
#[derive(Debug)]
pub enum Backend {
    /// A `dr-strange` plane.
    Drsg(PlaneSource),
    /// A `qdrant` collection.
    Qdrant(CollectionSource),
}

impl Backend {
    /// Opens the backend a retriever names, or says what about it this cannot speak to.
    ///
    /// Nothing is reached here. What can fail is the endpoint, which each client validates for
    /// itself and more narrowly than [`ip_retrieve::RetrieverEndpoint`] does: the row's own type
    /// checks the scheme and the host as text, while a client parses the whole of it as a url. A
    /// plane name cannot fail — the row's ceiling is 64 bytes against the client's 128, and both
    /// refuse whitespace — but it is still answered rather than unwrapped.
    pub fn open(retriever: &Retriever) -> Result<Self, Unanswered> {
        // A plane takes its credential as a bearer token and a collection in a header of its own,
        // so what is held is the key and the presenting is each client's.
        let key = match retriever.auth() {
            RetrieverAuth::None => None,
            RetrieverAuth::Key { key } => Some(Secret::from(key.expose())),
        };
        match retriever.source() {
            Source::Drsg {
                plane,
                label,
                property,
            } => {
                let endpoint = DrsgEndpoint::new(retriever.endpoint().as_str())
                    .map_err(|error| Unanswered::Refused(error.to_string()))?;
                let plane = ip_drsg::PlaneName::new(plane.as_str())
                    .map_err(|error| Unanswered::Refused(error.to_string()))?;
                Ok(Self::Drsg(PlaneSource::new(
                    DrsgClient::new(DrsgSettings {
                        endpoint,
                        // A plane behind no token takes the empty one it is presented and ignores
                        // it, which is what a retriever written with no auth asks for.
                        token: key.unwrap_or_else(|| Secret::from("")),
                        plane,
                        timeout: BACKEND_TIMEOUT,
                    }),
                    label.clone(),
                    property.clone(),
                )))
            }
            Source::Qdrant { collection, vector } => Ok(Self::Qdrant(CollectionSource::new(
                QdrantClient::new(QdrantSettings {
                    endpoint: retriever.endpoint().clone(),
                    key,
                    timeout: BACKEND_TIMEOUT,
                }),
                collection.clone(),
                vector.clone(),
            ))),
        }
    }

    /// How wide the vectors it holds are, none where it holds none to measure, and what it said
    /// where it would not say.
    ///
    /// The two backends answer this differently, and the difference is theirs rather than ours: a
    /// collection declares its width when it is created and reports it, while a plane reports no
    /// width at all and has to be measured by reading what it holds. A plane holding nothing under
    /// the property named is the one case that is neither a width nor a failure.
    pub async fn width(&self) -> Result<Option<u32>, Unanswered> {
        match self {
            Self::Drsg(plane) => plane.width().await.map_err(plane_said),
            Self::Qdrant(collection) => collection
                .described()
                .await
                .map(|held| Some(held.width))
                .map_err(store_said),
        }
    }

    /// What a run looks something up through.
    ///
    /// Shared rather than owned, because one run holds a look-up per retriever its template
    /// names and a tool may be called on every turn: the connection pool inside the client is
    /// what is being kept.
    #[must_use]
    pub fn retrieval(self) -> Arc<dyn Retrieval> {
        match self {
            Self::Drsg(plane) => Arc::new(plane),
            Self::Qdrant(collection) => Arc::new(collection),
        }
    }
}

/// Whose retrievers a run reads, which is whose template it is running.
///
/// The deployment's own template runs with the deployment's own retrievers and a tenant's with
/// that tenant's — [`templates_of`](crate::routes::graph::templates_of) applied once more, and
/// for its reason: a tenant's run must not reach another tenant's documents by naming an id, and
/// a global template is the deployment's own work.
#[must_use]
pub fn retrievers_of(owner: &AgentOwner) -> RetrieverOwner {
    match owner {
        AgentOwner::Global => RetrieverOwner::Global,
        AgentOwner::Tenant(tenant) => RetrieverOwner::Tenant(tenant.clone()),
    }
}

/// Opens every stored retriever against its backend, so a run can address it.
///
/// Refused rather than skipped where one will not open: a retriever a template names and this
/// could not open would otherwise read to the run as a retriever that is not registered, which
/// names the wrong thing to fix. Nothing is reached here — see [`Backend::open`].
pub fn held(stored: Vec<Retriever>) -> Result<Vec<Held>, Unanswered> {
    stored
        .into_iter()
        .map(|retriever| {
            Ok(Held {
                lookup: Backend::open(&retriever)?.retrieval(),
                retriever,
            })
        })
        .collect()
}

/// What a plane's refusal means to a caller.
fn plane_said(error: DrsgError) -> Unanswered {
    let said = error.to_string();
    match error {
        DrsgError::Unreachable { .. } => Unanswered::Unreachable(said),
        DrsgError::Refused { .. } | DrsgError::Unauthorized | DrsgError::Malformed { .. } => {
            Unanswered::Refused(said)
        }
    }
}

/// What a collection's refusal means to a caller.
fn store_said(error: QdrantError) -> Unanswered {
    let said = error.to_string();
    match error {
        QdrantError::Unreachable { .. } => Unanswered::Unreachable(said),
        QdrantError::Absent { .. }
        | QdrantError::Refused { .. }
        | QdrantError::Unauthorized
        | QdrantError::Malformed { .. } => Unanswered::Refused(said),
    }
}

#[cfg(test)]
mod tests {
    use ip_qdrant::testing::STUB_KEY;
    use ip_qdrant::{Answers, StubQdrant};
    use ip_retrieve::{
        CollectionName, MaxHits, MetaSchema, NodeLabel, RetrieverEndpoint, RetrieverId,
        RetrieverName, RetrieverParts, RetrieverTool, RetrieverTools, VectorProperty,
    };

    use super::*;

    /// A retriever over `endpoint`, of whichever kind `source` is.
    fn retriever(endpoint: &str, source: Source, key: Option<&str>) -> Retriever {
        Retriever::new(RetrieverParts {
            id: RetrieverId::new("handbook").expect("an id"),
            name: RetrieverName::new("The handbook").expect("a name"),
            endpoint: RetrieverEndpoint::new(endpoint).expect("an endpoint"),
            auth: match key {
                Some(key) => RetrieverAuth::Key {
                    key: key.to_owned().try_into().expect("a key"),
                },
                None => RetrieverAuth::None,
            },
            source,
            tools: RetrieverTools::new(vec![RetrieverTool::Search]).expect("one tool"),
            meta: MetaSchema::default(),
            max_hits: MaxHits::default(),
        })
        .expect("the parts agree")
    }

    fn plane(endpoint: &str) -> Retriever {
        retriever(
            endpoint,
            Source::Drsg {
                plane: "handbook".to_owned().try_into().expect("a plane name"),
                label: Some(NodeLabel::new("Document").expect("a label")),
                property: VectorProperty::new("embedding").expect("a property"),
            },
            Some("drsg-token"),
        )
    }

    fn collection(endpoint: &str, key: Option<&str>) -> Retriever {
        retriever(
            endpoint,
            Source::Qdrant {
                collection: CollectionName::new("handbook").expect("a collection"),
                vector: None,
            },
            key,
        )
    }

    /// A collection reports its width, which is the whole of what registration compares against.
    #[tokio::test]
    async fn a_collection_says_how_wide_its_vectors_are() {
        let stub = StubQdrant::start(Answers::Collection {
            vectors: serde_json::json!({"size": 384, "distance": "Cosine"}),
        })
        .await;

        let backend = Backend::open(&collection(
            &format!("http://{}", stub.address()),
            Some(STUB_KEY),
        ))
        .expect("the endpoint is one");

        assert_eq!(backend.width().await, Ok(Some(384)));
        assert_eq!(stub.only().api_key.as_deref(), Some(STUB_KEY));
    }

    /// A collection that is not there is the operator's to fix, and a store that is not listening
    /// is not: the two codes the api answers come from this difference.
    #[tokio::test]
    async fn a_store_that_answers_and_one_that_is_not_there_are_told_apart() {
        let absent = StubQdrant::start(Answers::Absent).await;
        let backend = Backend::open(&collection(
            &format!("http://{}", absent.address()),
            Some(STUB_KEY),
        ))
        .expect("the endpoint is one");
        match backend.width().await {
            Err(Unanswered::Refused(said)) => assert!(said.contains("handbook"), "{said}"),
            other => panic!("a missing collection was read as {other:?}"),
        }

        // A port nothing is listening on, which is what an operator's typo looks like.
        let nobody =
            Backend::open(&collection("http://127.0.0.1:1/", None)).expect("the endpoint is one");
        assert!(
            matches!(nobody.width().await, Err(Unanswered::Unreachable(_))),
            "a store nothing answers on is not a refusal"
        );
    }

    /// A key is presented to the backend that asked for it and never kept anywhere else.
    #[tokio::test]
    async fn a_store_requiring_no_key_is_sent_none() {
        let stub = StubQdrant::start(Answers::Collection {
            vectors: serde_json::json!({"size": 768, "distance": "Dot"}),
        })
        .await;

        let backend =
            Backend::open(&collection(&format!("http://{}", stub.address()), None)).expect("opens");

        assert_eq!(backend.width().await, Ok(Some(768)));
        assert_eq!(stub.only().api_key, None);
    }

    /// The parts beside the client are read once, so nothing downstream matches on the source
    /// again.
    #[test]
    fn each_kind_holds_what_a_look_up_of_it_needs() {
        match Backend::open(&plane("http://127.0.0.1:44453/rpc")).expect("opens") {
            Backend::Drsg(held) => {
                assert_eq!(held.label().map(NodeLabel::as_str), Some("Document"));
                assert_eq!(held.property().as_str(), "embedding");
            }
            other => panic!("a plane was opened as {other:?}"),
        }
        match Backend::open(&collection("http://127.0.0.1:6333", None)).expect("opens") {
            Backend::Qdrant(held) => {
                assert_eq!(held.collection().as_str(), "handbook");
                assert_eq!(held.vector(), None);
            }
            other => panic!("a collection was opened as {other:?}"),
        }
    }

    /// An endpoint each client refuses for itself, which is narrower than the row's own type: a
    /// pipe is not whitespace, so the row takes it, and no url holds one.
    #[test]
    fn an_endpoint_a_client_will_not_speak_to_is_the_operators_to_fix() {
        match Backend::open(&plane("http://files|acme.test/rpc")) {
            Err(Unanswered::Refused(said)) => assert!(said.contains("not a url"), "{said}"),
            other => panic!("an endpoint that is not a url was opened as {other:?}"),
        }
    }
}
