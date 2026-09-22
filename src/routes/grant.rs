use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use ip_core::{ApiId, CapabilityScope, Grant, Grants, ScopeKind, TenantId, UserId};
use ip_storage::{AccountKind, Standing, Storage, StorageError};

use super::{capability_name, capability_named, store_refusal};
use crate::manage::Manager;
use crate::state::AppState;

/// The grants one member holds inside one tenant, under the prefix the router nests them at.
///
/// These are set by whoever administers the tenant: its owner, or the system administrator.
/// Holding `UserMgr` puts accounts in and out, but hands out nothing.
pub fn member_router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_in_tenant))
        .route(
            "/{capability}",
            put(grant_in_tenant).delete(revoke_in_tenant),
        )
        .route(
            "/{capability}/{api}",
            put(grant_on_api).delete(revoke_on_api),
        )
}

/// The grants held against an account itself, under the prefix the router nests them at.
///
/// Only `TenantMgr` is held this way, and only the system administrator hands it out.
pub fn account_router() -> Router<AppState> {
    Router::new().route("/", get(list_on_account)).route(
        "/{capability}",
        put(grant_on_account).delete(revoke_on_account),
    )
}

/// One grant as the management api shows it.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub struct GrantView {
    /// The capability held.
    pub capability: &'static str,
    /// The api it is held against, when it is held against one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
}

/// A response turned away early, boxed so it passes back up a `Result` cheaply.
type Refusal = Box<Response<Body>>;

/// Which grant a request names, before it is known whether the caller may touch it.
struct Named {
    /// The tenant the grant sits inside.
    tenant: TenantId,
    /// The member holding it.
    user: UserId,
    /// The grant itself.
    grant: Grant,
}

