use async_trait::async_trait;
use ip_core::{
    ApiId, Counted, Decider, Difficulty, Hedge, Latency, ModelName, Money, Redacted, Replica,
    Served, TenantId, Timestamp, Tokens, TraceId, TurnId, UserId,
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
    /// Which endpoint behind the rule answered, when one did.
    pub served_by: Option<Replica>,
    /// How many endpoints behind the rule it was raced across, what actually happened; one is
    /// what it was before anything raced.
    pub raced_across: Hedge,
    /// The difficulty class it was dispatched at, and none when nothing named one.
    pub dispatched_class: Option<Difficulty>,
    /// Which level of the margin named that class, and none where there is no class.
    pub decided_by: Option<Decider>,
    /// What the filter took out of the request and its answer, and nothing at all
    /// where no filter looked.
    pub redacted: Redacted,
    /// How long the caller waited.
    pub latency: Latency,
    /// What it cost at the prices in force, and nothing at all where nothing priced the
    /// model, which is not a cost of nothing.
    pub cost: Option<Money>,
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
    /// Which endpoint behind the rule answered, when one did.
    pub served_by: Option<Replica>,
    /// How many endpoints behind the rule it was raced across, what actually happened; one is
    /// what it was before anything raced.
    pub raced_across: Hedge,
    /// The difficulty class it was dispatched at, and none when nothing named one.
    pub dispatched_class: Option<Difficulty>,
    /// Which level of the margin named that class, and none where there is no class.
    pub decided_by: Option<Decider>,
    /// What the filter took out of the request and its answer, and nothing at all
    /// where no filter looked.
    pub redacted: Redacted,
    /// How long the caller waited.
    pub latency: Latency,
    /// What it cost at the prices in force, and nothing at all where nothing priced the
    /// model, which is not a cost of nothing.
    pub cost: Option<Money>,
}

/// Requests answered and requests paid for, in one snapshot of the rows a filter picks.
///
/// `sent` is the one number that cannot be got by adding the columns up: a row served
/// out of the cache still carries the arms count its type says when nothing was raced,
/// which is one, and one cached answer is not one request the upstream was billed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpentArms {
    /// Requests answered — cache included, which is every row.
    pub answered: u64,
    /// Requests that actually went upstream: the arms of the rows `served` says were
    /// answered there.
    pub sent: u64,
    /// How many of them each level of the margin decided.
    pub decided: Decided,
}

/// How many of a window's requests each level of the margin decided, and how many it
/// could not.
///
/// The five add up to the requests answered, because every row either names a level or
/// names no class at all. They are counts and nothing else: what fraction of traffic the
/// local model carried, and what the arbiter cost, are subtractions the caller makes in
/// whichever frame fits the question.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Decided {
    /// Requests that named their own class.
    pub request: u64,
    /// Requests dispatched at the class their turn remembered.
    pub turn: u64,
    /// Openings the local classifier judged, which is its real call rate.
    pub classifier: u64,
    /// Openings the arbiter was paid to rule on.
    pub arbiter: u64,
    /// Requests nothing could decide, dispatched unclassified.
    pub undecided: u64,
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

    /// What the rows matching `filter` hold of `counted` since `moment`, the moment itself
    /// included, which is what a lost count is seeded from.
    async fn spent(
        &self,
        filter: &UsageFilter,
        counted: Counted,
        moment: Timestamp,
    ) -> Result<u64, StorageError>;

    /// How many requests the rows matching `filter` answered since `moment`, the moment
    /// itself included, and how many arms those requests went out on — one query, so the
    /// two numbers describe one snapshot rather than a window two calls could straddle.
    /// None opens the window at the beginning, as an absent `after` does for a list.
    ///
    /// `sent` sums the arms only over rows `served` says the upstream answered: a cache
    /// hit writes an arms count of one without sending anything, and counting it would
    /// inflate the very bill this reports.
    async fn sent_upstream(
        &self,
        filter: &UsageFilter,
        moment: Option<Timestamp>,
    ) -> Result<SpentArms, StorageError>;
}
