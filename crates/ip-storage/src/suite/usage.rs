use ip_core::{
    ApiId, Counted, Decider, Difficulty, Hedge, Host, Latency, ModelName, Money, Port, Redacted,
    Replica, Served, TenantId, Timestamp, TokenCount, Tokens, TraceId, TurnId, UserId,
};

use crate::error::{Entity, StorageError};
use crate::list::Page;
use crate::store::Backend;
use crate::usage::{NewUsage, UsageFilter, UsageRowId, UsageStore};

pub(crate) fn far_future() -> Timestamp {
    Timestamp::from_unix_seconds(4_000_000_000).unwrap()
}

/// One request as the gateway would record it, answered by the upstream.
pub(crate) fn spent() -> NewUsage {
    NewUsage {
        trace: TraceId::generate().unwrap(),
        turn: Some(TurnId::new("turn-1").unwrap()),
        tenant: TenantId::new("acme").unwrap(),
        user: UserId::new("alice").unwrap(),
        api: ApiId::new("anthropic").unwrap(),
        model: Some(ModelName::new("claude-opus-5").unwrap()),
        tokens: Tokens::new(TokenCount::new(120), TokenCount::new(30)),
        served: Served::Upstream,
        served_by: Some(Replica {
            host: Host::new("one.example.com").unwrap(),
            port: Port::new(443).unwrap(),
        }),
        raced_across: Hedge::default(),
        dispatched_class: None,
        decided_by: None,
        redacted: Redacted::NONE,
        latency: Latency::from_millis(1_250),
        cost: None,
    }
}

pub(crate) async fn a_recorded_request_round_trips(store: &impl UsageStore) {
    let spent = spent();
    let recorded = store.record_usage(spent.clone()).await.unwrap();
    assert_eq!(recorded.trace, spent.trace);
    assert_eq!(recorded.turn, spent.turn);
    assert_eq!(recorded.tenant, spent.tenant);
    assert_eq!(recorded.user, spent.user);
    assert_eq!(recorded.api, spent.api);
    assert_eq!(recorded.model, spent.model);
    assert_eq!(recorded.tokens, spent.tokens);
    assert_eq!(recorded.served, spent.served);
    assert_eq!(recorded.latency, spent.latency);

    let read = store.usage(recorded.row_id).await.unwrap().unwrap();
    assert_eq!(read, recorded);
}

pub(crate) async fn the_primary_key_is_an_integer_the_backend_assigns(store: &impl UsageStore) {
    let first = store.record_usage(spent()).await.unwrap();
    let second = store.record_usage(spent()).await.unwrap();
    assert!(second.row_id > first.row_id);
}

pub(crate) async fn an_answer_out_of_the_cache_is_recorded_spending_nothing(
    store: &impl UsageStore,
) {
    let hit = NewUsage {
        tokens: Tokens::ZERO,
        served: Served::Cache,
        ..spent()
    };
    let recorded = store.record_usage(hit).await.unwrap();
    let read = store.usage(recorded.row_id).await.unwrap().unwrap();
    assert_eq!(read.served, Served::Cache);
    assert_eq!(read.tokens.total(), 0);
}

/// Which cache answered survives the column, because the two are different promises
/// and an audit that could not tell them apart could not say which answers were
/// approximate.
///
/// Written over every value rather than the new one alone: the names are a mapping
/// kept by hand in two directions, and a round trip that only ever tries one of them
/// is how the other comes to disagree.
pub(crate) async fn where_an_answer_came_from_round_trips(store: &impl UsageStore) {
    for source in [Served::Upstream, Served::Cache, Served::Semantic] {
        let row = NewUsage {
            served: source,
            ..spent()
        };
        let recorded = store.record_usage(row).await.unwrap();
        let read = store.usage(recorded.row_id).await.unwrap().unwrap();
        assert_eq!(read.served, source, "{source:?} did not survive the column");
    }
}

pub(crate) async fn a_request_carrying_no_turn_or_model_round_trips(store: &impl UsageStore) {
    let bare = NewUsage {
        turn: None,
        model: None,
        ..spent()
    };
    let recorded = store.record_usage(bare).await.unwrap();
    let read = store.usage(recorded.row_id).await.unwrap().unwrap();
    assert_eq!(read.turn, None);
    assert_eq!(read.model, None);
}

