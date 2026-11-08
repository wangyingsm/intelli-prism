//! Run graphs.
//!
//! A run graph is a DAG of agent templates where one node's answer is the next node's input. This
//! crate holds what such a graph *is* and what makes one invalid, and takes no i/o at all — no
//! plane, no store, no network — so what a graph may say can be argued with in a unit test rather
//! than through a run.
//!
//! **One kind of code is deliberately absent: nothing here looks for a cycle or works out a run
//! order.** The plane the graph is kept in answers both, with `plane.algo`'s `cycles` when a graph
//! is written and `toposort` when one is read. What this crate carries is the types those answers
//! arrive as — [`Generation`], [`Placed`], [`RunOrder`] — and [`RunOrder::ready_after`], which
//! filters an order it was handed rather than traversing a graph itself.

#![forbid(unsafe_code)]

mod error;
mod graph;
mod id;
mod order;

pub use error::GraphError;
pub use graph::{GraphEdge, GraphNode, GraphOwner, MaxNodes, RunGraph, RunGraphParts};
pub use id::{GraphId, GraphName, NodeName};
pub use order::{Generation, Placed, RunOrder};
