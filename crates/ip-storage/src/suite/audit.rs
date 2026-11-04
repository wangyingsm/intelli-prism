use ip_core::{Allowance, Counted, LimitScope, Period, PluginKind, UserId};

use crate::audit::{AuditFilter, By};
use crate::error::Entity;
use crate::limit::NewLimit;
use crate::list::Page;
use crate::store::Backend;
use crate::suite::fixture::{tenant_id, tenant_with_user};
use crate::suite::limit::{by, monthly};
use crate::suite::price::priced_at;

/// Everything recorded, newest first.
async fn trail(store: &impl Backend) -> Vec<crate::audit::Audited> {
    store
        .audits(&AuditFilter::default(), Page::default())
        .await
        .unwrap()
}

/// What was recorded about one kind of thing, newest first.
///
/// A test that counts rows narrows to its own entity, because the fixtures that set a
/// test up now write rows of their own: creating the tenant's account is itself a change
/// somebody made.
async fn trail_of(store: &impl Backend, entity: Entity) -> Vec<crate::audit::Audited> {
    let filter = AuditFilter {
        entity: Some(entity),
        ..AuditFilter::default()
    };
    store.audits(&filter, Page::default()).await.unwrap()
}

pub(crate) async fn setting_a_limit_records_who_set_it(store: &impl Backend) {
    tenant_with_user(store).await;
    store.put_limit(monthly(1_000_000), &by()).await.unwrap();

    let trail = trail_of(store, Entity::Limit).await;
    assert_eq!(trail.len(), 1, "one change, one record");
    let one = &trail[0];
    assert_eq!(one.actor, UserId::new("alice").unwrap());
    assert_eq!(one.through, "/_ip/limits");
    assert_eq!(one.entity, Entity::Limit);
    assert_eq!(one.tenant, Some(tenant_id()));
    assert_eq!(one.before, None, "nothing was there before");
    assert_eq!(one.after.as_ref().unwrap()["allowance"], 1_000_000);
}

/// What the before is for: the question asked after an incident is not *who changed
/// this* but *what did it say before they did*.
pub(crate) async fn setting_a_limit_again_records_what_it_allowed(store: &impl Backend) {
    tenant_with_user(store).await;
    store.put_limit(monthly(1_000_000), &by()).await.unwrap();
    store.put_limit(monthly(50), &by()).await.unwrap();

    let trail = trail_of(store, Entity::Limit).await;
    assert_eq!(trail.len(), 2);
    // Newest first, so the second change is the first row.
    let second = &trail[0];
    assert_eq!(second.before.as_ref().unwrap()["allowance"], 1_000_000);
    assert_eq!(second.after.as_ref().unwrap()["allowance"], 50);
}

pub(crate) async fn removing_a_limit_records_what_was_removed(store: &impl Backend) {
    tenant_with_user(store).await;
    store.put_limit(monthly(1_000_000), &by()).await.unwrap();
    store
        .remove_limit(
            &LimitScope::of_tenant(tenant_id()),
            Counted::Tokens,
            Period::Month,
            &by(),
        )
        .await
        .unwrap();

    let trail = trail_of(store, Entity::Limit).await;
    assert_eq!(trail.len(), 2);
    let removal = &trail[0];
    assert_eq!(removal.before.as_ref().unwrap()["allowance"], 1_000_000);
    assert_eq!(removal.after, None, "nothing is left to describe");
}

/// A change nobody made leaves nothing behind. The refusal happens before the write, so
/// a trail holding a row for it would be describing something that did not happen.
pub(crate) async fn a_change_that_was_refused_records_nothing(store: &impl Backend) {
    tenant_with_user(store).await;
    assert!(
        store
            .remove_limit(
                &LimitScope::of_tenant(tenant_id()),
                Counted::Tokens,
                Period::Month,
                &by(),
            )
            .await
            .is_err()
    );
    assert!(trail_of(store, Entity::Limit).await.is_empty());
}

/// A limit on a tenant that is not there is refused inside the transaction, after it
/// was opened and before anything was written, so neither the limit nor its record
/// survives. The pair is what ruling 1 of M22 is: one transaction, both or neither.
pub(crate) async fn a_write_that_fails_leaves_no_record(store: &impl Backend) {
    let absent = NewLimit {
        scope: LimitScope::of_tenant(ip_core::TenantId::new("nobody").unwrap()),
        counted: Counted::Tokens,
        period: Period::Month,
        allowance: Allowance::new(10).unwrap(),
    };
    assert!(store.put_limit(absent, &by()).await.is_err());
    assert!(
        trail(store).await.is_empty(),
        "a refused write left a record of itself"
    );
}

