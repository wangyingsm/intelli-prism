use std::time::Duration;

use crate::error::CacheError;

/// How long an entry stays before the cache drops it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ttl(Duration);

impl Ttl {
    /// Wraps a span, refusing one that would expire on arrival.
    pub fn new(span: Duration) -> Result<Self, CacheError> {
        if span.is_zero() {
            return Err(CacheError::ZeroTtl);
        }
        Ok(Self(span))
    }

    /// Wraps a whole number of seconds.
    pub fn seconds(seconds: u64) -> Result<Self, CacheError> {
        Self::new(Duration::from_secs(seconds))
    }

    /// The span itself.
    pub fn get(self) -> Duration {
        self.0
    }

    /// The span in whole seconds, rounded up and never zero, as a backend counts it.
    pub fn whole_seconds(self) -> u64 {
        u64::try_from(self.0.as_millis().div_ceil(1_000)).unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ttl_keeps_the_span_it_was_given() {
        let ttl = Ttl::seconds(300).unwrap();
        assert_eq!(ttl.get(), Duration::from_secs(300));
        assert_eq!(ttl.whole_seconds(), 300);
    }

    #[test]
    fn a_span_between_seconds_rounds_up() {
        let ttl = Ttl::new(Duration::from_millis(1_200)).unwrap();
        assert_eq!(ttl.whole_seconds(), 2);
    }

    #[test]
    fn a_span_under_a_second_still_lasts_one() {
        let ttl = Ttl::new(Duration::from_millis(1)).unwrap();
        assert_eq!(ttl.whole_seconds(), 1);
    }

    #[test]
    fn a_span_of_no_time_is_refused() {
        assert!(matches!(Ttl::new(Duration::ZERO), Err(CacheError::ZeroTtl)));
        assert!(matches!(Ttl::seconds(0), Err(CacheError::ZeroTtl)));
    }
}
