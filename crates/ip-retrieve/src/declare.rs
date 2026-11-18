//! What a model is told it may look up, and what a backend is given when it calls.
//!
//! **One tool per thing a retriever can be asked, never one tool with a mode flag.** A model
//! filling in parameters that apply to one algorithm and not another gets it wrong, and every
//! mistake costs a paid turn — so `search` is shown a query and a filter, `relate` is shown two
//! node ids, and neither is shown the other's fields. The declarations are built per retriever
//! rather than written out once, because three things about them are the operator's: which tools
//! are offered at all, how many hits one look-up may answer with, and which payload fields a
//! filter may name.
//!
//! The same split runs through [`LookUp`]: what a model may pass is the schema here, and what a
//! backend is given is the variant of the same name. They are declared together so that a
//! parameter cannot be added to one and forgotten in the other.

use crate::meta::MetaSchema;
use crate::retriever::{MaxHits, Retriever};
use crate::source::Source;
use crate::tool::RetrieverTool;

/// What a model is told about one tool: the words, and the shape of what it may pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declared {
    /// What the tool does, in the words the model reads.
    pub description: String,
    /// What it may be called with, as a json schema.
    pub schema: serde_json::Value,
}

/// Which way a walk goes from the node it starts at.
///
/// Declared here rather than taken from the graph client, because this crate is what both the
/// client and the binary depend on and not the other way round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Along the edges that leave it, which is what a citation or a reference points at.
    #[default]
    Out,
    /// Along the edges that reach it.
    In,
    /// Both, which answers a neighbourhood rather than a direction.
    Both,
}

impl Direction {
    /// The word it is written as, in a schema and on a wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Out => "out",
            Self::In => "in",
            Self::Both => "both",
        }
    }
}

/// One look-up, as a backend is given it.
///
/// **A query arrives here already a vector.** Only this deployment's own artefact may turn words
/// into one — that is the whole of ruling 6 and of the width a registration checks — so the thing
/// that embeds is the thing that calls, and a backend is never handed text to embed with
/// credentials of its own.
#[derive(Debug, Clone, PartialEq)]
pub enum LookUp {
    /// What is nearest a query.
    Search {
        /// The query, already embedded by this deployment's artefact.
        query: Vec<f32>,
        /// How many hits to answer with, already clamped to the retriever's own ceiling.
        hits: u32,
        /// What to narrow by, in the backend's own condition grammar, and nothing where the
        /// look-up narrows nothing.
        filter: Option<serde_json::Value>,
    },
    /// What one hop reaches from a node.
    Neighbors {
        /// Which node to start at.
        node: u64,
        /// Which way to walk.
        direction: Direction,
        /// Only edges of this kind, and every kind where none is named.
        edge_type: Option<String>,
    },
    /// What matters most, by how much is joined to it.
    Rank {
        /// How many to answer with, already clamped.
        hits: u32,
    },
    /// How two nodes are connected.
    Relate {
        /// Where the path starts.
        from: u64,
        /// Where it ends.
        to: u64,
        /// Which way to walk.
        direction: Direction,
    },
    /// What groups things fall into.
    Cluster {
        /// How many to answer with, already clamped.
        hits: u32,
    },
}

impl LookUp {
    /// Which tool this is a look-up of.
    #[must_use]
    pub const fn tool(&self) -> RetrieverTool {
        match self {
            Self::Search { .. } => RetrieverTool::Search,
            Self::Neighbors { .. } => RetrieverTool::Neighbors,
            Self::Rank { .. } => RetrieverTool::Rank,
            Self::Relate { .. } => RetrieverTool::Relate,
            Self::Cluster { .. } => RetrieverTool::Cluster,
        }
    }
}

/// One thing a look-up answered with.
///
/// One shape for all five tools and both backends, because what is done with it is the same thing
/// in every case: it goes in a prompt. A point and a node are both addressed by an id a model can
/// pass back, and what each carries is json rather than a type of its own, since a payload is the
/// operator's own and a node's properties are the plane's.
#[derive(Debug, Clone, PartialEq)]
pub struct Looked {
    /// How to address it in a further look-up.
    pub id: String,
    /// How near, how highly ranked, or how far — whatever the tool scored it by, where it scored.
    pub score: Option<f32>,
    /// What it carries, which is the text a model reads.
    pub payload: serde_json::Value,
}