pub(crate) async fn the_trail_is_narrowed_to_what_the_filter_names(store: &impl Backend) {
    tenant_with_user(store).await;
    store.put_limit(monthly(10), &by()).await.unwrap();

    let mine = store
        .audits(&AuditFilter::of_tenant(tenant_id()), Page::default())
        .await
        .unwrap();
    // Two, because a tenant's own creation belongs in the trail that tenant is shown,
    // and the fixture created it.
    assert_eq!(mine.len(), 2);
    assert_eq!(mine[0].entity, Entity::Limit);
    assert_eq!(mine[1].entity, Entity::Tenant);

    let theirs = AuditFilter::of_tenant(ip_core::TenantId::new("other").unwrap());
    assert!(
        store
            .audits(&theirs, Page::default())
            .await
            .unwrap()
            .is_empty(),
        "a tenant read another tenant's trail"
    );

    let nobody = AuditFilter {
        actor: Some(UserId::new("bob").unwrap()),
        ..AuditFilter::default()
    };
    assert!(
        store
            .audits(&nobody, Page::default())
            .await
            .unwrap()
            .is_empty()
    );

    let priced = AuditFilter {
        entity: Some(Entity::Price),
        ..AuditFilter::default()
    };
    assert!(
        store
            .audits(&priced, Page::default())
            .await
            .unwrap()
            .is_empty(),
        "a limit was recorded as something else"
    );
}

/// A price is the deployment's, so its record names no tenant. The first entry of that
/// shape, and the reason the column is nullable at all.
pub(crate) async fn pricing_a_model_records_that_it_belongs_to_nobody(store: &impl Backend) {
    store
        .put_price(priced_at(3_000_000, 15_000_000), &crate::suite::price::by())
        .await
        .unwrap();

    let trail = trail(store).await;
    assert_eq!(trail.len(), 1);
    let one = &trail[0];
    assert_eq!(one.entity, Entity::Price);
    assert_eq!(one.entity_id, "claude-opus-5");
    assert_eq!(one.through, "/_ip/prices");
    assert_eq!(one.tenant, None, "a price belongs to the deployment");
    assert_eq!(one.before, None);
    assert_eq!(one.after.as_ref().unwrap()["input"], 3_000_000);
    assert_eq!(one.after.as_ref().unwrap()["output"], 15_000_000);
}

pub(crate) async fn repricing_and_unpricing_a_model_record_what_it_cost(store: &impl Backend) {
    let by = crate::suite::price::by();
    store
        .put_price(priced_at(3_000_000, 15_000_000), &by)
        .await
        .unwrap();
    store
        .put_price(priced_at(1_000_000, 5_000_000), &by)
        .await
        .unwrap();
    store
        .remove_price(&crate::suite::price::model(), &by)
        .await
        .unwrap();

    let trail = trail(store).await;
    assert_eq!(trail.len(), 3);
    let removal = &trail[0];
    assert_eq!(removal.before.as_ref().unwrap()["input"], 1_000_000);
    assert_eq!(removal.after, None, "nothing is left to describe");
    let repricing = &trail[1];
    assert_eq!(repricing.before.as_ref().unwrap()["input"], 3_000_000);
    assert_eq!(repricing.after.as_ref().unwrap()["input"], 1_000_000);
}

/// What belongs to the deployment is nobody's tenant trail, and is still in the whole one.
pub(crate) async fn a_tenant_sees_none_of_what_belongs_to_the_deployment(store: &impl Backend) {
    tenant_with_user(store).await;
    store.put_limit(monthly(10), &by()).await.unwrap();
    store
        .put_price(priced_at(3_000_000, 15_000_000), &crate::suite::price::by())
        .await
        .unwrap();

    let theirs = store
        .audits(&AuditFilter::of_tenant(tenant_id()), Page::default())
        .await
        .unwrap();
    assert_eq!(theirs.len(), 2, "a price was read as a tenant's");
    assert_eq!(theirs[0].entity, Entity::Limit);
    assert_eq!(theirs[1].entity, Entity::Tenant, "its own creation");
    assert_eq!(
        trail_of(store, Entity::Price).await.len(),
        1,
        "the price is in the whole trail, under nobody's tenant"
    );
}

