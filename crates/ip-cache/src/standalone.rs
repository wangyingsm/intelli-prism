use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use sled::transaction::{ConflictableTransactionError, Transactional};
use sled::{Db, IVec, Tree};
use tokio::sync::watch;

use crate::cache::Cache;
use crate::counter::{Counters, count_of, counted_bytes};
use crate::error::CacheError;
use crate::key::{CacheKey, CacheLevel};
use crate::limits::LevelLimits;
use crate::publication::{Publication, Published, announce, decode, encode};
use crate::ttl::Ttl;

/// Bytes an entry carries before its value, holding when the entry expires.
const STAMP: usize = size_of::<u64>();

/// The moment an entry written with no ttl expires, which no clock reaches.
const NEVER: u64 = u64::MAX;

/// Bytes an index key carries before the entry's own key: the level and when it was last used.
const INDEX_PREFIX: usize = 1 + size_of::<u64>();

/// How stale a recorded use may be before a read writes a fresher one, which keeps a busy
/// key from writing to the index on every single read.
const REFRESH_AFTER_MICROS: u64 = 1_000_000;

/// The standalone backend: one local sled database.
#[derive(Debug, Clone)]
pub struct SledCache {
    entries: Db,
    used: Tree,
    index: Tree,
    sizes: Tree,
    published: Tree,
    limits: LevelLimits,
}

impl SledCache {
    /// Opens the database at `path`, creating it when it is not there yet.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CacheError> {
        Self::with_limits(path, LevelLimits::none())
    }

    /// Opens the database at `path`, evicting each level's least recently used entries once
    /// it holds more than `limits` allows.
    pub fn with_limits(path: impl AsRef<Path>, limits: LevelLimits) -> Result<Self, CacheError> {
        let db = sled::open(path).map_err(CacheError::backend)?;
        Self::around(db, limits)
    }

    /// Opens a database that lives only as long as the cache, for tests and dry runs.
    pub fn temporary() -> Result<Self, CacheError> {
        Self::temporary_with_limits(LevelLimits::none())
    }

    /// Opens a temporary database that holds no more than `limits` allows.
    pub fn temporary_with_limits(limits: LevelLimits) -> Result<Self, CacheError> {
        let db = sled::Config::new()
            .temporary(true)
            .open()
            .map_err(CacheError::backend)?;
        Self::around(db, limits)
    }

    fn around(db: Db, limits: LevelLimits) -> Result<Self, CacheError> {
        let used = db.open_tree("used").map_err(CacheError::backend)?;
        let index = db.open_tree("index").map_err(CacheError::backend)?;
        let sizes = db.open_tree("sizes").map_err(CacheError::backend)?;
        let published = db.open_tree("published").map_err(CacheError::backend)?;
        Ok(Self {
            entries: db,
            used,
            index,
            sizes,
            published,
            limits,
        })
    }

    /// Drops the least recently used entries of `level` until it is inside its limit.
    fn evict(&self, level: CacheLevel) -> Result<(), CacheError> {
        let Some(limit) = self.limits.of(level) else {
            return Ok(());
        };
        let tag = [tag_of(level)];
        while self.held(level)? > limit.get() {
            let Some(oldest) = self
                .index
                .scan_prefix(tag)
                .next()
                .transpose()
                .map_err(CacheError::backend)?
            else {
                return Ok(());
            };
            let key = oldest.0[INDEX_PREFIX..].to_vec();
            self.drop_entry(level, &key)?;
        }
        Ok(())
    }

    /// How many bytes a level holds.
    fn held(&self, level: CacheLevel) -> Result<u64, CacheError> {
        let held = self
            .sizes
            .get([tag_of(level)])
            .map_err(CacheError::backend)?;
        Ok(held.as_deref().and_then(counted).unwrap_or(0))
    }

    /// Removes one entry with everything that accounts for it, reporting whether it was there.
    fn drop_entry(&self, level: CacheLevel, key: &[u8]) -> Result<bool, CacheError> {
        let dropped = (&*self.entries, &self.used, &self.index, &self.sizes)
            .transaction(|(entries, used, index, sizes)| {
                let Some(entry) = entries.remove(key)? else {
                    return Ok::<_, ConflictableTransactionError>(None);
                };
                if let Some(last_used) = used.remove(key)? {
                    index.remove(index_key(level, &last_used, key))?;
                }
                let size = weigh(key, entry.len());
                let held = sizes.get([tag_of(level)])?;
                let held = held.as_deref().and_then(counted).unwrap_or(0);
                sizes.insert(&[tag_of(level)], &held.saturating_sub(size).to_be_bytes())?;
                Ok(Some(entry))
            })
            .map_err(CacheError::backend)?;
        Ok(dropped.is_some_and(|entry| value_of(&entry, now()).is_some()))
    }

    /// Writes an entry, accounting for what it replaced.
    fn write(&self, level: CacheLevel, key: &[u8], entry: &[u8]) -> Result<(), CacheError> {
        let stamped = now_micros().to_be_bytes();
        (&*self.entries, &self.used, &self.index, &self.sizes)
            .transaction(|(entries, used, index, sizes)| {
                let replaced = entries.insert(key, entry)?;
                let mut held = sizes
                    .get([tag_of(level)])?
                    .as_deref()
                    .and_then(counted)
                    .unwrap_or(0);
                if let Some(replaced) = replaced {
                    held = held.saturating_sub(weigh(key, replaced.len()));
                }
                if let Some(last_used) = used.insert(key, &stamped)? {
                    index.remove(index_key(level, &last_used, key))?;
                }
                index.insert(index_key(level, &stamped, key), &[])?;
                held = held.saturating_add(weigh(key, entry.len()));
                sizes.insert(&[tag_of(level)], &held.to_be_bytes())?;
                Ok::<_, ConflictableTransactionError>(())
            })
            .map_err(CacheError::backend)?;
        self.evict(level)
    }

    /// Records that a key was just read, unless the recorded use is fresh enough already.
    fn touch(&self, level: CacheLevel, key: &[u8]) -> Result<(), CacheError> {
        if !self.limits.any() {
            return Ok(());
        }
        let now = now_micros();
        let last_used = self.used.get(key).map_err(CacheError::backend)?;
        if last_used
            .as_deref()
            .and_then(counted)
            .is_some_and(|last| now.saturating_sub(last) < REFRESH_AFTER_MICROS)
        {
            return Ok(());
        }
        let stamped = now.to_be_bytes();
        (&self.used, &self.index)
            .transaction(|(used, index)| {
                if let Some(previous) = used.insert(key, &stamped)? {
                    index.remove(index_key(level, &previous, key))?;
                }
                index.insert(index_key(level, &stamped, key), &[])?;
                Ok::<_, ConflictableTransactionError>(())
            })
            .map_err(CacheError::backend)
    }
}

