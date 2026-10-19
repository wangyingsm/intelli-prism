//! What one request cost, in the units every layer counts it in.

use std::fmt;

use crate::error::CoreError;

/// Tokens counted for one request.
///
/// Held as a `u32`: no model counts one request past four billion tokens, and the narrower
/// type makes every conversion to what a database stores exact.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct TokenCount(u32);

impl TokenCount {
    /// No tokens at all, which is what an answer out of the cache spends.
    pub const ZERO: Self = Self(0);

    /// Wraps a count.
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// The count as it is stored and reported.
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for TokenCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What one request spent, in each direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Tokens {
    /// Tokens the request carried to the model.
    pub input: TokenCount,
    /// Tokens the model answered with.
    pub output: TokenCount,
}

impl Tokens {
    /// Nothing spent in either direction.
    pub const ZERO: Self = Self {
        input: TokenCount::ZERO,
        output: TokenCount::ZERO,
    };

    /// Wraps what was spent in each direction.
    pub const fn new(input: TokenCount, output: TokenCount) -> Self {
        Self { input, output }
    }

    /// Both directions together, which is what a quota counts against.
    pub const fn total(self) -> u64 {
        self.input.get() as u64 + self.output.get() as u64
    }
}

/// Where the answer a request was given came from.
///
/// The two cache levels are told apart rather than counted together, because they are
/// different promises. An exact hit is the same bytes to the same question and cannot
/// be wrong; a semantic one is another question's answer, judged near enough. After an
/// incident the only question anyone will ask of this feature is *which answers were
/// approximate*, and a column that said only `cache` could not answer it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Served {
    /// The upstream model answered, and was paid in tokens for it.
    #[default]
    Upstream,
    /// The cache answered the same question with the same bytes, spending nothing.
    Cache,
    /// The cache answered a question that meant the same thing, spending nothing.
    Semantic,
}

/// How long a request took, to the millisecond.
///
/// Held as a `u32`, which is seven weeks: longer than any request a gateway waits out.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct Latency(u32);

impl Latency {
    /// Wraps a span of milliseconds, taking anything longer as the longest it can hold.
    pub fn from_millis(millis: u128) -> Self {
        Self(u32::try_from(millis).unwrap_or(u32::MAX))
    }

    /// The span in milliseconds, as it is stored and reported.
    pub const fn millis(self) -> u32 {
        self.0
    }
}

impl fmt::Display for Latency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}ms", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_directions_count_towards_the_total() {
        let tokens = Tokens::new(TokenCount::new(120), TokenCount::new(30));
        assert_eq!(tokens.total(), 150);
        assert_eq!(Tokens::ZERO.total(), 0);
    }

    #[test]
    fn a_span_no_count_can_hold_is_the_longest_it_can() {
        assert_eq!(Latency::from_millis(1_500).millis(), 1_500);
        assert_eq!(Latency::from_millis(u128::MAX).millis(), u32::MAX);
    }

    #[test]
    fn counts_and_spans_round_trip_as_plain_numbers() {
        assert_eq!(serde_json::to_string(&TokenCount::new(7)).unwrap(), "7");
        assert_eq!(
            serde_json::to_string(&Latency::from_millis(9)).unwrap(),
            "9"
        );
        assert_eq!(serde_json::to_string(&Served::Cache).unwrap(), "\"cache\"");
        assert_eq!(
            serde_json::from_str::<TokenCount>("7").unwrap(),
            TokenCount::new(7)
        );
    }

    #[test]
    fn what_is_spent_is_reported_the_way_it_is_counted() {
        assert_eq!(TokenCount::new(42).to_string(), "42");
        assert_eq!(Latency::from_millis(42).to_string(), "42ms");
    }
}

/// What a request gave up to the filter: how many secrets were taken out, and of what
/// kinds.
///
/// The two are one value because they are settled together, and a row carrying a count
/// with no kinds named — or kinds with nothing counted — is a row nobody can read. Held
/// so that such a pair cannot be built.
///
/// [`Redacted::NONE`] is a request no filter looked at, which is not the same as one it
/// looked at and found nothing in: the first says the gateway had no filter configured,
/// or could not hold the body to scan it, and the second says the body was clean.
/// Telling them apart is the whole value of the column.
///
/// Counts and kinds, never values. A ledger that stored the secrets it found would have
/// moved the leak rather than closed it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Redacted(Option<Taken>);

/// What one filtered request gave up.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Taken {
    count: u32,
    kinds: String,
}

impl Redacted {
    /// No filter looked at this request.
    pub const NONE: Self = Self(None);

    /// Looked at, and this much was taken out. The kinds are as the filter named them,
    /// in the order it named them.
    ///
    /// # Errors
    ///
    /// [`CoreError::RedactedKinds`] when a count is given with no kinds, or kinds with
    /// no count: the two describe each other and one without the other is a value this
    /// type will not hold.
    pub fn of(count: u32, kinds: &str) -> Result<Self, CoreError> {
        if (count == 0) != kinds.is_empty() {
            return Err(CoreError::RedactedKinds {
                count,
                kinds: kinds.to_owned(),
            });
        }
        Ok(Self(Some(Taken {
            count,
            kinds: kinds.to_owned(),
        })))
    }

    /// Looked at, and there was nothing to take out.
    #[must_use]
    pub fn clean() -> Self {
        Self(Some(Taken {
            count: 0,
            kinds: String::new(),
        }))
    }

    /// How many were taken out, and none where no filter looked.
    #[must_use]
    pub const fn count(&self) -> Option<u32> {
        match &self.0 {
            Some(taken) => Some(taken.count),
            None => None,
        }
    }

    /// The kinds taken out, and none where no filter looked.
    #[must_use]
    pub fn kinds(&self) -> Option<&str> {
        self.0.as_ref().map(|taken| taken.kinds.as_str())
    }

    /// Whether a filter looked at this request at all.
    #[must_use]
    pub const fn looked(&self) -> bool {
        self.0.is_some()
    }
}

#[cfg(test)]
mod redacted_tests {
    use super::*;

    #[test]
    fn a_request_no_filter_looked_at_says_nothing() {
        let none = Redacted::NONE;
        assert!(!none.looked());
        assert_eq!(none.count(), None);
        assert_eq!(none.kinds(), None);
        assert_eq!(none, Redacted::default());
    }

    /// The distinction the column exists for: looked at and clean is not the same as
    /// never looked at, and a reader must be able to tell them apart.
    #[test]
    fn looked_at_and_clean_is_not_the_same_as_never_looked_at() {
        let clean = Redacted::clean();
        assert!(clean.looked());
        assert_eq!(clean.count(), Some(0));
        assert_eq!(clean.kinds(), Some(""));
        assert_ne!(clean, Redacted::NONE);
    }

    #[test]
    fn what_was_taken_is_kept_as_it_was_named() {
        let taken = Redacted::of(3, "email=2, openai-key=1").unwrap();
        assert_eq!(taken.count(), Some(3));
        assert_eq!(taken.kinds(), Some("email=2, openai-key=1"));
    }

    /// A count with no kinds, or kinds with no count, describes nothing. Refused here
    /// so no row can hold one.
    #[test]
    fn a_count_without_kinds_or_kinds_without_a_count_is_refused() {
        assert!(Redacted::of(2, "").is_err());
        assert!(Redacted::of(0, "email=1").is_err());
        assert!(Redacted::of(0, "").is_ok(), "nothing taken, nothing named");
    }

    #[test]
    fn a_refusal_says_what_it_was_given() {
        let said = Redacted::of(2, "").unwrap_err().to_string();
        assert!(said.contains('2'), "{said}");
    }
}
