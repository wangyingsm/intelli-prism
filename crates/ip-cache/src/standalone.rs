use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use sled::{Db, IVec};

use crate::cache::Cache;
use crate::error::CacheError;
use crate::key::CacheKey;
use crate::ttl::Ttl;

/// Bytes an entry carries before its value, holding when the entry expires.
const STAMP: usize = size_of::<u64>();

/// The moment an entry written with no ttl expires, which no clock reaches.
const NEVER: u64 = u64::MAX;

/// The standalone backend: one local sled database.
#[derive(Debug, Clone)]
pub struct SledCache {
    db: Db,
}

impl SledCache {
    /// Opens the database at `path`, creating it when it is not there yet.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CacheError> {
        let db = sled::open(path).map_err(CacheError::backend)?;
        Ok(Self { db })
    }

    /// Opens a database that lives only as long as the cache, for tests and dry runs.
    pub fn temporary() -> Result<Self, CacheError> {
        let db = sled::Config::new()
            .temporary(true)
            .open()
            .map_err(CacheError::backend)?;
        Ok(Self { db })
    }
}

#[async_trait]
impl Cache for SledCache {
    async fn get(&self, key: &CacheKey) -> Result<Option<Vec<u8>>, CacheError> {
        let db = self.db.clone();
        let key = key.as_str().to_owned();
        blocking(move || {
            let Some(entry) = db.get(&key).map_err(CacheError::backend)? else {
                return Ok(None);
            };
            match value_of(&entry, now()) {
                Some(value) => Ok(Some(value.to_vec())),
                None => {
                    db.remove(&key).map_err(CacheError::backend)?;
                    Ok(None)
                }
            }
        })
        .await
    }

    async fn put(&self, key: &CacheKey, value: &[u8], ttl: Option<Ttl>) -> Result<(), CacheError> {
        let db = self.db.clone();
        let key = key.as_str().to_owned();
        let entry = entry(value, ttl);
        blocking(move || {
            db.insert(key, entry).map_err(CacheError::backend)?;
            Ok(())
        })
        .await
    }

    async fn claim(&self, key: &CacheKey, ttl: Option<Ttl>) -> Result<bool, CacheError> {
        let db = self.db.clone();
        let key = key.as_str().to_owned();
        let entry = entry(&[], ttl);
        blocking(move || {
            loop {
                let held = db.get(&key).map_err(CacheError::backend)?;
                let swapped = match &held {
                    Some(entry) if value_of(entry, now()).is_some() => return Ok(false),
                    Some(stale) => db.compare_and_swap(
                        &key,
                        Some(stale.clone()),
                        Some(IVec::from(entry.clone())),
                    ),
                    None => {
                        db.compare_and_swap(&key, None::<IVec>, Some(IVec::from(entry.clone())))
                    }
                };
                // A swap the key lost to another caller says nothing yet, so read it again.
                if swapped.map_err(CacheError::backend)?.is_ok() {
                    return Ok(true);
                }
            }
        })
        .await
    }

    async fn remove(&self, key: &CacheKey) -> Result<bool, CacheError> {
        let db = self.db.clone();
        let key = key.as_str().to_owned();
        blocking(move || {
            let dropped = db.remove(&key).map_err(CacheError::backend)?;
            Ok(dropped.is_some_and(|entry| value_of(&entry, now()).is_some()))
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
