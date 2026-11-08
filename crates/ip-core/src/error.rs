//! Every way a core type can refuse the value it was given.

use crate::capability::{Capability, ScopeKind};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
/// Every way a core value can fail to be built.
pub enum CoreError {
    /// A value that may not be empty is.
    #[error("{kind} is empty")]
    Empty {
        /// What was being read.
        kind: &'static str,
    },

    /// A value longer than its own type allows.
    #[error("{kind} is {len} bytes, over the {max} byte limit")]
    TooLong {
        /// What was being read.
        kind: &'static str,
        /// How long it was.
        len: usize,
        /// The most it may be.
        max: usize,
    },

    /// A value shorter than its own type allows.
    #[error("{kind} is {len} bytes, under the {min} byte minimum")]
    TooShort {
        /// What was being read.
        kind: &'static str,
        /// How long it was.
        len: usize,
        /// The least it may be.
        min: usize,
    },

    /// A value holding a character its own type refuses.
    #[error("{kind} contains the illegal character {ch:?}")]
    IllegalChar {
        /// What was being read.
        kind: &'static str,
        /// The character it may not hold.
        ch: char,
    },

    /// A hex value of the wrong width.
    #[error("{kind} is not {expected} hex characters")]
    HexLength {
        /// What was being read.
        kind: &'static str,
        /// How many characters it must be.
        expected: usize,
    },

    /// A value that is not hex at all.
    #[error("{kind} is not valid hex")]
    HexDigit {
        /// What was being read.
        kind: &'static str,
    },

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

    /// Something a limit may count that this application does not.
    #[error("unknown limit kind {value:?}")]
    UnknownCounted {
        /// The text that named no kind.
        value: String,
    },

    /// A dispatch strategy this application does not carry.
    #[error("unknown strategy {value:?}")]
    UnknownStrategy {
        /// The text that named no strategy.
        value: String,
    },

    /// A difficulty class this application does not carry.
    #[error("unknown difficulty {value:?}")]
    UnknownDifficulty {
        /// The text that named no class.
        value: String,
    },

    /// A level of the margin this application does not carry.
    #[error("unknown decider {value:?}")]
    UnknownDecider {
        /// The text that named no level.
        value: String,
    },

    /// A provider family this application does not carry.
    #[error("unknown dialect {value:?}")]
    UnknownDialect {
        /// The text that named no family.
        value: String,
    },

    /// A stretch of clock this application does not count over.
    #[error("unknown period {value:?}")]
    UnknownPeriod {
        /// The text that named no period.
        value: String,
    },

    /// A plugin order outside the range its scope may use.
    #[error("order {order} is not available to a {scope} plugin rule")]
    OrderOutOfScope {
        /// The order that was asked for.
        order: u8,
        /// Which scope asked for it.
        scope: &'static str,
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

    /// A grant whose scope is not the shape its capability is granted at.
    #[error("{capability:?} is scoped to {expected:?}, not {actual:?}")]
    ScopeMismatch {
        /// The capability that was granted.
        capability: Capability,
        /// The shape it must be scoped at.
        expected: ScopeKind,
        /// The shape it was scoped at.
        actual: ScopeKind,
    },

    /// A hedge of nothing, which would race a request across no endpoint at all.
    #[error("a hedge of nothing reaches no endpoint")]
    ZeroHedge,

    /// A hedge count past what this application will race a request across.
    #[error("a hedge of {asked} is past the {most} this gateway races across")]
    HedgeTooWide {
        /// What the rule asked for.
        asked: u8,
        /// The most it may ask for.
        most: u8,
    },

    /// A redaction tally describing nothing: a count with no kinds named, or kinds
    /// with nothing counted. The two describe each other and one without the other is
    /// a row nobody can read.
    #[error("a redaction of {count} names {kinds:?}, and the two do not describe each other")]
    RedactedKinds {
        /// How many the row said were taken out.
        count: u32,
        /// What it said they were.
        kinds: String,
    },

    /// A price past anything a vendor has ever charged, which is what a decimal point
    /// in the wrong place looks like.
    #[error("a rate of {asked} micros a million is past the {most} this gateway will price")]
    RateTooDear {
        /// What was quoted.
        asked: u64,
        /// The most it may be.
        most: u64,
    },
}
