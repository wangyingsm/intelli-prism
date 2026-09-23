use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use ip_core::{RouteKey, RouteRule, Timestamp};
use ip_storage::Page;

use super::{Paged, store_refusal};
use crate::manage::Manager;
use crate::state::AppState;

/// The routing rules the database holds, under the prefix the router nests them at.
///
/// Rules are global rather than a tenant's, so only the system administrator manages them.
/// A change is stored at once but reaches the running routing table only when it is rebuilt.
pub fn router() -> Router<AppState> {
    Router::new().route("/", get(list).put(put).delete(remove))
}

/// One rule in force, and where it comes from.
#[derive(Debug, serde::Serialize)]
pub struct RouteView<'a> {
    /// `config` for a rule the configuration file contributes, `stored` for one the
    /// database holds.
    pub source: &'static str,
    /// Whether a configured rule claims the same key, which leaves a stored rule unused.
    pub shadowed: bool,
    /// When a stored rule was stored, in seconds since the unix epoch; a configured rule has
    /// no such moment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<Timestamp>,
    /// The rule itself.
    #[serde(flatten)]
    pub rule: &'a RouteRule,
}

/// Every rule in force: the configured ones first, since they have no time to order them
/// by, then the stored ones newest first. A page that names a moment holds stored rules only,
/// since no configured rule was created after anything.
async fn list(
    State(state): State<AppState>,
    manager: Manager,
    Paged(page): Paged,
) -> Response<Body> {
    if !manager.is_system_administrator() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let configured = state.configured();
    let listed_configured = match page.after() {
        Some(_) => &[][..],
        None => configured,
    };
    let offset = page.offset() as usize;
    let limit = page.limit() as usize;
    let mut views: Vec<RouteView<'_>> = listed_configured
        .iter()
        .skip(offset)
        .take(limit)
        .map(|rule| RouteView {
            source: "config",
            shadowed: false,
            created_at: None,
            rule,
        })
        .collect();

    let room = limit - views.len();
    let stored = match room {
        0 => Vec::new(),
        room => {
            let skipped = offset.saturating_sub(listed_configured.len());
            let stored_page = Page::new(
                u32::try_from(room).unwrap_or(u32::MAX),
                u32::try_from(skipped).unwrap_or(u32::MAX),
                page.after(),
            );
            match state.store().list_routes(stored_page).await {
                Ok(stored) => stored,
                Err(error) => return store_refusal(error),
            }
        }
    };
    views.extend(stored.iter().map(|listed| RouteView {
        source: "stored",
        shadowed: claims(configured, &listed.item.key),
        created_at: Some(listed.created_at),
        rule: &listed.item,
    }));
    Json(views).into_response()
}

/// Stores a rule, replacing whatever its key routed to before.
///
/// A rule the table would refuse is refused here, since a stored rule the table cannot take
/// stops the server from starting. So is one under a key the configuration claims, which
/// would sit in the database doing nothing.
async fn put(State(state): State<AppState>, manager: Manager, body: Bytes) -> Response<Body> {
    if !manager.is_system_administrator() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Json(rule) = match Json::<RouteRule>::from_bytes(&body) {
        Ok(rule) => rule,
        Err(rejection) => return rejection.into_response(),
    };
    let path = &rule.key.endpoint().path;
    if path.is_reserved() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("{path} belongs to the gateway itself"),
        )
            .into_response();
    }
    if claims(state.configured(), &rule.key) {
        return (
            StatusCode::CONFLICT,
            format!("the configuration routes {} already", rule.key),
        )
            .into_response();
    }
    match state.store().put_route(rule).await {
        Ok(()) => {
            state.feed().after_change().await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => store_refusal(error),
    }
}

