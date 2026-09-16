use ip_core::{ApiId, Capability, CapabilityScope, Grant, PassphraseHash, TenantId, TnKey, UserId};

use super::fixture::*;
use crate::error::{Entity, StorageError};
use crate::model::{AccountKind, Membership, NewTenant, NewUser, Standing, TenantRowId};
use crate::store::Storage;

fn api_grant(user: &UserId, tenant: &TenantId, capability: Capability) -> Grant {
    Grant::new(
        capability,
        CapabilityScope::Api {
            user: user.clone(),
            tenant: tenant.clone(),
            api: ApiId::new("chat").unwrap(),
        },
    )
    .unwrap()
}

pub(crate) async fn a_fresh_database_is_migrated_and_empty(store: &impl Storage) {
    assert_eq!(store.tenant(&tenant_id()).await.unwrap(), None);
    assert_eq!(store.user(&user_id()).await.unwrap(), None);
}

pub(crate) async fn a_tenant_round_trips_with_its_key_intact(store: &impl Storage) {
    let created = store.create_tenant(new_tenant()).await.unwrap();
    let read = store.tenant(&tenant_id()).await.unwrap().unwrap();
    assert_eq!(read.row_id, created.row_id);
    assert_eq!(read.key, created.key);
    assert_eq!(read.created_at, created.created_at);
}

pub(crate) async fn the_primary_key_is_an_integer_the_backend_assigns(store: &impl Storage) {
    let first = store.create_tenant(new_tenant()).await.unwrap();
    let second = store
        .create_tenant(NewTenant {
            id: TenantId::new("globex").unwrap(),
            key: TnKey::generate().unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(first.row_id, TenantRowId::new(1));
    assert_eq!(second.row_id, TenantRowId::new(2));
}

pub(crate) async fn a_repeated_tenant_id_conflicts(store: &impl Storage) {
    store.create_tenant(new_tenant()).await.unwrap();
    assert!(matches!(
        store.create_tenant(new_tenant()).await,
        Err(StorageError::Conflict {
            entity: Entity::Tenant,
            ..
        })
    ));
}

pub(crate) async fn a_repeated_user_id_conflicts(store: &impl Storage) {
    store.create_user(new_user()).await.unwrap();
    assert!(matches!(
        store.create_user(new_user()).await,
        Err(StorageError::Conflict {
            entity: Entity::User,
            ..
        })
    ));
}

pub(crate) async fn a_user_round_trips_and_its_passphrase_can_be_replaced(store: &impl Storage) {
    let created = store.create_user(new_user()).await.unwrap();
    assert_eq!(store.user(&user_id()).await.unwrap(), Some(created));
    let replacement =
        PassphraseHash::new("$argon2id$v=19$m=19456,t=2,p=1$b3RoZXI$b3RoZXJoYXNo").unwrap();
    store
        .set_passphrase(&user_id(), &replacement)
        .await
        .unwrap();
    assert_eq!(
        store.user(&user_id()).await.unwrap().unwrap().passphrase,
        replacement
    );
}

pub(crate) async fn the_account_kind_survives_a_round_trip(store: &impl Storage) {
    store
        .create_user(NewUser {
            id: user_id(),
            passphrase: hash(),
            kind: AccountKind::SystemAdministrator,
        })
        .await
        .unwrap();
    assert_eq!(
        store.user(&user_id()).await.unwrap().unwrap().kind,
        AccountKind::SystemAdministrator
    );
}

pub(crate) async fn deleting_what_is_absent_reports_it_missing(store: &impl Storage) {
    assert!(matches!(
        store.delete_tenant(&tenant_id()).await,
        Err(StorageError::NotFound {
            entity: Entity::Tenant,
            ..
        })
    ));
    assert!(matches!(
        store.set_passphrase(&user_id(), &hash()).await,
        Err(StorageError::NotFound {
            entity: Entity::User,
            ..
        })
    ));
}

pub(crate) async fn attaching_twice_replaces_the_standing(store: &impl Storage) {
    tenant_with_user(store).await;
    for standing in [Standing::Owner, Standing::Member] {
        store
            .attach(Membership {
                user: user_id(),
                tenant: tenant_id(),
                standing,
            })
            .await
            .unwrap();
    }
    let held = store.memberships_of_user(&user_id()).await.unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].standing, Standing::Member);
}

pub(crate) async fn attaching_to_a_tenant_that_is_not_there_reports_it_missing(
    store: &impl Storage,
) {
    store.create_user(new_user()).await.unwrap();
    assert!(matches!(
        store
            .attach(Membership {
                user: user_id(),
                tenant: tenant_id(),
                standing: Standing::Member,
            })
            .await,
        Err(StorageError::NotFound {
            entity: Entity::Tenant,
            ..
        })
    ));
}

pub(crate) async fn attaching_a_user_that_is_not_there_reports_it_missing(store: &impl Storage) {
    store.create_tenant(new_tenant()).await.unwrap();
    assert!(matches!(
        store
            .attach(Membership {
                user: user_id(),
                tenant: tenant_id(),
                standing: Standing::Member,
            })
            .await,
        Err(StorageError::NotFound {
            entity: Entity::User,
            ..
        })
    ));
}

