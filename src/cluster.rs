//! Telling the rest of this cluster that this machine is here.
//!
//! A member registers itself and keeps saying so. Nothing else records a member, because nothing
//! else knows one is there; what a reader makes of a row that has stopped being written again is
//! the reader's own decision, which [`ip_graph::ClusterMember`] records and this does not.

use std::sync::Arc;
use std::time::Duration;

use ip_core::Timestamp;
use ip_core::route::ClusterAddress;
use ip_graph::{ClusterMember, GraphPlane};
use tokio::task::JoinHandle;

/// This machine's own membership of the cluster.
pub struct Membership {
    /// Where the row is kept.
    plane: Arc<GraphPlane>,
    /// The address the row names, which is this deployment's own.
    address: ClusterAddress,
    /// How often it is written again.
    every: Duration,
}

impl Membership {
    /// The membership this deployment keeps, where it keeps one at all.
    ///
    /// Nothing is registered by a deployment that cannot say where it answers, or that keeps no
    /// graphs: the same pair a placement needs, for the same reason — an address nobody can dial
    /// is worse than no row, and a plane that does not exist cannot hold one.
    pub fn of(
        address: Option<&ClusterAddress>,
        plane: Option<&Arc<GraphPlane>>,
        every: Duration,
    ) -> Option<Self> {
        address.zip(plane).map(|(address, plane)| Self {
            plane: Arc::clone(plane),
            address: address.clone(),
            every,
        })
    }

    /// Says this machine is here, now.
    ///
    /// **Best effort.** A registration that cannot be written warns and the server serves on: a
    /// member nobody can see is a degraded cluster, where refusing to serve over it would make
    /// the plane a dependency of answering an ordinary request.
    pub async fn register(&self) {
        let member = ClusterMember {
            address: self.address.clone(),
            at: Timestamp::now(),
        };
        if let Err(error) = self.plane.joined(&member).await {
            tracing::warn!(%error, address = %self.address, "could not say this machine is here");
        }
    }

    /// Takes this machine's row away, on the way out.
    ///
    /// Best effort for the same reason. A row left behind says a member is here that is not,
    /// which is what a reader's own staleness rule is for and what a machine that died leaves
    /// anyway.
    pub async fn going(&self) {
        if let Err(error) = self.plane.left(&self.address).await {
            tracing::warn!(%error, address = %self.address, "could not say this machine is going");
        }
    }

    /// Registers now, and again every interval for as long as the server runs.
    ///
    /// Now rather than after the first wait: a member invisible for its first interval is one
    /// nothing dispatches to, for no reason.
    pub fn keep_registering(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                self.register().await;
                tokio::time::sleep(self.every).await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use ip_drsg::{Replies, StubPlane};

    use super::*;

    fn address() -> ClusterAddress {
        "10.0.0.2:8080".parse().expect("an address")
    }

    /// A membership over a plane that answers however `replies` says.
    async fn membership(replies: Replies) -> (Membership, StubPlane) {
        let plane = StubPlane::start(replies).await;
        let member = Membership::of(
            Some(&address()),
            Some(&Arc::new(GraphPlane::new(plane.client()))),
            Duration::from_secs(10),
        )
        .expect("an address and a plane are both here");
        (member, plane)
    }

    #[tokio::test]
    async fn a_member_says_where_it_answers_and_when_it_said_so() {
        let (membership, plane) = membership(Replies::Echo).await;

        membership.register().await;

        let seen = plane.seen();
        let written = seen
            .iter()
            .find(|one| one.method == "node.create")
            .expect("the row is written");
        assert_eq!(written.params["properties"]["address"], "10.0.0.2:8080");
        assert!(
            written.params["properties"]["at"]
                .as_i64()
                .is_some_and(|at| at > 1_700_000_000),
            "the row says when, and a time this side of the present"
        );
    }

    #[tokio::test]
    async fn a_member_going_takes_its_row_away() {
        let (membership, plane) = membership(Replies::Deleted).await;

        membership.going().await;

        let seen = plane.only();
        assert_eq!(seen.method, "node.delete");
        assert!(
            seen.params["key"]
                .as_str()
                .is_some_and(|key| key.ends_with("10.0.0.2:8080")),
            "the row taken away is this machine's own"
        );
    }

    /// Neither half of a heartbeat may refuse: a plane that is down is not a reason to stop.
    #[tokio::test]
    async fn a_plane_that_refuses_everything_stops_neither_registering_nor_going() {
        let (membership, plane) = membership(Replies::Failure).await;

        membership.register().await;
        membership.going().await;

        assert!(
            plane.seen().len() >= 2,
            "both were attempted and neither was fatal"
        );
    }

    #[tokio::test]
    async fn a_deployment_missing_either_half_keeps_no_membership() {
        let plane = StubPlane::start(Replies::Echo).await;
        let kept = Arc::new(GraphPlane::new(plane.client()));
        let every = Duration::from_secs(10);

        assert!(
            Membership::of(None, Some(&kept), every).is_none(),
            "no address"
        );
        assert!(
            Membership::of(Some(&address()), None, every).is_none(),
            "no plane"
        );
        assert!(Membership::of(Some(&address()), Some(&kept), every).is_some());
    }

    /// A member invisible for its first interval is one nothing dispatches to, for no reason.
    #[tokio::test]
    async fn the_loop_says_so_at_once_rather_than_after_the_first_wait() {
        let plane = StubPlane::start(Replies::Echo).await;
        let membership = Membership::of(
            Some(&address()),
            Some(&Arc::new(GraphPlane::new(plane.client()))),
            Duration::from_secs(600),
        )
        .expect("both halves");

        let registering = Arc::new(membership).keep_registering();
        tokio::time::sleep(Duration::from_millis(80)).await;
        registering.abort();

        assert!(
            plane.seen().iter().any(|one| one.method == "node.create"),
            "nothing was written inside a six hundred second interval"
        );
    }

    /// The loop writes again rather than once, which is the whole of what makes a row a heartbeat.
    #[tokio::test]
    async fn the_loop_says_so_again_on_every_interval() {
        let plane = StubPlane::start(Replies::Echo).await;
        let membership = Membership::of(
            Some(&address()),
            Some(&Arc::new(GraphPlane::new(plane.client()))),
            Duration::from_millis(20),
        )
        .expect("both halves");

        let registering = Arc::new(membership).keep_registering();
        tokio::time::sleep(Duration::from_millis(120)).await;
        registering.abort();

        let written = plane
            .seen()
            .iter()
            .filter(|one| one.method == "node.create")
            .count();
        assert!(written >= 2, "wrote {written} times in six intervals");
    }
}