/// Removes the stored rule under a key.
async fn remove(State(state): State<AppState>, manager: Manager, body: Bytes) -> Response<Body> {
    if !manager.is_system_administrator() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Json(key) = match Json::<RouteKey>::from_bytes(&body) {
        Ok(key) => key,
        Err(rejection) => return rejection.into_response(),
    };
    match state.store().remove_route(&key).await {
        Ok(()) => {
            state.feed().after_change().await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => store_refusal(error),
    }
}

/// Whether one of these rules routes the key.
fn claims(rules: &[RouteRule], key: &RouteKey) -> bool {
    rules.iter().any(|rule| &rule.key == key)
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use ip_core::{
        AbsPath, ApiId, Endpoint, Host, Port, Protocol, RouteTarget, TenantId, TnKey, UserId,
    };
    use ip_storage::{
        AccountKind, Membership, MembershipStore, NewTenant, NewUser, RouteStore, SqliteStore,
        Standing, TenantStore, UserStore,
    };
    use tower::ServiceExt;

    use super::super::harness::{body_of, cookie_of, hashed, request, state_over};
    use super::*;

    fn id(raw: &str) -> UserId {
        UserId::new(raw).unwrap()
    }

    fn endpoint(host: &str, path: &str) -> Endpoint {
        Endpoint::new(
            Protocol::Https,
            Host::new(host).unwrap(),
            Port::new(443).unwrap(),
            AbsPath::new(path).unwrap(),
        )
    }

    fn rule(path: &str, upstream: &str) -> RouteRule {
        RouteRule {
            api: ApiId::new("chat").unwrap(),
            key: RouteKey::new(endpoint("gateway.local", path)),
            target: RouteTarget::new(endpoint(upstream, "/v1")),
        }
    }

    fn json(rule: &RouteRule) -> String {
        serde_json::to_string(rule).unwrap()
    }

    /// A store holding root, the system administrator, and alice, who owns acme.
    async fn store() -> SqliteStore {
        let store = SqliteStore::in_memory().await.unwrap();
        store
            .create_tenant(NewTenant {
                id: TenantId::new("acme").unwrap(),
                key: TnKey::generate().unwrap(),
            })
            .await
            .unwrap();
        for (user, kind) in [
            ("root", AccountKind::SystemAdministrator),
            ("alice", AccountKind::Regular),
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
        store
            .attach(Membership {
                user: id("alice"),
                tenant: TenantId::new("acme").unwrap(),
                standing: Standing::Owner,
            })
            .await
            .unwrap();
        store
    }

    async fn call(
        router: &Router,
        state: &AppState,
        caller: &str,
        method: &str,
        body: Option<&str>,
    ) -> axum::http::Response<Body> {
        router
            .clone()
            .oneshot(request(
                method,
                "/_ip/routes",
                &cookie_of(state, &id(caller)).await,
                body,
            ))
            .await
            .unwrap()
    }

    async fn listed(router: &Router, state: &AppState) -> serde_json::Value {
        let response = call(router, state, "root", "GET", None).await;
        assert_eq!(response.status(), StatusCode::OK);
        serde_json::from_str(&body_of(response).await).unwrap()
    }

    #[tokio::test]
    async fn the_administrator_stores_a_rule_and_reads_it_back() {
        let state = state_over(store().await);
        let router = crate::routes::router(state.clone());
        let stored = call(
            &router,
            &state,
            "root",
            "PUT",
            Some(&json(&rule("/v1", "api.example.com"))),
        )
        .await;
        assert_eq!(stored.status(), StatusCode::NO_CONTENT);

        let listed = listed(&router, &state).await;
        assert_eq!(listed.as_array().unwrap().len(), 1);
        assert_eq!(listed[0]["source"], "stored");
        assert_eq!(listed[0]["shadowed"], false);
        assert_eq!(listed[0]["api"], "chat");
        assert_eq!(listed[0]["key"]["path"], "/v1");
        assert_eq!(listed[0]["target"][0]["host"], "api.example.com");
    }

    #[tokio::test]
    async fn storing_a_key_again_replaces_where_it_goes() {
        let state = state_over(store().await);
        let router = crate::routes::router(state.clone());
        for upstream in ["first.example.com", "second.example.com"] {
            call(
                &router,
                &state,
                "root",
                "PUT",
                Some(&json(&rule("/v1", upstream))),
            )
            .await;
        }
        let listed = listed(&router, &state).await;
        assert_eq!(listed.as_array().unwrap().len(), 1);
        assert_eq!(listed[0]["target"][0]["host"], "second.example.com");
    }

    #[tokio::test]
    async fn only_the_administrator_touches_rules_whatever_it_sends() {
        let state = state_over(store().await);
        let router = crate::routes::router(state.clone());
        for (method, body) in [
            ("GET", None),
            ("PUT", Some("not json")),
            ("DELETE", Some("{}")),
        ] {
            let response = call(&router, &state, "alice", method, body).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method}");
        }
    }

    #[tokio::test]
    async fn a_rule_on_the_gateway_s_own_path_is_not_stored() {
        let state = state_over(store().await);
        let router = crate::routes::router(state.clone());
        let response = call(
            &router,
            &state,
            "root",
            "PUT",
            Some(&json(&rule("/_ip/admin", "api.example.com"))),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(state.store().routes().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_rule_the_types_refuse_is_not_stored() {
        let state = state_over(store().await);
        let router = crate::routes::router(state.clone());
        let good = serde_json::to_value(rule("/v1", "api.example.com")).unwrap();
        let mut bad_host = good.clone();
        bad_host["key"]["host"] = "not a host!".into();
        let mut no_target = good.clone();
        no_target["target"] = serde_json::json!([]);
        let mut zero_port = good;
        zero_port["key"]["port"] = 0.into();
        for body in [bad_host, no_target, zero_port] {
            let response = call(&router, &state, "root", "PUT", Some(&body.to_string())).await;
            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{body}"
            );
        }
        assert!(state.store().routes().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_key_the_configuration_routes_is_not_stored_again() {
        let configured = rule("/v1", "configured.example.com");
        let state = state_over(store().await).configuring(vec![configured.clone()]);
        let router = crate::routes::router(state.clone());

        let response = call(
            &router,
            &state,
            "root",
            "PUT",
            Some(&json(&rule("/v1", "api.example.com"))),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(state.store().routes().await.unwrap().is_empty());

        let listed = listed(&router, &state).await;
        assert_eq!(listed[0]["source"], "config");
        assert_eq!(listed[0]["target"][0]["host"], "configured.example.com");
    }

    #[tokio::test]
    async fn a_stored_rule_the_configuration_claims_is_listed_as_unused() {
        let store = store().await;
        store
            .put_route(rule("/v1", "stored.example.com"))
            .await
            .unwrap();
        let state = state_over(store).configuring(vec![rule("/v1", "configured.example.com")]);
        let router = crate::routes::router(state.clone());

        let listed = listed(&router, &state).await;
        let stored = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|view| view["source"] == "stored")
            .unwrap();
        assert_eq!(stored["shadowed"], true);
    }

    #[tokio::test]
    async fn a_rule_is_removed_once() {
        let state = state_over(store().await);
        let router = crate::routes::router(state.clone());
        let stored = rule("/v1", "api.example.com");
        call(&router, &state, "root", "PUT", Some(&json(&stored))).await;

        let key = serde_json::to_string(&stored.key).unwrap();
        let removed = call(&router, &state, "root", "DELETE", Some(&key)).await;
        assert_eq!(removed.status(), StatusCode::NO_CONTENT);
        assert!(state.store().routes().await.unwrap().is_empty());

        let again = call(&router, &state, "root", "DELETE", Some(&key)).await;
        assert_eq!(again.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn configured_rules_come_first_then_stored_ones_newest_first() {
        let store = store().await;
        for path in ["/stored-old", "/stored-new"] {
            store
                .put_route(rule(path, "api.example.com"))
                .await
                .unwrap();
        }
        let state = state_over(store).configuring(vec![rule("/configured", "api.example.com")]);
        let router = crate::routes::router(state.clone());
        let paths = |listed: serde_json::Value| -> Vec<String> {
            listed
                .as_array()
                .unwrap()
                .iter()
                .map(|view| view["key"]["path"].as_str().unwrap().to_owned())
                .collect()
        };
        let page = |query: &'static str| {
            let router = router.clone();
            let state = state.clone();
            async move {
                let response = router
                    .oneshot(request(
                        "GET",
                        &format!("/_ip/routes{query}"),
                        &cookie_of(&state, &id("root")).await,
                        None,
                    ))
                    .await
                    .unwrap();
                serde_json::from_str::<serde_json::Value>(&body_of(response).await).unwrap()
            }
        };

        assert_eq!(
            paths(page("?limit=2").await),
            ["/configured", "/stored-new"]
        );
        assert_eq!(
            paths(page("?offset=1").await),
            ["/stored-new", "/stored-old"]
        );
        assert_eq!(paths(page("?offset=2&limit=5").await), ["/stored-old"]);
        assert_eq!(
            paths(page("?after=0").await),
            ["/stored-new", "/stored-old"]
        );

        let listed = page("").await;
        assert!(listed[0].get("created_at").is_none());
        assert!(listed[1]["created_at"].is_i64());
    }

    #[tokio::test]
    async fn a_stored_rule_is_published_for_every_node() {
        let state = state_over(store().await);
        let router = crate::routes::router(state.clone());
        call(
            &router,
            &state,
            "root",
            "PUT",
            Some(&json(&rule("/v1", "api.example.com"))),
        )
        .await;

        let published = state.feed().published().await.unwrap().unwrap();
        let stored = state.store().rule_revision().await.unwrap();
        assert_eq!(published.revision, stored);
        assert_eq!(published.routes, [rule("/v1", "api.example.com")]);

        let key = serde_json::to_string(&rule("/v1", "api.example.com").key).unwrap();
        call(&router, &state, "root", "DELETE", Some(&key)).await;
        let published = state.feed().published().await.unwrap().unwrap();
        assert!(published.routes.is_empty());
        assert_eq!(
            published.revision,
            state.store().rule_revision().await.unwrap()
        );
    }

    #[tokio::test]
    async fn a_rule_stored_over_the_api_reaches_the_table_this_node_routes_by() {
        let state = state_over(store().await);
        let router = crate::routes::router(state.clone());
        assert!(state.gateway().table().is_empty());

        call(
            &router,
            &state,
            "root",
            "PUT",
            Some(&json(&rule("/v1", "api.example.com"))),
        )
        .await;
        state.reload_rules().await.unwrap();
        assert_eq!(state.gateway().table().len(), 1);

        let key = serde_json::to_string(&rule("/v1", "api.example.com").key).unwrap();
        call(&router, &state, "root", "DELETE", Some(&key)).await;
        state.reload_rules().await.unwrap();
        assert!(state.gateway().table().is_empty());
    }
}
