//! Transactions whose steps run in one order, fixed at compile time.
//!
//! Each workflow is its own module: the stages it passes through, the transitions between
//! them, and the trait a store opens it with. What the steps write is shared, since a write
//! belongs to the backend rather than to the workflow that runs it.

pub mod member_add;
pub mod member_remove;
pub mod plugin_disown;
pub mod plugin_upload;
pub mod user_create;

use async_trait::async_trait;
use ip_core::{Checksum, Grant, PluginKind, TenantId, Timestamp, UserId};
use sqlx::Database;

use crate::audit::NewAudit;
use crate::error::StorageError;
use crate::model::{Membership, NewTenant, NewUser, Tenant, User};
use crate::plugin::{PluginOwner, PluginRecord};

/// The audit write one backend contributes, in its own dialect.
///
/// A supertrait of every workflow's dialect, so the step that records what a transaction
/// did is available wherever a transaction is.
#[async_trait]
pub trait AuditDialect: Database {
    /// Records a change, saying when it was recorded.
    async fn insert_audit(
        connection: &mut Self::Connection,
        audit: NewAudit,
    ) -> Result<Timestamp, StorageError>;
}

/// The identity writes one backend contributes, in its own dialect.
///
/// Sqlite binds `?` and postgres binds `$1`, so the statements cannot be shared. What is
/// shared is the order they run in, which each workflow fixes at compile time.
#[async_trait]
pub trait IdentityDialect: AuditDialect {
    /// Writes a tenant row.
    async fn insert_tenant(
        connection: &mut Self::Connection,
        new: NewTenant,
    ) -> Result<Tenant, StorageError>;

    /// Writes a user row.
    async fn insert_user(
        connection: &mut Self::Connection,
        new: NewUser,
    ) -> Result<User, StorageError>;

    /// Attaches a user to a tenant, reporting either as missing when it is not there.
    async fn insert_membership(
        connection: &mut Self::Connection,
        membership: Membership,
    ) -> Result<(), StorageError>;

    /// Detaches a user from a tenant, handing back the attachment that was there, or
    /// reporting it missing.
    async fn delete_membership(
        connection: &mut Self::Connection,
        user: &UserId,
        tenant: &TenantId,
    ) -> Result<Membership, StorageError>;

    /// Revokes every grant one user holds inside one tenant, handing back the ones that went.
    async fn delete_grants_in_tenant(
        connection: &mut Self::Connection,
        user: &UserId,
        tenant: &TenantId,
    ) -> Result<Vec<Grant>, StorageError>;
}

/// The plugin writes one backend contributes, in its own dialect.
#[async_trait]
pub trait PluginDialect: AuditDialect {
    /// Stores wasm once under its checksum and hands back what is stored there, refusing wasm
    /// stored already as another kind.
    async fn insert_wasm(
        connection: &mut Self::Connection,
        kind: PluginKind,
        wasm: Vec<u8>,
    ) -> Result<PluginRecord, StorageError>;

    /// Records an owner's hold on a stored plugin, handing back since when it has held it.
    async fn insert_owner(
        connection: &mut Self::Connection,
        plugin: &PluginRecord,
        owner: &PluginOwner,
    ) -> Result<Timestamp, StorageError>;

    /// Ends an owner's hold, refusing while a rule in its chain runs the plugin, and hands
    /// back the plugin it held.
    async fn delete_owner(
        connection: &mut Self::Connection,
        checksum: &Checksum,
        owner: &PluginOwner,
    ) -> Result<PluginRecord, StorageError>;

    /// Removes a plugin's wasm once nobody owns it, reporting whether it went.
    async fn delete_unowned(
        connection: &mut Self::Connection,
        plugin: &PluginRecord,
    ) -> Result<bool, StorageError>;
}