/// A rule carries more than a name, so what the trail holds of it is the whole of what an
/// operator can set — which is what makes a routing change readable a year later.
pub(crate) async fn routing_a_key_somewhere_else_records_both_targets(store: &impl Backend) {
    let by = crate::suite::route::by();
    let first = crate::suite::route::rule("gateway.local", "/v1", "first.example.com");
    store.put_route(first.clone(), &by).await.unwrap();
    store
        .put_route(
            crate::suite::route::rule("gateway.local", "/v1", "second.example.com"),
            &by,
        )
        .await
        .unwrap();

    let trail = trail(store).await;
    assert_eq!(trail.len(), 2);
    let moved = &trail[0];
    assert_eq!(moved.entity, Entity::Route);
    assert_eq!(moved.entity_id, first.key.to_string());
    assert_eq!(moved.through, "/_ip/routes");
    assert_eq!(moved.tenant, None, "a route is the deployment's");
    assert_eq!(
        moved.before.as_ref().unwrap()["targets"][0]["endpoint"],
        "https://first.example.com:443/v1"
    );
    assert_eq!(
        moved.after.as_ref().unwrap()["targets"][0]["endpoint"],
        "https://second.example.com:443/v1"
    );
    assert_eq!(moved.after.as_ref().unwrap()["api"], "anthropic");
    assert_eq!(moved.after.as_ref().unwrap()["strategy"], "round_robin");
}

pub(crate) async fn removing_a_route_records_where_it_pointed(store: &impl Backend) {
    let by = crate::suite::route::by();
    let rule = crate::suite::route::rule("gateway.local", "/v1", "api.example.com");
    store.put_route(rule.clone(), &by).await.unwrap();
    store.remove_route(&rule.key, &by).await.unwrap();

    let trail = trail(store).await;
    assert_eq!(trail.len(), 2);
    let removal = &trail[0];
    assert_eq!(
        removal.before.as_ref().unwrap()["targets"][0]["endpoint"],
        "https://api.example.com:443/v1"
    );
    assert_eq!(removal.after, None, "nothing is left to describe");
}

/// A plugin is a tenant's when a tenant stored it and the deployment's when the global
/// chain did, so what the trail says it belongs to comes from the owner. The first entity
/// here whose tenant is a property of the value rather than of the kind of thing it is.
pub(crate) async fn whose_a_stored_plugin_is_comes_from_who_stored_it(store: &impl Backend) {
    tenant_with_user(store).await;
    let by = crate::suite::plugin::by();
    store
        .put_plugin(
            crate::suite::plugin::plugin(PluginKind::ReqBody, b"one"),
            &by,
        )
        .await
        .unwrap();
    store
        .put_plugin(
            crate::suite::plugin::owned_by(
                PluginKind::ReqBody,
                b"two",
                crate::plugin::PluginOwner::Tenant(tenant_id()),
            ),
            &by,
        )
        .await
        .unwrap();

    let trail = trail_of(store, Entity::Plugin).await;
    assert_eq!(trail.len(), 2);
    let theirs = &trail[0];
    assert_eq!(theirs.entity, Entity::Plugin);
    assert_eq!(theirs.tenant, Some(tenant_id()));
    assert_eq!(
        theirs.after.as_ref().unwrap()["owner"],
        tenant_id().as_str()
    );
    assert_eq!(theirs.after.as_ref().unwrap()["kind"], "req_body");
    assert_eq!(theirs.before, None, "nothing was stored there before");
    assert_eq!(trail[1].tenant, None, "the global chain is nobody's tenant");
    assert_eq!(trail[1].after.as_ref().unwrap()["owner"], "global");
}

/// Letting go of a plugin is a removal: the trail says what was held and nothing after.
pub(crate) async fn letting_go_of_a_plugin_records_what_was_held(store: &impl Backend) {
    let by = crate::suite::plugin::by();
    let stored = store
        .put_plugin(
            crate::suite::plugin::plugin(PluginKind::ReqBody, b"module"),
            &by,
        )
        .await
        .unwrap();
    store
        .disown_plugin(&stored.checksum, &crate::plugin::PluginOwner::Global, &by)
        .await
        .unwrap();

    let trail = trail(store).await;
    assert_eq!(trail.len(), 2);
    let gone = &trail[0];
    assert_eq!(gone.entity_id, stored.checksum.to_string());
    assert_eq!(
        gone.before.as_ref().unwrap()["checksum"],
        stored.checksum.to_hex()
    );
    assert_eq!(gone.after, None, "nothing is left to describe");
}

