use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use ip_core::Timestamp;
use ip_mcp::{McpAuth, McpEndpoint, McpId, McpName, McpServer};
use ip_storage::{By, StoredMcp};

use super::{Paged, store_refusal};
use crate::manage::Manager;
use crate::state::AppState;

/// The MCP servers an operator has registered, under the prefix the router nests it at.
///
/// A server is the deployment's, as a price is and unlike an agent template: `DESIGN.md` makes
/// `MCPs` a page of system global settings, so one id names one server for everybody and only
/// the system administrator manages them.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list))
        .route("/{mcp}", get(read).put(put).delete(remove))
}

/// What the audit trail records a server registered or removed as having come through.
const ENDPOINT: &str = "/_ip/mcps";

/// One MCP server, as the api reports it.
///
/// **There is no token here, and no field this type could carry one in.** What a reader needs
/// of a credential is whether one is required, which `auth` says; the token itself is written
/// and never read back, the way a tenant key and a passphrase are.
#[derive(Debug, serde::Serialize)]
pub struct McpView {
    /// Which server this is.
    pub id: String,
    /// What it is called.
    pub name: String,
    /// Where it answers.
    pub endpoint: String,
    /// Which kind of credential it requires: `none`, or `bearer` where a token is held for it.
    pub auth: &'static str,
    /// When it was registered, in seconds since the unix epoch.
    pub created_at: Timestamp,
}

impl From<StoredMcp> for McpView {
    fn from(stored: StoredMcp) -> Self {
        Self {
            id: stored.server.id.as_str().to_owned(),
            name: stored.server.name.as_str().to_owned(),
            endpoint: stored.server.endpoint.as_str().to_owned(),
            auth: stored.server.auth.name(),
            created_at: stored.created_at,
        }
    }
}

/// A server as the api takes it. Which one it is comes from the path, not from here.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Written {
    /// What it is called.
    name: McpName,
    /// Where it answers.
    endpoint: McpEndpoint,
    /// What it requires; nothing at all when left out.
    auth: Option<McpAuth>,
}

/// Every server registered, newest first.
async fn list(
    State(state): State<AppState>,
    manager: Manager,
    Paged(page): Paged,
) -> Response<Body> {
    if let Some(refusal) = refused(&manager) {
        return refusal;
    }
    match state.store().mcps(page).await {
        Ok(held) => Json(held.into_iter().map(McpView::from).collect::<Vec<_>>()).into_response(),
        Err(error) => store_refusal(error),
    }
}

/// One server, or a report that none is registered under the name.
async fn read(
    State(state): State<AppState>,
    manager: Manager,
    Path(id): Path<String>,
) -> Response<Body> {
    if let Some(refusal) = refused(&manager) {
        return refusal;
    }
    let id = match McpId::new(&id) {
        Ok(id) => id,
        Err(error) => return unprocessable(error),
    };
    match state.store().mcp(&id).await {
        Ok(Some(held)) => Json(McpView::from(held)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => store_refusal(error),
    }
}

/// Registers a server under the name the path gives it, replacing whatever was there.
///
/// A whole server, not a part of one: a body naming no credential registers a server that
/// requires none, so rewriting one to change its name alone drops the token it held. What was
/// stored is answered with, so a caller that did not mean to is told at once.
///
/// Nothing is told to reload: a server is read when something reaches a tool over it.
async fn put(
    State(state): State<AppState>,
    manager: Manager,
    Path(id): Path<String>,
    body: Bytes,
) -> Response<Body> {
    if let Some(refusal) = refused(&manager) {
        return refusal;
    }
    let id = match McpId::new(&id) {
        Ok(id) => id,
        Err(error) => return unprocessable(error),
    };
    let Json(written) = match Json::<Written>::from_bytes(&body) {
        Ok(written) => written,
        Err(rejection) => return rejection.into_response(),
    };
    let server = McpServer {
        id,
        name: written.name,
        endpoint: written.endpoint,
        auth: written.auth.unwrap_or(McpAuth::None),
    };
    let by = By::new(manager.user().clone(), ENDPOINT);
    match state.store().put_mcp(server, &by).await {
        // The server as stored, so the caller learns when it landed without asking again.
        Ok(stored) => (StatusCode::CREATED, Json(McpView::from(stored))).into_response(),
        Err(error) => store_refusal(error),
    }
}

/// Removes a server, or reports that none is registered under the name.
///
/// A template naming a tool of this server is left alone: what a run cannot reach is refused
/// when it resolves its tools, not here.
async fn remove(
    State(state): State<AppState>,
    manager: Manager,
    Path(id): Path<String>,
) -> Response<Body> {
    if let Some(refusal) = refused(&manager) {
        return refusal;
    }
    let id = match McpId::new(&id) {
        Ok(id) => id,
        Err(error) => return unprocessable(error),
    };
    let by = By::new(manager.user().clone(), ENDPOINT);
    match state.store().remove_mcp(&id, &by).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => store_refusal(error),
    }
}

