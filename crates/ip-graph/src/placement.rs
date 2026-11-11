//! Where one node of one run is running.
//!
//! A dispatched node runs somewhere, and until it answers the only evidence of where is what the
//! coordinator wrote down. That record is what a cluster reads to say what it is doing now, and
//! what is left behind when a coordinator dies holding work already paid for.

use ip_core::Timestamp;
use ip_core::route::ClusterAddress;

use crate::graph::GraphOwner;
use crate::id::{GraphId, GraphRunId, NodeName};

/// Where one node of one run is running, as the coordinator records it.
///
/// There is no "finished" state to record: a record exists while the node does, and
/// [`crate::GraphPlane::finished`] takes it away. A row that outlives its run is therefore
/// evidence of a coordinator that died rather than a node that ended, which is the question this
/// record exists to answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// Which run it belongs to.
    pub run: GraphRunId,
    /// Which node of the graph it is.
    pub node: NodeName,
    /// Which graph that node is from.
    pub graph: GraphId,
    /// Whose graph it is, and so what every read of this record filters on.
    pub owner: GraphOwner,
    /// Where the machine running it answers the rest of its own cluster.
    pub address: ClusterAddress,
    /// When it was dispatched.
    pub at: Timestamp,
}
