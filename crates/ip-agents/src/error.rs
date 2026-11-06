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

    /// A count or a span of nought, where its kind needs at least one.
    #[error("{kind} must not be zero")]
    Zero {
        /// What was being read.
        kind: &'static str,
    },

    /// A number past the ceiling its kind allows.
    #[error("{kind} is {value}, over the limit of {max}")]
    TooLarge {
        /// What was being read.
        kind: &'static str,
        /// What it was.
        value: u64,
        /// The most its kind allows.
        max: u64,
    },

    /// More items than its kind holds.
    #[error("{kind} carries {count} items, over the limit of {max}")]
    TooMany {
        /// What was being read.
        kind: &'static str,
        /// How many there were.
        count: usize,
        /// The most its kind holds.
        max: usize,
    },

    /// One item named twice where each may be named once.
    #[error("{kind} names {value:?} more than once")]
    Duplicate {
        /// What was being read.
        kind: &'static str,
        /// The item named twice.
        value: String,
    },

    /// A stored name this code does not carry.
    #[error("unknown {kind} {value:?}")]
    Unknown {
        /// What was being read.
        kind: &'static str,
        /// The text that named nothing.
        value: String,
    },

    /// A request allowed to outlive the run it belongs to.
    #[error("request timeout of {timeout}s outlives the run's own limit of {limit}s")]
    TimeoutOutlivesRun {
        /// What one request was allowed.
        timeout: u32,
        /// What the whole run was allowed.
        limit: u32,
    },

    /// A strategy for a best-effort run the template does not ask for.
    #[error("a best effort strategy is set while best effort is off")]
    StrategyWithoutBestEffort,
}
