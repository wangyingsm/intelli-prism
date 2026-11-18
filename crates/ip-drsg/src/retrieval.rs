//! A plane answering look-ups, as `ip-run` dispatches them.
//!
//! The five verbs already live on [`DrsgClient`]; what is here is the one thing a run needs of
//! them — [`Retrieval`], whose shape is the same for both backends — and the mapping onto it.
//! A client alone cannot answer a look-up, because which label and which property hold the
//! embeddings are the retriever's and not the client's, so this is the client with those beside
//! it.
//!
//! **Every answer becomes one [`Looked`] per row, whatever the verb.** Three of the five answer
//! ids rather than records, which is the store's shape rather than a choice here: a ranking and
//! a clustering carry no properties, and the declaration a model reads says so and points it at
//! `neighbors`. Hydrating them would be one call per row.

use ip_retrieve::{LookUp, Looked, NodeLabel, Retrieval, VectorProperty};

use crate::client::DrsgClient;
use crate::error::DrsgError;
use crate::look::{Across, Apart, Around, Direction, Grouped, Hit, Hop, Nearest, Path, Placed};

/// One plane, with what a look-up of it needs.
#[derive(Debug)]
pub struct PlaneSource {
    client: DrsgClient,
    label: Option<NodeLabel>,
    property: VectorProperty,
}

impl PlaneSource {
    /// Binds a client to the label and property a retriever named.
    #[must_use]
    pub const fn new(
        client: DrsgClient,
        label: Option<NodeLabel>,
        property: VectorProperty,
    ) -> Self {
        Self {
            client,
            label,
            property,
        }
    }

    /// How wide the embeddings it holds are, and none where it holds none to measure.
    pub async fn width(&self) -> Result<Option<u32>, DrsgError> {
        self.client.width(self.label.as_ref(), &self.property).await
    }

    /// Which label a look-up is scoped to, and every label where it is scoped to none.
    #[must_use]
    pub const fn label(&self) -> Option<&NodeLabel> {
        self.label.as_ref()
    }

    /// Which property holds the embeddings.
    #[must_use]
    pub const fn property(&self) -> &VectorProperty {
        &self.property
    }
}

#[async_trait::async_trait]
impl Retrieval for PlaneSource {
    /// Asks the verb this look-up is of, and renders what came back.
    ///
    /// **The metric is whatever the store assumes**, which is cosine, because a retriever says
    /// which property holds its embeddings and not how that index compares them. A plane built
    /// with dot product or `l2` is therefore searched by cosine, which is a caveat of this
    /// milestone rather than a thing a caller can correct here.
    async fn look(&self, asked: &LookUp) -> Result<Vec<Looked>, String> {
        match asked {
            LookUp::Search { query, hits, .. } => self
                .client
                .search(&Nearest {
                    property: self.property.clone(),
                    label: self.label.clone(),
                    query: query.clone(),
                    hits: *hits,
                    metric: None,
                })
                .await
                .map(|found| found.iter().map(hit).collect())
                .map_err(said),
            LookUp::Neighbors {
                node,
                direction,
                edge_type,
            } => self
                .client
                .neighbors(&Around {
                    node: *node,
                    direction: whichever(*direction),
                    edge_type: edge_type.clone(),
                })
                .await
                .map(|walked| walked.iter().map(hop).collect())
                .map_err(said),
            LookUp::Rank { hits } => self
                .client
                .rank(&self.across(*hits))
                .await
                .map(|ranked| ranked.iter().map(placed).collect())
                .map_err(said),
            LookUp::Cluster { hits } => self
                .client
                .cluster(&self.across(*hits))
                .await
                .map(|grouped| grouped.iter().map(group).collect())
                .map_err(said),
            LookUp::Relate {
                from,
                to,
                direction,
            } => self
                .client
                .relate(&Apart {
                    from: *from,
                    to: *to,
                    direction: whichever(*direction),
                })
                .await
                .map(|path| path.iter().map(|path| route(*to, path)).collect())
                .map_err(said),
        }
    }
}

impl PlaneSource {
    /// What a ranking or a clustering is told, which is this retriever's scope and a count.
    fn across(&self, hits: u32) -> Across {
        Across {
            label: self.label.clone(),
            limit: hits,
        }
    }
}

/// One scored node, as a model reads it.
fn hit(found: &Hit) -> Looked {
    Looked {
        id: found.id.to_string(),
        score: found.score,
        payload: carried(found),
    }
}

/// One hop, as a model reads it: where it went, and what it crossed to get there.
fn hop(walked: &Hop) -> Looked {
    Looked {
        id: walked.node.id.to_string(),
        score: walked.node.score,
        payload: serde_json::json!({
            "node": carried(&walked.node),
            "edge": {"type": walked.edge.kind, "properties": walked.edge.properties},
        }),
    }
}

/// One ranked node, which carries no properties for the reason this module's doc gives.
fn placed(ranked: &Placed) -> Looked {
    Looked {
        id: ranked.id.to_string(),
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a score read as f64 is rendered to a model, where f32 is precision enough"
        )]
        score: Some(ranked.score as f32),
        payload: serde_json::json!({}),
    }
}

