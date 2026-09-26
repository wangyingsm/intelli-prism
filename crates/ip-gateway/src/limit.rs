//! What a request is allowed to spend, and what it has spent already.

use std::collections::HashMap;
use std::sync::Arc;

use ip_cache::{CacheKey, CacheLevel, Counters, Ttl};
use ip_core::{Allowance, ApiId, Counted, LimitScope, Period, TenantId, Timestamp, UserId};
use ip_storage::{Limit, UsageFilter, UsageStore};

/// The limits a node holds, matched against each request as it arrives.
///
/// Holding none allows everything, which is what a server nobody has set a limit on does.
#[derive(Debug, Default, Clone)]
pub struct Limits(Arc<[Limit]>);

impl Limits {
    /// Holds what storage returned.
    pub fn new(limits: Vec<Limit>) -> Self {
        Self(limits.into())
    }

    /// How many limits are held.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether none is held, which allows everything.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The one limit that decides for each thing counted over each stretch: of the scopes this
    /// request falls inside, the narrowest.
    pub fn applying_to(&self, asking: &Asking) -> Vec<&Limit> {
        let mut deciding: HashMap<(Counted, Period), &Limit> = HashMap::new();
        for limit in self.0.iter() {
            if !limit.scope.holds(&asking.tenant, &asking.user, &asking.api) {
                continue;
            }
            deciding
                .entry((limit.counted, limit.period))
                .and_modify(|held| {
                    if limit.scope.narrowness() > held.scope.narrowness() {
                        *held = limit;
                    }
                })
                .or_insert(limit);
        }
        deciding.into_values().collect()
    }
}

/// Who is asking, which is what a limit's scope is matched against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asking {
    /// The tenant the request is spent for.
    pub tenant: TenantId,
    /// The account making it.
    pub user: UserId,
    /// The api it is routed to.
    pub api: ApiId,
}

/// What a check decided about one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Nothing counted against this request stands in its way.
    Allowed,
    /// A limit is spent, and the stretch it is spent for ends at this moment.
    Spent {
        /// What ran out.
        counted: Counted,
        /// Over what stretch.
        period: Period,
        /// How much that stretch allowed.
        allowance: Allowance,
        /// When the next stretch begins, which is when the request may be made again.
        ends: Timestamp,
    },
    /// A quota could not be read, so what this request would spend is unknown.
    Unknown {
        /// Why it could not be read.
        detail: String,
    },
}

/// Checks each request against what its scopes allow, and records what its answer spent.
///
/// The counts live in the cache and are seeded from the usage rows, so losing the cache costs
/// a query rather than a fresh allowance.
pub struct Limiter {
    counters: Arc<dyn Counters>,
    usage: Arc<dyn UsageStore>,
}

impl Limiter {
    /// Checks against `counters`, seeding a count that is not there from `usage`.
    pub fn new(counters: Arc<dyn Counters>, usage: Arc<dyn UsageStore>) -> Self {
        Self { counters, usage }
    }

    /// Whether this request may go on, counting it against every request rate that holds it.
    ///
    /// A rate that cannot be read lets the request through: it is there to protect an upstream,
    /// and a cache that cannot answer has never failed a request here. A quota that cannot be
    /// read refuses: it is there to protect a bill, which is worth a refusal.
    pub async fn admit(&self, limits: &Limits, asking: &Asking) -> Decision {
        let now = Timestamp::now();
        for limit in limits.applying_to(asking) {
            let decided = match limit.counted {
                Counted::Requests => self.rate(limit, now).await,
                Counted::Tokens => self.quota(limit, now).await,
            };
            if decided != Decision::Allowed {
                return decided;
            }
        }
        Decision::Allowed
    }

    /// Records what an answer spent against every quota that counts it.
    pub async fn spend(&self, limits: &Limits, asking: &Asking, tokens: u64) {
        if tokens == 0 {
            return;
        }
        let now = Timestamp::now();
        for limit in limits.applying_to(asking) {
            if limit.counted != Counted::Tokens {
                continue;
            }
            let (key, ttl) = counter(limit, now);
            if let Err(error) = self.counters.count(&key, tokens, ttl).await {
                tracing::warn!(%error, %key, "could not count what a request spent");
            }
        }
    }

