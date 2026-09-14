use ip_core::{
    ApiId, Checksum, NewPluginRule, PluginKind, PluginOrder, PluginScope, TenantId, TnKey,
};

use super::AnyStore;
use super::fixture::*;
use crate::error::{Entity, StorageError};
use crate::model::NewTenant;
use crate::plugin::NewPlugin;

/// A plugin of `kind` whose wasm is `body`.
pub(crate) fn plugin(kind: PluginKind, body: &[u8]) -> NewPlugin {
    NewPlugin {
        kind,
        wasm: body.to_vec(),
    }
}

fn placed(checksum: Checksum, order: u8, scope: PluginScope) -> NewPluginRule {
    NewPluginRule::new(checksum, PluginOrder::new(order), scope).unwrap()
}

fn acme_wide() -> PluginScope {
    PluginScope::Tenant {
        tenant: tenant_id(),
        user: None,
        api: None,
    }
}

fn alice_only() -> PluginScope {
    PluginScope::Tenant {
        tenant: tenant_id(),
        user: Some(user_id()),
        api: None,
    }
}

pub(crate) async fn a_plugin_round_trips_with_its_wasm(store: &impl AnyStore) {
    let record = store
        .put_plugin(plugin(PluginKind::ReqBody, b"\0asm module"))
        .await
        .unwrap();
    assert_eq!(record.checksum, Checksum::of(b"\0asm module"));
    assert_eq!(record.kind, PluginKind::ReqBody);
    let read = store.plugin(&record.checksum).await.unwrap().unwrap();
    assert_eq!(read.record, record);
    assert_eq!(read.wasm, b"\0asm module");
}

pub(crate) async fn storing_the_same_plugin_twice_keeps_one_row(store: &impl AnyStore) {
    let first = store
        .put_plugin(plugin(PluginKind::ReqBody, b"module"))
        .await
        .unwrap();
    let second = store
        .put_plugin(plugin(PluginKind::ReqBody, b"module"))
        .await
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(store.plugins().await.unwrap().len(), 1);
}

pub(crate) async fn listing_plugins_reports_their_size(store: &impl AnyStore) {
    store
        .put_plugin(plugin(PluginKind::ReqBody, b"123456"))
        .await
        .unwrap();
    store
        .put_plugin(plugin(PluginKind::RespBody, b"12"))
        .await
        .unwrap();
    let sizes: Vec<usize> = store
        .plugins()
        .await
        .unwrap()
        .iter()
        .map(|record| record.size)
        .collect();
    assert_eq!(sizes, vec![6, 2]);
}

pub(crate) async fn removing_a_plugin_that_is_absent_reports_it_missing(store: &impl AnyStore) {
    assert!(matches!(
        store.remove_plugin(&Checksum::of(b"absent")).await,
        Err(StorageError::NotFound {
            entity: Entity::Plugin,
            ..
        })
    ));
}

pub(crate) async fn a_plugin_a_rule_still_uses_is_not_removed_until_the_rule_goes(
    store: &impl AnyStore,
) {
    tenant_with_user(store).await;
    let record = store
        .put_plugin(plugin(PluginKind::ReqBody, b"module"))
        .await
        .unwrap();
    store
        .put_rule(placed(record.checksum, 100, acme_wide()))
        .await
        .unwrap();
    match store.remove_plugin(&record.checksum).await {
        Err(StorageError::InUse {
            entity: Entity::Plugin,
            ..
        }) => {}
        other => panic!("expected the plugin to be refused as in use, got {other:?}"),
    }
    store
        .remove_rule(
            Some(&tenant_id()),
            PluginKind::ReqBody,
            PluginOrder::new(100),
        )
        .await
        .unwrap();
    store.remove_plugin(&record.checksum).await.unwrap();
}

pub(crate) async fn every_rule_scope_round_trips(store: &impl AnyStore) {
    tenant_with_user(store).await;
    let record = store
        .put_plugin(plugin(PluginKind::ReqBody, b"module"))
        .await
        .unwrap();
    let anthropic = ApiId::new("anthropic").unwrap();
    let scopes = [
        (10, PluginScope::Global),
        (100, acme_wide()),
        (101, alice_only()),
        (
            102,
            PluginScope::Tenant {
                tenant: tenant_id(),
                user: None,
                api: Some(anthropic.clone()),
            },
        ),
        (
            103,
            PluginScope::Tenant {
                tenant: tenant_id(),
                user: Some(user_id()),
                api: Some(anthropic),
            },
        ),
    ];
    let mut expected = Vec::new();
    for (order, scope) in scopes {
        expected.push(
            store
                .put_rule(placed(record.checksum, order, scope))
                .await
                .unwrap(),
        );
    }
    let read = store.rules().await.unwrap();
    assert_eq!(read.len(), expected.len());
    for rule in &expected {
        assert!(read.contains(rule), "{rule:?} did not survive a round trip");
    }
}

pub(crate) async fn a_stored_rule_takes_its_kind_from_the_plugin(store: &impl AnyStore) {
    tenant_with_user(store).await;
    let record = store
        .put_plugin(plugin(PluginKind::RespHeader, b"module"))
        .await
        .unwrap();
    let rule = store
        .put_rule(placed(record.checksum, 100, acme_wide()))
        .await
        .unwrap();
    assert_eq!(rule.kind(), PluginKind::RespHeader);
    assert_eq!(
        store.rules().await.unwrap()[0].kind(),
        PluginKind::RespHeader
    );
}

