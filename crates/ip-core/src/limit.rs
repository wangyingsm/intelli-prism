//! What a tenant or a user may spend, and over what stretch of the clock.

use std::fmt;

use crate::error::CoreError;
use crate::id::{ApiId, TenantId, UserId};
use crate::timestamp::Timestamp;

/// Seconds in each period, which is also how a period's start is found.
const MINUTE: i64 = 60;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

/// What a limit is attached to: a tenant, narrowed to a user and to an api when either is named.
///
/// A request is matched by every scope it falls inside; the narrowest of them decides, so a
/// tenant's limit can be tightened for one account or one expensive api.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LimitScope {
    /// The tenant the limit belongs to.
    pub tenant: TenantId,
    /// The one account it applies to, or every account in the tenant.
    pub user: Option<UserId>,
    /// The one api it applies to, or every api the tenant reaches.
    pub api: Option<ApiId>,
}

impl LimitScope {
    /// A limit over a whole tenant.
    pub fn of_tenant(tenant: TenantId) -> Self {
        Self {
            tenant,
            user: None,
            api: None,
        }
    }

    /// The same, narrowed to one account.
    pub fn of_user(self, user: UserId) -> Self {
        Self {
            user: Some(user),
            ..self
        }
    }

    /// The same, narrowed to one api.
    pub fn of_api(self, api: ApiId) -> Self {
        Self {
            api: Some(api),
            ..self
        }
    }

    /// Whether a request by `user` to `api` inside `tenant` falls inside this scope.
    pub fn holds(&self, tenant: &TenantId, user: &UserId, api: &ApiId) -> bool {
        &self.tenant == tenant
            && self.user.as_ref().is_none_or(|named| named == user)
            && self.api.as_ref().is_none_or(|named| named == api)
    }

    /// How narrow the scope is: the more it names, the more it outranks a wider one.
    pub fn narrowness(&self) -> u8 {
        u8::from(self.user.is_some()) * 2 + u8::from(self.api.is_some())
    }
}

impl fmt::Display for LimitScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.tenant)?;
        match &self.user {
            Some(user) => write!(f, "/{user}")?,
            None => write!(f, "/*")?,
        }
        match &self.api {
            Some(api) => write!(f, "/{api}"),
            None => write!(f, "/*"),
        }
    }
}

/// What a limit counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Counted {
    /// Tokens spent at an upstream, which an answer from the cache adds nothing to.
    Tokens,
    /// Requests made, an answer from the cache included.
    Requests,
}

impl Counted {
    /// The name this is stored and reported under.
    pub fn name(self) -> &'static str {
        match self {
            Self::Tokens => "tokens",
            Self::Requests => "requests",
        }
    }

    /// Reads it back from that name.
    pub fn named(name: &str) -> Result<Self, CoreError> {
        match name {
            "tokens" => Ok(Self::Tokens),
            "requests" => Ok(Self::Requests),
            other => Err(CoreError::UnknownCounted {
                value: other.to_owned(),
            }),
        }
    }
}

/// The stretch of clock a limit is counted over, fixed rather than rolling.
///
/// A period starts where the clock divides, so it needs no bookkeeping of its own: what is
/// spent under it is found by the moment it began.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    /// One minute, which is where a request rate is usually set.
    Minute,
    /// One hour.
    Hour,
    /// One day, from midnight utc.
    Day,
    /// One calendar month, from the first of it in utc.
    Month,
}

impl Period {
    /// The name this is stored and reported under.
    pub fn name(self) -> &'static str {
        match self {
            Self::Minute => "minute",
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Month => "month",
        }
    }

    /// Reads it back from that name.
    pub fn named(name: &str) -> Result<Self, CoreError> {
        match name {
            "minute" => Ok(Self::Minute),
            "hour" => Ok(Self::Hour),
            "day" => Ok(Self::Day),
            "month" => Ok(Self::Month),
            other => Err(CoreError::UnknownPeriod {
                value: other.to_owned(),
            }),
        }
    }

    /// When the period holding `at` began.
    pub fn began(self, at: Timestamp) -> Timestamp {
        let seconds = at.unix_seconds();
        let began = match self {
            Self::Minute => seconds.div_euclid(MINUTE) * MINUTE,
            Self::Hour => seconds.div_euclid(HOUR) * HOUR,
            Self::Day => seconds.div_euclid(DAY) * DAY,
            Self::Month => return at.month_began(),
        };
        Timestamp::from_unix_seconds(began).unwrap_or(at)
    }

    /// When the period holding `at` ends, which is when the next one begins.
    pub fn ends(self, at: Timestamp) -> Timestamp {
        let began = self.began(at);
        let ends = match self {
            Self::Minute => began.unix_seconds() + MINUTE,
            Self::Hour => began.unix_seconds() + HOUR,
            Self::Day => began.unix_seconds() + DAY,
            Self::Month => return began.next_month(),
        };
        Timestamp::from_unix_seconds(ends).unwrap_or(at)
    }
}

/// How much one limit allows inside its period.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct Allowance(u64);

impl Allowance {
    /// Wraps a count, refusing one that allows nothing, which is a scope nobody may use rather
    /// than a limit.
    pub fn new(allowed: u64) -> Result<Self, CoreError> {
        if allowed == 0 {
            return Err(CoreError::Empty { kind: "allowance" });
        }
        Ok(Self(allowed))
    }

