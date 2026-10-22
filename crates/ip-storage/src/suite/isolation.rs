//! Whether one tenant can be shown another tenant's rows.
//!
//! Every other suite file asks whether a store does what it is for. This one asks the
//! opposite question of the same methods: given two tenants with data of their own,
//! does a listing naming one ever hand back the other's?
//!
//! It is written in one place and covers **every** listing on [`ListStore`] rather
//! than the few that happen to have a test of their own, because isolation is only
//! worth anything when it has no gaps. A listing added without a case here fails
//! [`every_listing_is_audited`], which is the tripwire that keeps that true.
//!
//! What this cannot say: reads that take an id rather than a tenant — `usage`,
//! `route`, `plugin` — are not scoped by anything the store knows, and could not be.
//! Nothing here protects them and nothing here pretends to; the capability check on
//! the route is what does, and that is the audit's other half.

use ip_core::{
    Capability, CapabilityScope, Grant, NewPluginRule, PluginKind, PluginScope, TenantId, TnKey,
};

use super::fixture::{hash, tenant_with_user};
use super::limit::monthly;
use super::plugin::{owned_by, stored_for};
use super::usage::spent;
use crate::list::Page;
use crate::model::{AccountKind, Membership, NewTenant, NewUser, Standing};
use crate::plugin::PluginOwner;
use crate::store::Backend;
use crate::suite::limit::by;
use crate::usage::UsageFilter;

/// The tenant whose rows must never be handed to anybody else.
fn theirs() -> TenantId {
    TenantId::new("elsewhere").unwrap()
}

/// The account that belongs to that other tenant alone.
fn their_user() -> ip_core::UserId {
    ip_core::UserId::new("mallory").unwrap()
}

/// Two tenants, each with a member of its own. `acme`/`alice` is the one asking;
/// `elsewhere`/`mallory` is the one whose rows must stay out of every answer.
async fn two_tenants(store: &impl Backend) {
    tenant_with_user(store).await;
    store
        .create_tenant(NewTenant {
            id: theirs(),
            key: TnKey::generate().unwrap(),
        })
        .await
        .unwrap();
    store
        .create_user(
            NewUser {
                id: their_user(),
                passphrase: hash(),
                kind: AccountKind::Regular,
            },
            &crate::suite::identity::accounts_by(),
        )
        .await
        .unwrap();
    store
        .attach(Membership {
            user: their_user(),
            tenant: theirs(),
            standing: Standing::Member,
        })
        .await
        .unwrap();
}

pub(crate) async fn members_of_one_tenant_are_not_listed_for_another(store: &impl Backend) {
    two_tenants(store).await;
    store
        .attach(Membership {
            user: super::fixture::user_id(),
            tenant: super::fixture::tenant_id(),
            standing: Standing::Member,
        })
        .await
        .unwrap();
    let listed = store
        .list_members(&super::fixture::tenant_id(), Page::default())
        .await
        .unwrap();
    assert!(
        listed.iter().all(|one| one.item.user != their_user()),
        "another tenant's member was listed"
    );
    assert_eq!(listed.len(), 1);
}

pub(crate) async fn grants_inside_one_tenant_are_not_listed_for_another(store: &impl Backend) {
    two_tenants(store).await;
    let scope = CapabilityScope::Tenant {
        user: their_user(),
        tenant: theirs(),
    };
    store
        .grant(&Grant::new(Capability::Observer, scope).unwrap())
        .await
        .unwrap();
    // Asked for the same account inside the tenant that is not theirs.
    let listed = store
        .list_tenant_grants(&their_user(), &super::fixture::tenant_id(), Page::default())
        .await
        .unwrap();
    assert!(listed.is_empty(), "a grant crossed a tenant boundary");
}

pub(crate) async fn grants_on_one_account_are_not_listed_for_another(store: &impl Backend) {
    two_tenants(store).await;
    let scope = CapabilityScope::User { user: their_user() };
    store
        .grant(&Grant::new(Capability::TenantMgr, scope).unwrap())
        .await
        .unwrap();
    let listed = store
        .list_account_grants(&super::fixture::user_id(), Page::default())
        .await
        .unwrap();
    assert!(listed.is_empty(), "another account's grant was listed");
}

