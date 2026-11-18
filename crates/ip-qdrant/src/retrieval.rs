//! A collection answering look-ups, as `ip-run` dispatches them.
//!
//! One verb of the five, because a vector store holds points and not edges: a collection is
//! registered offering `search` alone, which [`ip_retrieve::Retriever`] refuses to be built
//! otherwise, so the other four arms here are written for completeness and say what a model
//! would need to be told rather than being reachable.
//!
//! **A filter arrives as the model wrote it and is read into [`Filter`] here.** That is where
//! the refusal belongs: the type knows which conditions qdrant takes, and a model that invented
//! one is told so in the same breath as the schema that declared the field.

use ip_retrieve::{CollectionName, LookUp, Looked, Retrieval, VectorName};

use crate::client::QdrantClient;
use crate::error::QdrantError;
use crate::look::{Collection, Filter, Found, Query};

/// One collection, with what a look-up of it needs.
#[derive(Debug)]
pub struct CollectionSource {
    client: QdrantClient,
    collection: CollectionName,
    vector: Option<VectorName>,
}

impl CollectionSource {
    /// Binds a client to the collection and named vector a retriever named.
    #[must_use]
    pub const fn new(
        client: QdrantClient,
        collection: CollectionName,
        vector: Option<VectorName>,
    ) -> Self {
        Self {
            client,
            collection,
            vector,
        }
    }

    /// What the collection says of itself, the declared width among it.
    pub async fn described(&self) -> Result<Collection, QdrantError> {
        self.client
            .collection(&self.collection, self.vector.as_ref())
            .await
    }

    /// Which collection a look-up reads.
    #[must_use]
    pub const fn collection(&self) -> &CollectionName {
        &self.collection
    }

    /// Which of its named vectors, and its only one where it names none.
    #[must_use]
    pub const fn vector(&self) -> Option<&VectorName> {
        self.vector.as_ref()
    }
}

#[async_trait::async_trait]
impl Retrieval for CollectionSource {
    async fn look(&self, asked: &LookUp) -> Result<Vec<Looked>, String> {
        let LookUp::Search {
            query,
            hits,
            filter,
        } = asked
        else {
            return Err(format!(
                "a collection holds points and no edges, so it cannot answer `{}`",
                asked.tool().name()
            ));
        };
        self.client
            .query(&Query {
                collection: self.collection.clone(),
                vector: self.vector.clone(),
                query: query.clone(),
                hits: *hits,
                filter: narrowed(filter.as_ref())?,
            })
            .await
            .map(|found| found.iter().map(point).collect())
            .map_err(|error| error.to_string())
    }
}

/// What a model wrote as a filter, read into one, and nothing where it wrote none.
///
/// The refusal names the field and what was wrong with it, because the model can fix it: it was
/// told which fields it may narrow by, and the conditions are qdrant's own.
fn narrowed(written: Option<&serde_json::Value>) -> Result<Option<Filter>, String> {
    let Some(written) = written else {
        return Ok(None);
    };
    serde_json::from_value(written.clone())
        .map(Some)
        .map_err(|error| format!("`filter` is not one qdrant takes: {error}"))
}