/// Looking something up, whichever backend holds it.
///
/// The seam `ip-run` dispatches through, for the reason [`crate`] exists: a run may not depend on
/// either backend, and this is the one thing it needs of them. It is the same split
/// `ip_gateway::Embedder` already makes — the trait beside the types, the implementations beside
/// the wire.
#[async_trait::async_trait]
pub trait Retrieval: Send + Sync {
    /// Looks something up, or says why it could not in words a model is shown.
    ///
    /// The error is a sentence rather than a type because of where it goes: a tool that failed is
    /// an answer the model is given and may act on, so what matters is that it reads as a reason.
    async fn look(&self, asked: &LookUp) -> Result<Vec<Looked>, String>;
}

impl Retriever {
    /// What a model is told about one of this retriever's tools.
    ///
    /// Total over the five, and **not** a check that this retriever offers the tool: a caller
    /// resolving a tool set has to answer that with the list of what *is* offered, which is a
    /// better refusal than this could give.
    #[must_use]
    pub fn declare(&self, tool: RetrieverTool) -> Declared {
        let named = self.name();
        let most = self.max_hits();
        match tool {
            RetrieverTool::Search => Declared {
                description: format!(
                    "Find what is most like a query in {named}, as text to read.{}",
                    filterable(self.meta(), self.source())
                ),
                schema: searching(most, filters(self.source())),
            },
            RetrieverTool::Neighbors => Declared {
                description: format!(
                    "Follow one hop from something in {named} to what is joined to it. Takes an \
                     id a previous look-up answered with."
                ),
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "node": node("Which one to walk from."),
                        "direction": direction(),
                        "edge_type": {
                            "type": "string",
                            "description": "Only follow edges of this kind, and every kind when \
                                            left out.",
                        },
                    },
                    "required": ["node"],
                    "additionalProperties": false,
                }),
            },
            RetrieverTool::Rank => Declared {
                description: format!(
                    "List what matters most in {named}, by how much is joined to it rather than \
                     by what it says. Answers ids and scores, which `neighbors` reads."
                ),
                schema: counting(most, "How many to answer with, most important first."),
            },
            RetrieverTool::Relate => Declared {
                description: format!(
                    "Say how two things in {named} are connected, as the shortest chain of \
                     joins between them. Takes two ids a previous look-up answered with."
                ),
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "from": node("Where the chain starts."),
                        "to": node("Where it ends."),
                        "direction": direction(),
                    },
                    "required": ["from", "to"],
                    "additionalProperties": false,
                }),
            },
            RetrieverTool::Cluster => Declared {
                description: format!(
                    "Group what is in {named} by what is joined to what, and say which group each \
                     thing falls in. Answers ids and groups, which `neighbors` reads."
                ),
                schema: counting(most, "How many to answer with."),
            },
        }
    }
}

/// The fields a filter may name, as a sentence after the description, and nothing where the
/// operator described none or the backend cannot filter.
///
/// A model told only that it may filter writes a field name it invented; one told nothing at all
/// does not filter, which is the right behaviour for a retriever whose operator described no
/// fields.
fn filterable(meta: &MetaSchema, source: &Source) -> String {
    if meta.is_empty() || !filters(source) {
        return String::new();
    }
    let fields = meta
        .described()
        .map(|(field, note)| format!("`{field}` ({note})"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(" A filter may narrow by: {fields}.")
}

/// Whether this kind of backend narrows a search by payload at all.
const fn filters(source: &Source) -> bool {
    matches!(source, Source::Qdrant { .. })
}

/// The schema a search is called with, with a filter only where the backend takes one.
fn searching(most: MaxHits, filtering: bool) -> serde_json::Value {
    let mut schema = serde_json::json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "What to look for, in words. Whole sentences work better than \
                                keywords: it is compared by meaning and not by matching.",
            },
            "hits": hits(most),
        },
        "required": ["query"],
        "additionalProperties": false,
    });
    if let Some(properties) = schema["properties"].as_object_mut().filter(|_| filtering) {
        properties.insert(
            "filter".to_owned(),
            serde_json::json!({
                "type": "object",
                "description": "Narrow the search to the fields named above, as conditions: \
                                {\"must\": [{\"key\": \"team\", \"match\": \
                                {\"value\": \"ops\"}}]}. Leave it out to search everything.",
            }),
        );
    }
    schema
}