#[async_trait]
impl Cache for SledCache {
    async fn get(&self, key: &CacheKey) -> Result<Option<Vec<u8>>, CacheError> {
        let this = self.clone();
        let level = key.level();
        let key = key.as_str().to_owned();
        blocking(move || {
            let Some(entry) = this.entries.get(&key).map_err(CacheError::backend)? else {
                return Ok(None);
            };
            match value_of(&entry, now()) {
                Some(value) => {
                    let value = value.to_vec();
                    this.touch(level, key.as_bytes())?;
                    Ok(Some(value))
                }
                None => {
                    this.drop_entry(level, key.as_bytes())?;
                    Ok(None)
                }
            }
        })
        .await
    }

    async fn put(&self, key: &CacheKey, value: &[u8], ttl: Option<Ttl>) -> Result<(), CacheError> {
        let this = self.clone();
        let level = key.level();
        let key = key.as_str().to_owned();
        let entry = entry(value, ttl);
        blocking(move || this.write(level, key.as_bytes(), &entry)).await
    }

    async fn claim(&self, key: &CacheKey, ttl: Option<Ttl>) -> Result<bool, CacheError> {
        let this = self.clone();
        let level = key.level();
        let key = key.as_str().to_owned();
        let entry = entry(&[], ttl);
        blocking(move || {
            loop {
                let held = this.entries.get(&key).map_err(CacheError::backend)?;
                match &held {
                    Some(entry) if value_of(entry, now()).is_some() => return Ok(false),
                    // A key nothing holds any more is taken by writing over it, which the
                    // swap below only lets one caller do.
                    Some(stale) => {
                        let swapped = this
                            .entries
                            .compare_and_swap(
                                &key,
                                Some(stale.clone()),
                                Some(IVec::from(entry.clone())),
                            )
                            .map_err(CacheError::backend)?;
                        if swapped.is_ok() {
                            this.write(level, key.as_bytes(), &entry)?;
                            return Ok(true);
                        }
                    }
                    None => {
                        let swapped = this
                            .entries
                            .compare_and_swap(&key, None::<IVec>, Some(IVec::from(entry.clone())))
                            .map_err(CacheError::backend)?;
                        if swapped.is_ok() {
                            this.write(level, key.as_bytes(), &entry)?;
                            return Ok(true);
                        }
                    }
                }
            }
        })
        .await
    }