/// One grouped node, named by the node its group is represented by.
fn group(grouped: &Grouped) -> Looked {
    Looked {
        id: grouped.id.to_string(),
        score: None,
        payload: serde_json::json!({"group": grouped.group}),
    }
}

/// The one path a relate answered with, addressed by where it ends.
///
/// A path has no id of its own, and the node a model asked about is the one it can go on to walk
/// from, so that is what addresses it.
fn route(to: u64, path: &Path) -> Looked {
    Looked {
        id: to.to_string(),
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a cost read as f64 is rendered to a model, where f32 is precision enough"
        )]
        score: Some(path.cost as f32),
        payload: serde_json::json!({"nodes": path.nodes, "edges": path.edges}),
    }
}

/// What one node carries, which is the text a model reads.
fn carried(found: &Hit) -> serde_json::Value {
    serde_json::json!({
        "key": found.key,
        "labels": found.labels,
        "properties": found.properties,
    })
}

/// The store's own word for a direction a model named.
const fn whichever(asked: ip_retrieve::Direction) -> Direction {
    match asked {
        ip_retrieve::Direction::Out => Direction::Out,
        ip_retrieve::Direction::In => Direction::In,
        ip_retrieve::Direction::Both => Direction::Both,
    }
}

/// What a plane's refusal reads as to a model.
///
/// The whole sentence, because every one of them names what this deployment's operator has to
/// fix — a plane that is not there, a token it will not take, an answer that is not one — and a
/// model told only "it failed" retries the same call.
fn said(error: DrsgError) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ip_config::Secret;

    use super::*;
    use crate::client::DrsgSettings;
    use crate::plane::{DrsgEndpoint, PlaneName};
    use crate::testing::{Replies, StubPlane};

    /// A source over the stub at `address`, scoped as a retriever would scope it.
    fn source(address: std::net::SocketAddr, label: Option<&str>) -> PlaneSource {
        PlaneSource::new(
            DrsgClient::new(DrsgSettings {
                endpoint: DrsgEndpoint::new(&format!("http://{address}/rpc")).expect("an endpoint"),
                token: Secret::from("plane-token"),
                plane: PlaneName::new("handbook").expect("a plane"),
                timeout: Duration::from_secs(5),
            }),
            label.map(|label| NodeLabel::new(label).expect("a label")),
            VectorProperty::new("embedding").expect("a property"),
        )
    }

    fn searching() -> LookUp {
        LookUp::Search {
            query: vec![0.1, 0.2, 0.3],
            hits: 2,
            filter: None,
        }
    }

    /// What a model is shown of a hit: its id, how near it was, and what it carries.
    #[tokio::test]
    async fn a_search_answers_the_nodes_it_found() {
        let stub = StubPlane::start(Replies::in_turn([serde_json::json!([
            {
                "id": 7,
                "external_key": "doc:leave",
                "labels": ["Document"],
                "properties": {"title": "Leave", "embedding": "$vector(384 dims, omitted)"},
                "score": 0.83,
            },
            {"id": 9, "labels": [], "properties": {"title": "Pay"}},
        ])]))
        .await;

        let looked = source(stub.address(), Some("Document"))
            .look(&searching())
            .await
            .expect("the plane answered");

        assert_eq!(looked.len(), 2);
        assert_eq!(looked[0].id, "7");
        assert_eq!(looked[0].score, Some(0.83));
        assert_eq!(looked[0].payload["key"], "doc:leave");
        assert_eq!(looked[0].payload["properties"]["title"], "Leave");
        assert_eq!(
            looked[0].payload["properties"]["embedding"], "$vector(384 dims, omitted)",
            "an embedding reaches a prompt as a marker and never as its floats"
        );
        assert_eq!(looked[1].score, None, "an unscored row is still a row");

        let seen = stub.only();
        assert_eq!(seen.method, "plane.search");
        assert_eq!(seen.params["plane"], "handbook");
        assert_eq!(seen.params["label"], "Document");
        assert_eq!(seen.params["property"], "embedding");
        assert_eq!(seen.params["k"], 2);
        assert_eq!(
            seen.params["query"].as_array().map(Vec::len),
            Some(3),
            "the query crosses as the vector it was handed"
        );
        assert!(
            seen.params.get("metric").is_none(),
            "an unsaid metric is the store's own default"
        );
    }

    /// The scope is the retriever's, so a plane holding more than one corpus is not searched
    /// whole by a retriever registered against one label.
    #[tokio::test]
    async fn a_retriever_scoped_to_no_label_says_so_rather_than_naming_one() {
        let stub = StubPlane::start(Replies::in_turn([serde_json::json!([])])).await;

        let looked = source(stub.address(), None)
            .look(&searching())
            .await
            .expect("the plane answered");

        assert!(looked.is_empty(), "nothing found is nothing, not a failure");
        assert!(stub.only().params.get("label").is_none());
    }

    /// A walk answers where it went and what it crossed, which is what makes a second hop
    /// possible.
    #[tokio::test]
    async fn a_walk_answers_the_edge_it_crossed_beside_the_node_it_reached() {
        let stub = StubPlane::start(Replies::in_turn([serde_json::json!([{
            "node": {"id": 9, "labels": ["Document"], "properties": {"title": "Pay"}},
            "edge": {"id": 4, "type": "CITES", "properties": {"page": 12}},
        }])]))
        .await;

        let looked = source(stub.address(), None)
            .look(&LookUp::Neighbors {
                node: 7,
                direction: ip_retrieve::Direction::Both,
                edge_type: Some("CITES".to_owned()),
            })
            .await
            .expect("the plane answered");

        assert_eq!(looked[0].id, "9");
        assert_eq!(looked[0].payload["node"]["properties"]["title"], "Pay");
        assert_eq!(looked[0].payload["edge"]["type"], "CITES");
        assert_eq!(looked[0].payload["edge"]["properties"]["page"], 12);

        let seen = stub.only();
        assert_eq!(seen.method, "plane.neighbors");
        assert_eq!(seen.params["id"], 7);
        assert_eq!(seen.params["direction"], "both");
        assert_eq!(seen.params["type"], "CITES");
    }

    /// The three verbs that answer ids, each rendered as what it is.
    #[tokio::test]
    async fn a_ranking_a_clustering_and_a_path_are_answered_as_ids() {
        let ranked = StubPlane::start(Replies::in_turn([
            serde_json::json!({"results": [{"id": 7, "score": 0.42}]}),
        ]))
        .await;
        let looked = source(ranked.address(), Some("Document"))
            .look(&LookUp::Rank { hits: 3 })
            .await
            .expect("the plane answered");
        assert_eq!(looked[0].id, "7");
        assert_eq!(looked[0].score, Some(0.42));
        assert_eq!(looked[0].payload, serde_json::json!({}));
        let seen = ranked.only();
        assert_eq!(seen.params["algo"], "pagerank");
        assert_eq!(seen.params["label"], "Document");
        assert_eq!(seen.params["limit"], 3);

        let grouped = StubPlane::start(Replies::in_turn([
            serde_json::json!({"results": [{"id": 7, "community": 2}]}),
        ]))
        .await;
        let looked = source(grouped.address(), None)
            .look(&LookUp::Cluster { hits: 3 })
            .await
            .expect("the plane answered");
        assert_eq!(looked[0].id, "7");
        assert_eq!(looked[0].score, None);
        assert_eq!(looked[0].payload["group"], 2);
        assert_eq!(grouped.only().params["algo"], "louvain");

        let joined = StubPlane::start(Replies::in_turn([
            serde_json::json!({"path": {"nodes": [1, 4, 9], "edges": [2, 6], "cost": 2.0}}),
        ]))
        .await;
        let looked = source(joined.address(), None)
            .look(&LookUp::Relate {
                from: 1,
                to: 9,
                direction: ip_retrieve::Direction::Out,
            })
            .await
            .expect("the plane answered");
        assert_eq!(looked.len(), 1);
        assert_eq!(looked[0].id, "9", "a path is addressed by where it ends");
        assert_eq!(looked[0].score, Some(2.0));
        assert_eq!(looked[0].payload["nodes"], serde_json::json!([1, 4, 9]));
        assert_eq!(looked[0].payload["edges"], serde_json::json!([2, 6]));
    }

    /// Two documents with nothing joining them is what the model asked about, so it is an
    /// answer and not a failure.
    #[tokio::test]
    async fn two_nodes_nothing_connects_are_answered_with_nothing() {
        let stub = StubPlane::start(Replies::in_turn([serde_json::json!({"path": null})])).await;

        let looked = source(stub.address(), None)
            .look(&LookUp::Relate {
                from: 1,
                to: 9,
                direction: ip_retrieve::Direction::Out,
            })
            .await
            .expect("the plane answered");

        assert!(looked.is_empty());
    }

    /// A refusal reaches the model as the sentence that names what to fix, never as no hits.
    #[tokio::test]
    async fn a_plane_that_refuses_says_why_rather_than_answering_nothing() {
        let stub = StubPlane::start(Replies::Failure).await;

        let said = source(stub.address(), None)
            .look(&searching())
            .await
            .expect_err("a refusal is not an answer");

        assert!(
            said.contains("no plane named graphs"),
            "a model is told what the store said, not that it failed: {said}"
        );
    }

    /// The width probe registration runs, through the type a run holds.
    #[tokio::test]
    async fn a_source_measures_the_width_of_what_it_holds() {
        let stub = StubPlane::start(Replies::in_turn([serde_json::json!([{
            "id": 7,
            "labels": ["Document"],
            "properties": {"embedding": "$vector(384 dims, omitted)"},
        }])]))
        .await;

        assert_eq!(source(stub.address(), None).width().await, Ok(Some(384)));
    }

    /// The parts beside the client are the retriever's, and readable where a caller needs them.
    #[tokio::test]
    async fn a_source_says_what_it_is_scoped_to() {
        let stub = StubPlane::start(Replies::in_turn([serde_json::json!([])])).await;
        let held = source(stub.address(), Some("Document"));

        assert_eq!(held.label().map(NodeLabel::as_str), Some("Document"));
        assert_eq!(held.property().as_str(), "embedding");
    }
}