/// The schema a tool that takes nothing but a count is called with.
fn counting(most: MaxHits, note: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {"hits": {
            "type": "integer",
            "minimum": 1,
            "maximum": most.get(),
            "description": note,
        }},
        "additionalProperties": false,
    })
}

/// How many hits a look-up may ask for, which the operator's ceiling bounds.
fn hits(most: MaxHits) -> serde_json::Value {
    serde_json::json!({
        "type": "integer",
        "minimum": 1,
        "maximum": most.get(),
        "description": format!(
            "How many to answer with, at most {}. Ask for fewer where fewer will do: each one \
             lands in this conversation.",
            most.get()
        ),
    })
}

/// A node id, as a model passes one back.
fn node(note: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "integer",
        "minimum": 0,
        "description": format!("{note} An id a previous look-up answered with."),
    })
}

/// Which way to walk, where a tool walks.
fn direction() -> serde_json::Value {
    serde_json::json!({
        "type": "string",
        "enum": [Direction::Out.as_str(), Direction::In.as_str(), Direction::Both.as_str()],
        "description": "Which way to follow a join: `out` from it, `in` to it, or `both`. \
                        Defaults to `out`.",
    })
}

/// A model's arguments, read into what the tool takes.
///
/// Two cases rather than one, because the query is the only part a backend cannot be handed as
/// the model wrote it: the words have to be embedded by this deployment first, and only this
/// deployment may. Everything else is ready as read.
#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    /// Ready to look up.
    Ready(LookUp),
    /// A search, whose words still need embedding.
    Searching {
        /// What the model asked, in its own words.
        words: String,
        /// How many to answer with, already clamped to the retriever's ceiling.
        hits: u32,
        /// What to narrow by, as the model wrote it.
        filter: Option<serde_json::Value>,
    },
}

impl Retriever {
    /// Reads what a model passed into what the tool takes, or says why it will not read.
    ///
    /// **The refusal is a sentence rather than a type**, for the reason [`Retrieval::look`]'s is:
    /// a tool called wrongly is an answer the model is shown and may correct, not something that
    /// ends a run — so what matters is that it reads as a reason. A model is told the parameter
    /// it got wrong, having been told the schema that declared it.
    ///
    /// **The ceiling is applied here and nowhere else.** A model asking for more than the
    /// operator allowed is clamped rather than refused: it asked for a number it was told the
    /// maximum of, and answering fewer is what it would have asked for had it read the schema.
    pub fn parse(
        &self,
        tool: RetrieverTool,
        arguments: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Parsed, String> {
        let most = self.max_hits().get();
        let hits = counted(arguments, most)?;
        match tool {
            RetrieverTool::Search => Ok(Parsed::Searching {
                words: words(arguments)?,
                hits,
                filter: match arguments.get("filter") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(filter) if !filters(self.source()) => {
                        return Err(format!(
                            "this search takes no filter, and one was passed: {filter}"
                        ));
                    }
                    Some(filter) => Some(filter.clone()),
                },
            }),
            RetrieverTool::Neighbors => Ok(Parsed::Ready(LookUp::Neighbors {
                node: addressed(arguments, "node")?,
                direction: whichever(arguments)?,
                edge_type: match arguments.get("edge_type") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(serde_json::Value::String(kind)) => Some(kind.clone()),
                    Some(other) => {
                        return Err(format!("`edge_type` is a word, and this is {other}"));
                    }
                },
            })),
            RetrieverTool::Rank => Ok(Parsed::Ready(LookUp::Rank { hits })),
            RetrieverTool::Relate => Ok(Parsed::Ready(LookUp::Relate {
                from: addressed(arguments, "from")?,
                to: addressed(arguments, "to")?,
                direction: whichever(arguments)?,
            })),
            RetrieverTool::Cluster => Ok(Parsed::Ready(LookUp::Cluster { hits })),
        }
    }
}

