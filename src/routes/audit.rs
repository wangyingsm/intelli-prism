use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{FromRequestParts, Query, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use ip_core::{Capability, CapabilityScope, TenantId, Timestamp, UserId};
use ip_storage::{AuditFilter, Audited, Entity};

use super::{Paged, store_refusal};
use crate::manage::Manager;
use crate::state::AppState;

/// A refusal, boxed so the ordinary answer is not paid for by every caller.
type Refusal = Box<Response<Body>>;

/// Who changed what, under the prefix the router nests it at.
pub fn router() -> Router<AppState> {
    Router::new().route("/", get(list))
}

/// One recorded change, as the api reports it.
#[derive(Debug, serde::Serialize)]
pub struct AuditView {
    /// The account that made the change.
    pub actor: String,
    /// The endpoint it was made through.
    pub through: String,
    /// Which kind of thing changed.
    pub entity: &'static str,
    /// Which one, spelled the way the endpoint that changed it names it.
    pub entity_id: String,
    /// Whose it was, absent where it belongs to the deployment rather than to a tenant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// What it was before, absent where there was nothing there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<serde_json::Value>,
    /// What it became, absent where it went away.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<serde_json::Value>,
    /// When the change was recorded, in seconds since the unix epoch.
    pub created_at: Timestamp,
}

impl From<Audited> for AuditView {
    fn from(audited: Audited) -> Self {
        Self {
            actor: audited.actor.to_string(),
            through: audited.through,
            entity: audited.entity.name(),
            entity_id: audited.entity_id,
            tenant: audited.tenant.map(|tenant| tenant.to_string()),
            before: audited.before,
            after: audited.after,
            created_at: audited.created_at,
        }
    }
}

/// What a caller narrows the trail to, on top of what it is allowed to see at all.
#[derive(Debug, Default, serde::Deserialize)]
struct Narrowing {
    tenant: Option<String>,
    actor: Option<String>,
    entity: Option<String>,
}

/// Who changed what, newest first.
///
/// The system administrator reads every tenant, and may name one. Anyone else names the tenant
/// they are asking about and must hold `Observer` inside it, which a tenant owner does by
/// standing: a caller only ever reads the trail of the tenant whose name it asked for. What
/// belongs to the deployment rather than to any tenant — an account, a route, a price — is the
/// administrator's alone to read, because it is recorded against no tenant to name.
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
    match state.store().audits(&filter, page).await {
        Ok(trail) => {
            Json(trail.into_iter().map(AuditView::from).collect::<Vec<_>>()).into_response()
        }
        Err(error) => store_refusal(error),
    }
}

