//! What a graph, and a node inside one, is spelled by.

use std::fmt;

use crate::error::GraphError;

/// The longest a key may be, in bytes.
const KEY_MAX_BYTES: usize = 64;

/// The longest a label may be, in bytes.
const NAME_MAX_BYTES: usize = 64;

/// Refuses a key no graph or node may be spelled by.
///
/// The rule is the one `ip-core` applies to a tenant id and `ip-agents` to a template id — at most
/// 64 bytes of ascii alphanumerics, `-`, `_` and `.` — so an operator meets one spelling rule
/// across the system. A node's name is additionally the json key its answer reaches a child under,
/// and this rule is what makes every name one.
fn validate_key(raw: &str, kind: &'static str) -> Result<(), GraphError> {
    if raw.is_empty() {
        return Err(GraphError::Empty { kind });
    }
    if raw.len() > KEY_MAX_BYTES {
        return Err(GraphError::TooLong {
            kind,
            len: raw.len(),
            max: KEY_MAX_BYTES,
        });
    }
    match raw
        .chars()
        .find(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')))
    {
        Some(ch) => Err(GraphError::IllegalChar { kind, ch }),
        None => Ok(()),
    }
}

/// Identifies one run graph, as the operator spells it.
///
/// Spelled by whoever writes the graph rather than minted here, for `ip-agents`' reason: applying a
/// graph twice replaces it rather than making a second one, and a run names something a person can
/// place.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String")]
pub struct GraphId(Box<str>);

impl GraphId {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "graph id";

    /// Validates and wraps a raw identifier.
    pub fn new(raw: &str) -> Result<Self, GraphError> {
        validate_key(raw, Self::KIND)?;
        Ok(Self(Box::from(raw)))
    }

    /// The identifier as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GraphId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for GraphId {
    type Error = GraphError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        validate_key(&raw, Self::KIND)?;
        Ok(Self(raw.into_boxed_str()))
    }
}

/// Names one node inside a graph.
///
/// It is a key twice over: an edge names the nodes it joins by it, and a node with several parents
/// receives a json object keyed by it, so a child's prompt asks for an answer by the name its
/// parent was given.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String")]
pub struct NodeName(Box<str>);

impl NodeName {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "node name";

    /// Validates and wraps a raw name.
    pub fn new(raw: &str) -> Result<Self, GraphError> {
        validate_key(raw, Self::KIND)?;
        Ok(Self(Box::from(raw)))
    }

    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for NodeName {
    type Error = GraphError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        validate_key(&raw, Self::KIND)?;
        Ok(Self(raw.into_boxed_str()))
    }
}

/// What a graph is called, for somebody reading a list of them.
///
/// Unlike a [`GraphId`] this is a label rather than a key: it may carry spaces and anything a
/// person writes their own language in, and two graphs may share one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct GraphName(Box<str>);

impl GraphName {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "graph name";

    /// Validates and wraps a label.
    pub fn new(raw: &str) -> Result<Self, GraphError> {
        Self::try_from(raw.to_owned())
    }

    /// The label as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GraphName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for GraphName {
    type Error = GraphError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(GraphError::Empty { kind: Self::KIND });
        }
        if raw.len() > NAME_MAX_BYTES {
            return Err(GraphError::TooLong {
                kind: Self::KIND,
                len: raw.len(),
                max: NAME_MAX_BYTES,
            });
        }
        // Refused rather than trimmed: a store that quietly alters what it was given hands back
        // something the caller did not write and cannot find again.
        if raw.trim() != raw {
            return Err(GraphError::Untrimmed { kind: Self::KIND });
        }
        match raw.chars().find(|ch| ch.is_control()) {
            Some(ch) => Err(GraphError::IllegalChar {
                kind: Self::KIND,
                ch,
            }),
            None => Ok(Self(raw.into_boxed_str())),
        }
    }
}

/// How many bytes a graph run is named with.
const GRAPH_RUN_BYTES: usize = 16;

/// What one run of one graph is followed under.
///
/// Drawn rather than counted, for the reason `ip_run::RunId` is: no two nodes of a cluster can
/// agree a counter without asking each other.
///
/// It names the *graph* run, and each node of it still mints an `ip_run::RunId` of its own, so a
/// node's turns are a run's turns as they have always been. This is what ties those runs together:
/// what a stream is about, what a persisted answer would be keyed by, and what tells a dispatch
/// that was retried from one that is new.
///
/// Its json form is the hex and it is read back through [`GraphRunId::from_hex`], which that
/// method's own doc already demanded: a run recorded outside this process is recorded as its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct GraphRunId([u8; GRAPH_RUN_BYTES]);

