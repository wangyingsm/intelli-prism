use async_trait::async_trait;
use ip_core::{Checksum, PluginKind, Timestamp};

use crate::error::StorageError;

/// Surrogate primary key of a stored plugin row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PluginRowId(i64);

impl PluginRowId {
    /// Wraps a key the backend assigned.
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    /// The key as the backend stores it.
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// What is known about a stored plugin without reading its wasm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRecord {
    /// Primary key every other row references it by.
    pub row_id: PluginRowId,
    /// The sha256 of the wasm, which is how the plugin is named.
    pub checksum: Checksum,
    /// Where in the dataflow it runs.
    pub kind: PluginKind,
    /// How large the wasm is, in bytes.
    pub size: usize,
    /// When it was first stored.
    pub created_at: Timestamp,
}

/// A stored plugin together with its wasm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plugin {
    /// What is known about it without the wasm.
    pub record: PluginRecord,
    /// The module itself.
    pub wasm: Vec<u8>,
}

/// Wasm about to be stored, which is named by its own checksum once written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPlugin {
    /// Where in the dataflow it runs.
    pub kind: PluginKind,
    /// The module itself.
    pub wasm: Vec<u8>,
}

/// Holds the wasm of every plugin, addressed by the checksum of its own bytes.
#[async_trait]
pub trait PluginStore: Send + Sync {
    /// Stores wasm under the checksum of its bytes. Storing the same wasm twice
    /// changes nothing, because the name is derived from the content.
    async fn put_plugin(&self, new: NewPlugin) -> Result<PluginRecord, StorageError>;

    /// Reads one plugin, wasm included.
    async fn plugin(&self, checksum: &Checksum) -> Result<Option<Plugin>, StorageError>;

    /// What is stored, without reading any wasm.
    async fn plugins(&self) -> Result<Vec<PluginRecord>, StorageError>;

    /// Removes a plugin, or reports it missing.
    async fn remove_plugin(&self, checksum: &Checksum) -> Result<(), StorageError>;
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::error::Entity;

    #[derive(Default)]
    struct MemoryPlugins {
        held: Mutex<Vec<Plugin>>,
        next_row_id: Mutex<i64>,
    }

    #[async_trait]
    impl PluginStore for MemoryPlugins {
        async fn put_plugin(&self, new: NewPlugin) -> Result<PluginRecord, StorageError> {
            let checksum = Checksum::of(&new.wasm);
            let mut held = self.held.lock().unwrap();
            if let Some(plugin) = held
                .iter()
                .find(|plugin| plugin.record.checksum == checksum)
            {
                return Ok(plugin.record.clone());
            }
            let mut next = self.next_row_id.lock().unwrap();
            *next += 1;
            let record = PluginRecord {
                row_id: PluginRowId::new(*next),
                checksum,
                kind: new.kind,
                size: new.wasm.len(),
                created_at: Timestamp::now(),
            };
            held.push(Plugin {
                record: record.clone(),
                wasm: new.wasm,
            });
            Ok(record)
        }

        async fn plugin(&self, checksum: &Checksum) -> Result<Option<Plugin>, StorageError> {
            Ok(self
                .held
                .lock()
                .unwrap()
                .iter()
                .find(|plugin| &plugin.record.checksum == checksum)
                .cloned())
        }

        async fn plugins(&self) -> Result<Vec<PluginRecord>, StorageError> {
            Ok(self
                .held
                .lock()
                .unwrap()
                .iter()
                .map(|plugin| plugin.record.clone())
                .collect())
        }

        async fn remove_plugin(&self, checksum: &Checksum) -> Result<(), StorageError> {
            let mut held = self.held.lock().unwrap();
            let before = held.len();
            held.retain(|plugin| &plugin.record.checksum != checksum);
            if held.len() == before {
                return Err(StorageError::NotFound {
                    entity: Entity::Plugin,
                    id: checksum.to_string(),
                });
            }
            Ok(())
        }
    }

    fn store() -> Arc<dyn PluginStore> {
        Arc::new(MemoryPlugins::default())
    }

    fn wasm(body: &[u8]) -> NewPlugin {
        NewPlugin {
            kind: PluginKind::ReqBody,
            wasm: body.to_vec(),
        }
    }

    #[tokio::test]
    async fn the_surface_is_reachable_through_one_trait_object() {
        let store = store();
        let record = store.put_plugin(wasm(b"\0asm\x01\0\0\0")).await.unwrap();
        let plugin = store.plugin(&record.checksum).await.unwrap().unwrap();
        assert_eq!(plugin.wasm, b"\0asm\x01\0\0\0");
        assert_eq!(store.plugins().await.unwrap(), vec![record.clone()]);
        store.remove_plugin(&record.checksum).await.unwrap();
        assert_eq!(store.plugin(&record.checksum).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_plugin_is_named_by_the_checksum_of_its_own_bytes() {
        let store = store();
        let record = store.put_plugin(wasm(b"module")).await.unwrap();
        assert_eq!(record.checksum, Checksum::of(b"module"));
        assert!(record.checksum.matches(b"module"));
        assert_eq!(record.size, 6);
    }

    #[tokio::test]
    async fn storing_the_same_wasm_twice_changes_nothing() {
        let store = store();
        let first = store.put_plugin(wasm(b"module")).await.unwrap();
        let second = store.put_plugin(wasm(b"module")).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(store.plugins().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn different_wasm_is_a_different_plugin() {
        let store = store();
        store.put_plugin(wasm(b"one")).await.unwrap();
        store.put_plugin(wasm(b"two")).await.unwrap();
        assert_eq!(store.plugins().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn listing_does_not_carry_the_wasm() {
        let store = store();
        let record = store.put_plugin(wasm(b"module")).await.unwrap();
        let listed = store.plugins().await.unwrap();
        assert_eq!(listed, vec![record]);
        assert_eq!(listed[0].size, 6);
    }

    #[tokio::test]
    async fn removing_what_is_absent_reports_it_missing() {
        let store = store();
        assert!(matches!(
            store.remove_plugin(&Checksum::of(b"absent")).await,
            Err(StorageError::NotFound {
                entity: Entity::Plugin,
                ..
            })
        ));
    }
}
