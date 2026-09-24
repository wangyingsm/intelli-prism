use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{FromRequestParts, Query, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use ip_core::{ApiId, Capability, CapabilityScope, Latency, Served, TenantId, Timestamp, UserId};
use ip_storage::{Usage, UsageFilter};

use super::{Paged, store_refusal};
use crate::manage::Manager;
use crate::state::AppState;

/// A refusal, boxed so the ordinary answer is not paid for by every caller.
type Refusal = Box<Response<Body>>;

/// What every request cost, under the prefix the router nests it at.
pub fn router() -> Router<AppState> {
    Router::new().route("/", get(list))
}

/// One recorded request, as the api reports it.
#[derive(Debug, serde::Serialize)]
pub struct UsageView {
    /// The trace the request was followed under, in the hex the answer carried.
    pub trace: String,
    /// The chat turn it belonged to, when the caller marked one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
    /// Who it was spent for.
    pub tenant: String,
    /// Which account made it.
    pub user: String,
    /// Which api served it.
    pub api: String,
    /// The model that answered, when the answer named one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Tokens the request carried to the model.
    pub input_tokens: u32,
    /// Tokens the model answered with.
    pub output_tokens: u32,
    /// `upstream` when the model answered, `cache` when nothing was spent.
    pub served: Served,
    /// How long the caller waited, in milliseconds.
    pub latency_ms: u32,
    /// When it was recorded, in seconds since the unix epoch.
    pub created_at: Timestamp,
}

impl From<Usage> for UsageView {
    fn from(usage: Usage) -> Self {
        Self {
            trace: usage.trace.to_hex(),
            turn: usage.turn.map(|turn| turn.to_string()),
            tenant: usage.tenant.to_string(),
            user: usage.user.to_string(),
            api: usage.api.to_string(),
            model: usage.model.map(|model| model.to_string()),
            input_tokens: usage.tokens.input.get(),
            output_tokens: usage.tokens.output.get(),
            served: usage.served,
            latency_ms: Latency::millis(usage.latency),
            created_at: usage.created_at,
        }
    }
}

/// What a caller narrows the list to, on top of what it is allowed to see at all.
#[derive(Debug, Default, serde::Deserialize)]
struct Narrowing {
    tenant: Option<String>,
    user: Option<String>,
    api: Option<String>,
}

/// What was spent, newest first.
///
/// The system administrator sees every tenant, and may name one. Anyone else names the tenant
/// they are asking about and must hold `Observer` inside it, which a tenant owner does by
/// standing: a caller only ever reads the tenant whose name it asked for.
async fn list(
    State(state): State<AppState>,
    manager: Manager,
    Narrowed(narrowing): Narrowed,
    Paged(page): Paged,
) -> Response<Body> {
    let filter = match filter_for(&state, &manager, narrowing).await {
        Ok(filter) => filter,
        Err(refusal) => return *refusal,
    };
    match state.store().list_usage(&filter, page).await {
        Ok(listed) => {
            Json(listed.into_iter().map(UsageView::from).collect::<Vec<_>>()).into_response()
        }
        Err(error) => store_refusal(error),
    }
}

/// What this caller may read, narrowed by what it asked for.
async fn filter_for(
    state: &AppState,
    manager: &Manager,
    narrowing: Narrowing,
) -> Result<UsageFilter, Refusal> {
    let tenant = narrowing
        .tenant
        .as_deref()
        .map(TenantId::new)
        .transpose()
        .map_err(|error| {
            Box::new((StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response())
        })?;
    let user = narrowing
        .user
        .as_deref()
        .map(UserId::new)
        .transpose()
        .map_err(|error| {
            Box::new((StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response())
        })?;
    let api = narrowing
        .api
        .as_deref()
        .map(ApiId::new)
        .transpose()
        .map_err(|error| {
            Box::new((StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response())
        })?;

    if !manager.is_system_administrator() {
        let Some(named) = tenant.clone() else {
            return Err(Box::new(
                (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "name the tenant to read the usage of",
                )
                    .into_response(),
            ));
        };
        let scope = CapabilityScope::Tenant {
            user: manager.user().clone(),
            tenant: named,
        };
        match manager
            .allows(state.store().as_ref(), Capability::Observer, &scope)
            .await
        {
            Ok(true) => {}
            Ok(false) => return Err(Box::new(StatusCode::FORBIDDEN.into_response())),
            Err(error) => return Err(Box::new(store_refusal(error))),
        }
    }
    Ok(UsageFilter { tenant, user, api })
}

/// The `tenant`, `user` and `api` a caller narrows the list by.
struct Narrowed(Narrowing);

impl<S: Send + Sync> FromRequestParts<S> for Narrowed {
    type Rejection = Response<Body>;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Query(narrowing) = Query::<Narrowing>::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        Ok(Self(narrowing))
    }
}

#[cfg(test)]
mod tests {
    use ip_core::{Grant, ModelName, TnKey, TokenCount, Tokens, TraceId, TurnId};
    use ip_storage::{
        AccountKind, GrantStore, Membership, MembershipStore, NewTenant, NewUsage, NewUser,
        SqliteStore, Standing, TenantStore, UsageStore, UserStore,
    };
    use tower::ServiceExt;

    use super::super::harness::{body_of, cookie_of, hashed, request, state_over_shared};
    use super::*;
    use axum::Router;
    use std::sync::Arc;

    fn user(raw: &str) -> UserId {
        UserId::new(raw).unwrap()
    }

    fn tenant(raw: &str) -> TenantId {
        TenantId::new(raw).unwrap()
    }

    /// One request, spent by whoever is named.
    fn spent(tenant_id: &str, user_id: &str, api: &str) -> NewUsage {
        NewUsage {
            trace: TraceId::generate().unwrap(),
            turn: Some(TurnId::new("turn-1").unwrap()),
            tenant: tenant(tenant_id),
            user: user(user_id),
            api: ApiId::new(api).unwrap(),
            model: Some(ModelName::new("claude-opus-5").unwrap()),
            tokens: Tokens::new(TokenCount::new(120), TokenCount::new(30)),
            served: Served::Upstream,
            latency: Latency::from_millis(1_250),
        }
    }

    /// Two tenants, each having spent something: acme, owned by alice with bob a member, and
    /// other, owned by carol. root is the system administrator.
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
        store
            .record_usage(spent("acme", "alice", "anthropic"))
            .await
            .unwrap();
        store
            .record_usage(spent("acme", "bob", "openai"))
            .await
            .unwrap();
        store
            .record_usage(spent("other", "carol", "anthropic"))
            .await
            .unwrap();
        Arc::new(store)
    }

    async fn read(router: &Router, state: &AppState, caller: &str, query: &str) -> Response<Body> {
        router
            .clone()
            .oneshot(request(
                "GET",
                &format!("/_ip/usage{query}"),
                &cookie_of(state, &user(caller)).await,
                None,
            ))
            .await
            .unwrap()
    }

    async fn listed(response: Response<Body>) -> serde_json::Value {
        assert_eq!(response.status(), StatusCode::OK);
        serde_json::from_str(&body_of(response).await).unwrap()
    }

    #[tokio::test]
    async fn the_administrator_reads_what_every_tenant_spent() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let listed = listed(read(&router, &state, "root", "").await).await;
        assert_eq!(listed.as_array().unwrap().len(), 3);

        let row = &listed[0];
        assert_eq!(row["tenant"], "other");
        assert_eq!(row["user"], "carol");
        assert_eq!(row["api"], "anthropic");
        assert_eq!(row["model"], "claude-opus-5");
        assert_eq!(row["input_tokens"], 120);
        assert_eq!(row["output_tokens"], 30);
        assert_eq!(row["served"], "upstream");
        assert_eq!(row["latency_ms"], 1250);
        assert_eq!(row["turn"], "turn-1");
        assert!(row["created_at"].is_number(), "a time is not a number");
        assert_eq!(row["trace"].as_str().unwrap().len(), 32);
    }

    #[tokio::test]
    async fn the_administrator_may_name_one_tenant() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let listed = listed(read(&router, &state, "root", "?tenant=acme").await).await;
        assert_eq!(listed.as_array().unwrap().len(), 2);
        assert!(
            listed
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["tenant"] == "acme")
        );
    }

    #[tokio::test]
    async fn an_owner_reads_its_own_tenant() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let listed = listed(read(&router, &state, "alice", "?tenant=acme").await).await;
        assert_eq!(listed.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn an_owner_reads_no_other_tenant() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let refused = read(&router, &state, "alice", "?tenant=other").await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_member_reads_nothing_until_it_is_granted_observer() {
        let store = store().await;
        let state = state_over_shared(Arc::clone(&store));
        let router = crate::routes::router(state.clone());
        let refused = read(&router, &state, "bob", "?tenant=acme").await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);

        store
            .grant(
                &Grant::new(
                    Capability::Observer,
                    CapabilityScope::Tenant {
                        user: user("bob"),
                        tenant: tenant("acme"),
                    },
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let listed = listed(read(&router, &state, "bob", "?tenant=acme").await).await;
        assert_eq!(listed.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn anyone_but_the_administrator_names_the_tenant_it_is_asking_about() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let refused = read(&router, &state, "alice", "").await;
        assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn a_list_is_narrowed_by_user_and_by_api() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let by_user = listed(read(&router, &state, "alice", "?tenant=acme&user=bob").await).await;
        assert_eq!(by_user.as_array().unwrap().len(), 1);
        assert_eq!(by_user[0]["user"], "bob");

        let by_api =
            listed(read(&router, &state, "alice", "?tenant=acme&api=anthropic").await).await;
        assert_eq!(by_api.as_array().unwrap().len(), 1);
        assert_eq!(by_api[0]["user"], "alice");
    }

    #[tokio::test]
    async fn a_page_holds_what_it_was_asked_for() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let first = listed(read(&router, &state, "root", "?limit=2").await).await;
        assert_eq!(first.as_array().unwrap().len(), 2);
        let second = listed(read(&router, &state, "root", "?limit=2&offset=2").await).await;
        assert_eq!(second.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_name_that_is_not_one_is_refused_rather_than_ignored() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let refused = read(&router, &state, "root", "?tenant=NOT%20A%20TENANT").await;
        assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn a_caller_that_proved_nothing_reads_nothing() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let refused = router
            .oneshot(request("GET", "/_ip/usage", "", None))
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    }
}
