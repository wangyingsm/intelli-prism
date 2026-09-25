use async_trait::async_trait;
use std::time::Duration;

use futures_util::StreamExt;
use redis::aio::{ConnectionManager, PubSub};
use redis::{Client, cmd};
use tokio::sync::watch;

use crate::cache::Cache;
use crate::counter::Counters;
use crate::error::CacheError;
use crate::key::CacheKey;
use crate::publication::{Publication, Published, announce};
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
    client: Client,
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
        Ok(Self {
            client,
            connection,
            prefix,
        })
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

/// Checks the revision, writes and announces in one step, so no writer lands between the check
/// and the write. The hash holds the revision and the value side by side.
const PUBLISH: &str = r"
local current = redis.call('HGET', KEYS[1], 'revision')
if current and tonumber(current) >= tonumber(ARGV[1]) then
    return 0
end
redis.call('HSET', KEYS[1], 'revision', ARGV[1], 'value', ARGV[2])
redis.call('PUBLISH', KEYS[2], ARGV[1])
return 1
";

/// Adds to a count, giving it the stretch it belongs to the first time. A count that lost its
/// expiry, which nothing here does, is given it again rather than left to outlive its period.
const COUNT: &str = r"
local total = redis.call('INCRBY', KEYS[1], ARGV[1])
if redis.call('PTTL', KEYS[1]) < 0 then
    redis.call('PEXPIRE', KEYS[1], ARGV[2])
end
return total
";

/// Starts a count unless one is there, which is what seeding from storage needs: however many
/// nodes seed at once, the first decides and the rest read what it wrote.
const SEED: &str = r"
if redis.call('SET', KEYS[1], ARGV[1], 'NX', 'PX', ARGV[2]) then
    return tonumber(ARGV[1])
end
return tonumber(redis.call('GET', KEYS[1]))
";

/// How long a follower waits before subscribing again after losing its connection.
const RESUBSCRIBE_FIRST: Duration = Duration::from_secs(1);

/// The longest a follower waits between attempts to subscribe again.
const RESUBSCRIBE_MOST: Duration = Duration::from_secs(30);

impl RedisCache {
    /// The channel a topic's changes are announced on.
    fn channel(&self, topic: &CacheKey) -> String {
        format!("{}:published", self.scoped(topic))
    }

    /// A connection of its own, subscribed to one channel.
    async fn subscribed(&self, channel: &str) -> Result<PubSub, CacheError> {
        let mut pubsub = self
            .client
            .get_async_pubsub()
            .await
            .map_err(CacheError::backend)?;
        pubsub
            .subscribe(channel)
            .await
            .map_err(CacheError::backend)?;
        Ok(pubsub)
    }

    /// Hands every announcement on to the follower until it lets go, subscribing again after a
    /// lost connection and catching up on whatever was published while it was gone.
    async fn keep_following(
        self,
        mut pubsub: PubSub,
        channel: String,
        topic: CacheKey,
        follower: watch::Sender<u64>,
    ) {
        loop {
            let mut announcements = pubsub.into_on_message();
            loop {
                tokio::select! {
                    announcement = announcements.next() => match announcement {
                        Some(announcement) => {
                            if let Ok(revision) = announcement.get_payload::<u64>() {
                                announce(&follower, revision);
                            }
                        }
                        None => break,
                    },
                    () = follower.closed() => return,
                }
            }
            let mut wait = RESUBSCRIBE_FIRST;
            pubsub = loop {
                tokio::select! {
                    () = tokio::time::sleep(wait) => {}
                    () = follower.closed() => return,
                }
                if let Ok(pubsub) = self.subscribed(&channel).await {
                    break pubsub;
                }
                wait = (wait * 2).min(RESUBSCRIBE_MOST);
            };
            if let Ok(Some(published)) = self.published(&topic).await {
                announce(&follower, published.revision);
            }
        }
    }
}