pub(crate) async fn detaching_removes_the_membership(store: &impl Storage) {
    tenant_with_user(store).await;
    store
        .attach(Membership {
            user: user_id(),
            tenant: tenant_id(),
            standing: Standing::Member,
        })
        .await
        .unwrap();
    store.detach(&user_id(), &tenant_id()).await.unwrap();
    assert!(
        store
            .memberships_of_user(&user_id())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        store.detach(&user_id(), &tenant_id()).await,
        Err(StorageError::NotFound {
            entity: Entity::Membership,
            ..
        })
    ));
}

pub(crate) async fn deleting_a_user_takes_its_memberships_with_it(store: &impl Storage) {
    tenant_with_user(store).await;
    store
        .attach(Membership {
            user: user_id(),
            tenant: tenant_id(),
            standing: Standing::Member,
        })
        .await
        .unwrap();
    store.delete_user(&user_id()).await.unwrap();
    assert_eq!(store.user(&user_id()).await.unwrap(), None);
    assert!(
        store
            .members_of_tenant(&tenant_id())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        store.delete_user(&user_id()).await,
        Err(StorageError::NotFound {
            entity: Entity::User,
            ..
        })
    ));
}

pub(crate) async fn a_tenant_is_listed_from_both_sides(store: &impl Storage) {
    tenant_with_user(store).await;
    store
        .attach(Membership {
            user: user_id(),
            tenant: tenant_id(),
            standing: Standing::Owner,
        })
        .await
        .unwrap();
    assert_eq!(
        store.memberships_of_user(&user_id()).await.unwrap().len(),
        1
    );
    assert_eq!(
        store.members_of_tenant(&tenant_id()).await.unwrap()[0].user,
        user_id()
    );
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

pub(crate) async fn deleting_a_tenant_takes_its_memberships_and_grants_with_it(
    store: &impl Storage,
) {
    tenant_with_user(store).await;
    store
        .attach(Membership {
            user: user_id(),
            tenant: tenant_id(),
            standing: Standing::Member,
        })
        .await
        .unwrap();
    store
        .grant(&api_grant(&user_id(), &tenant_id(), Capability::ApiAccess))
        .await
        .unwrap();
    store.delete_tenant(&tenant_id()).await.unwrap();
    assert!(
        store
            .memberships_of_user(&user_id())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(store.grants_of(&user_id()).await.unwrap().is_empty());
}

pub(crate) async fn granting_the_same_capability_twice_changes_nothing(store: &impl Storage) {
    tenant_with_user(store).await;
    let grant = api_grant(&user_id(), &tenant_id(), Capability::ApiAccess);
    store.grant(&grant).await.unwrap();
    store.grant(&grant).await.unwrap();
    assert_eq!(store.grants_of(&user_id()).await.unwrap().len(), 1);
}

pub(crate) async fn every_scope_shape_round_trips(store: &impl Storage) {
    tenant_with_user(store).await;
    let user_scoped = Grant::new(
        Capability::TenantMgr,
        CapabilityScope::User { user: user_id() },
    )
    .unwrap();
    let tenant_scoped = Grant::new(
        Capability::UserMgr,
        CapabilityScope::Tenant {
            user: user_id(),
            tenant: tenant_id(),
        },
    )
    .unwrap();
    let api_scoped = api_grant(&user_id(), &tenant_id(), Capability::ApiAccess);
    for grant in [&user_scoped, &tenant_scoped, &api_scoped] {
        store.grant(grant).await.unwrap();
    }
    let held = store.grants_of(&user_id()).await.unwrap();
    assert_eq!(held.len(), 3);
    assert!(held.holds(Capability::TenantMgr, user_scoped.scope()));
    assert!(held.holds(Capability::UserMgr, tenant_scoped.scope()));
    assert!(held.holds(Capability::ApiAccess, api_scoped.scope()));
}

pub(crate) async fn grants_in_a_tenant_exclude_the_user_scoped_ones(store: &impl Storage) {
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
    let api_scoped = api_grant(&user_id(), &tenant_id(), Capability::ApiAccess);
    store.grant(&api_scoped).await.unwrap();
    let held = store
        .grants_in_tenant(&user_id(), &tenant_id())
        .await
        .unwrap();
    assert_eq!(held.len(), 1);
    assert!(held.holds(Capability::ApiAccess, api_scoped.scope()));
}

pub(crate) async fn revoking_what_is_not_held_reports_it_missing(store: &impl Storage) {
    tenant_with_user(store).await;
    assert!(matches!(
        store
            .revoke(&api_grant(&user_id(), &tenant_id(), Capability::ApiAccess))
            .await,
        Err(StorageError::NotFound {
            entity: Entity::Grant,
            ..
        })
    ));
}

pub(crate) async fn revoking_removes_only_the_named_grant(store: &impl Storage) {
    tenant_with_user(store).await;
    let access = api_grant(&user_id(), &tenant_id(), Capability::ApiAccess);
    let limits = api_grant(&user_id(), &tenant_id(), Capability::LimitMgr);
    store.grant(&access).await.unwrap();
    store.grant(&limits).await.unwrap();
    store.revoke(&limits).await.unwrap();
    let held = store.grants_of(&user_id()).await.unwrap();
    assert_eq!(held.len(), 1);
    assert!(held.holds(Capability::ApiAccess, access.scope()));
}
