use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use ip_auth::{Passphrase, PassphraseHasher};
use ip_core::{Capability, CapabilityScope, PassphraseHash, TenantId, Timestamp, TnKey, UserId};
use ip_storage::{
    AccountKind, IdentityDialect, NewTenant, NewUser, Standing, Storage, StorageError, Tenant,
    TenantWithOwner, UserCreateBegun, UserCreateTransactional, UserCreateTxn,
};

use super::store_refusal;
use crate::manage::Manager;
use crate::state::{AppState, Stores};

/// Every endpoint that manages tenants, under the prefix the router nests them at.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", post(create))
        .route("/{tenant}", get(read).delete(remove))
}

/// What creating a tenant is asked for.
#[derive(Debug, serde::Deserialize)]
pub struct NewTenantRequest {
    /// The tenant to create.
    tenant: String,
    /// The account that will own it.
    owner: String,
    /// What that account will log in with.
    passphrase: String,
}

/// A tenant as the management api shows it.
///
/// Its key is never here: a key leaves the system through the reveal endpoint alone, which
/// asks for the passphrase again.
#[derive(Debug, serde::Serialize)]
pub struct TenantView {
    /// The tenant.
    pub tenant: String,
    /// When it was created, in seconds since the unix epoch.
    pub created_at: Timestamp,
}

/// A tenant and the account created to own it.
#[derive(Debug, serde::Serialize)]
pub struct CreatedTenant {
    /// The tenant.
    pub tenant: String,
    /// The account that owns it.
    pub owner: String,
    /// When the tenant was created, in seconds since the unix epoch.
    pub created_at: Timestamp,
}

/// Everything the three writes need, so the service takes one argument.
pub struct NewTenantWithOwner {
    /// The tenant to create.
    pub tenant: TenantId,
    /// Its root secret, drawn here and stored with it.
    pub key: TnKey,
    /// The account that will own it.
    pub owner: UserId,
    /// That account's verifier.
    pub passphrase: PassphraseHash,
}

/// Creates a tenant, the account that owns it, and their attachment.
///
/// One endpoint, three writes, one transaction: the typestate is what keeps the writes in
/// their only workable order, and the transaction is what keeps a failure halfway through
/// from leaving a tenant nobody owns.
async fn create(
    State(state): State<AppState>,
    manager: Manager,
    Json(request): Json<NewTenantRequest>,
) -> Response<Body> {
    if let Some(refusal) = tenant_mgr_refusal(&manager, state.store().as_ref()).await {
        return refusal;
    }
    let (Ok(tenant), Ok(owner)) = (TenantId::new(&request.tenant), UserId::new(&request.owner))
    else {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    };
    let Ok(passphrase) = Passphrase::new(&request.passphrase) else {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    };
    let (Ok(passphrase), Ok(key)) = (PassphraseHasher::new().hash(&passphrase), TnKey::generate())
    else {
        tracing::error!("could not prepare a tenant for creation");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    let new = NewTenantWithOwner {
        tenant,
        key,
        owner,
        passphrase,
    };
    match create_with_owner(state.stores(), new).await {
        Ok(created) => (StatusCode::CREATED, Json(CreatedTenant::from(created))).into_response(),
        Err(error) => store_refusal(error),
    }
}

/// Shows one tenant to a caller that manages tenants or is attached to it.
async fn read(
    State(state): State<AppState>,
    manager: Manager,
    Path(tenant): Path<String>,
) -> Response<Body> {
    let store = state.store().as_ref();
    let Ok(tenant) = TenantId::new(&tenant) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match reaches(&manager, store, &tenant).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::FORBIDDEN.into_response(),
        Err(error) => return store_refusal(error),
    }
    match store.tenant(&tenant).await {
        Ok(Some(tenant)) => Json(TenantView::from(tenant)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => store_refusal(error),
    }
}

/// Removes a tenant, and with it every attachment and grant that named it, which the
/// schema cascades away.
async fn remove(
    State(state): State<AppState>,
    manager: Manager,
    Path(tenant): Path<String>,
) -> Response<Body> {
    let store = state.store().as_ref();
    if let Some(refusal) = tenant_mgr_refusal(&manager, store).await {
        return refusal;
    }
    let Ok(tenant) = TenantId::new(&tenant) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match store.delete_tenant(&tenant).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => store_refusal(error),
    }
}

