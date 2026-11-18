/// How this crate refuses what it is given.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RetrieveError {
    /// Nothing where something was required.
    #[error("{kind} is empty")]
    Empty {
        /// Which kind of thing was empty.
        kind: &'static str,
    },

    /// Longer than this code will hold.
    #[error("{kind} is {len} bytes, past the {max} it may be")]
    TooLong {
        /// Which kind of thing was too long.
        kind: &'static str,
        /// What was offered.
        len: usize,
        /// What is allowed.
        max: usize,
    },

    /// More of something than this code will hold.
    #[error("{kind} holds {len}, past the {max} it may")]
    TooMany {
        /// Which kind of thing held too much.
        kind: &'static str,
        /// What was offered.
        len: usize,
        /// What is allowed.
        max: usize,
    },

    /// A character this kind of thing may not carry.
    #[error("{kind} may not carry {ch:?}")]
    IllegalChar {
        /// Which kind of thing carried it.
        kind: &'static str,
        /// The character.
        ch: char,
    },

    /// Whitespace at an edge, which is refused rather than trimmed away.
    #[error("{kind} may not begin or end with whitespace")]
    Untrimmed {
        /// Which kind of thing was padded.
        kind: &'static str,
    },

    /// A url this gateway will not call: one carrying no scheme it speaks, no host, or a
    /// shape that is not a url at all.
    #[error("an endpoint must be an http or https url, and {value:?} is not")]
    NotAnEndpoint {
        /// What was offered.
        value: String,
    },

    /// A number outside the range this kind of thing is bounded to.
    #[error("{kind} is {value}, and may be from 1 to {max}")]
    OutOfRange {
        /// Which kind of thing was out of range.
        kind: &'static str,
        /// What was offered.
        value: u32,
        /// The highest it may be.
        max: u32,
    },

    /// A retriever offering no tool at all, which nothing could ever call.
    ///
    /// Unlike a template's tool set, which is empty for an agent that is a conversation: a
    /// retriever exists only to be asked something.
    #[error("a retriever must offer at least one tool")]
    OffersNothing,

    /// A tool the source it is declared on cannot answer.
    ///
    /// The field is `backend` rather than `source`, which reads better here and is a name
    /// `thiserror` takes for the error's own cause.
    #[error("{backend} cannot answer {tool}, which needs edges to walk")]
    NotForThisSource {
        /// The source as it is spelled.
        backend: &'static str,
        /// The tool as it is spelled.
        tool: &'static str,
    },
}