impl GraphRunId {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "graph run id";

    /// Draws a new graph run id from the operating system entropy source.
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; GRAPH_RUN_BYTES];
        getrandom::fill(&mut bytes)?;
        Ok(Self(bytes))
    }

    /// The hex form, which is what an event and a log carry.
    #[must_use]
    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }

    /// Reads back what [`GraphRunId::to_hex`] wrote.
    ///
    /// The one way back: a run that is recorded anywhere outside this process is recorded as its
    /// own text, so a record of it is only as useful as this is strict.
    pub fn from_hex(raw: &str) -> Result<Self, GraphError> {
        let mut bytes = [0u8; GRAPH_RUN_BYTES];
        hex::decode_to_slice(raw, &mut bytes).map_err(|_| GraphError::NotARunId {
            value: raw.to_owned(),
        })?;
        Ok(Self(bytes))
    }
}

impl fmt::Display for GraphRunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl TryFrom<String> for GraphRunId {
    type Error = GraphError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::from_hex(&raw)
    }
}

impl From<GraphRunId> for String {
    fn from(run: GraphRunId) -> Self {
        run.to_hex()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_an_operator_may_spell_is_taken() {
        for raw in ["triage", "fan-out.2", "a_b", "A1", &"g".repeat(64)] {
            assert!(GraphId::new(raw).is_ok(), "{raw} was refused as an id");
            assert!(NodeName::new(raw).is_ok(), "{raw} was refused as a name");
        }
    }

    #[test]
    fn a_key_that_is_not_one_is_refused_by_what_is_wrong_with_it() {
        assert_eq!(
            NodeName::new(""),
            Err(GraphError::Empty {
                kind: NodeName::KIND
            })
        );
        assert_eq!(
            NodeName::new(&"g".repeat(65)),
            Err(GraphError::TooLong {
                kind: NodeName::KIND,
                len: 65,
                max: 64,
            })
        );
        for (raw, ch) in [("two words", ' '), ("a/b", '/'), ("héllo", 'é')] {
            assert_eq!(
                NodeName::new(raw),
                Err(GraphError::IllegalChar {
                    kind: NodeName::KIND,
                    ch,
                }),
                "{raw} was taken"
            );
        }
    }

    #[test]
    fn a_key_is_read_from_json_through_its_own_rule() {
        assert_eq!(
            serde_json::from_str::<NodeName>(r#""triage""#).expect("a name a node may carry"),
            NodeName::new("triage").expect("the same name")
        );
        assert!(
            serde_json::from_str::<GraphId>(r#""two words""#).is_err(),
            "json got past the rule the constructor applies"
        );
    }

    #[test]
    fn a_label_carries_what_a_key_may_not() {
        for raw in ["Support triage", "工单分流", "a, b and c"] {
            assert!(GraphName::new(raw).is_ok(), "{raw} was refused");
        }
    }

    #[test]
    fn a_label_is_refused_rather_than_repaired() {
        assert_eq!(
            GraphName::new(" triage"),
            Err(GraphError::Untrimmed {
                kind: GraphName::KIND
            })
        );
        assert_eq!(
            GraphName::new("a\nb"),
            Err(GraphError::IllegalChar {
                kind: GraphName::KIND,
                ch: '\n',
            })
        );
        assert!(GraphName::new("").is_err(), "an empty label was taken");
    }

    #[test]
    fn a_key_prints_as_it_was_spelled() {
        assert_eq!(GraphId::new("triage").expect("an id").to_string(), "triage");
        assert_eq!(NodeName::new("triage").expect("a name").as_str(), "triage");
        assert_eq!(
            GraphName::new("Support triage")
                .expect("a label")
                .to_string(),
            "Support triage"
        );
    }

    /// The hex, as a json string — not the sixteen numbers the bytes would otherwise become.
    #[test]
    fn a_run_id_is_written_down_as_its_hex_and_read_back() {
        let run = GraphRunId::generate().expect("a run id");
        let written = serde_json::to_string(&run).expect("a run id writes down");

        assert_eq!(written, format!("\"{}\"", run.to_hex()));
        assert_eq!(
            serde_json::from_str::<GraphRunId>(&written).expect("a run id reads back"),
            run
        );
    }

    /// Reading goes through `from_hex`, so the wire is as strict as the record.
    #[test]
    fn a_json_string_that_is_not_a_run_id_is_refused() {
        for raw in [r#""not hex""#, r#""abcd""#, "[1, 2, 3]"] {
            assert!(
                serde_json::from_str::<GraphRunId>(raw).is_err(),
                "{raw} was taken as a run id"
            );
        }
    }
}
