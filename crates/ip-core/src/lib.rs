//! Identifiers, keys and capabilities shared by every layer of the proxy.

pub mod capability;
pub mod error;
pub mod id;
pub mod key;
pub mod model;
pub mod passphrase;
pub mod plugin;
pub mod route;
pub mod timestamp;
pub mod trace;

pub use capability::{Capability, CapabilityScope, Grant, Grants, Role, ScopeKind};
pub use error::CoreError;
pub use id::{ApiId, Nonce, TenantId, TurnId, UserId};
pub use key::{Signature, TnKey, UtKey};
pub use model::ModelName;
pub use passphrase::PassphraseHash;
pub use plugin::{
    Checksum, NewPluginRule, PRIMARY_ORDER_MAX, PluginKind, PluginOrder, PluginRule, PluginScope,
};
pub use route::{
    AbsPath, Endpoint, Host, Port, Protocol, RESERVED_PATH_PREFIX, RouteKey, RouteRule, RouteTarget,
};
pub use timestamp::Timestamp;
pub use trace::TraceId;
