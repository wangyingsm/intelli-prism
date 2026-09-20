//! Storage traits and the backends that satisfy them.

#[cfg(any(feature = "standalone-storage", feature = "fast-storage"))]
mod codec;
pub mod error;
pub mod model;
pub mod plugin;
#[cfg(feature = "fast-storage")]
pub mod postgres;
pub mod route;
#[cfg(feature = "standalone-storage")]
pub mod sqlite;
pub mod store;
#[cfg(all(test, any(feature = "standalone-storage", feature = "fast-storage")))]
mod suite;
pub mod transaction;

pub use error::{Entity, StorageError};
pub use model::{
    AccountKind, Membership, NewTenant, NewUser, Standing, Tenant, TenantRowId, TenantWithOwner,
    User, UserRowId, UserWithMembership,
};
pub use plugin::{NewPlugin, Plugin, PluginRecord, PluginRowId, PluginRuleStore, PluginStore};
#[cfg(feature = "fast-storage")]
pub use postgres::PostgresStore;
pub use route::RouteStore;
#[cfg(feature = "standalone-storage")]
pub use sqlite::SqliteStore;
pub use store::{Backend, GrantStore, MembershipStore, Storage, TenantStore, UserStore};
pub use transaction::IdentityDialect;
pub use transaction::member_add::{MemberAddTransactional, MemberAddTxn};
pub use transaction::user_create::{UserCreateTransactional, UserCreateTxn};
