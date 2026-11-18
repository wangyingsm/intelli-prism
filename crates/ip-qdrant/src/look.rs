//! What a look-up asks a collection, and what a collection answers.

use std::fmt;

use ip_retrieve::{CollectionName, VectorName};

/// How a collection compares two vectors.
///
/// **Read to report, never to send.** Unlike a dr-strange plane, qdrant compares with the metric
/// its own collection was built with, so a look-up has nothing to say about it and cannot get it
/// wrong. A word this build does not know is read as none rather than refused: nothing here acts
/// on it, and a newer store naming a metric we have not heard of is no reason to refuse a
/// registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distance {
    /// Cosine similarity.
    Cosine,
    /// Dot product.
    Dot,
    /// Euclidean distance.
    Euclid,
    /// Manhattan distance.
    Manhattan,
}

impl Distance {
    /// The word qdrant spells this by, which is capitalised and case-sensitive there.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cosine => "Cosine",
            Self::Dot => "Dot",
            Self::Euclid => "Euclid",
            Self::Manhattan => "Manhattan",
        }
    }

    /// The metric a collection reported, and none where it named one this build does not know.
    #[must_use]
    pub fn named(word: &str) -> Option<Self> {
        match word {
            "Cosine" => Some(Self::Cosine),
            "Dot" => Some(Self::Dot),
            "Euclid" => Some(Self::Euclid),
            "Manhattan" => Some(Self::Manhattan),
            _ => None,
        }
    }
}

impl fmt::Display for Distance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a collection says about the vectors it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Collection {
    /// How many numbers a vector in it has, which is what an embedder's own width must equal.
    pub width: u32,
    /// How it compares them, and none where it named a metric this build does not know.
    pub metric: Option<Distance>,
}

/// What a look-up narrows a collection by.
///
/// **The envelope is declared here and the conditions in it are not.** Qdrant's condition grammar
/// is large, versioned and theirs — a match, a range, a geo box, a nested filter — so
/// re-specifying it here would mean maintaining a second grammar that lags it, and a condition a
/// model wrote correctly would be refused by us rather than answered by them.
///
/// What this type refuses is the half that matters: there is **no field for the collection, the
/// vector or the limit**. Those come from the retriever an operator registered and from its own
/// ceiling, so a filter cannot widen a look-up to a corpus the node was not given. How long a
/// filter may be is bounded where a model's words are bounded, not here.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    /// Conditions a point must satisfy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub must: Vec<serde_json::Value>,
    /// Conditions a point may satisfy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub should: Vec<serde_json::Value>,
    /// Conditions a point must not satisfy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub must_not: Vec<serde_json::Value>,
    /// How many of `should` must hold, as qdrant's own `{conditions, min_count}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_should: Option<serde_json::Value>,
}

impl Filter {
    /// Whether it narrows nothing, in which case no filter is sent at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.must.is_empty()
            && self.should.is_empty()
            && self.must_not.is_empty()
            && self.min_should.is_none()
    }
}

/// What a look-up is told: which collection, what to compare against, and how many.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    /// Which collection to read.
    pub collection: CollectionName,
    /// Which of its named vectors to compare against, and its only one when left out.
    pub vector: Option<VectorName>,
    /// The query, already embedded.
    pub query: Vec<f32>,
    /// How many hits to answer with.
    pub hits: u32,
    /// What to narrow by, and nothing when the look-up narrows nothing.
    pub filter: Option<Filter>,
}

/// How a point is addressed, which qdrant allows to be either a number or a string.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(untagged)]
pub enum PointId {
    /// An unsigned integer or a uuid's numeric form.
    Number(u64),
    /// A uuid, or whatever else the operator indexed under.
    Text(String),
}

impl fmt::Display for PointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(id) => write!(f, "{id}"),
            Self::Text(id) => f.write_str(id),
        }
    }
}

/// One point a look-up answered with.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct Found {
    /// Which point it is.
    pub id: PointId,
    /// How near it was.
    #[serde(default)]
    pub score: f32,
    /// What it carries, which is the text a model reads. Asked for on every call, since a hit
    /// without its payload is an id a model can do nothing with.
    #[serde(default)]
    pub payload: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_metric_is_spelled_the_way_qdrant_spells_it_and_an_unknown_one_is_not_a_refusal() {
        assert_eq!(Distance::named("Cosine"), Some(Distance::Cosine));
        assert_eq!(Distance::named("Manhattan"), Some(Distance::Manhattan));
        assert_eq!(Distance::Euclid.as_str(), "Euclid");
        assert_eq!(
            Distance::named("cosine"),
            None,
            "qdrant's own names are capitalised"
        );
        assert_eq!(Distance::named("Hamming"), None);
    }

    #[test]
    fn a_filter_that_narrows_nothing_is_empty_however_it_was_written() {
        assert!(Filter::default().is_empty());
        assert!(
            serde_json::from_str::<Filter>(r#"{"must":[],"should":[]}"#)
                .expect("empty lists are a filter")
                .is_empty()
        );
        assert!(
            !serde_json::from_str::<Filter>(r#"{"must":[{"key":"team","match":{"value":"ops"}}]}"#)
                .expect("a condition is a filter")
                .is_empty()
        );
    }

    /// The envelope is ours and the conditions are theirs: a condition crosses the wire exactly
    /// as it arrived, and a field that is not one of the four is refused.
    #[test]
    fn a_condition_is_carried_through_and_a_field_we_do_not_carry_is_not() {
        let filter: Filter = serde_json::from_str(
            r#"{"must":[{"key":"team","match":{"value":"ops"}}],
                "must_not":[{"key":"draft","match":{"value":true}}],
                "min_should":{"conditions":[{"key":"year","range":{"gte":2026}}],"min_count":1}}"#,
        )
        .expect("qdrant's own grammar is taken as written");
        let written = serde_json::to_value(&filter).expect("and written back");
        assert_eq!(written["must"][0]["match"]["value"], "ops");
        assert_eq!(written["must_not"][0]["key"], "draft");
        assert_eq!(written["min_should"]["min_count"], 1);
        assert!(
            written.get("should").is_none(),
            "an empty list is no filter at all: {written}"
        );

        assert!(
            serde_json::from_str::<Filter>(r#"{"collection":"somebody-elses"}"#).is_err(),
            "a filter has no field a collection could travel in"
        );
    }

    #[test]
    fn a_point_is_named_by_a_number_or_by_text() {
        let found: Vec<Found> = serde_json::from_str(
            r#"[{"id":42,"version":3,"score":0.75,"payload":{"title":"Leave"}},
                {"id":"6b3f-7a","version":1,"score":0.5}]"#,
        )
        .expect("both forms are points");
        assert_eq!(found[0].id, PointId::Number(42));
        assert_eq!(found[0].id.to_string(), "42");
        assert_eq!(found[0].payload["title"], "Leave");
        assert_eq!(found[1].id, PointId::Text("6b3f-7a".to_owned()));
        assert_eq!(
            found[1].payload,
            serde_json::Value::Null,
            "a point stored with no payload is still a point"
        );
    }
}
