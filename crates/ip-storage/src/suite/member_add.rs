use ip_core::{TenantId, UserId};

use super::fixture::{hash, new_tenant, new_user, tenant_id, user_id};
use crate::error::Entity;
use crate::error::StorageError;
use crate::model::{AccountKind, NewUser, Standing};
use crate::store::{MembershipStore, TenantStore, UserStore};
use crate::transaction::member_add::MemberAddTransactional;

/// Everything a member add test needs from a backend.
pub(crate) trait MemberAddStore:
    MemberAddTransactional + TenantStore + UserStore + MembershipStore
{
}

impl<T> MemberAddStore for T where
    T: MemberAddTransactional + TenantStore + UserStore + MembershipStore
{
}

pub(crate) async fn a_member_lands_with_its_attachment(store: &impl MemberAddStore) {
    store.create_tenant(new_tenant()).await.unwrap();

    let added = store
        .begin_member_add()
        .await
        .unwrap()
        .create_user(new_user())
        .await
        .unwrap()
        .attach(tenant_id(), Standing::Member)
        .await
        .unwrap()
        .commit()
        .await
        .unwrap();

    assert_eq!(added.user.id, user_id());
    assert_eq!(added.membership.tenant, tenant_id());
    assert_eq!(added.membership.standing, Standing::Member);
    assert_eq!(
        store
            .membership(&user_id(), &tenant_id())
            .await
            .unwrap()
            .unwrap()
            .standing,
        Standing::Member
    );
}

pub(crate) async fn a_member_nobody_commits_leaves_no_user_behind(store: &impl MemberAddStore) {
    store.create_tenant(new_tenant()).await.unwrap();

    let attached = store
        .begin_member_add()
        .await
        .unwrap()
        .create_user(new_user())
        .await
        .unwrap()
        .attach(tenant_id(), Standing::Member)
        .await
        .unwrap();
    drop(attached);

    assert!(store.user(&user_id()).await.unwrap().is_none());
    assert!(
        store
            .membership(&user_id(), &tenant_id())
            .await
            .unwrap()
            .is_none()
    );
}

pub(crate) async fn a_tenant_that_is_not_there_takes_the_user_with_it(store: &impl MemberAddStore) {
    let missing = store
        .begin_member_add()
        .await
        .unwrap()
        .create_user(new_user())
        .await
        .unwrap()
        .attach(TenantId::new("nowhere").unwrap(), Standing::Member)
        .await;

    assert!(matches!(
        missing,
        Err(StorageError::NotFound {
            entity: Entity::Tenant,
            ..
        })
    ));
    // The user was written before the tenant was looked up, so it goes back with the rollback.
    assert!(store.user(&user_id()).await.unwrap().is_none());
}

pub(crate) async fn a_member_may_be_added_as_the_owner(store: &impl MemberAddStore) {
    store.create_tenant(new_tenant()).await.unwrap();

    let added = store
        .begin_member_add()
        .await
        .unwrap()
        .create_user(new_user())
        .await
        .unwrap()
        .attach(tenant_id(), Standing::Owner)
        .await
        .unwrap()
        .commit()
        .await
        .unwrap();

    assert_eq!(added.membership.standing, Standing::Owner);
    assert_eq!(added.membership.user, added.user.id);
}

pub(crate) async fn a_user_id_that_is_taken_conflicts(store: &impl MemberAddStore) {
    store.create_tenant(new_tenant()).await.unwrap();
    let taken = UserId::new("taken").unwrap();
    store
        .create_user(NewUser {
            id: taken.clone(),
            passphrase: hash(),
            kind: AccountKind::Regular,
        })
        .await
        .unwrap();

    let conflict = store
        .begin_member_add()
        .await
        .unwrap()
        .create_user(NewUser {
            id: taken.clone(),
            passphrase: hash(),
            kind: AccountKind::Regular,
        })
        .await;

    assert!(matches!(
        conflict,
        Err(StorageError::Conflict {
            entity: Entity::User,
            ..
        })
    ));
    assert!(
        store
            .membership(&taken, &tenant_id())
            .await
            .unwrap()
            .is_none()
    );
}