pub(crate) async fn what_a_tenant_spent_outlives_the_tenant(
    store: &(impl UsageStore + crate::store::TenantStore),
) {
    let tenant = store
        .create_tenant(crate::suite::fixture::new_tenant())
        .await
        .unwrap();
    let recorded = store.record_usage(spent()).await.unwrap();
    store.delete_tenant(&tenant.id).await.unwrap();
    assert_eq!(
        store.usage(recorded.row_id).await.unwrap().unwrap(),
        recorded
    );
}

pub(crate) async fn reading_a_request_nobody_recorded_is_nothing(store: &impl UsageStore) {
    assert_eq!(store.usage(UsageRowId::new(404)).await.unwrap(), None);
}

/// Records one request per name given, each for its own tenant, user and api.
/// The rows are handed back because the store stamps each one's moment itself, and a
/// test about moments can only be exact about one it was told.
async fn spent_by(store: &impl UsageStore, who: &[(&str, &str, &str)]) -> Vec<crate::usage::Usage> {
    let mut recorded = Vec::with_capacity(who.len());
    for (tenant, user, api) in who {
        recorded.push(
            store
                .record_usage(NewUsage {
                    tenant: TenantId::new(tenant).unwrap(),
                    user: UserId::new(user).unwrap(),
                    api: ApiId::new(api).unwrap(),
                    ..spent()
                })
                .await
                .unwrap(),
        );
    }
    recorded
}

/// Every tenant of a listed page, newest first.
fn tenants_of(listed: &[crate::usage::Usage]) -> Vec<String> {
    listed.iter().map(|row| row.tenant.to_string()).collect()
}

pub(crate) async fn what_was_spent_comes_newest_first_a_page_at_a_time(store: &impl Backend) {
    spent_by(
        store,
        &[
            ("acme", "alice", "anthropic"),
            ("acme", "bob", "anthropic"),
            ("other", "carol", "openai"),
        ],
    )
    .await;

    let everything = store
        .list_usage(&UsageFilter::default(), Page::new(20, 0, None))
        .await
        .unwrap();
    assert_eq!(tenants_of(&everything), ["other", "acme", "acme"]);

    let first = store
        .list_usage(&UsageFilter::default(), Page::new(2, 0, None))
        .await
        .unwrap();
    assert_eq!(tenants_of(&first), ["other", "acme"]);
    let last = store
        .list_usage(&UsageFilter::default(), Page::new(2, 2, None))
        .await
        .unwrap();
    assert_eq!(tenants_of(&last), ["acme"]);
    assert!(
        store
            .list_usage(&UsageFilter::default(), Page::new(2, 4, None))
            .await
            .unwrap()
            .is_empty()
    );
}

pub(crate) async fn a_list_is_narrowed_to_what_the_filter_names(store: &impl Backend) {
    spent_by(
        store,
        &[
            ("acme", "alice", "anthropic"),
            ("acme", "bob", "openai"),
            ("other", "carol", "anthropic"),
        ],
    )
    .await;

    let acme = UsageFilter::of_tenant(TenantId::new("acme").unwrap());
    assert_eq!(
        store
            .list_usage(&acme, Page::default())
            .await
            .unwrap()
            .len(),
        2
    );

    let alice = UsageFilter {
        user: Some(UserId::new("alice").unwrap()),
        ..acme.clone()
    };
    let listed = store.list_usage(&alice, Page::default()).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].user, UserId::new("alice").unwrap());

    let openai = UsageFilter {
        api: Some(ApiId::new("openai").unwrap()),
        ..acme
    };
    let listed = store.list_usage(&openai, Page::default()).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].user, UserId::new("bob").unwrap());

    let nobody = UsageFilter::of_tenant(TenantId::new("nobody").unwrap());
    assert!(
        store
            .list_usage(&nobody, Page::default())
            .await
            .unwrap()
            .is_empty()
    );
}