/// Refuses every caller but the system administrator, telling it nothing more than that.
///
/// There is no capability for this and no tenant to hold one in, because there is no tenant's
/// MCP server to manage: the registry is the deployment's, as the prices and the routing rules
/// are.
fn refused(manager: &Manager) -> Option<Response<Body>> {
    (!manager.is_system_administrator()).then(|| StatusCode::FORBIDDEN.into_response())
}

/// A value the caller spelled wrongly, reported as the caller's mistake rather than ours.
fn unprocessable<E: std::fmt::Display>(error: E) -> Response<Body> {
    (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ip_core::{TenantId, TnKey, UserId};
    use ip_storage::{
        AccountKind, AuditFilter, Entity, Membership, MembershipStore, NewTenant, NewUser, Page,
        SqliteStore, Standing, TenantStore, UserStore,
    };
    use tower::ServiceExt;

    use super::super::harness::{body_of, cookie_of, hashed, request, state_over_shared};
    use super::*;

    /// The credential every test below writes, and the one none of them may read back.
    const TOKEN: &str = "wh-live-9f2c4a7b1e";

    fn user(raw: &str) -> UserId {
        UserId::new(raw).unwrap()
    }

    fn by(actor: &str, through: &'static str) -> By {
        By::new(user(actor), through)
    }

    /// One tenant and three accounts: root administers, alice owns acme and bob is in it.
    ///
    /// Alice and bob are here because the refusal that matters is not an anonymous one — a
    /// tenant owner holding every capability inside its own tenant still manages no server.
    async fn store() -> Arc<SqliteStore> {
        let store = SqliteStore::in_memory().await.unwrap();
        store
            .create_tenant(
                NewTenant {
                    id: TenantId::new("acme").unwrap(),
                    key: TnKey::generate().unwrap(),
                },
                &by("root", "/_ip/tenants"),
            )
            .await
            .unwrap();
        for (id, kind) in [
            ("root", AccountKind::SystemAdministrator),
            ("alice", AccountKind::Regular),
            ("bob", AccountKind::Regular),
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
        for (id, standing) in [("alice", Standing::Owner), ("bob", Standing::Member)] {
            store
                .attach(
                    Membership {
                        user: user(id),
                        tenant: TenantId::new("acme").unwrap(),
                        standing,
                    },
                    &by("root", "/_ip/tenants/{tenant}/members"),
                )
                .await
                .unwrap();
        }
        Arc::new(store)
    }

    async fn fixture() -> (Router, AppState) {
        let state = state_over_shared(store().await);
        (super::super::router(state.clone()), state)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "a test fixture: one request, whose six parts a composite would only move"
    )]
    async fn send(
        router: &Router,
        state: &AppState,
        method: &str,
        caller: &str,
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

    /// A server as a caller writes one, without the id the path carries.
    fn written(name: &str) -> String {
        format!(r#"{{"name":"{name}","endpoint":"https://mcp.acme.test/sse"}}"#)
    }

    /// The same, requiring a bearer token.
    fn written_with_a_token(name: &str) -> String {
        format!(
            r#"{{"name":"{name}","endpoint":"https://mcp.acme.test/sse","auth":{{"kind":"bearer","token":"{TOKEN}"}}}}"#
        )
    }

    #[tokio::test]
    async fn a_server_registered_is_a_server_read_back() {
        let (router, state) = fixture().await;

        let written = send(
            &router,
            &state,
            "PUT",
            "root",
            "/_ip/mcps/warehouse",
            Some(&written("Warehouse tools")),
        )
        .await;
        assert_eq!(written.status(), StatusCode::CREATED);
        let body = body_of(written).await;
        let view: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(view["id"], "warehouse");
        assert_eq!(view["name"], "Warehouse tools");
        assert_eq!(view["endpoint"], "https://mcp.acme.test/sse");
        assert_eq!(view["auth"], "none");
        assert!(view["created_at"].is_number(), "{body}");

        let read = send(&router, &state, "GET", "root", "/_ip/mcps/warehouse", None).await;
        assert_eq!(read.status(), StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body_of(read).await).unwrap(),
            view
        );
    }

    /// The property this endpoint exists to keep: a token goes in and never comes out.
    #[tokio::test]
    async fn a_credential_is_never_answered_with() {
        let (router, state) = fixture().await;

        let written = send(
            &router,
            &state,
            "PUT",
            "root",
            "/_ip/mcps/warehouse",
            Some(&written_with_a_token("Warehouse tools")),
        )
        .await;
        assert_eq!(written.status(), StatusCode::CREATED);

        let read = send(&router, &state, "GET", "root", "/_ip/mcps/warehouse", None).await;
        let listed = send(&router, &state, "GET", "root", "/_ip/mcps", None).await;
        for (what, answer) in [
            ("the write", written),
            ("the read", read),
            ("the list", listed),
        ] {
            let body = body_of(answer).await;
            assert!(
                body.contains(r#""auth":"bearer""#),
                "{what} must say a credential is required: {body}"
            );
            assert!(
                !body.contains(TOKEN),
                "{what} answered with the credential: {body}"
            );
            assert!(
                !body.contains("token"),
                "{what} carries a field for one: {body}"
            );
        }
    }

    /// A caller rewriting a server and naming no credential is answered that it requires none,
    /// which is what it now does. A write replaces the whole server.
    #[tokio::test]
    async fn a_rewrite_that_names_no_credential_leaves_the_server_requiring_none() {
        let (router, state) = fixture().await;
        send(
            &router,
            &state,
            "PUT",
            "root",
            "/_ip/mcps/warehouse",
            Some(&written_with_a_token("Warehouse tools")),
        )
        .await;

        let again = send(
            &router,
            &state,
            "PUT",
            "root",
            "/_ip/mcps/warehouse",
            Some(&written("Renamed")),
        )
        .await;
        assert_eq!(again.status(), StatusCode::CREATED);
        assert!(body_of(again).await.contains(r#""auth":"none""#));

        let held = state
            .store()
            .mcp(&McpId::new("warehouse").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(held.server.auth, McpAuth::None, "a token outlived its need");
    }

    #[tokio::test]
    async fn nobody_but_the_administrator_touches_a_server() {
        let (router, state) = fixture().await;
        send(
            &router,
            &state,
            "PUT",
            "root",
            "/_ip/mcps/warehouse",
            Some(&written("Warehouse tools")),
        )
        .await;

        for caller in ["alice", "bob"] {
            for (method, uri, body) in [
                ("GET", "/_ip/mcps", None),
                ("GET", "/_ip/mcps/warehouse", None),
                (
                    "PUT",
                    "/_ip/mcps/theirs",
                    Some(written("Theirs")).as_deref().map(str::to_owned),
                ),
                ("DELETE", "/_ip/mcps/warehouse", None),
            ] {
                let response = send(&router, &state, method, caller, uri, body.as_deref()).await;
                assert_eq!(
                    response.status(),
                    StatusCode::FORBIDDEN,
                    "{caller} reached {method} {uri}"
                );
            }
        }

        // And the refusals left the registry as it was.
        assert_eq!(
            state.store().mcps(Page::default()).await.unwrap().len(),
            1,
            "a refused write landed anyway"
        );
    }

    /// A caller who may not write is refused before its body is read at all.
    ///
    /// Asked with a body that would not deserialize: a 422 here would mean the endpoint reported
    /// on something it had no business reading, and told a stranger which fields it takes.
    #[tokio::test]
    async fn a_caller_that_may_not_write_is_refused_before_its_body_is_read() {
        let (router, state) = fixture().await;
        let response = send(
            &router,
            &state,
            "PUT",
            "alice",
            "/_ip/mcps/warehouse",
            Some(r#"{"nothing":"of ours"}"#),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// A caller that proved nothing is told to authenticate, not that it may not.
    #[tokio::test]
    async fn a_caller_that_proved_nothing_reads_nothing() {
        let (router, _) = fixture().await;
        let response = router
            .clone()
            .oneshot(request("GET", "/_ip/mcps", "", None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_server_nobody_registered_is_not_there_to_read_or_remove() {
        let (router, state) = fixture().await;
        for method in ["GET", "DELETE"] {
            let response = send(&router, &state, method, "root", "/_ip/mcps/absent", None).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method}");
        }
    }

    #[tokio::test]
    async fn a_name_that_is_not_an_id_is_the_callers_mistake() {
        let (router, state) = fixture().await;
        for method in ["GET", "DELETE"] {
            let response = send(
                &router,
                &state,
                method,
                "root",
                "/_ip/mcps/not%20an%20id",
                None,
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{method}"
            );
        }
    }

    #[tokio::test]
    async fn a_body_no_type_of_ours_will_take_is_refused() {
        let (router, state) = fixture().await;
        for body in [
            r#"{"name":"Warehouse"}"#,
            r#"{"endpoint":"https://mcp.acme.test/sse"}"#,
            r#"{"name":" Warehouse","endpoint":"https://mcp.acme.test/sse"}"#,
            r#"{"name":"Warehouse","endpoint":"ftp://mcp.acme.test"}"#,
            r#"{"name":"Warehouse","endpoint":"https://mcp.acme.test/sse","auth":{"kind":"mtls"}}"#,
            r#"{"name":"Warehouse","endpoint":"https://mcp.acme.test/sse","auth":{"kind":"bearer"}}"#,
            r#"{"name":"Warehouse","endpoint":"https://mcp.acme.test/sse","auth":{"kind":"bearer","token":""}}"#,
            r#"{"name":"Warehouse","endpoint":"https://mcp.acme.test/sse","id":"elsewhere"}"#,
        ] {
            let response = send(
                &router,
                &state,
                "PUT",
                "root",
                "/_ip/mcps/warehouse",
                Some(body),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{body} was taken"
            );
        }
    }

    #[tokio::test]
    async fn servers_are_listed_newest_first_a_page_at_a_time() {
        let (router, state) = fixture().await;
        for id in ["first", "second", "third"] {
            send(
                &router,
                &state,
                "PUT",
                "root",
                &format!("/_ip/mcps/{id}"),
                Some(&written("Named")),
            )
            .await;
        }

        let newest = send(&router, &state, "GET", "root", "/_ip/mcps?limit=2", None).await;
        let rest = send(
            &router,
            &state,
            "GET",
            "root",
            "/_ip/mcps?limit=2&offset=2",
            None,
        )
        .await;

        assert_eq!(named(newest).await, ["third", "second"]);
        assert_eq!(named(rest).await, ["first"]);
    }

    /// The ids a listing answered with, in the order it answered.
    async fn named(response: Response<Body>) -> Vec<String> {
        let body = body_of(response).await;
        serde_json::from_str::<Vec<serde_json::Value>>(&body)
            .unwrap_or_else(|error| panic!("{error}: {body}"))
            .into_iter()
            .map(|held| held["id"].as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn a_server_removed_is_gone() {
        let (router, state) = fixture().await;
        send(
            &router,
            &state,
            "PUT",
            "root",
            "/_ip/mcps/warehouse",
            Some(&written("Warehouse tools")),
        )
        .await;

        let removed = send(
            &router,
            &state,
            "DELETE",
            "root",
            "/_ip/mcps/warehouse",
            None,
        )
        .await;
        assert_eq!(removed.status(), StatusCode::NO_CONTENT);
        assert!(
            state
                .store()
                .mcps(Page::default())
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// Who registered it, through what, and with no sight of the credential in the row.
    #[tokio::test]
    async fn a_write_is_recorded_against_the_endpoint_it_came_through() {
        let (router, state) = fixture().await;
        send(
            &router,
            &state,
            "PUT",
            "root",
            "/_ip/mcps/warehouse",
            Some(&written_with_a_token("Warehouse tools")),
        )
        .await;

        let trail = state
            .store()
            .audits(
                &AuditFilter {
                    actor: Some(user("root")),
                    entity: Some(Entity::Mcp),
                    ..AuditFilter::default()
                },
                Page::default(),
            )
            .await
            .unwrap();

        assert_eq!(trail.len(), 1);
        let one = &trail[0];
        assert_eq!(one.through, ENDPOINT);
        assert_eq!(one.entity_id, "warehouse");
        assert_eq!(one.tenant, None);
        let recorded = serde_json::to_string(&one.after).unwrap();
        assert!(recorded.contains(r#""auth":"bearer""#), "{recorded}");
        assert!(!recorded.contains(TOKEN), "the trail holds it: {recorded}");
    }

    /// Every store call a request makes carries its own "the store could not answer" arm,
    /// and a database that works reaches none of them. Failing each in turn reaches them all.
    ///
    /// The sweep starts at the first call of the whole request — the extractor's own read of
    /// the caller's standing, where there is one — and the count is asserted, so a handler
    /// that gains or loses a store call fails here rather than quietly leaving a new arm
    /// untested. Each refused request changes nothing, which is what lets one fixture serve
    /// a whole sweep.
    #[tokio::test]
    async fn a_store_that_cannot_answer_is_the_server_s_own_failure() {
        let registering = written("Ledger tools");
        for (method, uri, body, calls) in [
            ("GET", "/_ip/mcps", None, 2),
            ("GET", "/_ip/mcps/ledger", None, 2),
            ("PUT", "/_ip/mcps/ledger", Some(registering.as_str()), 2),
            ("DELETE", "/_ip/mcps/ledger", None, 2),
        ] {
            let (router, state) = fixture().await;
            send(
                &router,
                &state,
                "PUT",
                "root",
                "/_ip/mcps/ledger",
                Some(&registering),
            )
            .await;
            let cookie = cookie_of(&state, &user("root")).await;
            let mut refused = 0;

            loop {
                state.store_failure().answer_only(refused);
                let response = router
                    .clone()
                    .oneshot(request(method, uri, &cookie, body))
                    .await
                    .unwrap();
                state.store_failure().answer_again();
                if response.status() != StatusCode::INTERNAL_SERVER_ERROR {
                    break;
                }
                refused += 1;
                assert!(refused <= 9, "{method} {uri} refuses whatever it is given");
            }

            assert_eq!(refused, calls, "{method} {uri}");
        }
    }
}
