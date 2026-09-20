use ip_core::{PassphraseHash, TenantId, Timestamp, TnKey, UserId};

/// Surrogate primary key of a stored tenant row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TenantRowId(i64);

impl TenantRowId {
    /// Wraps a key the backend assigned.
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    /// The key as the backend stores it.
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// Surrogate primary key of a stored user row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UserRowId(i64);

impl UserRowId {
    /// Wraps a key the backend assigned.
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    /// The key as the backend stores it.
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// A tenant as it is stored.
#[derive(Debug, Clone, PartialEq)]
pub struct Tenant {
    /// Primary key every other row references it by.
    pub row_id: TenantRowId,
    /// Identifies the tenant across the application.
    pub id: TenantId,
    /// Root secret every `UtKey` under this tenant is derived from.
    pub key: TnKey,
    /// When the tenant was created.
    pub created_at: Timestamp,
}

/// A tenant about to be created.
#[derive(Debug, Clone, PartialEq)]
pub struct NewTenant {
    /// Identifies the tenant across the application.
    pub id: TenantId,
    /// Root secret, drawn by the caller so it is never derived from stored data.
    pub key: TnKey,
}

/// A user as it is stored.
#[derive(Debug, Clone, PartialEq)]
pub struct User {
    /// Primary key every other row references it by.
    pub row_id: UserRowId,
    /// Identifies the user across the application.
    pub id: UserId,
    /// Verifier for the user's passphrase.
    pub passphrase: PassphraseHash,
    /// Whether the account is the system administrator.
    pub kind: AccountKind,
    /// When the user was created.
    pub created_at: Timestamp,
}

/// A user about to be created.
#[derive(Debug, Clone, PartialEq)]
pub struct NewUser {
    /// Identifies the user across the application.
    pub id: UserId,
    /// Verifier for the user's passphrase.
    pub passphrase: PassphraseHash,
    /// Whether the account is the system administrator.
    pub kind: AccountKind,
}

/// Whether an account is the system administrator or an ordinary user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccountKind {
    /// Holds every capability, and may only call the api from localhost.
    SystemAdministrator,
    /// Holds nothing until a grant says otherwise.
    #[default]
    Regular,
}

/// One user's attachment to one tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    /// The attached user.
    pub user: UserId,
    /// The tenant it is attached to.
    pub tenant: TenantId,
    /// What the user is within that tenant.
    pub standing: Standing,
}

/// A tenant, the owner account created with it, and their attachment.
#[derive(Debug, Clone, PartialEq)]
pub struct TenantWithOwner {
    /// The tenant that was created.
    pub tenant: Tenant,
    /// The account that represents it.
    pub owner: User,
    /// How the owner is attached to it.
    pub membership: Membership,
}

/// What a user is within one tenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Standing {
    /// The tenant owner, which represents the tenant for management.
    Owner,
    /// An ordinary member.
    #[default]
    Member,
}
