use ip_core::{
    ApiId, Capability, CapabilityScope, Endpoint, Grant, NewPluginRule, PluginKind, PluginOrder,
    PluginScope, RouteTarget, TenantId, Timestamp, TnKey, UserId,
};

use super::fixture::*;
use super::plugin::plugin;
use super::route::rule;
use crate::list::{Listed, Page};
use crate::model::{AccountKind, Membership, NewTenant, NewUser, Standing};
use crate::store::Backend;

fn user(raw: &str) -> UserId {
    UserId::new(raw).unwrap()
}

fn globex() -> TenantId {
    TenantId::new("globex").unwrap()
}

fn page(limit: u32, offset: u32) -> Page {
    Page::new(limit, offset, None)
}

fn items<T: Clone>(listed: &[Listed<T>]) -> Vec<T> {
    listed.iter().map(|listed| listed.item.clone()).collect()
}

/// Acme with five members, attached one after another from `m1` to `m5`.
async fn five_members(store: &impl Backend) -> Vec<UserId> {
    store.create_tenant(new_tenant()).await.unwrap();
    let mut members = Vec::new();
    for n in 1..=5 {
        let id = user(&format!("m{n}"));
        store
            .create_user(NewUser {
                id: id.clone(),
                passphrase: hash(),
                kind: AccountKind::Regular,
            })
            .await
            .unwrap();
        store
            .attach(Membership {
                user: id.clone(),
                tenant: tenant_id(),
                standing: Standing::Member,
            })
            .await
            .unwrap();
        members.push(id);
    }
    members
}

fn users_of(listed: &[Listed<Membership>]) -> Vec<String> {
    listed.iter().map(|m| m.item.user.to_string()).collect()
}

pub(crate) async fn members_come_newest_first_a_page_at_a_time(store: &impl Backend) {
    five_members(store).await;

    let first = store.list_members(&tenant_id(), page(2, 0)).await.unwrap();
    assert_eq!(users_of(&first), ["m5", "m4"]);
    let second = store.list_members(&tenant_id(), page(2, 2)).await.unwrap();
    assert_eq!(users_of(&second), ["m3", "m2"]);
    let last = store.list_members(&tenant_id(), page(2, 4)).await.unwrap();
    assert_eq!(users_of(&last), ["m1"]);
    let past = store.list_members(&tenant_id(), page(2, 6)).await.unwrap();
    assert!(past.is_empty());
    assert!(first.iter().all(|m| m.created_at.unix_seconds() > 0));
}

pub(crate) async fn a_moment_to_list_after_keeps_what_came_before_it_out(store: &impl Backend) {
    five_members(store).await;
    let now = Timestamp::now().unix_seconds();
    let long_ago = Timestamp::from_unix_seconds(now - 3600).unwrap();
    let later = Timestamp::from_unix_seconds(now + 3600).unwrap();

    let since_long_ago = Page::new(20, 0, Some(long_ago));
    assert_eq!(
        store
            .list_members(&tenant_id(), since_long_ago)
            .await
            .unwrap()
            .len(),
        5
    );
    let since_later = Page::new(20, 0, Some(later));
    assert!(
        store
            .list_members(&tenant_id(), since_later)
            .await
            .unwrap()
            .is_empty()
    );
}

pub(crate) async fn grants_are_listed_newest_first_by_where_they_are_held(store: &impl Backend) {
    tenant_with_user(store).await;
    store
        .create_tenant(NewTenant {
            id: globex(),
            key: TnKey::generate().unwrap(),
        })
        .await
        .unwrap();
    let inside = |tenant: TenantId| CapabilityScope::Tenant {
        user: user_id(),
        tenant,
    };
    let chat = CapabilityScope::Api {
        user: user_id(),
        tenant: tenant_id(),
        api: ApiId::new("chat").unwrap(),
    };
    let held = [
        Grant::new(Capability::Observer, inside(tenant_id())).unwrap(),
        Grant::new(Capability::ApiAccess, chat).unwrap(),
        Grant::new(Capability::UserMgr, inside(tenant_id())).unwrap(),
        Grant::new(Capability::Observer, inside(globex())).unwrap(),
        Grant::new(
            Capability::TenantMgr,
            CapabilityScope::User { user: user_id() },
        )
        .unwrap(),
    ];
    for grant in &held {
        store.grant(grant).await.unwrap();
    }

    let in_acme = store
        .list_tenant_grants(&user_id(), &tenant_id(), Page::default())
        .await
        .unwrap();
    assert_eq!(
        items(&in_acme),
        [held[2].clone(), held[1].clone(), held[0].clone()]
    );

    let paged = store
        .list_tenant_grants(&user_id(), &tenant_id(), page(1, 1))
        .await
        .unwrap();
    assert_eq!(items(&paged), [held[1].clone()]);

    let on_account = store
        .list_account_grants(&user_id(), Page::default())
        .await
        .unwrap();
    assert_eq!(items(&on_account), [held[4].clone()]);
}

pub(crate) async fn routes_are_listed_newest_first_with_every_target(store: &impl Backend) {
    let replicated = {
        let mut replicated = rule("gateway.local", "/v2", "one.example.com");
        let mut endpoints: Vec<Endpoint> = replicated.target.endpoints().to_vec();
        let mut second = endpoints[0].clone();
        second.host = ip_core::Host::new("two.example.com").unwrap();
        endpoints.push(second);
        replicated.target = RouteTarget::from_endpoints(endpoints).unwrap();
        replicated
    };
    let rules = [
        rule("gateway.local", "/v1", "api.example.com"),
        replicated,
        rule("gateway.local", "/v3", "api.example.com"),
    ];
    for rule in &rules {
        store.put_route(rule.clone()).await.unwrap();
    }

    let listed = store.list_routes(Page::default()).await.unwrap();
    assert_eq!(
        items(&listed),
        [rules[2].clone(), rules[1].clone(), rules[0].clone()]
    );

    let middle = store.list_routes(page(1, 1)).await.unwrap();
    assert_eq!(items(&middle), [rules[1].clone()]);
    assert_eq!(middle[0].item.target.endpoints().len(), 2);
}

pub(crate) async fn rules_are_listed_by_chain_newest_first(store: &impl Backend) {
    tenant_with_user(store).await;
    let record = store
        .put_plugin(plugin(PluginKind::RespBody, b"module"))
        .await
        .unwrap();
    let placements = [
        (10, PluginScope::Global),
        (11, PluginScope::Global),
        (
            100,
            PluginScope::Tenant {
                tenant: tenant_id(),
                user: None,
                api: None,
            },
        ),
    ];
    for (order, scope) in placements {
        store
            .put_rule(NewPluginRule::new(record.checksum, PluginOrder::new(order), scope).unwrap())
            .await
            .unwrap();
    }

    let orders = |listed: Vec<Listed<ip_core::PluginRule>>| -> Vec<u8> {
        listed.iter().map(|rule| rule.item.order().get()).collect()
    };
    assert_eq!(
        orders(store.list_rules(None, Page::default()).await.unwrap()),
        [11, 10]
    );
    assert_eq!(
        orders(
            store
                .list_rules(Some(&tenant_id()), Page::default())
                .await
                .unwrap()
        ),
        [100]
    );
    assert!(
        store
            .list_rules(Some(&globex()), Page::default())
            .await
            .unwrap()
            .is_empty()
    );
}
