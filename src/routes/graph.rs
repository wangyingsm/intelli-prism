use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::StreamExt;
use ip_agents::AgentConfig;
use ip_auth::Authority;
use ip_core::{Capability, CapabilityScope, Protocol, TenantId};
use ip_gateway::Gateway;
use ip_graph::{
    GraphEdge, GraphError, GraphEvent, GraphId, GraphName, GraphNode, GraphOwner, GraphSummary,
    Graphing, Listing, MaxNodes, NodeName, NodeRan, RunGraph, RunGraphParts, RunsNode,
};
use ip_mcp::{McpServer, McpSettings};
use ip_run::{
    AsWhom, Pinned, Reporting, Requested, RunError, RunEvent, RunId, RunSignal, Running, Spend,
    Stopped, Target, Toolshed,
};
use ip_storage::{AgentOwner, Page};
use tokio::sync::watch;
use tokio_stream::wrappers::UnboundedReceiverStream;

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

/// How long a graph run's tool calls wait on an MCP server.
const MCP_SETTINGS: McpSettings = McpSettings {
    connect_timeout: Duration::from_secs(10),
    request_timeout: Duration::from_secs(30),
    max_answer_bytes: 4 * 1024 * 1024,
};

/// How many registered servers a graph's nodes may choose their tools from.
const REGISTRY_LIMIT: u32 = 1_000;

/// Running one graph, under the prefix the router nests it at.
pub fn run_router() -> Router<AppState> {
    Router::new().route("/{graph}", post(start))
}

/// What starting a graph run is asked for.
#[derive(Debug, serde::Deserialize)]
struct Start {
    /// What the graph's first nodes are being asked to do.
    input: String,
    /// Which model, where the request names one rather than taking each template's.
    #[serde(flatten)]
    requested: Requested,
}

/// Everything one node needs, resolved before the stream opens.
struct Prepared {
    /// What the node's agent runs under.
    config: AgentConfig,
    /// What it settled on for a model and a family.
    pinned: Pinned,
    /// Where its turns are sent, and as whom.
    target: Target,
    /// The servers its tools live on, already open.
    shed: Toolshed,
}

/// Runs one node of a graph through the deployment's own gateway.
///
/// Holds every node's resolved template rather than reading one when its turn comes: what a graph
/// run can refuse is settled before the 200, and a template that has gone missing halfway through
/// a stream is not a status code any more.
struct Nodes {
    /// The gateway every node's turns go through.
    gateway: Arc<Gateway>,
    /// What each node of this graph needs, by name.
    prepared: BTreeMap<NodeName, Prepared>,
    /// Where the graph's stream is written.
    say: tokio::sync::mpsc::UnboundedSender<GraphEvent>,
}

/// One node's turns, said into the graph's own stream under that node's name.
struct Saying {
    /// Which node is saying it.
    node: NodeName,
    /// Where the graph's stream is written.
    say: tokio::sync::mpsc::UnboundedSender<GraphEvent>,
}

impl Reporting for Saying {
    /// Everything but the run's own `done`, which the graph says itself as `node_done`.
    ///
    /// Both carry the same three things, and two events named `done` in one stream would leave a
    /// reader unable to tell a node ending from the graph ending.
    fn say(&self, event: RunEvent) {
        if matches!(event, RunEvent::Done { .. }) {
            return;
        }
        let _ = self.say.send(GraphEvent::Turn {
            node: self.node.clone(),
            event,
        });
    }
}

