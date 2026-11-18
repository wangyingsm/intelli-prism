//! Keeping a graph in a dr-strange plane, and reading it back out.
//!
//! The mapping, in one place so the write and the read cannot drift: a graph is one node labelled
//! [`GRAPH_LABEL`] carrying what the graph itself says, its nodes are nodes labelled
//! [`NODE_LABEL`], and an answer travelling from one node to another is a [`FEEDS`] edge between
//! them.
//!
//! **Every node and every edge carries its owner, and every read filters on it in the plan.** The
//! store knows nothing about tenants, so this is the whole of what keeps one tenant's graphs out of
//! another's reads — which is why the plan each read sends is asserted, call by call, in the tests
//! below rather than taken on trust.

use std::collections::BTreeSet;

use ip_agents::AgentId;
use ip_drsg::DrsgClient;
use serde_json::{Value, json};

use crate::error::GraphError;
use crate::graph::{
    GraphEdge, GraphNode, GraphOwner, LoopBack, LoopCap, MaxNodes, RunGraph, RunGraphParts,
};
use crate::id::{Condition, GraphId, GraphName, GraphRunId, NodeName};
use crate::member::ClusterMember;
use crate::order::{Generation, Placed, RunOrder};
use crate::placement::Placement;
use ip_core::Timestamp;
use ip_core::route::ClusterAddress;

/// The label on the one node that holds what a graph says about itself.
pub const GRAPH_LABEL: &str = "RunGraph";

/// The label on every node of every graph.
pub const NODE_LABEL: &str = "GraphNode";

/// The type of edge an answer travels along.
pub const FEEDS: &str = "FEEDS";

/// The type of edge a node's unfinished work may travel back along.
///
/// A second type beside [`FEEDS`] rather than a property on one, because that is what keeps every
/// algorithm asked of the store answering over the feeding edges alone: a graph is a DAG in what
/// it feeds, while its loop backs are expected to close cycles. Both `toposort` and `cycles` name
/// [`FEEDS`] and are asserted to, so widening either to every edge type cannot pass the tests.
pub const LOOPS_BACK: &str = "LOOPS_BACK";

/// How many times one loop back may be taken, which is the one thing a loop back's edge carries.
const CAP: &str = "cap";

/// The type of edge a conditional one is written as a second time, beside its [`FEEDS`] row.
///
/// The exact inverse of the reasoning on [`LOOPS_BACK`], and for the same cause. A branch **is** a
/// feeding edge — it delivers its source's answer to its target whenever it is taken — so its
/// `FEEDS` row is written like any other edge's and both algorithms walk it, which is what places a
/// branch's target in a later generation than its source and refuses a branch that closes a cycle.
/// This second row exists because the condition has to hang on a hop a read can ask about, and
/// because a branch is what a person reading the plane should see. Both rows are written, every
/// time; neither is enough on its own.
pub const BRANCH_FEEDS: &str = "BRANCH_FEEDS";

/// When a branch is worth taking, which is the one thing a branch's edge carries.
const CONDITION: &str = "condition";

/// The label on every record of where a node is running.
///
/// A third label beside [`GRAPH_LABEL`] and [`NODE_LABEL`], so a record of a run collides with no
/// stored graph: what a graph says about itself and where one of its nodes is running now are two
/// different kinds of thing with the same owner and the same name.
pub const PLACEMENT_LABEL: &str = "GraphPlacement";

/// The label on every member's record of itself.
///
/// The one label here no tenant owns: a member is the deployment's own machine, so nothing in a
/// member read filters on an owner and no tenant's read can reach one.
pub const MEMBER_LABEL: &str = "ClusterMember";

/// Which graph a row belongs to.
const GRAPH: &str = "graph";

/// Whose it is, and so the one property every read filters on.
const OWNER: &str = "owner";

/// What a graph, or a node of one, is called.
const NAME: &str = "name";

/// The template a node runs.
const AGENT: &str = "agent";

/// The ceiling a graph carries.
const MAX_NODES: &str = "max_nodes";

/// Which run a record of a running node belongs to.
const RUN: &str = "run";

/// Which node of the graph is running.
const NODE: &str = "node";

/// Where the machine running it answers its own cluster.
const ADDRESS: &str = "address";

/// When it was dispatched, in unix seconds.
const AT: &str = "at";

/// The key a store addresses one graph's own node by.
fn graph_key(owner: &GraphOwner, id: &GraphId) -> String {
    format!("{GRAPH_LABEL}/{}/{id}", owner.written())
}

/// The key a store addresses one node of one graph by.
fn node_key(owner: &GraphOwner, id: &GraphId, name: &NodeName) -> String {
    format!("{NODE_LABEL}/{}/{id}/{name}", owner.written())
}

/// The key a store addresses one record of one running node by.
///
/// The run and the node, and neither alone: one run has a record per node, and one node of one
/// graph may be running in two runs at once.
fn placement_key(run: &GraphRunId, node: &NodeName) -> String {
    format!("{PLACEMENT_LABEL}/{}/{node}", run.to_hex())
}

/// The key a store addresses one member by.
///
/// The address and nothing else: a machine that restarts is the same member, and a second row for
/// it would be a second machine as far as every reader is concerned.
fn member_key(address: &ClusterAddress) -> String {
    format!("{MEMBER_LABEL}/{address}")
}

/// The label that is one graph's nodes and nothing else.
///
/// A second label, beside [`NODE_LABEL`], because `plane.algo` can be narrowed to a label and to
/// nothing else: over `GraphNode` a topological sort would run across every graph in the plane, and
/// one tenant's cycle would then be an error on every tenant's read. `/` separates the parts, which
/// neither an owner nor a graph id may carry, so no two graphs can spell one label.
fn scope_label(owner: &GraphOwner, id: &GraphId) -> String {
    format!("Graph/{}/{id}", owner.written())
}

/// `property = value`, as a plan says it.
fn eq(property: &str, value: &str) -> Value {
    json!({
        "Compare": {
            "op": "Eq",
            "lhs": {"Property": property},
            "rhs": {"Literal": {"Str": value}},
        }
    })
}

/// The filter every read carries: this graph, and this owner.
///
/// Both halves, always and in the plan itself. An owner filter applied to rows after they arrive
/// would mean the store had already handed over another tenant's graph.
fn owned_by(owner: &GraphOwner, id: &GraphId) -> Value {
    json!({
        "Logic": {
            "op": "And",
            "lhs": eq(GRAPH, id.as_str()),
            "rhs": eq(OWNER, owner.written()),
        }
    })
}

/// How much of one owner's listing to answer with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listing {
    /// The most graphs to answer with.
    pub limit: u32,
    /// How many to pass over first.
    pub offset: u32,
}

/// One graph as a listing reports it: what it says about itself, without its nodes.
///
/// A graph's nodes and edges are two further reads each, so a listing of fifty graphs would be a
/// hundred and fifty round trips to answer a question nobody asked. [`GraphPlane::get`] is what
/// answers with a whole graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphSummary {
    /// What the graph is spelled by.
    pub id: GraphId,
    /// What it is called.
    pub name: GraphName,
    /// The most nodes it may hold.
    pub max_nodes: MaxNodes,
}

/// The plan that lists one owner's graphs, as a table of what each says about itself.
///
/// A projection rather than node rows for two reasons: a listing wants three properties of each
/// graph and not every property of each, and the projection is the only place a plan can *skip*
/// rows, which is what paging is. The owner is projected alongside them so the rows can be checked
/// the way [`ours`] checks a single read's.
fn graphs_plan(owner: &GraphOwner, listing: Listing) -> Value {
    let column =
        |property: &str| json!({"name": property, "expr": {"Value": {"Property": property}}});
    json!({
        "source": {"ScanLabel": GRAPH_LABEL},
        "steps": [{"Filter": eq(OWNER, owner.written())}],
        "project": {
            "items": [column(GRAPH), column(NAME), column(MAX_NODES), column(OWNER)],
            "distinct": false,
            // By the id, so two pages of one listing cannot answer with one graph twice and
            // leave another out.
            "order_by": [{"column": 0, "descending": false}],
            "skip": listing.offset,
            "limit": listing.limit,
        },
    })
}

/// The plan that reads one graph's own row.
fn graph_plan(owner: &GraphOwner, id: &GraphId) -> Value {
    json!({
        "source": {"ScanLabel": GRAPH_LABEL},
        "steps": [{"Filter": owned_by(owner, id)}],
    })
}

/// The plan that reads one graph's nodes.
fn nodes_plan(owner: &GraphOwner, id: &GraphId) -> Value {
    json!({
        "source": {"ScanLabel": NODE_LABEL},
        "steps": [{"Filter": owned_by(owner, id)}],
    })
}

/// The plan that reads one graph's edges, as a table of the two names each joins.
///
/// The filter is applied twice, once at each end of the hop: an edge is only this graph's if both
/// the node it leaves and the node it reaches are.
fn edges_plan(owner: &GraphOwner, id: &GraphId) -> Value {
    json!({
        "source": {"ScanLabel": NODE_LABEL},
        "steps": [
            {"Filter": owned_by(owner, id)},
            {"Expand": {"dir": "Out", "edge_type": FEEDS}},
            {"Filter": owned_by(owner, id)},
        ],
        "project": {
            "items": [
                {
                    "name": "from",
                    "expr": {"Value": {"At": {
                        "binding": {"Node": 0},
                        "inner": {"Property": NAME},
                    }}},
                },
                {"name": "to", "expr": {"Value": {"Property": NAME}}},
            ],
            "distinct": true,
            "order_by": [],
            "skip": null,
            "limit": null,
        },
    })
}

/// The statement that reads one graph's loop backs: the two names each joins, and its cap.
///
/// A statement rather than the read plan [`edges_plan`] is, and **measured rather than chosen**: a
/// plan projecting the hop's own property — `{"binding": {"Edge": 0}}` — answers `null` against a
/// real plane, at that index and at every other, while the property is there and a relationship
/// variable reads it. An edge's property is a statement's to read.
///
/// Both ends are filtered, as the plan filters twice, because a loop back is only this graph's if
/// the node it leaves and the node it reaches both are.
fn stored_loops() -> String {
    format!(
        "MATCH (a:{NODE_LABEL})-[r:{LOOPS_BACK}]->(b:{NODE_LABEL}) \
         WHERE a.{GRAPH} = $graph AND a.{OWNER} = $owner \
         AND b.{GRAPH} = $graph AND b.{OWNER} = $owner \
         RETURN DISTINCT a.{NAME} AS from, b.{NAME} AS to, r.{CAP} AS {CAP}"
    )
}

/// The statement that reads one graph's branches: the two names each joins, and its condition.
///
/// A statement for the reason [`stored_loops`] records and measured the same way: what a branch
/// adds to an edge is a property of the hop, and a read plan asking for one is answered `null`.
fn stored_branches() -> String {
    format!(
        "MATCH (a:{NODE_LABEL})-[r:{BRANCH_FEEDS}]->(b:{NODE_LABEL}) \
         WHERE a.{GRAPH} = $graph AND a.{OWNER} = $owner \
         AND b.{GRAPH} = $graph AND b.{OWNER} = $owner \
         RETURN DISTINCT a.{NAME} AS from, b.{NAME} AS to, r.{CONDITION} AS {CONDITION}"
    )
}

/// The statement that answers every node one node's work reaches by feeding.
///
/// The one question in this file a read plan cannot ask, and the reason [`DrsgClient::cypher`]
/// exists: a plan expands one hop per step, and what a loop back needs is every hop. The bound is
/// the graph's own node ceiling, since a path that visits no node twice cannot be longer, and the
/// store refuses an unbounded `*` anyway.
///
/// **Nothing is interpolated but the two labels and the bound**, none of which a caller can reach:
/// the node's key and the owner travel as parameters, which is the injection-safe way to pass a
/// value. The start is found by its external key, which the store seeks rather than scans.
///
/// The far end is filtered on the graph and the owner, so a name belonging to another graph cannot
/// come back even from a plane somebody else has edited. The hops *between* are not filtered,
/// because a plan cannot filter them and because no write here makes an edge that leaves one
/// graph: both ends of every edge are written by the same call.
fn reaching() -> String {
    format!(
        "MATCH (a:{NODE_LABEL})-[:{FEEDS}*1..{bound}]->(b:{NODE_LABEL}) \
         WHERE key(a) = $key AND b.{GRAPH} = $graph AND b.{OWNER} = $owner \
         RETURN DISTINCT b.{NAME} AS {NAME}",
        bound = MaxNodes::MOST
    )
}

/// The plan that reads what one owner's run is running.
fn placements_plan(owner: &GraphOwner, run: &GraphRunId) -> Value {
    json!({
        "source": {"ScanLabel": PLACEMENT_LABEL},
        "steps": [{"Filter": {
            "Logic": {
                "op": "And",
                "lhs": eq(RUN, &run.to_hex()),
                "rhs": eq(OWNER, owner.written()),
            }
        }}],
    })
}

/// The plan that reads every member of the cluster.
///
/// No filter: a member belongs to the deployment rather than to a tenant, so there is no owner to
/// narrow by and nothing narrower that would be true.
fn members_plan() -> Value {
    json!({"source": {"ScanLabel": MEMBER_LABEL}, "steps": []})
}