/// What to answer a caller that does not manage tenants, and nothing when it does.
async fn tenant_mgr_refusal(manager: &Manager, store: &dyn Storage) -> Option<Response<Body>> {
    match manager
        .allows(store, Capability::TenantMgr, &tenant_mgr_scope(manager))
        .await
    {
        Ok(true) => None,
        Ok(false) => Some(StatusCode::FORBIDDEN.into_response()),
        Err(error) => Some(store_refusal(error)),
    }
}

/// Whether the caller may see one tenant at all, which being inside it is enough for.
async fn reaches(
    manager: &Manager,
    store: &dyn Storage,
    tenant: &TenantId,
) -> Result<bool, StorageError> {
    if manager
        .allows(store, Capability::TenantMgr, &tenant_mgr_scope(manager))
        .await?
    {
        return Ok(true);
    }
    Ok(store.membership(manager.user(), tenant).await?.is_some())
}

/// `TenantMgr` is held against an account rather than inside a tenant, so this is the only
/// scope it is ever asked for at.
fn tenant_mgr_scope(manager: &Manager) -> CapabilityScope {
    CapabilityScope::User {
        user: manager.user().clone(),
    }
}

/// Runs the creation against whichever backend was opened.
async fn create_with_owner(
    stores: &Stores,
    new: NewTenantWithOwner,
) -> Result<TenantWithOwner, StorageError> {
    match stores {
        #[cfg(any(feature = "standalone-storage", test))]
        Stores::Sqlite(store) => in_one_transaction(store.begin_user_create().await?, new).await,
        #[cfg(feature = "fast-storage")]
        Stores::Postgres(store) => in_one_transaction(store.begin_user_create().await?, new).await,
    }
}

/// The three writes, in the order the typestate allows them in.
async fn in_one_transaction<DB: IdentityDialect>(
    transaction: UserCreateTxn<DB, UserCreateBegun>,
    new: NewTenantWithOwner,
) -> Result<TenantWithOwner, StorageError> {
    transaction
        .create_tenant(NewTenant {
            id: new.tenant,
            key: new.key,
        })
        .await?
        .create_user(NewUser {
            id: new.owner,
            passphrase: new.passphrase,
            kind: AccountKind::Regular,
        })
        .await?
        .attach(Standing::Owner)
        .await?
        .commit()
        .await
}

impl From<Tenant> for TenantView {
    fn from(tenant: Tenant) -> Self {
        Self {
            tenant: tenant.id.to_string(),
            created_at: tenant.created_at,
        }
    }
}

