//! Sweeping away the record of requests past their keeping.

use std::sync::Arc;
use std::time::Duration;

use ip_config::UsageConfig;
use ip_core::Timestamp;
use ip_storage::{StorageError, UsageStore};
use tokio::task::JoinHandle;

use crate::rules::jitter;

/// Removes the rows the configuration no longer keeps, on a timer of its own.
pub struct Sweeper {
    store: Arc<dyn UsageStore>,
    retention: Duration,
    every: Duration,
}

impl Sweeper {
    /// A sweeper for the configured retention, or nothing at all when every row is kept.
    pub fn new(store: Arc<dyn UsageStore>, config: &UsageConfig) -> Option<Self> {
        Some(Self {
            store,
            retention: config.retention?.as_duration(),
            every: config.sweep_every.as_duration(),
        })
    }

    /// Sweeps once, reporting how many rows went.
    pub async fn sweep(&self) -> Result<u64, StorageError> {
        let kept = i64::try_from(self.retention.as_secs()).unwrap_or(i64::MAX);
        let moment = Timestamp::from_unix_seconds(Timestamp::now().unix_seconds() - kept)?;
        self.store.sweep_usage(moment).await
    }

    /// Sweeps on a jittered timer for as long as the server runs.
    ///
    /// The jitter keeps a cluster from sweeping in step, which would put every node's delete
    /// on the database at the same moment.
    pub fn keep_sweeping(self) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(self.every + jitter(self.every / 4)).await;
                match self.sweep().await {
                    Ok(0) => {}
                    Ok(swept) => tracing::info!(swept, "swept away what was past its keeping"),
                    Err(error) => tracing::warn!(%error, "could not sweep what was recorded"),
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use ip_config::Seconds;
    use ip_storage::{NewUsage, Usage, UsageRowId};

    use super::*;

    /// A store that writes down the moment it was asked to sweep before.
    #[derive(Default)]
    struct Swept(Mutex<Vec<Timestamp>>);

    #[async_trait::async_trait]
    impl UsageStore for Swept {
        async fn record_usage(&self, _: NewUsage) -> Result<Usage, StorageError> {
            unreachable!("the sweeper records nothing")
        }

        async fn usage(&self, _: UsageRowId) -> Result<Option<Usage>, StorageError> {
            Ok(None)
        }

        async fn sweep_usage(&self, moment: Timestamp) -> Result<u64, StorageError> {
            self.0.lock().unwrap().push(moment);
            Ok(1)
        }

        async fn spent(
            &self,
            _: &ip_storage::UsageFilter,
            _: ip_core::Counted,
            _: Timestamp,
        ) -> Result<u64, StorageError> {
            unreachable!("the sweeper counts nothing")
        }
    }

    fn config(retention: Option<u64>) -> UsageConfig {
        UsageConfig {
            retention: retention.map(Seconds::new),
            sweep_every: Seconds::new(3600),
        }
    }

    #[test]
    fn a_server_that_keeps_every_row_sweeps_nothing() {
        let store = Arc::new(Swept::default());
        assert!(Sweeper::new(store, &config(None)).is_none());
    }

    #[tokio::test]
    async fn a_sweep_removes_everything_older_than_the_retention() {
        let store = Arc::new(Swept::default());
        let sweeper = Sweeper::new(
            Arc::clone(&store) as Arc<dyn UsageStore>,
            &config(Some(86_400)),
        )
        .expect("a retention was configured");
        assert_eq!(sweeper.sweep().await.unwrap(), 1);

        let asked = store.0.lock().unwrap()[0];
        let expected = Timestamp::now().unix_seconds() - 86_400;
        assert!(
            (asked.unix_seconds() - expected).abs() <= 1,
            "swept before {asked} rather than a day ago"
        );
    }
}
