//! What a look-up asks a plane, and what a plane answers.
//!
//! Five verbs, one per tool a retriever may offer, each with a parameter type of its own rather
//! than a list of strings and numbers: a caller cannot pass a label where a property goes, or a
//! query vector where a limit goes, because the fields are named and typed.
//!
//! **Three of the five answer ids.** `plane.algo` is a ranked or grouped answer over node ids —
//! it carries no properties — so a caller that wants text a model can read asks for those nodes
//! in a second call. Only [`DrsgClient::search`](crate::DrsgClient::search) and
//! [`neighbors`](crate::DrsgClient::neighbors) answer records.

use ip_retrieve::{NodeLabel, VectorProperty};
use serde::de::DeserializeOwned;

use crate::error::DrsgError;

/// How two vectors are compared.
///
/// **The store does not read the index's own metric.** Its default for an absent one is cosine
/// whatever the index was built with, so a search of a plane indexed by dot product or `l2` has
/// to say so here — an unsaid metric is not "whichever this index uses".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Metric {
    /// Cosine similarity, and what the store assumes when nothing is said.
    #[default]
    Cosine,
    /// Dot product.
    Dot,
    /// Euclidean distance.
    L2,
}

impl Metric {
    /// The word the store spells this by.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cosine => "cosine",
            Self::Dot => "dot",
            Self::L2 => "l2",
        }
    }
}

/// Which way a walk follows an edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    /// Along the edge as it was written, and what the store assumes.
    #[default]
    Out,
    /// Against it.
    In,
    /// Either way.
    Both,
}

impl Direction {
    /// The word the store spells this by.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Out => "out",
            Self::In => "in",
            Self::Both => "both",
        }
    }
}

/// What a search is told: where the embeddings are, what to compare against, and how many.
///
/// The query is a vector and not text, which is the whole reason a look-up needs an embedder:
/// the store embeds nothing on this method's behalf.
#[derive(Debug, Clone, PartialEq)]
pub struct Nearest {
    /// Which property of a node holds the embedding.
    pub property: VectorProperty,
    /// Which label to scope to, and every label in the plane when left out.
    pub label: Option<NodeLabel>,
    /// The query, already embedded.
    pub query: Vec<f32>,
    /// How many hits to answer with.
    pub hits: u32,
    /// How to compare, and cosine when left out. See [`Metric`] for why this is worth saying.
    pub metric: Option<Metric>,
}

/// What a walk is told: which node to start from, which way, and along what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Around {
    /// The node to walk from, by the id the store assigned it.
    pub node: u64,
    /// Which way to follow an edge.
    pub direction: Direction,
    /// Which type of edge to follow, and every type when left out.
    pub edge_type: Option<String>,
}

/// What ranking or clustering is told.
///
/// Neither narrows to an edge type — the store's own pagerank and louvain follow every edge
/// between the nodes they are given — so the label is the only scope there is, and a plane
/// holding more than one corpus is ranked as a whole without one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Across {
    /// Which label to scope to, and every label in the plane when left out.
    pub label: Option<NodeLabel>,
    /// How many rows to answer with.
    pub limit: u32,
}

/// What a relate is told: the two nodes, and which way an edge may be crossed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Apart {
    /// Where the path starts.
    pub from: u64,
    /// Where it ends.
    pub to: u64,
    /// Which way to follow an edge.
    pub direction: Direction,
}

/// One node a verb answered with.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct Hit {
    /// The id the store assigned it, which every other verb addresses it by.
    pub id: u64,
    /// The key it was created under, where it has one.
    // The store renders a key as `external_key` while every method takes it as `key`.
    #[serde(default, rename = "external_key")]
    pub key: Option<String>,
    /// Every label it carries.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Its properties, as the store rendered them: a vector among them is a marker rather than
    /// its floats, which is what keeps an embedding out of a prompt.
    #[serde(default)]
    pub properties: serde_json::Value,
    /// How near it was, where the verb scored it.
    #[serde(default)]
    pub score: Option<f32>,
}

/// One edge a walk crossed.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct Edge {
    /// The id the store assigned it.
    pub id: u64,
    /// What type of edge it is, where the store said.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    /// Its properties, as the store rendered them.
    #[serde(default)]
    pub properties: serde_json::Value,
}

/// One hop a walk answered with: what is joined, and what joins it.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct Hop {
    /// The node at the far end.
    pub node: Hit,
    /// The edge that reaches it.
    pub edge: Edge,
}

/// One node a ranking placed, by the id the store assigned it.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct Placed {
    /// Which node.
    pub id: u64,
    /// What it scored.
    #[serde(default)]
    pub score: f64,
}

/// One node a clustering grouped, by the id the store assigned it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Grouped {
    /// Which node.
    pub id: u64,
    /// The group it fell into, named by the id of the node that represents it.
    #[serde(rename = "community")]
    pub group: u64,
}

/// How two nodes are connected, by the ids along the way.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct Path {
    /// The nodes from one end to the other, both ends included.
    #[serde(default)]
    pub nodes: Vec<u64>,
    /// The edges crossed between them.
    #[serde(default)]
    pub edges: Vec<u64>,
    /// What the path cost, which is its length where no edge carries a weight.
    #[serde(default)]
    pub cost: f64,
}

/// What a vector property reads as when a plane answers without the floats, and what comes before
/// the width in it.
const MARKER: (&str, &str) = ("$vector(", " dims, omitted)");