/// One point, as a model reads it.
fn point(found: &Found) -> Looked {
    Looked {
        id: found.id.to_string(),
        score: Some(found.score),
        payload: found.payload.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ip_config::Secret;
    use ip_retrieve::RetrieverEndpoint;

    use super::*;
    use crate::client::QdrantSettings;
    use crate::testing::{Answers, STUB_KEY, StubQdrant};

    /// A source over the stub at `address`, scoped as a retriever would scope it.
    fn source(address: std::net::SocketAddr, vector: Option<&str>) -> CollectionSource {
        CollectionSource::new(
            QdrantClient::new(QdrantSettings {
                endpoint: RetrieverEndpoint::new(&format!("http://{address}"))
                    .expect("an endpoint"),
                key: Some(Secret::from(STUB_KEY)),
                timeout: Duration::from_secs(5),
            }),
            CollectionName::new("handbook").expect("a collection"),
            vector.map(|name| VectorName::new(name).expect("a vector")),
        )
    }

    fn searching(filter: Option<serde_json::Value>) -> LookUp {
        LookUp::Search {
            query: vec![0.1, 0.2, 0.3],
            hits: 2,
            filter,
        }
    }

    /// What a model is shown of a hit: its id, how near it was, and the payload it carries.
    #[tokio::test]
    async fn a_search_answers_the_points_it_found() {
        let stub = StubQdrant::start(Answers::Points).await;

        let looked = source(stub.address(), None)
            .look(&searching(None))
            .await
            .expect("the store answered");

        assert_eq!(looked.len(), 2);
        assert_eq!(looked[0].id, "42");
        assert_eq!(looked[0].score, Some(0.75));
        assert_eq!(looked[0].payload["title"], "Leave");
        assert_eq!(
            looked[1].id, "6b3f-7a",
            "a point addressed by a uuid is addressed by it in the answer too"
        );

        let seen = stub.only();
        assert_eq!(seen.path, "/collections/handbook/points/query");
        assert_eq!(seen.body["limit"], 2);
        assert_eq!(seen.body["with_payload"], true);
        assert_eq!(seen.body["with_vector"], false);
        assert!(seen.body.get("using").is_none());
        assert!(seen.body.get("filter").is_none());
        assert_eq!(seen.api_key.as_deref(), Some(STUB_KEY));
    }

    /// The named vector is the retriever's, so a collection holding several is read on the one
    /// its operator registered.
    #[tokio::test]
    async fn a_named_vector_is_what_the_look_up_compares_against() {
        let stub = StubQdrant::start(Answers::Points).await;

        source(stub.address(), Some("text"))
            .look(&searching(None))
            .await
            .expect("the store answered");

        assert_eq!(stub.only().body["using"], "text");
    }

    /// What the model wrote reaches the store, which is the whole point of declaring the fields.
    #[tokio::test]
    async fn a_filter_a_model_wrote_reaches_the_store() {
        let stub = StubQdrant::start(Answers::Points).await;

        source(stub.address(), None)
            .look(&searching(Some(serde_json::json!({
                "must": [{"key": "team", "match": {"value": "ops"}}]
            }))))
            .await
            .expect("the store answered");

        assert_eq!(
            stub.only().body["filter"]["must"][0]["key"],
            "team",
            "the condition crossed as the model wrote it"
        );
    }

    /// A model that invented a condition is told so and may try again, which is what a failed
    /// tool result is for.
    #[tokio::test]
    async fn a_filter_qdrant_would_not_take_is_refused_before_the_call() {
        let stub = StubQdrant::start(Answers::Points).await;

        let said = source(stub.address(), None)
            .look(&searching(Some(serde_json::json!({"nearly": "a filter"}))))
            .await
            .expect_err("a filter that is not one is not a look-up");

        assert!(said.contains("`filter`"), "{said}");
        assert!(
            stub.seen().is_empty(),
            "a filter the store would refuse cost a call anyway"
        );
    }

    /// Every refusal a store can answer with reaches the model as what it said.
    #[tokio::test]
    async fn a_store_that_refuses_says_why_rather_than_answering_nothing() {
        let absent = StubQdrant::start(Answers::Absent).await;
        let said = source(absent.address(), None)
            .look(&searching(None))
            .await
            .expect_err("a missing collection is not an answer");
        assert!(said.contains("handbook"), "{said}");

        let wrong = StubQdrant::start(Answers::Wrong).await;
        let said = source(wrong.address(), None)
            .look(&searching(None))
            .await
            .expect_err("a refused query is not an answer");
        assert!(!said.is_empty(), "a model was told nothing");
    }

    /// Nothing near enough is an answer: the model asked, and what it is told is that there is
    /// nothing there.
    #[tokio::test]
    async fn a_collection_with_nothing_near_answers_nothing_rather_than_failing() {
        let stub = StubQdrant::start(Answers::Collection {
            vectors: serde_json::json!({"size": 384, "distance": "Cosine"}),
        })
        .await;
        let held = source(stub.address(), None);

        // The width probe and the look-up are the same client, so this is also the one place
        // that shows a registered collection reporting what it declared.
        assert_eq!(held.described().await.map(|held| held.width), Ok(384));
        assert_eq!(held.collection().as_str(), "handbook");
        assert_eq!(held.vector(), None);
    }

    /// Unreachable through a registered retriever, which refuses these four on a collection —
    /// so what is checked is that the arm says something a model could act on.
    #[tokio::test]
    async fn a_collection_asked_to_walk_edges_says_it_holds_none() {
        let stub = StubQdrant::start(Answers::Points).await;

        let said = source(stub.address(), None)
            .look(&LookUp::Rank { hits: 3 })
            .await
            .expect_err("a collection ranks nothing");

        assert!(said.contains("rank"), "{said}");
        assert!(stub.seen().is_empty(), "it asked the store anyway");
    }
}
