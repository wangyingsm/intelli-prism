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

    /// A cap that permits no loop at all, or more of them than a run is built for.
    #[error("a loop back may be taken between 1 and {limit} times, not {value}")]
    LoopCeiling {
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

    /// One loop back declared twice, which would give one of them two caps.
    #[error("the loop back from {from} to {to} is declared more than once")]
    DuplicateLoop {
        /// The node that may ask for it.
        from: NodeName,
        /// The node it sends the work back to.
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

    /// The plane the graphs are kept in could not be reached, or would not answer.
    #[error(transparent)]
    Plane(#[from] ip_drsg::DrsgError),

    /// A stored row carries nothing under a property a graph is rebuilt from.
    #[error("a stored row carries no {property}")]
    PropertyMissing {
        /// What was being read.
        property: &'static str,
    },

    /// A stored row carries something under a property that is not what it must be.
    #[error("a stored row's {property} is not a value a graph can be rebuilt from")]
    PropertyKind {
        /// What was being read.
        property: &'static str,
    },

    /// A graph that feeds itself, which has no order to run it in.
    ///
    /// Found by the store rather than here, and only once the rows exist, so the graph is taken
    /// back out before this is reported: a refused write leaves the graph absent, not as it was.
    #[error("a graph cannot feed itself: {cycle}")]
    Cyclic {
        /// The nodes closing the loop, in the order they close it.
        cycle: String,
    },

    /// A loop back pointing somewhere the asking node's own work never came from.
    ///
    /// Found by the store, as a cycle is, and for the same reason: whether one node's answer
    /// reaches another is reachability, which this crate does not walk.
    #[error("the node {from} cannot send work back to {to}, which does not feed it")]
    LoopNotBackwards {
        /// The node that would have asked.
        from: NodeName,
        /// The node it named.
        to: NodeName,
    },

    /// The store answered with fewer places in the order than it said the graph has.
    ///
    /// Its own answers are capped, and a truncated order reads exactly like a complete one, so it
    /// is refused: half an order would run half a graph and call it finished.
    #[error("the graph store ordered {answered} of {held} nodes")]
    Truncated {
        /// How many it said there were.
        held: u64,
        /// How many it placed.
        answered: usize,
    },

    /// The order places a node the graph does not hold, so nothing can be run for it.
    ///
    /// Reachable where a graph changed between the sort and the read of its nodes.
    #[error("the order places a node this graph does not hold")]
    Unplaced,

    /// The plane answered with a row belonging to another graph, or to another owner.
    ///
    /// Reachable only where the filter in the read plan did not hold, which is why it is reported
    /// rather than skipped: rows that should not have arrived mean the read asked wrongly.
    #[error("the graph store answered with a row of another {property}")]
    Foreign {
        /// Which of the two did not match: the graph, or its owner.
        property: &'static str,
    },

    /// The plane answered with a shape this cannot read as a graph.
    #[error("the graph store answered with {detail}")]
    Unreadable {
        /// What was wrong with it.
        detail: String,
    },
    /// Text that is not the hex a run id is written as.
    #[error("{value:?} is not a run id")]
    NotARunId {
        /// What was read.
        value: String,
    },
}