pub(crate) async fn a_moment_to_list_after_keeps_what_came_before_it_out(store: &impl Backend) {
    spent_by(store, &[("acme", "alice", "anthropic")]).await;
    let now = ip_core::Timestamp::now().unix_seconds();
    let long_ago = ip_core::Timestamp::from_unix_seconds(now - 3600).unwrap();
    let later = ip_core::Timestamp::from_unix_seconds(now + 3600).unwrap();

    assert_eq!(
        store
            .list_usage(&UsageFilter::default(), Page::new(20, 0, Some(long_ago)))
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .list_usage(&UsageFilter::default(), Page::new(20, 0, Some(later)))
            .await
            .unwrap()
            .is_empty()
    );
}

pub(crate) async fn a_listed_row_carries_everything_it_was_recorded_with(store: &impl Backend) {
    let recorded = store.record_usage(spent()).await.unwrap();
    let listed = store
        .list_usage(&UsageFilter::default(), Page::default())
        .await
        .unwrap();
    assert_eq!(listed, vec![recorded]);
}

pub(crate) async fn rows_past_their_keeping_are_swept_away(store: &impl Backend) {
    let old = store.record_usage(spent()).await.unwrap();
    let now = ip_core::Timestamp::now().unix_seconds();
    store.record_usage(spent()).await.unwrap();

    let long_ago = ip_core::Timestamp::from_unix_seconds(now - 3600).unwrap();
    assert_eq!(store.sweep_usage(long_ago).await.unwrap(), 0);
    assert_eq!(
        store
            .list_usage(&UsageFilter::default(), Page::default())
            .await
            .unwrap()
            .len(),
        2
    );

    let later = ip_core::Timestamp::from_unix_seconds(now + 1).unwrap();
    assert_eq!(store.sweep_usage(later).await.unwrap(), 2);
    assert!(
        store
            .list_usage(&UsageFilter::default(), Page::default())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.usage(old.row_id).await.unwrap(), None);
}

pub(crate) async fn what_was_spent_is_summed_for_a_scope_since_a_moment(store: &impl Backend) {
    let now = ip_core::Timestamp::now();
    spent_by(
        store,
        &[
            ("acme", "alice", "anthropic"),
            ("acme", "bob", "openai"),
            ("other", "carol", "anthropic"),
        ],
    )
    .await;

    // Every row holds 120 in and 30 out.
    let long_ago = ip_core::Timestamp::from_unix_seconds(now.unix_seconds() - 3600).unwrap();
    let everywhere = UsageFilter::default();
    assert_eq!(
        store
            .spent(&everywhere, Counted::Tokens, long_ago)
            .await
            .unwrap(),
        450
    );
    assert_eq!(
        store
            .spent(&everywhere, Counted::Requests, long_ago)
            .await
            .unwrap(),
        3
    );

    let acme = UsageFilter::of_tenant(TenantId::new("acme").unwrap());
    assert_eq!(
        store.spent(&acme, Counted::Tokens, long_ago).await.unwrap(),
        300
    );
    assert_eq!(
        store
            .spent(&acme, Counted::Requests, long_ago)
            .await
            .unwrap(),
        2
    );

    let alice = UsageFilter {
        user: Some(UserId::new("alice").unwrap()),
        ..acme.clone()
    };
    assert_eq!(
        store
            .spent(&alice, Counted::Tokens, long_ago)
            .await
            .unwrap(),
        150
    );

    let openai = UsageFilter {
        api: Some(ApiId::new("openai").unwrap()),
        ..acme
    };
    assert_eq!(
        store
            .spent(&openai, Counted::Requests, long_ago)
            .await
            .unwrap(),
        1
    );
}

