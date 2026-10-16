//! Every way reading a body into the canonical shape can refuse.

/// Why a body could not be read as a conversation or as an answer.
///
/// Refusing to read is not a failure of the gateway: most bodies that reach a route are
/// not conversations at all, and an embeddings request the reader declines is a request
/// this layer has nothing to say about rather than one that went wrong.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    /// The bytes are not json, so no family could have sent them.
    #[error("the body is not json: {0}")]
    NotJson(String),

    /// Json, but holding none of the things that make a conversation: no messages, or
    /// none with anything in them. Embeddings, moderation and plain completions land
    /// here, and so does a conversation of another family.
    #[error("the body carries no conversation this family would send")]
    NotAConversation,

    /// Json, but not an answer of this family: no choices, no content, no candidates.
    #[error("the body carries no answer this family would send")]
    NotAnAnswer,

    /// A tool call whose id is empty. The id is the only join between a call and the
    /// result that answers it, so one that cannot be held is a conversation that cannot
    /// be put back together.
    #[error("a tool call must carry an id")]
    EmptyToolCallId,

    /// A tool with no name: nothing can be called, and nothing can declare it.
    #[error("a tool must carry a name")]
    EmptyToolName,
}
