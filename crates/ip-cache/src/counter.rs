//! Running counts a limit is checked against, kept only as long as the stretch they count.

use async_trait::async_trait;

use crate::error::CacheError;
use crate::key::CacheKey;
use crate::ttl::Ttl;

/// Counts that go up and let go of themselves.
///
/// A count is the cache's own, not a record: it is seeded from what storage holds and lost
/// with the cache, which costs a reseed rather than a fresh allowance.
#[async_trait]
pub trait Counters: Send + Sync {
    /// Adds `by` to the count under `key` and reports what it then stands at, starting it at
    /// `by` when nothing is counted there yet and letting it go after `ttl`.
    async fn count(&self, key: &CacheKey, by: u64, ttl: Ttl) -> Result<u64, CacheError>;

    /// What is counted under `key`, or nothing when no count was started.
    async fn counted(&self, key: &CacheKey) -> Result<Option<u64>, CacheError>;

    /// Starts the count under `key` at `from` unless one is already there, reporting what it
    /// stands at either way. However many callers seed at once, the first one decides.
    async fn seed(&self, key: &CacheKey, from: u64, ttl: Ttl) -> Result<u64, CacheError>;
}

/// A count as a backend stores it.
#[cfg(feature = "standalone-cache")]
pub(crate) fn counted_bytes(count: u64) -> Vec<u8> {
    count.to_be_bytes().to_vec()
}

/// Reads a stored count back, refusing bytes that are not one.
#[cfg(feature = "standalone-cache")]
pub(crate) fn count_of(stored: &[u8]) -> Result<u64, CacheError> {
    let counted: [u8; size_of::<u64>()] =
        stored.try_into().map_err(|_| CacheError::MalformedCount {
            detail: format!("{} bytes are not a count", stored.len()),
        })?;
    Ok(u64::from_be_bytes(counted))
}

#[cfg(all(test, feature = "standalone-cache"))]
mod tests {
    use super::*;

    #[test]
    fn a_count_reads_back_as_what_was_stored() {
        assert_eq!(count_of(&counted_bytes(1_234)).unwrap(), 1_234);
        assert_eq!(count_of(&counted_bytes(u64::MAX)).unwrap(), u64::MAX);
    }

    #[test]
    fn bytes_that_are_not_a_count_are_refused() {
        assert!(matches!(
            count_of(b"short"),
            Err(CacheError::MalformedCount { .. })
        ));
    }
}
