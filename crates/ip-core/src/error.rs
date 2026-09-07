use crate::capability::{Capability, ScopeKind};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
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

    #[error("{capability:?} is scoped to {expected:?}, not {actual:?}")]
    ScopeMismatch {
        capability: Capability,
        expected: ScopeKind,
        actual: ScopeKind,
    },
}