    /// Counts this request against a rate, refusing it when that takes the rate past what it
    /// allows. A rate that cannot be counted lets the request through.
    async fn rate(&self, limit: &Limit, now: Timestamp) -> Decision {
        let (key, ttl) = counter(limit, now);
        if let Err(error) = self.seeded(limit, now, &key, ttl).await {
            tracing::warn!(%error, %key, "could not seed a request rate; letting it through");
            return Decision::Allowed;
        }
        match self.counters.count(&key, 1, ttl).await {
            Ok(made) if made > limit.allowance.get() => spent(limit, now),
            Ok(_) => Decision::Allowed,
            Err(error) => {
                tracing::warn!(%error, %key, "could not count a request; letting it through");
                Decision::Allowed
            }
        }
    }

    /// Reads what is spent against a quota, refusing the request when it is already at what the
    /// quota allows. A quota that cannot be read refuses rather than spend unmeasured.
    async fn quota(&self, limit: &Limit, now: Timestamp) -> Decision {
        let (key, ttl) = counter(limit, now);
        let read = match self.seeded(limit, now, &key, ttl).await {
            Ok(spent) => spent,
            Err(error) => {
                return Decision::Unknown {
                    detail: error.to_string(),
                };
            }
        };
        if limit.allowance.is_spent(read) {
            return spent(limit, now);
        }
        Decision::Allowed
    }

    /// What is counted under this limit now, seeding it from the rows when the cache holds
    /// nothing for this stretch.
    async fn seeded(
        &self,
        limit: &Limit,
        now: Timestamp,
        key: &CacheKey,
        ttl: Ttl,
    ) -> Result<u64, Unreadable> {
        if let Some(counted) = self
            .counters
            .counted(key)
            .await
            .map_err(Unreadable::cache)?
        {
            return Ok(counted);
        }
        let began = limit.period.began(now);
        let spent = self
            .usage
            .spent(&filter(&limit.scope), limit.counted, began)
            .await
            .map_err(Unreadable::store)?;
        self.counters
            .seed(key, spent, ttl)
            .await
            .map_err(Unreadable::cache)
    }
}

/// Why a count could not be read.
#[derive(Debug, thiserror::Error)]
enum Unreadable {
    /// The cache holding the count could not answer.
    #[error("the counts could not be read: {0}")]
    Cache(String),
    /// The rows a count is seeded from could not be read.
    #[error("what was spent could not be read: {0}")]
    Store(String),
}

impl Unreadable {
    fn cache(error: ip_cache::CacheError) -> Self {
        Self::Cache(error.to_string())
    }

    fn store(error: ip_storage::StorageError) -> Self {
        Self::Store(error.to_string())
    }
}

/// The refusal a spent limit answers with.
fn spent(limit: &Limit, now: Timestamp) -> Decision {
    Decision::Spent {
        counted: limit.counted,
        period: limit.period,
        allowance: limit.allowance,
        ends: limit.period.ends(now),
    }
}

/// Where this limit's count for the stretch holding `now` is kept, and how long it lives.
///
/// The count belongs to the limit's scope rather than to whoever is asking, so a tenant wide
/// limit counts every account together. The stretch's start is in the key, so a new stretch
/// counts from nothing without anything having to clear the old one.
fn counter(limit: &Limit, now: Timestamp) -> (CacheKey, Ttl) {
    let began = limit.period.began(now);
    let id = format!(
        "limit:{}:{}:{}:{}",
        limit.scope,
        limit.counted.name(),
        limit.period.name(),
        began.unix_seconds()
    );
    let key = CacheKey::new(CacheLevel::System, &id)
        .unwrap_or_else(|_| unreachable!("a scope carries nothing a key refuses"));
    let left = limit.period.ends(now).unix_seconds() - now.unix_seconds();
    let ttl = Ttl::seconds(u64::try_from(left).unwrap_or(1).max(1))
        .unwrap_or_else(|_| unreachable!("a stretch always has a moment left"));
    (key, ttl)
}