/// Every grant a member holds inside a tenant, shown to the tenant's administrators and to
/// the member itself.
async fn list_in_tenant(
    State(state): State<AppState>,
    manager: Manager,
    Path((tenant, user)): Path<(String, String)>,
) -> Response<Body> {
    let (Ok(tenant), Ok(user)) = (TenantId::new(&tenant), UserId::new(&user)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let store = state.store().as_ref();
    if manager.user() != &user {
        match administers(&manager, store, &tenant).await {
            Ok(true) => {}
            Ok(false) => return StatusCode::FORBIDDEN.into_response(),
            Err(error) => return store_refusal(error),
        }
    }
    match store.grants_in_tenant(&user, &tenant).await {
        Ok(grants) => Json(views(&grants)).into_response(),
        Err(error) => store_refusal(error),
    }
}

/// Grants a capability held inside the whole tenant.
async fn grant_in_tenant(
    State(state): State<AppState>,
    manager: Manager,
    Path((tenant, user, capability)): Path<(String, String, String)>,
) -> Response<Body> {
    match named(&tenant, &user, &capability, None) {
        Ok(named) => grant(&state, &manager, named).await,
        Err(refusal) => *refusal,
    }
}

/// Revokes a capability held inside the whole tenant.
async fn revoke_in_tenant(
    State(state): State<AppState>,
    manager: Manager,
    Path((tenant, user, capability)): Path<(String, String, String)>,
) -> Response<Body> {
    match named(&tenant, &user, &capability, None) {
        Ok(named) => revoke(&state, &manager, named).await,
        Err(refusal) => *refusal,
    }
}

/// Grants a capability held against one api.
async fn grant_on_api(
    State(state): State<AppState>,
    manager: Manager,
    Path((tenant, user, capability, api)): Path<(String, String, String, String)>,
) -> Response<Body> {
    match named(&tenant, &user, &capability, Some(&api)) {
        Ok(named) => grant(&state, &manager, named).await,
        Err(refusal) => *refusal,
    }
}

/// Revokes a capability held against one api.
async fn revoke_on_api(
    State(state): State<AppState>,
    manager: Manager,
    Path((tenant, user, capability, api)): Path<(String, String, String, String)>,
) -> Response<Body> {
    match named(&tenant, &user, &capability, Some(&api)) {
        Ok(named) => revoke(&state, &manager, named).await,
        Err(refusal) => *refusal,
    }
}

/// Reads the grant a path names, refusing one whose parts do not fit together.
fn named(tenant: &str, user: &str, capability: &str, api: Option<&str>) -> Result<Named, Refusal> {
    let (Ok(tenant), Ok(user)) = (TenantId::new(tenant), UserId::new(user)) else {
        return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
    };
    let Some(capability) = capability_named(capability) else {
        return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
    };
    let scope = match api {
        None => CapabilityScope::Tenant {
            user: user.clone(),
            tenant: tenant.clone(),
        },
        Some(api) => match ApiId::new(api) {
            Ok(api) => CapabilityScope::Api {
                user: user.clone(),
                tenant: tenant.clone(),
                api,
            },
            Err(_) => return Err(Box::new(StatusCode::NOT_FOUND.into_response())),
        },
    };
    match Grant::new(capability, scope) {
        Ok(grant) => Ok(Named {
            tenant,
            user,
            grant,
        }),
        Err(error) => Err(Box::new(
            (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response(),
        )),
    }
}

/// Records a grant for a member of the tenant, once its prerequisite is held.
async fn grant(state: &AppState, manager: &Manager, named: Named) -> Response<Body> {
    let store = state.store().as_ref();
    let held = match member_grants(manager, store, &named).await {
        Ok(held) => held,
        Err(refusal) => return *refusal,
    };
    if let Some(prerequisite) = named.grant.capability().prerequisite()
        && !held.holds(prerequisite, named.grant.scope())
    {
        return (
            StatusCode::CONFLICT,
            format!("grant {} first", capability_name(prerequisite)),
        )
            .into_response();
    }
    match store.grant(&named.grant).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => store_refusal(error),
    }
}

/// Removes a grant from a member of the tenant, once nothing held depends on it.
async fn revoke(state: &AppState, manager: &Manager, named: Named) -> Response<Body> {
    let store = state.store().as_ref();
    let held = match member_grants(manager, store, &named).await {
        Ok(held) => held,
        Err(refusal) => return *refusal,
    };
    let revoked = named.grant.capability();
    if let Some(dependent) = held.iter().find(|other| {
        other.capability().prerequisite() == Some(revoked) && other.scope() == named.grant.scope()
    }) {
        return (
            StatusCode::CONFLICT,
            format!("revoke {} first", capability_name(dependent.capability())),
        )
            .into_response();
    }
    match store.revoke(&named.grant).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => store_refusal(error),
    }
}

/// What the member already holds inside the tenant, once the caller is known to administer
/// it and the member is known to be one grants mean something to.
async fn member_grants(
    manager: &Manager,
    store: &dyn Storage,
    named: &Named,
) -> Result<Grants, Refusal> {
    match administers(manager, store, &named.tenant).await {
        Ok(true) => {}
        Ok(false) => return Err(Box::new(StatusCode::FORBIDDEN.into_response())),
        Err(error) => return Err(Box::new(store_refusal(error))),
    }
    match store.membership(&named.user, &named.tenant).await {
        Ok(Some(held)) if held.standing == Standing::Owner => {
            return Err(Box::new(
                (
                    StatusCode::CONFLICT,
                    "an owner holds every capability in its tenant already",
                )
                    .into_response(),
            ));
        }
        Ok(Some(_)) => {}
        Ok(None) => return Err(Box::new(StatusCode::NOT_FOUND.into_response())),
        Err(error) => return Err(Box::new(store_refusal(error))),
    }
    store
        .grants_in_tenant(&named.user, &named.tenant)
        .await
        .map_err(|error| Box::new(store_refusal(error)))
}

/// Whether the caller administers the tenant: owns it, or is the system administrator.
async fn administers(
    manager: &Manager,
    store: &dyn Storage,
    tenant: &TenantId,
) -> Result<bool, StorageError> {
    if manager.is_system_administrator() {
        return Ok(true);
    }
    Ok(store
        .membership(manager.user(), tenant)
        .await?
        .is_some_and(|held| held.standing == Standing::Owner))
}

