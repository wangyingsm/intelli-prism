use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use ip_auth::{Passphrase, PassphraseHasher};
use ip_core::{Capability, CapabilityScope, PassphraseHash, TenantId, Timestamp, UserId};
use ip_storage::{
    AccountKind, IdentityDialect, MemberAddBegun, MemberAddTransactional, MemberAddTxn, Membership,
    NewUser, Standing, Storage, StorageError, User, UserWithMembership,
};

use super::{standing_name, store_refusal};
use crate::manage::Manager;
use crate::state::{AppState, Stores};

/// Every endpoint that manages user accounts, under the prefix the router nests them at.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", post(create))
        .route("/{user}", get(read).delete(remove))
}

/// What creating a user is asked for.
#[derive(Debug, serde::Deserialize)]
pub struct NewUserRequest {
    /// The account to create.
    user: String,
    /// The tenant it starts out in.
    tenant: String,
    /// What it will log in with.
    passphrase: String,
}

/// A user as the management api shows it.
#[derive(Debug, serde::Serialize)]
pub struct UserView {
    /// The account.
    pub user: String,
    /// Whether it is the system administrator or an ordinary account.
    pub kind: &'static str,
    /// When it was created, in seconds since the unix epoch.
    pub created_at: Timestamp,
    /// The tenants the caller may see it in, which is not always all of them.
    pub tenants: Vec<Attachment>,
}

/// One tenant an account is in.
#[derive(Debug, serde::Serialize)]
pub struct Attachment {
    /// Which tenant.
    pub tenant: String,
    /// What the account is inside it.
    pub standing: &'static str,
}

/// Everything the two writes need, so the service takes one argument.
pub struct NewMember {
    /// The account to create.
    pub user: UserId,
    /// The tenant to attach it to.
    pub tenant: TenantId,
    /// Its verifier.
    pub passphrase: PassphraseHash,
}