    /// The count it allows.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Whether `spent` is already at or past what is allowed.
    pub const fn is_spent(self, spent: u64) -> bool {
        spent >= self.0
    }
}

impl fmt::Display for Allowance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tenant() -> TenantId {
        TenantId::new("acme").unwrap()
    }

    fn user() -> UserId {
        UserId::new("alice").unwrap()
    }

    fn api() -> ApiId {
        ApiId::new("anthropic").unwrap()
    }

    fn at(seconds: i64) -> Timestamp {
        Timestamp::from_unix_seconds(seconds).unwrap()
    }

    #[test]
    fn a_tenant_scope_holds_every_user_and_api_inside_it() {
        let scope = LimitScope::of_tenant(tenant());
        assert!(scope.holds(&tenant(), &user(), &api()));
        assert!(scope.holds(&tenant(), &UserId::new("bob").unwrap(), &api()));
        assert!(!scope.holds(&TenantId::new("other").unwrap(), &user(), &api()));
    }

    #[test]
    fn a_narrowed_scope_holds_only_what_it_names() {
        let scope = LimitScope::of_tenant(tenant()).of_user(user());
        assert!(scope.holds(&tenant(), &user(), &api()));
        assert!(!scope.holds(&tenant(), &UserId::new("bob").unwrap(), &api()));

        let scope = scope.of_api(api());
        assert!(scope.holds(&tenant(), &user(), &api()));
        assert!(!scope.holds(&tenant(), &user(), &ApiId::new("openai").unwrap()));
    }

    #[test]
    fn the_more_a_scope_names_the_narrower_it_is() {
        let tenant_wide = LimitScope::of_tenant(tenant());
        let by_api = tenant_wide.clone().of_api(api());
        let by_user = tenant_wide.clone().of_user(user());
        let by_both = by_user.clone().of_api(api());
        assert!(tenant_wide.narrowness() < by_api.narrowness());
        assert!(by_api.narrowness() < by_user.narrowness());
        assert!(by_user.narrowness() < by_both.narrowness());
    }

    #[test]
    fn a_scope_reads_as_what_it_names() {
        let scope = LimitScope::of_tenant(tenant());
        assert_eq!(scope.to_string(), "acme/*/*");
        assert_eq!(scope.clone().of_user(user()).to_string(), "acme/alice/*");
        assert_eq!(scope.of_api(api()).to_string(), "acme/*/anthropic");
    }

    #[test]
    fn every_kind_and_period_round_trips_through_its_name() {
        for counted in [Counted::Tokens, Counted::Requests] {
            assert_eq!(Counted::named(counted.name()).unwrap(), counted);
        }
        for period in [Period::Minute, Period::Hour, Period::Day, Period::Month] {
            assert_eq!(Period::named(period.name()).unwrap(), period);
        }
        assert!(Counted::named("dollars").is_err());
        assert!(Period::named("fortnight").is_err());
    }

    #[test]
    fn a_period_begins_where_the_clock_divides() {
        // 2026-09-24T13:47:13Z
        let moment = at(1_790_257_633);
        assert_eq!(Period::Minute.began(moment).unix_seconds() % MINUTE, 0);
        assert_eq!(Period::Hour.began(moment).unix_seconds() % HOUR, 0);
        assert_eq!(Period::Day.began(moment).unix_seconds() % DAY, 0);
        assert_eq!(
            Period::Day.began(moment).to_rfc3339(),
            "2026-09-24T00:00:00+00:00"
        );
        assert_eq!(
            Period::Month.began(moment).to_rfc3339(),
            "2026-09-01T00:00:00+00:00"
        );
    }

    #[test]
    fn a_period_ends_where_the_next_one_begins() {
        let moment = at(1_790_257_633);
        for period in [Period::Minute, Period::Hour, Period::Day, Period::Month] {
            let ends = period.ends(moment);
            assert!(ends > moment, "{} ends before it holds", period.name());
            assert_eq!(
                period.began(ends),
                ends,
                "{} ends off the clock",
                period.name()
            );
        }
        assert_eq!(
            Period::Month.ends(moment).to_rfc3339(),
            "2026-10-01T00:00:00+00:00"
        );
    }

    #[test]
    fn a_period_before_the_epoch_still_begins_before_the_moment_it_holds() {
        let moment = at(-1);
        assert!(Period::Day.began(moment) <= moment);
        assert!(Period::Month.began(moment) <= moment);
    }

    #[test]
    fn an_allowance_of_nothing_is_not_a_limit() {
        assert!(matches!(
            Allowance::new(0),
            Err(CoreError::Empty { kind: "allowance" })
        ));
        let allowance = Allowance::new(100).unwrap();
        assert_eq!(allowance.get(), 100);
        assert_eq!(allowance.to_string(), "100");
    }

    #[test]
    fn what_is_allowed_is_spent_once_it_is_reached() {
        let allowance = Allowance::new(100).unwrap();
        assert!(!allowance.is_spent(99));
        assert!(allowance.is_spent(100));
        assert!(allowance.is_spent(101));
    }
}
