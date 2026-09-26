use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use ip_core::{Allowance, ApiId, Counted, LimitScope, Period, TenantId, Timestamp, UserId};
use ip_storage::{Limit, NewLimit, Standing};

use super::{Paged, store_refusal};
use crate::manage::Manager;
use crate::state::AppState;

/// What a tenant and its accounts may spend, under the prefix the router nests it at.
pub fn router() -> Router<AppState> {
    Router::new().route("/", get(list).put(put).delete(remove))
}

/// A refusal, boxed so the ordinary answer is not paid for by every caller.
type Refusal = Box<Response<Body>>;

/// One limit as the api reports it.
#[derive(Debug, serde::Serialize)]
pub struct LimitView {
    /// The tenant it belongs to.
    pub tenant: String,
    /// The one account it applies to, or absent for every account in the tenant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// The one api it applies to, or absent for every api.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// `tokens` or `requests`.
    pub counted: Counted,
    /// `minute`, `hour`, `day` or `month`.
    pub period: Period,
    /// How much that stretch allows.
    pub allowance: u64,
    /// When the limit was set, in seconds since the unix epoch.
    pub created_at: Timestamp,
}

impl From<Limit> for LimitView {
    fn from(limit: Limit) -> Self {
        Self {
            tenant: limit.scope.tenant.to_string(),
            user: limit.scope.user.map(|user| user.to_string()),
            api: limit.scope.api.map(|api| api.to_string()),
            counted: limit.counted,
            period: limit.period,
            allowance: limit.allowance.get(),
            created_at: limit.created_at,
        }
    }
}

/// A limit as a caller sets it, and as it names one to remove.
#[derive(Debug, serde::Deserialize)]
struct Setting {
    tenant: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    api: Option<String>,
    counted: Counted,
    period: Period,
    /// Absent when the caller is naming a limit to remove.
    #[serde(default)]
    allowance: Option<u64>,
}

impl Setting {
    /// The scope this names, refusing an id that is not one.
    fn scope(&self) -> Result<LimitScope, Refusal> {
        Ok(LimitScope {
            tenant: TenantId::new(&self.tenant).map_err(unprocessable)?,
            user: self
                .user
                .as_deref()
                .map(UserId::new)
                .transpose()
                .map_err(unprocessable)?,
            api: self
                .api
                .as_deref()
                .map(ApiId::new)
                .transpose()
                .map_err(unprocessable)?,
        })
    }
}

/// Which tenant a caller is asking about.
#[derive(Debug, Default, serde::Deserialize)]
struct Narrowing {
    tenant: Option<String>,
}

/// The limits in force, newest first.
///
/// The system administrator reads every tenant, and may name one. Anyone else reads the tenant
/// it owns, whichever it names.
async fn list(
    State(state): State<AppState>,
    manager: Manager,
    Query(narrowing): Query<Narrowing>,
    Paged(page): Paged,
) -> Response<Body> {
    let named = match narrowing.tenant.as_deref().map(TenantId::new).transpose() {
        Ok(named) => named,
        Err(error) => return unprocessable(error).into_response(),
    };
    let reading = match reading(&state, &manager, named).await {
        Ok(reading) => reading,
        Err(refusal) => return *refusal,
    };
    match state.store().list_limits(reading.as_ref(), page).await {
        Ok(listed) => {
            Json(listed.into_iter().map(LimitView::from).collect::<Vec<_>>()).into_response()
        }
        Err(error) => store_refusal(error),
    }
}

/// Sets a limit, replacing what the same scope allowed of the same thing over the same stretch.
async fn put(State(state): State<AppState>, manager: Manager, body: Bytes) -> Response<Body> {
    let (setting, scope) = match named(&body) {
        Ok(named) => named,
        Err(refusal) => return *refusal,
    };
    if let Err(refusal) = may_set(&state, &manager, &scope).await {
        return *refusal;
    }
    let Some(allowance) = setting.allowance else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "a limit says how much it allows",
        )
            .into_response();
    };
    let allowance = match Allowance::new(allowance) {
        Ok(allowance) => allowance,
        Err(error) => return *unprocessable(error),
    };
    let limit = NewLimit {
        scope,
        counted: setting.counted,
        period: setting.period,
        allowance,
    };
    match state.store().put_limit(limit).await {
        Ok(limit) => {
            state.feed().after_change().await;
            (StatusCode::CREATED, Json(LimitView::from(limit))).into_response()
        }
        Err(error) => store_refusal(error),
    }
}

