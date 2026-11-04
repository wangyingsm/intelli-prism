use ip_core::{Grant, PassphraseHash, TenantId, Timestamp, TnKey, UserId};

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

impl crate::audit::Auditable for Tenant {
    fn entity(&self) -> crate::error::Entity {
        crate::error::Entity::Tenant
    }

    fn entity_id(&self) -> String {
        self.id.to_string()
    }

    /// Its own: what is done to a tenant belongs in the trail that tenant is shown.
    fn tenant(&self) -> Option<TenantId> {
        Some(self.id.clone())
    }

    /// The root key is deliberately absent, for the reason the passphrase verifier is
    /// absent from an account: every `UtKey` under the tenant is derived from it, so a
    /// trail carrying it would hand its reader every key the tenant ever issues.
    fn audited(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id.as_str(),
            "created_at": self.created_at.unix_seconds(),
        })
    }
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

impl crate::audit::Auditable for User {
    fn entity(&self) -> crate::error::Entity {
        crate::error::Entity::User
    }

    fn entity_id(&self) -> String {
        self.id.to_string()
    }

    /// An account is the deployment's: it exists before it is attached to any tenant, and
    /// may be attached to several.
    fn tenant(&self) -> Option<TenantId> {
        None
    }

    /// The passphrase verifier is deliberately absent. It is the one field this trail
    /// could carry that would be worth stealing, and the trail is kept for years and read
    /// by people with no business holding it. A change to it is therefore recorded as a
    /// change with nothing visible in it — which is unambiguous only while the passphrase
    /// is the sole thing about an account that can be replaced.
    fn audited(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id.as_str(),
            "kind": crate::codec::account_kind_name(self.kind),
            "created_at": self.created_at.unix_seconds(),
        })
    }
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

impl crate::audit::Auditable for Membership {
    fn entity(&self) -> crate::error::Entity {
        crate::error::Entity::Membership
    }

    /// Spelled the way an attachment is named everywhere else it is reported missing.
    fn entity_id(&self) -> String {
        format!("{}@{}", self.user, self.tenant)
    }

    /// The tenant it is in: an attachment is a change to that tenant's membership, and the
    /// owner reading its own trail is the reader it is kept for.
    fn tenant(&self) -> Option<TenantId> {
        Some(self.tenant.clone())
    }

    fn audited(&self) -> serde_json::Value {
        serde_json::json!({
            "user": self.user.as_str(),
            "tenant": self.tenant.as_str(),
            "standing": crate::codec::standing_name(self.standing),
        })
    }
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
    /// When the pair was recorded as created.
    pub recorded_at: Timestamp,
}

/// A user and the attachment created with it.
#[derive(Debug, Clone, PartialEq)]
pub struct UserWithMembership {
    /// The account that was created.
    pub user: User,
    /// How it is attached to its tenant.
    pub membership: Membership,
    /// When the pair was recorded as created.
    pub recorded_at: Timestamp,
}

/// An attachment that was removed, and the grants that went with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberRemoved {
    /// The attachment as it was.
    pub membership: Membership,
    /// Every grant the user held inside that tenant, all now revoked.
    pub revoked: Vec<Grant>,
    /// When the removal was recorded.
    pub recorded_at: Timestamp,
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