    async fn remove(&self, key: &CacheKey) -> Result<bool, CacheError> {
        let this = self.clone();
        let level = key.level();
        let key = key.as_str().to_owned();
        blocking(move || this.drop_entry(level, key.as_bytes())).await
    }
}

#[async_trait]
impl Counters for SledCache {
    async fn count(&self, key: &CacheKey, by: u64, ttl: Ttl) -> Result<u64, CacheError> {
        self.counting(key, ttl, move |counted| {
            counted.unwrap_or(0).saturating_add(by)
        })
        .await
    }

    async fn counted(&self, key: &CacheKey) -> Result<Option<u64>, CacheError> {
        let this = self.clone();
        let level = key.level();
        let key = key.as_str().to_owned();
        blocking(move || {
            let held = this.entries.get(&key).map_err(CacheError::backend)?;
            match held.as_deref().and_then(|entry| value_of(entry, now())) {
                Some(counted) => count_of(counted).map(Some),
                None => {
                    this.drop_entry(level, key.as_bytes())?;
                    Ok(None)
                }
            }
        })
        .await
    }

    async fn seed(&self, key: &CacheKey, from: u64, ttl: Ttl) -> Result<u64, CacheError> {
        self.counting(key, ttl, move |counted| counted.unwrap_or(from))
            .await
    }
}

impl SledCache {
    /// Replaces a count with what `next` makes of it, retrying until no other writer lands in
    /// between, which is what one node counting for several requests at once needs.
    async fn counting(
        &self,
        key: &CacheKey,
        ttl: Ttl,
        next: impl Fn(Option<u64>) -> u64 + Send + 'static,
    ) -> Result<u64, CacheError> {
        let this = self.clone();
        let level = key.level();
        let key = key.as_str().to_owned();
        blocking(move || {
            loop {
                let held = this.entries.get(&key).map_err(CacheError::backend)?;
                let counted = match held.as_deref().and_then(|entry| value_of(entry, now())) {
                    Some(counted) => Some(count_of(counted)?),
                    None => None,
                };
                let total = next(counted);
                let written = entry(&counted_bytes(total), Some(ttl));
                let swapped = this
                    .entries
                    .compare_and_swap(&key, held.clone(), Some(IVec::from(written.clone())))
                    .map_err(CacheError::backend)?;
                if swapped.is_ok() {
                    this.write(level, key.as_bytes(), &written)?;
                    return Ok(total);
                }
            }
        })
        .await
    }
}

/// Runs sled's blocking work off the reactor.
async fn blocking<T, F>(work: F) -> Result<T, CacheError>
where
    F: FnOnce() -> Result<T, CacheError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(CacheError::backend)?
}

/// Milliseconds since the unix epoch, which is how an entry stamps its expiry.
fn now() -> u64 {
    since_epoch(1_000)
}

/// Microseconds since the unix epoch, which is how finely a use is ordered.
fn now_micros() -> u64 {
    since_epoch(1_000_000)
}

fn since_epoch(per_second: u128) -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            let ticks = since.as_nanos() / (1_000_000_000 / per_second);
            u64::try_from(ticks).unwrap_or(u64::MAX)
        })
}

/// What one entry costs its level, key and value together.
fn weigh(key: &[u8], entry: usize) -> u64 {
    u64::try_from(key.len().saturating_add(entry)).unwrap_or(u64::MAX)
}

/// Reads a counter back, or nothing when the bytes are not one.
fn counted(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(bytes.try_into().ok()?))
}

/// The byte a level is written under, so one level's entries scan together.
fn tag_of(level: CacheLevel) -> u8 {
    match level {
        CacheLevel::Response => 0,
        CacheLevel::Semantic => 1,
        CacheLevel::System => 2,
    }
}

/// Orders one level's keys by when each was last used, oldest first.
fn index_key(level: CacheLevel, last_used: &[u8], key: &[u8]) -> Vec<u8> {
    let mut index = Vec::with_capacity(INDEX_PREFIX + key.len());
    index.push(tag_of(level));
    index.extend_from_slice(last_used);
    index.extend_from_slice(key);
    index
}

