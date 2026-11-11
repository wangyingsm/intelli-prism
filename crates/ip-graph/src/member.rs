//! Which machines are in this cluster, and when each last said so.
//!
//! Membership is not a graph, and it lives in this crate because the plane does: a member's row
//! sits beside the placements, in the one store every member can read and none of them owns. If a
//! later milestone gives cluster state a home of its own, this and [`crate::Placement`] move to it
//! together.

use ip_core::Timestamp;
use ip_core::route::ClusterAddress;

/// One member of the cluster, as it last registered itself.
///
/// There is no state here and no liveness flag. A row exists because a member wrote it, and how
/// long ago it wrote is the whole of what a reader needs: a member that stops writing goes stale
/// where it stands, which is what a machine that died looks like from outside. The same decision
/// [`crate::Placement`] records for a node, for the same reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterMember {
    /// Where it answers the rest of the cluster.
    pub address: ClusterAddress,
    /// When it last said it was here.
    pub at: Timestamp,
}
