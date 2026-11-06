#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
/// Every way a value in this crate can fail to be built.
pub enum AgentError {
    /// A value that must carry something carries nothing.
    #[error("{kind} is empty")]
    Empty {
        /// What was being read.
        kind: &'static str,
    },

    /// A value past the length its kind allows.
    #[error("{kind} is {len} bytes, over the {max} byte limit")]
    TooLong {
        /// What was being read.
        kind: &'static str,
        /// How long it was.
        len: usize,
        /// The longest its kind allows.
        max: usize,
    },

    /// A value carrying a character its kind does not admit.
    #[error("{kind} contains the illegal character {ch:?}")]
    IllegalChar {
        /// What was being read.
        kind: &'static str,
        /// The first character that was not allowed.
        ch: char,
    },
}