/// The words a search looks for, which it cannot run without.
fn words(arguments: &serde_json::Map<String, serde_json::Value>) -> Result<String, String> {
    match arguments.get("query") {
        Some(serde_json::Value::String(words)) if !words.trim().is_empty() => Ok(words.clone()),
        Some(serde_json::Value::String(_)) | None => {
            Err("say what to look for in `query`".to_owned())
        }
        Some(other) => Err(format!("`query` is what to look for in words, not {other}")),
    }
}

/// How many hits were asked for, clamped to the ceiling, and the ceiling itself where none was.
fn counted(
    arguments: &serde_json::Map<String, serde_json::Value>,
    most: u32,
) -> Result<u32, String> {
    match arguments.get("hits") {
        None | Some(serde_json::Value::Null) => Ok(most),
        Some(serde_json::Value::Number(asked)) => match asked.as_u64() {
            Some(0) | None => Err(format!(
                "`hits` is a whole number of at least 1, not {asked}"
            )),
            Some(asked) => Ok(u32::try_from(asked).unwrap_or(most).min(most)),
        },
        Some(other) => Err(format!("`hits` is how many to answer with, not {other}")),
    }
}

/// The node a walk names under `field`, which it cannot walk without.
fn addressed(
    arguments: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<u64, String> {
    match arguments.get(field) {
        Some(serde_json::Value::Number(id)) => id
            .as_u64()
            .ok_or_else(|| format!("`{field}` is an id a look-up answered with, not {id}")),
        None => Err(format!("name which one in `{field}`")),
        Some(other) => Err(format!(
            "`{field}` is an id a look-up answered with, not {other}"
        )),
    }
}

/// Which way the walk goes, and outward where the model did not say.
fn whichever(arguments: &serde_json::Map<String, serde_json::Value>) -> Result<Direction, String> {
    let Some(said) = arguments.get("direction") else {
        return Ok(Direction::default());
    };
    match said {
        serde_json::Value::Null => Ok(Direction::default()),
        serde_json::Value::String(word) if word == Direction::Out.as_str() => Ok(Direction::Out),
        serde_json::Value::String(word) if word == Direction::In.as_str() => Ok(Direction::In),
        serde_json::Value::String(word) if word == Direction::Both.as_str() => Ok(Direction::Both),
        other => Err(format!(
            "`direction` is `{}`, `{}` or `{}`, not {other}",
            Direction::Out.as_str(),
            Direction::In.as_str(),
            Direction::Both.as_str()
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::id::RetrieverId;
    use crate::meta::{MetaField, MetaNote};
    use crate::retriever::{RetrieverAuth, RetrieverEndpoint, RetrieverName};
    use crate::source::{CollectionName, NodeLabel, PlaneName, VectorProperty};
    use crate::tool::RetrieverTools;
    use crate::{RetrieverParts, RetrieverTool};

    fn meta(fields: &[(&str, &str)]) -> MetaSchema {
        MetaSchema::new(
            fields
                .iter()
                .map(|(field, note)| {
                    (
                        MetaField::new(field).expect("a field"),
                        MetaNote::new(note).expect("a note"),
                    )
                })
                .collect::<BTreeMap<_, _>>(),
        )
        .expect("a schema")
    }

    fn retriever(source: Source, meta: MetaSchema, max_hits: u32) -> Retriever {
        Retriever::new(RetrieverParts {
            id: RetrieverId::new("kb").expect("an id"),
            name: RetrieverName::new("the staff handbook").expect("a name"),
            endpoint: RetrieverEndpoint::new("https://store.acme.test:6333").expect("an endpoint"),
            auth: RetrieverAuth::None,
            source,
            tools: RetrieverTools::new(vec![
                RetrieverTool::Search,
                RetrieverTool::Neighbors,
                RetrieverTool::Rank,
                RetrieverTool::Relate,
                RetrieverTool::Cluster,
            ])
            .expect("every tool"),
            meta,
            max_hits: MaxHits::new(max_hits).expect("a ceiling"),
        })
        .expect("the parts agree")
    }

    fn plane(meta: MetaSchema) -> Retriever {
        retriever(
            Source::Drsg {
                plane: PlaneName::new("handbook").expect("a plane"),
                label: Some(NodeLabel::new("Document").expect("a label")),
                property: VectorProperty::new("embedding").expect("a property"),
            },
            meta,
            12,
        )
    }

    fn collection(meta: MetaSchema) -> Retriever {
        Retriever::new(RetrieverParts {
            tools: RetrieverTools::new(vec![RetrieverTool::Search]).expect("one tool"),
            ..parts(meta)
        })
        .expect("the parts agree")
    }

    fn parts(meta: MetaSchema) -> RetrieverParts {
        RetrieverParts {
            id: RetrieverId::new("kb").expect("an id"),
            name: RetrieverName::new("the staff handbook").expect("a name"),
            endpoint: RetrieverEndpoint::new("https://store.acme.test:6333").expect("an endpoint"),
            auth: RetrieverAuth::None,
            source: Source::Qdrant {
                collection: CollectionName::new("handbook").expect("a collection"),
                vector: None,
            },
            tools: RetrieverTools::new(vec![RetrieverTool::Search]).expect("one tool"),
            meta,
            max_hits: MaxHits::new(8).expect("a ceiling"),
        }
    }

    /// The operator's own words reach the model, which is what a meta schema is for.
    #[test]
    fn the_fields_an_operator_described_are_what_a_filter_is_offered() {
        let declared = collection(meta(&[
            ("team", "Which team the page belongs to"),
            ("quarter", "Which quarter the figure is for"),
        ]))
        .declare(RetrieverTool::Search);

        assert!(
            declared
                .description
                .contains("`team` (Which team the page belongs to)"),
            "{}",
            declared.description
        );
        assert!(declared.description.contains("`quarter`"), "{declared:?}");
        assert!(
            declared.schema["properties"]["filter"].is_object(),
            "a store that filters was declared without one: {declared:?}"
        );
    }

    /// A model told it may filter and not told by what invents a field name, so it is told
    /// neither.
    #[test]
    fn a_retriever_describing_no_field_offers_no_filter_to_write() {
        let declared = collection(MetaSchema::default()).declare(RetrieverTool::Search);
        assert!(
            !declared.description.contains("filter"),
            "{}",
            declared.description
        );
    }

    /// Ruling 5 at its sharpest: a plane's search takes no payload filter, so a model is not
    /// shown one — however many fields the operator described.
    #[test]
    fn a_plane_is_never_offered_a_filter_it_cannot_take() {
        let declared = plane(meta(&[("team", "Which team the page belongs to")]))
            .declare(RetrieverTool::Search);
        assert!(
            declared.schema["properties"].get("filter").is_none(),
            "{declared:?}"
        );
        assert!(
            !declared.description.contains("filter"),
            "{}",
            declared.description
        );
    }

    /// What the operator capped is what the model is told it may ask for, in both places a model
    /// reads.
    #[test]
    fn the_ceiling_an_operator_set_is_what_the_model_is_offered() {
        for tool in [
            RetrieverTool::Search,
            RetrieverTool::Rank,
            RetrieverTool::Cluster,
        ] {
            let declared = plane(MetaSchema::default()).declare(tool);
            assert_eq!(
                declared.schema["properties"]["hits"]["maximum"],
                12,
                "{} offered another ceiling: {declared:?}",
                tool.name()
            );
            assert_eq!(declared.schema["properties"]["hits"]["minimum"], 1);
        }
        assert!(
            plane(MetaSchema::default())
                .declare(RetrieverTool::Search)
                .schema["properties"]["hits"]["description"]
                .as_str()
                .expect("a description")
                .contains("at most 12")
        );
    }

    /// No tool is shown a parameter that belongs to another, which is the whole of ruling 5.
    #[test]
    fn each_tool_is_shown_its_own_parameters_and_no_others() {
        let held = plane(MetaSchema::default());
        let fields = |tool: RetrieverTool| -> Vec<String> {
            held.declare(tool).schema["properties"]
                .as_object()
                .expect("an object")
                .keys()
                .cloned()
                .collect()
        };
        assert_eq!(fields(RetrieverTool::Search), ["hits", "query"]);
        assert_eq!(
            fields(RetrieverTool::Neighbors),
            ["direction", "edge_type", "node"]
        );
        assert_eq!(fields(RetrieverTool::Rank), ["hits"]);
        assert_eq!(fields(RetrieverTool::Relate), ["direction", "from", "to"]);
        assert_eq!(fields(RetrieverTool::Cluster), ["hits"]);
    }

    /// What a model must say, and what it may leave out.
    #[test]
    fn what_a_tool_cannot_be_called_without_is_required_of_the_model() {
        let held = plane(MetaSchema::default());
        let required = |tool: RetrieverTool| held.declare(tool).schema["required"].clone();
        assert_eq!(
            required(RetrieverTool::Search),
            serde_json::json!(["query"])
        );
        assert_eq!(
            required(RetrieverTool::Neighbors),
            serde_json::json!(["node"])
        );
        assert_eq!(
            required(RetrieverTool::Relate),
            serde_json::json!(["from", "to"])
        );
        assert_eq!(
            required(RetrieverTool::Rank),
            serde_json::Value::Null,
            "a count has a default, so nothing is required"
        );
        for tool in [
            RetrieverTool::Search,
            RetrieverTool::Neighbors,
            RetrieverTool::Rank,
            RetrieverTool::Relate,
            RetrieverTool::Cluster,
        ] {
            let declared = held.declare(tool);
            assert_eq!(
                declared.schema["additionalProperties"],
                serde_json::json!(false),
                "{} takes anything: {declared:?}",
                tool.name()
            );
            assert!(
                declared.description.contains("the staff handbook"),
                "{} does not say what it reads: {declared:?}",
                tool.name()
            );
        }
    }

    /// Which arguments reach a backend, which the schema above declared.
    #[test]
    fn what_a_model_passes_is_read_into_what_the_tool_takes() {
        let held = plane(MetaSchema::default());
        let asked =
            |tool, raw: serde_json::Value| held.parse(tool, raw.as_object().expect("an object"));

        assert_eq!(
            asked(
                RetrieverTool::Neighbors,
                serde_json::json!({"node": 7, "direction": "in", "edge_type": "CITES"})
            ),
            Ok(Parsed::Ready(LookUp::Neighbors {
                node: 7,
                direction: Direction::In,
                edge_type: Some("CITES".to_owned()),
            }))
        );
        assert_eq!(
            asked(
                RetrieverTool::Relate,
                serde_json::json!({"from": 1, "to": 2})
            ),
            Ok(Parsed::Ready(LookUp::Relate {
                from: 1,
                to: 2,
                direction: Direction::Out,
            })),
            "a direction nobody named is outward, as the schema says"
        );
        assert_eq!(
            asked(
                RetrieverTool::Search,
                serde_json::json!({"query": "leave policy"})
            ),
            Ok(Parsed::Searching {
                words: "leave policy".to_owned(),
                hits: 12,
                filter: None,
            }),
            "a search is answered as words to embed, never as a vector this crate made up"
        );
    }

    /// The ceiling is the operator's, and a model asking past it is answered rather than
    /// refused: it asked for a number it was told the maximum of.
    #[test]
    fn a_model_asking_for_more_than_the_ceiling_is_given_the_ceiling() {
        let held = plane(MetaSchema::default());
        for (asked, given) in [
            (1_u64, 1_u32),
            (12, 12),
            (13, 12),
            (9_999, 12),
            (u64::MAX, 12),
        ] {
            let raw = serde_json::json!({"query": "leave", "hits": asked});
            assert_eq!(
                held.parse(RetrieverTool::Search, raw.as_object().expect("an object")),
                Ok(Parsed::Searching {
                    words: "leave".to_owned(),
                    hits: given,
                    filter: None,
                }),
                "{asked} hits were not clamped to 12"
            );
        }
        assert_eq!(
            held.parse(
                RetrieverTool::Rank,
                serde_json::json!({}).as_object().expect("an object")
            ),
            Ok(Parsed::Ready(LookUp::Rank { hits: 12 })),
            "a count nobody named is the ceiling"
        );
    }

    /// Every way a model can call a tool wrongly, each answered with the parameter to fix.
    #[test]
    fn a_tool_called_wrongly_says_which_parameter_to_fix() {
        let held = plane(MetaSchema::default());
        let said = |tool, raw: serde_json::Value| {
            held.parse(tool, raw.as_object().expect("an object"))
                .expect_err("it was read")
        };

        assert!(said(RetrieverTool::Search, serde_json::json!({})).contains("`query`"));
        assert!(
            said(RetrieverTool::Search, serde_json::json!({"query": "  "})).contains("`query`")
        );
        assert!(said(RetrieverTool::Search, serde_json::json!({"query": 7})).contains("`query`"));
        assert!(
            said(
                RetrieverTool::Search,
                serde_json::json!({"query": "a", "hits": 0})
            )
            .contains("`hits`")
        );
        assert!(
            said(
                RetrieverTool::Search,
                serde_json::json!({"query": "a", "hits": "many"})
            )
            .contains("`hits`")
        );
        assert!(said(RetrieverTool::Neighbors, serde_json::json!({})).contains("`node`"));
        assert!(said(RetrieverTool::Neighbors, serde_json::json!({"node": -1})).contains("`node`"));
        assert!(
            said(
                RetrieverTool::Neighbors,
                serde_json::json!({"node": 1, "direction": "sideways"})
            )
            .contains("`direction`")
        );
        assert!(
            said(
                RetrieverTool::Neighbors,
                serde_json::json!({"node": 1, "edge_type": 4})
            )
            .contains("`edge_type`")
        );
        assert!(said(RetrieverTool::Relate, serde_json::json!({"from": 1})).contains("`to`"));
    }

    /// A filter is read where the backend takes one, and refused where the schema declared none
    /// — so a model is never silently obeyed in a way the backend would ignore.
    #[test]
    fn a_filter_reaches_a_store_and_is_refused_by_a_plane() {
        let raw = serde_json::json!({
            "query": "leave",
            "filter": {"must": [{"key": "team", "match": {"value": "ops"}}]},
        });
        let arguments = raw.as_object().expect("an object");

        match collection(MetaSchema::default()).parse(RetrieverTool::Search, arguments) {
            Ok(Parsed::Searching { filter, .. }) => {
                assert_eq!(filter, Some(raw["filter"].clone()));
            }
            other => panic!("a store's filter was read as {other:?}"),
        }
        let said = plane(MetaSchema::default())
            .parse(RetrieverTool::Search, arguments)
            .expect_err("a plane takes no filter");
        assert!(said.contains("no filter"), "{said}");
    }

    /// The two halves of this module are one set of five, and a tool added to the enum has to
    /// reach both.
    #[test]
    fn every_tool_has_a_declaration_and_a_look_up_of_its_own() {
        let looked = [
            LookUp::Search {
                query: vec![0.1, 0.2],
                hits: 3,
                filter: None,
            },
            LookUp::Neighbors {
                node: 7,
                direction: Direction::Both,
                edge_type: Some("CITES".to_owned()),
            },
            LookUp::Rank { hits: 3 },
            LookUp::Relate {
                from: 1,
                to: 2,
                direction: Direction::default(),
            },
            LookUp::Cluster { hits: 3 },
        ];
        let held = plane(MetaSchema::default());
        for asked in &looked {
            assert!(
                !held.declare(asked.tool()).schema["properties"]
                    .as_object()
                    .expect("an object")
                    .is_empty(),
                "{} declares nothing to pass",
                asked.tool().name()
            );
        }
        assert_eq!(looked.len(), 5, "there are five tools and no more");
        assert_eq!(Direction::default().as_str(), "out");
    }
}