#[async_trait::async_trait]
impl RunsNode for Nodes {
    async fn run(
        &self,
        node: GraphNode,
        input: String,
        signals: watch::Receiver<RunSignal>,
    ) -> NodeRan {
        let refused = |why: String| NodeRan {
            answer: String::new(),
            stopped: Stopped::Refused { why },
            spend: Spend::NOTHING,
            traces: Vec::new(),
        };
        let Some(prepared) = self.prepared.get(&node.name) else {
            // Unreachable from the route, which prepares every node the graph holds: it would
            // mean the graph that was prepared and the graph being run are not the same one.
            return refused(format!("the node {} was never prepared", node.name));
        };
        let id = match RunId::generate() {
            Ok(id) => id,
            Err(error) => {
                return refused(format!(
                    "the node {} could not be named: {error}",
                    node.name
                ));
            }
        };
        let ran = ip_run::run(
            &Running {
                id,
                gateway: self.gateway.as_ref(),
                target: &prepared.target,
                shed: &prepared.shed,
                config: &prepared.config,
                pinned: &prepared.pinned,
                reporting: &Saying {
                    node: node.name.clone(),
                    say: self.say.clone(),
                },
                signals: &signals,
            },
            &input,
        )
        .await;
        NodeRan {
            answer: ran.text(),
            stopped: ran.stopped,
            spend: ran.spend,
            traces: ran.traces,
        }
    }
}

/// What preparing a graph's nodes takes.
///
/// A struct rather than six arguments: what every node needs is the same deployment, the same
/// request and the same registry, and only the node itself differs.
struct Preparing<'a> {
    /// The deployment the run is served by.
    state: &'a AppState,
    /// Which model the request named, where it named one.
    requested: &'a Requested,
    /// What the caller holds inside the tenant the run is charged to.
    authority: &'a Authority,
    /// The servers the nodes may choose their tools from.
    registry: &'a [McpServer],
    /// Whose templates the nodes name.
    templates: AgentOwner,
}

