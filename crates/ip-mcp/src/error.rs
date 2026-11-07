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

    /// A server that could not be reached, answered too slowly, or dropped the connection.
    #[error("the server could not be reached: {detail}")]
    Unreachable {
        /// What went wrong on the way.
        detail: String,
    },

    /// A server that answered with a status rather than with a message.
    #[error("the server answered {status}")]
    Refused {
        /// The http status it answered with.
        status: u16,
    },

    /// An answer that is not a JSON-RPC message, whatever else it may be.
    #[error("the server did not answer in json-rpc: {detail}")]
    NotJsonRpc {
        /// What was wrong with it.
        detail: String,
    },

    /// An answer to some other call, which is not an answer to this one.
    #[error("the server answered a call that was not the one made")]
    AnsweredSomethingElse,

    /// A server reporting, in json-rpc, that it would not do what was asked.
    #[error("the server refused: {message} ({code})")]
    ServerRefused {
        /// The code the server chose.
        code: i64,
        /// What it said about it.
        message: String,
    },

    /// A server speaking a version of the protocol this code does not.
    #[error("the server speaks {theirs:?} and this speaks {ours:?}")]
    UnsupportedProtocol {
        /// What the server answered the handshake with.
        theirs: String,
        /// What was asked for.
        ours: &'static str,
    },

    /// An answer longer than this will read from a server it does not run.
    #[error("the server answered with more than the {max} bytes this will read")]
    AnswerTooLong {
        /// What is allowed.
        max: usize,
    },

    /// Something a server offered as a tool that is not one.
    #[error("the server offered something that is not a tool: {detail}")]
    NotATool {
        /// What was wrong with it.
        detail: String,
    },

    /// A server handing a listing over in more pages than this will follow.
    #[error("the server paged its answer past the {max} pages this will read")]
    TooManyPages {
        /// What is allowed.
        max: usize,
    },
}