pub(crate) async fn what_was_spent_before_the_moment_is_not_counted(store: &impl Backend) {
    let recorded = spent_by(store, &[("acme", "alice", "anthropic")]).await;
    // The row's own moment, not a clock read beside it. The store stamps `created_at`
    // inside `record_usage` and times are whole seconds, so an insert that finishes
    // either side of a second boundary from a `now()` taken here puts the row one
    // second outside the window it is meant to be inside — which is a failure of the
    // test's arithmetic and not of the store's, and only shows up on a loaded machine.
    let now = recorded[0].created_at;
    let filter = UsageFilter::default();

    // The moment a period begins counts what was spent in it, so the row now is inside.
    assert_eq!(
        store.spent(&filter, Counted::Requests, now).await.unwrap(),
        1
    );
    let later = ip_core::Timestamp::from_unix_seconds(now.unix_seconds() + 1).unwrap();
    assert_eq!(
        store
            .spent(&filter, Counted::Requests, later)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        store.spent(&filter, Counted::Tokens, later).await.unwrap(),
        0
    );
}

pub(crate) async fn nothing_spent_sums_to_nothing(store: &impl Backend) {
    let long_ago = ip_core::Timestamp::from_unix_seconds(0).unwrap();
    let nobody = UsageFilter::of_tenant(TenantId::new("nobody").unwrap());
    assert_eq!(
        store
            .spent(&nobody, Counted::Tokens, long_ago)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .spent(&nobody, Counted::Requests, long_ago)
            .await
            .unwrap(),
        0
    );
}

pub(crate) async fn a_row_says_which_endpoint_behind_the_rule_answered(store: &impl Backend) {
    let recorded = store.record_usage(spent()).await.unwrap();
    let read = store.usage(recorded.row_id).await.unwrap().unwrap();
    assert_eq!(read, recorded);
    let served = read.served_by.expect("an endpoint answered");
    assert_eq!(served.host.as_str(), "one.example.com");
    assert_eq!(served.port.get(), 443);
}

/// One request raced across two endpoints, the count it was raced across round-tripping onto
/// the row and out of the paged list alike.
pub(crate) async fn a_row_says_how_many_endpoints_it_was_raced_across(store: &impl Backend) {
    let raced = NewUsage {
        raced_across: Hedge::new(2).unwrap(),
        ..spent()
    };
    let recorded = store.record_usage(raced).await.unwrap();
    let read = store.usage(recorded.row_id).await.unwrap().unwrap();
    assert_eq!(read.raced_across, Hedge::new(2).unwrap());
    let listed = store
        .list_usage(&UsageFilter::default(), Page::default())
        .await
        .unwrap();
    assert_eq!(listed, vec![recorded]);
}

pub(crate) async fn a_row_the_cache_answered_names_no_endpoint(store: &impl Backend) {
    let hit = NewUsage {
        served: Served::Cache,
        served_by: None,
        tokens: Tokens::ZERO,
        ..spent()
    };
    let recorded = store.record_usage(hit).await.unwrap();
    let read = store.usage(recorded.row_id).await.unwrap().unwrap();
    assert_eq!(read.served_by, None);
    assert_eq!(read.served, Served::Cache);
}

/// One request dispatched at a class it named: the class round-trips onto the row and out
/// of the paged list, and a request that named none reads back with none.
pub(crate) async fn a_row_says_what_class_it_was_dispatched_at(store: &impl Backend) {
    let classed = NewUsage {
        dispatched_class: Some(Difficulty::Hard),
        ..spent()
    };
    let recorded = store.record_usage(classed).await.unwrap();
    let read = store.usage(recorded.row_id).await.unwrap().unwrap();
    assert_eq!(read.dispatched_class, Some(Difficulty::Hard));
    let listed = store
        .list_usage(&UsageFilter::default(), Page::default())
        .await
        .unwrap();
    assert_eq!(listed, vec![recorded]);

    let unclassed = store.record_usage(spent()).await.unwrap();
    assert_eq!(unclassed.dispatched_class, None);
    let read = store.usage(unclassed.row_id).await.unwrap().unwrap();
    assert_eq!(read.dispatched_class, None);
}