impl From<TenantWithOwner> for CreatedTenant {
    fn from(created: TenantWithOwner) -> Self {
        Self {
            tenant: created.tenant.id.to_string(),
            owner: created.owner.id.to_string(),
            created_at: created.tenant.created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use ip_core::Grant;
    use ip_storage::{Membership, MembershipStore, SqliteStore, TenantStore, UserStore};
    use tower::ServiceExt;

    use super::super::harness::{PASSPHRASE, body_of, cookie_of, hashed, request, state_over};
    use super::*;

    fn tenant_id() -> TenantId {
        TenantId::new("acme").unwrap()
    }

    fn admin_id() -> UserId {
        UserId::new("root").unwrap()
    }

    fn member_id() -> UserId {
        UserId::new("alice").unwrap()
    }

    /// The server's own router over a store holding one tenant, its member, and a system
    /// administrator that is attached to nothing, so the tests cross the prefix these
    /// endpoints are nested at.
    async fn fixture() -> (Router, AppState) {
        let store = SqliteStore::in_memory().await.unwrap();
        store
            .create_tenant(NewTenant {
                id: tenant_id(),
                key: TnKey::generate().unwrap(),
            })
            .await
            .unwrap();
        for (id, kind) in [
            (admin_id(), AccountKind::SystemAdministrator),
            (member_id(), AccountKind::Regular),
        ] {
            store
                .create_user(NewUser {
                    id,
                    passphrase: hashed(),
                    kind,
                })
                .await
                .unwrap();
        }
        store
            .attach(Membership {
                user: member_id(),
                tenant: tenant_id(),
                standing: Standing::Member,
            })
            .await
            .unwrap();

        let state = state_over(store);
        (crate::routes::router(state.clone()), state)
    }

    fn creating(tenant: &str, owner: &str, passphrase: &str) -> String {
        format!(r#"{{"tenant":"{tenant}","owner":"{owner}","passphrase":"{passphrase}"}}"#)
    }

    #[tokio::test]
    async fn the_administrator_creates_a_tenant_with_the_account_that_owns_it() {
        let (router, state) = fixture().await;
        let cookie = cookie_of(&state, &admin_id()).await;
        let response = router
            .oneshot(request(
                "POST",
                "/_ip/tenants",
                &cookie,
                Some(&creating("globex", "hank", PASSPHRASE)),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body: serde_json::Value = serde_json::from_str(&body_of(response).await).unwrap();
        assert_eq!(body["tenant"], "globex");
        assert_eq!(body["owner"], "hank");
        assert!(body["created_at"].is_i64(), "{body}");

        let store = state.store();
        let tenant = TenantId::new("globex").unwrap();
        let owner = UserId::new("hank").unwrap();
        assert!(store.tenant(&tenant).await.unwrap().is_some());
        assert_eq!(
            store.user(&owner).await.unwrap().unwrap().kind,
            AccountKind::Regular
        );
        assert_eq!(
            store
                .membership(&owner, &tenant)
                .await
                .unwrap()
                .unwrap()
                .standing,
            Standing::Owner
        );
    }

    #[tokio::test]
    async fn the_tenant_key_is_in_nothing_the_endpoints_answer() {
        let (router, state) = fixture().await;
        let cookie = cookie_of(&state, &admin_id()).await;
        let created = body_of(
            router
                .clone()
                .oneshot(request(
                    "POST",
                    "/_ip/tenants",
                    &cookie,
                    Some(&creating("globex", "hank", PASSPHRASE)),
                ))
                .await
                .unwrap(),
        )
        .await;
        let read = body_of(
            router
                .oneshot(request("GET", "/_ip/tenants/globex", &cookie, None))
                .await
                .unwrap(),
        )
        .await;

        let key = state
            .store()
            .tenant(&TenantId::new("globex").unwrap())
            .await
            .unwrap()
            .unwrap()
            .key
            .to_hex();
        for body in [created, read] {
            assert!(!body.contains(&key), "{body}");
        }
    }

    #[tokio::test]
    async fn a_caller_holding_no_capability_creates_nothing() {
        let (router, state) = fixture().await;
        let cookie = cookie_of(&state, &member_id()).await;
        let response = router
            .oneshot(request(
                "POST",
                "/_ip/tenants",
                &cookie,
                Some(&creating("globex", "hank", PASSPHRASE)),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            state
                .store()
                .tenant(&TenantId::new("globex").unwrap())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn creating_a_tenant_that_exists_conflicts() {
        let (router, state) = fixture().await;
        let cookie = cookie_of(&state, &admin_id()).await;
        let response = router
            .oneshot(request(
                "POST",
                "/_ip/tenants",
                &cookie,
                Some(&creating("acme", "hank", PASSPHRASE)),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn a_tenant_whose_owner_cannot_be_written_is_not_written_either() {
        let (router, state) = fixture().await;
        let cookie = cookie_of(&state, &admin_id()).await;
        let response = router
            .oneshot(request(
                "POST",
                "/_ip/tenants",
                &cookie,
                Some(&creating("globex", member_id().as_str(), PASSPHRASE)),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(
            state
                .store()
                .tenant(&TenantId::new("globex").unwrap())
                .await
                .unwrap()
                .is_none(),
            "the tenant outlived the transaction that failed"
        );
    }

    #[tokio::test]
    async fn a_body_the_types_refuse_creates_nothing() {
        let (router, state) = fixture().await;
        let cookie = cookie_of(&state, &admin_id()).await;
        for body in [
            creating("not a tenant id", "hank", PASSPHRASE),
            creating("globex", "not a user id", PASSPHRASE),
            creating("globex", "hank", "short"),
        ] {
            let response = router
                .clone()
                .oneshot(request("POST", "/_ip/tenants", &cookie, Some(&body)))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{body}"
            );
        }
    }

    #[tokio::test]
    async fn a_member_reads_the_tenant_it_is_in_and_no_other() {
        let (router, state) = fixture().await;
        let cookie = cookie_of(&state, &member_id()).await;
        state
            .store()
            .create_tenant(NewTenant {
                id: TenantId::new("globex").unwrap(),
                key: TnKey::generate().unwrap(),
            })
            .await
            .unwrap();

        let mine = router
            .clone()
            .oneshot(request("GET", "/_ip/tenants/acme", &cookie, None))
            .await
            .unwrap();
        assert_eq!(mine.status(), StatusCode::OK);
        assert!(body_of(mine).await.contains("acme"));

        let theirs = router
            .oneshot(request("GET", "/_ip/tenants/globex", &cookie, None))
            .await
            .unwrap();
        assert_eq!(theirs.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn the_administrator_reads_a_tenant_it_is_not_in_and_is_told_when_there_is_none() {
        let (router, state) = fixture().await;
        let cookie = cookie_of(&state, &admin_id()).await;
        let found = router
            .clone()
            .oneshot(request("GET", "/_ip/tenants/acme", &cookie, None))
            .await
            .unwrap();
        assert_eq!(found.status(), StatusCode::OK);

        let absent = router
            .oneshot(request("GET", "/_ip/tenants/globex", &cookie, None))
            .await
            .unwrap();
        assert_eq!(absent.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn only_a_caller_that_manages_tenants_removes_one() {
        let (router, state) = fixture().await;
        let refused = router
            .clone()
            .oneshot(request(
                "DELETE",
                "/_ip/tenants/acme",
                &cookie_of(&state, &member_id()).await,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        assert!(state.store().tenant(&tenant_id()).await.unwrap().is_some());

        let cookie = cookie_of(&state, &admin_id()).await;
        let removed = router
            .clone()
            .oneshot(request("DELETE", "/_ip/tenants/acme", &cookie, None))
            .await
            .unwrap();
        assert_eq!(removed.status(), StatusCode::NO_CONTENT);
        assert!(state.store().tenant(&tenant_id()).await.unwrap().is_none());
        assert!(
            state
                .store()
                .membership(&member_id(), &tenant_id())
                .await
                .unwrap()
                .is_none(),
            "the attachment outlived the tenant"
        );

        let again = router
            .oneshot(request("DELETE", "/_ip/tenants/acme", &cookie, None))
            .await
            .unwrap();
        assert_eq!(again.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_granted_account_manages_tenants_without_being_an_administrator() {
        let (router, state) = fixture().await;
        state
            .store()
            .grant(
                &Grant::new(
                    Capability::TenantMgr,
                    CapabilityScope::User { user: member_id() },
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let response = router
            .oneshot(request(
                "POST",
                "/_ip/tenants",
                &cookie_of(&state, &member_id()).await,
                Some(&creating("globex", "hank", PASSPHRASE)),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
    }
}
