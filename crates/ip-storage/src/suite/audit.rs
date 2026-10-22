use ip_core::{Allowance, Counted, LimitScope, Period, UserId};

use crate::audit::{AuditFilter, By};
use crate::error::Entity;
use crate::limit::NewLimit;
use crate::list::Page;
use crate::store::Backend;
use crate::suite::fixture::{tenant_id, tenant_with_user};
use crate::suite::limit::{by, monthly};

/// Everything recorded, newest first.
async fn trail(store: &impl Backend) -> Vec<crate::audit::Audited> {
    store
        .audits(&AuditFilter::default(), Page::default())
        .await
        .unwrap()
}

pub(crate) async fn setting_a_limit_records_who_set_it(store: &impl Backend) {
    tenant_with_user(store).await;
    store.put_limit(monthly(1_000_000), &by()).await.unwrap();

    let trail = trail(store).await;
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

    let trail = trail(store).await;
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

    let trail = trail(store).await;
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
    assert!(trail(store).await.is_empty());
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

    let mine = AuditFilter::of_tenant(tenant_id());
    assert_eq!(store.audits(&mine, Page::default()).await.unwrap().len(), 1);

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

/// The actor is whoever the handler said, not a name the store invents.
pub(crate) async fn the_trail_names_the_account_that_asked(store: &impl Backend) {
    tenant_with_user(store).await;
    let bob = By::new(UserId::new("bob").unwrap(), "/_ip/limits");
    store.put_limit(monthly(10), &bob).await.unwrap();

    assert_eq!(trail(store).await[0].actor, UserId::new("bob").unwrap());
}
