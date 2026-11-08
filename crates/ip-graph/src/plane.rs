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

use ip_agents::AgentId;
use ip_drsg::DrsgClient;
use serde_json::{Value, json};

use crate::error::GraphError;
use crate::graph::{GraphEdge, GraphNode, GraphOwner, MaxNodes, RunGraph, RunGraphParts};
use crate::id::{GraphId, GraphName, NodeName};

/// The label on the one node that holds what a graph says about itself.
pub const GRAPH_LABEL: &str = "RunGraph";

/// The label on every node of every graph.
pub const NODE_LABEL: &str = "GraphNode";

/// The type of edge an answer travels along.
pub const FEEDS: &str = "FEEDS";

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

/// The key a store addresses one graph's own node by.
fn graph_key(owner: &GraphOwner, id: &GraphId) -> String {
    format!("{GRAPH_LABEL}/{}/{id}", owner.written())
}

/// The key a store addresses one node of one graph by.
fn node_key(owner: &GraphOwner, id: &GraphId, name: &NodeName) -> String {
    format!("{NODE_LABEL}/{}/{id}/{name}", owner.written())
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
    pub async fn put(&self, graph: &RunGraph) -> Result<(), GraphError> {
        let (owner, id) = (graph.owner(), graph.id());
        for key in self.held(owner, id).await? {
            self.drsg.node_delete(&key).await?;
        }
        self.drsg.node_delete(&graph_key(owner, id)).await?;

        self.drsg
            .node_create(
                &graph_key(owner, id),
                &[GRAPH_LABEL],
                graph_properties(graph),
            )
            .await?;
        let scope = scope_label(owner, id);
        for node in graph.nodes() {
            self.drsg
                .node_create(
                    &node_key(owner, id, &node.name),
                    &[NODE_LABEL, &scope],
                    node_properties(graph, node),
                )
                .await?;
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
        }
        Ok(())
    }

    /// One owner's graph, or nothing where that owner has none under the id.
    ///
    /// Three reads, each filtered on the owner, and the answer goes through
    /// [`RunGraph::checked`]: a graph that comes back from the plane is checked exactly as one
    /// arriving from a caller is, because rows can be edited in the store by something that is not
    /// this code.
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
        RunGraph::checked(RunGraphParts {
            id: id.clone(),
            name,
            owner: owner.clone(),
            nodes,
            edges,
            max_nodes,
        })
        .map(Some)
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
            nodes: vec![node("intake"), node("reply")],
            edges: vec![GraphEdge {
                from: name("intake"),
                to: name("reply"),
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

    /// Every read a graph store sends, answered as a plane holding that graph would answer it.
    ///
    /// One script cannot answer three different `plane.query` calls differently, so each read is
    /// driven by its own stub; what this gives is the write path with reads that find nothing.
    async fn empty_plane() -> StubPlane {
        StubPlane::start(Replies::scripted([("plane.query", json!([]))])).await
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
            ],
            "the write did not read, then delete, then write"
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

    /// The round trip the mapping exists for, over a socket: a plane answers the three reads of
    /// `get` with exactly what a write sent it, and the graph that comes back is the one that went
    /// in — id, name, owner, ceiling, nodes and edge alike.
    #[tokio::test]
    async fn a_graph_read_back_is_the_graph_that_was_stored() {
        let stored = graph();
        let stub = StubPlane::start(Replies::in_turn([
            graph_rows(&stored),
            node_rows(&stored),
            edge_table(&stored),
        ]))
        .await;
        assert_eq!(
            GraphPlane::new(stub.client())
                .get(&tenant(), &id())
                .await
                .expect("a plane holding the graph answers with it"),
            Some(stored)
        );
        assert_eq!(
            stub.calls("plane.query").len(),
            3,
            "a graph is read in three queries: itself, its nodes, its edges"
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
}