/// Each of the four levels round-trips onto a row and out of the paged list, and a row
/// nothing could decide carries neither class nor decider. The tuning an operator does
/// with this column is only as good as every level reading back as the one that ruled.
pub(crate) async fn a_row_says_which_level_decided_its_class(store: &impl Backend) {
    for decider in Decider::ALL {
        let decided = NewUsage {
            dispatched_class: Some(Difficulty::Hard),
            decided_by: Some(decider),
            ..spent()
        };
        let recorded = store.record_usage(decided).await.unwrap();
        let read = store.usage(recorded.row_id).await.unwrap().unwrap();
        assert_eq!(read.decided_by, Some(decider), "{decider} read back wrong");
        let listed = store
            .list_usage(&UsageFilter::default(), Page::default())
            .await
            .unwrap();
        assert_eq!(listed.first(), Some(&recorded), "{decider} listed wrong");
    }

    let undecided = store.record_usage(spent()).await.unwrap();
    assert_eq!(undecided.dispatched_class, None);
    assert_eq!(undecided.decided_by, None);
    let read = store.usage(undecided.row_id).await.unwrap().unwrap();
    assert_eq!(read.decided_by, None);
}

/// The summary counts each level over the window, and the five counts add up to the
/// requests answered — which is the arithmetic an operator tunes a threshold with, and
/// which only holds while every row either names a level or names no class at all.
pub(crate) async fn the_summary_counts_what_each_level_decided(store: &impl UsageStore) {
    for (level, many) in [
        (Some(Decider::Request), 1),
        (Some(Decider::Turn), 4),
        (Some(Decider::Classifier), 3),
        (Some(Decider::Arbiter), 2),
        (None, 5),
    ] {
        for _ in 0..many {
            let row = NewUsage {
                dispatched_class: level.map(|_| Difficulty::Hard),
                decided_by: level,
                ..spent()
            };
            store.record_usage(row).await.unwrap();
        }
    }
    let sums = store
        .sent_upstream(&UsageFilter::default(), None)
        .await
        .unwrap();
    assert_eq!(sums.decided.request, 1);
    assert_eq!(sums.decided.turn, 4);
    assert_eq!(sums.decided.classifier, 3);
    assert_eq!(sums.decided.arbiter, 2);
    assert_eq!(sums.decided.undecided, 5);
    assert_eq!(
        sums.decided.request
            + sums.decided.turn
            + sums.decided.classifier
            + sums.decided.arbiter
            + sums.decided.undecided,
        sums.answered,
        "every answered request is decided by one level or by none"
    );
}

/// A cache hit raises the count of answered requests and leaves the arms paid for
/// alone: the hit carries an arms count of one — what the type says when nothing was
/// raced — and it went nowhere. The sum takes its arms only where `served` is the
/// upstream, which is the one thing a reader is bound to get wrong here.
pub(crate) async fn a_cache_hit_raises_what_was_answered_not_what_was_sent(
    store: &impl UsageStore,
) {
    store.record_usage(spent()).await.unwrap();
    let hit = NewUsage {
        served: Served::Cache,
        served_by: None,
        ..spent()
    };
    store.record_usage(hit).await.unwrap();
    let sums = store
        .sent_upstream(
            &UsageFilter::of_tenant(TenantId::new("acme").unwrap()),
            Some(Timestamp::from_unix_seconds(0).unwrap()),
        )
        .await
        .unwrap();
    assert_eq!(sums.answered, 2);
    assert_eq!(sums.sent, 1);
}

/// Rows that never raced: arms and requests are the same count, one each.
pub(crate) async fn what_was_sent_upstream_counts_one_per_request_when_none_raced(
    store: &impl UsageStore,
) {
    store.record_usage(spent()).await.unwrap();
    store.record_usage(spent()).await.unwrap();
    let sums = store
        .sent_upstream(
            &UsageFilter::of_tenant(TenantId::new("acme").unwrap()),
            Some(Timestamp::from_unix_seconds(0).unwrap()),
        )
        .await
        .unwrap();
    assert_eq!(sums.answered, 2);
    assert_eq!(sums.sent, 2);
}

