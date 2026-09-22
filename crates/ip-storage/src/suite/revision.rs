use ip_core::{
    Capability, CapabilityScope, Grant, NewPluginRule, PluginKind, PluginOrder, PluginScope,
};

use super::fixture::*;
use super::plugin::stored_for;
use super::route::rule;
use crate::plugin::PluginOwner;
use crate::revision::RuleRevision;
use crate::store::Backend;

async fn revision(store: &impl Backend) -> RuleRevision {
    store.rule_revision().await.unwrap()
}

/// Places a plugin acme owns, for acme alone and narrowed to alice.
async fn alice_rule(store: &impl Backend) {
    let record = stored_for(
        store,
        PluginKind::ReqBody,
        b"module",
        &[PluginOwner::Tenant(tenant_id())],
    )
    .await;
    let scope = PluginScope::Tenant {
        tenant: tenant_id(),
        user: Some(user_id()),
        api: None,
    };
    store
        .put_rule(NewPluginRule::new(record.checksum, PluginOrder::new(100), scope).unwrap())
        .await
        .unwrap();
}

pub(crate) async fn every_change_to_a_route_moves_the_revision_on(store: &impl Backend) {
    let mut seen = revision(store).await;
    let first = rule("gateway.local", "/v1", "one.example.com");
    let replaced = rule("gateway.local", "/v1", "two.example.com");
    store.put_route(first).await.unwrap();
    for change in 0..2 {
        match change {
            0 => store.put_route(replaced.clone()).await.unwrap(),
            _ => store.remove_route(&replaced.key).await.unwrap(),
        }
        let now = revision(store).await;
        assert!(now > seen, "change {change} left the revision at {now}");
        seen = now;
    }
}

pub(crate) async fn every_change_to_a_plugin_rule_moves_the_revision_on(store: &impl Backend) {
    tenant_with_user(store).await;
    let before = revision(store).await;
    alice_rule(store).await;
    let placed = revision(store).await;
    assert!(placed > before);
    store
        .remove_rule(
            Some(&tenant_id()),
            PluginKind::ReqBody,
            PluginOrder::new(100),
        )
        .await
        .unwrap();
    assert!(revision(store).await > placed);
}

pub(crate) async fn rules_a_deleted_tenant_takes_with_it_move_the_revision_on(
    store: &impl Backend,
) {
    tenant_with_user(store).await;
    alice_rule(store).await;
    let before = revision(store).await;
    store.delete_tenant(&tenant_id()).await.unwrap();
    assert!(revision(store).await > before);
    assert!(store.rules().await.unwrap().is_empty());
}

pub(crate) async fn rules_a_deleted_user_takes_with_it_move_the_revision_on(store: &impl Backend) {
    tenant_with_user(store).await;
    alice_rule(store).await;
    let before = revision(store).await;
    store.delete_user(&user_id()).await.unwrap();
    assert!(revision(store).await > before);
    assert!(store.rules().await.unwrap().is_empty());
}

pub(crate) async fn changes_to_nothing_a_node_holds_leave_the_revision_alone(store: &impl Backend) {
    let before = revision(store).await;
    tenant_with_user(store).await;
    store
        .grant(
            &Grant::new(
                Capability::TenantMgr,
                CapabilityScope::User { user: user_id() },
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(revision(store).await, before);
}

pub(crate) async fn the_rule_set_is_read_at_the_revision_it_names(store: &impl Backend) {
    tenant_with_user(store).await;
    let route = rule("gateway.local", "/v1", "api.example.com");
    store.put_route(route.clone()).await.unwrap();
    alice_rule(store).await;

    let set = store.rule_set().await.unwrap();
    assert_eq!(set.revision, revision(store).await);
    assert_eq!(set.routes, [route]);
    assert_eq!(set.rules.len(), 1);
}
