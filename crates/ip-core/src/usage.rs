//! What one request cost, in the units every layer counts it in.

use std::fmt;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Served {
    /// The upstream model answered, and was paid in tokens for it.
    #[default]
    Upstream,
    /// The cache answered, spending nothing.
    Cache,
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
