//! What each model costs, as stored.

use async_trait::async_trait;
use ip_core::{ModelName, Price, Timestamp};

use crate::audit::By;
use crate::error::StorageError;

/// What one model costs, as it is stored.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Priced {
    /// The model the price is for.
    pub model: ModelName,
    /// What it charges in each direction, per million tokens.
    pub price: Price,
    /// When the price was set.
    pub created_at: Timestamp,
}

impl crate::audit::Auditable for Priced {
    fn entity(&self) -> crate::error::Entity {
        crate::error::Entity::Price
    }

    fn entity_id(&self) -> String {
        self.model.as_str().to_owned()
    }

    /// A price is the deployment's, so it belongs to no tenant and the row says none.
    fn tenant(&self) -> Option<ip_core::TenantId> {
        None
    }

    fn audited(&self) -> serde_json::Value {
        serde_json::json!({
            "model": self.model.as_str(),
            "input": self.price.input.get(),
            "output": self.price.output.get(),
        })
    }
}

/// A price about to be set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPrice {
    /// The model the price is for.
    pub model: ModelName,
    /// What it charges in each direction, per million tokens.
    pub price: Price,
}

/// Sets and reads what each model costs.
///
/// Every node keeps the whole set in memory and is told to rebuild it by the rule revision,
/// which a change here moves on, so there is no read of this on the request path — which
/// matters more here than for a limit, because this is read while a row is being written
/// rather than while a request is being admitted.
#[async_trait]
pub trait PriceStore: Send + Sync {
    /// Sets a price, replacing whatever that model cost before.
    async fn put_price(&self, price: NewPrice, by: &By) -> Result<Priced, StorageError>;

    /// Removes one model's price, or reports it missing.
    ///
    /// Removing a price does not make the model free: a model nothing prices costs nothing
    /// *known*, and the rows written after this say so rather than saying nought.
    async fn remove_price(&self, model: &ModelName, by: &By) -> Result<(), StorageError>;

    /// Every price set, for building the set a node holds.
    async fn prices(&self) -> Result<Vec<Priced>, StorageError>;
}
