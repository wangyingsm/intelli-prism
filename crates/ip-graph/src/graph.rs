//! What a run graph is, and what makes one invalid.

use std::collections::BTreeSet;

use ip_agents::AgentId;
use ip_core::TenantId;

use crate::error::GraphError;
use crate::id::{GraphId, GraphName, NodeName};

/// The most nodes a graph's own ceiling may allow.
const NODES_MOST: u32 = MaxNodes::MOST;

/// Whose a graph is: the deployment's, or one tenant's.
///
/// Mirrors `ip-storage`'s `AgentOwner` rather than sharing it. This crate takes no i/o, and a type
/// from the storage crate here would make a database part of what a graph is.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    max_nodes: MaxNodes,
}

impl RunGraph {
    /// A graph, if its own shape holds together.
    ///
    /// What is refused here is local to the graph: a graph with no node, more nodes than its own
    /// ceiling, one node name declared twice, one edge declared twice, and an edge naming a node
    /// the graph does not hold.
    ///
    /// **Acyclicity is not among them, and neither is the run order.** Both are the plane's to
    /// answer — `plane.algo` `cycles` when a graph is written and `toposort` when one is read — so
    /// that the one graph algorithm this crate carries is none. A node naming no template is not
    /// checked either: [`AgentId`] refuses an empty id, so a node without one cannot be built in
    /// the first place.
    pub fn checked(parts: RunGraphParts) -> Result<Self, GraphError> {
        let RunGraphParts {
            id,
            name,
            owner,
            nodes,
            edges,
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
        Ok(Self {
            id,
            name,
            owner,
            nodes,
            edges,
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

    fn parts(nodes: Vec<GraphNode>, edges: Vec<GraphEdge>) -> RunGraphParts {
        RunGraphParts {
            id: GraphId::new("triage").expect("a graph id"),
            name: GraphName::new("Support triage").expect("a graph name"),
            owner: GraphOwner::Tenant(TenantId::new("acme").expect("a tenant id")),
            nodes,
            edges,
            max_nodes: MaxNodes::new(8).expect("a ceiling"),
        }
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
}