/// Placing a plugin in a chain is a creation, never a replacement: the order a chain
/// already uses is refused rather than overwritten, so the before is always nothing.
pub(crate) async fn placing_a_plugin_in_a_chain_records_where_it_was_put(store: &impl Backend) {
    tenant_with_user(store).await;
    let stored = store
        .put_plugin(
            crate::suite::plugin::owned_by(
                PluginKind::ReqBody,
                b"module",
                crate::plugin::PluginOwner::Tenant(tenant_id()),
            ),
            &crate::suite::plugin::by(),
        )
        .await
        .unwrap();
    let placed = ip_core::NewPluginRule::new(
        stored.checksum,
        ip_core::PluginOrder::new(100),
        ip_core::PluginScope::Tenant {
            tenant: tenant_id(),
            user: None,
            api: None,
        },
    )
    .unwrap();
    store
        .put_rule(placed, &crate::suite::plugin::rules_by())
        .await
        .unwrap();

    let trail = trail_of(store, Entity::PluginRule).await;
    assert_eq!(trail.len(), 1, "one placing, one record");
    let rule = &trail[0];
    assert_eq!(rule.entity, Entity::PluginRule);
    assert_eq!(rule.through, "/_ip/plugin-rules");
    assert_eq!(rule.tenant, Some(tenant_id()));
    assert_eq!(rule.before, None, "a placing never replaces");
    assert_eq!(rule.after.as_ref().unwrap()["order"], 100);
    assert_eq!(
        rule.after.as_ref().unwrap()["checksum"],
        stored.checksum.to_hex()
    );
}

/// Taking a plugin out of the global chain is the deployment's change, not a tenant's.
pub(crate) async fn taking_a_plugin_out_of_the_global_chain_names_no_tenant(store: &impl Backend) {
    let stored = store
        .put_plugin(
            crate::suite::plugin::plugin(PluginKind::ReqBody, b"module"),
            &crate::suite::plugin::by(),
        )
        .await
        .unwrap();
    let by = crate::suite::plugin::rules_by();
    store
        .put_rule(
            ip_core::NewPluginRule::new(
                stored.checksum,
                ip_core::PluginOrder::new(10),
                ip_core::PluginScope::Global,
            )
            .unwrap(),
            &by,
        )
        .await
        .unwrap();
    store
        .remove_rule(
            None,
            PluginKind::ReqBody,
            ip_core::PluginOrder::new(10),
            &by,
        )
        .await
        .unwrap();

    let trail = trail(store).await;
    assert_eq!(trail.len(), 3);
    let taken = &trail[0];
    assert_eq!(taken.entity, Entity::PluginRule);
    assert_eq!(taken.tenant, None, "the global chain is nobody's tenant");
    assert_eq!(taken.before.as_ref().unwrap()["order"], 10);
    assert_eq!(taken.after, None, "nothing is left to describe");
}

/// **The ruling this milestone turns on.** An account's verifier is the one field the
/// trail could carry that would be worth stealing, and the trail outlives the account and
/// is read by people with no business holding it. It is absent from every row, whatever
/// the change was.
pub(crate) async fn an_accounts_record_never_holds_its_passphrase(store: &impl Backend) {
    let by = crate::suite::identity::accounts_by();
    let made = crate::suite::fixture::new_user();
    let verifier = made.passphrase.as_str().to_owned();
    store.create_user(made, &by).await.unwrap();
    store
        .set_passphrase(
            &crate::suite::fixture::user_id(),
            &crate::suite::fixture::hash(),
            &by,
        )
        .await
        .unwrap();
    store
        .delete_user(&crate::suite::fixture::user_id(), &by)
        .await
        .unwrap();

    let trail = trail(store).await;
    assert_eq!(trail.len(), 3, "created, changed, removed");
    for one in &trail {
        let written = serde_json::to_string(&(&one.before, &one.after)).unwrap();
        assert!(
            !written.contains(&verifier) && !written.contains("passphrase"),
            "a verifier reached the trail: {written}"
        );
        assert_eq!(one.entity, Entity::User);
        assert_eq!(one.tenant, None, "an account is nobody's tenant");
    }
}

/// What a passphrase change leaves behind is *that* it happened. Before and after are the
/// same shape because the only thing that changed is not in that shape, which is the
/// point — and which reads unambiguously only while the verifier is the sole part of an
/// account that can be replaced.
pub(crate) async fn changing_a_passphrase_records_that_it_changed_and_never_what_to(
    store: &impl Backend,
) {
    let by = crate::suite::identity::accounts_by();
    store
        .create_user(crate::suite::fixture::new_user(), &by)
        .await
        .unwrap();
    store
        .set_passphrase(
            &crate::suite::fixture::user_id(),
            &crate::suite::fixture::hash(),
            &by,
        )
        .await
        .unwrap();

    let trail = trail(store).await;
    let changed = &trail[0];
    assert_eq!(changed.entity_id, crate::suite::fixture::user_id().as_str());
    assert_eq!(
        changed.before, changed.after,
        "nothing a reader may see changed"
    );
    assert!(changed.before.is_some(), "the account was there before");
}