/// The usage rows a limit's scope covers.
fn filter(scope: &LimitScope) -> UsageFilter {
    UsageFilter {
        tenant: Some(scope.tenant.clone()),
        user: scope.user.clone(),
        api: scope.api.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use ip_cache::CacheError;
    use ip_storage::{NewUsage, Usage, UsageRowId};

    use super::*;

    /// Counts held in memory, which can be told to refuse.
    #[derive(Default)]
    struct Held(Mutex<HashMap<String, u64>>, Mutex<bool>);

    impl Held {
        fn refusing(&self) {
            *self.1.lock().unwrap() = true;
        }

        fn answering(&self) -> Result<(), CacheError> {
            match *self.1.lock().unwrap() {
                true => Err(CacheError::backend(std::io::Error::other("told to refuse"))),
                false => Ok(()),
            }
        }
    }

    #[async_trait::async_trait]
    impl Counters for Held {
        async fn count(&self, key: &CacheKey, by: u64, _: Ttl) -> Result<u64, CacheError> {
            self.answering()?;
            let mut held = self.0.lock().unwrap();
            let total = held.entry(key.as_str().to_owned()).or_insert(0);
            *total = total.saturating_add(by);
            Ok(*total)
        }

        async fn counted(&self, key: &CacheKey) -> Result<Option<u64>, CacheError> {
            self.answering()?;
            Ok(self.0.lock().unwrap().get(key.as_str()).copied())
        }

        async fn seed(&self, key: &CacheKey, from: u64, _: Ttl) -> Result<u64, CacheError> {
            self.answering()?;
            Ok(*self
                .0
                .lock()
                .unwrap()
                .entry(key.as_str().to_owned())
                .or_insert(from))
        }
    }

    /// Rows that answer with whatever the test says was spent.
    struct Rows(u64, bool);

    impl Rows {
        fn holding(spent: u64) -> Arc<Self> {
            Arc::new(Self(spent, false))
        }

        fn refusing() -> Arc<Self> {
            Arc::new(Self(0, true))
        }
    }

    #[async_trait::async_trait]
    impl UsageStore for Rows {
        async fn record_usage(&self, _: NewUsage) -> Result<Usage, ip_storage::StorageError> {
            unreachable!("a limiter records nothing")
        }

        async fn usage(&self, _: UsageRowId) -> Result<Option<Usage>, ip_storage::StorageError> {
            Ok(None)
        }

        async fn sweep_usage(&self, _: Timestamp) -> Result<u64, ip_storage::StorageError> {
            Ok(0)
        }

        async fn spent(
            &self,
            _: &UsageFilter,
            _: Counted,
            _: Timestamp,
        ) -> Result<u64, ip_storage::StorageError> {
            if self.1 {
                return Err(ip_storage::StorageError::backend(std::io::Error::other(
                    "told to refuse",
                )));
            }
            Ok(self.0)
        }
    }

    fn tenant() -> TenantId {
        TenantId::new("acme").unwrap()
    }

    fn user() -> UserId {
        UserId::new("alice").unwrap()
    }

    fn api() -> ApiId {
        ApiId::new("anthropic").unwrap()
    }

    fn asking() -> Asking {
        Asking {
            tenant: tenant(),
            user: user(),
            api: api(),
        }
    }

    fn limit(scope: LimitScope, counted: Counted, period: Period, allowance: u64) -> Limit {
        Limit {
            scope,
            counted,
            period,
            allowance: Allowance::new(allowance).unwrap(),
            created_at: Timestamp::now(),
        }
    }

    fn tenant_wide(counted: Counted, period: Period, allowance: u64) -> Limit {
        limit(LimitScope::of_tenant(tenant()), counted, period, allowance)
    }

    fn limiter(counters: Arc<Held>, rows: Arc<Rows>) -> Limiter {
        Limiter::new(counters, rows)
    }

    #[tokio::test]
    async fn a_server_with_no_limits_allows_everything() {
        let limiter = limiter(Arc::new(Held::default()), Rows::holding(0));
        assert_eq!(
            limiter.admit(&Limits::default(), &asking()).await,
            Decision::Allowed
        );
    }

    #[tokio::test]
    async fn a_rate_allows_what_it_says_and_refuses_the_next() {
        let counters = Arc::new(Held::default());
        let limiter = limiter(Arc::clone(&counters), Rows::holding(0));
        let limits = Limits::new(vec![tenant_wide(Counted::Requests, Period::Minute, 2)]);

        assert_eq!(limiter.admit(&limits, &asking()).await, Decision::Allowed);
        assert_eq!(limiter.admit(&limits, &asking()).await, Decision::Allowed);
        assert!(matches!(
            limiter.admit(&limits, &asking()).await,
            Decision::Spent {
                counted: Counted::Requests,
                period: Period::Minute,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_quota_refuses_once_what_is_spent_reaches_it() {
        let counters = Arc::new(Held::default());
        let limiter = limiter(Arc::clone(&counters), Rows::holding(0));
        let limits = Limits::new(vec![tenant_wide(Counted::Tokens, Period::Month, 100)]);

        assert_eq!(limiter.admit(&limits, &asking()).await, Decision::Allowed);
        limiter.spend(&limits, &asking(), 99).await;
        assert_eq!(limiter.admit(&limits, &asking()).await, Decision::Allowed);
        limiter.spend(&limits, &asking(), 1).await;
        assert!(matches!(
            limiter.admit(&limits, &asking()).await,
            Decision::Spent {
                counted: Counted::Tokens,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_refusal_says_when_the_stretch_it_is_spent_for_ends() {
        let limiter = limiter(Arc::new(Held::default()), Rows::holding(1_000));
        let limits = Limits::new(vec![tenant_wide(Counted::Tokens, Period::Day, 10)]);
        let Decision::Spent {
            ends, allowance, ..
        } = limiter.admit(&limits, &asking()).await
        else {
            panic!("a spent quota allowed a request");
        };
        assert_eq!(allowance.get(), 10);
        assert!(ends > Timestamp::now());
        assert_eq!(Period::Day.began(ends), ends);
    }

    #[tokio::test]
    async fn a_count_that_is_not_there_is_seeded_from_the_rows() {
        let counters = Arc::new(Held::default());
        let limiter = limiter(Arc::clone(&counters), Rows::holding(80));
        let limits = Limits::new(vec![tenant_wide(Counted::Tokens, Period::Month, 100)]);

        assert_eq!(limiter.admit(&limits, &asking()).await, Decision::Allowed);
        limiter.spend(&limits, &asking(), 20).await;
        assert!(matches!(
            limiter.admit(&limits, &asking()).await,
            Decision::Spent { .. }
        ));
    }

    #[tokio::test]
    async fn the_narrowest_scope_a_request_falls_inside_decides() {
        let counters = Arc::new(Held::default());
        let limiter = limiter(Arc::clone(&counters), Rows::holding(0));
        let limits = Limits::new(vec![
            tenant_wide(Counted::Requests, Period::Minute, 1_000),
            limit(
                LimitScope::of_tenant(tenant()).of_user(user()),
                Counted::Requests,
                Period::Minute,
                1,
            ),
        ]);

        assert_eq!(limiter.admit(&limits, &asking()).await, Decision::Allowed);
        assert!(matches!(
            limiter.admit(&limits, &asking()).await,
            Decision::Spent { .. }
        ));
    }

    #[tokio::test]
    async fn a_limit_on_another_scope_is_not_this_request_s() {
        let limiter = limiter(Arc::new(Held::default()), Rows::holding(1_000));
        let elsewhere = LimitScope::of_tenant(TenantId::new("other").unwrap());
        let limits = Limits::new(vec![limit(elsewhere, Counted::Tokens, Period::Month, 1)]);
        assert_eq!(limiter.admit(&limits, &asking()).await, Decision::Allowed);
    }

    #[tokio::test]
    async fn every_kind_and_stretch_is_checked() {
        let counters = Arc::new(Held::default());
        let limiter = limiter(Arc::clone(&counters), Rows::holding(0));
        let limits = Limits::new(vec![
            tenant_wide(Counted::Requests, Period::Minute, 100),
            tenant_wide(Counted::Requests, Period::Day, 100),
            tenant_wide(Counted::Tokens, Period::Month, 100),
        ]);

        assert_eq!(limiter.admit(&limits, &asking()).await, Decision::Allowed);
        assert_eq!(counters.0.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_rate_that_cannot_be_counted_lets_the_request_through() {
        let counters = Arc::new(Held::default());
        counters.refusing();
        let limiter = limiter(Arc::clone(&counters), Rows::holding(0));
        let limits = Limits::new(vec![tenant_wide(Counted::Requests, Period::Minute, 1)]);
        assert_eq!(limiter.admit(&limits, &asking()).await, Decision::Allowed);
    }

    #[tokio::test]
    async fn a_quota_that_cannot_be_read_refuses_the_request() {
        let counters = Arc::new(Held::default());
        counters.refusing();
        let limiter = limiter(Arc::clone(&counters), Rows::holding(0));
        let limits = Limits::new(vec![tenant_wide(Counted::Tokens, Period::Month, 100)]);
        assert!(matches!(
            limiter.admit(&limits, &asking()).await,
            Decision::Unknown { .. }
        ));
    }

    #[tokio::test]
    async fn a_quota_whose_rows_cannot_be_read_refuses_the_request() {
        let limiter = limiter(Arc::new(Held::default()), Rows::refusing());
        let limits = Limits::new(vec![tenant_wide(Counted::Tokens, Period::Month, 100)]);
        assert!(matches!(
            limiter.admit(&limits, &asking()).await,
            Decision::Unknown { .. }
        ));
    }

    #[tokio::test]
    async fn what_an_answer_spent_counts_against_a_quota_alone() {
        let counters = Arc::new(Held::default());
        let limiter = limiter(Arc::clone(&counters), Rows::holding(0));
        let limits = Limits::new(vec![
            tenant_wide(Counted::Tokens, Period::Month, 1_000),
            tenant_wide(Counted::Requests, Period::Minute, 1_000),
        ]);

        limiter.spend(&limits, &asking(), 150).await;
        let held = counters.0.lock().unwrap();
        let counted: Vec<_> = held
            .iter()
            .map(|(key, total)| (key.clone(), *total))
            .collect();
        assert_eq!(counted.len(), 1, "counted {counted:?}");
        assert!(counted[0].0.contains("tokens"));
        assert_eq!(counted[0].1, 150);
    }

    #[tokio::test]
    async fn an_answer_that_spent_nothing_counts_nothing() {
        let counters = Arc::new(Held::default());
        let limiter = limiter(Arc::clone(&counters), Rows::holding(0));
        let limits = Limits::new(vec![tenant_wide(Counted::Tokens, Period::Month, 1_000)]);
        limiter.spend(&limits, &asking(), 0).await;
        assert!(counters.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_count_belongs_to_the_scope_rather_than_to_whoever_asks() {
        let counters = Arc::new(Held::default());
        let limiter = limiter(Arc::clone(&counters), Rows::holding(0));
        let limits = Limits::new(vec![tenant_wide(Counted::Requests, Period::Minute, 2)]);

        limiter.admit(&limits, &asking()).await;
        let bob = Asking {
            user: UserId::new("bob").unwrap(),
            ..asking()
        };
        assert_eq!(limiter.admit(&limits, &bob).await, Decision::Allowed);
        assert!(matches!(
            limiter.admit(&limits, &bob).await,
            Decision::Spent { .. }
        ));
    }

    #[test]
    fn the_key_a_count_is_held_under_names_the_stretch_it_counts() {
        let limit = tenant_wide(Counted::Tokens, Period::Month, 100);
        let now = Timestamp::from_unix_seconds(1_790_257_633).unwrap();
        let (key, ttl) = counter(&limit, now);
        assert!(key.as_str().contains("limit:acme/*/*:tokens:month:"));
        assert!(
            key.as_str()
                .ends_with(&Period::Month.began(now).unix_seconds().to_string())
        );
        assert!(ttl.get().as_secs() > 0);
    }

    #[test]
    fn holding_no_limit_holds_nothing_to_apply() {
        let limits = Limits::default();
        assert!(limits.is_empty());
        assert_eq!(limits.len(), 0);
        assert!(limits.applying_to(&asking()).is_empty());
    }
}
