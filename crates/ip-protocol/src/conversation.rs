//! What a request to a model is, once the family that spelled it is forgotten.
//!
//! The shape is the union of what the three families carry, not their intersection and
//! not a superset invented here: every field below is something at least one family
//! sends and the others can be given or can do without.

use ip_core::ModelName;
use serde_json::Value;

use crate::error::ProtocolError;

/// The identifier joining a tool call to the result that answers it.
///
/// Two families carry it as a string on both halves; the third joins by function name
/// instead and has one synthesised for it. Either way nothing may hold an empty one,
/// because a call that cannot be matched to its result is a conversation with a hole.
///
/// It reads back off a wire through [`ToolCallId::new`], so an empty one is refused there too: a
/// hole that would be refused in a body is not let in because it arrived as json instead.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String")]
pub struct ToolCallId(String);

impl ToolCallId {
    /// Wraps an id, refusing an empty one.
    pub fn new(id: &str) -> Result<Self, ProtocolError> {
        Self::try_from(id.to_owned())
    }

    /// The id as the families spell it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ToolCallId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ToolCallId {
    type Error = ProtocolError;

    fn try_from(id: String) -> Result<Self, Self::Error> {
        if id.is_empty() {
            return Err(ProtocolError::EmptyToolCallId);
        }
        Ok(Self(id))
    }
}

/// What a tool is called, in a declaration and in the call that uses it.
///
/// It reads back off a wire through [`ToolName::new`], for the reason [`ToolCallId`] does.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String")]
pub struct ToolName(String);

impl ToolName {
    /// Wraps a name, refusing an empty one.
    pub fn new(name: &str) -> Result<Self, ProtocolError> {
        Self::try_from(name.to_owned())
    }

    /// The name as the families spell it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ToolName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ToolName {
    type Error = ProtocolError;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        if name.is_empty() {
            return Err(ProtocolError::EmptyToolName);
        }
        Ok(Self(name))
    }
}

/// Who a message is from.
///
/// There is no system role and no tool role. A system instruction is a field of the
/// conversation, because two of the three families put it outside the messages and
/// writing it as a message to them is refused outright. A tool result is a block inside
/// a user message, for the same reason and in the same two families: it is input to the
/// model, and only one family gives it a role of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// The caller.
    User,
    /// The model.
    Assistant,
}

/// One piece of a message.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// Plain text.
    Text(String),
    /// The model asking for a tool to be run, with the arguments it chose.
    ToolCall {
        /// What the result must quote to answer this call.
        id: ToolCallId,
        /// Which tool.
        name: ToolName,
        /// The arguments, as the model produced them.
        arguments: Value,
    },
    /// The answer to a call, carried back to the model.
    ToolResult {
        /// The call this answers.
        id: ToolCallId,
        /// What the tool returned, rendered as text.
        content: String,
    },
}

/// One turn of the conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// Who it is from.
    pub role: Role,
    /// What it holds, in order.
    pub blocks: Vec<Block>,
}

impl Message {
    /// A message of one text block.
    #[must_use]
    pub fn text(role: Role, text: &str) -> Self {
        Self {
            role,
            blocks: vec![Block::Text(text.to_owned())],
        }
    }

    /// Whether every block is a tool result, which is the shape one family gives a role
    /// of its own and the others keep inside a user message.
    #[must_use]
    pub fn is_only_tool_results(&self) -> bool {
        !self.blocks.is_empty()
            && self
                .blocks
                .iter()
                .all(|block| matches!(block, Block::ToolResult { .. }))
    }
}

/// A tool the model is told it may call.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    /// What it is called.
    pub name: ToolName,
    /// What it does, when the caller said.
    pub description: Option<String>,
    /// The json schema of its arguments, as every family carries it.
    pub schema: Value,
}

/// The knobs all three families turn, each absent when the caller did not set it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sampling {
    /// How much the model may wander.
    pub temperature: Option<f64>,
    /// Nucleus sampling's cut.
    pub top_p: Option<f64>,
    /// The ceiling on what the answer may be.
    pub max_tokens: Option<u32>,
    /// Sequences that end the answer where they appear.
    pub stop: Vec<String>,
}

/// A request to a model, in the shape every family is read into.
#[derive(Debug, Clone, PartialEq)]
pub struct Conversation {
    /// The model asked for, when the body names one. It is optional because one family
    /// names the model in the request path rather than in the body, and a conversation
    /// read from there has nowhere else to carry it.
    pub model: Option<ModelName>,
    /// The standing instruction, which no family carries as a message.
    pub system: Option<String>,
    /// The turns, oldest first.
    pub messages: Vec<Message>,
    /// What the model is told it may call.
    pub tools: Vec<Tool>,
    /// What the caller asked of the sampling.
    pub sampling: Sampling,
    /// Whether the answer was asked for as a stream.
    pub stream: bool,
}