pub(crate) async fn creating_and_deleting_an_account_record_both_ends(store: &impl Backend) {
    let by = crate::suite::identity::accounts_by();
    store
        .create_user(crate::suite::fixture::new_user(), &by)
        .await
        .unwrap();
    store
        .delete_user(&crate::suite::fixture::user_id(), &by)
        .await
        .unwrap();

    let trail = trail(store).await;
    assert_eq!(trail.len(), 2);
    assert_eq!(
        trail[1].before, None,
        "nothing was there before it was made"
    );
    assert_eq!(trail[1].after.as_ref().unwrap()["kind"], "regular");
    assert_eq!(trail[0].after, None, "nothing is left to describe");
    assert_eq!(trail[0].before.as_ref().unwrap()["kind"], "regular");
}

/// The actor is whoever the handler said, not a name the store invents.
pub(crate) async fn the_trail_names_the_account_that_asked(store: &impl Backend) {
    tenant_with_user(store).await;
    let bob = By::new(UserId::new("bob").unwrap(), "/_ip/limits");
    store.put_limit(monthly(10), &bob).await.unwrap();

    assert_eq!(trail(store).await[0].actor, UserId::new("bob").unwrap());
}

pub(crate) async fn creating_and_deleting_a_tenant_record_both_ends(store: &impl Backend) {
    let by = crate::suite::identity::tenants_by();
    store
        .create_tenant(crate::suite::fixture::new_tenant(), &by)
        .await
        .unwrap();
    store.delete_tenant(&tenant_id(), &by).await.unwrap();

    let trail = trail_of(store, Entity::Tenant).await;
    assert_eq!(trail.len(), 2);
    assert_eq!(trail[1].through, "/_ip/tenants");
    assert_eq!(
        trail[1].before, None,
        "nothing was there before it was made"
    );
    assert_eq!(trail[1].after.as_ref().unwrap()["id"], "acme");
    assert_eq!(trail[0].after, None, "nothing is left to describe");
    assert_eq!(trail[0].before.as_ref().unwrap()["id"], "acme");
}

/// The root key is held to the rule an account's verifier is held to, and the test looks
/// for the secret itself rather than for the shape that is meant to leave it out.
pub(crate) async fn a_tenants_record_never_holds_its_root_key(store: &impl Backend) {
    let by = crate::suite::identity::tenants_by();
    let made = crate::suite::fixture::new_tenant();
    let secret = made.key.to_hex();
    store.create_tenant(made, &by).await.unwrap();
    store.delete_tenant(&tenant_id(), &by).await.unwrap();

    let trail = trail_of(store, Entity::Tenant).await;
    assert_eq!(trail.len(), 2, "created, removed");
    for one in &trail {
        let written = serde_json::to_string(&(&one.before, &one.after)).unwrap();
        assert!(
            !written.contains(&secret) && !written.contains("key"),
            "a root key reached the trail: {written}"
        );
        assert_eq!(one.tenant, Some(tenant_id()), "a tenant's own");
    }
}

/// Creating a tenant with its owner is two things somebody did, in one transaction, so it
/// is two records that land together or not at all.
pub(crate) async fn creating_a_tenant_with_its_owner_records_both(
    store: &(impl Backend + crate::transaction::user_create::UserCreateTransactional),
) {
    let by = crate::suite::identity::tenants_by();
    store
        .begin_user_create()
        .await
        .unwrap()
        .create_tenant(crate::suite::fixture::new_tenant())
        .await
        .unwrap()
        .create_user(crate::suite::fixture::new_user())
        .await
        .unwrap()
        .attach(crate::model::Standing::Owner)
        .await
        .unwrap()
        .record(by)
        .await
        .unwrap()
        .commit()
        .await
        .unwrap();

    let tenants = trail_of(store, Entity::Tenant).await;
    assert_eq!(tenants.len(), 1);
    assert_eq!(tenants[0].entity_id, "acme");
    assert_eq!(tenants[0].through, "/_ip/tenants");
    let accounts = trail_of(store, Entity::User).await;
    assert_eq!(accounts.len(), 1, "the owner was created here too");
    assert_eq!(accounts[0].entity_id, "alice");
    assert_eq!(accounts[0].tenant, None, "an account is nobody's tenant");
}