/// Rows that raced: the arms exceed the requests by exactly the extra arms they went
/// out on.
pub(crate) async fn what_was_sent_upstream_adds_the_arms_of_a_raced_request(
    store: &impl UsageStore,
) {
    store.record_usage(spent()).await.unwrap();
    let raced = NewUsage {
        raced_across: Hedge::new(3).unwrap(),
        ..spent()
    };
    store.record_usage(raced).await.unwrap();
    let sums = store
        .sent_upstream(
            &UsageFilter::of_tenant(TenantId::new("acme").unwrap()),
            Some(Timestamp::from_unix_seconds(0).unwrap()),
        )
        .await
        .unwrap();
    assert_eq!(sums.answered, 2);
    assert_eq!(sums.sent, 4);
}

/// Nothing recorded at all: zero answered, zero sent — nothing to divide and nothing
/// to sum.
pub(crate) async fn what_was_sent_upstream_over_nobody_is_nothing(store: &impl UsageStore) {
    let sums = store
        .sent_upstream(
            &UsageFilter::of_tenant(TenantId::new("acme").unwrap()),
            Some(Timestamp::from_unix_seconds(0).unwrap()),
        )
        .await
        .unwrap();
    assert_eq!(sums.answered, 0);
    assert_eq!(sums.sent, 0);
}

/// The filter narrows both counts the way it narrows a list: what another tenant
/// spent stands outside this tenant's window.
pub(crate) async fn what_was_sent_upstream_narrows_to_the_filter(store: &impl UsageStore) {
    store.record_usage(spent()).await.unwrap();
    let other = NewUsage {
        tenant: TenantId::new("other-tenant").unwrap(),
        ..spent()
    };
    store.record_usage(other).await.unwrap();
    let acme = store
        .sent_upstream(
            &UsageFilter::of_tenant(TenantId::new("acme").unwrap()),
            Some(Timestamp::from_unix_seconds(0).unwrap()),
        )
        .await
        .unwrap();
    assert_eq!((acme.answered, acme.sent), (1, 1));
    let whole = store
        .sent_upstream(
            &UsageFilter::default(),
            Some(Timestamp::from_unix_seconds(0).unwrap()),
        )
        .await
        .unwrap();
    assert_eq!(whole.answered, 2);
}

/// The moment opens the window: nothing beyond it counts, whatever the clock has done
/// while the test ran.
pub(crate) async fn what_was_sent_upstream_after_a_moment_excludes_the_moment(
    store: &impl UsageStore,
) {
    store.record_usage(spent()).await.unwrap();
    let since = store
        .sent_upstream(
            &UsageFilter::of_tenant(TenantId::new("acme").unwrap()),
            Some(far_future()),
        )
        .await
        .unwrap();
    assert_eq!((since.answered, since.sent), (0, 0));
    let all = store
        .sent_upstream(
            &UsageFilter::of_tenant(TenantId::new("acme").unwrap()),
            Some(Timestamp::from_unix_seconds(0).unwrap()),
        )
        .await
        .unwrap();
    assert_eq!((all.answered, all.sent), (1, 1));
}

/// A row carrying what the filter took out, and one carrying that a filter looked and
/// found nothing. Telling the two apart from a row no filter looked at is the whole
/// reason the column is a pair rather than a number.
pub(crate) async fn a_row_says_what_was_taken_out_of_it(store: &impl UsageStore) {
    let taken = NewUsage {
        redacted: Redacted::of(3, "email=2, openai-key=1").unwrap(),
        ..spent()
    };
    let recorded = store.record_usage(taken).await.unwrap();
    assert_eq!(recorded.redacted.count(), Some(3));
    assert_eq!(recorded.redacted.kinds(), Some("email=2, openai-key=1"));
    let read = store.usage(recorded.row_id).await.unwrap().unwrap();
    assert_eq!(read.redacted.count(), Some(3));
    assert_eq!(read.redacted.kinds(), Some("email=2, openai-key=1"));
}

pub(crate) async fn a_row_a_filter_looked_at_and_found_nothing_in_says_so(store: &impl UsageStore) {
    let clean = NewUsage {
        redacted: Redacted::clean(),
        ..spent()
    };
    let read = store
        .usage(store.record_usage(clean).await.unwrap().row_id)
        .await
        .unwrap()
        .unwrap();
    assert!(read.redacted.looked(), "a filter did look at this one");
    assert_eq!(read.redacted.count(), Some(0));
}

