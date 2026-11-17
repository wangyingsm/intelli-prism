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
use ip_agents::{AgentConfig, AgentId};
use ip_auth::Authority;
use ip_cluster::{ClusterClient, Dispatch, NodeEvent};
use ip_core::route::ClusterAddress;
use ip_core::{Capability, CapabilityScope, Protocol, TenantId, UserId};
use ip_gateway::Gateway;
use ip_graph::{
    ClusterMember, Downstream, GraphEdge, GraphError, GraphEvent, GraphId, GraphName, GraphNode,
    GraphOwner, GraphPlane, GraphRunId, GraphSaid, GraphSummary, Graphing, Listing, MaxNodes,
    NodeName, NodeRan, Placement, RunGraph, RunGraphParts, RunningNode, RunsNode, Stopping,
};
use ip_mcp::{McpServer, McpSettings};
use ip_run::{
    AsWhom, Pinned, Reporting, Requested, RunError, RunEvent, RunId, Running, Spend, Stopped,
    Target, Toolshed,
};
use ip_storage::{AgentOwner, Page};
use tokio_stream::wrappers::UnboundedReceiverStream;

use super::{Paged, store_refusal};
use crate::cluster::{Kept, Signalling, Spreading};
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
        loops: Vec::new(),
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
///
/// Shared with the dispatch route rather than written twice: a node runs the same way whichever
/// machine it runs on, and two sets of timeouts for one node would be a difference nobody chose.
pub(super) const MCP_SETTINGS: McpSettings = McpSettings {
    connect_timeout: Duration::from_secs(10),
    request_timeout: Duration::from_secs(30),
    max_answer_bytes: 4 * 1024 * 1024,
};

/// How many registered servers a graph's nodes may choose their tools from.
pub(super) const REGISTRY_LIMIT: u32 = 1_000;

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
    /// Which template it names, which a dispatch carries instead of what was resolved from it.
    agent: AgentId,
    /// What the node's agent runs under.
    config: AgentConfig,
    /// What it settled on for a model and a family.
    pinned: Pinned,
    /// Where its turns are sent, and as whom.
    target: Target,
    /// The servers its tools live on, already open.
    shed: Toolshed,
}

/// Runs one node of a graph, here or on another machine of this cluster.
///
/// Holds every node's resolved template rather than reading one when its turn comes: what a graph
/// run can refuse is settled before the 200, and a template that has gone missing halfway through
/// a stream is not a status code any more. A node sent elsewhere is resolved here all the same,
/// because that resolution is what makes the local fallback possible at all.
struct Nodes {
    /// The gateway every node's turns go through, when the node runs here.
    gateway: Arc<Gateway>,
    /// What each node of this graph needs, by name.
    prepared: BTreeMap<NodeName, Prepared>,
    /// Where the graph's stream is written.
    say: tokio::sync::mpsc::UnboundedSender<GraphSaid>,
    /// Where a node's placement is recorded, when this deployment can say where it is.
    placing: Option<Placing>,
    /// Where a node may be sent instead, when this deployment is in a cluster it can dispatch in.
    dispatching: Option<Dispatching>,
    /// Where a node's answer is kept, when there is another coordinator that could ask for it.
    kept: Option<Arc<Kept>>,
}

/// What recording where a node is running takes.
///
/// Here rather than in `ip-graph`'s own run, because the dispatcher is what knows where a node is
/// going: in this process that is our own address, and in a cluster it is the worker picked for it.
struct Placing {
    /// Where the record is kept.
    plane: Arc<GraphPlane>,
    /// Which graph the nodes are from.
    graph: GraphId,
    /// Whose graph it is.
    owner: GraphOwner,
    /// Where this machine answers its own cluster, which is where a node runs unless it is sent.
    us: ClusterAddress,
}

/// What every dispatch of this run carries, whichever of its nodes it is for.
///
/// The caller is *named* rather than resolved: the member that runs the node builds the authority
/// itself, from its own reads of the same two rows. A dispatch is intent, never permission.
struct Sending {
    /// Which graph the node is from.
    graph: GraphId,
    /// Whose graph it is.
    owner: GraphOwner,
    /// The tenant the run is charged to.
    tenant: TenantId,
    /// The account the run was started by.
    user: UserId,
    /// What the request asked for, where it asked for anything.
    requested: Requested,
}

/// Where this run's nodes may be sent, when this deployment is in a cluster that can take one.
struct Dispatching {
    /// What every dispatch carries.
    sending: Sending,
    /// The pooled client they go out on.
    client: Arc<ClusterClient>,
    /// Where this machine answers, so it is never picked to run its own node.
    us: ClusterAddress,
    /// Where the members of the cluster say they are here.
    plane: Arc<GraphPlane>,
    /// How often a member says so, which is what tells one that is late from one that is gone.
    heartbeat: Duration,
    /// How one of the live members is picked.
    spreading: Arc<Spreading>,
}

/// How many heartbeats a member may miss before this coordinator stops picking it.
///
/// Three rather than one, because a member a moment late on one write has not died and the cost
/// of being wrong is small in one direction only: a dispatch to a member that *has* died fails
/// before its first event, which falls back and runs the node here. Skipping a member that is
/// merely late would send every node to one machine for no reason.
const BELIEVED_FOR: u32 = 3;

/// Which node of which run is being sent, and what it is being asked.
struct OneNode<'a> {
    /// The graph run it belongs to.
    run: GraphRunId,
    /// Which node it is.
    node: &'a NodeName,
    /// Which template that node names.
    agent: &'a AgentId,
    /// What it is being asked to do.
    input: &'a str,
}

/// What came of trying to run a node somewhere else.
enum Sent {
    /// It ran there, and this is what it came to — an answer or a failure that is the node's.
    Ran(NodeRan),
    /// Nothing was said and nothing was spent, so the node may still run here.
    Nowhere,
}

/// Where the records of this run's nodes are kept, when this deployment keeps any.
///
/// Nothing is recorded by a deployment that cannot say where it answers, or that keeps no graphs:
/// an address nobody can dial is worse than no record at all.
fn placing_for(state: &AppState, graph: &RunGraph) -> Option<Placing> {
    state
        .cluster()
        .zip(state.graphs())
        .map(|(address, plane)| Placing {
            plane: Arc::clone(plane),
            graph: graph.id().clone(),
            owner: graph.owner().clone(),
            us: address.clone(),
        })
}

/// Where this run's nodes may be sent, when all three of the things that takes are there.
///
/// The plane, because that is where the members are; an address of this machine's own, so it can
/// tell its own row from a peer's; and a client, which exists only where the secret does. Any one
/// missing and every node of the run is run here, which is what every deployment did until now.
fn dispatching_for(state: &AppState, run: &Run<'_>) -> Option<Dispatching> {
    Some(Dispatching {
        sending: Sending {
            graph: run.graph.id().clone(),
            owner: run.graph.owner().clone(),
            tenant: run.tenant.clone(),
            user: run.user.clone(),
            requested: run.requested.clone(),
        },
        client: Arc::clone(state.cluster_client()?),
        us: state.cluster()?.clone(),
        plane: Arc::clone(state.graphs()?),
        heartbeat: state.heartbeat(),
        spreading: Arc::clone(state.spreading()),
    })
}

/// What carries this run's stop to the members running its nodes, where any are.
///
/// Gated on the same thing dispatching is, and for a reason rather than for symmetry: a
/// deployment that never sends a node elsewhere has nobody running one of its nodes, so the
/// publication would be a write nothing follows.
fn stopping_for(state: &AppState) -> Option<Arc<dyn Stopping>> {
    state.cluster_client()?;
    Some(Arc::new(Signalling::new(Arc::clone(state.shared_cache()))))
}

/// What says which nodes a taken loop back runs again, which is the plane the graphs are kept in.
fn reaching(state: &AppState) -> Option<Arc<dyn Downstream>> {
    state
        .graphs()
        .map(|plane| Arc::clone(plane) as Arc<dyn Downstream>)
}

/// Where this run's answers are kept, where anything else could ask for them.
///
/// Gated on the cluster for the same reason the stop is: a deployment that is the only machine
/// serving a run is also the only one that could pick it up, and it holds the answers in memory
/// while it does. Alone, this would be a write nobody reads.
fn keeping_for(state: &AppState) -> Option<Arc<Kept>> {
    state.cluster_client()?;
    Some(Arc::new(Kept::new(Arc::clone(state.shared_cache()))))
}