pub(crate) async fn plugins_one_tenant_owns_are_not_listed_for_another(store: &impl Backend) {
    two_tenants(store).await;
    store
        .put_plugin(
            owned_by(
                PluginKind::ReqHeader,
                b"\0asm\x01\0\0\0theirs",
                PluginOwner::Tenant(theirs()),
            ),
            &crate::suite::plugin::by(),
        )
        .await
        .unwrap();
    for owner in [
        PluginOwner::Tenant(super::fixture::tenant_id()),
        PluginOwner::Global,
    ] {
        let listed = store.list_plugins(&owner, Page::default()).await.unwrap();
        assert!(
            listed.is_empty(),
            "a plugin owned by another tenant was listed for {owner:?}"
        );
    }
}

pub(crate) async fn limits_on_one_tenant_are_not_listed_for_another(store: &impl Backend) {
    two_tenants(store).await;
    store.put_limit(monthly(10), &by()).await.unwrap();
    let listed = store
        .list_limits(Some(&theirs()), Page::default())
        .await
        .unwrap();
    assert!(listed.is_empty(), "another tenant's limit was listed");
}

pub(crate) async fn what_one_tenant_spent_is_not_listed_for_another(store: &impl Backend) {
    two_tenants(store).await;
    store.record_usage(spent()).await.unwrap();
    let filter = UsageFilter {
        tenant: Some(theirs()),
        ..UsageFilter::default()
    };
    let listed = store.list_usage(&filter, Page::default()).await.unwrap();
    assert!(listed.is_empty(), "another tenant's spending was listed");
}

/// And what one tenant spent is not *summed* for another either: a total is a leak as
/// surely as a row is, and it is read through a different method.
pub(crate) async fn what_one_tenant_spent_is_not_summed_for_another(store: &impl Backend) {
    two_tenants(store).await;
    store.record_usage(spent()).await.unwrap();
    let filter = UsageFilter {
        tenant: Some(theirs()),
        ..UsageFilter::default()
    };
    let summed = store
        .spent(
            &filter,
            ip_core::Counted::Tokens,
            ip_core::Timestamp::from_unix_seconds(0).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(summed, 0, "another tenant's spending was summed");
}

pub(crate) async fn rules_in_one_tenants_chain_are_not_listed_for_another(store: &impl Backend) {
    two_tenants(store).await;
    let plugin = stored_for(
        store,
        PluginKind::ReqHeader,
        b"\0asm\x01\0\0\0rule",
        &[PluginOwner::Tenant(theirs())],
    )
    .await;
    store
        .put_rule(
            NewPluginRule::new(
                plugin.checksum,
                // Past the primary band, which only the rules the system ships may use.
                ip_core::PluginOrder::new(100),
                PluginScope::Tenant {
                    tenant: theirs(),
                    user: None,
                    api: None,
                },
            )
            .unwrap(),
            &crate::suite::plugin::rules_by(),
        )
        .await
        .unwrap();
    for asked in [Some(&super::fixture::tenant_id()), None] {
        let listed = store.list_rules(asked, Page::default()).await.unwrap();
        assert!(
            listed.is_empty(),
            "another tenant's rule was listed for {asked:?}"
        );
    }
}

/// Routing rules belong to the gateway, not to a tenant, so every listing shows every
/// rule. Asserted rather than left implied: if routes ever become tenant-scoped, this
/// is the test that says the audit must grow a case for them.
pub(crate) async fn routes_belong_to_no_tenant_and_the_audit_says_so(store: &impl Backend) {
    two_tenants(store).await;
    let listed = store.list_routes(Page::default()).await.unwrap();
    assert!(
        listed.is_empty(),
        "no route was stored, so none should be listed"
    );
}

/// The tripwire. Every listing on `ListStore` must have a case above, or isolation has
/// a gap exactly where nobody looked.
///
/// Counted from the source rather than from a list kept by hand, because a list kept
/// by hand is the thing that goes stale.
#[test]
fn every_listing_is_audited() {
    const SOURCE: &str = include_str!("../list.rs");
    let listings: Vec<&str> = SOURCE
        .lines()
        .filter_map(|line| line.trim().strip_prefix("async fn list_"))
        .map(|rest| rest.split(['(', '<']).next().unwrap_or(rest))
        .collect();
    // One case each, and `list_usage` twice over: listed and summed are separate reads.
    let audited = [
        "members",
        "tenant_grants",
        "account_grants",
        "routes",
        "plugins",
        "limits",
        "usage",
        "rules",
    ];
    for listing in &listings {
        assert!(
            audited.contains(listing),
            "list_{listing} has no isolation case; add one to suite/isolation.rs"
        );
    }
    assert_eq!(
        listings.len(),
        audited.len(),
        "ListStore has {} listings and {} are audited: {listings:?}",
        listings.len(),
        audited.len()
    );
}