#[async_trait]
impl Counters for RedisCache {
    async fn count(&self, key: &CacheKey, by: u64, ttl: Ttl) -> Result<u64, CacheError> {
        let mut connection = self.connection.clone();
        cmd("EVAL")
            .arg(COUNT)
            .arg(1)
            .arg(self.scoped(key))
            .arg(by)
            .arg(millis(ttl))
            .query_async(&mut connection)
            .await
            .map_err(CacheError::backend)
    }

    async fn counted(&self, key: &CacheKey) -> Result<Option<u64>, CacheError> {
        let mut connection = self.connection.clone();
        cmd("GET")
            .arg(self.scoped(key))
            .query_async(&mut connection)
            .await
            .map_err(CacheError::backend)
    }

    async fn seed(&self, key: &CacheKey, from: u64, ttl: Ttl) -> Result<u64, CacheError> {
        let mut connection = self.connection.clone();
        cmd("EVAL")
            .arg(SEED)
            .arg(1)
            .arg(self.scoped(key))
            .arg(from)
            .arg(millis(ttl))
            .query_async(&mut connection)
            .await
            .map_err(CacheError::backend)
    }
}

#[async_trait]
impl Publication for RedisCache {
    async fn publish(
        &self,
        topic: &CacheKey,
        revision: u64,
        value: &[u8],
    ) -> Result<bool, CacheError> {
        let mut connection = self.connection.clone();
        let landed: i64 = cmd("EVAL")
            .arg(PUBLISH)
            .arg(2)
            .arg(self.scoped(topic))
            .arg(self.channel(topic))
            .arg(revision)
            .arg(value)
            .query_async(&mut connection)
            .await
            .map_err(CacheError::backend)?;
        Ok(landed == 1)
    }

    async fn published(&self, topic: &CacheKey) -> Result<Option<Published>, CacheError> {
        let mut connection = self.connection.clone();
        let (revision, value): (Option<u64>, Option<Vec<u8>>) = cmd("HMGET")
            .arg(self.scoped(topic))
            .arg("revision")
            .arg("value")
            .query_async(&mut connection)
            .await
            .map_err(CacheError::backend)?;
        Ok(match (revision, value) {
            (Some(revision), Some(value)) => Some(Published { revision, value }),
            _ => None,
        })
    }

    async fn follow(&self, topic: &CacheKey) -> Result<watch::Receiver<u64>, CacheError> {
        let channel = self.channel(topic);
        // Subscribing comes before the read, so nothing published between the two is missed.
        let pubsub = self.subscribed(&channel).await?;
        let now = self
            .published(topic)
            .await?
            .map_or(0, |published| published.revision);
        let (follower, following) = watch::channel(now);
        tokio::spawn(
            self.clone()
                .keep_following(pubsub, channel, topic.clone(), follower),
        );
        Ok(following)
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

    #[async_trait]
    impl Publication for ScratchCache {
        async fn publish(
            &self,
            topic: &CacheKey,
            revision: u64,
            value: &[u8],
        ) -> Result<bool, CacheError> {
            self.inner.publish(topic, revision, value).await
        }

        async fn published(&self, topic: &CacheKey) -> Result<Option<Published>, CacheError> {
            self.inner.published(topic).await
        }

        async fn follow(&self, topic: &CacheKey) -> Result<watch::Receiver<u64>, CacheError> {
            self.inner.follow(topic).await
        }
    }

    #[async_trait]
    impl Counters for ScratchCache {
        async fn count(&self, key: &CacheKey, by: u64, ttl: Ttl) -> Result<u64, CacheError> {
            self.inner.count(key, by, ttl).await
        }

        async fn counted(&self, key: &CacheKey) -> Result<Option<u64>, CacheError> {
            self.inner.counted(key).await
        }

        async fn seed(&self, key: &CacheKey, from: u64, ttl: Ttl) -> Result<u64, CacheError> {
            self.inner.seed(key, from, ttl).await
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
    crate::suite::publication_suite!(crate::cluster::scratch::cache);
    crate::suite::counter_suite!(crate::cluster::scratch::cache);
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
