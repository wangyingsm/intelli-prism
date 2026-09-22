//! Ending one owner's hold on a plugin, and removing the wasm once nobody holds it.

use ip_core::Checksum;
use typestate_txn::transaction;

use super::PluginDialect;
use crate::error::StorageError;
use crate::plugin::{PluginDisowned, PluginOwner, PluginRecord};

transaction! {
    name: PluginDisown,
    generics: <DB: PluginDialect>,
    carrier: sqlx::Transaction<'static, DB>,
    error: StorageError,
    record: PluginDisowned,
    finish: { carrier.commit().await.map_err(StorageError::backend)? },
    abort: { let _ = carrier.rollback().await; },
    steps: {
        disown(checksum: Checksum, owner: PluginOwner) -> plugin: PluginRecord as Disowned {
            DB::delete_owner(carrier, &checksum, &owner).await?
        }
        sweep() -> swept: bool as Swept {
            DB::delete_unowned(carrier, &plugin).await?
        }
    }
}

/// A store that can open a transaction ending an owner's hold on a plugin.
pub trait PluginDisownTransactional: Send + Sync {
    /// The backend whose dialect the transaction writes in.
    type Db: PluginDialect;

    /// Opens the transaction. Dropping it before committing rolls it back.
    fn begin_plugin_disown(
        &self,
    ) -> impl Future<Output = Result<PluginDisownTxn<Self::Db, PluginDisownBegun>, StorageError>> + Send;
}
