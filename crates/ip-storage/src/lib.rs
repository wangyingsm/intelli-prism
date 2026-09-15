//! Storage traits and the backends that satisfy them.

pub mod error;
pub mod model;
pub mod plugin;
#[cfg(feature = "fast-storage")]
pub mod postgres;
pub mod route;
#[cfg(feature = "standalone-storage")]
pub mod sqlite;
pub mod store;
#[cfg(all(test, feature = "standalone-storage"))]
mod suite;

pub use error::{Entity, StorageError};
pub use model::{
    AccountKind, Membership, NewTenant, NewUser, Standing, Tenant, TenantRowId, User, UserRowId,
};
pub use plugin::{NewPlugin, Plugin, PluginRecord, PluginRowId, PluginRuleStore, PluginStore};
#[cfg(feature = "fast-storage")]
pub use postgres::PostgresStore;
pub use route::RouteStore;
#[cfg(feature = "standalone-storage")]
pub use sqlite::SqliteStore;
pub use store::{GrantStore, MembershipStore, Storage, TenantStore, UserStore};
