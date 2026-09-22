use ip_core::{ApiId, Capability, CapabilityScope, Grant, TenantId, TnKey};

use super::fixture::{tenant_id, tenant_with_user, user_id};
use crate::error::{Entity, StorageError};
use crate::model::{Membership, NewTenant, Standing};
use crate::store::{GrantStore, MembershipStore, TenantStore, UserStore};
use crate::transaction::member_remove::MemberRemoveTransactional;

/// Everything a member remove test needs from a backend.
pub(crate) trait MemberRemoveStore:
    MemberRemoveTransactional + TenantStore + UserStore + MembershipStore + GrantStore
{
}

impl<T> MemberRemoveStore for T where
    T: MemberRemoveTransactional + TenantStore + UserStore + MembershipStore + GrantStore
{
}

fn elsewhere() -> TenantId {
    TenantId::new("globex").unwrap()
}

/// Alice in acme and in globex, holding two grants inside acme, one inside globex and one
/// against her account.
async fn member_of_two(store: &impl MemberRemoveStore) -> [Grant; 4] {
    tenant_with_user(store).await;
    store
        .create_tenant(NewTenant {
            id: elsewhere(),
            key: TnKey::generate().unwrap(),
        })
        .await
        .unwrap();
    for tenant in [tenant_id(), elsewhere()] {
        store
            .attach(Membership {
                user: user_id(),
                tenant,
                standing: Standing::Member,
            })
            .await
            .unwrap();
    }
    let inside = |tenant: TenantId| CapabilityScope::Tenant {
        user: user_id(),
        tenant,
    };
    let grants = [
        Grant::new(Capability::UserMgr, inside(tenant_id())).unwrap(),
        Grant::new(
            Capability::ApiAccess,
            CapabilityScope::Api {
                user: user_id(),
                tenant: tenant_id(),
                api: ApiId::new("chat").unwrap(),
            },
        )
        .unwrap(),
        Grant::new(Capability::Observer, inside(elsewhere())).unwrap(),
        Grant::new(
            Capability::TenantMgr,
            CapabilityScope::User { user: user_id() },
        )
        .unwrap(),
    ];
    for grant in &grants {
        store.grant(grant).await.unwrap();
    }
    grants
}

pub(crate) async fn a_member_leaves_with_every_grant_it_held_inside(
    store: &impl MemberRemoveStore,
) {
    let [_, _, observer, tenant_mgr] = member_of_two(store).await;

    let removed = store
        .begin_member_remove()
        .await
        .unwrap()
        .detach(user_id(), tenant_id())
        .await
        .unwrap()
        .revoke_grants()
        .await
        .unwrap()
        .commit()
        .await
        .unwrap();

    assert_eq!(removed.membership.tenant, tenant_id());
    assert_eq!(removed.membership.standing, Standing::Member);
    assert_eq!(removed.revoked, 2);
    assert!(
        store
            .membership(&user_id(), &tenant_id())
            .await
            .unwrap()
            .is_none()
    );
    let left = store.grants_of(&user_id()).await.unwrap();
    assert_eq!(left.len(), 2);
    assert!(left.iter().any(|grant| grant == &observer));
    assert!(left.iter().any(|grant| grant == &tenant_mgr));
    assert!(
        store
            .membership(&user_id(), &elsewhere())
            .await
            .unwrap()
            .is_some()
    );
}

pub(crate) async fn a_removal_nobody_commits_leaves_the_member_and_its_grants(
    store: &impl MemberRemoveStore,
) {
    member_of_two(store).await;

    let revoked = store
        .begin_member_remove()
        .await
        .unwrap()
        .detach(user_id(), tenant_id())
        .await
        .unwrap()
        .revoke_grants()
        .await
        .unwrap();
    drop(revoked);

    assert!(
        store
            .membership(&user_id(), &tenant_id())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(store.grants_of(&user_id()).await.unwrap().len(), 4);
}

pub(crate) async fn leaving_a_tenant_it_is_not_in_reports_it_missing(
    store: &impl MemberRemoveStore,
) {
    tenant_with_user(store).await;

    let missing = store
        .begin_member_remove()
        .await
        .unwrap()
        .detach(user_id(), tenant_id())
        .await;

    assert!(matches!(
        missing,
        Err(StorageError::NotFound {
            entity: Entity::Membership,
            ..
        })
    ));
    assert!(store.user(&user_id()).await.unwrap().is_some());
}
