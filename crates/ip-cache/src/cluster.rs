use async_trait::async_trait;
use redis::aio::ConnectionManager;
use redis::{Client, cmd};

use crate::cache::Cache;
use crate::error::CacheError;
use crate::key::CacheKey;
use crate::ttl::Ttl;

/// A word every key of one cache carries, which is what deployments sharing a redis need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefix(String);

impl Prefix {
    /// Wraps a prefix, refusing one redis could not carry in a key.
    pub fn new(prefix: &str) -> Result<Self, CacheError> {
        if prefix.is_empty() || prefix.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(CacheError::UnusablePrefix {
                prefix: prefix.to_owned(),
            });
        }
        Ok(Self(prefix.to_owned()))
    }

    /// The prefix as a key carries it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The cluster backend: the redis every node shares.
#[derive(Debug, Clone)]
pub struct RedisCache {
    connection: ConnectionManager,
    prefix: Option<Prefix>,
}

impl RedisCache {
    /// Connects to the server `url` names, keying entries as `CacheKey` spells them.
    pub async fn connect(url: &str) -> Result<Self, CacheError> {
        Self::open(url, None).await
    }

    /// Connects to the server `url` names, keying every entry under `prefix`.
    pub async fn connect_under(url: &str, prefix: Prefix) -> Result<Self, CacheError> {
        Self::open(url, Some(prefix)).await
    }

    async fn open(url: &str, prefix: Option<Prefix>) -> Result<Self, CacheError> {
        let client = Client::open(url).map_err(CacheError::backend)?;
        let connection = client
            .get_connection_manager()
            .await
            .map_err(CacheError::backend)?;
        Ok(Self { connection, prefix })
    }

    /// The key as this cache stores it, under its prefix when it has one.
    fn scoped(&self, key: &CacheKey) -> String {
        match &self.prefix {
            Some(prefix) => format!("{}:{key}", prefix.as_str()),
            None => key.as_str().to_owned(),
        }
    }
}

#[async_trait]
impl Cache for RedisCache {
    async fn get(&self, key: &CacheKey) -> Result<Option<Vec<u8>>, CacheError> {
        let mut connection = self.connection.clone();
        cmd("GET")
            .arg(self.scoped(key))
            .query_async(&mut connection)
            .await
            .map_err(CacheError::backend)
    }

    async fn put(&self, key: &CacheKey, value: &[u8], ttl: Option<Ttl>) -> Result<(), CacheError> {
        let mut connection = self.connection.clone();
        let mut set = cmd("SET");
        set.arg(self.scoped(key)).arg(value);
        if let Some(ttl) = ttl {
            set.arg("PX").arg(millis(ttl));
        }
        set.query_async::<()>(&mut connection)
            .await
            .map_err(CacheError::backend)
    }

    async fn claim(&self, key: &CacheKey, ttl: Option<Ttl>) -> Result<bool, CacheError> {
        let mut connection = self.connection.clone();
        let mut set = cmd("SET");
        set.arg(self.scoped(key)).arg(b"".as_slice()).arg("NX");
        if let Some(ttl) = ttl {
            set.arg("PX").arg(millis(ttl));
        }
        // Redis answers a refused NX with nil, which is the whole race this backend has to settle.
        let taken: Option<String> = set
            .query_async(&mut connection)
            .await
            .map_err(CacheError::backend)?;
        Ok(taken.is_some())
    }

    async fn remove(&self, key: &CacheKey) -> Result<bool, CacheError> {
        let mut connection = self.connection.clone();
        let dropped: i64 = cmd("DEL")
            .arg(self.scoped(key))
            .query_async(&mut connection)
            .await
            .map_err(CacheError::backend)?;
        Ok(dropped > 0)
    }
}

/// A ttl in the milliseconds redis counts, capped at what its `PX` argument holds.
fn millis(ttl: Ttl) -> i64 {
    i64::try_from(ttl.get().as_millis()).unwrap_or(i64::MAX)
}

/// Caches under a prefix of their own, on the server `REDIS_URL` names.
#[cfg(test)]
pub(crate) mod scratch {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// A cache whose prefix, and every key under it, goes when the test does.
    #[derive(Debug)]
    pub(crate) struct ScratchCache {
        inner: RedisCache,
        url: String,
        prefix: Prefix,
    }

    #[async_trait]
    impl Cache for ScratchCache {
        async fn get(&self, key: &CacheKey) -> Result<Option<Vec<u8>>, CacheError> {
            self.inner.get(key).await
        }

        async fn put(
            &self,
            key: &CacheKey,
            value: &[u8],
            ttl: Option<Ttl>,
        ) -> Result<(), CacheError> {
            self.inner.put(key, value, ttl).await
        }

        async fn claim(&self, key: &CacheKey, ttl: Option<Ttl>) -> Result<bool, CacheError> {
            self.inner.claim(key, ttl).await
        }

        async fn remove(&self, key: &CacheKey) -> Result<bool, CacheError> {
            self.inner.remove(key).await
        }
    }

    impl Drop for ScratchCache {
        fn drop(&mut self) {
            // The cache's own connection belongs to the test's runtime, which a drop cannot drive.
            let swept = sweep(&self.url, &self.prefix);
            if swept.is_err() {
                eprintln!("could not sweep scratch prefix {}", self.prefix.as_str());
            }
        }
    }

    /// Deletes every key under `prefix`, over a connection of its own.
    fn sweep(url: &str, prefix: &Prefix) -> redis::RedisResult<()> {
        let client = Client::open(url)?;
        let mut connection = client.get_connection()?;
        let keys: Vec<String> = cmd("KEYS")
            .arg(format!("{}:*", prefix.as_str()))
            .query(&mut connection)?;
        if !keys.is_empty() {
            cmd("DEL").arg(keys).query::<i64>(&mut connection)?;
        }
        Ok(())
    }

    /// A cache under a new prefix on the server `REDIS_URL` names, or none when it is unset.
    pub(crate) async fn cache() -> Option<ScratchCache> {
        let Ok(url) = std::env::var("REDIS_URL") else {
            eprintln!("REDIS_URL is unset, so this redis test checks nothing");
            return None;
        };
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let ordinal = NEXT.fetch_add(1, Ordering::Relaxed);
        let prefix = Prefix::new(&format!("scratch-{stamp}-{ordinal}")).expect("a usable prefix");
        let inner = RedisCache::connect_under(&url, prefix.clone())
            .await
            .expect("a redis on REDIS_URL");
        Some(ScratchCache { inner, url, prefix })
    }
}

#[cfg(test)]
mod suite {
    crate::suite::cache_suite!(crate::cluster::scratch::cache);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prefix_keeps_what_it_was_given() {
        assert_eq!(Prefix::new("tenant-a").unwrap().as_str(), "tenant-a");
    }

    #[test]
    fn a_prefix_of_nothing_is_refused() {
        assert!(matches!(
            Prefix::new(""),
            Err(CacheError::UnusablePrefix { prefix }) if prefix.is_empty()
        ));
    }

    #[test]
    fn a_prefix_holding_whitespace_is_refused() {
        assert!(matches!(
            Prefix::new("two words"),
            Err(CacheError::UnusablePrefix { .. })
        ));
    }

    #[test]
    fn a_ttl_longer_than_redis_counts_is_capped() {
        let ttl = Ttl::new(std::time::Duration::from_secs(u64::MAX / 1_000)).unwrap();
        assert_eq!(millis(ttl), i64::MAX);
    }
}