/// What this caller may read, narrowed by what it asked for.
async fn filter_for(
    state: &AppState,
    manager: &Manager,
    narrowing: Narrowing,
) -> Result<AuditFilter, Refusal> {
    let tenant = narrowing
        .tenant
        .as_deref()
        .map(TenantId::new)
        .transpose()
        .map_err(unprocessable)?;
    let actor = narrowing
        .actor
        .as_deref()
        .map(UserId::new)
        .transpose()
        .map_err(unprocessable)?;
    let entity = narrowing
        .entity
        .as_deref()
        .map(Entity::named)
        .transpose()
        .map_err(unprocessable)?;

    if !manager.is_system_administrator() {
        let Some(named) = tenant.clone() else {
            return Err(Box::new(
                (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "name the tenant to read the trail of",
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
    Ok(AuditFilter {
        tenant,
        actor,
        entity,
    })
}

/// A value the caller spelled wrongly, reported as the caller's mistake rather than ours.
fn unprocessable<E: std::fmt::Display>(error: E) -> Refusal {
    Box::new((StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response())
}

/// The `tenant`, `actor` and `entity` a caller narrows the trail by.
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
    use ip_core::{ApiId, Grant, TnKey};
    use ip_storage::{
        AccountKind, GrantStore, Membership, MembershipStore, NewTenant, NewUser, SqliteStore,
        Standing, TenantStore, UserStore,
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

    fn by(actor: &str, through: &'static str) -> ip_storage::By {
        ip_storage::By::new(user(actor), through)
    }

    /// Two tenants and four accounts, which is already a trail: every write below records a
    /// change, so this fixture needs no changes fabricated on top of it.
    ///
    /// acme is owned by alice with bob a member, other is owned by carol, and root is the
    /// system administrator. root made all of it except the one grant alice hands out, so
    /// narrowing by actor has something to find.
    ///
    /// Ten rows: two tenants, four accounts, three attachments, one grant. Four of them are
    /// acme's — its own creation, its two attachments and the grant — and two are other's.
    async fn store() -> Arc<SqliteStore> {
        let store = SqliteStore::in_memory().await.unwrap();
        for id in ["acme", "other"] {
            store
                .create_tenant(
                    NewTenant {
                        id: tenant(id),
                        key: TnKey::generate().unwrap(),
                    },
                    &by("root", "/_ip/tenants"),
                )
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
                .create_user(
                    NewUser {
                        id: user(id),
                        passphrase: hashed(),
                        kind,
                    },
                    &by("root", "/_ip/users"),
                )
                .await
                .unwrap();
        }
        for (id, tenant_id, standing) in [
            ("alice", "acme", Standing::Owner),
            ("bob", "acme", Standing::Member),
            ("carol", "other", Standing::Owner),
        ] {
            store
                .attach(
                    Membership {
                        user: user(id),
                        tenant: tenant(tenant_id),
                        standing,
                    },
                    &by("root", "/_ip/tenants/{tenant}/members"),
                )
                .await
                .unwrap();
        }
        store
            .grant(
                &Grant::new(
                    Capability::ApiAccess,
                    CapabilityScope::Api {
                        user: user("bob"),
                        tenant: tenant("acme"),
                        api: ApiId::new("chat").unwrap(),
                    },
                )
                .unwrap(),
                &by("alice", "/_ip/tenants/{tenant}/members/{user}/grants"),
            )
            .await
            .unwrap();
        Arc::new(store)
    }

    async fn read(router: &Router, state: &AppState, caller: &str, query: &str) -> Response<Body> {
        router
            .clone()
            .oneshot(request(
                "GET",
                &format!("/_ip/audit{query}"),
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
    async fn the_administrator_reads_every_change() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let trail = listed(read(&router, &state, "root", "").await).await;
        assert_eq!(trail.as_array().unwrap().len(), 10);
    }

    #[tokio::test]
    async fn the_trail_comes_newest_first() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let trail = listed(read(&router, &state, "root", "").await).await;
        assert_eq!(
            trail[0]["entity"], "grant",
            "the grant was the last change made"
        );
        assert_eq!(trail[0]["actor"], "alice");
        assert_eq!(trail[0]["entity_id"], "api_access on bob@acme/chat");
        assert_eq!(
            trail[0]["through"],
            "/_ip/tenants/{tenant}/members/{user}/grants"
        );
        assert_eq!(trail[0]["tenant"], "acme");
        assert!(
            trail[0]["before"].is_null(),
            "nothing was there before, so the key is left out"
        );
        assert_eq!(trail[0]["after"]["capability"], "api_access");
        assert!(trail[0]["created_at"].is_number(), "unix seconds");
    }

    #[tokio::test]
    async fn an_owner_reads_its_own_tenant() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let trail = listed(read(&router, &state, "alice", "?tenant=acme").await).await;
        assert_eq!(trail.as_array().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn an_owner_reads_no_other_tenant() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let refused = read(&router, &state, "alice", "?tenant=other").await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    }

    /// An account is recorded against no tenant, so naming one never reaches it. This is the
    /// isolation the whole endpoint rests on: a tenant owner cannot read who created an
    /// account, nor what any other tenant did, because both are recorded outside its name.
    #[tokio::test]
    async fn an_owner_reads_nothing_the_deployment_holds() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let trail = listed(read(&router, &state, "alice", "?tenant=acme").await).await;
        for one in trail.as_array().unwrap() {
            assert_eq!(one["tenant"], "acme");
            assert_ne!(one["entity"], "user", "accounts belong to the deployment");
        }
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
                &by("alice", "/_ip/tenants/{tenant}/members/{user}/grants"),
            )
            .await
            .unwrap();
        let trail = listed(read(&router, &state, "bob", "?tenant=acme").await).await;
        assert_eq!(
            trail.as_array().unwrap().len(),
            5,
            "acme's four, and the fifth is bob being handed the right to read them"
        );
    }

    #[tokio::test]
    async fn anyone_but_the_administrator_names_the_tenant_it_is_asking_about() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let refused = read(&router, &state, "alice", "").await;
        assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn the_trail_is_narrowed_by_actor_and_by_entity() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let by_actor = listed(read(&router, &state, "root", "?actor=alice").await).await;
        assert_eq!(by_actor.as_array().unwrap().len(), 1);
        assert_eq!(by_actor[0]["entity"], "grant");

        let by_entity = listed(read(&router, &state, "root", "?entity=user").await).await;
        assert_eq!(by_entity.as_array().unwrap().len(), 4);

        let both = listed(read(&router, &state, "root", "?actor=root&entity=tenant").await).await;
        assert_eq!(both.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_kind_of_thing_this_build_does_not_carry_is_the_callers_mistake() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let refused = read(&router, &state, "root", "?entity=unicorn").await;
        assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn a_name_that_is_not_one_is_the_callers_mistake() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        for query in ["?tenant=not%20a%20tenant", "?actor=not%20an%20account"] {
            let refused = read(&router, &state, "root", query).await;
            assert_eq!(
                refused.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{query} names nothing this deployment could hold"
            );
        }
    }

    #[tokio::test]
    async fn a_page_holds_what_it_was_asked_for() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let first = listed(read(&router, &state, "root", "?limit=4").await).await;
        assert_eq!(first.as_array().unwrap().len(), 4);
        let rest = listed(read(&router, &state, "root", "?limit=4&offset=8").await).await;
        assert_eq!(rest.as_array().unwrap().len(), 2, "ten rows, eight skipped");
    }
}
