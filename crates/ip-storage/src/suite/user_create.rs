use ip_core::UserId;

use super::fixture::{hash, new_tenant, new_user, tenant_id, user_id};
use crate::model::{AccountKind, NewUser, Standing};
use crate::store::{MembershipStore, TenantStore, UserStore};
use crate::transaction::user_create::UserCreateTransactional;

/// Everything a transaction test needs from a backend.
pub(crate) trait UserCreateStore:
    UserCreateTransactional + TenantStore + UserStore + MembershipStore
{
}

impl<T> UserCreateStore for T where
    T: UserCreateTransactional + TenantStore + UserStore + MembershipStore
{
}

pub(crate) async fn a_committed_transaction_lands_every_write(store: &impl UserCreateStore) {
    let created = store
        .begin_user_create()
        .await
        .unwrap()
        .create_tenant(new_tenant())
        .await
        .unwrap()
        .create_user(new_user())
        .await
        .unwrap()
        .attach(Standing::Owner)
        .await
        .unwrap()
        .commit()
        .await
        .unwrap();

    assert_eq!(created.tenant.id, tenant_id());
    assert_eq!(created.owner.id, user_id());
    assert_eq!(created.membership.standing, Standing::Owner);
    assert!(store.tenant(&tenant_id()).await.unwrap().is_some());
    assert!(store.user(&user_id()).await.unwrap().is_some());
    assert_eq!(
        store
            .membership(&user_id(), &tenant_id())
            .await
            .unwrap()
            .unwrap()
            .standing,
        Standing::Owner
    );
}

pub(crate) async fn a_transaction_nobody_commits_leaves_nothing(store: &impl UserCreateStore) {
    let attached = store
        .begin_user_create()
        .await
        .unwrap()
        .create_tenant(new_tenant())
        .await
        .unwrap()
        .create_user(new_user())
        .await
        .unwrap()
        .attach(Standing::Owner)
        .await
        .unwrap();
    drop(attached);

    assert!(store.tenant(&tenant_id()).await.unwrap().is_none());
    assert!(store.user(&user_id()).await.unwrap().is_none());
    assert!(
        store
            .membership(&user_id(), &tenant_id())
            .await
            .unwrap()
            .is_none()
    );
}

pub(crate) async fn a_write_that_fails_part_way_undoes_the_ones_before_it(
    store: &impl UserCreateStore,
) {
    let taken = UserId::new("taken").unwrap();
    store
        .create_user(NewUser {
            id: taken.clone(),
            passphrase: hash(),
            kind: AccountKind::Regular,
        })
        .await
        .unwrap();

    // The owner conflicts with a user that is already there, so the tenant goes with it.
    let conflict = store
        .begin_user_create()
        .await
        .unwrap()
        .create_tenant(new_tenant())
        .await
        .unwrap()
        .create_user(NewUser {
            id: taken,
            passphrase: hash(),
            kind: AccountKind::Regular,
        })
        .await;
    assert!(conflict.is_err());

    assert!(store.tenant(&tenant_id()).await.unwrap().is_none());
}

pub(crate) async fn a_transaction_reads_back_what_it_has_written(store: &impl UserCreateStore) {
    let saved = store
        .begin_user_create()
        .await
        .unwrap()
        .create_tenant(new_tenant())
        .await
        .unwrap()
        .create_user(new_user())
        .await
        .unwrap();
    assert_eq!(saved.tenant().id, tenant_id());
    assert_eq!(saved.user().id, user_id());
    // Attaching looks up both row ids, which are only there for a transaction reading its own writes.
    saved
        .attach(Standing::Owner)
        .await
        .unwrap()
        .commit()
        .await
        .unwrap();
    assert!(
        store
            .membership(&user_id(), &tenant_id())
            .await
            .unwrap()
            .is_some()
    );
}

pub(crate) async fn an_owner_is_attached_to_the_tenant_the_transaction_wrote(
    store: &impl UserCreateStore,
) {
    let created = store
        .begin_user_create()
        .await
        .unwrap()
        .create_tenant(new_tenant())
        .await
        .unwrap()
        .create_user(new_user())
        .await
        .unwrap()
        .attach(Standing::Member)
        .await
        .unwrap()
        .commit()
        .await
        .unwrap();
    assert_eq!(created.membership.tenant, created.tenant.id);
    assert_eq!(created.membership.user, created.owner.id);
    assert_eq!(created.membership.standing, Standing::Member);
}

pub(crate) async fn writes_outside_a_transaction_are_not_rolled_back(store: &impl UserCreateStore) {
    store.create_tenant(new_tenant()).await.unwrap();
    drop(store.begin_user_create().await.unwrap());
    assert!(store.tenant(&tenant_id()).await.unwrap().is_some());
}