/// Everything every node of the graph needs, or the refusal that stops the run before it opens.
///
/// Every node, before the first one runs: a graph whose third node names a template that is not
/// there is a status code, and would otherwise be a stream that stops two nodes in.
async fn prepare(
    preparing: &Preparing<'_>,
    graph: &RunGraph,
) -> Result<BTreeMap<NodeName, Prepared>, Refusal> {
    let mut prepared = BTreeMap::new();
    for node in graph.nodes() {
        let config = match preparing
            .state
            .store()
            .agent(&preparing.templates, &node.agent)
            .await
        {
            Ok(Some(held)) => held.template.config,
            Ok(None) => {
                return Err(Box::new(
                    (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        format!(
                            "the node {} names the template {}, which this owner does not store",
                            node.name, node.agent
                        ),
                    )
                        .into_response(),
                ));
            }
            Err(error) => return Err(Box::new(store_refusal(error))),
        };
        let pinned = Pinned::resolve(preparing.requested, &config).map_err(unprocessable)?;
        let target = match Target::aimed(
            preparing.state.upstreams(),
            &pinned,
            AsWhom {
                authority: preparing.authority.clone(),
                listen: preparing.state.listen(),
                protocol: Protocol::Http,
            },
        ) {
            Ok(target) => target,
            // Nothing named a model, which is the request's own omission rather than the
            // deployment's shape, so it is answered as the request's own mistake.
            Err(error @ RunError::NoModelChosen { .. }) => {
                return Err(Box::new(
                    (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
                ));
            }
            Err(error) => return Err(unprocessable(error)),
        };
        let shed = Toolshed::open(config.tool_set(), preparing.registry, MCP_SETTINGS)
            .await
            .map_err(unprocessable)?;
        prepared.insert(
            node.name.clone(),
            Prepared {
                config,
                pinned,
                target,
                shed,
            },
        );
    }
    Ok(prepared)
}

/// The same owner, as the template store spells it.
///
/// A node's template is looked up under the graph's own owner: a tenant's graph composes that
/// tenant's templates and the deployment's graph composes the deployment's. There is deliberately
/// no fallback from the one to the other — a graph that quietly ran a global template under a
/// tenant's name would be a hole in the tenancy rather than a convenience.
fn templates_of(owner: &GraphOwner) -> AgentOwner {
    match owner {
        GraphOwner::Global => AgentOwner::Global,
        GraphOwner::Tenant(tenant) => AgentOwner::Tenant(tenant.clone()),
    }
}

/// Runs a stored graph, streaming what it does as it does it.
///
/// Everything that can refuse is settled before the stream opens, which for a graph means every
/// node and not just the first: each template is read, its tools are resolved against the
/// registry, its servers are opened and its target is worked out, all before the 200. A graph
/// whose third node names a template that is not there is a `422` and never a stream that stops
/// two nodes in. Once the stream is open the graph has begun, and every outcome — a node refused,
/// the graph stopped — arrives as an event.
async fn start(
    State(state): State<AppState>,
    manager: Manager,
    Path(id): Path<String>,
    Query(owned): Query<Owned>,
    Json(asked): Json<Start>,
) -> Response<Body> {
    let plane = match plane_of(&state) {
        Ok(plane) => plane,
        Err(refusal) => return *refusal,
    };
    let id = match GraphId::new(&id) {
        Ok(id) => id,
        Err(error) => return *unprocessable(error),
    };
    // A run spends money, so it is charged to a tenant rather than to the deployment.
    let Some(named) = owned.tenant.clone() else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "name the tenant this graph run is charged to",
        )
            .into_response();
    };
    let tenant = match TenantId::new(&named) {
        Ok(tenant) => tenant,
        Err(error) => return *unprocessable(error),
    };
    let owner = match owner_of(&state, &manager, owned).await {
        Ok(owner) => owner,
        Err(refusal) => return *refusal,
    };
    let graph = match plane.get(&owner, &id).await {
        Ok(Some(graph)) => graph,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return *graph_refusal(error),
    };
    // The plane's answer, not ours: a cycle is refused here and the order comes back with it.
    let order = match plane.order(&owner, &id).await {
        Ok(order) => order,
        Err(error) => return *graph_refusal(error),
    };
    let authority = match manager.authority(state.store().as_ref(), &tenant).await {
        Ok(Some(authority)) => authority,
        Ok(None) => return StatusCode::FORBIDDEN.into_response(),
        Err(error) => return store_refusal(error),
    };
    let registry = match state.store().mcps(Page::new(REGISTRY_LIMIT, 0, None)).await {
        Ok(held) => held
            .into_iter()
            .map(|stored| stored.server)
            .collect::<Vec<_>>(),
        Err(error) => return store_refusal(error),
    };
    let prepared = match prepare(
        &Preparing {
            state: &state,
            requested: &asked.requested,
            authority: &authority,
            registry: &registry,
            templates: templates_of(&owner),
        },
        &graph,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(refusal) => return *refusal,
    };

    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let graphing = Graphing {
        graph,
        order,
        runner: Arc::new(Nodes {
            gateway: state.shared_gateway(),
            prepared,
            say: sender.clone(),
        }),
        input: asked.input,
        reporting: Arc::new(sender),
    };
    tokio::spawn(async move {
        ip_graph::run(&graphing).await;
    });
    Sse::new(UnboundedReceiverStream::new(receiver).map(|event| {
        Ok::<_, std::convert::Infallible>(
            SseEvent::default()
                .event(event.name())
                .data(event.body().to_string()),
        )
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ip_core::{Grant, TnKey, UserId};
    use ip_drsg::{Replies, SilentPlane, StubPlane};
    use ip_graph::{GRAPH_LABEL, GraphPlane, NODE_LABEL};
    use ip_storage::{
        AccountKind, AgentStore, By, GrantStore, Membership, MembershipStore, NewTenant, NewUser,
        SqliteStore, Standing, TenantStore, UserStore,
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

    /// The deployment the run tests serve: one upstream, answering anthropic.
    const SERVING: &str = r#"
[server]
listen = "127.0.0.1:8080"

[storage]
backend = "sqlite"
path = "./unopened.db"

[cache]
backend = "sled"
path = "./unopened"

[auth.jwt]
issuer = "intelli-prism"
secret = "0123456789abcdef0123456789abcdef"

[[upstream]]
id = "anthropic"
base_url = "https://api.example.com/v1"
api_key = "sk-deployment"
model = "claude-opus-5"
caller_speaks = "anthropic"
upstream_speaks = "anthropic"
"#;

    /// An upstream that answers every turn with the same finished message.
    struct Answering;

    #[async_trait::async_trait]
    impl ip_gateway::upstream::Upstream for Answering {
        async fn send(
            &self,
            _request: axum::http::Request<ip_gateway::body::GatewayBody>,
        ) -> Result<axum::http::Response<ip_gateway::body::GatewayBody>, ip_gateway::UpstreamError>
        {
            let body = serde_json::json!({
                "id": "msg_1",
                "model": "claude-opus-5",
                "role": "assistant",
                "type": "message",
                "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "four"}],
                "usage": {"input_tokens": 10, "output_tokens": 2},
            })
            .to_string();
            Ok(axum::http::Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(ip_gateway::from_bytes(body.into()))
                .unwrap())
        }
    }

    /// A template every node of the test graphs runs.
    fn template(id: &str) -> ip_agents::AgentTemplate {
        use ip_agents::{
            AgentConfigParts, AgentName, AgentRole, AgentTemplate, MaxTurns, Prompt, ReqTimeout,
            Sandbox, TimeLimit, ToolSet,
        };
        AgentTemplate {
            id: ip_agents::AgentId::new(id).unwrap(),
            name: AgentName::new(id).unwrap(),
            sandbox: Sandbox::default(),
            config: AgentConfig::new(AgentConfigParts {
                role: AgentRole::System,
                prompt: Prompt::new("Be helpful.").unwrap(),
                tool_set: ToolSet::default(),
                max_turns: MaxTurns::new(4).unwrap(),
                time_limit: TimeLimit::new(60).unwrap(),
                req_timeout: ReqTimeout::new(10).unwrap(),
                use_best_effort: false,
                best_effort_strategy: None,
                model: None,
                api: None,
            })
            .unwrap(),
        }
    }

    /// The same store, plus the templates the nodes of a run name.
    async fn store_with_templates(named: &[&str]) -> Arc<SqliteStore> {
        let store = store().await;
        for id in named {
            store
                .put_agent(
                    ip_storage::NewAgent {
                        owner: AgentOwner::Tenant(tenant("acme")),
                        template: template(id),
                    },
                    &by("root", "/_ip/agents"),
                )
                .await
                .unwrap();
        }
        store
    }

    /// A router that can really run a graph: a fake upstream, and a plane answering as scripted.
    async fn running(named: &[&str], replies: Replies) -> (Router, AppState, StubPlane) {
        let plane = StubPlane::start(replies).await;
        let state = super::super::harness::serving(
            store_with_templates(named).await,
            SERVING,
            Arc::new(Answering),
        )
        .with_graphs(Arc::new(GraphPlane::new(plane.client())));
        (super::super::router(state.clone()), state, plane)
    }

    /// Everything the plane is asked for when a chain of two is run, in the order it is asked.
    ///
    /// Five calls, not four: reading the graph is three queries, and reading the order is the
    /// `toposort` and then the node read again, which is what turns the store's own ids into the
    /// names the graph calls its nodes.
    fn chain_of_two(order: serde_json::Value) -> Replies {
        let nodes = || {
            serde_json::json!([
                node_row(2, "acme", "triage", "intake", "intake-agent"),
                node_row(3, "acme", "triage", "reply", "reply-agent"),
            ])
        };
        Replies::in_turn([
            serde_json::json!([graph_row("acme", "triage")]),
            nodes(),
            serde_json::json!({"columns": ["from", "to"], "rows": [["intake", "reply"]]}),
            order,
            nodes(),
        ])
    }

    /// The order a plane answers for a chain of two, as `toposort` reports one.
    fn sorted_chain() -> serde_json::Value {
        serde_json::json!({
            "algo": "toposort",
            "count": 2,
            "results": [
                {"id": 2, "generation": 0},
                {"id": 3, "generation": 1},
            ],
        })
    }

    async fn start_run(
        router: &Router,
        state: &AppState,
        caller: &str,
        uri: &str,
    ) -> Response<Body> {
        send(
            router,
            state,
            "POST",
            caller,
            uri,
            Some(r#"{"input":"how many?"}"#),
        )
        .await
    }

    /// Every `data:` line of a stream, in the order it arrived.
    async fn streamed(response: Response<Body>) -> Vec<(String, serde_json::Value)> {
        let body = body_of(response).await;
        let mut said = Vec::new();
        let mut name = String::new();
        for line in body.lines() {
            if let Some(rest) = line.strip_prefix("event:") {
                name = rest.trim().to_owned();
            } else if let Some(rest) = line.strip_prefix("data:") {
                said.push((
                    name.clone(),
                    serde_json::from_str(rest.trim()).unwrap_or_else(|error| {
                        panic!("{error}: {rest}");
                    }),
                ));
            }
        }
        said
    }

    #[tokio::test]
    async fn a_graph_run_streams_every_node_and_then_the_graphs_own_answer() {
        let (router, state, _plane) = running(
            &["intake-agent", "reply-agent"],
            chain_of_two(sorted_chain()),
        )
        .await;

        let answered = start_run(
            &router,
            &state,
            "alice",
            "/_ip/graph_run/triage?tenant=acme",
        )
        .await;

        assert_eq!(answered.status(), StatusCode::OK);
        let said = streamed(answered).await;
        let names: Vec<&str> = said.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "node",
                "turn",
                "text",
                "node_done",
                "node",
                "turn",
                "text",
                "node_done",
                "done"
            ],
            "{said:?}"
        );
        // Every event names the node it belongs to, and names the right one: ruling 6 is only
        // worth anything if a reader can tell whose turn it is reading.
        let whose: Vec<&str> = said
            .iter()
            .filter(|(name, _)| name != "done")
            .map(|(name, body)| {
                body["node"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{name} carried no node: {body}"))
            })
            .collect();
        assert_eq!(
            whose,
            [
                "intake", "intake", "intake", "intake", "reply", "reply", "reply", "reply"
            ],
            "a turn was said under a node that was not taking it: {said:?}"
        );
        let (_, done) = said.last().expect("a graph says it is over");
        assert_eq!(done["answer"], serde_json::json!({"reply": "four"}));
        assert_eq!(done["stopped_at"], serde_json::Value::Null);
        assert_eq!(
            done["tokens"]["input"], 20,
            "the graph's spend is not both nodes': {done}"
        );
    }

    /// Ruling 2 over the wire: the second node is given the first's answer under its name.
    #[tokio::test]
    async fn a_nodes_input_is_said_as_it_is_given_keyed_by_the_parent_that_fed_it() {
        let (router, state, _plane) = running(
            &["intake-agent", "reply-agent"],
            chain_of_two(sorted_chain()),
        )
        .await;

        let said = streamed(
            start_run(
                &router,
                &state,
                "alice",
                "/_ip/graph_run/triage?tenant=acme",
            )
            .await,
        )
        .await;

        let begun: Vec<&serde_json::Value> = said
            .iter()
            .filter(|(name, _)| name == "node")
            .map(|(_, body)| body)
            .collect();
        assert_eq!(begun[0]["input"], "how many?");
        assert_eq!(begun[1]["input"], r#"{"intake":"four"}"#);
    }

    /// A node naming a template that is not there is a status code, never a stream that stops.
    #[tokio::test]
    async fn a_node_naming_a_template_this_owner_does_not_store_is_refused_before_the_stream() {
        let (router, state, _plane) =
            running(&["intake-agent"], chain_of_two(sorted_chain())).await;

        let answered = start_run(
            &router,
            &state,
            "alice",
            "/_ip/graph_run/triage?tenant=acme",
        )
        .await;

        assert_eq!(answered.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let said = body_of(answered).await;
        assert!(said.contains("reply"), "{said}");
        assert!(said.contains("reply-agent"), "{said}");
    }

    #[tokio::test]
    async fn a_graph_this_owner_does_not_keep_cannot_be_run() {
        let (router, state, _plane) = running(
            &["intake-agent"],
            Replies::scripted([("plane.query", serde_json::json!([]))]),
        )
        .await;

        assert_eq!(
            start_run(
                &router,
                &state,
                "alice",
                "/_ip/graph_run/absent?tenant=acme"
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
    }

    /// A run spends money, so it is charged to a tenant and never to the deployment.
    #[tokio::test]
    async fn a_graph_run_naming_no_tenant_is_refused() {
        let (router, state, plane) = running(
            &["intake-agent"],
            Replies::scripted([("plane.query", serde_json::json!([]))]),
        )
        .await;

        let answered = start_run(&router, &state, "root", "/_ip/graph_run/triage").await;

        assert_eq!(answered.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body_of(answered).await.contains("charged to"));
        assert!(
            plane.seen().is_empty(),
            "the graph store was asked for a run that was going to be refused"
        );
    }

    /// A run is refused when the order comes back unusable, which is **502** and not a stream.
    ///
    /// A *cycle* is a different answer again: the plane reports one as a JSON-RPC error, so it
    /// arrives as `GraphError::Plane` and reads as 503 — worth recording rather than defending,
    /// since nothing at that level tells a cyclic scope apart from a store that is broken. It
    /// costs nothing today because [`GraphPlane::put`] refuses a cyclic graph when it is written,
    /// so a stored one can only have been edited in the plane by something that is not this code.
    #[tokio::test]
    async fn a_plane_answering_the_order_with_something_that_is_not_one_stops_the_run() {
        let (router, state, _plane) = running(
            &["intake-agent", "reply-agent"],
            chain_of_two(serde_json::json!({"ordered": "not an order"})),
        )
        .await;

        let answered = start_run(
            &router,
            &state,
            "alice",
            "/_ip/graph_run/triage?tenant=acme",
        )
        .await;

        assert_eq!(
            answered.status(),
            StatusCode::BAD_GATEWAY,
            "a graph whose order could not be read was run anyway"
        );
    }

    #[tokio::test]
    async fn a_graph_run_cannot_be_started_by_a_caller_of_another_tenant() {
        let (router, state, plane) = running(
            &["intake-agent"],
            Replies::scripted([("plane.query", serde_json::json!([]))]),
        )
        .await;

        for caller in ["carol", "bob"] {
            assert_eq!(
                start_run(&router, &state, caller, "/_ip/graph_run/triage?tenant=acme")
                    .await
                    .status(),
                StatusCode::FORBIDDEN,
                "{caller}"
            );
        }
        assert!(plane.seen().is_empty());
    }

    #[tokio::test]
    async fn a_deployment_keeping_no_graphs_cannot_run_one() {
        let state = super::super::harness::serving(
            store_with_templates(&["intake-agent"]).await,
            SERVING,
            Arc::new(Answering),
        );
        let router = super::super::router(state.clone());

        assert_eq!(
            start_run(
                &router,
                &state,
                "alice",
                "/_ip/graph_run/triage?tenant=acme"
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn a_graph_run_over_an_unreachable_plane_is_unavailable() {
        let silent = SilentPlane::start().await;
        let state = super::super::harness::serving(
            store_with_templates(&["intake-agent"]).await,
            SERVING,
            Arc::new(Answering),
        )
        .with_graphs(Arc::new(GraphPlane::new(silent.client())));
        let router = super::super::router(state.clone());

        assert_eq!(
            start_run(
                &router,
                &state,
                "alice",
                "/_ip/graph_run/triage?tenant=acme"
            )
            .await
            .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