/// How many numbers a rendered property holds, and none where it is not a vector at all.
///
/// **The width is in that text and nowhere else.** No verb a plane offers reports a property's
/// dimension, and a read answers a vector as `$vector(384 dims, omitted)` rather than as its
/// floats — which is what keeps an embedding out of a prompt, and the only place the number
/// appears. A property that is text, or a marker whose width is not a number, is none rather than
/// a refusal: it is a plane holding something else under that name.
pub(crate) fn marker_width(rendered: Option<&serde_json::Value>) -> Option<u32> {
    rendered?
        .as_str()?
        .strip_prefix(MARKER.0)?
        .strip_suffix(MARKER.1)?
        .parse()
        .ok()
}

/// Reads an answer that must be a list of one shape, and refuses one that is not.
///
/// A store answering a verb with something other than what that verb promises is the same
/// failure as one answering with no JSON-RPC at all: this client is not talking to the store it
/// thinks it is, and a caller is told so rather than handed an empty list.
pub(crate) fn rows<T: DeserializeOwned>(
    answer: serde_json::Value,
    promised: &str,
) -> Result<Vec<T>, DrsgError> {
    let Some(listed) = answer.as_array() else {
        return Err(DrsgError::Malformed {
            detail: format!("{promised} is not a list"),
        });
    };
    listed
        .iter()
        .map(|row| {
            serde_json::from_value(row.clone()).map_err(|error| DrsgError::Malformed {
                detail: format!("{promised} holds something else: {error}"),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_word_the_store_reads_is_spelled_as_the_store_spells_it() {
        assert_eq!(Metric::default().as_str(), "cosine");
        assert_eq!(Metric::Dot.as_str(), "dot");
        assert_eq!(Metric::L2.as_str(), "l2");
        assert_eq!(Direction::default().as_str(), "out");
        assert_eq!(Direction::In.as_str(), "in");
        assert_eq!(Direction::Both.as_str(), "both");
    }

    #[test]
    fn a_hit_is_read_with_the_key_the_store_renders_it_under() {
        let read: Vec<Hit> = rows(
            serde_json::json!([{
                "id": 7,
                "external_key": "doc:handbook:leave",
                "labels": ["Document"],
                "properties": {"title": "Leave", "embedding": "$vector(384 dims, omitted)"},
                "score": 0.83,
            }]),
            "a search",
        )
        .expect("a scored row is a hit");
        let one = &read[0];
        assert_eq!(one.id, 7);
        assert_eq!(one.key.as_deref(), Some("doc:handbook:leave"));
        assert_eq!(one.labels, ["Document"]);
        assert_eq!(one.properties["title"], "Leave");
        assert_eq!(one.score, Some(0.83));
        assert_eq!(
            marker_width(one.properties.get("embedding")),
            Some(384),
            "an embedding reads as a marker saying how wide it is"
        );
    }

    /// Everything a property can be under the name a retriever named, other than a vector.
    #[test]
    fn a_property_that_is_not_a_vector_is_no_width() {
        let text = serde_json::json!({
            "embedding": "the leave policy",
            "marked": "$vector(wide dims, omitted)",
            "floats": [0.1, 0.2],
            "counted": 384,
        });
        assert_eq!(marker_width(text.get("embedding")), None);
        assert_eq!(
            marker_width(text.get("marked")),
            None,
            "a marker whose width is a word is not a width"
        );
        assert_eq!(
            marker_width(text.get("floats")),
            None,
            "a read answers markers, so floats under that name are something else"
        );
        assert_eq!(marker_width(text.get("counted")), None);
        assert_eq!(
            marker_width(text.get("nothing")),
            None,
            "a node need not carry the property at all"
        );
    }

    /// A node created without one, and an unscored verb: neither is a malformed answer.
    #[test]
    fn a_hit_with_no_key_and_no_score_is_still_a_hit() {
        let read: Vec<Hit> = rows(
            serde_json::json!([{"id": 7, "labels": [], "properties": {}}]),
            "a search",
        )
        .expect("an unscored row is a hit");
        assert_eq!(read[0].key, None);
        assert_eq!(read[0].score, None);
    }

    #[test]
    fn a_hop_carries_the_edge_type_under_the_word_the_store_uses() {
        let read: Vec<Hop> = rows(
            serde_json::json!([{
                "node": {"id": 9, "labels": ["Document"], "properties": {}},
                "edge": {"id": 4, "type": "CITES", "properties": {"page": 12}},
            }]),
            "a walk",
        )
        .expect("a hop is a node and an edge");
        assert_eq!(read[0].node.id, 9);
        assert_eq!(read[0].edge.kind.as_deref(), Some("CITES"));
        assert_eq!(read[0].edge.properties["page"], 12);
    }

    /// The store renders `{id}` alone for an edge it could not read back, which is a hop that
    /// still names where it went.
    #[test]
    fn an_edge_the_store_could_not_describe_is_still_an_edge() {
        let read: Vec<Hop> = rows(
            serde_json::json!([{
                "node": {"id": 9, "labels": [], "properties": {}},
                "edge": {"id": 4},
            }]),
            "a walk",
        )
        .expect("an id is an edge");
        assert_eq!(read[0].edge.id, 4);
        assert_eq!(read[0].edge.kind, None);
    }

    #[test]
    fn an_answer_that_is_not_a_list_is_not_an_answer() {
        let refused: Result<Vec<Hit>, _> = rows(serde_json::json!({"hits": []}), "a search");
        match refused {
            Err(DrsgError::Malformed { detail }) => {
                assert!(detail.contains("a search"), "{detail}");
            }
            other => panic!("an object was read as {other:?}"),
        }
    }

    #[test]
    fn a_row_that_is_not_the_shape_the_verb_promised_is_not_an_answer() {
        let refused: Result<Vec<Grouped>, _> =
            rows(serde_json::json!([{"id": 1, "score": 0.5}]), "a clustering");
        assert!(
            matches!(refused, Err(DrsgError::Malformed { .. })),
            "a ranked row was read as a grouped one"
        );
    }
}