/// An attachment is the tenant's, so its record names the tenant rather than nothing — it
/// is what an owner reading its own trail is shown.
pub(crate) async fn attaching_an_account_records_the_tenant_it_joined(store: &impl Backend) {
    tenant_with_user(store).await;
    let by = crate::suite::identity::members_by();
    store
        .attach(
            crate::model::Membership {
                user: crate::suite::fixture::user_id(),
                tenant: tenant_id(),
                standing: crate::model::Standing::Member,
            },
            &by,
        )
        .await
        .unwrap();

    let trail = trail_of(store, Entity::Membership).await;
    assert_eq!(trail.len(), 1, "one change, one record");
    let one = &trail[0];
    assert_eq!(one.through, "/_ip/tenants/{tenant}/members");
    assert_eq!(one.entity_id, "alice@acme", "named as the api names it");
    assert_eq!(one.tenant, Some(tenant_id()));
    assert_eq!(one.before, None, "the account was not in it before");
    assert_eq!(one.after.as_ref().unwrap()["standing"], "member");
}

/// Attaching again replaces a standing, and the before is what makes that readable: the
/// question after an incident is not who changed it but what it was.
pub(crate) async fn promoting_an_account_records_what_it_was(store: &impl Backend) {
    tenant_with_user(store).await;
    let by = crate::suite::identity::members_by();
    for standing in [
        crate::model::Standing::Member,
        crate::model::Standing::Owner,
    ] {
        store
            .attach(
                crate::model::Membership {
                    user: crate::suite::fixture::user_id(),
                    tenant: tenant_id(),
                    standing,
                },
                &by,
            )
            .await
            .unwrap();
    }

    let trail = trail_of(store, Entity::Membership).await;
    assert_eq!(trail.len(), 2);
    // Newest first, so the promotion is the first row.
    assert_eq!(trail[0].before.as_ref().unwrap()["standing"], "member");
    assert_eq!(trail[0].after.as_ref().unwrap()["standing"], "owner");
}

/// Adding a member creates an account and attaches it, which is two things somebody did,
/// in one transaction.
pub(crate) async fn adding_a_member_records_the_account_and_the_attachment(
    store: &(impl Backend + crate::transaction::member_add::MemberAddTransactional),
) {
    store
        .create_tenant(
            crate::suite::fixture::new_tenant(),
            &crate::suite::identity::tenants_by(),
        )
        .await
        .unwrap();
    store
        .begin_member_add()
        .await
        .unwrap()
        .create_user(crate::suite::fixture::new_user())
        .await
        .unwrap()
        .attach(tenant_id(), crate::model::Standing::Member)
        .await
        .unwrap()
        .record(crate::suite::identity::members_by())
        .await
        .unwrap()
        .commit()
        .await
        .unwrap();

    assert_eq!(trail_of(store, Entity::User).await.len(), 1);
    let attached = trail_of(store, Entity::Membership).await;
    assert_eq!(attached.len(), 1);
    assert_eq!(attached[0].entity_id, "alice@acme");
    assert_eq!(attached[0].after.as_ref().unwrap()["standing"], "member");
}

/// Taking a member out records the attachment that went, with the standing it held — what
/// is *not* here is the grants that went with it, which have their own records.
pub(crate) async fn taking_a_member_out_records_the_attachment_that_went(
    store: &(impl Backend + crate::transaction::member_remove::MemberRemoveTransactional),
) {
    tenant_with_user(store).await;
    store
        .attach(
            crate::model::Membership {
                user: crate::suite::fixture::user_id(),
                tenant: tenant_id(),
                standing: crate::model::Standing::Member,
            },
            &crate::suite::identity::members_by(),
        )
        .await
        .unwrap();
    store
        .begin_member_remove()
        .await
        .unwrap()
        .detach(crate::suite::fixture::user_id(), tenant_id())
        .await
        .unwrap()
        .revoke_grants()
        .await
        .unwrap()
        .record(crate::suite::identity::members_by())
        .await
        .unwrap()
        .commit()
        .await
        .unwrap();

    let trail = trail_of(store, Entity::Membership).await;
    assert_eq!(trail.len(), 2, "attached, then removed");
    assert_eq!(trail[0].after, None, "nothing is left to describe");
    assert_eq!(trail[0].before.as_ref().unwrap()["standing"], "member");
    assert_eq!(trail[0].tenant, Some(tenant_id()));
}