/// What a member's record of itself carries.
fn member_properties(member: &ClusterMember) -> Value {
    json!({
        ADDRESS: member.address.to_string(),
        AT: member.at.unix_seconds(),
    })
}

/// One member, rebuilt from the row the store answered with.
fn member_from(row: &Value) -> Result<ClusterMember, GraphError> {
    let properties = &row["properties"];
    let at = properties[AT]
        .as_i64()
        .ok_or(GraphError::PropertyKind { property: AT })?;
    Ok(ClusterMember {
        address: properties[ADDRESS]
            .as_str()
            .ok_or(GraphError::PropertyMissing { property: ADDRESS })?
            .parse()
            .map_err(|_| GraphError::PropertyKind { property: ADDRESS })?,
        at: Timestamp::from_unix_seconds(at)
            .map_err(|_| GraphError::PropertyKind { property: AT })?,
    })
}

/// What a record of a running node carries.
fn placement_properties(placement: &Placement) -> Value {
    json!({
        RUN: placement.run.to_hex(),
        NODE: placement.node.as_str(),
        GRAPH: placement.graph.as_str(),
        OWNER: placement.owner.written(),
        ADDRESS: placement.address.to_string(),
        AT: placement.at.unix_seconds(),
    })
}

/// One record of a running node, rebuilt from the row the store answered with.
fn placement_from(row: &Value, owner: &GraphOwner) -> Result<Placement, GraphError> {
    let properties = &row["properties"];
    let said = |property: &'static str| -> Result<&str, GraphError> {
        properties[property]
            .as_str()
            .ok_or(GraphError::PropertyMissing { property })
    };
    let at = properties[AT]
        .as_i64()
        .ok_or(GraphError::PropertyKind { property: AT })?;
    let placed = Placement {
        run: GraphRunId::from_hex(said(RUN)?)?,
        node: NodeName::new(said(NODE)?)?,
        graph: GraphId::new(said(GRAPH)?)?,
        owner: owner.clone(),
        address: said(ADDRESS)?
            .parse()
            .map_err(|_| GraphError::PropertyKind { property: ADDRESS })?,
        at: Timestamp::from_unix_seconds(at)
            .map_err(|_| GraphError::PropertyKind { property: AT })?,
    };
    // The owner is the caller's, so a row belonging to another is a read that leaked.
    if properties[OWNER].as_str() != Some(owner.written()) {
        return Err(GraphError::Foreign { property: OWNER });
    }
    Ok(placed)
}

/// What one loop back carries: whose it is, and how often it may be taken.
///
/// The names it joins are the nodes it joins, as with [`edge_properties`]; the cap is the one
/// thing about it that is not in its ends.
fn loop_properties(graph: &RunGraph, back: &LoopBack) -> Value {
    json!({
        GRAPH: graph.id().as_str(),
        OWNER: graph.owner().written(),
        CAP: back.cap.get(),
    })
}

/// One graph's loop backs, rebuilt from the table the store answered with.
fn loops_from(table: &Value) -> Result<Vec<LoopBack>, GraphError> {
    let rows = table["rows"]
        .as_array()
        .ok_or_else(|| GraphError::Unreadable {
            detail: "a loop back read answered with something that is not a table".to_owned(),
        })?;
    rows.iter()
        .map(|row| {
            let cells = row
                .as_array()
                .filter(|cells| cells.len() == 3)
                .ok_or_else(|| GraphError::Unreadable {
                    detail: "a loop back row is not the two names and the cap it was asked for"
                        .to_owned(),
                })?;
            let name = |cell: &Value| {
                cell.as_str()
                    .ok_or(GraphError::PropertyKind { property: NAME })
                    .and_then(NodeName::new)
            };
            let stored = cells[2]
                .as_u64()
                .and_then(|cap| u32::try_from(cap).ok())
                .ok_or(GraphError::PropertyKind { property: CAP })?;
            Ok(LoopBack {
                from: name(&cells[0])?,
                to: name(&cells[1])?,
                cap: LoopCap::new(stored)?,
            })
        })
        .collect()
}

/// What one branch carries: whose it is, and when it is worth taking.
fn branch_properties(graph: &RunGraph, condition: &Condition) -> Value {
    json!({
        GRAPH: graph.id().as_str(),
        OWNER: graph.owner().written(),
        CONDITION: condition.as_str(),
    })
}

/// One graph's branches, rebuilt from the table the store answered with.
///
/// Each comes back as the edge it is rather than as a pair and a condition, so what reattaches it
/// to the graph's own edges is the two names it joins.
fn branches_from(table: &Value) -> Result<Vec<GraphEdge>, GraphError> {
    let rows = table["rows"]
        .as_array()
        .ok_or_else(|| GraphError::Unreadable {
            detail: "a branch read answered with something that is not a table".to_owned(),
        })?;
    rows.iter()
        .map(|row| {
            let cells = row
                .as_array()
                .filter(|cells| cells.len() == 3)
                .ok_or_else(|| GraphError::Unreadable {
                    detail: "a branch row is not the two names and the condition it was asked for"
                        .to_owned(),
                })?;
            let name = |cell: &Value| {
                cell.as_str()
                    .ok_or(GraphError::PropertyKind { property: NAME })
                    .and_then(NodeName::new)
            };
            let said = cells[2].as_str().ok_or(GraphError::PropertyKind {
                property: CONDITION,
            })?;
            Ok(GraphEdge {
                from: name(&cells[0])?,
                to: name(&cells[1])?,
                condition: Some(Condition::new(said)?),
            })
        })
        .collect()
}

/// A graph's edges with each branch's condition back on the edge it belongs to.
///
/// The two are read apart because an edge's property is a statement's to read, and what joins them
/// again is the pair of names. A branch naming a pair no edge feeds is **refused rather than
/// dropped**: a condition that went missing turns a branch into an edge that always delivers, which
/// is a graph that runs differently rather than one that fails to read.
fn with_conditions(
    mut edges: Vec<GraphEdge>,
    branches: Vec<GraphEdge>,
) -> Result<Vec<GraphEdge>, GraphError> {
    for branch in branches {
        let fed = edges
            .iter_mut()
            .find(|edge| edge.from == branch.from && edge.to == branch.to)
            .ok_or_else(|| GraphError::Unreadable {
                detail: format!(
                    "a branch from {} to {}, which no edge feeds",
                    branch.from, branch.to
                ),
            })?;
        fed.condition = branch.condition;
    }
    Ok(edges)
}

/// What a graph's own node carries.
fn graph_properties(graph: &RunGraph) -> Value {
    json!({
        GRAPH: graph.id().as_str(),
        OWNER: graph.owner().written(),
        NAME: graph.name().as_str(),
        MAX_NODES: graph.max_nodes().get(),
    })
}

/// What one node of a graph carries.
fn node_properties(graph: &RunGraph, node: &GraphNode) -> Value {
    json!({
        GRAPH: graph.id().as_str(),
        OWNER: graph.owner().written(),
        NAME: node.name.as_str(),
        AGENT: node.agent.as_str(),
    })
}

/// What one edge carries, which is only whose it is: the names it joins are the nodes it joins.
fn edge_properties(graph: &RunGraph) -> Value {
    json!({
        GRAPH: graph.id().as_str(),
        OWNER: graph.owner().written(),
    })
}

/// How many cycles a write asks about.
///
/// One is enough to refuse the write, and the node it names is enough to say where. A graph of 256
/// nodes could hold many, and reporting the rest would be a longer answer to the same question.
const CYCLES_ASKED: u64 = 1;

/// One row's text property, or why it is not one.
fn text<'row>(row: &'row Value, property: &'static str) -> Result<&'row str, GraphError> {
    match &row["properties"][property] {
        Value::Null => Err(GraphError::PropertyMissing { property }),
        value => value.as_str().ok_or(GraphError::PropertyKind { property }),
    }
}

/// The key a store gave a row, which is what addresses it again.
fn external_key(row: &Value) -> Result<&str, GraphError> {
    row["external_key"]
        .as_str()
        .ok_or(GraphError::PropertyMissing {
            property: "external_key",
        })
}

/// Refuses a row that is not the one that was asked for.
///
/// The filter in the plan is what keeps one owner's graphs out of another's read, and this is the
/// second lock on the same door: a plan that lost its owner clause fails the read here instead of
/// answering with somebody else's rows. It is not a substitute for the filter — a store would have
/// handed the rows over by the time this runs — it is what makes losing the filter loud.
fn ours(row: &Value, owner: &GraphOwner, id: &GraphId) -> Result<(), GraphError> {
    for (property, asked) in [(GRAPH, id.as_str()), (OWNER, owner.written())] {
        if text(row, property)? != asked {
            return Err(GraphError::Foreign { property });
        }
    }
    Ok(())
}

/// The rows of a node read, as the nodes of a graph.
fn nodes_from(
    rows: &Value,
    owner: &GraphOwner,
    id: &GraphId,
) -> Result<Vec<GraphNode>, GraphError> {
    let rows = rows.as_array().ok_or_else(|| GraphError::Unreadable {
        detail: "a node read answered with something that is not rows".to_owned(),
    })?;
    rows.iter()
        .map(|row| {
            ours(row, owner, id)?;
            Ok(GraphNode {
                name: NodeName::new(text(row, NAME)?)?,
                agent: AgentId::new(text(row, AGENT)?).map_err(|error| GraphError::Unreadable {
                    detail: format!("a stored node's template id is not one: {error}"),
                })?,
            })
        })
        .collect()
}

/// The table of an edge read, as the edges of a graph.
fn edges_from(table: &Value) -> Result<Vec<GraphEdge>, GraphError> {
    let rows = table["rows"]
        .as_array()
        .ok_or_else(|| GraphError::Unreadable {
            detail: "an edge read answered with something that is not a table".to_owned(),
        })?;
    rows.iter()
        .map(|row| {
            let pair = row
                .as_array()
                .filter(|cells| cells.len() == 2)
                .ok_or_else(|| GraphError::Unreadable {
                    detail: "an edge row is not the two names it was asked for".to_owned(),
                })?;
            let name = |cell: &Value| {
                cell.as_str()
                    .ok_or(GraphError::PropertyKind { property: NAME })
                    .and_then(NodeName::new)
            };
            Ok(GraphEdge {
                from: name(&pair[0])?,
                to: name(&pair[1])?,
                // The conditions are a read of their own, for the reason the caps are: an edge's
                // property is a statement's to read and this plan cannot project one.
                condition: None,
            })
        })
        .collect()
}

/// The table of a listing, as what each graph says about itself.
fn summaries_from(table: &Value, owner: &GraphOwner) -> Result<Vec<GraphSummary>, GraphError> {
    let rows = table["rows"]
        .as_array()
        .ok_or_else(|| GraphError::Unreadable {
            detail: "a listing answered with something that is not a table".to_owned(),
        })?;
    rows.iter()
        .map(|row| {
            let cells = row
                .as_array()
                .filter(|cells| cells.len() == 4)
                .ok_or_else(|| GraphError::Unreadable {
                    detail: "a listed row is not the four columns it was asked for".to_owned(),
                })?;
            fn said<'cell>(
                cell: &'cell Value,
                property: &'static str,
            ) -> Result<&'cell str, GraphError> {
                cell.as_str().ok_or(GraphError::PropertyKind { property })
            }
            if said(&cells[3], OWNER)? != owner.written() {
                return Err(GraphError::Foreign { property: OWNER });
            }
            let ceiling = cells[2].as_u64().ok_or(GraphError::PropertyKind {
                property: MAX_NODES,
            })?;
            Ok(GraphSummary {
                id: GraphId::new(said(&cells[0], GRAPH)?)?,
                name: GraphName::new(said(&cells[1], NAME)?)?,
                max_nodes: MaxNodes::new(u32::try_from(ceiling).map_err(|_| {
                    GraphError::PropertyKind {
                        property: MAX_NODES,
                    }
                })?)?,
            })
        })
        .collect()
}

/// Where one deployment's run graphs are kept.
///
/// One client, bound to one plane, so no caller can name another. The plane is made on first write
/// rather than at startup, so a store that comes up after this process is not a startup failure.
pub struct GraphPlane {
    drsg: DrsgClient,
}

impl GraphPlane {
    /// A store over a client already bound to the plane the graphs live in.
    pub fn new(drsg: DrsgClient) -> Self {
        Self { drsg }
    }

