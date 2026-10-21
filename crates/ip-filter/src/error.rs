//! Every way a declared secret can refuse to become a working one.

/// What was wrong with what an operator wrote.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FilterError {
    /// A kind's name is built into the placeholder that replaces what was found, and
    /// that placeholder is sent to the upstream, so the charset is narrow on purpose.
    #[error("{value} is not a name for a kind: {why}")]
    BadKind {
        /// The name that was refused.
        value: String,
        /// What a name is, so the message says what to write instead.
        why: String,
    },

    /// An expression that will not compile. Refusing at startup beats running without
    /// it: an operator who asked for a filter and silently got none is worse off than
    /// one whose gateway will not start.
    #[error("the {how} for {kind} will not compile: {detail}")]
    BadPattern {
        /// The kind it was declared for.
        kind: String,
        /// How it was matching — plain, prefix, suffix or regexp.
        how: &'static str,
        /// What the regular expression engine said.
        detail: String,
    },

    /// A way of matching that cannot match anything, which is almost always a line left
    /// half-written rather than an intention.
    #[error("the {how} for {kind} is empty, and would match everything or nothing")]
    EmptyPattern {
        /// The kind it was declared for.
        kind: String,
        /// How it was matching.
        how: &'static str,
    },

    /// A floor of nothing, which is what leaving it unset already means.
    #[error("a length floor of nought is the same as setting none")]
    EmptyLeast,

    /// A configuration naming a default that does not exist.
    #[error("no secret ships under the name {value}; this build carries {carried}")]
    UnknownDefault {
        /// The name that was asked for.
        value: String,
        /// Every name this build does carry.
        carried: String,
    },
}