/// A grant handed over inside a tenant, recorded with the capability, whose it is, and the
/// api it is held against.
pub(crate) async fn handing_a_grant_over_records_who_handed_it(store: &impl Backend) {
    tenant_with_user(store).await;
    let by = crate::suite::identity::grants_by();
    store.grant(&api_access(), &by).await.unwrap();

    let trail = trail_of(store, Entity::Grant).await;
    assert_eq!(trail.len(), 1, "one change, one record");
    let one = &trail[0];
    assert_eq!(one.actor, UserId::new("alice").unwrap());
    assert_eq!(one.through, "/_ip/tenants/{tenant}/members/{user}/grants");
    assert_eq!(one.entity_id, "api_access on alice@acme/chat");
    assert_eq!(one.tenant, Some(tenant_id()));
    assert_eq!(one.before, None, "nothing was there before");
    let after = one.after.as_ref().unwrap();
    assert_eq!(after["capability"], "api_access");
    assert_eq!(after["user"], "alice");
    assert_eq!(after["tenant"], "acme");
    assert_eq!(after["api"], "chat");
}

/// Granting what is already held changes nothing, and a trail that says otherwise is worse
/// than one that says nothing: it would put a capability's arrival on the wrong day.
pub(crate) async fn handing_over_a_grant_already_held_records_nothing_twice(store: &impl Backend) {
    tenant_with_user(store).await;
    let by = crate::suite::identity::grants_by();
    store.grant(&api_access(), &by).await.unwrap();
    store.grant(&api_access(), &by).await.unwrap();

    assert_eq!(trail_of(store, Entity::Grant).await.len(), 1);
}

/// Taking a grant back records what went, which is the half of a capability's life an
/// incident asks about as often as the half that handed it over.
pub(crate) async fn taking_a_grant_back_records_what_went(store: &impl Backend) {
    tenant_with_user(store).await;
    let by = crate::suite::identity::grants_by();
    store.grant(&api_access(), &by).await.unwrap();
    store.revoke(&api_access(), &by).await.unwrap();

    let trail = trail_of(store, Entity::Grant).await;
    assert_eq!(trail.len(), 2, "handed over, then taken back");
    assert_eq!(trail[0].after, None, "nothing is left to describe");
    assert_eq!(
        trail[0].before.as_ref().unwrap()["capability"],
        "api_access"
    );
    assert_eq!(trail[0].entity_id, "api_access on alice@acme/chat");
}

/// Refusing to take back what is not held records nothing: there was no change to record.
pub(crate) async fn taking_back_a_grant_that_is_not_held_records_nothing(store: &impl Backend) {
    tenant_with_user(store).await;
    assert!(
        store
            .revoke(&api_access(), &crate::suite::identity::grants_by())
            .await
            .is_err()
    );

    assert!(trail_of(store, Entity::Grant).await.is_empty());
}

/// A grant held against an account itself belongs to the deployment, not to a tenant, so no
/// tenant owner reading its own trail is shown somebody else's `TenantMgr`.
pub(crate) async fn a_grant_against_an_account_belongs_to_no_tenant(store: &impl Backend) {
    tenant_with_user(store).await;
    let grant = ip_core::Grant::new(
        ip_core::Capability::TenantMgr,
        ip_core::CapabilityScope::User {
            user: crate::suite::fixture::user_id(),
        },
    )
    .unwrap();
    store
        .grant(&grant, &crate::suite::identity::grants_by())
        .await
        .unwrap();

    let trail = trail_of(store, Entity::Grant).await;
    assert_eq!(trail.len(), 1);
    assert_eq!(trail[0].tenant, None, "the deployment's, not a tenant's");
    assert_eq!(trail[0].entity_id, "tenant_mgr on alice");
    assert_eq!(
        trail[0].after.as_ref().unwrap()["tenant"],
        serde_json::Value::Null
    );
}