impl Conversation {
    /// The first user message's text, which is what a turn is anchored on and what a
    /// classifier judges. Blocks that are not text are passed over.
    #[must_use]
    pub fn first_user_text(&self) -> Option<String> {
        let message = self
            .messages
            .iter()
            .find(|message| message.role == Role::User)?;
        let text = message
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        Some(text)
    }
}

/// What a translation could not carry, named so it can be said out loud.
///
/// Translation is best-effort by ruling: what maps is translated, what does not is
/// dropped and the request is still served. The cost of that is a caller who cannot
/// tell, so every adapter records here what it left behind — the names are short and
/// stable because they are meant to travel in a response header, not to be read as
/// prose.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dropped(std::collections::BTreeSet<&'static str>);

impl Dropped {
    /// Nothing was lost.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Records that something of this kind did not survive. Recording the same kind
    /// twice says no more than recording it once.
    pub fn note(&mut self, what: &'static str) {
        self.0.insert(what);
    }

    /// Whether anything was lost at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// What was lost, in a stable order.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.0.iter().copied().collect()
    }

    /// Everything the other record holds, taken into this one.
    pub fn absorb(&mut self, other: &Self) {
        self.0.extend(other.0.iter().copied());
    }
}

impl std::fmt::Display for Dropped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.names().join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_and_a_name_refuse_to_be_empty() {
        assert_eq!(ToolCallId::new(""), Err(ProtocolError::EmptyToolCallId));
        assert_eq!(ToolName::new(""), Err(ProtocolError::EmptyToolName));
        assert_eq!(ToolCallId::new("call_1").unwrap().as_str(), "call_1");
        assert_eq!(ToolName::new("search").unwrap().to_string(), "search");
        assert_eq!(ToolCallId::new("call_1").unwrap().to_string(), "call_1");
    }

    /// Both are plain json strings, and both come back as what they went out as.
    #[test]
    fn an_id_and_a_name_are_written_down_as_text_and_read_back() {
        let id = ToolCallId::new("call_1").unwrap();
        let name = ToolName::new("warehouse.get_stock").unwrap();

        assert_eq!(serde_json::to_string(&id).unwrap(), "\"call_1\"");
        assert_eq!(
            serde_json::to_string(&name).unwrap(),
            "\"warehouse.get_stock\""
        );
        assert_eq!(
            serde_json::from_str::<ToolCallId>("\"call_1\"").unwrap(),
            id
        );
        assert_eq!(
            serde_json::from_str::<ToolName>("\"warehouse.get_stock\"").unwrap(),
            name
        );
    }

    /// The emptiness rule holds on the wire too: it is the same `try_from` either way in.
    #[test]
    fn an_empty_id_or_name_is_refused_as_json_as_well() {
        assert!(serde_json::from_str::<ToolCallId>("\"\"").is_err());
        assert!(serde_json::from_str::<ToolName>("\"\"").is_err());
    }

    #[test]
    fn what_was_dropped_is_a_set_in_a_stable_order() {
        let mut dropped = Dropped::none();
        assert!(dropped.is_empty());
        dropped.note("tools");
        dropped.note("top_p");
        dropped.note("tools");
        assert_eq!(dropped.names(), vec!["tools", "top_p"]);
        assert_eq!(dropped.to_string(), "tools,top_p");

        let mut other = Dropped::none();
        other.note("stop");
        other.absorb(&dropped);
        assert_eq!(other.names(), vec!["stop", "tools", "top_p"]);
    }

    #[test]
    fn the_opening_text_is_the_first_user_messages_words() {
        let conversation = Conversation {
            model: None,
            system: None,
            messages: vec![
                Message::text(Role::Assistant, "ignored, not the user"),
                Message {
                    role: Role::User,
                    blocks: vec![
                        Block::Text("first".to_owned()),
                        Block::ToolResult {
                            id: ToolCallId::new("call_1").unwrap(),
                            content: "passed over".to_owned(),
                        },
                        Block::Text("second".to_owned()),
                    ],
                },
                Message::text(Role::User, "a later turn, not the opening"),
            ],
            tools: Vec::new(),
            sampling: Sampling::default(),
            stream: false,
        };
        assert_eq!(
            conversation.first_user_text(),
            Some("first\nsecond".to_owned())
        );
    }

    #[test]
    fn a_message_of_only_tool_results_knows_it() {
        let result = Block::ToolResult {
            id: ToolCallId::new("call_1").unwrap(),
            content: "42".to_owned(),
        };
        let only = Message {
            role: Role::User,
            blocks: vec![result.clone()],
        };
        let mixed = Message {
            role: Role::User,
            blocks: vec![result, Block::Text("and a question".to_owned())],
        };
        let empty = Message {
            role: Role::User,
            blocks: Vec::new(),
        };
        assert!(only.is_only_tool_results());
        assert!(!mixed.is_only_tool_results());
        assert!(!empty.is_only_tool_results(), "nothing is not tool results");
    }
}
