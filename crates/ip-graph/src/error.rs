//! Every way a graph, or an order over one, can be refused.

use crate::id::NodeName;

/// Every way a graph can fail to be one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GraphError {
    /// A name or an id that is empty.
    #[error("{kind} is empty")]
    Empty {
        /// What was being read.
        kind: &'static str,
    },

    /// A name or an id longer than its own kind allows.
    #[error("{kind} is {len} bytes, over the {max}")]
    TooLong {
        /// What was being read.
        kind: &'static str,
        /// How long it is.
        len: usize,
        /// The most it may be.
        max: usize,
    },

    /// A name or an id holding a character its own kind refuses.
    #[error("{kind} contains the illegal character {ch:?}")]
    IllegalChar {
        /// What was being read.
        kind: &'static str,
        /// The character it may not hold.
        ch: char,
    },

    /// A name with space at either end, refused rather than trimmed.
    #[error("{kind} has space at one end")]
    Untrimmed {
        /// What was being read.
        kind: &'static str,
    },

    /// A graph holding no node, for which there is nothing to run.
    #[error("a graph holds at least one node")]
    NoNodes,

    /// A graph holding more nodes than its own ceiling allows.
    #[error("a graph of {count} nodes is over its ceiling of {max}")]
    TooManyNodes {
        /// How many it holds.
        count: usize,
        /// The most it said it may.
        max: u32,
    },

    /// A ceiling that lets a graph hold nothing, or more than one run builds for.
    #[error("a graph's ceiling is between 1 and {limit} nodes, not {value}")]
    NodeCeiling {
        /// What was asked for.
        value: u32,
        /// The most that may be asked for.
        limit: u32,
    },

    /// Two nodes of one graph spelled the same, so an edge could not name one of them.
    #[error("the node {name} is declared more than once")]
    DuplicateNode {
        /// The name declared twice.
        name: NodeName,
    },

    /// One edge declared twice, which would feed a node one answer under one key twice.
    #[error("the edge from {from} to {to} is declared more than once")]
    DuplicateEdge {
        /// Whose answer it carries.
        from: NodeName,
        /// Which node receives it.
        to: NodeName,
    },

    /// An edge naming a node the graph does not hold.
    #[error("an edge names the node {name}, which the graph does not hold")]
    UnknownNode {
        /// The name the edge used.
        name: NodeName,
    },

    /// An order placing one node twice, so which generation it runs in has two answers.
    #[error("the order places the node {name} more than once")]
    DuplicatePlacement {
        /// The name placed twice.
        name: NodeName,
    },
}
