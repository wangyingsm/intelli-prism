//! What a run graph is, and what makes one invalid.

use std::collections::BTreeSet;

use ip_agents::AgentId;
use ip_core::TenantId;

use crate::error::GraphError;
use crate::id::{GraphId, GraphName, NodeName};

/// The most nodes a graph's own ceiling may allow.
const NODES_MOST: u32 = MaxNodes::MOST;

/// The most times one loop back's own cap may allow it to be taken.
const LOOPS_MOST: u32 = LoopCap::MOST;

/// Whose a graph is: the deployment's, or one tenant's.
///
/// Mirrors `ip-storage`'s `AgentOwner` rather than sharing it. This crate takes no i/o, and a type
/// from the storage crate here would make a database part of what a graph is.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphOwner {
    /// The deployment's own, which no tenant may change.
    Global,
    /// One tenant's, which only that tenant reads.
    Tenant(TenantId),
}

impl GraphOwner {
    /// What the deployment's own graphs are recorded as.
    pub const GLOBAL: &'static str = "global";

    /// The tenant whose it is, where it is a tenant's.
    pub fn tenant(&self) -> Option<&TenantId> {
        match self {
            Self::Global => None,
            Self::Tenant(tenant) => Some(tenant),
        }
    }

    /// The owner as the plane records it, which is what every read filters on.
    pub fn written(&self) -> &str {
        match self {
            Self::Global => Self::GLOBAL,
            Self::Tenant(tenant) => tenant.as_str(),
        }
    }
}

/// The most nodes one graph may hold.
///
/// A graph's only ceiling of its own: money and time stay with the limits that already apply per
/// turn and per tenant, because a second accounting of spend is the thing most likely to disagree
/// with the first.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "u32")]
pub struct MaxNodes(u32);

impl MaxNodes {
    /// The most any graph's ceiling may be, whatever a graph asks for.
    ///
    /// Public because it is also what a read of the order must ask the store for: the store caps
    /// its own answer, so a question asked for fewer than this could come back truncated.
    pub const MOST: u32 = 256;

    /// A ceiling, if it is one a graph can be run under.
    pub fn new(value: u32) -> Result<Self, GraphError> {
        if value == 0 || value > NODES_MOST {
            return Err(GraphError::NodeCeiling {
                value,
                limit: NODES_MOST,
            });
        }
        Ok(Self(value))
    }

    /// The ceiling as a number.
    pub fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for MaxNodes {
    type Error = GraphError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// How many times one loop back may be taken in a run.
///
/// Per loop back rather than per graph, so a cheap critique loop and an expensive re-survey get
/// their own budgets. What follows from that, recorded because it is the cost of the choice: a
/// graph with several loop backs has several caps, and what bounds a whole run is their sum, so no
/// one number here says what a run may come to.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "u32")]
pub struct LoopCap(u32);

impl LoopCap {
    /// The most any cap may be, whatever a graph asks for.
    ///
    /// A run that doubles back nine times is a graph that wants a different shape, and a ceiling
    /// here is what stops a stored graph from costing without bound.
    pub const MOST: u32 = 8;

    /// A cap, if it is one a loop back can be taken under.
    pub fn new(value: u32) -> Result<Self, GraphError> {
        if value == 0 || value > LOOPS_MOST {
            return Err(GraphError::LoopCeiling {
                value,
                limit: LOOPS_MOST,
            });
        }
        Ok(Self(value))
    }

    /// The cap as a number.
    pub fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for LoopCap {
    type Error = GraphError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// One node of a graph: what it is called, and the template it runs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GraphNode {
    /// What this node is called inside the graph.
    pub name: NodeName,
    /// The template it runs.
    pub agent: AgentId,
}

/// One edge: whose answer feeds which node.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GraphEdge {
    /// The node whose answer travels.
    pub from: NodeName,
    /// The node that receives it, under the sender's own name.
    pub to: NodeName,
}

/// One declared loop back: who may ask for it, which node it sends work back to, and how often.
///
/// Its own list on a graph rather than a kind on [`GraphEdge`], which is the whole reason this is
/// a separate type. Every reader of the edges — the keying of a node's input, what the graph
/// answers with, the plane's topological sort and its cycle check — is over feeding edges alone,
/// and a kind field would make each of them a place that can forget to filter. Here there is
/// nothing to filter: a loop back is simply not an edge.
///
/// Whether the loop is taken is the asking node's own decision, up to the cap. Nothing in this
/// crate reads one yet.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LoopBack {
    /// The node whose answer may ask for it.
    pub from: NodeName,
    /// The node the work goes back to, which runs again.
    pub to: NodeName,
    /// How many times this one may be taken in a run.
    pub cap: LoopCap,
}

/// Everything a graph is made of, before anything has checked that it holds together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunGraphParts {
    /// What the graph is spelled by.
    pub id: GraphId,
    /// What it is called.
    pub name: GraphName,
    /// Whose it is.
    pub owner: GraphOwner,
    /// The nodes it holds.
    pub nodes: Vec<GraphNode>,
    /// The edges between them.
    pub edges: Vec<GraphEdge>,
    /// The loop backs it declares, where it declares any.
    pub loops: Vec<LoopBack>,
    /// The most nodes it may hold.
    pub max_nodes: MaxNodes,
}

