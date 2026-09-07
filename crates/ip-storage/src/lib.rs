//! Storage traits and the backends that satisfy them.

pub mod error;
pub mod model;
pub mod store;

pub use error::{Entity, StorageError};
pub use model::{
    AccountKind, Membership, NewTenant, NewUser, Standing, Tenant, TenantRowId, User, UserRowId,
};
pub use store::{GrantStore, MembershipStore, Storage, TenantStore, UserStore};
