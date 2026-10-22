//! Who changed what, and how an entity says what it is without saying what is secret.

use async_trait::async_trait;
use ip_core::{TenantId, Timestamp, UserId};

use crate::error::{Entity, StorageError};
use crate::list::Page;

/// Who made a change, and what they came through.
///
/// `through` is a literal written in this repository, never anything a caller sends, so
/// the trail cannot be made to say a request arrived somewhere it did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct By {
    /// The account that made the change.
    pub actor: UserId,
    /// The endpoint it came through as the router nests it, or the command that ran it.
    pub through: &'static str,
}

impl By {
    /// This account, acting through this endpoint.
    #[must_use]
    pub fn new(actor: UserId, through: &'static str) -> Self {
        Self { actor, through }
    }
}

/// What an entity looks like in the trail, which is never everything it holds.
///
/// Declared apart from how the entity is stored, and that separation is the whole point:
/// a column added to a table is absent from the trail until somebody puts it here on
/// purpose. It is what keeps a passphrase hash, a tenant key and an upstream credential
/// out of a table meant to be kept for years and read by people who need none of them.
pub trait Auditable {
    /// Which kind of thing this is.
    fn entity(&self) -> Entity;

    /// Which one, spelled as the api names it.
    fn entity_id(&self) -> String;

    /// Whose it is, and none where it belongs to the deployment rather than to a tenant.
    fn tenant(&self) -> Option<TenantId>;

    /// What it says, with nothing secret in it.
    fn audited(&self) -> serde_json::Value;
}

/// One change about to be recorded, beside the change itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAudit {
    /// Who made it, and through what.
    pub by: By,
    /// Which kind of thing changed.
    pub entity: Entity,
    /// Which one.
    pub entity_id: String,
    /// Whose, and none for what belongs to the deployment.
    pub tenant: Option<TenantId>,
    /// What it said before, and nothing where there was nothing.
    pub before: Option<serde_json::Value>,
    /// What it says now, and nothing where it is gone.
    pub after: Option<serde_json::Value>,
}

impl NewAudit {
    /// Something created: no before, and this after.
    #[must_use]
    pub fn created(by: By, after: &impl Auditable) -> Self {
        Self {
            by,
            entity: after.entity(),
            entity_id: after.entity_id(),
            tenant: after.tenant(),
            before: None,
            after: Some(after.audited()),
        }
    }

    /// Something changed, where `before` is what was there and may have been nothing.
    #[must_use]
    pub fn changed(by: By, before: Option<&impl Auditable>, after: &impl Auditable) -> Self {
        Self {
            by,
            entity: after.entity(),
            entity_id: after.entity_id(),
            tenant: after.tenant(),
            before: before.map(Auditable::audited),
            after: Some(after.audited()),
        }
    }

    /// Something removed: this before, and no after.
    #[must_use]
    pub fn removed(by: By, before: &impl Auditable) -> Self {
        Self {
            by,
            entity: before.entity(),
            entity_id: before.entity_id(),
            tenant: before.tenant(),
            before: Some(before.audited()),
            after: None,
        }
    }
}

/// One recorded change, as it is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audited {
    /// The account that made it.
    pub actor: UserId,
    /// The endpoint it came through.
    pub through: String,
    /// Which kind of thing changed.
    pub entity: Entity,
    /// Which one.
    pub entity_id: String,
    /// Whose, and none for what belongs to the deployment.
    pub tenant: Option<TenantId>,
    /// What it said before, and nothing where there was nothing.
    pub before: Option<serde_json::Value>,
    /// What it says now, and nothing where it is gone.
    pub after: Option<serde_json::Value>,
    /// When the change was made.
    pub created_at: Timestamp,
}

/// Which recorded changes a list is narrowed to, each part admitting everything when unset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditFilter {
    /// Only what was done to one tenant's things.
    pub tenant: Option<TenantId>,
    /// Only what one account did.
    pub actor: Option<UserId>,
    /// Only one kind of thing.
    pub entity: Option<Entity>,
}

impl AuditFilter {
    /// Everything done to one tenant's things.
    #[must_use]
    pub fn of_tenant(tenant: TenantId) -> Self {
        Self {
            tenant: Some(tenant),
            ..Self::default()
        }
    }
}

/// Reads what was changed and by whom.
///
/// There is no method here for writing one. A row is written by the same transaction as
/// the change it describes, from inside the store, so that neither can exist without the
/// other — and a public way to write one would be a way to write a row describing a
/// change that never happened.
#[async_trait]
pub trait AuditStore: Send + Sync {
    /// What was changed, newest first.
    async fn audits(&self, filter: &AuditFilter, page: Page) -> Result<Vec<Audited>, StorageError>;
}