/// Stamps a value with the moment its ttl runs out, or with `NEVER` when it has none.
fn entry(value: &[u8], ttl: Option<Ttl>) -> Vec<u8> {
    let expires_at = match ttl {
        Some(ttl) => {
            let span = u64::try_from(ttl.get().as_millis()).unwrap_or(u64::MAX);
            now().saturating_add(span)
        }
        None => NEVER,
    };
    let mut entry = Vec::with_capacity(STAMP + value.len());
    entry.extend_from_slice(&expires_at.to_be_bytes());
    entry.extend_from_slice(value);
    entry
}

#[async_trait]
impl Publication for SledCache {
    async fn publish(
        &self,
        topic: &CacheKey,
        revision: u64,
        value: &[u8],
    ) -> Result<bool, CacheError> {
        let key = topic.as_str().as_bytes();
        let replacement = encode(revision, value);
        loop {
            let current = self.published.get(key).map_err(CacheError::backend)?;
            if let Some(current) = &current
                && decode(current)?.revision >= revision
            {
                return Ok(false);
            }
            let swapped = self
                .published
                .compare_and_swap(key, current, Some(replacement.as_slice()))
                .map_err(CacheError::backend)?;
            if swapped.is_ok() {
                return Ok(true);
            }
        }
    }

    async fn published(&self, topic: &CacheKey) -> Result<Option<Published>, CacheError> {
        self.published
            .get(topic.as_str().as_bytes())
            .map_err(CacheError::backend)?
            .map(|stored| decode(&stored))
            .transpose()
    }

    async fn follow(&self, topic: &CacheKey) -> Result<watch::Receiver<u64>, CacheError> {
        let key = IVec::from(topic.as_str().as_bytes());
        // Watching starts before the read, so nothing published between the two is missed.
        let mut changes = self.published.watch_prefix(key.clone());
        let now = self
            .published(topic)
            .await?
            .map_or(0, |published| published.revision);
        let (follower, following) = watch::channel(now);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    change = &mut changes => match change {
                        Some(sled::Event::Insert { key: changed, value }) if changed == key => {
                            if let Ok(published) = decode(&value) {
                                announce(&follower, published.revision);
                            }
                        }
                        Some(_) => {}
                        None => return,
                    },
                    () = follower.closed() => return,
                }
            }
        });
        Ok(following)
    }
}

/// Reads an entry's value back, or nothing once its ttl has passed.
fn value_of(entry: &[u8], now: u64) -> Option<&[u8]> {
    let (stamp, value) = entry.split_at_checked(STAMP)?;
    let expires_at = u64::from_be_bytes(stamp.try_into().ok()?);
    (expires_at > now).then_some(value)
}

#[cfg(test)]
mod suite {
    /// A cache on a database that goes when the test does.
    async fn open() -> Option<super::SledCache> {
        Some(super::SledCache::temporary().unwrap())
    }

    crate::suite::cache_suite!(crate::standalone::suite::open);
    crate::suite::publication_suite!(crate::standalone::suite::open);
    crate::suite::counter_suite!(crate::standalone::suite::open);
}

#[cfg(test)]
mod tests {
    use crate::limits::MaxBytes;

    use super::*;

    /// A cache whose response level holds `bytes` before the least recently used entry goes.
    fn holding(bytes: u64) -> SledCache {
        SledCache::temporary_with_limits(
            LevelLimits::none().with_response(MaxBytes::new(bytes).unwrap()),
        )
        .unwrap()
    }

    fn key(level: CacheLevel, id: &str) -> CacheKey {
        CacheKey::new(level, id).unwrap()
    }

    fn a_while() -> Option<Ttl> {
        Some(Ttl::seconds(60).unwrap())
    }

    #[test]
    fn an_entry_carries_its_value_after_the_stamp() {
        let entry = entry(b"body", Some(Ttl::seconds(60).unwrap()));
        assert_eq!(entry.len(), STAMP + 4);
        assert_eq!(value_of(&entry, now()), Some(b"body".as_slice()));
    }

    #[test]
    fn an_entry_whose_stamp_has_passed_reads_as_nothing() {
        let entry = entry(b"body", Some(Ttl::seconds(60).unwrap()));
        assert_eq!(value_of(&entry, u64::MAX), None);
    }

