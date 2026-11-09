use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use ip_core::{Capability, CapabilityScope, TenantId};
use ip_graph::{
    GraphEdge, GraphError, GraphId, GraphName, GraphNode, GraphOwner, GraphSummary, Listing,
    MaxNodes, RunGraph, RunGraphParts,
};

use super::{Paged, store_refusal};
use crate::manage::Manager;
use crate::state::AppState;

/// A refusal, boxed so the ordinary answer is not paid for by every caller.
type Refusal = Box<Response<Body>>;

/// The run graphs an operator has stored, under the prefix the router nests it at.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list))
        .route("/{graph}", get(read).put(put).delete(remove))
}

/// One graph as a listing reports it: what it says about itself, without its nodes.
#[derive(Debug, serde::Serialize)]
pub struct GraphBrief {
    /// Which graph this is.
    pub id: String,
    /// What it is called.
    pub name: String,
    /// Whose it is, absent where it belongs to the deployment rather than to a tenant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// The most nodes it may hold.
    pub max_nodes: u32,
}

/// One whole graph, as the api reports it.
#[derive(Debug, serde::Serialize)]
pub struct GraphView {
    /// Which graph this is.
    pub id: String,
    /// What it is called.
    pub name: String,
    /// Whose it is, absent where it belongs to the deployment rather than to a tenant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// The nodes it holds.
    pub nodes: Vec<GraphNode>,
    /// The edges between them.
    pub edges: Vec<GraphEdge>,
    /// The most nodes it may hold.
    pub max_nodes: u32,
}

impl From<RunGraph> for GraphView {
    fn from(graph: RunGraph) -> Self {
        Self {
            id: graph.id().to_string(),
            name: graph.name().to_string(),
            tenant: graph.owner().tenant().map(ToString::to_string),
            max_nodes: graph.max_nodes().get(),
            nodes: graph.nodes().to_vec(),
            edges: graph.edges().to_vec(),
        }
    }
}

/// A graph as the api takes it. Which one it is comes from the path, not from here.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Written {
    /// What it is called.
    name: GraphName,
    /// The nodes it holds.
    nodes: Vec<GraphNode>,
    /// The edges between them; none at all when left out, which is a graph of one generation.
    #[serde(default)]
    edges: Vec<GraphEdge>,
    /// The most nodes it may hold.
    max_nodes: MaxNodes,
}

/// Whose graphs a request is about.
#[derive(Debug, Default, serde::Deserialize)]
pub struct Owned {
    /// The tenant whose graphs these are; the deployment's own when left out.
    tenant: Option<String>,
}