pub(crate) async fn a_row_no_filter_looked_at_says_nothing_at_all(store: &impl UsageStore) {
    let read = store
        .usage(store.record_usage(spent()).await.unwrap().row_id)
        .await
        .unwrap()
        .unwrap();
    assert!(!read.redacted.looked());
    assert_eq!(read.redacted.count(), None);
    assert_eq!(read.redacted.kinds(), None);
}

pub(crate) async fn a_row_says_what_it_cost(store: &impl UsageStore) {
    let priced = NewUsage {
        cost: Some(Money::from_micros(810_000)),
        ..spent()
    };
    let recorded = store.record_usage(priced).await.unwrap();
    assert_eq!(recorded.cost, Some(Money::from_micros(810_000)));
    let read = store.usage(recorded.row_id).await.unwrap().unwrap();
    assert_eq!(read.cost, Some(Money::from_micros(810_000)));
}

/// The distinction the whole column rests on: unpriced is not priced at nothing.
pub(crate) async fn a_row_nothing_priced_costs_nothing_known(store: &impl UsageStore) {
    let unpriced = store
        .usage(store.record_usage(spent()).await.unwrap().row_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unpriced.cost, None);

    let free = NewUsage {
        cost: Some(Money::ZERO),
        ..spent()
    };
    let free = store
        .usage(store.record_usage(free).await.unwrap().row_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(free.cost, Some(Money::ZERO));
}

pub(crate) async fn a_cost_past_what_a_column_holds_is_refused(store: &impl UsageStore) {
    let dear = NewUsage {
        cost: Some(Money::from_micros(u64::MAX)),
        ..spent()
    };
    assert!(matches!(
        store.record_usage(dear).await,
        Err(StorageError::Malformed {
            entity: Entity::Usage,
            ..
        })
    ));
}

pub(crate) async fn a_listed_row_carries_what_it_cost(store: &impl Backend) {
    let priced = NewUsage {
        cost: Some(Money::from_micros(42)),
        ..spent()
    };
    store.record_usage(priced).await.unwrap();
    let listed = store
        .list_usage(&UsageFilter::default(), Page::default())
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].cost, Some(Money::from_micros(42)));
}

pub(crate) async fn a_window_sums_what_its_rows_cost(store: &impl UsageStore) {
    for micros in [810, 1_530, 0] {
        let priced = NewUsage {
            cost: Some(Money::from_micros(micros)),
            ..spent()
        };
        store.record_usage(priced).await.unwrap();
    }
    let sums = store
        .sent_upstream(
            &UsageFilter::of_tenant(TenantId::new("acme").unwrap()),
            Some(Timestamp::from_unix_seconds(0).unwrap()),
        )
        .await
        .unwrap();
    assert_eq!(sums.answered, 3);
    assert_eq!(sums.cost, Money::from_micros(2_340));
    assert_eq!(sums.unpriced, 0);
}

/// A window holding a row nothing priced reports the total it can and how many rows are
/// missing from it, rather than reading as a smaller bill.
pub(crate) async fn a_window_says_how_many_of_its_rows_nothing_priced(store: &impl UsageStore) {
    let priced = NewUsage {
        cost: Some(Money::from_micros(810)),
        ..spent()
    };
    store.record_usage(priced).await.unwrap();
    store.record_usage(spent()).await.unwrap();

    let sums = store
        .sent_upstream(
            &UsageFilter::of_tenant(TenantId::new("acme").unwrap()),
            Some(Timestamp::from_unix_seconds(0).unwrap()),
        )
        .await
        .unwrap();
    assert_eq!(sums.answered, 2);
    assert_eq!(sums.cost, Money::from_micros(810));
    assert_eq!(sums.unpriced, 1);
}

pub(crate) async fn a_window_of_nothing_costs_nothing(store: &impl UsageStore) {
    let sums = store
        .sent_upstream(
            &UsageFilter::of_tenant(TenantId::new("acme").unwrap()),
            None,
        )
        .await
        .unwrap();
    assert_eq!(sums.answered, 0);
    assert_eq!(sums.cost, Money::ZERO);
    assert_eq!(sums.unpriced, 0);
}