/// The grants held against an account itself, shown to the system administrator and to the
/// account.
async fn list_on_account(
    State(state): State<AppState>,
    manager: Manager,
    Path(user): Path<String>,
) -> Response<Body> {
    let Ok(user) = UserId::new(&user) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !manager.is_system_administrator() && manager.user() != &user {
        return StatusCode::FORBIDDEN.into_response();
    }
    match state.store().grants_of(&user).await {
        Ok(grants) => {
            let own: Grants = grants
                .iter()
                .filter(|grant| grant.scope().kind() == ScopeKind::User)
                .cloned()
                .collect();
            Json(views(&own)).into_response()
        }
        Err(error) => store_refusal(error),
    }
}

/// Grants a capability held against an account itself.
async fn grant_on_account(
    State(state): State<AppState>,
    manager: Manager,
    Path((user, capability)): Path<(String, String)>,
) -> Response<Body> {
    let grant = match account_grant(&state, &manager, &user, &capability).await {
        Ok(grant) => grant,
        Err(refusal) => return *refusal,
    };
    match state.store().grant(&grant).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => store_refusal(error),
    }
}

/// Revokes a capability held against an account itself.
async fn revoke_on_account(
    State(state): State<AppState>,
    manager: Manager,
    Path((user, capability)): Path<(String, String)>,
) -> Response<Body> {
    let grant = match account_grant(&state, &manager, &user, &capability).await {
        Ok(grant) => grant,
        Err(refusal) => return *refusal,
    };
    match state.store().revoke(&grant).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => store_refusal(error),
    }
}

