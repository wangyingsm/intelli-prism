/// How this crate refuses what it is given.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpError {
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

    /// A bearer kind declaring a token, or one declaring none and carrying one.
    #[error("a bearer credential needs a token, and no other kind may carry one")]
    TokenWithoutBearer,
}