    /// Writes a graph, replacing whatever that owner had under the same id.
    ///
    /// Delete what the graph had, then write what it has now. A partial update would be a second
    /// consistency problem for no gain, since a graph is a handful of rows; the cost is that a
    /// write that fails partway leaves the graph as neither version, which is why the management
    /// route reports it rather than retrying.
    ///
    /// Deleting a node takes its edges with it, so the `FEEDS` edges need no pass of their own.
    ///
    /// An edge that carries a condition is written twice, its [`FEEDS`] row and then its
    /// [`BRANCH_FEEDS`] twin, for the reason that const records.
    pub async fn put(&self, graph: &RunGraph) -> Result<(), GraphError> {
        let (owner, id) = (graph.owner(), graph.id());
        self.clear(owner, id).await?;

        self.drsg
            .node_create(
                &graph_key(owner, id),
                &[GRAPH_LABEL],
                graph_properties(graph),
            )
            .await?;
        let scope = scope_label(owner, id);
        let mut written = Vec::with_capacity(graph.nodes().len());
        for node in graph.nodes() {
            let stored = self
                .drsg
                .node_create(
                    &node_key(owner, id, &node.name),
                    &[NODE_LABEL, &scope],
                    node_properties(graph, node),
                )
                .await?;
            written.push((stored["id"].as_u64(), node.name.clone()));
        }
        for edge in graph.edges() {
            self.drsg
                .edge_create(
                    &node_key(owner, id, &edge.from),
                    &node_key(owner, id, &edge.to),
                    FEEDS,
                    edge_properties(graph),
                )
                .await?;
            if let Some(condition) = edge.condition.as_ref() {
                self.drsg
                    .edge_create(
                        &node_key(owner, id, &edge.from),
                        &node_key(owner, id, &edge.to),
                        BRANCH_FEEDS,
                        branch_properties(graph, condition),
                    )
                    .await?;
            }
        }
        for back in graph.loops() {
            self.drsg
                .edge_create(
                    &node_key(owner, id, &back.from),
                    &node_key(owner, id, &back.to),
                    LOOPS_BACK,
                    loop_properties(graph, back),
                )
                .await?;
        }
        if let Some(cycle) = self.cycle(&scope, &written).await? {
            self.clear(owner, id).await?;
            return Err(GraphError::Cyclic { cycle });
        }
        if let Some((from, to)) = self.forwards(&scope, &written, graph.loops()).await? {
            self.clear(owner, id).await?;
            return Err(GraphError::LoopNotBackwards { from, to });
        }
        Ok(())
    }

    /// The first loop back whose asking node does not descend from its target, where there is one.
    ///
    /// Asked of the store, one path search per declared loop back, so a graph that doubles back
    /// *sideways* or *forwards* is refused when it is written rather than when it is run: a node
    /// sending work to somewhere its own answer never came from would be re-running a node whose
    /// output it had not read.
    ///
    /// After the cycle check rather than before it, because a cyclic graph has no ancestry worth
    /// reporting on and `Cyclic` is the more fundamental answer. A node's loop back **on itself**
    /// is not asked about at all: there is no feeding path from a node to itself in a graph that
    /// passed the cycle check, and that loop is permitted.
    async fn forwards(
        &self,
        scope: &str,
        written: &[(Option<u64>, NodeName)],
        loops: &[LoopBack],
    ) -> Result<Option<(NodeName, NodeName)>, GraphError> {
        let stored = |name: &NodeName| {
            written
                .iter()
                .find(|(_, written)| written == name)
                .and_then(|(id, _)| *id)
                .ok_or_else(|| GraphError::Unreadable {
                    detail: "a node the store took and then gave no id for".to_owned(),
                })
        };
        for back in loops {
            if back.from == back.to {
                continue;
            }
            let answered = self
                .drsg
                .algo(
                    "shortest_path",
                    scope,
                    FEEDS,
                    json!({
                        "src": stored(&back.to)?,
                        "dst": stored(&back.from)?,
                        "dir": "out",
                    }),
                )
                .await?;
            let found = answered["found"]
                .as_bool()
                .ok_or_else(|| GraphError::Unreadable {
                    detail: "a path search that did not say whether it found one".to_owned(),
                })?;
            if !found {
                return Ok(Some((back.from.clone(), back.to.clone())));
            }
        }
        Ok(None)
    }

    /// Every node one node's answer reaches by feeding, the node itself among them.
    ///
    /// What a run asks when a loop back is taken: these are the nodes whose work the target's
    /// second pass invalidates, and so the ones that run again. Itself is included because the
    /// target is the node being re-run, which saves every caller remembering to add it.
    ///
    /// One read, however deep the graph, because the statement walks the hops rather than this
    /// code walking them a call at a time — see [`reaching`].
    pub async fn downstream(
        &self,
        owner: &GraphOwner,
        id: &GraphId,
        node: &NodeName,
    ) -> Result<BTreeSet<NodeName>, GraphError> {
        let answered = self
            .drsg
            .cypher(
                &reaching(),
                json!({
                    "key": node_key(owner, id, node),
                    "graph": id.as_str(),
                    "owner": owner.written(),
                }),
            )
            .await?;
        let rows = answered["rows"]
            .as_array()
            .ok_or_else(|| GraphError::Unreadable {
                detail: "a walk of the feeding edges answered with something that is not a table"
                    .to_owned(),
            })?;
        let mut reached = BTreeSet::new();
        reached.insert(node.clone());
        for row in rows {
            let named = row
                .as_array()
                .and_then(|cells| cells.first())
                .and_then(Value::as_str)
                .ok_or(GraphError::PropertyKind { property: NAME })?;
            reached.insert(NodeName::new(named)?);
        }
        Ok(reached)
    }

    /// Records that one node of one run is running, and where.
    ///
    /// Written again over whatever was there under the same key, so a dispatch retried after a
    /// worker was lost replaces the record rather than being refused one: the store binds a key
    /// once, and a retry is the case this is most needed in.
    pub async fn placed(&self, placement: &Placement) -> Result<(), GraphError> {
        let key = placement_key(&placement.run, &placement.node);
        self.drsg.node_delete(&key).await?;
        self.drsg
            .node_create(&key, &[PLACEMENT_LABEL], placement_properties(placement))
            .await?;
        Ok(())
    }

    /// Takes away the record of a node that has finished, wherever it ran.
    ///
    /// Answers nothing about whether there was one: a node that was never recorded, because the
    /// write was best effort and failed, must not make its own ending an error.
    pub async fn finished(&self, run: &GraphRunId, node: &NodeName) -> Result<(), GraphError> {
        self.drsg
            .node_delete(&placement_key(run, node))
            .await
            .map(|_| ())
            .map_err(GraphError::from)
    }

    /// What one owner's run is running, and where each of it is.
    ///
    /// Filtered on the owner in the plan, as every read here is, and on the run beside it.
    pub async fn placements(
        &self,
        owner: &GraphOwner,
        run: &GraphRunId,
    ) -> Result<Vec<Placement>, GraphError> {
        let rows = self.drsg.query(placements_plan(owner, run)).await?;
        let rows = rows.as_array().ok_or_else(|| GraphError::Unreadable {
            detail: "a placement read answered with something that is not rows".to_owned(),
        })?;
        rows.iter()
            .map(|row| placement_from(row, owner))
            .collect::<Result<Vec<_>, _>>()
    }

    /// Records that this member is here, now.
    ///
    /// Rewrites rather than adding, as [`Self::placed`] does and for the same reason: a member
    /// saying so again on its next heartbeat is the same member, and `node_create` refuses a key
    /// that is already bound.
    pub async fn joined(&self, member: &ClusterMember) -> Result<(), GraphError> {
        let key = member_key(&member.address);
        self.drsg.node_delete(&key).await?;
        self.drsg
            .node_create(&key, &[MEMBER_LABEL], member_properties(member))
            .await?;
        Ok(())
    }

    /// Takes away the record of a member that is going.
    ///
    /// Answers nothing about whether there was one, for the reason [`Self::finished`] does not: a
    /// registration that was best effort and failed must not make its own machine's shutdown an
    /// error.
    pub async fn left(&self, address: &ClusterAddress) -> Result<(), GraphError> {
        self.drsg
            .node_delete(&member_key(address))
            .await
            .map(|_| ())
            .map_err(GraphError::from)
    }

    /// Every member of the cluster, the one that wrote most recently first.
    ///
    /// Ordered here rather than in the store, because a scan answers in whatever order it holds
    /// its rows, and a reader choosing between members should not choose differently each call.
    ///
    /// Liveness is not applied: how old a row may be before it is ignored belongs to the reader,
    /// and only a reader that knows the heartbeat interval can say.
    pub async fn members(&self) -> Result<Vec<ClusterMember>, GraphError> {
        let rows = self.drsg.query(members_plan()).await?;
        let rows = rows.as_array().ok_or_else(|| GraphError::Unreadable {
            detail: "a member read answered with something that is not rows".to_owned(),
        })?;
        let mut members = rows
            .iter()
            .map(member_from)
            .collect::<Result<Vec<_>, _>>()?;
        members.sort_by_key(|member| std::cmp::Reverse(member.at.unix_seconds()));
        Ok(members)
    }

    /// The order one owner's graph runs in, as the store's own topological sort answers it.
    ///
    /// Scoped to the graph's own label, so one deployment's graphs are sorted one at a time: over
    /// the label every graph shares, a cycle in any of them would be an error on all of their
    /// reads, since a cyclic scope has no order to return.
    pub async fn order(&self, owner: &GraphOwner, id: &GraphId) -> Result<RunOrder, GraphError> {
        let scope = scope_label(owner, id);
        let answered = self.sorted(&scope).await?;
        let placed = answered
            .iter()
            .map(|row| {
                let id = row["id"].as_u64().ok_or_else(|| GraphError::Unreadable {
                    detail: "an order placing a node it did not name".to_owned(),
                })?;
                let generation = row["generation"]
                    .as_u64()
                    .and_then(|value| u32::try_from(value).ok())
                    .ok_or_else(|| GraphError::Unreadable {
                        detail: "an order with no generation to run a node in".to_owned(),
                    })?;
                Ok((id, Generation::new(generation)))
            })
            .collect::<Result<Vec<_>, GraphError>>()?;
        self.named(owner, id, placed).await
    }

    /// The rows the store's topological sort answered with, all of them.
    ///
    /// `dir` is `out` because this store counts a node's *incoming* edges to find the roots, so
    /// over `parent FEEDS child` the parents come back first and a generation is a set with no
    /// edge inside it.
    ///
    /// The count is checked against the rows: the store truncates its answer to `limit` while
    /// reporting the true total, and a truncated order reads exactly like a complete one.
    async fn sorted(&self, scope: &str) -> Result<Vec<Value>, GraphError> {
        let answered = self
            .drsg
            .algo(
                "toposort",
                scope,
                FEEDS,
                json!({ "dir": "out", "limit": MaxNodes::MOST }),
            )
            .await?;
        let rows = answered["results"]
            .as_array()
            .ok_or_else(|| GraphError::Unreadable {
                detail: "a topological sort that ordered nothing".to_owned(),
            })?;
        let counted = answered["count"]
            .as_u64()
            .ok_or_else(|| GraphError::Unreadable {
                detail: "a topological sort that counted nothing".to_owned(),
            })?;
        if counted != rows.len() as u64 {
            return Err(GraphError::Truncated {
                held: counted,
                answered: rows.len(),
            });
        }
        Ok(rows.clone())
    }

    /// An order over node ids, as an order over the names a graph calls them by.
    ///
    /// The sort answers in the store's own ids, which mean nothing to a caller and nothing to the
    /// graph, so the nodes are read to name them.
    async fn named(
        &self,
        owner: &GraphOwner,
        id: &GraphId,
        placed: Vec<(u64, Generation)>,
    ) -> Result<RunOrder, GraphError> {
        let rows = self.drsg.query(nodes_plan(owner, id)).await?;
        let rows = rows.as_array().ok_or_else(|| GraphError::Unreadable {
            detail: "a node read answered with something that is not rows".to_owned(),
        })?;
        let mut named = Vec::with_capacity(placed.len());
        for (stored, generation) in placed {
            let row = rows
                .iter()
                .find(|row| row["id"].as_u64() == Some(stored))
                .ok_or(GraphError::Unplaced)?;
            ours(row, owner, id)?;
            named.push(Placed {
                name: NodeName::new(text(row, NAME)?)?,
                generation,
            });
        }
        RunOrder::checked(named)
    }

    /// The first cycle the store finds among a graph's nodes, named, or nothing where it is a DAG.
    ///
    /// Asked of the store rather than answered here, for the reason [`RunGraph::checked`] does not
    /// look for one: a cycle witness and a topological order are what a graph database is for, and
    /// a second implementation in a consumer is one that can disagree with the order a run is
    /// driven by.
    async fn cycle(
        &self,
        scope: &str,
        written: &[(Option<u64>, NodeName)],
    ) -> Result<Option<String>, GraphError> {
        let answered = self
            .drsg
            .algo("cycles", scope, FEEDS, json!({ "limit": CYCLES_ASKED }))
            .await?;
        let found = answered["count"]
            .as_u64()
            .ok_or_else(|| GraphError::Unreadable {
                detail: "a cycle check that counted nothing, not even none".to_owned(),
            })?;
        if found == 0 {
            return Ok(None);
        }
        let name = |stored: &Value| match stored.as_u64() {
            Some(stored) => written
                .iter()
                .find(|(written, _)| *written == Some(stored))
                .map_or_else(|| stored.to_string(), |(_, name)| name.to_string()),
            None => "?".to_owned(),
        };
        Ok(Some(
            answered["results"][0]["cycle"].as_array().map_or_else(
                || "nodes the store did not name".to_owned(),
                |cycle| cycle.iter().map(name).collect::<Vec<_>>().join(" → "),
            ),
        ))
    }

    /// Takes a graph out of the plane, rows and edges alike.
    ///
    /// Both halves of a write start here: what a write replaces, and what a refused write leaves
    /// behind. A cycle can only be looked for once the rows exist, so a cyclic graph is written
    /// and then removed, which leaves it **absent rather than as it was** — the delete-then-
    /// rewrite decision showing through, and the reason a refused write is reported and never
    /// retried.
    async fn clear(&self, owner: &GraphOwner, id: &GraphId) -> Result<(), GraphError> {
        for key in self.held(owner, id).await? {
            self.drsg.node_delete(&key).await?;
        }
        self.drsg.node_delete(&graph_key(owner, id)).await?;
        Ok(())
    }

