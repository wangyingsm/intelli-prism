//! Values every node follows: moved only forward, by revision, and announced when they move.

use async_trait::async_trait;
use tokio::sync::watch;

use crate::cache::Cache;
use crate::counter::Counters;
use crate::error::CacheError;
use crate::key::CacheKey;

/// What is published under a topic, and the revision it was published at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    /// How far the value has moved on; a publication never goes back.
    pub revision: u64,
    /// The value itself.
    pub value: Vec<u8>,
}

/// Publishes a value under a topic for every node to follow.
///
/// A write carries the revision it was made at and lands only when that is newer than what
/// is there, so two writers finishing out of order can never leave the older value on top.
#[async_trait]
pub trait Publication: Send + Sync {
    /// Publishes `value` at `revision` and announces it, unless what is published is as new
    /// already. Reports whether it landed.
    async fn publish(
        &self,
        topic: &CacheKey,
        revision: u64,
        value: &[u8],
    ) -> Result<bool, CacheError>;

    /// What is published under a topic now.
    async fn published(&self, topic: &CacheKey) -> Result<Option<Published>, CacheError>;

    /// Follows a topic. The receiver starts at the revision published now, or zero, and holds
    /// the newest announced since: a burst of changes wakes a follower once.
    async fn follow(&self, topic: &CacheKey) -> Result<watch::Receiver<u64>, CacheError>;
}

/// A cache that also publishes and counts, which is what every backend is.
pub trait CacheBackend: Cache + Publication + Counters {}

impl<T> CacheBackend for T where T: Cache + Publication + Counters {}

/// Moves a follower on to `revision`, reporting whether that was news to it.
pub(crate) fn announce(follower: &watch::Sender<u64>, revision: u64) -> bool {
    follower.send_if_modified(|seen| {
        let newer = revision > *seen;
        if newer {
            *seen = revision;
        }
        newer
    })
}

/// A published value as sled stores it: the revision, then the value.
#[cfg(feature = "standalone-cache")]
pub(crate) fn encode(revision: u64, value: &[u8]) -> Vec<u8> {
    let mut stored = Vec::with_capacity(size_of::<u64>() + value.len());
    stored.extend_from_slice(&revision.to_be_bytes());
    stored.extend_from_slice(value);
    stored
}

/// Reads back what [`encode`] wrote.
#[cfg(feature = "standalone-cache")]
pub(crate) fn decode(stored: &[u8]) -> Result<Published, CacheError> {
    let Some((revision, value)) = stored.split_first_chunk::<{ size_of::<u64>() }>() else {
        return Err(CacheError::MalformedPublication {
            detail: format!("{} bytes cannot hold a revision", stored.len()),
        });
    };
    Ok(Published {
        revision: u64::from_be_bytes(*revision),
        value: value.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "standalone-cache")]
    #[test]
    fn a_published_value_reads_back_with_its_revision() {
        let read = decode(&encode(7, b"rules")).unwrap();
        assert_eq!(
            read,
            Published {
                revision: 7,
                value: b"rules".to_vec()
            }
        );
    }

    #[cfg(feature = "standalone-cache")]
    #[test]
    fn a_value_too_short_to_hold_a_revision_is_refused() {
        assert!(matches!(
            decode(b"short"),
            Err(CacheError::MalformedPublication { .. })
        ));
    }

    #[test]
    fn a_follower_hears_only_what_is_newer_than_it_has() {
        let (follower, following) = watch::channel(5);
        assert!(!announce(&follower, 5));
        assert!(!announce(&follower, 3));
        assert!(announce(&follower, 6));
        assert_eq!(*following.borrow(), 6);
    }
}
