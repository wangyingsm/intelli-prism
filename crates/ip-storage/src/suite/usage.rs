use ip_core::{
    ApiId, Counted, Latency, ModelName, Served, TenantId, TokenCount, Tokens, TraceId, TurnId,
    UserId,
};

use crate::list::Page;
use crate::store::Backend;
use crate::usage::{NewUsage, UsageFilter, UsageRowId, UsageStore};

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
        latency: Latency::from_millis(1_250),
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
async fn spent_by(store: &impl UsageStore, who: &[(&str, &str, &str)]) {
    for (tenant, user, api) in who {
        store
            .record_usage(NewUsage {
                tenant: TenantId::new(tenant).unwrap(),
                user: UserId::new(user).unwrap(),
                api: ApiId::new(api).unwrap(),
                ..spent()
            })
            .await
            .unwrap();
    }
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
    spent_by(store, &[("acme", "alice", "anthropic")]).await;
    let now = ip_core::Timestamp::now();
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