/// Leaving a tenant records every grant the sweep took away, one row each, so an incident
/// asking what somebody lost on the way out is not left with a count.
pub(crate) async fn leaving_a_tenant_records_every_grant_it_took_away(
    store: &(impl Backend + crate::transaction::member_remove::MemberRemoveTransactional),
) {
    a_member_holding(
        store,
        &[api_access(), inside_the_tenant(), on_the_account()],
    )
    .await;
    detach_the_member(store).await;

    let swept: Vec<_> = trail_of(store, Entity::Grant)
        .await
        .into_iter()
        .filter(|one| one.after.is_none())
        .collect();
    assert_eq!(swept.len(), 2, "the two held inside, and not the third");
    let ids: Vec<&str> = swept.iter().map(|one| one.entity_id.as_str()).collect();
    assert!(ids.contains(&"api_access on alice@acme/chat"));
    assert!(ids.contains(&"user_mgr on alice@acme"));
    for one in &swept {
        assert_eq!(one.tenant, Some(tenant_id()), "the tenant it was held in");
        assert_eq!(
            one.through, "/_ip/tenants/{tenant}/members",
            "the endpoint that took the member out, not the one that grants"
        );
        assert_eq!(one.before.as_ref().unwrap()["user"], "alice");
    }
}

/// The grant held against the account itself survives being taken out of a tenant, and the
/// trail says nothing about it: a tenant's sweep is not the deployment's business.
pub(crate) async fn leaving_a_tenant_leaves_what_is_held_outside_it(
    store: &(impl Backend + crate::transaction::member_remove::MemberRemoveTransactional),
) {
    a_member_holding(store, &[api_access(), on_the_account()]).await;
    detach_the_member(store).await;

    let left = store
        .grants_of(&crate::suite::fixture::user_id())
        .await
        .unwrap();
    assert_eq!(left.len(), 1, "the account's own grant is still held");
    let swept: Vec<_> = trail_of(store, Entity::Grant)
        .await
        .into_iter()
        .filter(|one| one.after.is_none())
        .collect();
    assert_eq!(swept.len(), 1);
    assert_eq!(swept[0].entity_id, "api_access on alice@acme/chat");
}

/// A member who held nothing inside leaves no grant rows behind: the sweep found nothing, and
/// a row saying otherwise would invent a revocation that never happened.
pub(crate) async fn leaving_a_tenant_holding_nothing_records_only_the_attachment(
    store: &(impl Backend + crate::transaction::member_remove::MemberRemoveTransactional),
) {
    a_member_holding(store, &[]).await;
    detach_the_member(store).await;

    assert!(
        trail_of(store, Entity::Grant).await.is_empty(),
        "nothing was swept, so nothing is recorded"
    );
    assert_eq!(
        trail_of(store, Entity::Membership).await.len(),
        2,
        "attached, then removed"
    );
}

/// Attaches the fixture's account to the fixture's tenant, holding the grants given.
async fn a_member_holding(store: &impl Backend, grants: &[ip_core::Grant]) {
    tenant_with_user(store).await;
    store
        .attach(
            crate::model::Membership {
                user: crate::suite::fixture::user_id(),
                tenant: tenant_id(),
                standing: crate::model::Standing::Member,
            },
            &crate::suite::identity::members_by(),
        )
        .await
        .unwrap();
    for grant in grants {
        store
            .grant(grant, &crate::suite::identity::grants_by())
            .await
            .unwrap();
    }
}

/// Runs the whole removal, through the endpoint that detaches a member.
async fn detach_the_member(
    store: &(impl Backend + crate::transaction::member_remove::MemberRemoveTransactional),
) {
    store
        .begin_member_remove()
        .await
        .unwrap()
        .detach(crate::suite::fixture::user_id(), tenant_id())
        .await
        .unwrap()
        .revoke_grants()
        .await
        .unwrap()
        .record(crate::suite::identity::members_by())
        .await
        .unwrap()
        .commit()
        .await
        .unwrap();
}

/// A grant held across the whole tenant rather than against one api.
fn inside_the_tenant() -> ip_core::Grant {
    ip_core::Grant::new(
        ip_core::Capability::UserMgr,
        ip_core::CapabilityScope::Tenant {
            user: crate::suite::fixture::user_id(),
            tenant: tenant_id(),
        },
    )
    .unwrap()
}

/// A grant held against the account itself, which no tenant's sweep reaches.
fn on_the_account() -> ip_core::Grant {
    ip_core::Grant::new(
        ip_core::Capability::TenantMgr,
        ip_core::CapabilityScope::User {
            user: crate::suite::fixture::user_id(),
        },
    )
    .unwrap()
}

/// The one grant every test above hands over.
fn api_access() -> ip_core::Grant {
    ip_core::Grant::new(
        ip_core::Capability::ApiAccess,
        ip_core::CapabilityScope::Api {
            user: crate::suite::fixture::user_id(),
            tenant: tenant_id(),
            api: ip_core::ApiId::new("chat").unwrap(),
        },
    )
    .unwrap()
}
