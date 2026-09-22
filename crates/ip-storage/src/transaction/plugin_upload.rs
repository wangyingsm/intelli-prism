//! Storing a plugin's wasm together with the owner that stores it.

use ip_core::{PluginKind, Timestamp};
use typestate_txn::transaction;

use super::PluginDialect;
use crate::error::StorageError;
use crate::plugin::{PluginOwner, PluginRecord, PluginUploaded};

transaction! {
    name: PluginUpload,
    generics: <DB: PluginDialect>,
    carrier: sqlx::Transaction<'static, DB>,
    error: StorageError,
    record: PluginUploaded,
    finish: { carrier.commit().await.map_err(StorageError::backend)? },
    abort: { let _ = carrier.rollback().await; },
    steps: {
        store_wasm(kind: PluginKind, wasm: Vec<u8>) -> plugin: PluginRecord as WasmStored {
            DB::insert_wasm(carrier, kind, wasm).await?
        }
        own(owner: PluginOwner) -> owned_at: Timestamp as Owned {
            DB::insert_owner(carrier, &plugin, &owner).await?
        }
    }
}

/// A store that can open a transaction storing a plugin for one owner.
pub trait PluginUploadTransactional: Send + Sync {
    /// The backend whose dialect the transaction writes in.
    type Db: PluginDialect;

    /// Opens the transaction. Dropping it before committing rolls it back.
    fn begin_plugin_upload(
        &self,
    ) -> impl Future<Output = Result<PluginUploadTxn<Self::Db, PluginUploadBegun>, StorageError>> + Send;
}