    /// One graph's loop backs, without reading the rest of it.
    ///
    /// A read of its own because a member running one node of somebody else's graph needs this and
    /// nothing else of the graph: it is one statement where [`get`](Self::get) is five reads, and
    /// what it answers is what says whether that node may send its work back.
    ///
    /// Nothing distinguishes a graph that is not there from one that declares no loop back. Both
    /// answer with nothing, and a caller that needs to know which has [`get`](Self::get).
    pub async fn loops(
        &self,
        owner: &GraphOwner,
        id: &GraphId,
    ) -> Result<Vec<LoopBack>, GraphError> {
        loops_from(
            &self
                .drsg
                .cypher(
                    &stored_loops(),
                    json!({"graph": id.as_str(), "owner": owner.written()}),
                )
                .await?,
        )
    }

    /// One graph's branches, without reading the rest of it.
    ///
    /// A read of its own for [`loops`](Self::loops)' reason: a member running one node of somebody
    /// else's graph needs the branches that leave that node and nothing else of the graph, and what
    /// they answer is what that node may choose between.
    ///
    /// Each answers as the edge it is, condition and all, so a caller reads a branch's ends the way
    /// it reads any other edge's.
    pub async fn branches(
        &self,
        owner: &GraphOwner,
        id: &GraphId,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        branches_from(
            &self
                .drsg
                .cypher(
                    &stored_branches(),
                    json!({"graph": id.as_str(), "owner": owner.written()}),
                )
                .await?,
        )
    }

    /// One owner's graph, or nothing where that owner has none under the id.
    ///
    /// Five reads — the graph, its nodes, its edges, its loop backs, its branches — each filtered
    /// on the owner, and the answer goes through [`RunGraph::checked`]: a graph that comes back from
    /// the plane is checked exactly as one arriving from a caller is, because rows can be edited in
    /// the store by something that is not this code.
    ///
    /// The last two are statements rather than plans, for the reason [`stored_loops`] records.
    pub async fn get(
        &self,
        owner: &GraphOwner,
        id: &GraphId,
    ) -> Result<Option<RunGraph>, GraphError> {
        let answered = self.drsg.query(graph_plan(owner, id)).await?;
        let rows = answered.as_array().ok_or_else(|| GraphError::Unreadable {
            detail: "a graph read answered with something that is not rows".to_owned(),
        })?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        ours(row, owner, id)?;
        let name = GraphName::new(text(row, NAME)?)?;
        let stored =
            row["properties"][MAX_NODES]
                .as_u64()
                .ok_or(match row["properties"][MAX_NODES] {
                    Value::Null => GraphError::PropertyMissing {
                        property: MAX_NODES,
                    },
                    _ => GraphError::PropertyKind {
                        property: MAX_NODES,
                    },
                })?;
        let max_nodes =
            MaxNodes::new(u32::try_from(stored).map_err(|_| GraphError::PropertyKind {
                property: MAX_NODES,
            })?)?;
        let nodes = nodes_from(&self.drsg.query(nodes_plan(owner, id)).await?, owner, id)?;
        let edges = edges_from(&self.drsg.query(edges_plan(owner, id)).await?)?;
        let loops = self.loops(owner, id).await?;
        let edges = with_conditions(edges, self.branches(owner, id).await?)?;
        RunGraph::checked(RunGraphParts {
            id: id.clone(),
            name,
            owner: owner.clone(),
            nodes,
            edges,
            loops,
            max_nodes,
        })
        .map(Some)
    }

    /// One owner's graphs, by id, as each says about itself.
    ///
    /// One read however many graphs it answers with, and the rows are checked against the owner
    /// that asked for the same reason [`ours`] checks a single read's: the filter in the plan is
    /// the lock, and this is the second one on the same door.
    pub async fn graphs(
        &self,
        owner: &GraphOwner,
        listing: Listing,
    ) -> Result<Vec<GraphSummary>, GraphError> {
        let answered = self.drsg.query(graphs_plan(owner, listing)).await?;
        summaries_from(&answered, owner)
    }

    /// Takes one owner's graph out of the plane, saying whether it held one.
    ///
    /// Presence is the graph's own row and not whether the whole graph can be rebuilt: a graph
    /// whose stored rows no longer make a valid graph is exactly one an operator must be able to
    /// delete, so this does not go through [`GraphPlane::get`].
    pub async fn remove(&self, owner: &GraphOwner, id: &GraphId) -> Result<bool, GraphError> {
        let answered = self.drsg.query(graph_plan(owner, id)).await?;
        let rows = answered.as_array().ok_or_else(|| GraphError::Unreadable {
            detail: "a graph read answered with something that is not rows".to_owned(),
        })?;
        let Some(row) = rows.first() else {
            return Ok(false);
        };
        ours(row, owner, id)?;
        self.clear(owner, id).await?;
        Ok(true)
    }

    /// The keys of the nodes this owner's graph holds now, which is what a write deletes first.
    async fn held(&self, owner: &GraphOwner, id: &GraphId) -> Result<Vec<String>, GraphError> {
        let answered = self.drsg.query(nodes_plan(owner, id)).await?;
        let rows = answered.as_array().ok_or_else(|| GraphError::Unreadable {
            detail: "a node read answered with something that is not rows".to_owned(),
        })?;
        rows.iter()
            .map(|row| external_key(row).map(str::to_owned))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use ip_core::TenantId;
    use ip_drsg::{Replies, StubPlane};

    use super::*;
    use ip_core::route::ClusterAddress;

    fn tenant() -> GraphOwner {
        GraphOwner::Tenant(TenantId::new("acme").expect("a tenant id"))
    }

    fn id() -> GraphId {
        GraphId::new("triage").expect("a graph id")
    }

    fn name(raw: &str) -> NodeName {
        NodeName::new(raw).expect("a name a node may carry")
    }

    fn node(raw: &str) -> GraphNode {
        GraphNode {
            name: name(raw),
            agent: AgentId::new(raw).expect("a template id"),
        }
    }

    /// A chain of two, which is the smallest graph with an edge in it.
    fn graph() -> RunGraph {
        RunGraph::checked(RunGraphParts {
            id: id(),
            name: GraphName::new("Support triage").expect("a graph name"),
            owner: tenant(),
            loops: Vec::new(),
            nodes: vec![node("intake"), node("reply")],
            edges: vec![GraphEdge {
                from: name("intake"),
                to: name("reply"),
                condition: None,
            }],
            max_nodes: MaxNodes::new(8).expect("a ceiling"),
        })
        .expect("the fixture's own graph holds together")
    }

    /// The rows a plane answers a node read with, built from what a write would have sent.
    fn node_rows(graph: &RunGraph) -> Value {
        Value::Array(
            graph
                .nodes()
                .iter()
                .enumerate()
                .map(|(index, node)| {
                    json!({
                        "id": index + 1,
                        "external_key": node_key(graph.owner(), graph.id(), &node.name),
                        "labels": [NODE_LABEL, scope_label(graph.owner(), graph.id())],
                        "properties": node_properties(graph, node),
                    })
                })
                .collect(),
        )
    }

    /// The row a plane answers a graph read with.
    fn graph_rows(graph: &RunGraph) -> Value {
        json!([{
            "id": 0,
            "external_key": graph_key(graph.owner(), graph.id()),
            "labels": [GRAPH_LABEL],
            "properties": graph_properties(graph),
        }])
    }

    /// The table a plane answers an edge read with.
    fn edge_table(graph: &RunGraph) -> Value {
        json!({
            "columns": ["from", "to"],
            "rows": graph
                .edges()
                .iter()
                .map(|edge| json!([edge.from.as_str(), edge.to.as_str()]))
                .collect::<Vec<_>>(),
        })
    }

    /// The table a plane answers a loop back read with, empty for a graph that declares none.
    fn loop_table(graph: &RunGraph) -> Value {
        json!({
            "columns": ["from", "to", CAP],
            "rows": graph
                .loops()
                .iter()
                .map(|back| json!([back.from.as_str(), back.to.as_str(), back.cap.get()]))
                .collect::<Vec<_>>(),
        })
    }

    /// The table a plane answers a branch read with, empty for a graph that declares none.
    fn branch_table(graph: &RunGraph) -> Value {
        json!({
            "columns": ["from", "to", CONDITION],
            "rows": graph
                .branches()
                .map(|(edge, when)| json!([edge.from.as_str(), edge.to.as_str(), when.as_str()]))
                .collect::<Vec<_>>(),
        })
    }

    /// The fixture's chain of two, with the second node able to send the work back.
    fn looping_graph() -> RunGraph {
        let stored = graph();
        RunGraph::checked(RunGraphParts {
            id: stored.id().clone(),
            name: stored.name().clone(),
            owner: stored.owner().clone(),
            nodes: stored.nodes().to_vec(),
            edges: stored.edges().to_vec(),
            loops: vec![LoopBack {
                from: name("reply"),
                to: name("intake"),
                cap: LoopCap::new(2).expect("a cap"),
            }],
            max_nodes: stored.max_nodes(),
        })
        .expect("a chain that may double back holds together")
    }

    /// The fixture's chain of two, where whether the second node runs is the first node's to say.
    fn branching_graph() -> RunGraph {
        let stored = graph();
        RunGraph::checked(RunGraphParts {
            id: stored.id().clone(),
            name: stored.name().clone(),
            owner: stored.owner().clone(),
            nodes: stored.nodes().to_vec(),
            edges: vec![GraphEdge {
                from: name("intake"),
                to: name("reply"),
                condition: Some(
                    Condition::new("the complaint needs an answer").expect("a condition"),
                ),
            }],
            loops: Vec::new(),
            max_nodes: stored.max_nodes(),
        })
        .expect("a chain whose edge carries a condition holds together")
    }

    /// A plane holding nothing, which takes a write and finds no cycle in it.
    async fn empty_plane() -> StubPlane {
        StubPlane::start(Replies::scripted([
            ("plane.query", json!([])),
            ("plane.algo", json!({"count": 0, "results": []})),
        ]))
        .await
    }

    #[tokio::test]
    async fn a_listing_filters_on_the_owner_and_pages_in_the_plan_itself() {
        let stub = StubPlane::start(Replies::scripted([(
            "plane.query",
            json!({
                "columns": [GRAPH, NAME, MAX_NODES, OWNER],
                "rows": [["triage", "Support triage", 8, "acme"]],
            }),
        )]))
        .await;

        let listed = GraphPlane::new(stub.client())
            .graphs(
                &tenant(),
                Listing {
                    limit: 5,
                    offset: 10,
                },
            )
            .await
            .expect("a listing");

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id.as_str(), "triage");
        assert_eq!(listed[0].name.as_str(), "Support triage");
        assert_eq!(listed[0].max_nodes.get(), 8);
        let plan = &stub.only().params["plan"];
        assert_eq!(plan["source"]["ScanLabel"], GRAPH_LABEL);
        assert_eq!(plan["project"]["skip"], 10);
        assert_eq!(plan["project"]["limit"], 5);
        assert_eq!(
            plan["steps"][0]["Filter"]["Compare"]["rhs"]["Literal"]["Str"], "acme",
            "a listing did not filter on the owner: {plan}"
        );
    }

    /// The projection carries the owner so the rows can be checked the way a single read's are.
    #[tokio::test]
    async fn a_listed_row_of_another_owner_is_refused() {
        let stub = StubPlane::start(Replies::scripted([(
            "plane.query",
            json!({
                "columns": [GRAPH, NAME, MAX_NODES, OWNER],
                "rows": [["triage", "Support triage", 8, "other"]],
            }),
        )]))
        .await;

        assert_eq!(
            GraphPlane::new(stub.client())
                .graphs(
                    &tenant(),
                    Listing {
                        limit: 5,
                        offset: 0
                    }
                )
                .await,
            Err(GraphError::Foreign { property: OWNER })
        );
    }

    #[tokio::test]
    async fn a_listing_answering_with_anything_but_a_table_is_refused() {
        for answered in [
            json!([]),
            json!({"rows": [["triage"]]}),
            json!({"rows": "no"}),
        ] {
            let stub =
                StubPlane::start(Replies::scripted([("plane.query", answered.clone())])).await;
            assert!(
                GraphPlane::new(stub.client())
                    .graphs(
                        &tenant(),
                        Listing {
                            limit: 5,
                            offset: 0
                        }
                    )
                    .await
                    .is_err(),
                "a listing took {answered}"
            );
        }
    }