/// The account grant a path names, once the system administrator is the one asking and
/// the account is one a grant means something to.
async fn account_grant(
    state: &AppState,
    manager: &Manager,
    user: &str,
    capability: &str,
) -> Result<Grant, Refusal> {
    if !manager.is_system_administrator() {
        return Err(Box::new(StatusCode::FORBIDDEN.into_response()));
    }
    let (Ok(user), Some(capability)) = (UserId::new(user), capability_named(capability)) else {
        return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
    };
    let grant =
        Grant::new(capability, CapabilityScope::User { user: user.clone() }).map_err(|error| {
            Box::new((StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response())
        })?;
    match state.store().user(&user).await {
        Ok(Some(account)) if account.kind == AccountKind::SystemAdministrator => Err(Box::new(
            (
                StatusCode::CONFLICT,
                "a system administrator holds every capability already",
            )
                .into_response(),
        )),
        Ok(Some(_)) => Ok(grant),
        Ok(None) => Err(Box::new(StatusCode::NOT_FOUND.into_response())),
        Err(error) => Err(Box::new(store_refusal(error))),
    }
}

/// Grants as the api shows them, in an order that does not change between reads.
fn views(grants: &Grants) -> Vec<GrantView> {
    let mut views: Vec<GrantView> = grants
        .iter()
        .map(|grant| GrantView {
            capability: capability_name(grant.capability()),
            api: grant.scope().api().map(ToString::to_string),
        })
        .collect();
    views.sort_by(|left, right| (left.capability, &left.api).cmp(&(right.capability, &right.api)));
    views
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use ip_core::{Capability, TnKey};
    use ip_storage::{
        GrantStore, Membership, MembershipStore, NewTenant, NewUser, SqliteStore, TenantStore,
        UserStore,
    };
    use tower::ServiceExt;

    use super::super::harness::{PASSPHRASE, body_of, cookie_of, hashed, request, state_over};
    use super::*;

    fn id(raw: &str) -> UserId {
        UserId::new(raw).unwrap()
    }

    fn tenant(raw: &str) -> TenantId {
        TenantId::new(raw).unwrap()
    }

    /// The server's own router over acme, owned by alice with bob and erin in it, erin
    /// holding `UserMgr`, and globex, owned by dave. Carol is in nothing; root is the system
    /// administrator.
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
        store
            .grant(
                &Grant::new(
                    Capability::UserMgr,
                    CapabilityScope::Tenant {
                        user: id("erin"),
                        tenant: tenant("acme"),
                    },
                )
                .unwrap(),
            )
            .await
            .unwrap();

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

    /// What bob holds inside acme, as the api names it.
    async fn bob_holds(state: &AppState) -> Vec<GrantView> {
        views(
            &state
                .store()
                .grants_in_tenant(&id("bob"), &tenant("acme"))
                .await
                .unwrap(),
        )
    }

    fn view(capability: &'static str, api: Option<&str>) -> GrantView {
        GrantView {
            capability,
            api: api.map(ToOwned::to_owned),
        }
    }

    const BOB: &str = "/_ip/tenants/acme/members/bob/grants";

    #[tokio::test]
    async fn an_owner_grants_a_member_a_capability_once_however_often_it_asks() {
        let (router, state) = fixture().await;
        for _ in 0..2 {
            let response = call(&router, &state, "alice", "PUT", &format!("{BOB}/observer")).await;
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        }
        assert_eq!(bob_holds(&state).await, [view("observer", None)]);
    }

    #[tokio::test]
    async fn an_api_capability_waits_for_access_to_that_api() {
        let (router, state) = fixture().await;
        let early = call(
            &router,
            &state,
            "alice",
            "PUT",
            &format!("{BOB}/api_adv_mgr/chat"),
        )
        .await;
        assert_eq!(early.status(), StatusCode::CONFLICT);
        assert_eq!(body_of(early).await, "grant api_access first");

        for path in ["api_access/chat", "api_adv_mgr/chat"] {
            let response = call(&router, &state, "alice", "PUT", &format!("{BOB}/{path}")).await;
            assert_eq!(response.status(), StatusCode::NO_CONTENT, "{path}");
        }
        assert_eq!(
            bob_holds(&state).await,
            [
                view("api_access", Some("chat")),
                view("api_adv_mgr", Some("chat")),
            ]
        );
    }

    #[tokio::test]
    async fn access_to_an_api_goes_only_after_what_depends_on_it() {
        let (router, state) = fixture().await;
        for path in ["api_access/chat", "limit_mgr/chat"] {
            call(&router, &state, "alice", "PUT", &format!("{BOB}/{path}")).await;
        }

        let early = call(
            &router,
            &state,
            "alice",
            "DELETE",
            &format!("{BOB}/api_access/chat"),
        )
        .await;
        assert_eq!(early.status(), StatusCode::CONFLICT);
        assert_eq!(body_of(early).await, "revoke limit_mgr first");

        for path in ["limit_mgr/chat", "api_access/chat"] {
            let response = call(&router, &state, "alice", "DELETE", &format!("{BOB}/{path}")).await;
            assert_eq!(response.status(), StatusCode::NO_CONTENT, "{path}");
        }
        assert!(bob_holds(&state).await.is_empty());
    }

    #[tokio::test]
    async fn revoking_what_is_not_held_reports_it_missing() {
        let (router, state) = fixture().await;
        let response = call(
            &router,
            &state,
            "alice",
            "DELETE",
            &format!("{BOB}/observer"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn managing_users_or_owning_another_tenant_hands_out_nothing() {
        let (router, state) = fixture().await;
        for caller in ["erin", "dave", "bob"] {
            let response = call(&router, &state, caller, "PUT", &format!("{BOB}/observer")).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{caller}");
        }
        assert!(bob_holds(&state).await.is_empty());
    }

    #[tokio::test]
    async fn the_administrator_grants_inside_any_tenant() {
        let (router, state) = fixture().await;
        let response = call(&router, &state, "root", "PUT", &format!("{BOB}/sys_agent")).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(bob_holds(&state).await, [view("sys_agent", None)]);
    }

    #[tokio::test]
    async fn only_a_member_other_than_the_owner_is_granted_anything() {
        let (router, state) = fixture().await;
        let outsider = call(
            &router,
            &state,
            "alice",
            "PUT",
            "/_ip/tenants/acme/members/carol/grants/observer",
        )
        .await;
        assert_eq!(outsider.status(), StatusCode::NOT_FOUND);

        let owner = call(
            &router,
            &state,
            "root",
            "PUT",
            "/_ip/tenants/acme/members/alice/grants/observer",
        )
        .await;
        assert_eq!(owner.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn a_capability_named_at_the_wrong_scope_is_refused() {
        let (router, state) = fixture().await;
        for path in ["api_access", "tenant_mgr", "observer/chat", "user_mgr/chat"] {
            let response = call(&router, &state, "alice", "PUT", &format!("{BOB}/{path}")).await;
            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{path}"
            );
        }
        let unknown = call(&router, &state, "alice", "PUT", &format!("{BOB}/root")).await;
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
        assert!(bob_holds(&state).await.is_empty());
    }

    #[tokio::test]
    async fn a_member_reads_its_own_grants_and_only_an_administrator_reads_another_s() {
        let (router, state) = fixture().await;
        for path in ["observer", "api_access/chat"] {
            call(&router, &state, "alice", "PUT", &format!("{BOB}/{path}")).await;
        }

        for caller in ["bob", "alice", "root"] {
            let response = call(&router, &state, caller, "GET", BOB).await;
            assert_eq!(response.status(), StatusCode::OK, "{caller}");
            let body: serde_json::Value = serde_json::from_str(&body_of(response).await).unwrap();
            assert_eq!(
                body,
                serde_json::json!([
                    {"capability": "api_access", "api": "chat"},
                    {"capability": "observer"},
                ]),
                "{caller}"
            );
        }
        let refused = call(&router, &state, "erin", "GET", BOB).await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn an_account_granted_tenant_mgr_creates_a_tenant() {
        let (router, state) = fixture().await;
        let granted = call(
            &router,
            &state,
            "root",
            "PUT",
            "/_ip/users/carol/grants/tenant_mgr",
        )
        .await;
        assert_eq!(granted.status(), StatusCode::NO_CONTENT);

        let created = router
            .clone()
            .oneshot(request(
                "POST",
                "/_ip/tenants",
                &cookie_of(&state, &id("carol")).await,
                Some(&format!(
                    r#"{{"tenant":"initech","owner":"peter","passphrase":"{PASSPHRASE}"}}"#
                )),
            ))
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);

        let listed = call(&router, &state, "carol", "GET", "/_ip/users/carol/grants").await;
        let body: serde_json::Value = serde_json::from_str(&body_of(listed).await).unwrap();
        assert_eq!(body, serde_json::json!([{"capability": "tenant_mgr"}]));

        let revoked = call(
            &router,
            &state,
            "root",
            "DELETE",
            "/_ip/users/carol/grants/tenant_mgr",
        )
        .await;
        assert_eq!(revoked.status(), StatusCode::NO_CONTENT);
        assert!(
            state
                .store()
                .grants_of(&id("carol"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn only_the_administrator_hands_out_tenant_mgr_and_never_to_itself() {
        let (router, state) = fixture().await;
        let refused = call(
            &router,
            &state,
            "alice",
            "PUT",
            "/_ip/users/carol/grants/tenant_mgr",
        )
        .await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);

        let itself = call(
            &router,
            &state,
            "root",
            "PUT",
            "/_ip/users/root/grants/tenant_mgr",
        )
        .await;
        assert_eq!(itself.status(), StatusCode::CONFLICT);

        let wrong_scope = call(
            &router,
            &state,
            "root",
            "PUT",
            "/_ip/users/carol/grants/observer",
        )
        .await;
        assert_eq!(wrong_scope.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let absent = call(
            &router,
            &state,
            "root",
            "PUT",
            "/_ip/users/nobody/grants/tenant_mgr",
        )
        .await;
        assert_eq!(absent.status(), StatusCode::NOT_FOUND);

        let others = call(&router, &state, "alice", "GET", "/_ip/users/carol/grants").await;
        assert_eq!(others.status(), StatusCode::FORBIDDEN);
        assert!(
            state
                .store()
                .grants_of(&id("carol"))
                .await
                .unwrap()
                .is_empty()
        );
    }
}