/// A graph of templates to run, whose own shape holds together.
///
/// Only [`RunGraph::checked`] builds one, so every value of this type has passed the refusals
/// recorded there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunGraph {
    id: GraphId,
    name: GraphName,
    owner: GraphOwner,
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    loops: Vec<LoopBack>,
    max_nodes: MaxNodes,
}

impl RunGraph {
    /// A graph, if its own shape holds together.
    ///
    /// What is refused here is local to the graph: a graph with no node, more nodes than its own
    /// ceiling, one node name declared twice, one edge declared twice, an edge naming a node the
    /// graph does not hold, one loop back declared twice, and a loop back naming such a node.
    ///
    /// **Acyclicity is not among them, and neither is the run order.** Both are the plane's to
    /// answer — `plane.algo` `cycles` when a graph is written and `toposort` when one is read — so
    /// that the one graph algorithm this crate carries is none. A node naming no template is not
    /// checked either: [`AgentId`] refuses an empty id, so a node without one cannot be built in
    /// the first place.
    ///
    /// **Two things about a loop back are deliberately not refused here.** A node may declare one
    /// on itself, which is a second pass over its own work and the commonest use of the thing;
    /// unlike a self-edge it closes no cycle in the feeding edges, so nothing downstream has to
    /// cope with it. And whether the asking node even descends from its target is reachability,
    /// which this crate does not carry — the plane refuses a loop back that points sideways or
    /// forwards, when the graph is written.
    pub fn checked(parts: RunGraphParts) -> Result<Self, GraphError> {
        let RunGraphParts {
            id,
            name,
            owner,
            nodes,
            edges,
            loops,
            max_nodes,
        } = parts;
        if nodes.is_empty() {
            return Err(GraphError::NoNodes);
        }
        if nodes.len() > max_nodes.get() as usize {
            return Err(GraphError::TooManyNodes {
                count: nodes.len(),
                max: max_nodes.get(),
            });
        }
        let mut held = BTreeSet::new();
        for node in &nodes {
            if !held.insert(&node.name) {
                return Err(GraphError::DuplicateNode {
                    name: node.name.clone(),
                });
            }
        }
        let mut declared = BTreeSet::new();
        for edge in &edges {
            for end in [&edge.from, &edge.to] {
                if !held.contains(end) {
                    return Err(GraphError::UnknownNode { name: end.clone() });
                }
            }
            if !declared.insert((&edge.from, &edge.to)) {
                return Err(GraphError::DuplicateEdge {
                    from: edge.from.clone(),
                    to: edge.to.clone(),
                });
            }
        }
        let mut backwards = BTreeSet::new();
        for loop_back in &loops {
            for end in [&loop_back.from, &loop_back.to] {
                if !held.contains(end) {
                    return Err(GraphError::UnknownNode { name: end.clone() });
                }
            }
            if !backwards.insert((&loop_back.from, &loop_back.to)) {
                return Err(GraphError::DuplicateLoop {
                    from: loop_back.from.clone(),
                    to: loop_back.to.clone(),
                });
            }
        }
        Ok(Self {
            id,
            name,
            owner,
            nodes,
            edges,
            loops,
            max_nodes,
        })
    }

