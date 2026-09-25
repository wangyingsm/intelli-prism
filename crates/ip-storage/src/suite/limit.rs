use ip_core::{Allowance, ApiId, Counted, LimitScope, Period, TenantId, UserId};

use crate::error::{Entity, StorageError};
use crate::limit::NewLimit;
use crate::store::Backend;
use crate::suite::fixture::{new_user, tenant_id, tenant_with_user, user_id};

/// A limit over a whole tenant: a million tokens a month.
fn monthly(allowance: u64) -> NewLimit {
    NewLimit {
        scope: LimitScope::of_tenant(tenant_id()),
        counted: Counted::Tokens,
        period: Period::Month,
        allowance: Allowance::new(allowance).unwrap(),
    }
}

pub(crate) async fn a_limit_round_trips(store: &impl Backend) {
    tenant_with_user(store).await;
    let set = store.put_limit(monthly(1_000_000)).await.unwrap();
    assert_eq!(set.scope, LimitScope::of_tenant(tenant_id()));
    assert_eq!(set.counted, Counted::Tokens);
    assert_eq!(set.period, Period::Month);
    assert_eq!(set.allowance.get(), 1_000_000);

    let held = store.limits().await.unwrap();
    assert_eq!(held, vec![set]);
}

pub(crate) async fn every_scope_shape_round_trips(store: &impl Backend) {
    tenant_with_user(store).await;
    let tenant_wide = LimitScope::of_tenant(tenant_id());
    let by_user = tenant_wide.clone().of_user(user_id());
    let by_api = tenant_wide.clone().of_api(ApiId::new("anthropic").unwrap());
    let by_both = by_user.clone().of_api(ApiId::new("anthropic").unwrap());

    for (at, scope) in [&tenant_wide, &by_user, &by_api, &by_both]
        .into_iter()
        .enumerate()
    {
        store
            .put_limit(NewLimit {
                scope: scope.clone(),
                allowance: Allowance::new(at as u64 + 1).unwrap(),
                ..monthly(1)
            })
            .await
            .unwrap();
    }

    let held = store.limits().await.unwrap();
    assert_eq!(held.len(), 4);
    for scope in [tenant_wide, by_user, by_api, by_both] {
        assert!(
            held.iter().any(|limit| limit.scope == scope),
            "{scope} was not held"
        );
    }
}

pub(crate) async fn setting_the_same_limit_again_replaces_what_it_allows(store: &impl Backend) {
    tenant_with_user(store).await;
    store.put_limit(monthly(1_000_000)).await.unwrap();
    store.put_limit(monthly(2_000_000)).await.unwrap();

    let held = store.limits().await.unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].allowance.get(), 2_000_000);
}

pub(crate) async fn one_scope_holds_a_limit_per_thing_counted_and_period(store: &impl Backend) {
    tenant_with_user(store).await;
    for (counted, period) in [
        (Counted::Tokens, Period::Month),
        (Counted::Tokens, Period::Day),
        (Counted::Requests, Period::Minute),
        (Counted::Requests, Period::Hour),
    ] {
        store
            .put_limit(NewLimit {
                counted,
                period,
                ..monthly(10)
            })
            .await
            .unwrap();
    }
    assert_eq!(store.limits().await.unwrap().len(), 4);
}

pub(crate) async fn removing_a_limit_leaves_the_others(store: &impl Backend) {
    tenant_with_user(store).await;
    let scope = LimitScope::of_tenant(tenant_id());
    store.put_limit(monthly(10)).await.unwrap();
    store
        .put_limit(NewLimit {
            period: Period::Day,
            ..monthly(5)
        })
        .await
        .unwrap();

    store
        .remove_limit(&scope, Counted::Tokens, Period::Month)
        .await
        .unwrap();
    let held = store.limits().await.unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].period, Period::Day);
}

pub(crate) async fn removing_a_limit_that_is_absent_reports_it_missing(store: &impl Backend) {
    tenant_with_user(store).await;
    assert!(matches!(
        store
            .remove_limit(
                &LimitScope::of_tenant(tenant_id()),
                Counted::Tokens,
                Period::Month
            )
            .await,
        Err(StorageError::NotFound {
            entity: Entity::Limit,
            ..
        })
    ));
}

pub(crate) async fn a_limit_on_a_tenant_that_is_not_there_is_refused(store: &impl Backend) {
    let nowhere = LimitScope::of_tenant(TenantId::new("nowhere").unwrap());
    assert!(matches!(
        store
            .put_limit(NewLimit {
                scope: nowhere,
                ..monthly(10)
            })
            .await,
        Err(StorageError::NotFound {
            entity: Entity::Tenant,
            ..
        })
    ));
}

pub(crate) async fn a_limit_on_a_user_that_is_not_there_is_refused(store: &impl Backend) {
    tenant_with_user(store).await;
    let nobody = LimitScope::of_tenant(tenant_id()).of_user(UserId::new("nobody").unwrap());
    assert!(matches!(
        store
            .put_limit(NewLimit {
                scope: nobody,
                ..monthly(10)
            })
            .await,
        Err(StorageError::NotFound {
            entity: Entity::User,
            ..
        })
    ));
}

pub(crate) async fn deleting_a_tenant_takes_its_limits_with_it(store: &impl Backend) {
    tenant_with_user(store).await;
    store.put_limit(monthly(10)).await.unwrap();
    store
        .put_limit(NewLimit {
            scope: LimitScope::of_tenant(tenant_id()).of_user(user_id()),
            ..monthly(5)
        })
        .await
        .unwrap();

    store.delete_tenant(&tenant_id()).await.unwrap();
    assert!(store.limits().await.unwrap().is_empty());
}

pub(crate) async fn deleting_a_user_takes_the_limits_named_for_it(store: &impl Backend) {
    tenant_with_user(store).await;
    store.put_limit(monthly(10)).await.unwrap();
    store
        .put_limit(NewLimit {
            scope: LimitScope::of_tenant(tenant_id()).of_user(user_id()),
            ..monthly(5)
        })
        .await
        .unwrap();

    store.delete_user(&new_user().id).await.unwrap();
    let held = store.limits().await.unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].scope.user, None);
}

pub(crate) async fn every_change_to_a_limit_moves_the_revision_on(store: &impl Backend) {
    tenant_with_user(store).await;
    let before = store.rule_revision().await.unwrap();
    store.put_limit(monthly(10)).await.unwrap();
    let set = store.rule_revision().await.unwrap();
    assert!(
        set > before,
        "setting a limit left the revision at {before}"
    );

    store
        .remove_limit(
            &LimitScope::of_tenant(tenant_id()),
            Counted::Tokens,
            Period::Month,
        )
        .await
        .unwrap();
    assert!(store.rule_revision().await.unwrap() > set);
}
