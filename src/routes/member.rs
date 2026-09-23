use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use ip_core::{Capability, CapabilityScope, TenantId, Timestamp, UserId};
use ip_storage::{
    AccountKind, IdentityDialect, Listed, MemberRemoveBegun, MemberRemoveTransactional,
    MemberRemoveTxn, MemberRemoved, Membership, Standing, StorageError,
};

use super::{Paged, standing_name, store_refusal};
use crate::manage::Manager;
use crate::state::{AppState, Stores};

/// Every endpoint that manages who is in a tenant, under the prefix the router nests them at.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list))
        .route("/{user}", put(attach).delete(detach))
}

/// One account in a tenant.
#[derive(Debug, serde::Serialize)]
pub struct MemberView {
    /// The account.
    pub user: String,
    /// What it is inside the tenant.
    pub standing: &'static str,
    /// When it joined, in seconds since the unix epoch, as a list reads it back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<Timestamp>,
}

/// An account that left a tenant, and how much it held there.
#[derive(Debug, serde::Serialize)]
pub struct DetachedView {
    /// The account.
    pub user: String,
    /// The tenant it left.
    pub tenant: String,
    /// How many grants it held inside that tenant, all now revoked.
    pub revoked: u64,
}

/// Everyone in a tenant, newest first.
async fn list(
    State(state): State<AppState>,
    manager: Manager,
    Path(tenant): Path<String>,
    Paged(page): Paged,
) -> Response<Body> {
    let Ok(tenant) = TenantId::new(&tenant) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(refusal) = user_mgr_refusal(&state, &manager, &tenant).await {
        return refusal;
    }
    match state.store().list_members(&tenant, page).await {
        Ok(members) => Json(
            members
                .into_iter()
                .map(MemberView::from)
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(error) => store_refusal(error),
    }
}

/// Puts an account that already exists into a tenant, as a member.
///
/// Answers 201 when it joined and 204 when it was already in. An owner is never touched:
/// writing a member over it would take the tenant from it.
async fn attach(
    State(state): State<AppState>,
    manager: Manager,
    Path((tenant, user)): Path<(String, String)>,
) -> Response<Body> {
    let (Ok(tenant), Ok(user)) = (TenantId::new(&tenant), UserId::new(&user)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(refusal) = user_mgr_refusal(&state, &manager, &tenant).await {
        return refusal;
    }
    let store = state.store();
    match store.user(&user).await {
        Ok(Some(account)) if account.kind == AccountKind::SystemAdministrator => {
            return (
                StatusCode::CONFLICT,
                "a system administrator holds every tenant already and joins none",
            )
                .into_response();
        }
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return store_refusal(error),
    }
    match store.membership(&user, &tenant).await {
        Ok(Some(held)) if held.standing == Standing::Owner => {
            return (StatusCode::CONFLICT, "the account owns this tenant").into_response();
        }
        Ok(Some(_)) => return StatusCode::NO_CONTENT.into_response(),
        Ok(None) => {}
        Err(error) => return store_refusal(error),
    }
    let membership = Membership {
        user,
        tenant,
        standing: Standing::Member,
    };
    match store.attach(membership.clone()).await {
        Ok(()) => (StatusCode::CREATED, Json(MemberView::from(membership))).into_response(),
        Err(error) => store_refusal(error),
    }
}

/// Takes an account out of a tenant, and every grant it held inside it with it.
///
/// A grant left behind would come back the moment the account were put in again, so the two
/// go together or not at all. An owner is not taken out: its tenant goes first.
async fn detach(
    State(state): State<AppState>,
    manager: Manager,
    Path((tenant, user)): Path<(String, String)>,
) -> Response<Body> {
    let (Ok(tenant), Ok(user)) = (TenantId::new(&tenant), UserId::new(&user)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(refusal) = user_mgr_refusal(&state, &manager, &tenant).await {
        return refusal;
    }
    match state.store().membership(&user, &tenant).await {
        Ok(Some(held)) if held.standing == Standing::Owner => {
            return (StatusCode::CONFLICT, "an owner leaves only with its tenant").into_response();
        }
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return store_refusal(error),
    }
    match remove_member(state.stores(), user, tenant).await {
        Ok(removed) => Json(DetachedView::from(removed)).into_response(),
        Err(error) => store_refusal(error),
    }
}

/// What to answer a caller that does not manage users in this tenant, and nothing when it
/// does.
async fn user_mgr_refusal(
    state: &AppState,
    manager: &Manager,
    tenant: &TenantId,
) -> Option<Response<Body>> {
    let scope = CapabilityScope::Tenant {
        user: manager.user().clone(),
        tenant: tenant.clone(),
    };
    match manager
        .allows(state.store().as_ref(), Capability::UserMgr, &scope)
        .await
    {
        Ok(true) => None,
        Ok(false) => Some(StatusCode::FORBIDDEN.into_response()),
        Err(error) => Some(store_refusal(error)),
    }
}

/// Runs the removal against whichever backend was opened.
async fn remove_member(
    stores: &Stores,
    user: UserId,
    tenant: TenantId,
) -> Result<MemberRemoved, StorageError> {
    match stores {
        #[cfg(any(feature = "standalone-storage", test))]
        Stores::Sqlite(store) => {
            in_one_transaction(store.begin_member_remove().await?, user, tenant).await
        }
        #[cfg(feature = "fast-storage")]
        Stores::Postgres(store) => {
            in_one_transaction(store.begin_member_remove().await?, user, tenant).await
        }
    }
}

/// The two writes, in the order the typestate allows them in.
async fn in_one_transaction<DB: IdentityDialect>(
    transaction: MemberRemoveTxn<DB, MemberRemoveBegun>,
    user: UserId,
    tenant: TenantId,
) -> Result<MemberRemoved, StorageError> {
    transaction
        .detach(user, tenant)
        .await?
        .revoke_grants()
        .await?
        .commit()
        .await
}

impl From<Membership> for MemberView {
    fn from(membership: Membership) -> Self {
        Self {
            user: membership.user.to_string(),
            standing: standing_name(membership.standing),
            created_at: None,
        }
    }
}

impl From<Listed<Membership>> for MemberView {
    fn from(listed: Listed<Membership>) -> Self {
        Self {
            created_at: Some(listed.created_at),
            ..Self::from(listed.item)
        }
    }
}

impl From<MemberRemoved> for DetachedView {
    fn from(removed: MemberRemoved) -> Self {
        Self {
            user: removed.membership.user.to_string(),
            tenant: removed.membership.tenant.to_string(),
            revoked: removed.revoked,
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use ip_core::{ApiId, Grant, TnKey};
    use ip_storage::{
        GrantStore, MembershipStore, NewTenant, NewUser, SqliteStore, TenantStore, UserStore,
    };
    use tower::ServiceExt;

    use super::super::harness::{body_of, cookie_of, hashed, request, state_over};
    use super::*;

    fn id(raw: &str) -> UserId {
        UserId::new(raw).unwrap()
    }

    fn tenant(raw: &str) -> TenantId {
        TenantId::new(raw).unwrap()
    }

    fn in_acme(user: &str) -> CapabilityScope {
        CapabilityScope::Tenant {
            user: id(user),
            tenant: tenant("acme"),
        }
    }

    /// The server's own router over acme, owned by alice with bob in it holding two grants
    /// and erin in it holding `UserMgr`, and globex, owned by dave. Carol is in nothing, and
    /// root is the system administrator.
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
            ("alice", AccountKind::Regular),
            ("bob", AccountKind::Regular),
            ("carol", AccountKind::Regular),
            ("dave", AccountKind::Regular),
            ("erin", AccountKind::Regular),
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
            ("erin", "acme", Standing::Member),
            ("dave", "globex", Standing::Owner),
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
        for grant in [
            Grant::new(Capability::Observer, in_acme("bob")).unwrap(),
            Grant::new(
                Capability::ApiAccess,
                CapabilityScope::Api {
                    user: id("bob"),
                    tenant: tenant("acme"),
                    api: ApiId::new("chat").unwrap(),
                },
            )
            .unwrap(),
            Grant::new(Capability::UserMgr, in_acme("erin")).unwrap(),
        ] {
            store.grant(&grant).await.unwrap();
        }

        let state = state_over(store);
        (crate::routes::router(state.clone()), state)
    }

    async fn call(
        router: &Router,
        state: &AppState,
        caller: &str,
        method: &str,
        uri: &str,
    ) -> axum::http::Response<Body> {
        router
            .clone()
            .oneshot(request(
                method,
                uri,
                &cookie_of(state, &id(caller)).await,
                None,
            ))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn an_owner_lists_everyone_in_its_tenant() {
        let (router, state) = fixture().await;
        let response = call(&router, &state, "alice", "GET", "/_ip/tenants/acme/members").await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_of(response).await).unwrap();
        let mut listed: Vec<(String, String)> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|member| {
                (
                    member["user"].as_str().unwrap().to_owned(),
                    member["standing"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        listed.sort();
        assert_eq!(
            listed,
            [
                ("alice".to_owned(), "owner".to_owned()),
                ("bob".to_owned(), "member".to_owned()),
                ("erin".to_owned(), "member".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn a_member_managing_nothing_lists_nobody() {
        let (router, state) = fixture().await;
        let response = call(&router, &state, "bob", "GET", "/_ip/tenants/acme/members").await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn an_account_joins_once_and_joining_again_changes_nothing() {
        let (router, state) = fixture().await;
        let joined = call(
            &router,
            &state,
            "alice",
            "PUT",
            "/_ip/tenants/acme/members/carol",
        )
        .await;
        assert_eq!(joined.status(), StatusCode::CREATED);
        assert_eq!(
            state
                .store()
                .membership(&id("carol"), &tenant("acme"))
                .await
                .unwrap()
                .unwrap()
                .standing,
            Standing::Member
        );

        let again = call(
            &router,
            &state,
            "alice",
            "PUT",
            "/_ip/tenants/acme/members/carol",
        )
        .await;
        assert_eq!(again.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn a_granted_user_manager_puts_accounts_in_without_owning_the_tenant() {
        let (router, state) = fixture().await;
        let joined = call(
            &router,
            &state,
            "erin",
            "PUT",
            "/_ip/tenants/acme/members/carol",
        )
        .await;
        assert_eq!(joined.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn an_owner_puts_nobody_into_a_tenant_it_does_not_manage() {
        let (router, state) = fixture().await;
        let response = call(
            &router,
            &state,
            "alice",
            "PUT",
            "/_ip/tenants/globex/members/carol",
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            state
                .store()
                .membership(&id("carol"), &tenant("globex"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_account_that_is_not_there_joins_nothing() {
        let (router, state) = fixture().await;
        let response = call(
            &router,
            &state,
            "alice",
            "PUT",
            "/_ip/tenants/acme/members/nobody",
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_system_administrator_joins_no_tenant() {
        let (router, state) = fixture().await;
        let response = call(
            &router,
            &state,
            "root",
            "PUT",
            "/_ip/tenants/acme/members/root",
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(
            state
                .store()
                .membership(&id("root"), &tenant("acme"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn putting_the_owner_in_again_does_not_make_it_a_member() {
        let (router, state) = fixture().await;
        let response = call(
            &router,
            &state,
            "root",
            "PUT",
            "/_ip/tenants/acme/members/alice",
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            state
                .store()
                .membership(&id("alice"), &tenant("acme"))
                .await
                .unwrap()
                .unwrap()
                .standing,
            Standing::Owner
        );
    }

    #[tokio::test]
    async fn an_account_that_leaves_holds_nothing_when_it_comes_back() {
        let (router, state) = fixture().await;
        let left = call(
            &router,
            &state,
            "alice",
            "DELETE",
            "/_ip/tenants/acme/members/bob",
        )
        .await;
        assert_eq!(left.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_of(left).await).unwrap();
        assert_eq!(body["revoked"], 2);
        assert!(
            state
                .store()
                .membership(&id("bob"), &tenant("acme"))
                .await
                .unwrap()
                .is_none()
        );

        let back = call(
            &router,
            &state,
            "alice",
            "PUT",
            "/_ip/tenants/acme/members/bob",
        )
        .await;
        assert_eq!(back.status(), StatusCode::CREATED);
        assert!(
            state
                .store()
                .grants_of(&id("bob"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn an_owner_does_not_leave_while_its_tenant_stands() {
        let (router, state) = fixture().await;
        let response = call(
            &router,
            &state,
            "root",
            "DELETE",
            "/_ip/tenants/acme/members/alice",
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(
            state
                .store()
                .membership(&id("alice"), &tenant("acme"))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn an_account_that_is_not_in_the_tenant_cannot_leave_it() {
        let (router, state) = fixture().await;
        let response = call(
            &router,
            &state,
            "alice",
            "DELETE",
            "/_ip/tenants/acme/members/carol",
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_member_managing_nothing_takes_nobody_out() {
        let (router, state) = fixture().await;
        let response = call(
            &router,
            &state,
            "bob",
            "DELETE",
            "/_ip/tenants/acme/members/erin",
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            state
                .store()
                .membership(&id("erin"), &tenant("acme"))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn members_are_listed_newest_first_a_page_at_a_time() {
        let (router, state) = fixture().await;
        let page = call(
            &router,
            &state,
            "alice",
            "GET",
            "/_ip/tenants/acme/members?limit=2",
        )
        .await;
        assert_eq!(page.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_of(page).await).unwrap();
        let users: Vec<&str> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|member| member["user"].as_str().unwrap())
            .collect();
        assert_eq!(users, ["erin", "bob"]);
        assert!(body[0]["created_at"].is_i64());

        let rest = call(
            &router,
            &state,
            "alice",
            "GET",
            "/_ip/tenants/acme/members?offset=2",
        )
        .await;
        let rest: serde_json::Value = serde_json::from_str(&body_of(rest).await).unwrap();
        assert_eq!(rest.as_array().unwrap().len(), 1);
        assert_eq!(rest[0]["user"], "alice");
    }

    #[tokio::test]
    async fn a_page_that_is_not_one_is_refused() {
        let (router, state) = fixture().await;
        let bad_moment = call(
            &router,
            &state,
            "alice",
            "GET",
            "/_ip/tenants/acme/members?after=99999999999999",
        )
        .await;
        assert_eq!(bad_moment.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bad_limit = call(
            &router,
            &state,
            "alice",
            "GET",
            "/_ip/tenants/acme/members?limit=many",
        )
        .await;
        assert_eq!(bad_limit.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_store_that_cannot_answer_is_the_server_s_own_failure() {
        let (router, state) = fixture().await;
        let cookie = cookie_of(&state, &id("alice")).await;
        state.store_failure().answer_only(1);

        let response = router
            .oneshot(request("GET", "/_ip/tenants/acme/members", &cookie, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
