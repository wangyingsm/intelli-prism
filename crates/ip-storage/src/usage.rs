use async_trait::async_trait;
use ip_core::{
    ApiId, Latency, ModelName, Served, TenantId, Timestamp, Tokens, TraceId, TurnId, UserId,
};

use crate::error::StorageError;

/// Surrogate primary key of a recorded request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UsageRowId(i64);

impl UsageRowId {
    /// Wraps a key the backend assigned.
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    /// The key as the backend stores it.
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// What one request cost, as it is stored.
#[derive(Debug, Clone, PartialEq)]
pub struct Usage {
    /// Primary key the backend assigned.
    pub row_id: UsageRowId,
    /// The trace the request was followed under, which is what the collector holds it by.
    pub trace: TraceId,
    /// The chat turn it belonged to, when the caller marked one.
    pub turn: Option<TurnId>,
    /// Who the request was spent for.
    pub tenant: TenantId,
    /// Which account made it.
    pub user: UserId,
    /// Which api served it.
    pub api: ApiId,
    /// The model that answered, when the answer said which.
    pub model: Option<ModelName>,
    /// What it spent in each direction.
    pub tokens: Tokens,
    /// Whether the upstream or the cache answered.
    pub served: Served,
    /// How long the caller waited.
    pub latency: Latency,
    /// When the request was recorded.
    pub created_at: Timestamp,
}

/// A request about to be recorded.
#[derive(Debug, Clone, PartialEq)]
pub struct NewUsage {
    /// The trace the request was followed under.
    pub trace: TraceId,
    /// The chat turn it belonged to, when the caller marked one.
    pub turn: Option<TurnId>,
    /// Who the request was spent for.
    pub tenant: TenantId,
    /// Which account made it.
    pub user: UserId,
    /// Which api served it.
    pub api: ApiId,
    /// The model that answered, when the answer said which.
    pub model: Option<ModelName>,
    /// What it spent in each direction.
    pub tokens: Tokens,
    /// Whether the upstream or the cache answered.
    pub served: Served,
    /// How long the caller waited.
    pub latency: Latency,
}

/// Which recorded requests a list is narrowed to, each part admitting everything when unset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageFilter {
    /// Only what one tenant spent.
    pub tenant: Option<TenantId>,
    /// Only what one account spent.
    pub user: Option<UserId>,
    /// Only what went to one api.
    pub api: Option<ApiId>,
}

impl UsageFilter {
    /// Everything one tenant spent.
    pub fn of_tenant(tenant: TenantId) -> Self {
        Self {
            tenant: Some(tenant),
            ..Self::default()
        }
    }
}

/// Records what each request cost, and reads one back.
///
/// A row is written for every request, an answer out of the cache included, so a quota counts
/// what was asked as well as what was paid for.
#[async_trait]
pub trait UsageStore: Send + Sync {
    /// Records one request, giving back the row as it was stored.
    async fn record_usage(&self, usage: NewUsage) -> Result<Usage, StorageError>;

    /// Reads one recorded request back.
    async fn usage(&self, row_id: UsageRowId) -> Result<Option<Usage>, StorageError>;

    /// Removes every row recorded before `moment`, reporting how many went.
    async fn sweep_usage(&self, moment: Timestamp) -> Result<u64, StorageError>;
}