/// Creates an account and attaches it to one tenant.
///
/// An account created here is always a member: an owner comes into being with its tenant,
/// and a system administrator only from the command line.
async fn create(
    State(state): State<AppState>,
    manager: Manager,
    Json(request): Json<NewUserRequest>,
) -> Response<Body> {
    let store = state.store().as_ref();
    let (Ok(user), Ok(tenant)) = (UserId::new(&request.user), TenantId::new(&request.tenant))
    else {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    };
    match manager
        .allows(
            store,
            Capability::UserMgr,
            &user_mgr_scope(&manager, &tenant),
        )
        .await
    {
        Ok(true) => {}
        Ok(false) => return StatusCode::FORBIDDEN.into_response(),
        Err(error) => return store_refusal(error),
    }
    let Ok(passphrase) = Passphrase::new(&request.passphrase) else {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    };
    let Ok(passphrase) = PassphraseHasher::new().hash(&passphrase) else {
        tracing::error!("could not prepare an account for creation");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    let new = NewMember {
        user,
        tenant,
        passphrase,
    };
    match add_member(state.stores(), new).await {
        Ok(added) => (StatusCode::CREATED, Json(UserView::from(added))).into_response(),
        Err(error) => store_refusal(error),
    }
}

/// Shows one account, with the tenants the caller manages it in.
///
/// An account the caller has no business with is answered exactly like one that is not
/// there, so this endpoint says nothing about who exists.
async fn read(
    State(state): State<AppState>,
    manager: Manager,
    Path(user): Path<String>,
) -> Response<Body> {
    let store = state.store().as_ref();
    let Ok(user) = UserId::new(&user) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (stored, seen) = match found(&manager, store, &user).await {
        Ok(Some(found)) => found,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return store_refusal(error),
    };
    Json(UserView::of(stored, seen.shown)).into_response()
}

/// Removes an account, and with it every attachment and grant that named it.
///
/// Only a caller that sees the whole account may remove it: managing one of the tenants it
/// is in is not enough to end what it does in the others.
async fn remove(
    State(state): State<AppState>,
    manager: Manager,
    Path(user): Path<String>,
) -> Response<Body> {
    let store = state.store().as_ref();
    let Ok(user) = UserId::new(&user) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if manager.user() == &user {
        return (StatusCode::FORBIDDEN, "an account does not remove itself").into_response();
    }
    let (stored, seen) = match found(&manager, store, &user).await {
        Ok(Some(found)) => found,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return store_refusal(error),
    };
    if stored.kind == AccountKind::SystemAdministrator {
        return (
            StatusCode::FORBIDDEN,
            "a system administrator is not removed over the api",
        )
            .into_response();
    }
    if seen.shown.len() != seen.attached {
        return (
            StatusCode::FORBIDDEN,
            "the account is in tenants the caller does not manage",
        )
            .into_response();
    }
    if let Some(owned) = seen.shown.iter().find(|m| m.standing == Standing::Owner) {
        return (
            StatusCode::CONFLICT,
            format!("the account owns {}, which must go first", owned.tenant),
        )
            .into_response();
    }
    match store.delete_user(&user).await {
        Ok(()) => {
            state.feed().after_change().await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => store_refusal(error),
    }
}

/// What one caller may be shown of one account.
struct Seen {
    /// The attachments it may see.
    shown: Vec<Membership>,
    /// How many the account has in all, which is what tells a full view from a partial one.
    attached: usize,
}

/// The account and what the caller may see of it, or nothing when it may see none of it.
async fn found(
    manager: &Manager,
    store: &dyn Storage,
    user: &UserId,
) -> Result<Option<(User, Seen)>, StorageError> {
    let Some(stored) = store.user(user).await? else {
        return Ok(None);
    };
    let attached = store.memberships_of_user(user).await?;
    if manager.is_system_administrator() || manager.user() == user {
        let seen = Seen {
            attached: attached.len(),
            shown: attached,
        };
        return Ok(Some((stored, seen)));
    }
    let mut shown = Vec::new();
    for membership in &attached {
        if manager
            .allows(
                store,
                Capability::UserMgr,
                &user_mgr_scope(manager, &membership.tenant),
            )
            .await?
        {
            shown.push(membership.clone());
        }
    }
    match shown.is_empty() {
        true => Ok(None),
        false => Ok(Some((
            stored,
            Seen {
                shown,
                attached: attached.len(),
            },
        ))),
    }
}

/// `UserMgr` is held inside one tenant, so every question about it names one.
fn user_mgr_scope(manager: &Manager, tenant: &TenantId) -> CapabilityScope {
    CapabilityScope::Tenant {
        user: manager.user().clone(),
        tenant: tenant.clone(),
    }
}

/// Runs the creation against whichever backend was opened.
async fn add_member(stores: &Stores, new: NewMember) -> Result<UserWithMembership, StorageError> {
    match stores {
        #[cfg(any(feature = "standalone-storage", test))]
        Stores::Sqlite(store) => in_one_transaction(store.begin_member_add().await?, new).await,
        #[cfg(feature = "fast-storage")]
        Stores::Postgres(store) => in_one_transaction(store.begin_member_add().await?, new).await,
    }
}

/// The two writes, in the order the typestate allows them in.
async fn in_one_transaction<DB: IdentityDialect>(
    transaction: MemberAddTxn<DB, MemberAddBegun>,
    new: NewMember,
) -> Result<UserWithMembership, StorageError> {
    transaction
        .create_user(NewUser {
            id: new.user,
            passphrase: new.passphrase,
            kind: AccountKind::Regular,
        })
        .await?
        .attach(new.tenant, Standing::Member)
        .await?
        .commit()
        .await
}

/// The name an account kind is shown under.
fn kind_name(kind: AccountKind) -> &'static str {
    match kind {
        AccountKind::SystemAdministrator => "system_administrator",
        AccountKind::Regular => "regular",
    }
}

impl UserView {
    /// One account with the attachments the caller may see.
    fn of(user: User, shown: Vec<Membership>) -> Self {
        Self {
            user: user.id.to_string(),
            kind: kind_name(user.kind),
            created_at: user.created_at,
            tenants: shown.into_iter().map(Attachment::from).collect(),
        }
    }
}

impl From<UserWithMembership> for UserView {
    fn from(added: UserWithMembership) -> Self {
        Self::of(added.user, vec![added.membership])
    }
}

impl From<Membership> for Attachment {
    fn from(membership: Membership) -> Self {
        Self {
            tenant: membership.tenant.to_string(),
            standing: standing_name(membership.standing),
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::http::StatusCode;
    use ip_core::TnKey;
    use ip_storage::{MembershipStore, NewTenant, SqliteStore, TenantStore, UserStore};
    use tower::ServiceExt;

    use super::super::harness::{PASSPHRASE, body_of, cookie_of, hashed, request, state_over};
    use super::*;

    fn id(raw: &str) -> UserId {
        UserId::new(raw).unwrap()
    }

    fn tenant(raw: &str) -> TenantId {
        TenantId::new(raw).unwrap()
    }

    /// The server's own router over two tenants: acme, owned by alice with bob and carol in
    /// it, and globex, owned by dave with carol in it. Two system administrators stand
    /// outside both.
    async fn fixture() -> (Router, AppState) {
        let store = SqliteStore::in_memory().await.unwrap();
        for name in ["acme", "globex"] {
            store
                .create_tenant(NewTenant {
                    id: tenant(name),
                    key: TnKey::generate().unwrap(),
                })
                .await
                .unwrap();
        }
        for (user, kind) in [
            ("root", AccountKind::SystemAdministrator),
            ("spare", AccountKind::SystemAdministrator),
            ("alice", AccountKind::Regular),
            ("bob", AccountKind::Regular),
            ("carol", AccountKind::Regular),
            ("dave", AccountKind::Regular),
        ] {
            store
                .create_user(NewUser {
                    id: id(user),
                    passphrase: hashed(),
                    kind,
                })
                .await
                .unwrap();
        }
        for (user, in_tenant, standing) in [
            ("alice", "acme", Standing::Owner),
            ("bob", "acme", Standing::Member),
            ("carol", "acme", Standing::Member),
            ("dave", "globex", Standing::Owner),
            ("carol", "globex", Standing::Member),
        ] {
            store
                .attach(Membership {
                    user: id(user),
                    tenant: tenant(in_tenant),
                    standing,
                })
                .await
                .unwrap();
        }

        let state = state_over(store);
        (crate::routes::router(state.clone()), state)
    }

    fn creating(user: &str, in_tenant: &str, passphrase: &str) -> String {
        format!(r#"{{"user":"{user}","tenant":"{in_tenant}","passphrase":"{passphrase}"}}"#)
    }

    #[tokio::test]
    async fn a_tenant_owner_creates_a_member_of_its_tenant() {
        let (router, state) = fixture().await;
        let response = router
            .oneshot(request(
                "POST",
                "/_ip/users",
                &cookie_of(&state, &id("alice")).await,
                Some(&creating("erin", "acme", PASSPHRASE)),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body: serde_json::Value = serde_json::from_str(&body_of(response).await).unwrap();
        assert_eq!(body["user"], "erin");
        assert_eq!(body["tenants"][0]["tenant"], "acme");
        assert_eq!(body["tenants"][0]["standing"], "member");
        assert!(body["created_at"].is_i64(), "{body}");

        let store = state.store();
        assert_eq!(
            store.user(&id("erin")).await.unwrap().unwrap().kind,
            AccountKind::Regular
        );
        assert_eq!(
            store
                .membership(&id("erin"), &tenant("acme"))
                .await
                .unwrap()
                .unwrap()
                .standing,
            Standing::Member
        );
    }

    #[tokio::test]
    async fn a_member_of_a_tenant_creates_nobody_in_it() {
        let (router, state) = fixture().await;
        let response = router
            .oneshot(request(
                "POST",
                "/_ip/users",
                &cookie_of(&state, &id("bob")).await,
                Some(&creating("erin", "acme", PASSPHRASE)),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(state.store().user(&id("erin")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_owner_creates_nobody_in_a_tenant_it_does_not_own() {
        let (router, state) = fixture().await;
        let response = router
            .oneshot(request(
                "POST",
                "/_ip/users",
                &cookie_of(&state, &id("alice")).await,
                Some(&creating("erin", "globex", PASSPHRASE)),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(state.store().user(&id("erin")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn creating_an_account_that_exists_conflicts() {
        let (router, state) = fixture().await;
        let response = router
            .oneshot(request(
                "POST",
                "/_ip/users",
                &cookie_of(&state, &id("alice")).await,
                Some(&creating("bob", "acme", PASSPHRASE)),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn a_body_the_types_refuse_creates_nothing() {
        let (router, state) = fixture().await;
        let cookie = cookie_of(&state, &id("alice")).await;
        for body in [
            creating("not a user id", "acme", PASSPHRASE),
            creating("erin", "not a tenant id", PASSPHRASE),
            creating("erin", "acme", "short"),
        ] {
            let response = router
                .clone()
                .oneshot(request("POST", "/_ip/users", &cookie, Some(&body)))
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
    async fn an_account_sees_every_tenant_it_is_in() {
        let (router, state) = fixture().await;
        let response = router
            .oneshot(request(
                "GET",
                "/_ip/users/carol",
                &cookie_of(&state, &id("carol")).await,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_of(response).await;
        assert!(body.contains("acme") && body.contains("globex"), "{body}");
    }

    #[tokio::test]
    async fn a_manager_sees_an_account_only_in_the_tenants_it_manages() {
        let (router, state) = fixture().await;
        let response = router
            .oneshot(request(
                "GET",
                "/_ip/users/carol",
                &cookie_of(&state, &id("alice")).await,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_of(response).await;
        assert!(body.contains("acme"), "{body}");
        assert!(!body.contains("globex"), "{body}");
    }

    #[tokio::test]
    async fn the_administrator_sees_every_tenant_an_account_is_in() {
        let (router, state) = fixture().await;
        let response = router
            .oneshot(request(
                "GET",
                "/_ip/users/carol",
                &cookie_of(&state, &id("root")).await,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_of(response).await;
        assert!(body.contains("acme") && body.contains("globex"), "{body}");
    }

    #[tokio::test]
    async fn an_account_the_caller_has_no_business_with_is_answered_as_absent() {
        let (router, state) = fixture().await;
        for (caller, target) in [("bob", "carol"), ("alice", "dave"), ("root", "nobody")] {
            let response = router
                .clone()
                .oneshot(request(
                    "GET",
                    &format!("/_ip/users/{target}"),
                    &cookie_of(&state, &id(caller)).await,
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{caller} reading {target}"
            );
        }
    }

    #[tokio::test]
    async fn a_tenant_owner_removes_a_member_of_its_tenant() {
        let (router, state) = fixture().await;
        let response = router
            .oneshot(request(
                "DELETE",
                "/_ip/users/bob",
                &cookie_of(&state, &id("alice")).await,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(state.store().user(&id("bob")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_account_in_a_tenant_the_caller_does_not_manage_stays() {
        let (router, state) = fixture().await;
        let response = router
            .oneshot(request(
                "DELETE",
                "/_ip/users/carol",
                &cookie_of(&state, &id("alice")).await,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(state.store().user(&id("carol")).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn an_owner_is_not_removed_while_its_tenant_stands() {
        let (router, state) = fixture().await;
        let response = router
            .oneshot(request(
                "DELETE",
                "/_ip/users/alice",
                &cookie_of(&state, &id("root")).await,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(state.store().user(&id("alice")).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_system_administrator_is_not_removed_over_the_api() {
        let (router, state) = fixture().await;
        let response = router
            .oneshot(request(
                "DELETE",
                "/_ip/users/spare",
                &cookie_of(&state, &id("root")).await,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(state.store().user(&id("spare")).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn an_account_does_not_remove_itself() {
        let (router, state) = fixture().await;
        let response = router
            .oneshot(request(
                "DELETE",
                "/_ip/users/root",
                &cookie_of(&state, &id("root")).await,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(state.store().user(&id("root")).await.unwrap().is_some());
    }
}
