use ip_core::{
    ApiId, Latency, ModelName, Served, TenantId, TokenCount, Tokens, TraceId, TurnId, UserId,
};

use crate::usage::{NewUsage, UsageRowId, UsageStore};

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