pub(crate) async fn a_rule_naming_an_absent_plugin_is_refused(store: &impl AnyStore) {
    tenant_with_user(store).await;
    assert!(matches!(
        store
            .put_rule(placed(Checksum::of(b"absent"), 100, acme_wide()))
            .await,
        Err(StorageError::NotFound {
            entity: Entity::Plugin,
            ..
        })
    ));
}

pub(crate) async fn a_rule_in_a_tenant_that_is_not_there_is_refused(store: &impl AnyStore) {
    let record = store
        .put_plugin(plugin(PluginKind::ReqBody, b"module"))
        .await
        .unwrap();
    assert!(matches!(
        store
            .put_rule(placed(record.checksum, 100, acme_wide()))
            .await,
        Err(StorageError::NotFound {
            entity: Entity::Tenant,
            ..
        })
    ));
}

pub(crate) async fn an_order_a_tenant_already_uses_for_that_kind_is_refused(store: &impl AnyStore) {
    tenant_with_user(store).await;
    let first = store
        .put_plugin(plugin(PluginKind::ReqBody, b"one"))
        .await
        .unwrap();
    let second = store
        .put_plugin(plugin(PluginKind::ReqBody, b"two"))
        .await
        .unwrap();
    store
        .put_rule(placed(first.checksum, 100, acme_wide()))
        .await
        .unwrap();
    assert!(matches!(
        store
            .put_rule(placed(second.checksum, 100, alice_only()))
            .await,
        Err(StorageError::Conflict {
            entity: Entity::PluginRule,
            ..
        })
    ));
}

pub(crate) async fn the_same_order_under_another_kind_is_allowed(store: &impl AnyStore) {
    tenant_with_user(store).await;
    let request = store
        .put_plugin(plugin(PluginKind::ReqBody, b"one"))
        .await
        .unwrap();
    let response = store
        .put_plugin(plugin(PluginKind::RespBody, b"two"))
        .await
        .unwrap();
    store
        .put_rule(placed(request.checksum, 100, acme_wide()))
        .await
        .unwrap();
    assert!(
        store
            .put_rule(placed(response.checksum, 100, acme_wide()))
            .await
            .is_ok()
    );
}

pub(crate) async fn a_tenant_reads_its_own_rules_and_the_global_ones(store: &impl AnyStore) {
    tenant_with_user(store).await;
    let globex = TenantId::new("globex").unwrap();
    store
        .create_tenant(NewTenant {
            id: globex.clone(),
            key: TnKey::generate().unwrap(),
        })
        .await
        .unwrap();
    let record = store
        .put_plugin(plugin(PluginKind::ReqBody, b"module"))
        .await
        .unwrap();
    store
        .put_rule(placed(record.checksum, 10, PluginScope::Global))
        .await
        .unwrap();
    store
        .put_rule(placed(record.checksum, 100, acme_wide()))
        .await
        .unwrap();
    store
        .put_rule(placed(
            record.checksum,
            100,
            PluginScope::Tenant {
                tenant: globex,
                user: None,
                api: None,
            },
        ))
        .await
        .unwrap();
    let seen = store.rules_for_tenant(&tenant_id()).await.unwrap();
    assert_eq!(seen.len(), 2);
    assert!(seen.iter().all(|rule| {
        rule.scope()
            .tenant()
            .is_none_or(|owner| owner == &tenant_id())
    }));
}

pub(crate) async fn rules_come_back_highest_order_first(store: &impl AnyStore) {
    tenant_with_user(store).await;
    let record = store
        .put_plugin(plugin(PluginKind::ReqBody, b"module"))
        .await
        .unwrap();
    for order in [100, 200, 150] {
        store
            .put_rule(placed(record.checksum, order, acme_wide()))
            .await
            .unwrap();
    }
    let orders: Vec<u8> = store
        .rules_for_tenant(&tenant_id())
        .await
        .unwrap()
        .iter()
        .map(|rule| rule.order().get())
        .collect();
    assert_eq!(orders, vec![200, 150, 100]);
}

pub(crate) async fn deleting_a_tenant_takes_its_rules_with_it(store: &impl AnyStore) {
    tenant_with_user(store).await;
    let record = store
        .put_plugin(plugin(PluginKind::ReqBody, b"module"))
        .await
        .unwrap();
    store
        .put_rule(placed(record.checksum, 10, PluginScope::Global))
        .await
        .unwrap();
    store
        .put_rule(placed(record.checksum, 100, acme_wide()))
        .await
        .unwrap();
    store.delete_tenant(&tenant_id()).await.unwrap();
    let left = store.rules().await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].scope(), &PluginScope::Global);
}

pub(crate) async fn removing_a_global_rule_leaves_a_tenant_rule_at_the_same_kind(
    store: &impl AnyStore,
) {
    tenant_with_user(store).await;
    let record = store
        .put_plugin(plugin(PluginKind::ReqBody, b"module"))
        .await
        .unwrap();
    store
        .put_rule(placed(record.checksum, 10, PluginScope::Global))
        .await
        .unwrap();
    store
        .put_rule(placed(record.checksum, 100, acme_wide()))
        .await
        .unwrap();
    store
        .remove_rule(None, PluginKind::ReqBody, PluginOrder::new(10))
        .await
        .unwrap();
    assert_eq!(store.rules().await.unwrap().len(), 1);
}