/// What runs this graph's nodes.
///
/// The gateway, every node's resolved template, and — where this deployment is in a cluster it
/// can dispatch in — the members it may send a node to instead of running it here.
fn driving(
    state: &AppState,
    run: &Run<'_>,
    prepared: BTreeMap<NodeName, Prepared>,
    say: tokio::sync::mpsc::UnboundedSender<GraphSaid>,
) -> Nodes {
    Nodes {
        gateway: state.shared_gateway(),
        prepared,
        say,
        placing: placing_for(state, run.graph),
        dispatching: dispatching_for(state, run),
        kept: keeping_for(state),
    }
}

/// What one graph run is, beyond the graph itself.
struct Run<'a> {
    /// The graph being run.
    graph: &'a RunGraph,
    /// The tenant it is charged to.
    tenant: &'a TenantId,
    /// The account that started it.
    user: &'a UserId,
    /// What it was asked to run on, where anything was named.
    requested: &'a Requested,
}

impl<'a> Run<'a> {
    /// The same four, named at a call site that has them all to hand.
    const fn of(
        graph: &'a RunGraph,
        tenant: &'a TenantId,
        user: &'a UserId,
        requested: &'a Requested,
    ) -> Self {
        Self {
            graph,
            tenant,
            user,
            requested,
        }
    }
}

impl Placing {
    /// Records that this node is running at `address`, which is this machine unless it was sent.
    ///
    /// **Best effort.** A record that cannot be written is a warning and not a refusal: the
    /// alternative makes the plane a dependency of *running* a node rather than of reading a
    /// graph, which is a cost the graph store was deliberately not given.
    async fn note(&self, run: GraphRunId, node: &NodeName, address: &ClusterAddress) {
        let placement = Placement {
            run,
            node: node.clone(),
            graph: self.graph.clone(),
            owner: self.owner.clone(),
            address: address.clone(),
            at: ip_core::Timestamp::now(),
        };
        if let Err(error) = self.plane.placed(&placement).await {
            tracing::warn!(%error, %node, "could not record where a node is running");
        }
    }

    /// Takes the record away, now that the node has ended wherever it ran.
    ///
    /// Best effort for the same reason, and with the same consequence either way: a record left
    /// behind says a node is running that is not, which is what a resume reads and what a sweep
    /// is for. It is not worth failing an answer the caller already has.
    async fn done(&self, run: GraphRunId, node: &NodeName) {
        if let Err(error) = self.plane.finished(&run, node).await {
            tracing::warn!(%error, %node, "could not clear where a node was running");
        }
    }
}

impl Dispatching {
    /// A live member to run this node on, which may well be this machine.
    ///
    /// **This coordinator is a candidate like any other member.** A cluster of three spreads a
    /// generation over three machines and not over the two that are not coordinating: a member
    /// that happens to be the one a caller asked is still a machine with a gateway on it, and
    /// leaving it out would make the busiest member the one doing none of the work. Picked for
    /// itself, the node runs here and nothing is dispatched — see [`Self::attempt`].
    ///
    /// **Our own liveness is not read from the plane.** Every other member is believed in for as
    /// long as its last heartbeat allows, because a heartbeat is all we know of it. Of ourselves
    /// we know more than any heartbeat can say, so this machine is a candidate whether the plane
    /// holds a fresh row for it, a stale one or none at all — otherwise a plane this machine is
    /// struggling to write to would send every node away at exactly the moment the cluster is
    /// least able to take it.
    ///
    /// The membership read is best effort in the same way the placement write is: a plane that
    /// cannot answer means this run has no cluster to speak of, and the node runs here.
    async fn pick(&self) -> Option<ClusterAddress> {
        let members = match self.plane.members().await {
            Ok(members) => members,
            Err(error) => {
                tracing::warn!(%error, "could not read this cluster's members; running here");
                return None;
            }
        };
        let believed = i64::try_from(self.heartbeat.as_secs())
            .unwrap_or(i64::MAX)
            .saturating_mul(i64::from(BELIEVED_FOR));
        let now = ip_core::Timestamp::now().unix_seconds();
        // Filtered here and picked there: whether a member may be given a node is this
        // coordinator's decision, and which of the ones that may is the strategy's.
        let mut live: Vec<ClusterMember> = members
            .into_iter()
            .filter(|member| now.saturating_sub(member.at.unix_seconds()) <= believed)
            .collect();
        if !live.iter().any(|member| member.address == self.us) {
            live.push(ClusterMember {
                address: self.us.clone(),
                at: ip_core::Timestamp::now(),
            });
        }
        self.spreading.pick(&live)
    }

    /// One node's work, as the member running it is asked for it.
    fn work(&self, one: &OneNode<'_>) -> Dispatch {
        Dispatch {
            run: one.run,
            node: one.node.clone(),
            graph: self.sending.graph.clone(),
            owner: self.sending.owner.clone(),
            tenant: self.sending.tenant.clone(),
            user: self.sending.user.clone(),
            agent: one.agent.clone(),
            requested: self.sending.requested.clone(),
            input: one.input.to_owned(),
        }
    }

    /// Runs this node on a live member of the cluster, which may be this machine.
    ///
    /// The placement is written before the dispatch and at the member picked for it, so a record
    /// exists for the whole time the work is away from here. Picked for ourselves, nothing is
    /// written here: the caller records this address and runs the node.
    async fn attempt(&self, placing: Option<&Placing>, one: &OneNode<'_>, saying: &Saying) -> Sent {
        let Some(at) = self.pick().await else {
            return Sent::Nowhere;
        };
        // Picked ourselves, which is this machine taking its share and not a dispatch at all.
        // Answered as [`Sent::Nowhere`] because that is exactly what it means — nothing was sent
        // and nothing was spent elsewhere — so the caller notes the placement at this address and
        // runs the node, down the same path a cluster with nobody else to ask takes. Dialling our
        // own listener would buy a round trip, a second authority built from the same rows and a
        // second stream, and arrive at the same turns.
        if at == self.us {
            return Sent::Nowhere;
        }
        if let Some(placing) = placing {
            placing.note(one.run, one.node, &at).await;
        }
        self.elsewhere(&at, &self.work(one), saying).await
    }

    /// Runs the node on `at`, saying what it says into this graph's own stream as it arrives.
    ///
    /// **Where the boundary between a retry and a double charge is.** A member that will not take
    /// the work, or refuses before saying anything, has spent nothing, so the node may still run
    /// here — [`Sent::Nowhere`]. Once it has said *anything* it has reported a turn, and a turn is
    /// reported before it is taken, so the money is gone whether we hear the answer or not. From
    /// there a member that stops answering is a failed node and never a node run twice. That is
    /// M10's ruling for a hedge — only the winner is counted — applied to a boundary.
    async fn elsewhere(&self, at: &ClusterAddress, work: &Dispatch, saying: &Saying) -> Sent {
        let mut stream = match self.client.dispatch(at, work).await {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(
                    %error, %at, node = %work.node,
                    "a member would not take a node; running it here"
                );
                return Sent::Nowhere;
            }
        };
        let mut heard = 0usize;
        loop {
            let said = match stream.next().await {
                Ok(Some(said)) => said,
                Ok(None) | Err(_) if heard == 0 => {
                    tracing::warn!(
                        %at, node = %work.node,
                        "a member took a node and said nothing of it; running it here"
                    );
                    return Sent::Nowhere;
                }
                // Unreachable: the client answers `Ok(None)` only once a terminal event has
                // been read, and this loop returns on every terminal event. Kept as a cut-off
                // rather than a panic, since the safe reading of an ended stream is that the
                // turns already reported were taken.
                Ok(None) => return Sent::Ran(cut_off(at, &work.node, "it ended its stream")),
                Err(error) => {
                    return Sent::Ran(cut_off(at, &work.node, &error.to_string()));
                }
            };
            match said {
                NodeEvent::Said(event) => {
                    heard += 1;
                    saying.say(event);
                }
                NodeEvent::Answered(ran) => return Sent::Ran(ran),
                // A refusal is the worker's considered answer that it cannot run this node at
                // all, and it only ever arrives before any turn — so it is the one failure this
                // coordinator can answer by running the node itself.
                NodeEvent::Refused(why) if heard == 0 => {
                    tracing::warn!(%at, node = %work.node, %why, "a member refused a node; running it here");
                    return Sent::Nowhere;
                }
                NodeEvent::Refused(why) => {
                    return Sent::Ran(NodeRan {
                        answer: String::new(),
                        stopped: Stopped::Refused { why },
                        spend: Spend::NOTHING,
                        traces: Vec::new(),
                    });
                }
            }
        }
    }
}

