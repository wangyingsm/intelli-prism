//! The stored limits a tenant, a user or an api is held to.

use async_trait::async_trait;
use ip_core::{Allowance, Counted, LimitScope, Period, Timestamp};

use crate::audit::By;
use crate::error::StorageError;

/// What one scope may spend of one thing over one period, as it is stored.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Limit {
    /// Who it applies to.
    pub scope: LimitScope,
    /// What it counts.
    pub counted: Counted,
    /// The stretch of clock it counts over.
    pub period: Period,
    /// How much that stretch allows.
    pub allowance: Allowance,
    /// When the limit was set.
    pub created_at: Timestamp,
}

impl crate::audit::Auditable for Limit {
    fn entity(&self) -> crate::error::Entity {
        crate::error::Entity::Limit
    }

    fn entity_id(&self) -> String {
        format!(
            "{} {} per {}",
            self.scope,
            self.counted.name(),
            self.period.name()
        )
    }

    fn tenant(&self) -> Option<ip_core::TenantId> {
        Some(self.scope.tenant.clone())
    }

    fn audited(&self) -> serde_json::Value {
        serde_json::json!({
            "scope": self.scope.to_string(),
            "counted": self.counted.name(),
            "period": self.period.name(),
            "allowance": self.allowance.get(),
        })
    }
}

/// A limit about to be set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewLimit {
    /// Who it applies to.
    pub scope: LimitScope,
    /// What it counts.
    pub counted: Counted,
    /// The stretch of clock it counts over.
    pub period: Period,
    /// How much that stretch allows.
    pub allowance: Allowance,
}

/// Sets and reads what tenants and their accounts may spend.
///
/// Every node keeps the whole set in memory and is told to rebuild it by the rule revision,
/// which a change here moves on, so there is no read of this on the request path.
#[async_trait]
pub trait LimitStore: Send + Sync {
    /// Sets a limit, replacing whatever the same scope allowed of the same thing over the same
    /// period before.
    async fn put_limit(&self, limit: NewLimit, by: &By) -> Result<Limit, StorageError>;

    /// Removes one limit, or reports it missing.
    async fn remove_limit(
        &self,
        scope: &LimitScope,
        counted: Counted,
        period: Period,
        by: &By,
    ) -> Result<(), StorageError>;

    /// Every limit set, for building the set a node holds.
    async fn limits(&self) -> Result<Vec<Limit>, StorageError>;
}
