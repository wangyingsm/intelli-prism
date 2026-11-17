//! Run graphs.
//!
//! A run graph is a DAG of agent templates where one node's answer is the next node's input. This
//! crate holds what such a graph *is* and what makes one invalid, and keeps that apart from
//! everything that touches a wire: [`RunGraph`], [`RunOrder`] and the names they are spelled by
//! take no i/o, so what a graph may say can be argued with in a unit test rather than through a
//! run. [`GraphPlane`] is the one part that talks to anything, and it only maps those types onto
//! the plane the graphs are kept in.
//!
//! **One kind of code is deliberately absent: nothing here looks for a cycle or works out a run
//! order.** The plane the graph is kept in answers both, with `plane.algo`'s `cycles` when a graph
//! is written and `toposort` when one is read. What this crate carries is the types those answers
//! arrive as — [`Generation`], [`Placed`], [`RunOrder`] — and [`RunOrder::ready_after`], which
//! filters an order it was handed rather than traversing a graph itself.

#![forbid(unsafe_code)]

mod error;
mod event;
mod graph;
mod id;
mod member;
mod order;
mod placement;
mod plane;
mod run;

pub use error::GraphError;
pub use event::{GraphEvent, GraphReporting, GraphSaid, LoopRefusal};
pub use graph::{
    GraphEdge, GraphNode, GraphOwner, LoopBack, LoopCap, MaxNodes, RunGraph, RunGraphParts,
};
pub use id::{GraphId, GraphName, GraphRunId, NodeName};
pub use member::ClusterMember;
pub use order::{Generation, Placed, RunOrder};
pub use placement::Placement;
pub use plane::{FEEDS, GRAPH_LABEL, GraphPlane, GraphSummary, Listing, NODE_LABEL};
pub use run::{
    AGAIN_KEY, Downstream, GRAPH_INPUT_KEY, GraphRan, Graphing, LOOP_BACK_KEY, NodeRan,
    RunningNode, RunsNode, Stopping, run,
};
