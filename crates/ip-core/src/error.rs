use crate::capability::{Capability, ScopeKind};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
/// Every way a core value can fail to be built.
pub enum CoreError {
    #[error("{kind} is empty")]
    Empty { kind: &'static str },

    #[error("{kind} is {len} bytes, over the {max} byte limit")]
    TooLong {
        kind: &'static str,
        len: usize,
        max: usize,
    },

    #[error("{kind} is {len} bytes, under the {min} byte minimum")]
    TooShort {
        kind: &'static str,
        len: usize,
        min: usize,
    },

    #[error("{kind} contains the illegal character {ch:?}")]
    IllegalChar { kind: &'static str, ch: char },

    #[error("{kind} is not {expected} hex characters")]
    HexLength { kind: &'static str, expected: usize },

    #[error("{kind} is not valid hex")]
    HexDigit { kind: &'static str },

    /// A value that must open with a particular character does not.
    #[error("{kind} must start with {expected:?}")]
    MustStartWith {
        /// What was being read.
        kind: &'static str,
        /// The character it must open with.
        expected: char,
    },

    /// A protocol this application does not speak.
    #[error("unknown protocol {value:?}")]
    UnknownProtocol {
        /// The text that named no protocol.
        value: String,
    },

    /// A plugin kind this application does not run.
    #[error("unknown plugin kind {value:?}")]
    UnknownPluginKind {
        /// The text that named no kind.
        value: String,
    },

    /// A route target that names nowhere to send.
    #[error("a route target names no endpoint")]
    NoEndpoint,

    /// Port zero names no service.
    #[error("port zero is not a port")]
    ZeroPort,

    /// A stored number that is too large to be a port.
    #[error("{value} is outside the range of a port")]
    PortOutOfRange {
        /// The number that named no port.
        value: i64,
    },

    /// A count of seconds that lands outside any representable moment.
    #[error("{seconds} is not a moment in time")]
    Timestamp {
        /// The count that could not be read as a moment.
        seconds: i64,
    },

    #[error("{capability:?} is scoped to {expected:?}, not {actual:?}")]
    ScopeMismatch {
        capability: Capability,
        expected: ScopeKind,
        actual: ScopeKind,
    },
}
