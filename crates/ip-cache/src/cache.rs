use async_trait::async_trait;

use crate::error::CacheError;
use crate::key::CacheKey;
use crate::ttl::Ttl;

/// What every cache backend does.
#[async_trait]
pub trait Cache: Send + Sync {
    /// Reads an entry, or nothing when it is absent or its ttl has passed.
    async fn get(&self, key: &CacheKey) -> Result<Option<Vec<u8>>, CacheError>;

    /// Writes an entry, replacing whatever was there and starting its ttl again.
    async fn put(&self, key: &CacheKey, value: &[u8], ttl: Ttl) -> Result<(), CacheError>;

    /// Takes a key nothing else holds, reporting whether this caller took it. However many
    /// callers race for one key, exactly one is told it took it, which is what spending a
    /// nonce needs.
    async fn claim(&self, key: &CacheKey, ttl: Ttl) -> Result<bool, CacheError>;

    /// Drops an entry, reporting whether one was there.
    async fn remove(&self, key: &CacheKey) -> Result<bool, CacheError>;
}