/// A node whose member stopped answering with turns already taken.
///
/// The spend is recorded as nothing because this machine cannot know what the other spent, which
/// understates the run's cost. The placement left behind in the plane is what says where to look.
fn cut_off(at: &ClusterAddress, node: &NodeName, why: &str) -> NodeRan {
    tracing::warn!(%at, %node, %why, "a member stopped answering for a node it had begun");
    NodeRan {
        answer: String::new(),
        stopped: Stopped::Refused {
            why: format!("the node {node} was running on {at} and that member stopped: {why}"),
        },
        spend: Spend::NOTHING,
        traces: Vec::new(),
    }
}

/// One node's turns, said into the graph's own stream under that node's name.
struct Saying {
    /// Which graph run the node belongs to.
    run: GraphRunId,
    /// Which node is saying it.
    node: NodeName,
    /// Where the graph's stream is written.
    say: tokio::sync::mpsc::UnboundedSender<GraphSaid>,
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
        let _ = self.say.send(GraphSaid {
            run: self.run,
            event: GraphEvent::Turn {
                node: self.node.clone(),
                event,
            },
        });
    }
}

#[async_trait::async_trait]
impl RunsNode for Nodes {
    async fn run(&self, running: RunningNode) -> NodeRan {
        let RunningNode {
            run,
            node,
            input,
            signals,
        } = running;
        let refused = |why: String| NodeRan {
            answer: String::new(),
            stopped: Stopped::Refused { why },
            spend: Spend::NOTHING,
            traces: Vec::new(),
        };
        // Before everything, including the template and the dispatch: a node whose answer is
        // already in hand needs no template, no run id and no member to send it to. What a retry
        // must not pay for twice is the turn, not the crossing — a node already answered is
        // already paid for wherever it ran. One cache read against a node about to take upstream
        // turns, and only in a cluster, where something else could be asking.
        if let Some(kept) = &self.kept
            && let Some(ran) = kept.answer(run, &node.name).await
        {
            return ran;
        }
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
        let saying = Saying {
            run,
            node: node.name.clone(),
            say: self.say.clone(),
        };
        // Sent first, because a node that ran elsewhere is recorded at the member it went to:
        // a reader of the plane learns where the work is and not where it was dispatched from.
        // `Sent::Nowhere` spent nothing there, so the lines below rewrite the record and run it.
        let one = OneNode {
            run,
            node: &node.name,
            agent: &prepared.agent,
            input: &input,
        };
        let sent = match &self.dispatching {
            Some(dispatching) => {
                dispatching
                    .attempt(self.placing.as_ref(), &one, &saying)
                    .await
            }
            None => Sent::Nowhere,
        };
        if let Sent::Ran(ran) = sent {
            if let Some(placing) = &self.placing {
                placing.done(run, &node.name).await;
            }
            self.worth_keeping(run, &node.name, &ran).await;
            return ran;
        }
        if let Some(placing) = &self.placing {
            placing.note(run, &node.name, &placing.us).await;
        }
        let ran = ip_run::run(
            &Running {
                id,
                gateway: self.gateway.as_ref(),
                target: &prepared.target,
                shed: &prepared.shed,
                config: &prepared.config,
                pinned: &prepared.pinned,
                reporting: &saying,
                signals: &signals,
            },
            &input,
        )
        .await;
        if let Some(placing) = &self.placing {
            placing.done(run, &node.name).await;
        }
        let ran = NodeRan {
            answer: ran.text(),
            stopped: ran.stopped,
            spend: ran.spend,
            traces: ran.traces,
        };
        self.worth_keeping(run, &node.name, &ran).await;
        ran
    }
}