/// Removes one limit, leaving every other limit on the same scope.
async fn remove(State(state): State<AppState>, manager: Manager, body: Bytes) -> Response<Body> {
    let (setting, scope) = match named(&body) {
        Ok(named) => named,
        Err(refusal) => return *refusal,
    };
    if let Err(refusal) = may_set(&state, &manager, &scope).await {
        return *refusal;
    }
    match state
        .store()
        .remove_limit(&scope, setting.counted, setting.period)
        .await
    {
        Ok(()) => {
            state.feed().after_change().await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => store_refusal(error),
    }
}

/// The limit a body names, and the scope it applies to.
fn named(body: &Bytes) -> Result<(Setting, LimitScope), Refusal> {
    let Json(setting) = Json::<Setting>::from_bytes(body)
        .map_err(|rejection| Box::new(rejection.into_response()))?;
    let scope = setting.scope()?;
    Ok((setting, scope))
}

/// Which tenant this caller may read, or none for every tenant.
///
/// A caller that owns a tenant reads that tenant whichever it names, so naming another's is a
/// refusal rather than a quiet answer about its own.
async fn reading(
    state: &AppState,
    manager: &Manager,
    named: Option<TenantId>,
) -> Result<Option<TenantId>, Refusal> {
    if manager.is_system_administrator() {
        return Ok(named);
    }
    let owned = owned_by(state, manager).await?;
    match named {
        Some(named) if named != owned => Err(Box::new(StatusCode::FORBIDDEN.into_response())),
        _ => Ok(Some(owned)),
    }
}

/// Whether this caller may set the limit on `scope`.
///
/// A limit over a whole tenant is the system administrator's to set, since it binds every
/// account in it. A limit naming an account is the tenant owner's, inside its own tenant.
async fn may_set(state: &AppState, manager: &Manager, scope: &LimitScope) -> Result<(), Refusal> {
    if manager.is_system_administrator() {
        return Ok(());
    }
    if scope.user.is_none() {
        return Err(Box::new(
            (
                StatusCode::FORBIDDEN,
                "a limit over a whole tenant is the system administrator's to set",
            )
                .into_response(),
        ));
    }
    match owned_by(state, manager).await? == scope.tenant {
        true => Ok(()),
        false => Err(Box::new(StatusCode::FORBIDDEN.into_response())),
    }
}

/// The tenant this caller owns, or a refusal when it owns none.
async fn owned_by(state: &AppState, manager: &Manager) -> Result<TenantId, Refusal> {
    let memberships = state
        .store()
        .memberships_of_user(manager.user())
        .await
        .map_err(|error| Box::new(store_refusal(error)))?;
    memberships
        .into_iter()
        .find(|held| held.standing == Standing::Owner)
        .map(|held| held.tenant)
        .ok_or_else(|| Box::new(StatusCode::FORBIDDEN.into_response()))
}

/// A value the caller spelled wrongly.
fn unprocessable(error: ip_core::CoreError) -> Refusal {
    Box::new((StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response())
}

#[cfg(test)]
mod tests {
    use ip_core::TnKey;
    use ip_storage::{
        AccountKind, Membership, MembershipStore, NewTenant, NewUser, SqliteStore, TenantStore,
        UserStore,
    };
    use std::sync::Arc;
    use tower::ServiceExt;

    use super::super::harness::{body_of, cookie_of, hashed, request, state_over_shared};
    use super::*;
    use axum::Router;

    fn user(raw: &str) -> UserId {
        UserId::new(raw).unwrap()
    }

    fn tenant(raw: &str) -> TenantId {
        TenantId::new(raw).unwrap()
    }

    /// acme, owned by alice with bob a member; other, owned by carol; root the administrator.
    async fn store() -> Arc<SqliteStore> {
        let store = SqliteStore::in_memory().await.unwrap();
        for id in ["acme", "other"] {
            store
                .create_tenant(NewTenant {
                    id: tenant(id),
                    key: TnKey::generate().unwrap(),
                })
                .await
                .unwrap();
        }
        for (id, kind) in [
            ("root", AccountKind::SystemAdministrator),
            ("alice", AccountKind::Regular),
            ("bob", AccountKind::Regular),
            ("carol", AccountKind::Regular),
        ] {
            store
                .create_user(NewUser {
                    id: user(id),
                    passphrase: hashed(),
                    kind,
                })
                .await
                .unwrap();
        }
        for (id, tenant_id, standing) in [
            ("alice", "acme", Standing::Owner),
            ("bob", "acme", Standing::Member),
            ("carol", "other", Standing::Owner),
        ] {
            store
                .attach(Membership {
                    user: user(id),
                    tenant: tenant(tenant_id),
                    standing,
                })
                .await
                .unwrap();
        }
        Arc::new(store)
    }

    async fn call(
        router: &Router,
        state: &AppState,
        caller: &str,
        method: &str,
        uri: &str,
        body: Option<&str>,
    ) -> Response<Body> {
        router
            .clone()
            .oneshot(request(
                method,
                uri,
                &cookie_of(state, &user(caller)).await,
                body,
            ))
            .await
            .unwrap()
    }

    /// The body a caller sets a limit with.
    fn setting(tenant: &str, user: Option<&str>, allowance: Option<u64>) -> String {
        let user = match user {
            Some(user) => format!("\"user\": \"{user}\", "),
            None => String::new(),
        };
        let allowance = match allowance {
            Some(allowance) => format!(", \"allowance\": {allowance}"),
            None => String::new(),
        };
        format!(
            "{{\"tenant\": \"{tenant}\", {user}\"counted\": \"tokens\", \
             \"period\": \"month\"{allowance}}}"
        )
    }

    #[tokio::test]
    async fn the_administrator_sets_a_limit_over_a_tenant_and_reads_it_back() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let set = call(
            &router,
            &state,
            "root",
            "PUT",
            "/_ip/limits",
            Some(&setting("acme", None, Some(1_000_000))),
        )
        .await;
        assert_eq!(set.status(), StatusCode::CREATED);
        let view: serde_json::Value = serde_json::from_str(&body_of(set).await).unwrap();
        assert_eq!(view["tenant"], "acme");
        assert_eq!(view["counted"], "tokens");
        assert_eq!(view["period"], "month");
        assert_eq!(view["allowance"], 1_000_000);
        assert!(view["created_at"].is_number());
        assert!(view.get("user").is_none());

        let listed = call(&router, &state, "root", "GET", "/_ip/limits", None).await;
        assert_eq!(listed.status(), StatusCode::OK);
        let listed: serde_json::Value = serde_json::from_str(&body_of(listed).await).unwrap();
        assert_eq!(listed.as_array().unwrap().len(), 1);
        assert_eq!(listed[0]["allowance"], 1_000_000);
    }

    #[tokio::test]
    async fn setting_the_same_limit_again_replaces_what_it_allows() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        for allowance in [10, 20] {
            let set = call(
                &router,
                &state,
                "root",
                "PUT",
                "/_ip/limits",
                Some(&setting("acme", None, Some(allowance))),
            )
            .await;
            assert_eq!(set.status(), StatusCode::CREATED);
        }
        let listed = call(&router, &state, "root", "GET", "/_ip/limits", None).await;
        let listed: serde_json::Value = serde_json::from_str(&body_of(listed).await).unwrap();
        assert_eq!(listed.as_array().unwrap().len(), 1);
        assert_eq!(listed[0]["allowance"], 20);
    }

    #[tokio::test]
    async fn an_owner_sets_a_limit_on_one_of_its_own_accounts() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let set = call(
            &router,
            &state,
            "alice",
            "PUT",
            "/_ip/limits",
            Some(&setting("acme", Some("bob"), Some(500))),
        )
        .await;
        assert_eq!(set.status(), StatusCode::CREATED);
        let view: serde_json::Value = serde_json::from_str(&body_of(set).await).unwrap();
        assert_eq!(view["user"], "bob");
    }

    #[tokio::test]
    async fn an_owner_may_not_set_a_limit_over_its_whole_tenant() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let refused = call(
            &router,
            &state,
            "alice",
            "PUT",
            "/_ip/limits",
            Some(&setting("acme", None, Some(500))),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        assert!(body_of(refused).await.contains("system administrator"));
    }

    #[tokio::test]
    async fn an_owner_reaches_no_other_tenant() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let refused = call(
            &router,
            &state,
            "alice",
            "PUT",
            "/_ip/limits",
            Some(&setting("other", Some("carol"), Some(500))),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);

        let read = call(
            &router,
            &state,
            "alice",
            "GET",
            "/_ip/limits?tenant=other",
            None,
        )
        .await;
        assert_eq!(read.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_member_that_owns_nothing_sets_nothing() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let refused = call(
            &router,
            &state,
            "bob",
            "PUT",
            "/_ip/limits",
            Some(&setting("acme", Some("bob"), Some(500))),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            call(&router, &state, "bob", "GET", "/_ip/limits", None)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn an_owner_reads_its_own_tenant_without_naming_it() {
        let store = store().await;
        let state = state_over_shared(Arc::clone(&store));
        let router = crate::routes::router(state.clone());
        call(
            &router,
            &state,
            "alice",
            "PUT",
            "/_ip/limits",
            Some(&setting("acme", Some("bob"), Some(500))),
        )
        .await;
        let listed = call(&router, &state, "alice", "GET", "/_ip/limits", None).await;
        let listed: serde_json::Value = serde_json::from_str(&body_of(listed).await).unwrap();
        assert_eq!(listed.as_array().unwrap().len(), 1);
        assert_eq!(listed[0]["user"], "bob");
    }

    #[tokio::test]
    async fn a_limit_is_removed_by_the_scope_and_stretch_it_names() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        call(
            &router,
            &state,
            "root",
            "PUT",
            "/_ip/limits",
            Some(&setting("acme", None, Some(10))),
        )
        .await;
        let removed = call(
            &router,
            &state,
            "root",
            "DELETE",
            "/_ip/limits",
            Some(&setting("acme", None, None)),
        )
        .await;
        assert_eq!(removed.status(), StatusCode::NO_CONTENT);
        let listed = call(&router, &state, "root", "GET", "/_ip/limits", None).await;
        let listed: serde_json::Value = serde_json::from_str(&body_of(listed).await).unwrap();
        assert!(listed.as_array().unwrap().is_empty());

        let again = call(
            &router,
            &state,
            "root",
            "DELETE",
            "/_ip/limits",
            Some(&setting("acme", None, None)),
        )
        .await;
        assert_eq!(again.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_limit_that_allows_nothing_is_refused() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let refused = call(
            &router,
            &state,
            "root",
            "PUT",
            "/_ip/limits",
            Some(&setting("acme", None, Some(0))),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn a_limit_that_says_nothing_about_what_it_allows_is_refused() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let refused = call(
            &router,
            &state,
            "root",
            "PUT",
            "/_ip/limits",
            Some(&setting("acme", None, None)),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn a_stretch_this_server_does_not_count_over_is_refused() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let refused = call(
            &router,
            &state,
            "root",
            "PUT",
            "/_ip/limits",
            Some(
                r#"{"tenant": "acme", "counted": "tokens", "period": "fortnight", "allowance": 5}"#,
            ),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn a_limit_on_a_tenant_that_is_not_there_is_not_found() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let refused = call(
            &router,
            &state,
            "root",
            "PUT",
            "/_ip/limits",
            Some(&setting("nowhere", None, Some(10))),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_caller_that_proved_nothing_sets_nothing() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state);
        let refused = router
            .oneshot(request("GET", "/_ip/limits", "", None))
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    }
}