/// One owner's graphs, by id.
///
/// The page's `after` is ignored here, and alone among the management routes: the plane records no
/// time a graph was written, so there is no clock to page against. The id is what orders a
/// listing, which is what makes `offset` mean the same thing twice.
async fn list(
    State(state): State<AppState>,
    manager: Manager,
    Query(owned): Query<Owned>,
    Paged(page): Paged,
) -> Response<Body> {
    let plane = match plane_of(&state) {
        Ok(plane) => plane,
        Err(refusal) => return *refusal,
    };
    let owner = match owner_of(&state, &manager, owned).await {
        Ok(owner) => owner,
        Err(refusal) => return *refusal,
    };
    let listing = Listing {
        limit: page.limit(),
        offset: page.offset(),
    };
    match plane.graphs(&owner, listing).await {
        Ok(held) => Json(
            held.into_iter()
                .map(|summary| brief(summary, &owner))
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(error) => *graph_refusal(error),
    }
}

/// One graph, or a report that this owner keeps none under the id.
async fn read(
    State(state): State<AppState>,
    manager: Manager,
    Path(id): Path<String>,
    Query(owned): Query<Owned>,
) -> Response<Body> {
    let plane = match plane_of(&state) {
        Ok(plane) => plane,
        Err(refusal) => return *refusal,
    };
    let id = match GraphId::new(&id) {
        Ok(id) => id,
        Err(error) => return *unprocessable(error),
    };
    let owner = match owner_of(&state, &manager, owned).await {
        Ok(owner) => owner,
        Err(refusal) => return *refusal,
    };
    match plane.get(&owner, &id).await {
        Ok(Some(graph)) => Json(GraphView::from(graph)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => *graph_refusal(error),
    }
}

/// Stores a graph under the id the path gives it, replacing whatever was there.
///
/// Nothing is told to reload: a graph is read when something runs one, and a run reads it then.
async fn put(
    State(state): State<AppState>,
    manager: Manager,
    Path(id): Path<String>,
    Query(owned): Query<Owned>,
    body: Bytes,
) -> Response<Body> {
    let plane = match plane_of(&state) {
        Ok(plane) => plane,
        Err(refusal) => return *refusal,
    };
    let id = match GraphId::new(&id) {
        Ok(id) => id,
        Err(error) => return *unprocessable(error),
    };
    let owner = match owner_of(&state, &manager, owned).await {
        Ok(owner) => owner,
        Err(refusal) => return *refusal,
    };
    let Json(written) = match Json::<Written>::from_bytes(&body) {
        Ok(written) => written,
        Err(rejection) => return rejection.into_response(),
    };
    let graph = match RunGraph::checked(RunGraphParts {
        id,
        name: written.name,
        owner: owner.clone(),
        nodes: written.nodes,
        edges: written.edges,
        max_nodes: written.max_nodes,
    }) {
        Ok(graph) => graph,
        Err(error) => return *unprocessable(error),
    };
    match plane.put(&graph).await {
        // The graph as it was taken, so a caller sees what the store now holds.
        Ok(()) => (StatusCode::CREATED, Json(GraphView::from(graph))).into_response(),
        Err(error) => *graph_refusal(error),
    }
}

/// Removes a graph, or reports that this owner keeps none under the id.
async fn remove(
    State(state): State<AppState>,
    manager: Manager,
    Path(id): Path<String>,
    Query(owned): Query<Owned>,
) -> Response<Body> {
    let plane = match plane_of(&state) {
        Ok(plane) => plane,
        Err(refusal) => return *refusal,
    };
    let id = match GraphId::new(&id) {
        Ok(id) => id,
        Err(error) => return *unprocessable(error),
    };
    let owner = match owner_of(&state, &manager, owned).await {
        Ok(owner) => owner,
        Err(refusal) => return *refusal,
    };
    match plane.remove(&owner, &id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => *graph_refusal(error),
    }
}

/// One summary as the api reports it, under the owner the request asked as.
fn brief(summary: GraphSummary, owner: &GraphOwner) -> GraphBrief {
    GraphBrief {
        id: summary.id.to_string(),
        name: summary.name.to_string(),
        tenant: owner.tenant().map(ToString::to_string),
        max_nodes: summary.max_nodes.get(),
    }
}

/// Where the graphs are kept, or a report that this deployment keeps none.
///
/// A deployment with no `[graph_store]` has no graph feature rather than a graph store that is
/// down, which is why this is a 404 and not the 503 an unreachable plane answers: there is
/// nothing here to reach.
fn plane_of(state: &AppState) -> Result<&ip_graph::GraphPlane, Refusal> {
    state.graphs().map(AsRef::as_ref).ok_or_else(|| {
        Box::new((StatusCode::NOT_FOUND, "this deployment keeps no run graphs").into_response())
    })
}

/// How a graph store's refusal reads to a caller.
///
/// Three kinds, because they want three different things done about them. What a graph itself is
/// wrong about is the caller's to fix, so **422**. A plane that cannot be reached is nobody's
/// fault and worth trying again, so **503**. A plane that answers with rows no graph can be
/// rebuilt from is neither: trying again will answer the same way, and what is stored has to be
/// looked at, so **502** — the store is the gateway that answered badly.
fn graph_refusal(error: GraphError) -> Refusal {
    let said = error.to_string();
    match error {
        GraphError::Plane(error) => Box::new(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("the graph store could not be reached: {error}"),
            )
                .into_response(),
        ),
        GraphError::Foreign { .. }
        | GraphError::PropertyMissing { .. }
        | GraphError::PropertyKind { .. }
        | GraphError::Unreadable { .. }
        | GraphError::Truncated { .. }
        | GraphError::Unplaced => Box::new(
            (
                StatusCode::BAD_GATEWAY,
                format!("the graph store answered with something unusable: {said}"),
            )
                .into_response(),
        ),
        _ => Box::new((StatusCode::UNPROCESSABLE_ENTITY, said).into_response()),
    }
}

/// Whose graphs the caller may manage, or the refusal that says they may not.
///
/// The same rule as the agent templates': a sysadmin manages anyone's and the deployment's own,
/// and anybody else manages one named tenant's and only with [`Capability::SysAgent`] over it.
/// The capability is deliberately not a new one — a graph is a composition of templates, so
/// whoever may write the templates may write what composes them.
async fn owner_of(
    state: &AppState,
    manager: &Manager,
    owned: Owned,
) -> Result<GraphOwner, Refusal> {
    let named = owned
        .tenant
        .as_deref()
        .map(TenantId::new)
        .transpose()
        .map_err(unprocessable)?;

    let Some(named) = named else {
        return if manager.is_system_administrator() {
            Ok(GraphOwner::Global)
        } else {
            Err(Box::new(
                (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "name the tenant whose run graphs you manage",
                )
                    .into_response(),
            ))
        };
    };
    if manager.is_system_administrator() {
        return Ok(GraphOwner::Tenant(named));
    }
    let scope = CapabilityScope::Tenant {
        user: manager.user().clone(),
        tenant: named.clone(),
    };
    match manager
        .allows(state.store().as_ref(), Capability::SysAgent, &scope)
        .await
    {
        Ok(true) => Ok(GraphOwner::Tenant(named)),
        Ok(false) => Err(Box::new(StatusCode::FORBIDDEN.into_response())),
        Err(error) => Err(Box::new(store_refusal(error))),
    }
}

/// What a caller got wrong about the shape of what they asked.
fn unprocessable<E: std::fmt::Display>(error: E) -> Refusal {
    Box::new((StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ip_core::{Grant, TnKey, UserId};
    use ip_drsg::{Replies, SilentPlane, StubPlane};
    use ip_graph::{GRAPH_LABEL, GraphPlane, NODE_LABEL};
    use ip_storage::{
        AccountKind, By, GrantStore, Membership, MembershipStore, NewTenant, NewUser, SqliteStore,
        Standing, TenantStore, UserStore,
    };
    use tower::ServiceExt;

    use super::super::harness::{body_of, cookie_of, hashed, request, state_over_shared};
    use super::*;

    fn user(raw: &str) -> UserId {
        UserId::new(raw).unwrap()
    }

    fn tenant(raw: &str) -> TenantId {
        TenantId::new(raw).unwrap()
    }

    fn by(actor: &str, through: &'static str) -> By {
        By::new(user(actor), through)
    }

    /// Two tenants and four accounts, as the agent routes' own fixture has them.
    ///
    /// root administers, alice owns acme, bob is a member of it holding nothing, and carol owns
    /// other — so every refusal below has somebody it really belongs to.
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
                    Capability::SysAgent,
                    CapabilityScope::Tenant {
                        user: user("alice"),
                        tenant: tenant("acme"),
                    },
                )
                .unwrap(),
                &by("root", "/_ip/users/{user}/grants"),
            )
            .await
            .unwrap();
        Arc::new(store)
    }

    /// A graph as a caller writes one, without the id the path carries.
    const WRITTEN: &str = r#"{"name":"Support triage","nodes":[{"name":"intake","agent":"acme-triage"},{"name":"reply","agent":"acme-triage"}],"edges":[{"from":"intake","to":"reply"}],"max_nodes":8}"#;

    /// What the plane answers a `plane.algo` cycle check with when a graph is a DAG.
    fn no_cycle() -> serde_json::Value {
        serde_json::json!({"algo": "cycles", "count": 0, "results": []})
    }

    /// One node row as the plane answers it.
    fn node_row(id: u64, owner: &str, graph: &str, name: &str, agent: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "external_key": format!("{NODE_LABEL}/{owner}/{graph}/{name}"),
            "labels": [NODE_LABEL],
            "properties": {"graph": graph, "owner": owner, "name": name, "agent": agent},
        })
    }

    /// The graph's own row as the plane answers it.
    fn graph_row(owner: &str, graph: &str) -> serde_json::Value {
        serde_json::json!({
            "id": 1,
            "external_key": format!("{GRAPH_LABEL}/{owner}/{graph}"),
            "labels": [GRAPH_LABEL],
            "properties": {
                "graph": graph, "owner": owner, "name": "Support triage", "max_nodes": 8,
            },
        })
    }

    /// The router over a store and a plane that answers as scripted.
    async fn over(replies: Replies) -> (Router, AppState, StubPlane) {
        let plane = StubPlane::start(replies).await;
        let state =
            state_over_shared(store().await).with_graphs(Arc::new(GraphPlane::new(plane.client())));
        (super::super::router(state.clone()), state, plane)
    }

    /// The router over a store and no plane at all, which is a deployment with no graph feature.
    async fn without_a_plane() -> (Router, AppState) {
        let state = state_over_shared(store().await);
        (super::super::router(state.clone()), state)
    }

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

    #[tokio::test]
    async fn a_graph_is_written_as_the_rows_a_plane_holds() {
        let (router, state, plane) = over(Replies::scripted([
            ("plane.query", serde_json::json!([])),
            ("node.create", graph_row("acme", "triage")),
            ("edge.create", serde_json::json!({"id": 1})),
            ("plane.algo", no_cycle()),
        ]))
        .await;

        let answered = send(
            &router,
            &state,
            "PUT",
            "alice",
            "/_ip/graphs/triage?tenant=acme",
            Some(WRITTEN),
        )
        .await;

        assert_eq!(answered.status(), StatusCode::CREATED);
        let body = body_of(answered).await;
        let view: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(view["id"], "triage");
        assert_eq!(view["tenant"], "acme");
        assert_eq!(view["nodes"].as_array().unwrap().len(), 2);
        // One row for the graph and one per node, and every one of them the tenant's.
        let written = plane.calls("node.create");
        assert_eq!(written.len(), 3, "{written:?}");
        for row in &written {
            assert_eq!(row["properties"]["owner"], "acme", "{row}");
        }
        assert_eq!(plane.calls("edge.create").len(), 1);
        assert_eq!(
            plane.calls("plane.algo").len(),
            1,
            "no cycle was looked for"
        );
    }

    #[tokio::test]
    async fn a_graph_is_read_back_as_the_graph_that_was_stored() {
        let (router, state, _plane) = over(Replies::in_turn([
            serde_json::json!([graph_row("acme", "triage")]),
            serde_json::json!([
                node_row(2, "acme", "triage", "intake", "acme-triage"),
                node_row(3, "acme", "triage", "reply", "acme-triage"),
            ]),
            serde_json::json!({"columns": ["from", "to"], "rows": [["intake", "reply"]]}),
        ]))
        .await;

        let answered = send(
            &router,
            &state,
            "GET",
            "alice",
            "/_ip/graphs/triage?tenant=acme",
            None,
        )
        .await;

        assert_eq!(answered.status(), StatusCode::OK);
        let body = body_of(answered).await;
        let view: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(view["name"], "Support triage");
        assert_eq!(view["max_nodes"], 8);
        assert_eq!(view["edges"][0]["from"], "intake");
        assert_eq!(view["edges"][0]["to"], "reply");
    }

    #[tokio::test]
    async fn a_listing_asks_for_this_owners_graphs_and_the_page_it_was_given() {
        let (router, state, plane) = over(Replies::scripted([(
            "plane.query",
            serde_json::json!({
                "columns": ["graph", "name", "max_nodes", "owner"],
                "rows": [["triage", "Support triage", 8, "acme"]],
            }),
        )]))
        .await;

        let answered = send(
            &router,
            &state,
            "GET",
            "alice",
            "/_ip/graphs?tenant=acme&limit=5&offset=10",
            None,
        )
        .await;

        assert_eq!(answered.status(), StatusCode::OK);
        let body = body_of(answered).await;
        let listed: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["id"], "triage");
        assert_eq!(listed[0]["tenant"], "acme");
        let plan = &plane.calls("plane.query")[0]["plan"];
        assert_eq!(plan["project"]["limit"], 5);
        assert_eq!(plan["project"]["skip"], 10);
        assert_eq!(
            plan["steps"][0]["Filter"]["Compare"]["rhs"]["Literal"]["Str"], "acme",
            "a listing did not filter on the owner in the plan itself: {plan}"
        );
    }

    #[tokio::test]
    async fn a_graph_is_removed_rows_and_all() {
        let (router, state, plane) = over(Replies::in_turn([
            serde_json::json!([graph_row("acme", "triage")]),
            serde_json::json!([node_row(2, "acme", "triage", "intake", "acme-triage")]),
            serde_json::json!({"deleted": true}),
            serde_json::json!({"deleted": true}),
        ]))
        .await;

        let answered = send(
            &router,
            &state,
            "DELETE",
            "alice",
            "/_ip/graphs/triage?tenant=acme",
            None,
        )
        .await;

        assert_eq!(answered.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            plane.calls("node.delete").len(),
            2,
            "the graph's own row and its node are both taken out"
        );
    }

    #[tokio::test]
    async fn a_graph_this_owner_does_not_keep_is_not_found() {
        let (router, state, _plane) =
            over(Replies::scripted([("plane.query", serde_json::json!([]))])).await;

        for method in ["GET", "DELETE"] {
            let answered = send(
                &router,
                &state,
                method,
                "alice",
                "/_ip/graphs/absent?tenant=acme",
                None,
            )
            .await;
            assert_eq!(answered.status(), StatusCode::NOT_FOUND, "{method}");
        }
    }

    /// A deployment that never configured a graph store has no graph feature, not a broken one.
    #[tokio::test]
    async fn a_deployment_keeping_no_graphs_has_no_graph_routes() {
        let (router, state) = without_a_plane().await;

        for (method, uri, body) in [
            ("GET", "/_ip/graphs?tenant=acme", None),
            ("GET", "/_ip/graphs/triage?tenant=acme", None),
            ("PUT", "/_ip/graphs/triage?tenant=acme", Some(WRITTEN)),
            ("DELETE", "/_ip/graphs/triage?tenant=acme", None),
        ] {
            let answered = send(&router, &state, method, "alice", uri, body).await;
            assert_eq!(
                answered.status(),
                StatusCode::NOT_FOUND,
                "{method} {uri} answered as though a graph store were configured"
            );
        }
    }

    #[tokio::test]
    async fn a_caller_of_another_tenant_sees_nothing_of_this_one() {
        let (router, state, plane) =
            over(Replies::scripted([("plane.query", serde_json::json!([]))])).await;

        for (caller, expected) in [
            ("carol", StatusCode::FORBIDDEN),
            ("bob", StatusCode::FORBIDDEN),
        ] {
            let answered = send(
                &router,
                &state,
                "GET",
                caller,
                "/_ip/graphs/triage?tenant=acme",
                None,
            )
            .await;
            assert_eq!(answered.status(), expected, "{caller}");
        }
        assert!(
            plane.seen().is_empty(),
            "the graph store was asked before the caller was allowed"
        );
    }

    #[tokio::test]
    async fn naming_no_tenant_is_refused_unless_the_caller_administers_the_deployment() {
        let (router, state, _plane) =
            over(Replies::scripted([("plane.query", serde_json::json!([]))])).await;

        assert_eq!(
            send(&router, &state, "GET", "alice", "/_ip/graphs/triage", None)
                .await
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            send(&router, &state, "GET", "root", "/_ip/graphs/triage", None)
                .await
                .status(),
            StatusCode::NOT_FOUND,
            "a sysadmin naming no tenant manages the deployment's own graphs"
        );
    }

    /// Every refusal `RunGraph::checked` carries, answered before the store is asked anything.
    #[tokio::test]
    async fn a_graph_whose_shape_does_not_hold_is_refused_before_the_store_is_asked() {
        let (router, state, plane) =
            over(Replies::scripted([("plane.query", serde_json::json!([]))])).await;

        for (what, body) in [
            ("no nodes", r#"{"name":"N","nodes":[],"max_nodes":8}"#),
            (
                "an edge naming a node it does not hold",
                r#"{"name":"N","nodes":[{"name":"a","agent":"x"}],"edges":[{"from":"a","to":"z"}],"max_nodes":8}"#,
            ),
            (
                "one node declared twice",
                r#"{"name":"N","nodes":[{"name":"a","agent":"x"},{"name":"a","agent":"y"}],"max_nodes":8}"#,
            ),
            (
                "more nodes than its own ceiling",
                r#"{"name":"N","nodes":[{"name":"a","agent":"x"},{"name":"b","agent":"y"}],"max_nodes":1}"#,
            ),
            (
                "a ceiling no graph may carry",
                r#"{"name":"N","nodes":[{"name":"a","agent":"x"}],"max_nodes":0}"#,
            ),
        ] {
            let answered = send(
                &router,
                &state,
                "PUT",
                "alice",
                "/_ip/graphs/triage?tenant=acme",
                Some(body),
            )
            .await;
            assert_eq!(
                answered.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "a graph with {what} was taken"
            );
        }
        assert!(
            plane.seen().is_empty(),
            "the store was asked to hold a graph that does not hold together"
        );
    }

    #[tokio::test]
    async fn a_graph_id_that_is_not_one_is_refused() {
        let (router, state, plane) =
            over(Replies::scripted([("plane.query", serde_json::json!([]))])).await;

        let answered = send(
            &router,
            &state,
            "GET",
            "alice",
            "/_ip/graphs/not%20an%20id?tenant=acme",
            None,
        )
        .await;

        assert_eq!(answered.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(plane.seen().is_empty());
    }

    /// Worth trying again, so it is a 503 and says which it was.
    #[tokio::test]
    async fn a_plane_that_cannot_be_reached_is_unavailable() {
        let silent = SilentPlane::start().await;
        let state = state_over_shared(store().await)
            .with_graphs(Arc::new(GraphPlane::new(silent.client())));
        let router = super::super::router(state.clone());

        let answered = send(
            &router,
            &state,
            "GET",
            "alice",
            "/_ip/graphs/triage?tenant=acme",
            None,
        )
        .await;

        assert_eq!(answered.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            body_of(answered).await.contains("could not be reached"),
            "an unreachable store did not say so"
        );
    }

    /// Trying again would answer the same way, so it is a 502 and not a 503.
    #[tokio::test]
    async fn a_plane_answering_with_rows_no_graph_can_be_rebuilt_from_is_a_bad_gateway() {
        let (router, state, _plane) = over(Replies::scripted([(
            "plane.query",
            serde_json::json!([{
                "id": 1,
                "external_key": "RunGraph/acme/triage",
                "labels": [GRAPH_LABEL],
                "properties": {"graph": "triage", "owner": "acme", "name": "Support triage"},
            }]),
        )]))
        .await;

        let answered = send(
            &router,
            &state,
            "GET",
            "alice",
            "/_ip/graphs/triage?tenant=acme",
            None,
        )
        .await;

        assert_eq!(answered.status(), StatusCode::BAD_GATEWAY);
        assert!(body_of(answered).await.contains("max_nodes"));
    }

    /// The owner is in the plan, so a store that answers with another tenant's row is caught.
    #[tokio::test]
    async fn a_row_of_another_owner_is_refused_rather_than_answered_with() {
        let (router, state, _plane) = over(Replies::scripted([(
            "plane.query",
            serde_json::json!([graph_row("other", "triage")]),
        )]))
        .await;

        let answered = send(
            &router,
            &state,
            "GET",
            "alice",
            "/_ip/graphs/triage?tenant=acme",
            None,
        )
        .await;

        assert_eq!(answered.status(), StatusCode::BAD_GATEWAY);
    }
}
