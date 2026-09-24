//! Storage traits and the backends that satisfy them.

#[cfg(any(feature = "standalone-storage", feature = "fast-storage"))]
mod codec;
pub mod error;
#[cfg(feature = "testing")]
pub mod failing;
pub mod list;
pub mod model;
pub mod plugin;
#[cfg(feature = "fast-storage")]
pub mod postgres;
pub mod revision;
pub mod route;
#[cfg(feature = "standalone-storage")]
pub mod sqlite;
pub mod store;
#[cfg(all(test, any(feature = "standalone-storage", feature = "fast-storage")))]
mod suite;
#[cfg(any(feature = "standalone-storage", feature = "fast-storage"))]
pub mod transaction;
pub mod usage;

pub use error::{Entity, StorageError};
#[cfg(feature = "testing")]
pub use failing::FailingStore;
pub use list::{DEFAULT_PAGE_LIMIT, ListStore, Listed, MAX_PAGE_LIMIT, Page};
pub use model::{
    AccountKind, MemberRemoved, Membership, NewTenant, NewUser, Standing, Tenant, TenantRowId,
    TenantWithOwner, User, UserRowId, UserWithMembership,
};
pub use plugin::{
    NewPlugin, Plugin, PluginDisowned, PluginOwner, PluginRecord, PluginRowId, PluginRuleStore,
    PluginStore, PluginUploaded,
};
#[cfg(feature = "fast-storage")]
pub use postgres::PostgresStore;
pub use revision::{RevisionStore, RuleRevision, RuleSet};
pub use route::RouteStore;
#[cfg(feature = "standalone-storage")]
pub use sqlite::SqliteStore;
pub use store::{Backend, GrantStore, MembershipStore, Storage, TenantStore, UserStore};
#[cfg(any(feature = "standalone-storage", feature = "fast-storage"))]
pub use transaction::member_add::{MemberAddBegun, MemberAddTransactional, MemberAddTxn};
#[cfg(any(feature = "standalone-storage", feature = "fast-storage"))]
pub use transaction::member_remove::{
    MemberRemoveBegun, MemberRemoveTransactional, MemberRemoveTxn,
};
#[cfg(any(feature = "standalone-storage", feature = "fast-storage"))]
pub use transaction::plugin_disown::{
    PluginDisownBegun, PluginDisownTransactional, PluginDisownTxn,
};
#[cfg(any(feature = "standalone-storage", feature = "fast-storage"))]
pub use transaction::plugin_upload::{
    PluginUploadBegun, PluginUploadTransactional, PluginUploadTxn,
};
#[cfg(any(feature = "standalone-storage", feature = "fast-storage"))]
pub use transaction::user_create::{UserCreateBegun, UserCreateTransactional, UserCreateTxn};
#[cfg(any(feature = "standalone-storage", feature = "fast-storage"))]
pub use transaction::{IdentityDialect, PluginDialect};
pub use usage::{NewUsage, Usage, UsageFilter, UsageRowId, UsageStore};