    #[tokio::test]
    async fn removing_a_graph_takes_its_own_row_and_every_node_with_it() {
        let graph = graph();
        let stub = StubPlane::start(Replies::in_turn([
            graph_rows(&graph),
            node_rows(&graph),
            json!({"deleted": true}),
            json!({"deleted": true}),
            json!({"deleted": true}),
        ]))
        .await;

        assert!(
            GraphPlane::new(stub.client())
                .remove(&tenant(), &id())
                .await
                .expect("the graph was there"),
        );
        let deleted: Vec<String> = stub
            .calls("node.delete")
            .into_iter()
            .map(|params| params["key"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert!(
            deleted.contains(&format!("{GRAPH_LABEL}/acme/triage")),
            "the graph's own row was left behind: {deleted:?}"
        );
        assert_eq!(deleted.len(), 3, "{deleted:?}");
    }

    #[tokio::test]
    async fn removing_a_graph_that_is_not_there_says_so_and_deletes_nothing() {
        let stub = StubPlane::start(Replies::scripted([("plane.query", json!([]))])).await;

        assert!(
            !GraphPlane::new(stub.client())
                .remove(&tenant(), &id())
                .await
                .expect("a read that found nothing is not a failure")
        );
        assert!(stub.calls("node.delete").is_empty());
    }

    /// A delete is the one place a foreign row must stop the call rather than be read past.
    #[tokio::test]
    async fn removing_a_graph_the_store_says_is_another_owners_is_refused() {
        let stub = StubPlane::start(Replies::scripted([(
            "plane.query",
            json!([{
                "id": 1,
                "external_key": format!("{GRAPH_LABEL}/other/triage"),
                "labels": [GRAPH_LABEL],
                "properties": {
                    GRAPH: "triage", OWNER: "other", NAME: "Support triage", MAX_NODES: 8,
                },
            }]),
        )]))
        .await;

        assert_eq!(
            GraphPlane::new(stub.client())
                .remove(&tenant(), &id())
                .await,
            Err(GraphError::Foreign { property: OWNER })
        );
        assert!(
            stub.calls("node.delete").is_empty(),
            "a row the store should never have answered with was deleted anyway"
        );
    }

    #[test]
    fn every_read_filters_on_the_owner_as_well_as_the_graph() {
        for plan in [
            graph_plan(&tenant(), &id()),
            nodes_plan(&tenant(), &id()),
            edges_plan(&tenant(), &id()),
        ] {
            let printed = plan.to_string();
            assert!(
                printed.contains(r#"{"Property":"owner"}"#),
                "a read went out unfiltered by owner: {printed}"
            );
            assert!(
                printed.contains(r#"{"Str":"acme"}"#),
                "a read did not name the owner it is for: {printed}"
            );
            assert!(
                printed.contains(r#"{"Str":"triage"}"#),
                "a read did not name the graph it is for: {printed}"
            );
        }
    }

    /// The filter is an `And` of both halves rather than either one, which is the shape that makes
    /// one tenant's id insufficient to read another tenant's graph of the same name.
    #[test]
    fn the_filter_is_the_graph_and_the_owner_together() {
        assert_eq!(
            owned_by(&tenant(), &id()),
            json!({
                "Logic": {
                    "op": "And",
                    "lhs": {"Compare": {
                        "op": "Eq",
                        "lhs": {"Property": "graph"},
                        "rhs": {"Literal": {"Str": "triage"}},
                    }},
                    "rhs": {"Compare": {
                        "op": "Eq",
                        "lhs": {"Property": "owner"},
                        "rhs": {"Literal": {"Str": "acme"}},
                    }},
                }
            })
        );
    }

    #[test]
    fn an_edge_read_asks_for_both_ends_of_one_feeds_hop() {
        let plan = edges_plan(&tenant(), &id());
        assert_eq!(plan["steps"][1]["Expand"]["edge_type"], FEEDS);
        assert_eq!(plan["steps"][1]["Expand"]["dir"], "Out");
        assert_eq!(
            plan["steps"][2]["Filter"],
            owned_by(&tenant(), &id()),
            "an edge was taken on its source alone"
        );
        assert_eq!(plan["project"]["items"][0]["name"], "from");
        assert_eq!(plan["project"]["items"][1]["name"], "to");
    }

    #[test]
    fn a_graph_and_its_nodes_are_addressed_by_keys_nothing_else_can_spell() {
        assert_eq!(graph_key(&tenant(), &id()), "RunGraph/acme/triage");
        assert_eq!(
            node_key(&tenant(), &id(), &name("intake")),
            "GraphNode/acme/triage/intake"
        );
        assert_eq!(scope_label(&tenant(), &id()), "Graph/acme/triage");
        assert_eq!(
            graph_key(&GraphOwner::Global, &id()),
            "RunGraph/global/triage",
            "the deployment's own graphs are addressed the same way"
        );
    }

    /// The round trip the whole mapping exists for, with no plane in it: what a write sends is fed
    /// back as what a read would answer, and the graph that comes out is the graph that went in.
    #[test]
    fn a_graph_rebuilt_from_what_was_written_is_the_graph_that_was_written() {
        let written = graph();
        assert_eq!(
            nodes_from(&node_rows(&written), &tenant(), &id())
                .expect("the rows a write sent are readable"),
            written.nodes()
        );
        assert_eq!(
            edges_from(&edge_table(&written)).expect("the table a read answers is readable"),
            written.edges()
        );
        let row = graph_rows(&written);
        assert_eq!(text(&row[0], NAME).expect("the name"), "Support triage");
        assert_eq!(row[0]["properties"][MAX_NODES], 8);
        assert_eq!(row[0]["properties"][OWNER], "acme");
    }

    #[tokio::test]
    async fn a_write_deletes_what_the_graph_had_before_it_writes_what_it_has() {
        let stub = empty_plane().await;
        GraphPlane::new(stub.client())
            .put(&graph())
            .await
            .expect("the stub takes the write");
        let seen: Vec<String> = stub.seen().into_iter().map(|one| one.method).collect();
        assert_eq!(
            seen,
            [
                "plane.query",
                "node.delete",
                "node.create",
                "node.create",
                "node.create",
                "edge.create",
                "plane.algo",
            ],
            "the write did not read, then delete, then write, then check"
        );
    }

    #[tokio::test]
    async fn a_write_asks_whether_the_graph_it_just_wrote_feeds_itself() {
        let stub = empty_plane().await;
        GraphPlane::new(stub.client())
            .put(&graph())
            .await
            .expect("a graph with no cycle in it is taken");
        let asked = stub.calls("plane.algo");
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0]["algo"], "cycles");
        assert_eq!(
            asked[0]["label"], "Graph/acme/triage",
            "the check ran over more than this graph"
        );
        assert_eq!(
            asked[0]["edge_type"], FEEDS,
            "the check followed every kind of edge, not just the ones that feed"
        );
        assert_eq!(asked[0]["limit"], 1);
    }

    /// A plane that takes a write of a graph with a loop back in it.
    ///
    /// One answer covers both algorithm calls a looping write makes — the cycle check reads
    /// `count`, the ancestry check reads `found` — because a script answers per method and a write
    /// asks `plane.algo` twice.
    async fn looping_plane() -> StubPlane {
        StubPlane::start(Replies::scripted([
            ("plane.query", json!([])),
            ("node.create", json!({"id": 7})),
            (
                "plane.algo",
                json!({"count": 0, "results": [], "found": true}),
            ),
        ]))
        .await
    }

    #[tokio::test]
    async fn a_loop_back_is_written_as_its_own_kind_of_edge_carrying_its_cap() {
        let stub = looping_plane().await;
        GraphPlane::new(stub.client())
            .put(&looping_graph())
            .await
            .expect("the stub takes the write");

        let edges = stub.calls("edge.create");
        assert_eq!(
            edges.len(),
            2,
            "a loop back is an edge beside the feeding one"
        );
        assert_eq!(edges[0]["type"], FEEDS);
        assert_eq!(edges[1]["type"], LOOPS_BACK);
        assert_eq!(
            edges[1]["src"], "GraphNode/acme/triage/reply",
            "the loop back left the wrong end"
        );
        assert_eq!(edges[1]["dst"], "GraphNode/acme/triage/intake");
        assert_eq!(edges[1]["properties"][CAP], 2);
        assert_eq!(edges[1]["properties"][OWNER], "acme");
    }

    /// The duplication the whole shape rests on, and the reason it is asserted rather than trusted:
    /// a conditional edge is **two rows over the same pair**, and reads as a bug to anyone who meets
    /// it without [`BRANCH_FEEDS`]' own doc. Lose the `FEEDS` row and the ordering stops seeing the
    /// edge; lose the twin and the condition has nowhere to live.
    #[tokio::test]
    async fn a_branch_is_written_as_both_a_feeding_edge_and_a_branch_carrying_its_condition() {
        let stub = empty_plane().await;
        GraphPlane::new(stub.client())
            .put(&branching_graph())
            .await
            .expect("the stub takes the write");

        let edges = stub.calls("edge.create");
        assert_eq!(edges.len(), 2, "a branch is written as one row, not two");
        for row in &edges {
            assert_eq!(row["src"], "GraphNode/acme/triage/intake");
            assert_eq!(row["dst"], "GraphNode/acme/triage/reply");
            assert_eq!(row["properties"][OWNER], "acme");
        }
        assert_eq!(edges[0]["type"], FEEDS);
        assert_eq!(
            edges[0]["properties"][CONDITION],
            Value::Null,
            "the feeding row carried the condition, where no algorithm reads it"
        );
        assert_eq!(edges[1]["type"], BRANCH_FEEDS);
        assert_eq!(
            edges[1]["properties"][CONDITION],
            "the complaint needs an answer"
        );
    }

    /// A branch is a feeding edge, so the cycle check must see it — by the `FEEDS` row, not the twin.
    #[tokio::test]
    async fn the_cycle_check_of_a_branching_write_still_follows_only_what_feeds() {
        let stub = empty_plane().await;
        GraphPlane::new(stub.client())
            .put(&branching_graph())
            .await
            .expect("the stub takes the write");

        let asked = stub.calls("plane.algo");
        assert_eq!(asked.len(), 1, "a branching write asks for the cycle alone");
        assert_eq!(asked[0]["algo"], "cycles");
        assert_eq!(
            asked[0]["edge_type"], FEEDS,
            "the check followed the branch twin as well as the edge it duplicates"
        );
    }

    /// The invariant the whole milestone rests on: a loop back is invisible to both algorithms.
    #[tokio::test]
    async fn neither_algorithm_a_write_asks_for_follows_a_loop_back() {
        let stub = looping_plane().await;
        GraphPlane::new(stub.client())
            .put(&looping_graph())
            .await
            .expect("the stub takes the write");

        let asked = stub.calls("plane.algo");
        assert_eq!(
            asked.len(),
            2,
            "a looping write asks for the cycle and the ancestry"
        );
        for one in &asked {
            assert_eq!(
                one["edge_type"], FEEDS,
                "{} followed an edge that does not feed",
                one["algo"]
            );
        }
        assert_eq!(asked[0]["algo"], "cycles");
        assert_eq!(asked[1]["algo"], "shortest_path");
        assert_eq!(
            (&asked[1]["src"], &asked[1]["dst"]),
            (&json!(7), &json!(7)),
            "the search did not run from the target towards the asker"
        );
        assert_eq!(asked[1]["dir"], "out");
    }

    /// Refused at the write, which is the roadmap's own requirement: a person reading the graph
    /// can see where it may double back, so a graph that cannot must not be stored.
    #[tokio::test]
    async fn a_loop_back_to_a_node_that_does_not_feed_the_asker_is_refused() {
        let stub = StubPlane::start(Replies::scripted([
            ("plane.query", json!([])),
            ("node.create", json!({"id": 7})),
            (
                "plane.algo",
                json!({"count": 0, "results": [], "found": false}),
            ),
        ]))
        .await;

        let refusal = GraphPlane::new(stub.client()).put(&looping_graph()).await;

        assert_eq!(
            refusal,
            Err(GraphError::LoopNotBackwards {
                from: name("reply"),
                to: name("intake"),
            })
        );
        assert!(
            !stub.calls("node.delete").is_empty(),
            "a refused write left its rows behind"
        );
    }

    /// A node asking its own work again, which needs no path and is never asked about.
    #[tokio::test]
    async fn a_loop_back_a_node_declares_on_itself_is_not_asked_about() {
        let stored = graph();
        let itself = RunGraph::checked(RunGraphParts {
            id: stored.id().clone(),
            name: stored.name().clone(),
            owner: stored.owner().clone(),
            nodes: stored.nodes().to_vec(),
            edges: stored.edges().to_vec(),
            loops: vec![LoopBack {
                from: name("reply"),
                to: name("reply"),
                cap: LoopCap::new(1).expect("a cap"),
            }],
            max_nodes: stored.max_nodes(),
        })
        .expect("a node may think again");
        let stub = empty_plane().await;

        GraphPlane::new(stub.client())
            .put(&itself)
            .await
            .expect("a self loop needs no ancestry");

        let asked = stub.calls("plane.algo");
        assert_eq!(
            asked.len(),
            1,
            "the ancestry of a node to itself was asked for"
        );
        assert_eq!(asked[0]["algo"], "cycles");
    }

    #[tokio::test]
    async fn the_nodes_a_loop_back_sends_back_to_work_are_asked_for_in_one_walk() {
        let stub = StubPlane::start(Replies::scripted([(
            "plane.cypher",
            json!({"columns": ["name"], "rows": [["triage"], ["reply"]]}),
        )]))
        .await;

        let reached = GraphPlane::new(stub.client())
            .downstream(&tenant(), &id(), &name("intake"))
            .await
            .expect("the walk answers");

        assert_eq!(
            reached,
            ["intake", "reply", "triage"]
                .into_iter()
                .map(name)
                .collect::<BTreeSet<_>>(),
            "the node whose work is redone was left out of what runs again"
        );
        let asked = stub.calls("plane.cypher");
        assert_eq!(
            asked.len(),
            1,
            "a walk of the graph took more than one read"
        );
        assert_eq!(asked[0]["params"]["key"], "GraphNode/acme/triage/intake");
        assert_eq!(asked[0]["params"][GRAPH], "triage");
        assert_eq!(asked[0]["params"][OWNER], "acme");
        let statement = asked[0]["query"].as_str().expect("a statement crossed");
        assert!(
            statement.contains(&format!("{FEEDS}*1..{}", MaxNodes::MOST)),
            "the walk followed something other than a bounded run of feeding edges: {statement}"
        );
        assert!(
            !statement.contains(LOOPS_BACK),
            "the walk followed the loop backs it is answering about: {statement}"
        );
    }

    #[tokio::test]
    async fn a_walk_the_store_answers_unusably_is_refused() {
        for answer in [
            json!({"rows": "not a table"}),
            json!({"columns": ["name"], "rows": [[7]]}),
            json!({"columns": ["name"], "rows": [["two words"]]}),
        ] {
            let stub =
                StubPlane::start(Replies::scripted([("plane.cypher", answer.clone())])).await;
            let refusal = GraphPlane::new(stub.client())
                .downstream(&tenant(), &id(), &name("intake"))
                .await;
            assert!(
                refusal.is_err(),
                "{answer} was read as the nodes to run again"
            );
        }
    }

    /// A cycle cannot be looked for until the rows exist, so the rows go in and come back out.
    #[tokio::test]
    async fn a_graph_that_feeds_itself_is_refused_and_taken_back_out() {
        let stub = StubPlane::start(Replies::scripted([
            ("plane.query", json!([])),
            ("node.create", json!({"id": 7})),
            (
                "plane.algo",
                json!({"count": 1, "results": [{"cycle": [7, 7]}]}),
            ),
        ]))
        .await;
        let refusal = GraphPlane::new(stub.client()).put(&graph()).await;
        let Err(GraphError::Cyclic { cycle }) = refusal else {
            panic!("{refusal:?} was taken as a graph");
        };
        assert_eq!(
            cycle, "intake → intake",
            "the cycle was reported in the store's own ids"
        );
        assert_eq!(
            stub.calls("node.delete").len(),
            2,
            "the rows a refused write left behind were not taken back out"
        );
    }

    #[tokio::test]
    async fn the_order_comes_back_named_and_in_generations() {
        let stored = graph();
        let stub = StubPlane::start(Replies::scripted([
            (
                "plane.algo",
                json!({"count": 2, "results": [
                    {"id": 2, "order": 0, "generation": 0},
                    {"id": 1, "order": 1, "generation": 1},
                ]}),
            ),
            ("plane.query", node_rows(&stored)),
        ]))
        .await;
        let order = GraphPlane::new(stub.client())
            .order(&tenant(), &id())
            .await
            .expect("a sorted graph has an order");
        assert_eq!(
            order.placed(),
            [
                Placed {
                    name: name("reply"),
                    generation: Generation::new(0),
                },
                Placed {
                    name: name("intake"),
                    generation: Generation::new(1),
                },
            ],
            "the store's ids were not read back as the graph's own names"
        );
    }

    /// The store caps its own answer, and a truncated order reads exactly like a complete one.
    #[tokio::test]
    async fn an_order_shorter_than_the_graph_is_refused_rather_than_run() {
        let stub = StubPlane::start(Replies::scripted([(
            "plane.algo",
            json!({"count": 120, "results": [{"id": 1, "order": 0, "generation": 0}]}),
        )]))
        .await;
        assert_eq!(
            GraphPlane::new(stub.client()).order(&tenant(), &id()).await,
            Err(GraphError::Truncated {
                held: 120,
                answered: 1,
            })
        );
    }

    #[tokio::test]
    async fn a_sort_asks_for_every_node_a_graph_may_hold_and_for_parents_first() {
        let stub = StubPlane::start(Replies::scripted([
            ("plane.algo", json!({"count": 0, "results": []})),
            ("plane.query", json!([])),
        ]))
        .await;
        GraphPlane::new(stub.client())
            .order(&tenant(), &id())
            .await
            .expect("an empty order is still an order");
        let asked = stub.calls("plane.algo");
        assert_eq!(asked[0]["algo"], "toposort");
        assert_eq!(asked[0]["label"], "Graph/acme/triage");
        assert_eq!(asked[0]["edge_type"], FEEDS);
        assert_eq!(
            asked[0]["dir"], "out",
            "the order was asked for children first"
        );
        assert_eq!(
            asked[0]["limit"],
            MaxNodes::MOST,
            "the order was asked for fewer nodes than a graph may hold"
        );
    }

    #[tokio::test]
    async fn an_order_placing_a_node_the_graph_does_not_hold_is_refused() {
        let stub = StubPlane::start(Replies::scripted([
            (
                "plane.algo",
                json!({"count": 1, "results": [{"id": 99, "order": 0, "generation": 0}]}),
            ),
            ("plane.query", node_rows(&graph())),
        ]))
        .await;
        assert_eq!(
            GraphPlane::new(stub.client()).order(&tenant(), &id()).await,
            Err(GraphError::Unplaced)
        );
    }

    #[tokio::test]
    async fn every_row_a_write_sends_carries_its_owner() {
        let stub = empty_plane().await;
        GraphPlane::new(stub.client())
            .put(&graph())
            .await
            .expect("the stub takes the write");
        let written = stub.calls("node.create");
        assert_eq!(written.len(), 3, "a graph of two nodes writes three rows");
        for one in &written {
            assert_eq!(one["properties"][OWNER], "acme", "a row carries no owner");
            assert_eq!(one["properties"][GRAPH], "triage");
        }
        for one in stub.calls("edge.create") {
            assert_eq!(one["properties"][OWNER], "acme", "an edge carries no owner");
            assert_eq!(one["type"], FEEDS);
            assert_eq!(one["src"], "GraphNode/acme/triage/intake");
            assert_eq!(one["dst"], "GraphNode/acme/triage/reply");
        }
    }

    /// The second label is what makes `plane.algo` answer for one graph instead of every graph in
    /// the plane, so a node without it is a node a topological sort would not see.
    #[tokio::test]
    async fn every_node_a_write_sends_carries_its_graphs_own_label() {
        let stub = empty_plane().await;
        GraphPlane::new(stub.client())
            .put(&graph())
            .await
            .expect("the stub takes the write");
        let nodes: Vec<Value> = stub
            .calls("node.create")
            .into_iter()
            .filter(|one| one["labels"][0] == NODE_LABEL)
            .collect();
        assert_eq!(nodes.len(), 2, "a graph of two nodes labelled otherwise");
        for one in &nodes {
            assert_eq!(one["labels"][1], "Graph/acme/triage");
        }
    }

    #[tokio::test]
    async fn a_graph_the_owner_does_not_have_is_nothing_rather_than_a_failure() {
        let stub = empty_plane().await;
        assert_eq!(
            GraphPlane::new(stub.client())
                .get(&tenant(), &id())
                .await
                .expect("a read of nothing is not a failure"),
            None
        );
        assert_eq!(
            stub.calls("plane.query").len(),
            1,
            "a graph that is not there was read three times"
        );
    }

    /// The round trip the mapping exists for, over a socket: a plane answers the four reads of
    /// `get` with exactly what a write sent it, and the graph that comes back is the one that went
    /// in — id, name, owner, ceiling, nodes, edge and loop back alike.
    #[tokio::test]
    async fn a_graph_read_back_is_the_graph_that_was_stored() {
        for stored in [graph(), looping_graph(), branching_graph()] {
            let stub = StubPlane::start(Replies::in_turn([
                graph_rows(&stored),
                node_rows(&stored),
                edge_table(&stored),
                loop_table(&stored),
                branch_table(&stored),
            ]))
            .await;
            assert_eq!(
                GraphPlane::new(stub.client())
                    .get(&tenant(), &id())
                    .await
                    .expect("a plane holding the graph answers with it"),
                Some(stored.clone())
            );
            assert_eq!(
                stub.calls("plane.query").len(),
                3,
                "a graph's own row, its nodes and its edges are three plans"
            );
            assert_eq!(
                stub.calls("plane.cypher").len(),
                2,
                "its loop backs and its branches are the two reads a plan cannot answer"
            );
        }
    }

    /// The cap is read off the hop, which is the one thing a read plan could not do.
    ///
    /// Asserted on the statement because a stub answers whatever it is scripted: what went wrong
    /// here once was a plan that asked for an edge's property and was answered `null`, with every
    /// test passing. The statement is the fix, so the statement is what is pinned.
    #[tokio::test]
    async fn the_loop_backs_of_a_graph_are_read_with_the_cap_off_the_hop_itself() {
        let stored = looping_graph();
        let stub = StubPlane::start(Replies::in_turn([
            graph_rows(&stored),
            node_rows(&stored),
            edge_table(&stored),
            loop_table(&stored),
            branch_table(&stored),
        ]))
        .await;

        let read = GraphPlane::new(stub.client())
            .get(&tenant(), &id())
            .await
            .expect("a plane holding the graph answers with it")
            .expect("the graph is there");

        assert_eq!(
            read.loops()[0].cap.get(),
            2,
            "the stored cap did not come back"
        );
        let asked = stub.calls("plane.cypher");
        let statement = asked[0]["query"].as_str().expect("a statement crossed");
        assert!(
            statement.contains(&format!("[r:{LOOPS_BACK}]")),
            "the read did not follow the loop backs: {statement}"
        );
        assert!(
            statement.contains(&format!("r.{CAP}")),
            "the cap was not read off the hop: {statement}"
        );
        assert_eq!(asked[0]["params"][GRAPH], "triage");
        assert_eq!(asked[0]["params"][OWNER], "acme");
    }

    /// A graph stored before loop backs existed carries no such rows, and must still read back.
    ///
    /// The whole of the migration story for this milestone: the plane holds no loop back edges for
    /// it, the read asks anyway, and an empty table is a graph that declares none.
    #[tokio::test]
    async fn a_graph_stored_before_loop_backs_existed_reads_back_without_any() {
        let stored = graph();
        let stub = StubPlane::start(Replies::in_turn([
            graph_rows(&stored),
            node_rows(&stored),
            edge_table(&stored),
            json!({"columns": ["from", "to", CAP], "rows": []}),
            branch_table(&stored),
        ]))
        .await;

        let read = GraphPlane::new(stub.client())
            .get(&tenant(), &id())
            .await
            .expect("a plane holding the old graph answers with it")
            .expect("the graph is there");

        assert!(read.loops().is_empty());
        assert_eq!(read, stored);
    }

    /// A graph stored before branches existed carries no such rows, and must still read back.
    ///
    /// The migration story again, one milestone on: the plane holds no `BRANCH_FEEDS` row for it,
    /// the read asks anyway, and an empty table is a graph whose edges all deliver.
    #[tokio::test]
    async fn a_graph_stored_before_branches_existed_reads_back_without_any() {
        let stored = graph();
        let stub = StubPlane::start(Replies::in_turn([
            graph_rows(&stored),
            node_rows(&stored),
            edge_table(&stored),
            loop_table(&stored),
            json!({"columns": ["from", "to", CONDITION], "rows": []}),
        ]))
        .await;

        let read = GraphPlane::new(stub.client())
            .get(&tenant(), &id())
            .await
            .expect("a plane holding the old graph answers with it")
            .expect("the graph is there");

        assert_eq!(read.branches().count(), 0);
        assert_eq!(read, stored);
    }

    /// The condition is read off the hop, for the reason the cap is, and the statement is pinned.
    ///
    /// The M28 defect this guards against passed 125 tests: a stub answers whatever it is scripted,
    /// so a plan asking a *real* plane for an edge's property is answered `null` while every test
    /// goes green. Nothing but the statement itself can hold that line in process.
    #[tokio::test]
    async fn the_branches_of_a_graph_are_read_with_the_condition_off_the_hop_itself() {
        let stored = branching_graph();
        let stub = StubPlane::start(Replies::in_turn([
            graph_rows(&stored),
            node_rows(&stored),
            edge_table(&stored),
            loop_table(&stored),
            branch_table(&stored),
        ]))
        .await;

        let read = GraphPlane::new(stub.client())
            .get(&tenant(), &id())
            .await
            .expect("a plane holding the graph answers with it")
            .expect("the graph is there");

        assert_eq!(
            read.branch(&name("intake"), &name("reply"))
                .expect("the stored condition came back")
                .as_str(),
            "the complaint needs an answer"
        );
        let statement = stub.calls("plane.cypher")[1]["query"]
            .as_str()
            .expect("a statement crossed")
            .to_owned();
        assert!(
            statement.contains(&format!("[r:{BRANCH_FEEDS}]")),
            "the read did not follow the branches: {statement}"
        );
        assert!(
            statement.contains(&format!("r.{CONDITION}")),
            "the condition was not read off the hop: {statement}"
        );
    }

    /// One statement, and not the four reads of a whole graph: what a member running one node needs.
    #[tokio::test]
    async fn the_branches_of_a_graph_are_answered_without_reading_the_rest_of_it() {
        let stored = branching_graph();
        let stub = StubPlane::start(Replies::in_turn([branch_table(&stored)])).await;

        let branches = GraphPlane::new(stub.client())
            .branches(&tenant(), &id())
            .await
            .expect("a plane holding the graph answers with its branches");

        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].from.as_str(), "intake");
        assert_eq!(branches[0].to.as_str(), "reply");
        assert_eq!(
            branches[0].condition.as_ref().map(Condition::as_str),
            Some("the complaint needs an answer")
        );
        assert!(
            stub.calls("plane.query").is_empty(),
            "the branches were read by reading the graph"
        );
        let asked = stub.calls("plane.cypher");
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0]["params"][GRAPH], "triage");
        assert_eq!(asked[0]["params"][OWNER], "acme");
    }

    /// A condition the plane answers with as anything but text is refused, not dropped.
    #[tokio::test]
    async fn a_stored_condition_that_is_not_text_is_refused() {
        for answered in [
            json!({"columns": ["from", "to", CONDITION], "rows": [["intake", "reply", 7]]}),
            json!({"columns": ["from", "to", CONDITION], "rows": [["intake", "reply", null]]}),
            json!({"columns": ["from", "to", CONDITION], "rows": [["intake", "reply"]]}),
            json!({"columns": ["from", "to", CONDITION], "rows": "no"}),
        ] {
            let stub = StubPlane::start(Replies::in_turn([answered.clone()])).await;
            assert!(
                GraphPlane::new(stub.client())
                    .branches(&tenant(), &id())
                    .await
                    .is_err(),
                "a branch read took {answered}"
            );
        }
    }

    /// A condition the plane holds against a pair no edge feeds is refused rather than lost.
    ///
    /// Reachable only where the store was edited by something that is not this code, which is why
    /// it is refused: dropping it would turn a branch into an edge that always delivers, and the
    /// graph would run differently rather than fail to read.
    #[tokio::test]
    async fn a_stored_branch_no_edge_feeds_is_refused_rather_than_dropped() {
        let stored = graph();
        let stub = StubPlane::start(Replies::in_turn([
            graph_rows(&stored),
            node_rows(&stored),
            edge_table(&stored),
            loop_table(&stored),
            json!({
                "columns": ["from", "to", CONDITION],
                "rows": [["reply", "intake", "whenever"]],
            }),
        ]))
        .await;

        let refusal = GraphPlane::new(stub.client()).get(&tenant(), &id()).await;
        assert!(
            matches!(refusal, Err(GraphError::Unreadable { .. })),
            "a branch over a pair the graph does not feed was taken: {refusal:?}"
        );
    }

    /// What the filter in the plan is there to prevent, were it ever lost: rows arrive that were
    /// not asked for, and the read says so rather than handing them over.
    #[tokio::test]
    async fn a_row_belonging_to_another_owner_is_refused_rather_than_returned() {
        let theirs = RunGraph::checked(RunGraphParts {
            id: id(),
            name: GraphName::new("Support triage").expect("a graph name"),
            owner: GraphOwner::Tenant(TenantId::new("globex").expect("another tenant")),
            nodes: vec![node("intake")],
            edges: Vec::new(),
            loops: Vec::new(),
            max_nodes: MaxNodes::new(8).expect("a ceiling"),
        })
        .expect("another tenant's graph holds together");
        let stub = StubPlane::start(Replies::in_turn([graph_rows(&theirs)])).await;
        assert_eq!(
            GraphPlane::new(stub.client()).get(&tenant(), &id()).await,
            Err(GraphError::Foreign { property: OWNER })
        );
    }

    #[tokio::test]
    async fn a_store_that_cannot_be_reached_is_said_so_rather_than_read_as_empty() {
        let stub = StubPlane::start(Replies::Failure).await;
        let plane = GraphPlane::new(stub.client());
        assert!(matches!(
            plane.get(&tenant(), &id()).await,
            Err(GraphError::Plane(_))
        ));
        assert!(matches!(
            plane.put(&graph()).await,
            Err(GraphError::Plane(_))
        ));
    }

    /// One row of this graph and this owner, carrying whatever a case is about.
    fn stored_node(properties: Value) -> Value {
        let mut row = json!({"graph": "triage", "owner": "acme"});
        if let (Value::Object(row), Value::Object(extra)) = (&mut row, properties) {
            row.extend(extra);
        }
        json!([{"properties": row}])
    }

    fn read_node(properties: Value) -> Result<Vec<GraphNode>, GraphError> {
        nodes_from(&stored_node(properties), &tenant(), &id())
    }

    #[test]
    fn a_stored_row_that_is_not_a_node_is_refused_by_what_is_wrong_with_it() {
        assert_eq!(
            read_node(json!({"name": "intake"})),
            Err(GraphError::PropertyMissing { property: AGENT })
        );
        assert_eq!(
            read_node(json!({"name": 7, "agent": "a"})),
            Err(GraphError::PropertyKind { property: NAME })
        );
        assert!(
            matches!(
                read_node(json!({"name": "two words", "agent": "a"})),
                Err(GraphError::IllegalChar { .. })
            ),
            "a stored name no node may carry was taken"
        );
        assert!(
            matches!(
                read_node(json!({"name": "a", "agent": "two words"})),
                Err(GraphError::Unreadable { .. })
            ),
            "a stored template id that is not one was taken"
        );
        assert!(matches!(
            nodes_from(&json!({"rows": []}), &tenant(), &id()),
            Err(GraphError::Unreadable { .. })
        ));
    }

    #[test]
    fn a_node_row_of_another_graph_is_refused_by_which_half_did_not_match() {
        for (property, row) in [
            (
                GRAPH,
                json!([{"properties": {"graph": "other", "owner": "acme"}}]),
            ),
            (
                OWNER,
                json!([{"properties": {"graph": "triage", "owner": "globex"}}]),
            ),
        ] {
            assert_eq!(
                nodes_from(&row, &tenant(), &id()),
                Err(GraphError::Foreign { property }),
                "a row of another {property} was taken"
            );
        }
    }

    #[test]
    fn a_table_that_is_not_edges_is_refused() {
        assert!(matches!(
            edges_from(&json!({"columns": ["from", "to"]})),
            Err(GraphError::Unreadable { .. })
        ));
        assert!(
            matches!(
                edges_from(&json!({"rows": [["intake"]]})),
                Err(GraphError::Unreadable { .. })
            ),
            "a row of one name was read as an edge"
        );
        assert_eq!(
            edges_from(&json!({"rows": [[1, 2]]})),
            Err(GraphError::PropertyKind { property: NAME })
        );
    }

    /// What a store answers when it answers something no read can use.
    fn not_rows() -> Value {
        json!({"answered": "something that is not rows"})
    }

    /// Every read a method makes, answered unusably in turn, and each one must refuse.
    ///
    /// The control comes first — the same answers as they should be — so a refusal below is the
    /// spoiled answer and not a fixture that never worked. The read count is asserted, which is
    /// the lever M24's store sweep used: a method that gains or loses a read fails here rather
    /// than quietly leaving a new arm untested.
    async fn refused_at_each_read<Call, Answering, Answered>(
        answers: &[Value],
        reads: usize,
        call: Call,
    ) where
        Call: Fn(GraphPlane) -> Answering,
        Answering: std::future::Future<Output = Result<Answered, GraphError>>,
        Answered: std::fmt::Debug,
    {
        assert_eq!(
            answers.len(),
            reads,
            "the sweep prepares {} answers for {reads} reads",
            answers.len()
        );
        let stub = StubPlane::start(Replies::in_turn(answers.to_vec())).await;
        call(GraphPlane::new(stub.client()))
            .await
            .expect("the control: every read answered as it should be");

        for spoiled in 0..reads {
            let mut answers = answers.to_vec();
            answers[spoiled] = not_rows();
            let stub = StubPlane::start(Replies::in_turn(answers)).await;
            let refusal = call(GraphPlane::new(stub.client())).await;
            assert!(
                refusal.is_err(),
                "read {spoiled} answered unusably and the call carried on with {refusal:?}"
            );
        }
    }

    /// The order a plane answers for the fixture's two nodes, as `toposort` reports one.
    fn sorted() -> Value {
        json!({
            "algo": "toposort",
            "count": 2,
            "results": [{"id": 1, "generation": 0}, {"id": 2, "generation": 1}],
        })
    }

    #[tokio::test]
    async fn a_graph_read_refuses_at_whichever_of_its_five_reads_cannot_be_used() {
        let graph = looping_graph();
        refused_at_each_read(
            &[
                graph_rows(&graph),
                node_rows(&graph),
                edge_table(&graph),
                loop_table(&graph),
                branch_table(&graph),
            ],
            5,
            |plane| async move { plane.get(&tenant(), &id()).await },
        )
        .await;
    }

    #[tokio::test]
    async fn an_order_refuses_at_whichever_of_its_two_reads_cannot_be_used() {
        let graph = graph();
        refused_at_each_read(&[sorted(), node_rows(&graph)], 2, |plane| async move {
            plane.order(&tenant(), &id()).await
        })
        .await;
    }

    #[tokio::test]
    async fn a_listing_refuses_when_its_one_read_cannot_be_used() {
        refused_at_each_read(
            &[json!({
                "columns": [GRAPH, NAME, MAX_NODES, OWNER],
                "rows": [["triage", "Support triage", 8, "acme"]],
            })],
            1,
            |plane| async move {
                plane
                    .graphs(
                        &tenant(),
                        Listing {
                            limit: 5,
                            offset: 0,
                        },
                    )
                    .await
            },
        )
        .await;
    }

    #[tokio::test]
    async fn a_removal_refuses_at_whichever_of_its_two_reads_cannot_be_used() {
        let graph = graph();
        refused_at_each_read(
            &[graph_rows(&graph), node_rows(&graph)],
            2,
            |plane| async move { plane.remove(&tenant(), &id()).await },
        )
        .await;
    }

    /// An order is read for what it places as well as for its shape.
    #[tokio::test]
    async fn an_order_this_code_cannot_act_on_is_refused_by_name() {
        let graph = graph();
        let cases: [(Value, &str); 4] = [
            (
                json!({"count": 2, "results": [{"generation": 0}, {"id": 2, "generation": 1}]}),
                "a node it did not name",
            ),
            (
                json!({"count": 2, "results": [{"id": 1}, {"id": 2, "generation": 1}]}),
                "a node placed in no generation",
            ),
            (
                json!({"count": 9, "results": [{"id": 1, "generation": 0}]}),
                "an order the store cut short",
            ),
            (
                json!({"count": 1, "results": [{"id": 404, "generation": 0}]}),
                "an order placing a node this graph has not got",
            ),
        ];
        for (answered, what) in cases {
            let stub = StubPlane::start(Replies::in_turn([answered, node_rows(&graph)])).await;
            let refusal = GraphPlane::new(stub.client()).order(&tenant(), &id()).await;
            assert!(refusal.is_err(), "{what} was taken: {refusal:?}");
        }
    }

    /// A ceiling past what the type holds, which no schema guard refuses first.
    #[tokio::test]
    async fn a_stored_ceiling_no_ceiling_can_hold_is_refused() {
        let graph = graph();
        let mut rows = graph_rows(&graph);
        rows[0]["properties"][MAX_NODES] = json!(5_000_000_000u64);
        let stub = StubPlane::start(Replies::in_turn([rows])).await;

        let refusal = GraphPlane::new(stub.client()).get(&tenant(), &id()).await;

        match refusal {
            Err(GraphError::PropertyKind { property }) => assert_eq!(property, MAX_NODES),
            other => panic!("{other:?} was taken"),
        }
    }

    /// A listed row is read as strictly as a single one, and refuses the same things.
    #[tokio::test]
    async fn a_listed_graph_its_own_types_will_not_take_is_refused() {
        let cases: [(Value, &str); 4] = [
            (
                json!([["triage", "Support triage", 5_000_000_000u64, "acme"]]),
                "a ceiling no ceiling can hold",
            ),
            (
                json!([["triage", "Support triage", 8, "globex"]]),
                "another owner's row",
            ),
            (
                json!([["", "Support triage", 8, "acme"]]),
                "an id that is none",
            ),
            (
                json!([["triage", 7, 8, "acme"]]),
                "a name that is not even text",
            ),
        ];
        for (rows, what) in cases {
            let stub = StubPlane::start(Replies::scripted([(
                "plane.query",
                json!({"columns": [GRAPH, NAME, MAX_NODES, OWNER], "rows": rows}),
            )]))
            .await;

            let refusal = GraphPlane::new(stub.client())
                .graphs(
                    &tenant(),
                    Listing {
                        limit: 5,
                        offset: 0,
                    },
                )
                .await;

            assert!(refusal.is_err(), "{what} was listed: {refusal:?}");
        }
    }

    /// A count is read before the rows are trusted, and an answer carrying none is refused.
    #[tokio::test]
    async fn an_algorithm_that_counted_nothing_is_refused_rather_than_believed() {
        let graph = graph();
        let countless = json!({"algo": "toposort", "results": []});

        let stub = StubPlane::start(Replies::in_turn([countless.clone()])).await;
        let ordered = GraphPlane::new(stub.client()).order(&tenant(), &id()).await;
        assert!(ordered.is_err(), "an order with no count: {ordered:?}");

        // The same arm one layer down, where a write checks what it just wrote for a cycle.
        let stub = StubPlane::start(Replies::scripted([
            ("plane.query", json!([])),
            ("plane.algo", countless),
        ]))
        .await;
        let written = GraphPlane::new(stub.client()).put(&graph).await;
        assert!(written.is_err(), "a cycle check with no count: {written:?}");
    }

    /// What a refused write says about the cycle it found, as well as the store allows.
    ///
    /// Which of the three it can say depends on what the store hands back: a node's own name only
    /// when the write was answered with the id the cycle then names, the bare id when it was not,
    /// and `?` for an entry that is not an id at all.
    #[tokio::test]
    async fn a_cycle_is_named_as_well_as_the_store_allows() {
        let graph = graph();
        let cases: [(Value, Value, &str); 3] = [
            (
                json!({"id": 1}),
                json!({"count": 1, "results": [{"cycle": [1]}]}),
                "intake",
            ),
            (
                json!({}),
                json!({"count": 1, "results": [{"cycle": [404]}]}),
                "404",
            ),
            (
                json!({}),
                json!({"count": 1, "results": [{"cycle": ["not an id"]}]}),
                "?",
            ),
        ];
        for (created, found, said) in cases {
            let stub = StubPlane::start(Replies::scripted([
                ("plane.query", json!([])),
                ("node.create", created),
                ("plane.algo", found),
            ]))
            .await;

            let refusal = GraphPlane::new(stub.client()).put(&graph).await;

            match refusal {
                Err(GraphError::Cyclic { cycle }) => {
                    assert!(cycle.contains(said), "{cycle:?} does not name {said}");
                }
                other => panic!("{other:?} was taken, wanting {said}"),
            }
        }
    }

    /// A cycle the store found but would not describe at all.
    #[tokio::test]
    async fn a_cycle_the_store_will_not_describe_is_still_refused() {
        let stub = StubPlane::start(Replies::scripted([
            ("plane.query", json!([])),
            ("plane.algo", json!({"count": 1, "results": [{}]})),
        ]))
        .await;

        let refusal = GraphPlane::new(stub.client()).put(&graph()).await;

        match refusal {
            Err(GraphError::Cyclic { cycle }) => {
                assert_eq!(cycle, "nodes the store did not name");
            }
            other => panic!("{other:?} was taken"),
        }
    }

    /// A stored ceiling that is absent, and one that is not a number at all.
    ///
    /// The two are told apart on purpose: an operator reading "the property is missing" looks for
    /// a write that never set it, and one reading "the wrong kind" looks at what is in the column.
    #[tokio::test]
    async fn a_graph_stored_without_a_usable_ceiling_is_refused_by_name() {
        let graph = graph();
        let mut rows = graph_rows(&graph);
        rows[0]["properties"][MAX_NODES] = Value::Null;
        let stub = StubPlane::start(Replies::in_turn([rows])).await;
        match GraphPlane::new(stub.client()).get(&tenant(), &id()).await {
            Err(GraphError::PropertyMissing { property }) => assert_eq!(property, MAX_NODES),
            other => panic!("{other:?} was taken for a ceiling nothing stored"),
        }

        let mut rows = graph_rows(&graph);
        rows[0]["properties"][MAX_NODES] = json!("eight");
        let stub = StubPlane::start(Replies::in_turn([rows])).await;
        match GraphPlane::new(stub.client()).get(&tenant(), &id()).await {
            Err(GraphError::PropertyKind { property }) => assert_eq!(property, MAX_NODES),
            other => panic!("{other:?} was taken for a ceiling that is a word"),
        }
    }

    /// A run id a test can write down and read back.
    fn run_id() -> GraphRunId {
        GraphRunId::from_hex("0123456789abcdef0123456789abcdef").expect("a run id")
    }

    /// Where a test's own node is running.
    fn address() -> ClusterAddress {
        "10.0.0.2:8080".parse().expect("an address")
    }

    fn placement() -> Placement {
        Placement {
            run: run_id(),
            node: node("intake").name,
            graph: id(),
            owner: tenant(),
            address: address(),
            at: Timestamp::from_unix_seconds(1_762_000_000).expect("a time"),
        }
    }

    /// The row a plane answers a placement read with, built from what the write would have sent.
    fn placement_rows(placement: &Placement) -> Value {
        json!([{"id": 1, "properties": placement_properties(placement)}])
    }

    #[tokio::test]
    async fn a_running_node_is_recorded_where_it_runs_and_read_back_whole() {
        let placed = placement();
        let stub = StubPlane::start(Replies::in_turn([placement_rows(&placed)])).await;

        let read = GraphPlane::new(stub.client())
            .placements(&tenant(), &run_id())
            .await
            .expect("the record reads back");

        assert_eq!(read, vec![placed]);
    }

    #[tokio::test]
    async fn a_dispatch_retried_replaces_the_record_rather_than_being_refused_one() {
        let stub = StubPlane::start(Replies::Echo).await;

        GraphPlane::new(stub.client())
            .placed(&placement())
            .await
            .expect("the stub takes the write");

        let seen: Vec<String> = stub.seen().into_iter().map(|one| one.method).collect();
        assert_eq!(
            seen,
            ["node.delete", "node.create"],
            "a key the store binds once is cleared before it is written"
        );
    }

    #[tokio::test]
    async fn a_record_is_keyed_by_the_run_and_the_node_together() {
        let other = NodeName::new("reply").expect("a node name");

        assert_ne!(
            placement_key(&run_id(), &node("intake").name),
            placement_key(&run_id(), &other),
            "two nodes of one run"
        );
        let elsewhere = GraphRunId::from_hex("ffffffffffffffffffffffffffffffff").expect("a run id");
        assert_ne!(
            placement_key(&run_id(), &node("intake").name),
            placement_key(&elsewhere, &node("intake").name),
            "one node in two runs"
        );
    }

    #[tokio::test]
    async fn a_node_that_finishes_takes_its_record_away() {
        let stub = StubPlane::start(Replies::Deleted).await;

        GraphPlane::new(stub.client())
            .finished(&run_id(), &node("intake").name)
            .await
            .expect("the record goes");

        let seen = stub.only();
        assert_eq!(seen.method, "node.delete");
        assert_eq!(
            seen.params["key"],
            placement_key(&run_id(), &node("intake").name)
        );
    }

    /// A write that was best effort and failed leaves nothing to take away.
    #[tokio::test]
    async fn a_node_whose_record_was_never_written_still_finishes() {
        let stub = StubPlane::start(Replies::Absent).await;

        GraphPlane::new(stub.client())
            .finished(&run_id(), &node("intake").name)
            .await
            .expect("nothing to delete is not a failure");
    }

    #[tokio::test]
    async fn a_placement_read_filters_on_the_owner_and_the_run_in_the_plan() {
        let stub = StubPlane::start(Replies::in_turn([json!([])])).await;

        GraphPlane::new(stub.client())
            .placements(&tenant(), &run_id())
            .await
            .expect("the stub answers nothing");

        let plan = stub.only().params["plan"].clone();
        assert_eq!(plan["source"]["ScanLabel"], PLACEMENT_LABEL);
        let filter = &plan["steps"][0]["Filter"]["Logic"];
        assert_eq!(filter["op"], "And");
        assert_eq!(filter["lhs"]["Compare"]["lhs"]["Property"], RUN);
        assert_eq!(
            filter["lhs"]["Compare"]["rhs"]["Literal"]["Str"],
            run_id().to_hex()
        );
        assert_eq!(filter["rhs"]["Compare"]["lhs"]["Property"], OWNER);
        assert_eq!(filter["rhs"]["Compare"]["rhs"]["Literal"]["Str"], "acme");
    }

    #[tokio::test]
    async fn a_record_belonging_to_another_owner_is_refused_rather_than_returned() {
        let mut rows = placement_rows(&placement());
        rows[0]["properties"][OWNER] = json!("globex");
        let stub = StubPlane::start(Replies::in_turn([rows])).await;

        let refusal = GraphPlane::new(stub.client())
            .placements(&tenant(), &run_id())
            .await;

        match refusal {
            Err(GraphError::Foreign { property }) => assert_eq!(property, OWNER),
            other => panic!("{other:?} was taken"),
        }
    }

    #[tokio::test]
    async fn a_record_its_own_types_will_not_take_is_refused_by_name() {
        for (property, stored) in [
            (RUN, json!("not hex")),
            (NODE, json!("")),
            (GRAPH, json!("NOT AN ID")),
            (ADDRESS, json!("10.0.0.2")),
            (AT, json!("whenever")),
            (ADDRESS, Value::Null),
        ] {
            let mut rows = placement_rows(&placement());
            rows[0]["properties"][property] = stored.clone();
            let stub = StubPlane::start(Replies::in_turn([rows])).await;

            let refusal = GraphPlane::new(stub.client())
                .placements(&tenant(), &run_id())
                .await;

            assert!(
                refusal.is_err(),
                "{property} stored as {stored:?} was taken"
            );
        }
    }

    #[tokio::test]
    async fn a_placement_read_refuses_when_its_one_read_cannot_be_used() {
        refused_at_each_read(&[placement_rows(&placement())], 1, |plane| async move {
            plane.placements(&tenant(), &run_id()).await
        })
        .await;
    }

    fn member() -> ClusterMember {
        ClusterMember {
            address: address(),
            at: Timestamp::from_unix_seconds(1_762_000_000).expect("a time"),
        }
    }

    /// The rows a plane answers a member read with, built from what each write would have sent.
    fn member_rows(members: &[ClusterMember]) -> Value {
        Value::Array(
            members
                .iter()
                .enumerate()
                .map(|(at, one)| json!({"id": at + 1, "properties": member_properties(one)}))
                .collect(),
        )
    }

    #[tokio::test]
    async fn a_member_registers_itself_and_reads_back_whole() {
        let joined = member();
        let stub = StubPlane::start(Replies::in_turn([member_rows(std::slice::from_ref(
            &joined,
        ))]))
        .await;

        let read = GraphPlane::new(stub.client())
            .members()
            .await
            .expect("the registration reads back");

        assert_eq!(read, vec![joined]);
    }

    #[tokio::test]
    async fn a_member_saying_so_again_replaces_its_row_rather_than_being_refused_one() {
        let stub = StubPlane::start(Replies::Echo).await;

        GraphPlane::new(stub.client())
            .joined(&member())
            .await
            .expect("the stub takes the write");

        let seen: Vec<String> = stub.seen().into_iter().map(|one| one.method).collect();
        assert_eq!(
            seen,
            ["node.delete", "node.create"],
            "every heartbeat rewrites a key the store would bind only once"
        );
    }

    /// A machine that restarts is the same member, so its address is the whole of its key.
    #[tokio::test]
    async fn a_member_is_keyed_by_its_address_alone() {
        let elsewhere: ClusterAddress = "10.0.0.3:8080".parse().expect("an address");

        assert_eq!(member_key(&address()), member_key(&address()));
        assert_ne!(member_key(&address()), member_key(&elsewhere));
    }

    #[tokio::test]
    async fn a_member_that_goes_takes_its_row_away() {
        let stub = StubPlane::start(Replies::Deleted).await;

        GraphPlane::new(stub.client())
            .left(&address())
            .await
            .expect("the row goes");

        let seen = stub.only();
        assert_eq!(seen.method, "node.delete");
        assert_eq!(seen.params["key"], member_key(&address()));
    }

    /// A registration that was best effort and failed leaves nothing to take away.
    #[tokio::test]
    async fn a_member_whose_row_was_never_written_still_goes() {
        let stub = StubPlane::start(Replies::Absent).await;

        GraphPlane::new(stub.client())
            .left(&address())
            .await
            .expect("nothing to delete is not a failure");
    }

    /// The one read here that narrows by nothing, because a member belongs to no tenant.
    #[tokio::test]
    async fn a_member_read_scans_the_label_and_filters_on_nobody() {
        let stub = StubPlane::start(Replies::in_turn([json!([])])).await;

        GraphPlane::new(stub.client())
            .members()
            .await
            .expect("the stub answers nothing");

        let plan = stub.only().params["plan"].clone();
        assert_eq!(plan["source"]["ScanLabel"], MEMBER_LABEL);
        assert_eq!(plan["steps"], json!([]));
    }

    #[tokio::test]
    async fn the_member_that_wrote_most_recently_is_answered_first() {
        let newer = ClusterMember {
            address: "10.0.0.3:8080".parse().expect("an address"),
            at: Timestamp::from_unix_seconds(1_762_000_500).expect("a time"),
        };
        let stub =
            StubPlane::start(Replies::in_turn([member_rows(&[member(), newer.clone()])])).await;

        let read = GraphPlane::new(stub.client())
            .members()
            .await
            .expect("both read back");

        assert_eq!(
            read,
            vec![newer, member()],
            "a scan's own order is not an order"
        );
    }

    #[tokio::test]
    async fn a_member_row_its_own_types_will_not_take_is_refused_by_name() {
        for (property, stored) in [
            (ADDRESS, json!("10.0.0.2")),
            (ADDRESS, Value::Null),
            (AT, json!("whenever")),
            (AT, json!(i64::MIN)),
        ] {
            let mut rows = member_rows(&[member()]);
            rows[0]["properties"][property] = stored.clone();
            let stub = StubPlane::start(Replies::in_turn([rows])).await;

            let refusal = GraphPlane::new(stub.client()).members().await;

            assert!(
                refusal.is_err(),
                "{property} stored as {stored:?} was taken"
            );
        }
    }

    #[tokio::test]
    async fn a_member_read_refuses_when_its_one_read_cannot_be_used() {
        refused_at_each_read(&[member_rows(&[member()])], 1, |plane| async move {
            plane.members().await
        })
        .await;
    }
}
