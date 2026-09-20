//! Transactions whose steps run in one order, fixed at compile time.
//!
//! Each workflow is its own module: the stages it passes through, the transitions between
//! them, and the trait a store opens it with. What the steps write is shared, since a write
//! belongs to the backend rather than to the workflow that runs it.

pub mod member_add;
pub mod user_create;

use async_trait::async_trait;
use sqlx::Database;

use crate::error::StorageError;
use crate::model::{Membership, NewTenant, NewUser, Tenant, User};

pub(crate) mod sealed {
    pub trait Sealed {}
}

/// The identity writes one backend contributes, in its own dialect.
///
/// Sqlite binds `?` and postgres binds `$1`, so the statements cannot be shared. What is
/// shared is the order they run in, which each workflow fixes at compile time.
#[async_trait]
pub trait IdentityDialect: Database {
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
}
