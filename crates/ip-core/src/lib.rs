//! Identifiers, keys and capabilities shared by every layer of the proxy.

pub mod capability;
pub mod error;
pub mod id;
pub mod key;
pub mod passphrase;
pub mod plugin;
pub mod route;
pub mod timestamp;

pub use capability::{Capability, CapabilityScope, Grant, Grants, Role, ScopeKind};
pub use error::CoreError;
pub use id::{ApiId, Nonce, TenantId, UserId};
pub use key::{Signature, TnKey, UtKey};
pub use passphrase::PassphraseHash;
pub use plugin::{Checksum, PluginKind};
pub use route::{
    AbsPath, Endpoint, Host, Port, Protocol, RESERVED_PATH_PREFIX, RouteKey, RouteRule, RouteTarget,
};
pub use timestamp::Timestamp;
