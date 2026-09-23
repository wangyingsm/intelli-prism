//! A cache that fails when it is told to, for the arms a working one never reaches.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use tokio::sync::watch;

use crate::cache::Cache;
use crate::error::{CacheError, ToldToFail};
use crate::key::CacheKey;
use crate::publication::{CacheBackend, Publication, Published};
use crate::ttl::Ttl;

/// A cache that answers like the one it wraps until the calls it was given run out, and
/// refuses every call after that.
///
/// What a caller does when the cache cannot answer — carry on, log, fall back to the store —
/// is only reachable with a cache that fails where a test says.
pub struct FailingCache {
    inner: Arc<dyn CacheBackend>,
    answers_left: AtomicUsize,
}

impl FailingCache {
    /// A cache that answers everything, until it is told otherwise.
    pub fn new(inner: Arc<dyn CacheBackend>) -> Self {
        Self {
            inner,
            answers_left: AtomicUsize::new(usize::MAX),
        }
    }

    /// Answers `calls` more times, then refuses everything.
    pub fn answer_only(&self, calls: usize) {
        self.answers_left.store(calls, Ordering::SeqCst);
    }

    /// Refuses every call from here on.
    pub fn fail_now(&self) {
        self.answer_only(0);
    }

    /// Answers everything again, however many calls were left.
    pub fn answer_again(&self) {
        self.answers_left.store(usize::MAX, Ordering::SeqCst);
    }

    /// Takes one of the answers left, or refuses when there are none.
    fn answering(&self) -> Result<(), CacheError> {
        let taken = self
            .answers_left
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                (left > 0).then(|| left.saturating_sub(1))
            });
        match taken {
            Ok(_) => Ok(()),
            Err(_) => Err(CacheError::backend(ToldToFail)),
        }
    }
}

#[async_trait]
impl Cache for FailingCache {
    async fn get(&self, key: &CacheKey) -> Result<Option<Vec<u8>>, CacheError> {
        self.answering()?;
        self.inner.get(key).await
    }

    async fn put(&self, key: &CacheKey, value: &[u8], ttl: Option<Ttl>) -> Result<(), CacheError> {
        self.answering()?;
        self.inner.put(key, value, ttl).await
    }

    async fn claim(&self, key: &CacheKey, ttl: Option<Ttl>) -> Result<bool, CacheError> {
        self.answering()?;
        self.inner.claim(key, ttl).await
    }

    async fn remove(&self, key: &CacheKey) -> Result<bool, CacheError> {
        self.answering()?;
        self.inner.remove(key).await
    }
}

#[async_trait]
impl Publication for FailingCache {
    async fn publish(
        &self,
        topic: &CacheKey,
        revision: u64,
        value: &[u8],
    ) -> Result<bool, CacheError> {
        self.answering()?;
        self.inner.publish(topic, revision, value).await
    }

    async fn published(&self, topic: &CacheKey) -> Result<Option<Published>, CacheError> {
        self.answering()?;
        self.inner.published(topic).await
    }

    async fn follow(&self, topic: &CacheKey) -> Result<watch::Receiver<u64>, CacheError> {
        self.answering()?;
        self.inner.follow(topic).await
    }
}

#[cfg(all(test, feature = "standalone-cache"))]
mod tests {
    use crate::key::CacheLevel;
    use crate::standalone::SledCache;

    use super::*;

    fn failing() -> FailingCache {
        FailingCache::new(Arc::new(SledCache::temporary().unwrap()))
    }

    fn key() -> CacheKey {
        CacheKey::new(CacheLevel::System, "told-to-fail").unwrap()
    }

    fn told_to_fail(error: &CacheError) -> bool {
        matches!(error, CacheError::Backend(source) if source.is::<ToldToFail>())
    }

    #[tokio::test]
    async fn a_cache_answers_until_it_is_told_to_stop() {
        let cache = failing();
        assert!(cache.get(&key()).await.is_ok());

        cache.fail_now();
        let refused = cache.get(&key()).await.unwrap_err();
        assert!(told_to_fail(&refused), "{refused:?}");
    }

    #[tokio::test]
    async fn the_call_it_fails_on_is_the_one_it_was_given() {
        let cache = failing();
        cache.answer_only(1);
        assert!(cache.put(&key(), b"kept", None).await.is_ok());
        assert!(told_to_fail(&cache.get(&key()).await.unwrap_err()));
    }

    #[tokio::test]
    async fn publishing_fails_the_same_way_reading_does() {
        let cache = failing();
        cache.fail_now();
        assert!(told_to_fail(
            &cache.publish(&key(), 1, b"rules").await.unwrap_err()
        ));
        assert!(told_to_fail(&cache.published(&key()).await.unwrap_err()));
        assert!(told_to_fail(&cache.follow(&key()).await.unwrap_err()));

        cache.answer_again();
        assert!(cache.publish(&key(), 1, b"rules").await.unwrap());
    }
}