    /// What the graph is spelled by.
    pub fn id(&self) -> &GraphId {
        &self.id
    }

    /// What it is called.
    pub fn name(&self) -> &GraphName {
        &self.name
    }

    /// Whose it is.
    pub fn owner(&self) -> &GraphOwner {
        &self.owner
    }

    /// The nodes it holds.
    pub fn nodes(&self) -> &[GraphNode] {
        &self.nodes
    }

    /// The edges between them.
    pub fn edges(&self) -> &[GraphEdge] {
        &self.edges
    }

    /// The loop backs it declares.
    pub fn loops(&self) -> &[LoopBack] {
        &self.loops
    }

    /// The loop back one node declares to another, where it declares one.
    ///
    /// This is the question a run asks of a node that has asked to double back: a graph that did
    /// not declare it is a graph where it may not happen, however the node phrased it.
    pub fn loop_back(&self, from: &NodeName, to: &NodeName) -> Option<&LoopBack> {
        self.loops
            .iter()
            .find(|back| &back.from == from && &back.to == to)
    }

    /// The most nodes it may hold.
    pub fn max_nodes(&self) -> MaxNodes {
        self.max_nodes
    }

    /// The nodes whose answers feed one node, which are the keys its input arrives under.
    pub fn parents_of<'graph>(
        &'graph self,
        name: &'graph NodeName,
    ) -> impl Iterator<Item = &'graph NodeName> {
        self.edges
            .iter()
            .filter(move |edge| &edge.to == name)
            .map(|edge| &edge.from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(raw: &str) -> NodeName {
        NodeName::new(raw).expect("a name a node may carry")
    }

    fn node(raw: &str) -> GraphNode {
        GraphNode {
            name: name(raw),
            agent: AgentId::new(raw).expect("a template id"),
        }
    }

    fn edge(from: &str, to: &str) -> GraphEdge {
        GraphEdge {
            from: name(from),
            to: name(to),
        }
    }

    fn back(from: &str, to: &str, cap: u32) -> LoopBack {
        LoopBack {
            from: name(from),
            to: name(to),
            cap: LoopCap::new(cap).expect("a cap"),
        }
    }

    fn parts(nodes: Vec<GraphNode>, edges: Vec<GraphEdge>) -> RunGraphParts {
        RunGraphParts {
            id: GraphId::new("triage").expect("a graph id"),
            name: GraphName::new("Support triage").expect("a graph name"),
            owner: GraphOwner::Tenant(TenantId::new("acme").expect("a tenant id")),
            nodes,
            edges,
            loops: Vec::new(),
            max_nodes: MaxNodes::new(8).expect("a ceiling"),
        }
    }

    /// A graph of three in a chain, with a loop back from the last node to the first.
    fn looping(loops: Vec<LoopBack>) -> RunGraphParts {
        let mut parts = parts(
            vec![node("intake"), node("triage"), node("reply")],
            vec![edge("intake", "triage"), edge("triage", "reply")],
        );
        parts.loops = loops;
        parts
    }

    #[test]
    fn a_graph_whose_shape_holds_together_is_taken() {
        let graph = RunGraph::checked(parts(
            vec![node("intake"), node("triage"), node("reply")],
            vec![edge("intake", "triage"), edge("triage", "reply")],
        ))
        .expect("a chain of three");
        assert_eq!(graph.nodes().len(), 3);
        assert_eq!(graph.edges().len(), 2);
        assert_eq!(graph.id().as_str(), "triage");
        assert_eq!(graph.name().as_str(), "Support triage");
        assert_eq!(graph.max_nodes().get(), 8);
    }

    #[test]
    fn a_graph_with_no_node_is_refused() {
        assert_eq!(
            RunGraph::checked(parts(Vec::new(), Vec::new())),
            Err(GraphError::NoNodes)
        );
    }

    #[test]
    fn a_graph_over_its_own_ceiling_is_refused() {
        let mut over = parts(vec![node("a"), node("b"), node("c")], Vec::new());
        over.max_nodes = MaxNodes::new(2).expect("a ceiling of two");
        assert_eq!(
            RunGraph::checked(over),
            Err(GraphError::TooManyNodes { count: 3, max: 2 })
        );
    }

    #[test]
    fn one_name_declared_twice_is_refused() {
        assert_eq!(
            RunGraph::checked(parts(vec![node("a"), node("b"), node("a")], Vec::new())),
            Err(GraphError::DuplicateNode { name: name("a") })
        );
    }

    #[test]
    fn one_edge_declared_twice_is_refused() {
        assert_eq!(
            RunGraph::checked(parts(
                vec![node("a"), node("b")],
                vec![edge("a", "b"), edge("a", "b")],
            )),
            Err(GraphError::DuplicateEdge {
                from: name("a"),
                to: name("b"),
            })
        );
    }

    /// Both ends, because an edge out of nothing and an edge into nothing are the same mistake
    /// and only one of them is the first thing a loop looks at.
    #[test]
    fn an_edge_naming_a_node_the_graph_does_not_hold_is_refused() {
        for (from, to, missing) in [("a", "z", "z"), ("z", "a", "z")] {
            assert_eq!(
                RunGraph::checked(parts(vec![node("a")], vec![edge(from, to)])),
                Err(GraphError::UnknownNode {
                    name: name(missing),
                }),
                "the edge {from} to {to} was taken"
            );
        }
    }

    /// Recorded as a decision rather than an oversight: a self-edge is a cycle, and a cycle is
    /// the plane's to find.
    #[test]
    fn a_node_feeding_itself_is_left_for_the_plane_to_refuse() {
        assert!(
            RunGraph::checked(parts(vec![node("a")], vec![edge("a", "a")])).is_ok(),
            "the shape check started looking for cycles"
        );
    }

    #[test]
    fn a_declared_loop_back_is_carried_and_can_be_looked_up() {
        let graph = RunGraph::checked(looping(vec![back("reply", "intake", 2)]))
            .expect("a chain that may double back");

        assert_eq!(graph.loops().len(), 1);
        assert_eq!(
            graph.loop_back(&name("reply"), &name("intake")).map(|back| back.cap.get()),
            Some(2)
        );
        assert_eq!(
            graph.loop_back(&name("intake"), &name("reply")),
            None,
            "a loop back was found in the direction it was not declared in"
        );
    }

    /// The point of a loop back being its own list: nothing that reads the edges sees it.
    #[test]
    fn a_loop_back_is_not_an_edge() {
        let graph = RunGraph::checked(looping(vec![back("reply", "intake", 2)]))
            .expect("a chain that may double back");

        assert_eq!(graph.edges().len(), 2, "a loop back was counted as an edge");
        let target = name("intake");
        let parents: Vec<&NodeName> = graph.parents_of(&target).collect();
        assert!(
            parents.is_empty(),
            "a loop back fed a node its input, as {parents:?}"
        );
    }

    /// Both ends, for the reason the same case on an edge tests both.
    #[test]
    fn a_loop_back_naming_a_node_the_graph_does_not_hold_is_refused() {
        for (from, to) in [("reply", "survey"), ("survey", "intake")] {
            assert_eq!(
                RunGraph::checked(looping(vec![back(from, to, 2)])),
                Err(GraphError::UnknownNode {
                    name: name("survey"),
                }),
                "the loop back {from} to {to} was taken"
            );
        }
    }

    #[test]
    fn one_loop_back_declared_twice_is_refused() {
        assert_eq!(
            RunGraph::checked(looping(vec![
                back("reply", "intake", 2),
                back("reply", "intake", 3),
            ])),
            Err(GraphError::DuplicateLoop {
                from: name("reply"),
                to: name("intake"),
            })
        );
    }

    /// Unlike a self-edge, which is a cycle: a node asking to read its own work again closes no
    /// loop in the feeding edges, so it is permitted here and needs nothing of the plane.
    #[test]
    fn a_node_may_declare_a_loop_back_on_itself() {
        let graph = RunGraph::checked(looping(vec![back("reply", "reply", 1)]))
            .expect("a node may think again");

        assert!(graph.loop_back(&name("reply"), &name("reply")).is_some());
    }

    #[test]
    fn a_cap_that_loops_never_or_too_often_is_refused() {
        for value in [0, 9] {
            assert_eq!(
                LoopCap::new(value),
                Err(GraphError::LoopCeiling { value, limit: 8 }),
                "a cap of {value} was taken"
            );
        }
        assert!(LoopCap::new(8).is_ok(), "the limit itself is allowed");
        assert!(LoopCap::new(1).is_ok(), "one pass back is allowed");
    }

    #[test]
    fn a_cap_is_read_from_json_through_its_own_rule() {
        assert_eq!(
            serde_json::from_str::<LoopCap>("2").expect("a cap"),
            LoopCap::new(2).expect("the same cap")
        );
        assert!(
            serde_json::from_str::<LoopCap>("0").is_err(),
            "json got past the rule the constructor applies"
        );
    }

    #[test]
    fn a_ceiling_that_runs_nothing_or_too_much_is_refused() {
        for value in [0, 257] {
            assert_eq!(
                MaxNodes::new(value),
                Err(GraphError::NodeCeiling { value, limit: 256 }),
                "a ceiling of {value} was taken"
            );
        }
        assert!(MaxNodes::new(256).is_ok(), "the limit itself is allowed");
        assert!(MaxNodes::new(1).is_ok(), "a graph of one node is allowed");
    }

    #[test]
    fn a_ceiling_is_read_from_json_through_its_own_rule() {
        assert_eq!(
            serde_json::from_str::<MaxNodes>("8").expect("a ceiling"),
            MaxNodes::new(8).expect("the same ceiling")
        );
        assert!(
            serde_json::from_str::<MaxNodes>("0").is_err(),
            "json got past the rule the constructor applies"
        );
    }

    #[test]
    fn an_owner_is_recorded_as_the_plane_will_filter_on_it() {
        let tenant = TenantId::new("acme").expect("a tenant id");
        let theirs = GraphOwner::Tenant(tenant.clone());
        assert_eq!(theirs.written(), "acme");
        assert_eq!(theirs.tenant(), Some(&tenant));
        assert_eq!(GraphOwner::Global.written(), "global");
        assert_eq!(GraphOwner::Global.tenant(), None);
    }

    /// Both halves, since whose a graph is decides which templates a machine may read.
    #[test]
    fn an_owner_survives_being_written_down_and_read_back() {
        for owner in [
            GraphOwner::Global,
            GraphOwner::Tenant(TenantId::new("acme").expect("a tenant id")),
        ] {
            let written = serde_json::to_string(&owner).expect("an owner writes down");
            assert_eq!(
                serde_json::from_str::<GraphOwner>(&written).expect("an owner reads back"),
                owner,
                "{written} did not come back as what it went out as"
            );
        }
        assert_eq!(
            serde_json::to_string(&GraphOwner::Global).expect("an owner writes down"),
            "\"global\""
        );
    }

    /// A tenant id is read back through its own rule, wherever it arrived from.
    #[test]
    fn an_owner_naming_a_tenant_that_is_not_one_is_refused() {
        assert!(
            serde_json::from_str::<GraphOwner>(r#"{"tenant": "two words"}"#).is_err(),
            "json got past the rule the constructor applies"
        );
    }
}