impl Nodes {
    /// Keeps this node's answer where another coordinator can find it, if it cost anything.
    ///
    /// **What spent nothing is not kept**, and that is the whole rule: the saving being bought is
    /// money already paid to an upstream, so a node that paid nothing has nothing to protect and
    /// is left free to run again. It also keeps the two refusals out of the cache that would
    /// otherwise be permanent — a template that has gone missing, and a member that could not be
    /// reached — neither of which is a reason to answer a retry the same way for a day.
    async fn worth_keeping(&self, run: GraphRunId, node: &NodeName, ran: &NodeRan) {
        if let Some(kept) = &self.kept
            && ran.spend != Spend::NOTHING
        {
            kept.keep(run, node, ran).await;
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
                agent: node.agent.clone(),
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
pub(super) fn templates_of(owner: &GraphOwner) -> AgentOwner {
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

    let run = match GraphRunId::generate() {
        Ok(run) => run,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("this run could not be named: {error}"),
            )
                .into_response();
        }
    };
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let runner = driving(
        &state,
        &Run::of(&graph, &tenant, manager.user(), &asked.requested),
        prepared,
        sender.clone(),
    );
    let graphing = Graphing {
        id: run,
        graph,
        order,
        runner: Arc::new(runner),
        input: asked.input,
        reporting: Arc::new(sender),
        stopping: stopping_for(&state),
        plane: reaching(&state),
    };
    tokio::spawn(async move {
        ip_graph::run(&graphing).await;
    });
    Sse::new(UnboundedReceiverStream::new(receiver).map(|said| {
        Ok::<_, std::convert::Infallible>(
            SseEvent::default()
                .event(said.name())
                .data(said.body().to_string()),
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
            no_loops(),
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
                best_effort: None,
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
    /// Six calls, not four: reading the graph is three queries and a statement for its loop backs,
    /// and reading the order is the `toposort` and then the node read again, which is what turns
    /// the store's own ids into the names the graph calls its nodes.
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
            no_loops(),
            order,
            nodes(),
        ])
    }

    /// What a plane answers when a graph declares no loop back, which is every graph here.
    fn no_loops() -> serde_json::Value {
        serde_json::json!({"columns": ["from", "to", "cap"], "rows": []})
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

    /// Every event of one stream names one run, which is what ties a graph's nodes together.
    #[tokio::test]
    async fn every_event_of_a_run_names_the_same_run() {
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

        let runs: std::collections::BTreeSet<&str> = said
            .iter()
            .map(|(name, body)| {
                body["run"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{name} carried no run: {body}"))
            })
            .collect();
        assert_eq!(runs.len(), 1, "one run said more than one run id: {runs:?}");
        let run = runs.iter().next().expect("a run id");
        assert_eq!(run.len(), 32, "a run id that is not one: {run}");
    }

    /// Two runs of the same graph are told apart, which a retried dispatch will depend on.
    #[tokio::test]
    async fn two_runs_of_one_graph_are_named_apart() {
        let mut named = Vec::new();
        for _ in 0..2 {
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
            named.push(said[0].1["run"].as_str().expect("a run id").to_owned());
        }
        assert_ne!(named[0], named[1]);
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

    /// A plane that answers every management read, so a 500 can only be the store's.
    fn answering_plane() -> Replies {
        Replies::scripted([
            ("plane.query", serde_json::json!([])),
            ("plane.algo", serde_json::json!({"count": 0, "results": []})),
        ])
    }

    /// Every store call a graph request makes carries its own "the store could not answer" arm,
    /// and a database that works reaches none of them. Failing each in turn reaches them all.
    ///
    /// The sweep starts at the first call of the whole request — the extractor's own read of the
    /// caller's standing — and the count is asserted, so a handler that gains or loses a store
    /// call fails here rather than quietly leaving a new arm untested. Each refused request
    /// changes nothing, which is what lets one fixture serve a whole sweep.
    ///
    /// The graphs themselves are in the plane and not in the store, so what is swept here is the
    /// door rather than the write: the plane answers everything, and a 500 is the store's alone.
    #[tokio::test]
    async fn a_store_that_cannot_answer_is_the_servers_own_failure() {
        for (method, uri, body, calls) in [
            ("GET", "/_ip/graphs?tenant=acme", None, 3),
            ("GET", "/_ip/graphs/triage?tenant=acme", None, 3),
            (
                "PUT",
                "/_ip/graphs/triage?tenant=acme",
                Some(r#"{"name":"Triage","max_nodes":4,"nodes":[{"name":"a","agent":"x"}]}"#),
                3,
            ),
            ("DELETE", "/_ip/graphs/triage?tenant=acme", None, 3),
        ] {
            let (router, state, _plane) = running(&[], answering_plane()).await;
            let cookie = cookie_of(&state, &user("alice")).await;
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

    /// The same sweep over a run, which reaches past the door into what a run itself reads.
    ///
    /// A fixture of its own per request, where the management sweep reuses one: the plane answers
    /// a graph read in three queries and an order in two more, scripted in the order they arrive,
    /// so a second request through one fixture would be answered the first one's answers.
    ///
    /// Eight calls, and the count is what makes this worth keeping: a run that gains a node, or a
    /// handler that reads one more row before it starts, fails here rather than quietly adding an
    /// arm nothing reaches.
    #[tokio::test]
    async fn a_store_that_cannot_answer_refuses_a_run_at_every_read_it_makes() {
        let mut refused = 0;

        loop {
            let (router, state, _plane) = running(
                &["intake-agent", "reply-agent"],
                chain_of_two(sorted_chain()),
            )
            .await;
            // Minted before the store is told to fail, or the cookie itself cannot be made.
            let cookie = cookie_of(&state, &user("alice")).await;
            state.store_failure().answer_only(refused);

            let response = router
                .oneshot(request(
                    "POST",
                    "/_ip/graph_run/triage?tenant=acme",
                    &cookie,
                    Some(r#"{"input":"how many?"}"#),
                ))
                .await
                .unwrap();

            if response.status() != StatusCode::INTERNAL_SERVER_ERROR {
                assert_eq!(
                    response.status(),
                    StatusCode::OK,
                    "the sweep ended on something that is neither a refusal nor a run"
                );
                break;
            }
            refused += 1;
            assert!(refused <= 12, "a run refuses whatever it is given");
        }

        assert_eq!(refused, 8, "a run's own reads");
    }

    /// Where a test's own deployment answers its cluster.
    ///
    /// Chosen to sort *after* a stub worker's `127.0.0.1:<port>`, because round-robin's order is
    /// the printed address and the fixture below runs on it: a coordinator that sorted first
    /// would take the first node of every two-node graph, and every test about what a dispatch
    /// does would be a test about the second node instead.
    fn here() -> ClusterAddress {
        "192.168.0.2:8080".parse().expect("an address")
    }

    /// The same fixture as [`running`], as a member of a cluster that can say where it is.
    async fn running_in_a_cluster(
        named: &[&str],
        replies: Replies,
    ) -> (Router, AppState, StubPlane) {
        let plane = StubPlane::start(replies).await;
        let state = super::super::harness::serving(
            store_with_templates(named).await,
            SERVING,
            Arc::new(Answering),
        )
        .with_graphs(Arc::new(GraphPlane::new(plane.client())))
        .with_cluster(here());
        (super::super::router(state.clone()), state, plane)
    }

    #[tokio::test]
    async fn a_run_notes_where_each_node_is_running_and_clears_it_after() {
        let (router, state, plane) = running_in_a_cluster(
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

        let seen = plane.seen();
        let written: Vec<&serde_json::Value> = seen
            .iter()
            .filter(|one| one.method == "node.create")
            .map(|one| &one.params)
            .collect();
        assert_eq!(written.len(), 2, "one record per node of the graph");
        let cleared = seen
            .iter()
            .filter(|one| one.method == "node.delete")
            .count();
        // Two per node: the key is cleared before it is written, and again when the node ends.
        assert_eq!(cleared, 4, "every record is taken away again");

        // The run the records name is the run the stream is about.
        let run = said
            .iter()
            .find_map(|(name, body)| (name == "done").then(|| body["run"].clone()))
            .expect("the run says it is done");
        for record in written {
            assert_eq!(record["properties"]["run"], run);
            assert_eq!(record["properties"]["address"], here().to_string());
            assert_eq!(record["properties"]["graph"], "triage");
            assert_eq!(record["properties"]["owner"], "acme");
        }
    }

    #[tokio::test]
    async fn a_deployment_that_cannot_say_where_it_is_records_nothing() {
        let (router, state, plane) = running(
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
        streamed(answered).await;

        let wrote_or_cleared = plane
            .seen()
            .iter()
            .filter(|one| one.method == "node.create" || one.method == "node.delete")
            .count();
        assert_eq!(
            wrote_or_cleared, 0,
            "a deployment with no cluster address records no placement"
        );
    }

    /// A record that cannot be written does not stop the node that would have been recorded.
    #[tokio::test]
    async fn a_plane_that_refuses_the_record_does_not_stop_the_node() {
        let plane = StubPlane::start(Replies::Failure).await;
        let placing = Placing {
            plane: Arc::new(GraphPlane::new(plane.client())),
            graph: GraphId::new("triage").expect("a graph id"),
            owner: GraphOwner::Tenant(TenantId::new("acme").expect("a tenant")),
            us: here(),
        };
        let run = GraphRunId::generate().expect("a run id");
        let node = NodeName::new("intake").expect("a node name");

        // Neither says anything, and neither refuses: the warning is all that is left of it.
        placing.note(run, &node, &here()).await;
        placing.done(run, &node).await;

        // It tried, and the first call is where it stopped: `placed` clears the key before it
        // writes, so a plane that refuses everything refuses that delete and never gets to create.
        let tried: Vec<String> = plane.seen().into_iter().map(|one| one.method).collect();
        assert_eq!(tried, ["node.delete", "node.delete"]);
    }

    /// A membership row, as the plane hands one back.
    fn member_row(address: &str, ago: i64) -> serde_json::Value {
        serde_json::json!({
            "properties": {
                "address": address,
                "at": ip_core::Timestamp::now().unix_seconds() - ago,
            },
        })
    }

    /// The plane's answers for a chain of two whose nodes are dispatched to `members`.
    ///
    /// Every call in the order it arrives, which for a dispatched node is the membership read
    /// and then the two writes that rewrite its placement. A call past the end is answered with
    /// an empty object, which the membership read refuses — so a script that runs out is a run
    /// that stops dispatching rather than one that hangs.
    fn dispatching_chain(members: &[serde_json::Value]) -> Replies {
        let nodes = || {
            serde_json::json!([
                node_row(2, "acme", "triage", "intake", "intake-agent"),
                node_row(3, "acme", "triage", "reply", "reply-agent"),
            ])
        };
        let ignored = serde_json::json!({});
        Replies::in_turn([
            serde_json::json!([graph_row("acme", "triage")]),
            nodes(),
            serde_json::json!({"columns": ["from", "to"], "rows": [["intake", "reply"]]}),
            no_loops(),
            sorted_chain(),
            nodes(),
            // intake: the members, its placement, and clearing it.
            serde_json::json!(members),
            ignored.clone(),
            ignored.clone(),
            ignored.clone(),
            // reply: the same again.
            serde_json::json!(members),
            ignored.clone(),
            ignored.clone(),
            ignored,
        ])
    }

    /// The plane's answers for a graph of one node, whose node is dispatched to `members`.
    ///
    /// One node rather than two wherever a test is about the fallback: the script is read in the
    /// order the calls arrive, and a node that falls back writes its placement twice — once at
    /// the member and once here — so a chain of two would put the second node's membership read
    /// at a position that depends on what the first node did.
    fn one_node(members: &[serde_json::Value]) -> Replies {
        let nodes = || serde_json::json!([node_row(2, "acme", "triage", "intake", "intake-agent")]);
        Replies::in_turn([
            serde_json::json!([graph_row("acme", "triage")]),
            nodes(),
            serde_json::json!({"columns": ["from", "to"], "rows": []}),
            no_loops(),
            serde_json::json!({
                "algo": "toposort",
                "count": 1,
                "results": [{"id": 2, "generation": 0}],
            }),
            nodes(),
            serde_json::json!(members),
        ])
    }

    /// The same fixture as [`running_in_a_cluster`], dispatching on a client of the test's own.
    ///
    /// **Round-robin rather than the default**, and not because these tests are about the
    /// strategy: this coordinator is a candidate for its own work, so under `random` a one-peer
    /// cluster sends each node to the peer only about half the time and every test below that
    /// asserts a dispatch happened would be a coin flip. Round-robin over a peer that sorts
    /// before [`here`] picks the peer first, every time. What `random` does over many draws is
    /// asserted where it belongs, in `crate::cluster`.
    async fn dispatching(
        named: &[&str],
        replies: Replies,
        client: ClusterClient,
    ) -> (Router, AppState, StubPlane) {
        let plane = StubPlane::start(replies).await;
        let state = super::super::harness::serving(
            store_with_templates(named).await,
            SERVING,
            Arc::new(Answering),
        )
        .with_graphs(Arc::new(GraphPlane::new(plane.client())))
        .with_cluster(here())
        .with_cluster_client(Arc::new(client))
        // Stated rather than inherited, so the tests that turn on how stale a member is read
        // against a number written here and not against whatever the default becomes.
        .with_heartbeat(Duration::from_secs(10))
        .with_strategy(ip_core::route::ClusterStrategy::RoundRobin);
        (super::super::router(state.clone()), state, plane)
    }

    /// The whole point of the milestone: a node runs somewhere else and nothing about the
    /// stream says so.
    ///
    /// The names are asserted against the local run's own — `a_graph_run_streams_every_node_and_
    /// then_the_graphs_own_answer` — rather than against a list written here, because what is
    /// being claimed is that the two are the same and not that both match something.
    #[tokio::test]
    async fn a_node_dispatched_to_a_member_streams_as_a_local_one_does() {
        // Two peers, so that a two-node graph still crosses on both: this coordinator takes
        // its turn in the rotation, so one peer and two nodes would leave the second here.
        let worker = ip_cluster::StubWorker::start(ip_cluster::Answers::Ran).await;
        let spare = ip_cluster::StubWorker::start(ip_cluster::Answers::Ran).await;
        let (router, state, _plane) = dispatching(
            &["intake-agent", "reply-agent"],
            dispatching_chain(&[
                member_row(&worker.address().to_string(), 1),
                member_row(&spare.address().to_string(), 1),
            ]),
            worker.client(),
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
            ]
        );
        // The worker's answer and not this deployment's: `Answering` says "four".
        let (_, done) = said.last().expect("a graph says it is over");
        assert_eq!(
            done["answer"],
            serde_json::json!({"reply": "41 crates in bay 3."})
        );
        assert_eq!(
            done["tokens"]["input"], 240,
            "the spend both members reported: {done}"
        );
        assert_eq!(
            worker.seen().len() + spare.seen().len(),
            2,
            "both nodes went to a member"
        );
    }

    /// What a dispatch names, which is a caller and never what that caller may do.
    #[tokio::test]
    async fn a_dispatch_names_the_run_the_node_and_whose_it_is() {
        // Two peers, so a two-node graph crosses on both: this coordinator takes its turn in
        // the rotation, so one peer would leave the second node here and there would be no
        // second dispatch to read. Which peer goes first is round-robin's order, the printed
        // address, so the two are sorted here rather than assumed.
        let one = ip_cluster::StubWorker::start(ip_cluster::Answers::Ran).await;
        let other = ip_cluster::StubWorker::start(ip_cluster::Answers::Ran).await;
        let ones = one.address().to_string();
        let others = other.address().to_string();
        let (first, second) = if ones < others {
            (&one, &other)
        } else {
            (&other, &one)
        };
        let (router, state, _plane) = dispatching(
            &["intake-agent", "reply-agent"],
            dispatching_chain(&[
                member_row(&one.address().to_string(), 1),
                member_row(&other.address().to_string(), 1),
            ]),
            one.client(),
        )
        .await;

        streamed(
            start_run(
                &router,
                &state,
                "alice",
                "/_ip/graph_run/triage?tenant=acme",
            )
            .await,
        )
        .await;

        let seen = first.seen();
        let work = &seen.first().expect("a dispatch").work;
        assert_eq!(work["node"], "intake");
        assert_eq!(work["graph"], "triage");
        assert_eq!(work["agent"], "intake-agent");
        assert_eq!(work["tenant"], "acme");
        assert_eq!(work["user"], "alice");
        assert_eq!(work["input"], "how many?");
        assert_eq!(work["owner"], serde_json::json!({"tenant": "acme"}));
        // Nothing about what that caller may do: the member reads that for itself.
        for widened in ["authority", "grants", "capabilities", "standing", "kind"] {
            assert!(
                work.get(widened).is_none(),
                "a dispatch carried {widened}: {work}"
            );
        }
        let crossed = second.seen();
        let next = &crossed.first().expect("the second dispatch").work;
        assert_eq!(next["node"], "reply");
        assert_eq!(
            next["run"], work["run"],
            "both nodes belong to one run: {seen:?}"
        );
        // The second node is handed what the first answered, in the keyed-json form a local
        // chain hands on — so a dispatched node's answer re-enters the graph as any other does.
        assert_eq!(next["input"], r#"{"intake":"41 crates in bay 3."}"#);
    }

    /// The record says where the work went, which is the whole use of keeping it.
    #[tokio::test]
    async fn a_dispatched_nodes_placement_names_the_member_and_not_this_machine() {
        // Two peers, so that a two-node graph still crosses on both: this coordinator takes
        // its turn in the rotation, so one peer and two nodes would leave the second here.
        let worker = ip_cluster::StubWorker::start(ip_cluster::Answers::Ran).await;
        let spare = ip_cluster::StubWorker::start(ip_cluster::Answers::Ran).await;
        let (router, state, plane) = dispatching(
            &["intake-agent", "reply-agent"],
            dispatching_chain(&[
                member_row(&worker.address().to_string(), 1),
                member_row(&spare.address().to_string(), 1),
            ]),
            worker.client(),
        )
        .await;

        streamed(
            start_run(
                &router,
                &state,
                "alice",
                "/_ip/graph_run/triage?tenant=acme",
            )
            .await,
        )
        .await;

        let recorded: Vec<String> = plane
            .seen()
            .into_iter()
            .filter(|one| one.method == "node.create")
            .filter_map(|one| {
                one.params["properties"]["address"]
                    .as_str()
                    .map(str::to_owned)
            })
            .collect();
        let mut named = recorded.clone();
        named.sort();
        let mut peers = vec![worker.address().to_string(), spare.address().to_string()];
        peers.sort();
        assert_eq!(
            named, peers,
            "a placement named the wrong machine for a node running elsewhere"
        );
        assert!(
            !recorded.contains(&here().to_string()),
            "the coordinator recorded itself: {recorded:?}"
        );
    }

    /// Picked for its own node, this machine runs it rather than dialling itself.
    ///
    /// A cluster of one is this machine, so it is the only candidate and is certain to be
    /// picked. Nothing is dispatched: the node runs here, down the same path a deployment with
    /// no cluster at all takes, and the placement names this address. Sending the work to our
    /// own listener would buy a round trip, a second authority built from the same rows and a
    /// second stream, and arrive at the same turns.
    #[tokio::test]
    async fn a_coordinator_picked_for_its_own_node_runs_it_here_rather_than_dialling_itself() {
        let worker = ip_cluster::StubWorker::start(ip_cluster::Answers::Ran).await;
        let (router, state, plane) = dispatching(
            &["intake-agent", "reply-agent"],
            dispatching_chain(&[member_row(&here().to_string(), 1)]),
            worker.client(),
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

        assert!(
            worker.seen().is_empty(),
            "this machine dispatched to itself"
        );
        let (_, done) = said.last().expect("a graph says it is over");
        assert_eq!(
            done["answer"],
            serde_json::json!({"reply": "four"}),
            "the node did not run here: {done}"
        );
        let recorded: Vec<String> = plane
            .seen()
            .into_iter()
            .filter(|one| one.method == "node.create")
            .filter_map(|one| {
                one.params["properties"]["address"]
                    .as_str()
                    .map(str::to_owned)
            })
            .collect();
        assert_eq!(recorded, [here().to_string(), here().to_string()]);
    }

    /// A member that stopped writing goes stale where it stands, and three heartbeats — thirty
    /// seconds on the ten-second interval this fixture sets — is how long it is believed in.
    #[tokio::test]
    async fn a_member_that_stopped_saying_it_is_here_is_not_picked() {
        let worker = ip_cluster::StubWorker::start(ip_cluster::Answers::Ran).await;
        let (router, state, _plane) = dispatching(
            &["intake-agent", "reply-agent"],
            dispatching_chain(&[member_row(&worker.address().to_string(), 31)]),
            worker.client(),
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

        assert!(
            worker.seen().is_empty(),
            "a member thirty-one seconds quiet was dispatched to"
        );
        let (_, done) = said.last().expect("a graph says it is over");
        assert_eq!(done["answer"], serde_json::json!({"reply": "four"}));
    }

    /// The same member one heartbeat later, which is late rather than gone.
    #[tokio::test]
    async fn a_member_merely_late_on_one_heartbeat_is_still_picked() {
        let worker = ip_cluster::StubWorker::start(ip_cluster::Answers::Ran).await;
        let (router, state, _plane) = dispatching(
            &["intake-agent", "reply-agent"],
            dispatching_chain(&[member_row(&worker.address().to_string(), 12)]),
            worker.client(),
        )
        .await;

        streamed(
            start_run(
                &router,
                &state,
                "alice",
                "/_ip/graph_run/triage?tenant=acme",
            )
            .await,
        )
        .await;

        assert_eq!(
            worker.seen().len(),
            1,
            "a member twelve seconds quiet was passed over"
        );
    }

    /// A plane that cannot say who is in the cluster is a run with no cluster.
    #[tokio::test]
    async fn a_membership_that_cannot_be_read_runs_every_node_here() {
        let worker = ip_cluster::StubWorker::start(ip_cluster::Answers::Ran).await;
        let (router, state, _plane) = dispatching(
            &["intake-agent", "reply-agent"],
            chain_of_two(sorted_chain()),
            worker.client(),
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

        assert!(worker.seen().is_empty());
        let (_, done) = said.last().expect("a graph says it is over");
        assert_eq!(done["answer"], serde_json::json!({"reply": "four"}));
    }

    /// Nothing was spent, so the node runs here. This is the safe half of the boundary.
    #[tokio::test]
    async fn a_member_that_cannot_be_reached_at_all_falls_back_to_running_here() {
        let gone = ip_cluster::vacant_address().await;
        let (router, state, _plane) = dispatching(
            &["intake-agent", "reply-agent"],
            dispatching_chain(&[member_row(&gone.to_string(), 1)]),
            ClusterClient::new(ip_cluster::stub_settings()),
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
            ]
        );
        let (_, done) = said.last().expect("a graph says it is over");
        assert_eq!(
            done["answer"],
            serde_json::json!({"reply": "four"}),
            "a dead member's node did not run here: {done}"
        );
    }

    /// A member that will not run the node says so as its only event, before taking a turn, so
    /// the coordinator — which resolved every node before the stream opened — runs it itself.
    #[tokio::test]
    async fn a_member_that_refuses_before_any_turn_falls_back_to_running_here() {
        let worker = ip_cluster::StubWorker::start(ip_cluster::Answers::Refused).await;
        let (router, state, _plane) = dispatching(
            &["intake-agent"],
            one_node(&[member_row(&worker.address().to_string(), 1)]),
            worker.client(),
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

        assert_eq!(worker.seen().len(), 1, "the node was offered to the member");
        let (_, done) = said.last().expect("a graph says it is over");
        assert_eq!(
            done["answer"],
            serde_json::json!({"intake": "four"}),
            "a refused node was not run here: {done}"
        );
        assert_eq!(done["stopped_at"], serde_json::Value::Null);
    }

    /// **The unsafe half, and the one ruling of this commit worth getting right.** A member that
    /// said something has reported a turn, and a turn is reported before it is taken, so the money
    /// is already gone. The node fails where it is and is never run a second time.
    #[tokio::test]
    async fn a_member_that_stops_after_an_event_fails_the_node_rather_than_rerunning_it() {
        let worker = ip_cluster::StubWorker::start(ip_cluster::Answers::CutOff).await;
        let (router, state, _plane) = dispatching(
            &["intake-agent"],
            one_node(&[member_row(&worker.address().to_string(), 1)]),
            worker.client(),
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

        assert_eq!(
            worker.seen().len(),
            1,
            "the node was offered once and never again: {said:?}"
        );
        let names: Vec<&str> = said.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            ["node", "turn", "node_done", "done"],
            "what the member managed to say is in the stream, and then the node ends: {said:?}"
        );
        let (_, done) = said.last().expect("a graph says it is over");
        assert_eq!(
            done["stopped_at"], "intake",
            "a cut-off node did not stop the graph: {done}"
        );
        let (_, ended) = said
            .iter()
            .find(|(name, _)| name == "node_done")
            .expect("the node ends");
        assert_eq!(ended["stop"], "refused", "{ended}");
        let why = ended["why"]
            .as_str()
            .unwrap_or_else(|| panic!("a cut-off node said no reason: {ended}"));
        assert!(
            why.contains("intake") && why.contains(&worker.address().to_string()),
            "the failure does not say which node stopped where: {why}"
        );
    }

    /// A deployment that cannot say where it answers dispatches nothing, because it could not
    /// tell its own membership row from a peer's.
    #[tokio::test]
    async fn a_deployment_with_no_address_of_its_own_dispatches_nothing() {
        let worker = ip_cluster::StubWorker::start(ip_cluster::Answers::Ran).await;
        let plane = StubPlane::start(dispatching_chain(&[member_row(
            &worker.address().to_string(),
            1,
        )]))
        .await;
        let state = super::super::harness::serving(
            store_with_templates(&["intake-agent", "reply-agent"]).await,
            SERVING,
            Arc::new(Answering),
        )
        .with_graphs(Arc::new(GraphPlane::new(plane.client())))
        .with_cluster_client(Arc::new(worker.client()));
        let router = super::super::router(state.clone());

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

        assert!(worker.seen().is_empty());
        let (_, done) = said.last().expect("a graph says it is over");
        assert_eq!(done["answer"], serde_json::json!({"reply": "four"}));
    }

    /// A deployment holding no cluster secret has no client, so every node runs here — which is
    /// what every deployment did before this commit and must keep doing.
    #[tokio::test]
    async fn a_deployment_in_no_cluster_runs_every_node_here() {
        let (router, state, plane) = running_in_a_cluster(
            &["intake-agent", "reply-agent"],
            dispatching_chain(&[member_row("10.0.0.9:8080", 1)]),
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

        let (_, done) = said.last().expect("a graph says it is over");
        assert_eq!(done["answer"], serde_json::json!({"reply": "four"}));
        // Not even read: a deployment that cannot dispatch does not ask who is in the cluster.
        let asked = plane
            .seen()
            .into_iter()
            .filter(|one| one.method == "plane.query")
            .count();
        // Four queries are the graph's own — its row, its nodes, its edges, and its nodes
        // again to turn the order's ids back into names. A membership read would be a fifth.
        assert_eq!(
            asked, 4,
            "a run that dispatches nothing reads no membership"
        );
    }

    /// The same fixture again, picking members the way the test is about.
    async fn spreading(
        named: &[&str],
        replies: Replies,
        client: ClusterClient,
        strategy: ip_core::route::ClusterStrategy,
    ) -> (Router, AppState, StubPlane) {
        let plane = StubPlane::start(replies).await;
        let state = super::super::harness::serving(
            store_with_templates(named).await,
            SERVING,
            Arc::new(Answering),
        )
        .with_graphs(Arc::new(GraphPlane::new(plane.client())))
        .with_cluster(here())
        .with_cluster_client(Arc::new(client))
        .with_heartbeat(Duration::from_secs(10))
        .with_strategy(strategy);
        (super::super::router(state.clone()), state, plane)
    }

    /// Round-robin over two live members sends one node to each, which is the whole point of
    /// configuring it: the strategy reaches the dispatch rather than stopping at the config.
    #[tokio::test]
    async fn round_robin_sends_a_graphs_two_nodes_to_two_different_members() {
        let one = ip_cluster::StubWorker::start(ip_cluster::Answers::Ran).await;
        let other = ip_cluster::StubWorker::start(ip_cluster::Answers::Ran).await;
        let (router, state, _plane) = spreading(
            &["intake-agent", "reply-agent"],
            dispatching_chain(&[
                member_row(&one.address().to_string(), 1),
                member_row(&other.address().to_string(), 1),
            ]),
            one.client(),
            ip_core::route::ClusterStrategy::RoundRobin,
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

        assert_eq!(
            (one.seen().len(), other.seen().len()),
            (1, 1),
            "both nodes went to the same member: {said:?}"
        );
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
            ]
        );
    }

    /// Whichever member each node went to, the record says which — so a reader of the plane can
    /// tell a spread run from one that piled onto a single machine.
    #[tokio::test]
    async fn each_dispatched_nodes_placement_names_the_member_that_ran_it() {
        let one = ip_cluster::StubWorker::start(ip_cluster::Answers::Ran).await;
        let other = ip_cluster::StubWorker::start(ip_cluster::Answers::Ran).await;
        let (router, state, plane) = spreading(
            &["intake-agent", "reply-agent"],
            dispatching_chain(&[
                member_row(&one.address().to_string(), 1),
                member_row(&other.address().to_string(), 1),
            ]),
            one.client(),
            ip_core::route::ClusterStrategy::RoundRobin,
        )
        .await;

        streamed(
            start_run(
                &router,
                &state,
                "alice",
                "/_ip/graph_run/triage?tenant=acme",
            )
            .await,
        )
        .await;

        let mut recorded: Vec<String> = plane
            .seen()
            .into_iter()
            .filter(|seen| seen.method == "node.create")
            .filter_map(|seen| {
                seen.params["properties"]["address"]
                    .as_str()
                    .map(str::to_owned)
            })
            .collect();
        recorded.sort_unstable();
        let mut expected = vec![one.address().to_string(), other.address().to_string()];
        expected.sort_unstable();

        assert_eq!(recorded, expected);
    }

    /// A cluster whose only member is this machine runs every node here, whichever strategy is
    /// configured, because this machine is the only thing there is to pick.
    ///
    /// Asserted of the **default** strategy on purpose: it is the one case about `random` at this
    /// level that is a certainty rather than a draw. Over one peer and this machine, random sends
    /// each node away about half the time, so a route test that asserted a dispatch would be a
    /// coin flip — what random does over many draws is asserted in `crate::cluster`, 200 of them.
    #[tokio::test]
    async fn the_default_strategy_with_nobody_else_live_runs_every_node_here() {
        let worker = ip_cluster::StubWorker::start(ip_cluster::Answers::Ran).await;
        let (router, state, _plane) = spreading(
            &["intake-agent", "reply-agent"],
            dispatching_chain(&[]),
            worker.client(),
            ip_core::route::ClusterStrategy::default(),
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

        assert!(
            worker.seen().is_empty(),
            "a node was dispatched to a member the plane does not hold"
        );
        let (_, done) = said.last().expect("a graph says it is over");
        assert_eq!(
            done["answer"],
            serde_json::json!({"reply": "four"}),
            "the nodes did not run here: {done}"
        );
    }

    /// **The coordinator takes its share.** Two nodes over one peer and this machine is one
    /// each, because a member that happens to be the one a caller asked is still a machine with
    /// a gateway on it — leaving it out would make the busiest member the one doing none of the
    /// work.
    ///
    /// Round-robin, so the turn is a certainty and not a draw: the peer sorts before [`here`],
    /// takes the first node, and the second comes back to this machine.
    #[tokio::test]
    async fn a_coordinator_takes_its_turn_in_the_rotation_like_any_other_member() {
        let worker = ip_cluster::StubWorker::start(ip_cluster::Answers::Ran).await;
        let (router, state, plane) = spreading(
            &["intake-agent", "reply-agent"],
            dispatching_chain(&[member_row(&worker.address().to_string(), 1)]),
            worker.client(),
            ip_core::route::ClusterStrategy::RoundRobin,
        )
        .await;

        streamed(
            start_run(
                &router,
                &state,
                "alice",
                "/_ip/graph_run/triage?tenant=acme",
            )
            .await,
        )
        .await;

        assert_eq!(
            worker.seen().len(),
            1,
            "the peer took both nodes, so this machine was passed over"
        );
        let mut recorded: Vec<String> = plane
            .seen()
            .into_iter()
            .filter(|one| one.method == "node.create")
            .filter_map(|one| {
                one.params["properties"]["address"]
                    .as_str()
                    .map(str::to_owned)
            })
            .collect();
        recorded.sort();
        let mut both = vec![worker.address().to_string(), here().to_string()];
        both.sort();
        assert_eq!(
            recorded, both,
            "a node ran on each, so each should have a placement naming it"
        );
    }

    /// This machine is a candidate however the plane reads its own row — fresh, stale or absent.
    ///
    /// The plane here holds a row for this machine far past any heartbeat, beside a live peer. A
    /// coordinator that believed that row would hand the peer both nodes; one that knows it is
    /// running takes its turn. The point is not kindness to ourselves: a plane this machine
    /// cannot write to would otherwise send every node away at the moment the cluster is least
    /// able to take it.
    #[tokio::test]
    async fn a_coordinator_is_a_candidate_even_with_no_fresh_row_of_its_own() {
        let worker = ip_cluster::StubWorker::start(ip_cluster::Answers::Ran).await;
        let (router, state, _plane) = spreading(
            &["intake-agent", "reply-agent"],
            dispatching_chain(&[
                member_row(&worker.address().to_string(), 1),
                member_row(&here().to_string(), 86_400),
            ]),
            worker.client(),
            ip_core::route::ClusterStrategy::RoundRobin,
        )
        .await;

        streamed(
            start_run(
                &router,
                &state,
                "alice",
                "/_ip/graph_run/triage?tenant=acme",
            )
            .await,
        )
        .await;

        assert_eq!(
            worker.seen().len(),
            1,
            "a stale row of our own took this machine out of its own rotation"
        );
    }

    /// What is published under a run's signal topic, if anything is.
    async fn signalled(state: &AppState, run: &serde_json::Value) -> Option<u64> {
        let run = ip_graph::GraphRunId::from_hex(run.as_str().expect("a run id is a string"))
            .expect("the stream names a run this crate made");
        let topic = crate::cluster::signal_topic(run).expect("a run names a topic");
        state
            .shared_cache()
            .published(&topic)
            .await
            .expect("the cache answers")
            .map(|published| published.revision)
    }

    /// A graph that stops tells the cluster, so a node of it running on another member hears.
    ///
    /// The node is stopped by a member cutting it off, which is the one way this fixture can make
    /// a node fail — and it is also the realistic one: the node that broke broke elsewhere.
    #[tokio::test]
    async fn a_run_that_stops_publishes_the_stop_for_the_members_running_its_nodes() {
        let worker = ip_cluster::StubWorker::start(ip_cluster::Answers::CutOff).await;
        let (router, state, _plane) = dispatching(
            &["intake-agent"],
            one_node(&[member_row(&worker.address().to_string(), 1)]),
            worker.client(),
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

        let (_, done) = said.last().expect("a graph says it is over");
        assert_eq!(done["stopped_at"], "intake", "{done}");
        assert_eq!(
            signalled(&state, &done["run"]).await,
            Some(1),
            "a stopped run told nobody: {done}"
        );
    }

    /// A graph that answers has no stop to pass on, so nothing is published at all.
    #[tokio::test]
    async fn a_run_that_answers_publishes_no_stop() {
        let worker = ip_cluster::StubWorker::start(ip_cluster::Answers::Ran).await;
        let (router, state, _plane) = dispatching(
            &["intake-agent"],
            one_node(&[member_row(&worker.address().to_string(), 1)]),
            worker.client(),
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

        let (_, done) = said.last().expect("a graph says it is over");
        assert_eq!(done["stopped_at"], serde_json::Value::Null);
        assert_eq!(signalled(&state, &done["run"]).await, None);
    }

    /// A deployment that never sends a node elsewhere has nobody running one of its nodes, so
    /// the publication would be a write nothing follows.
    #[tokio::test]
    async fn a_deployment_that_dispatches_nothing_publishes_no_stop() {
        let (router, state, _plane) = running(&["intake-agent"], one_node(&[])).await;

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

        let (_, done) = said.last().expect("a graph says it is over");
        assert_eq!(signalled(&state, &done["run"]).await, None);
    }

    /// Said twice is said once: a run that has stopped does not un-stop, and a publication lands
    /// only above what is there — so a coordinator repeating itself raises no second announcement.
    #[tokio::test]
    async fn saying_a_run_stopped_twice_announces_it_once() {
        let (_, state, _plane) = running(&[], answering_plane()).await;
        let run = ip_graph::GraphRunId::generate().expect("a run id");
        let signalling = crate::cluster::Signalling::new(Arc::clone(state.shared_cache()));

        <crate::cluster::Signalling as Stopping>::stopped(&signalling, run).await;
        let once = signalled(&state, &serde_json::json!(run.to_string())).await;
        <crate::cluster::Signalling as Stopping>::stopped(&signalling, run).await;

        assert_eq!(once, Some(1));
        assert_eq!(
            signalled(&state, &serde_json::json!(run.to_string())).await,
            once
        );
    }

    /// A cache that cannot take the stop does not fail the run: the node it would have cut short
    /// is already paid for, and the run ends on this machine either way.
    #[tokio::test]
    async fn a_stop_that_cannot_be_published_does_not_fail_the_run() {
        let (_, state, _plane) = running(&[], answering_plane()).await;
        let run = ip_graph::GraphRunId::generate().expect("a run id");
        state.cache_failure().fail_now();

        // Answers nothing and refuses nothing: the warning is all that is left of it.
        <crate::cluster::Signalling as Stopping>::stopped(
            &crate::cluster::Signalling::new(Arc::clone(state.shared_cache())),
            run,
        )
        .await;

        state.cache_failure().answer_again();
        assert_eq!(
            signalled(&state, &serde_json::json!(run.to_string())).await,
            None
        );
    }

    /// What is kept for this run's one node, if anything is.
    async fn kept_answer(state: &AppState, run: &serde_json::Value) -> Option<NodeRan> {
        let run = ip_graph::GraphRunId::from_hex(run.as_str().expect("a run id is a string"))
            .expect("the stream names a run this crate made");
        crate::cluster::Kept::new(Arc::clone(state.shared_cache()))
            .answer(run, &NodeName::new("intake").expect("a node name"))
            .await
    }

    /// The run id of the one node in these fixtures, for a test that writes the answer itself.
    fn one_run() -> ip_graph::GraphRunId {
        ip_graph::GraphRunId::generate().expect("a run id")
    }

    /// A node that ran and spent something is kept, so a second coordinator can see what was paid.
    #[tokio::test]
    async fn what_a_node_answered_is_kept_for_a_coordinator_that_asks_later() {
        let worker = ip_cluster::StubWorker::start(ip_cluster::Answers::Ran).await;
        let (router, state, _plane) = dispatching(
            &["intake-agent"],
            one_node(&[member_row(&worker.address().to_string(), 1)]),
            worker.client(),
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

        let (_, done) = said.last().expect("a graph says it is over");
        let kept = kept_answer(&state, &done["run"])
            .await
            .expect("what the node answered was not kept");
        assert_eq!(kept.answer, done["answer"]["intake"], "{done}");
        assert_ne!(kept.spend, Spend::NOTHING);
    }

    /// The saving itself, and it is driven directly rather than over the route for a reason:
    /// nothing in M27 starts a run under an id somebody else already used, so the only caller
    /// that can reach this is the second coordinator the cache exists for. The route generates a
    /// fresh id every time and would never find a kept answer.
    ///
    /// `prepared` is left **empty** and `dispatching` `None` on purpose. A node that reached
    /// either would answer "never prepared", so an answer that is the kept one is proof the read
    /// came before the template, the run id, the dispatch and the gateway alike.
    #[tokio::test]
    async fn a_node_already_answered_is_neither_run_nor_dispatched() {
        let (_, state, _plane) = running(&["intake-agent"], one_node(&[])).await;
        let run = one_run();
        let node = NodeName::new("intake").expect("a node name");
        let kept = Arc::new(crate::cluster::Kept::new(Arc::clone(state.shared_cache())));
        let answered = NodeRan {
            answer: "what the first coordinator paid for".to_owned(),
            stopped: ip_run::Stopped::Answered,
            spend: Spend::NOTHING,
            traces: Vec::new(),
        };
        kept.keep(run, &node, &answered).await;
        let (say, _said) = tokio::sync::mpsc::unbounded_channel();
        let nodes = Nodes {
            gateway: state.shared_gateway(),
            prepared: BTreeMap::new(),
            say,
            placing: None,
            dispatching: None,
            kept: Some(Arc::clone(&kept)),
        };

        let ran = RunsNode::run(
            &nodes,
            RunningNode {
                run,
                node: GraphNode {
                    name: node,
                    agent: ip_agents::AgentId::new("intake-agent").expect("a template name"),
                },
                input: "asked again".to_owned(),
                signals: ip_run::RunSignal::never(),
            },
        )
        .await;

        assert_eq!(ran, answered, "the node was run again rather than recalled");
    }

    /// A node that spent nothing is left free to run again: there is no payment to protect, and a
    /// kept refusal would answer every retry the same way for a day.
    ///
    /// Driven at the rule rather than over the route, because the route cannot produce a node
    /// that spent nothing *and* finished: commit 5 answers a member's refusal by running the node
    /// at home, which then spends. The two arms are what the rule is, so both are asserted here.
    #[tokio::test]
    async fn a_node_is_kept_when_it_spent_something_and_not_when_it_spent_nothing() {
        let (_, state, _plane) = running(&["intake-agent"], one_node(&[])).await;
        let kept = Arc::new(crate::cluster::Kept::new(Arc::clone(state.shared_cache())));
        let (say, _said) = tokio::sync::mpsc::unbounded_channel();
        let nodes = Nodes {
            gateway: state.shared_gateway(),
            prepared: BTreeMap::new(),
            say,
            placing: None,
            dispatching: None,
            kept: Some(Arc::clone(&kept)),
        };
        let free = one_run();
        let paid = one_run();
        let node = NodeName::new("intake").expect("a node name");
        let answered = |spend| NodeRan {
            answer: "four".to_owned(),
            stopped: ip_run::Stopped::Answered,
            spend,
            traces: Vec::new(),
        };

        nodes
            .worth_keeping(free, &node, &answered(Spend::NOTHING))
            .await;
        nodes
            .worth_keeping(
                paid,
                &node,
                &answered(Spend::new(
                    ip_core::Tokens::ZERO,
                    ip_core::Money::from_micros(12),
                )),
            )
            .await;

        assert!(
            kept.answer(free, &node).await.is_none(),
            "a node that paid nothing was kept, so a retry would get its refusal for a day"
        );
        assert!(kept.answer(paid, &node).await.is_some());
    }

    /// Best effort, as everywhere else: a cache that cannot answer is a node that runs, never a
    /// run that refuses. One node run twice against every graph run failing is not a trade.
    ///
    /// At the reader rather than over the route: `None` is exactly what makes the caller above
    /// fall through to running the node, and a route test would have to break the cache the
    /// session cookie is minted from to reach it.
    #[tokio::test]
    async fn a_cache_that_cannot_answer_keeps_nothing_and_recalls_nothing() {
        let (_, state, _plane) = running(&["intake-agent"], one_node(&[])).await;
        let kept = crate::cluster::Kept::new(Arc::clone(state.shared_cache()));
        let run = one_run();
        let node = NodeName::new("intake").expect("a node name");
        state.cache_failure().fail_now();

        kept.keep(
            run,
            &node,
            &NodeRan {
                answer: "four".to_owned(),
                stopped: ip_run::Stopped::Answered,
                spend: Spend::NOTHING,
                traces: Vec::new(),
            },
        )
        .await;
        assert!(kept.answer(run, &node).await.is_none());

        state.cache_failure().answer_again();
        assert!(
            kept.answer(run, &node).await.is_none(),
            "a cache that could not take the answer kept it anyway"
        );
    }

    /// A deployment that is the only machine serving a run holds the answers in memory while it
    /// does, so keeping them would be a write nobody reads.
    #[tokio::test]
    async fn a_deployment_alone_keeps_nothing() {
        let (router, state, _plane) = running(&["intake-agent"], one_node(&[])).await;

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

        let (_, done) = said.last().expect("a graph says it is over");
        assert_eq!(done["answer"]["intake"], "four", "{done}");
        assert!(kept_answer(&state, &done["run"]).await.is_none());
    }
}