    #[test]
    fn an_entry_with_no_ttl_outlasts_every_clock() {
        let entry = entry(b"body", None);
        assert_eq!(value_of(&entry, u64::MAX - 1), Some(b"body".as_slice()));
    }

    #[test]
    fn bytes_too_short_to_hold_a_stamp_read_as_nothing() {
        assert_eq!(value_of(&[0, 1, 2], now()), None);
    }

    #[test]
    fn every_level_scans_under_its_own_tag() {
        let tags = [
            tag_of(CacheLevel::Response),
            tag_of(CacheLevel::Semantic),
            tag_of(CacheLevel::System),
        ];
        assert_eq!(
            tags.len(),
            tags.iter().collect::<std::collections::HashSet<_>>().len()
        );
    }

    #[tokio::test]
    async fn a_level_holds_what_its_entries_weigh() {
        let cache = holding(4_096);
        cache
            .put(&key(CacheLevel::Response, "a"), b"body", a_while())
            .await
            .unwrap();
        let held = cache.held(CacheLevel::Response).unwrap();
        assert_eq!(held, weigh(b"ip:resp:a", STAMP + 4));
        cache.remove(&key(CacheLevel::Response, "a")).await.unwrap();
        assert_eq!(cache.held(CacheLevel::Response).unwrap(), 0);
    }

    #[tokio::test]
    async fn writing_over_an_entry_does_not_count_it_twice() {
        let cache = holding(4_096);
        cache
            .put(&key(CacheLevel::Response, "a"), b"body", a_while())
            .await
            .unwrap();
        let once = cache.held(CacheLevel::Response).unwrap();
        cache
            .put(&key(CacheLevel::Response, "a"), b"body", a_while())
            .await
            .unwrap();
        assert_eq!(cache.held(CacheLevel::Response).unwrap(), once);
    }

    #[tokio::test]
    async fn a_level_over_its_limit_drops_its_oldest_entry() {
        // Room for two entries of this size, so the third pushes the first out.
        let cache = holding(2 * weigh(b"ip:resp:a", STAMP + 8));
        for id in ["a", "b"] {
            cache
                .put(&key(CacheLevel::Response, id), b"12345678", a_while())
                .await
                .unwrap();
        }
        cache
            .put(&key(CacheLevel::Response, "c"), b"12345678", a_while())
            .await
            .unwrap();
        assert_eq!(
            cache.get(&key(CacheLevel::Response, "a")).await.unwrap(),
            None
        );
        assert!(
            cache
                .get(&key(CacheLevel::Response, "b"))
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            cache
                .get(&key(CacheLevel::Response, "c"))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn reading_an_entry_keeps_it_from_being_the_one_that_goes() {
        let cache = holding(2 * weigh(b"ip:resp:a", STAMP + 8));
        cache
            .put(&key(CacheLevel::Response, "a"), b"12345678", a_while())
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
        cache
            .put(&key(CacheLevel::Response, "b"), b"12345678", a_while())
            .await
            .unwrap();
        // Reading the older entry makes the newer one the least recently used.
        tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
        assert!(
            cache
                .get(&key(CacheLevel::Response, "a"))
                .await
                .unwrap()
                .is_some()
        );
        cache
            .put(&key(CacheLevel::Response, "c"), b"12345678", a_while())
            .await
            .unwrap();
        assert!(
            cache
                .get(&key(CacheLevel::Response, "a"))
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            cache.get(&key(CacheLevel::Response, "b")).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn one_level_filling_up_leaves_another_alone() {
        let cache = holding(weigh(b"ip:resp:a", STAMP + 8));
        cache
            .put(&key(CacheLevel::System, "kept"), b"12345678", a_while())
            .await
            .unwrap();
        for id in ["a", "b", "c"] {
            cache
                .put(&key(CacheLevel::Response, id), b"12345678", a_while())
                .await
                .unwrap();
        }
        assert!(
            cache
                .get(&key(CacheLevel::System, "kept"))
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            cache.get(&key(CacheLevel::Response, "a")).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_level_nothing_limits_keeps_everything() {
        let cache = SledCache::temporary().unwrap();
        for id in ["a", "b", "c", "d"] {
            cache
                .put(&key(CacheLevel::Response, id), b"12345678", a_while())
                .await
                .unwrap();
        }
        for id in ["a", "b", "c", "d"] {
            assert!(
                cache
                    .get(&key(CacheLevel::Response, id))
                    .await
                    .unwrap()
                    .is_some()
            );
        }
    }
}
