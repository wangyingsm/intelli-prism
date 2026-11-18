use std::fmt;

use crate::error::RetrieveError;
use crate::spelling::key;
use crate::tool::RetrieverTool;

/// The longest a plane, label or property name may be, in bytes.
const NAME_MAX_BYTES: usize = 64;

/// The longest a collection name may be, in bytes.
///
/// Wider than the rest because it travels in a url path and is the operator's own naming, often
/// one per tenant per corpus.
const COLLECTION_MAX_BYTES: usize = 255;

/// Which `dr-strange` plane a look-up reads.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct PlaneName(Box<str>);

/// Which label a look-up is scoped to, where it is scoped to one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct NodeLabel(Box<str>);

/// Which property of a node holds the embedding a search compares against.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct VectorProperty(Box<str>);

/// Which `qdrant` collection a look-up reads.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct CollectionName(Box<str>);

/// Which of a collection's named vectors a search uses, where it has more than the one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct VectorName(Box<str>);

/// Writes the five name types, each a key with its own kind and width.
macro_rules! named {
    ($type:ty, $kind:literal, $max:expr) => {
        impl $type {
            /// Name of this kind, as it appears in errors.
            pub const KIND: &'static str = $kind;

            /// Validates and wraps a name.
            pub fn new(raw: &str) -> Result<Self, RetrieveError> {
                Self::try_from(raw.to_owned())
            }

            /// The name as text.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $type {
            type Error = RetrieveError;

            fn try_from(raw: String) -> Result<Self, Self::Error> {
                key(&raw, Self::KIND, $max)?;
                Ok(Self(raw.into_boxed_str()))
            }
        }

        impl fmt::Display for $type {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

named!(PlaneName, "plane name", NAME_MAX_BYTES);
named!(NodeLabel, "node label", NAME_MAX_BYTES);
named!(VectorProperty, "vector property", NAME_MAX_BYTES);
named!(CollectionName, "collection name", COLLECTION_MAX_BYTES);
named!(VectorName, "vector name", NAME_MAX_BYTES);

/// Where a retriever looks, which is also what kind of retriever it is.
///
/// **The kind is the variant and not a field beside it.** A `qdrant` retriever naming a plane, or
/// a `drsg` one naming a collection, is then a shape this type cannot hold rather than a
/// contradiction something has to remember to check: the tag is read on the way in, and a body
/// mixing one kind's fields with another's is refused by serde before any of this code runs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Source {
    /// A `dr-strange` plane: a graph, so every tool is answerable.
    Drsg {
        /// Which plane.
        plane: PlaneName,
        /// Which label to scope to, and every label when left out.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<NodeLabel>,
        /// Which property holds the embeddings.
        property: VectorProperty,
    },
    /// A `qdrant` collection: points and payloads, with no edges between them.
    Qdrant {
        /// Which collection.
        collection: CollectionName,
        /// Which named vector, and the collection's only one when left out.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        vector: Option<VectorName>,
    },
}

impl Source {
    /// The one word this kind is stored and reported under.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Drsg { .. } => "drsg",
            Self::Qdrant { .. } => "qdrant",
        }
    }

    /// Whether a look-up of this kind can be answered here at all.
    #[must_use]
    pub const fn answers(&self, tool: RetrieverTool) -> bool {
        match self {
            Self::Drsg { .. } => true,
            Self::Qdrant { .. } => !tool.needs_edges(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plane_is_read_with_or_without_a_label() {
        let scoped: Source = serde_json::from_str(
            r#"{"kind": "drsg", "plane": "handbook", "label": "Document", "property": "embedding"}"#,
        )
        .expect("a scoped plane is a source");
        let whole: Source = serde_json::from_str(
            r#"{"kind": "drsg", "plane": "handbook", "property": "embedding"}"#,
        )
        .expect("an unscoped plane is a source");
        assert_eq!(scoped.name(), "drsg");
        assert!(matches!(whole, Source::Drsg { label: None, .. }));
    }

    #[test]
    fn a_collection_may_name_one_of_its_vectors() {
        let named: Source = serde_json::from_str(
            r#"{"kind": "qdrant", "collection": "handbook", "vector": "text"}"#,
        )
        .expect("a named vector is a source");
        assert_eq!(named.name(), "qdrant");
    }

    #[test]
    fn a_kind_cannot_be_given_the_other_kinds_fields() {
        assert!(
            serde_json::from_str::<Source>(
                r#"{"kind": "qdrant", "collection": "handbook", "plane": "handbook"}"#
            )
            .is_err(),
            "a qdrant source naming a plane is a shape this type does not hold"
        );
        assert!(
            serde_json::from_str::<Source>(r#"{"kind": "drsg", "plane": "handbook"}"#).is_err(),
            "a plane with no embedding property cannot be searched"
        );
        assert!(
            serde_json::from_str::<Source>(r#"{"kind": "milvus", "collection": "x"}"#).is_err(),
            "a kind this deployment does not speak is not a source"
        );
    }

    #[test]
    fn a_collection_answers_only_what_needs_no_edges() {
        let graph = Source::Drsg {
            plane: PlaneName::new("handbook").expect("a plane name"),
            label: None,
            property: VectorProperty::new("embedding").expect("a property name"),
        };
        let points = Source::Qdrant {
            collection: CollectionName::new("handbook").expect("a collection name"),
            vector: None,
        };
        for tool in [
            RetrieverTool::Search,
            RetrieverTool::Neighbors,
            RetrieverTool::Rank,
            RetrieverTool::Relate,
            RetrieverTool::Cluster,
        ] {
            assert!(graph.answers(tool), "a graph answers {}", tool.name());
            assert_eq!(
                points.answers(tool),
                !tool.needs_edges(),
                "a collection has no edges to walk for {}",
                tool.name()
            );
        }
    }

    #[test]
    fn each_name_is_spelled_as_a_key() {
        assert!(PlaneName::new("intelli-prism_git").is_ok());
        assert!(NodeLabel::new("Document").is_ok());
        assert!(VectorProperty::new("embedding").is_ok());
        assert!(VectorName::new("text").is_ok());
        assert!(CollectionName::new(&"c".repeat(COLLECTION_MAX_BYTES)).is_ok());
        assert!(
            CollectionName::new(&"c".repeat(COLLECTION_MAX_BYTES + 1)).is_err(),
            "a collection name travels in a url path and is bounded"
        );
        assert!(
            PlaneName::new("my plane").is_err(),
            "a plane name is a key, not prose"
        );
    }
}
