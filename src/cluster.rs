//! Telling the rest of this cluster that this machine is here.
//!
//! A member registers itself and keeps saying so. Nothing else records a member, because nothing
//! else knows one is there; what a reader makes of a row that has stopped being written again is
//! the reader's own decision, which [`ip_graph::ClusterMember`] records and this does not.
//!
//! The other half of that decision is here too: [`Spreading`] picks which of the members a node
//! is dispatched to. It is in this module and not in the route because it is a property of the
//! deployment rather than of one run — round-robin's place in the order is shared by every run
//! this machine serves.
//!
//! And [`Signalling`] carries a graph run's stop to the members running its nodes, over the
//! publication channel the rules have used on both cache backends since M6. Both ends of that
//! channel are here — what a coordinator publishes and what a worker follows — so the topic is
//! named once and the two cannot drift apart.
//!
//! [`Kept`] is the other thing this module puts in the cache: what a node answered, under the run
//! and the node that answered it, so a coordinator picking up a run somebody else started can see
//! what has already been paid for instead of paying for it again.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ip_cache::{CacheBackend, CacheError, CacheKey, CacheLevel, Ttl};
use ip_core::Timestamp;
use ip_core::route::{ClusterAddress, ClusterStrategy};
use ip_graph::{ClusterMember, GraphPlane, GraphRunId, NodeName, NodeRan};
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

/// How this deployment picks the member a node is dispatched to.
///
/// One of these per deployment, shared by every run, because what round-robin remembers is where
/// *this machine* got to in the order and not where one run did.
pub struct Spreading {
    /// Which way a member is picked.
    strategy: ClusterStrategy,
    /// Where round-robin has got to. Only read by that strategy, and wraps rather than growing
    /// without bound, since what is taken from it is a remainder.
    turn: AtomicUsize,
}

impl Spreading {
    /// Picking members the way this deployment was configured to.
    #[must_use]
    pub const fn new(strategy: ClusterStrategy) -> Self {
        Self {
            strategy,
            turn: AtomicUsize::new(0),
        }
    }

    /// One of `live`, or nothing when there is nobody to pick.
    ///
    /// Takes members already filtered for liveness, so what each strategy decides is only *which*
    /// of them and never *whether*: a coordinator that sends a node to a member it believes is
    /// gone has made two decisions where it should have made one.
    ///
    /// The coordinator itself is one of `live`, and a strategy knows nothing of that: picked for
    /// its own node it runs the node rather than dispatching it, which is the caller's business
    /// and not a strategy's.
    #[must_use]
    pub fn pick(&self, live: &[ClusterMember]) -> Option<ClusterAddress> {
        if live.is_empty() {
            return None;
        }
        match self.strategy {
            ClusterStrategy::Random => {
                let total = u64::try_from(live.len()).unwrap_or(1);
                let at = usize::try_from(roll(total)).unwrap_or(0);
                live.get(at).map(|member| member.address.clone())
            }
            ClusterStrategy::RoundRobin => {
                let mut ordered: Vec<&ClusterAddress> =
                    live.iter().map(|member| &member.address).collect();
                // An order every member agrees on, so two coordinators walking the same cluster
                // walk it the same way. By how an address prints, which is the one total order
                // it has without giving the type an `Ord` nothing else asked for.
                ordered.sort_unstable_by_key(|address| address.to_string());
                let at = self.turn.fetch_add(1, Ordering::Relaxed) % ordered.len();
                ordered.get(at).map(|&address| address.clone())
            }
        }
    }
}

/// The topic one graph run's signals are published under.
///
/// Per run, so a stop reaches the members running that run's nodes and nobody else — which the
/// publication channel already guarantees, since a follower hears only its own topic.
///
/// `System` rather than a tenant's level because this is control traffic between the machines of
/// one deployment, like the rules: which tenant's run it concerns is in the key and is not what
/// decides who may read it.
///
/// # Errors
///
/// [`CacheError`] when the run's own name will not make a key, which a hex id cannot.
pub fn signal_topic(run: GraphRunId) -> Result<CacheKey, CacheError> {
    CacheKey::new(CacheLevel::System, &format!("graph-run/{run}/signal"))
}

/// What is published under that topic when a run stops.
///
/// Written for whoever reads the cache by hand rather than read back: the topic carries one kind
/// of thing, so a revision above where a follower started *is* the stop. A second kind of signal
/// is what would make reading the value worth the second call it costs.
const STOP: &[u8] = b"stop";

/// The revision a stop is published at.
///
/// One, because there is one transition: a run that has stopped does not un-stop, and a
/// publication lands only when its revision is newer than what is there — so saying it twice is
/// idempotent rather than a second announcement.
const STOPPED_AT: u64 = 1;

/// Carries a graph run's stop to the members of this cluster running its nodes.
pub struct Signalling {
    /// Where the topic lives.
    cache: Arc<dyn CacheBackend>,
}

impl Signalling {
    /// Signalling over this deployment's own cache.
    #[must_use]
    pub const fn new(cache: Arc<dyn CacheBackend>) -> Self {
        Self { cache }
    }
}

#[async_trait::async_trait]
impl ip_graph::Stopping for Signalling {
    /// **Best effort**, as every other cluster write here is.
    ///
    /// A stop that cannot be published is a warning: the node it would have cut short is already
    /// paid for, the run ends on this machine either way, and refusing over it would turn a cache
    /// that is down into graph runs that cannot finish.
    async fn stopped(&self, run: GraphRunId) {
        let topic = match signal_topic(run) {
            Ok(topic) => topic,
            Err(error) => {
                tracing::warn!(%error, %run, "a graph run has no signal topic");
                return;
            }
        };
        if let Err(error) = self.cache.publish(&topic, STOPPED_AT, STOP).await {
            tracing::warn!(
                %error, %run,
                "could not tell this cluster a run stopped; its nodes elsewhere run their course"
            );
        }
    }
}

/// Whether this run has already been told to stop.
///
/// Read before a dispatched node runs, because [`ip_cache::Publication::follow`] starts a
/// follower at the revision standing *now* and wakes it only on what comes after: a node handed
/// to a member after the stop was published has nothing to wake on and would otherwise run.
pub async fn already_stopped(cache: &dyn CacheBackend, run: GraphRunId) -> bool {
    let Ok(topic) = signal_topic(run) else {
        return false;
    };
    cache
        .published(&topic)
        .await
        .is_ok_and(|published| published.is_some_and(|published| published.revision >= STOPPED_AT))
}

/// Where one node's answer is kept.
///
/// Under the run *and* the node, because a graph runs each node once: the pair is what a second
/// coordinator of the same run would ask by, and it is the only pair that identifies the work.
///
/// # Errors
///
/// [`CacheError`] when the pair will not make a key, which a hex id and a node name cannot — a
/// node name is checked when the graph is stored.
pub fn answer_key(run: GraphRunId, node: &NodeName) -> Result<CacheKey, CacheError> {
    CacheKey::new(CacheLevel::System, &format!("graph-run/{run}/node/{node}"))
}

/// How long a node's answer is worth keeping.
///
/// A day, which is a statement about retries and not about the answer: a coordinator picking the
/// run up long after this is not retrying it, and an answer nobody will ask for again is a row
/// that should fall out of the cache rather than one somebody has to sweep.
const KEPT_FOR: u64 = 86_400;

/// What a node answered, kept where another coordinator of the same run can find it.
///
/// This is what makes the placement records M26 leaves behind actionable. The records say a node
/// ran at a member; what it *answered* was only ever in the coordinator's memory, so a coordinator
/// that died took the answer with it and a retry paid the upstream twice for the same node.
pub struct Kept {
    /// Where the answers live.
    cache: Arc<dyn CacheBackend>,
}

impl Kept {
    /// Keeping answers in this deployment's own cache.
    #[must_use]
    pub const fn new(cache: Arc<dyn CacheBackend>) -> Self {
        Self { cache }
    }

    /// What this node already answered, where it has and the answer can still be read.
    ///
    /// **Best effort.** Nothing when the cache cannot answer, which runs the node — the cost of
    /// being wrong that way is one node run again, against a cache outage making every graph run
    /// refuse. An answer that will not read back as a `NodeRan` is treated the same way and said
    /// in a warning, since the only thing that writes this key is the line below.
    pub async fn answer(&self, run: GraphRunId, node: &NodeName) -> Option<NodeRan> {
        let kept = self.cache.get(&answer_key(run, node).ok()?).await;
        let held = match kept {
            Ok(held) => held?,
            Err(error) => {
                tracing::warn!(%error, %run, %node, "could not read what this node answered");
                return None;
            }
        };
        match serde_json::from_slice(&held) {
            Ok(ran) => Some(ran),
            Err(error) => {
                tracing::warn!(%error, %run, %node, "what this node answered will not read back");
                None
            }
        }
    }

    /// Keeps what this node answered, for a coordinator that asks after this one is gone.
    ///
    /// **Best effort**, and silent about a key or a ttl it cannot build: failing a run over an
    /// answer that could not be kept would trade a saving for an outage.
    pub async fn keep(&self, run: GraphRunId, node: &NodeName, ran: &NodeRan) {
        let Ok(key) = answer_key(run, node) else {
            return;
        };
        let Ok(value) = serde_json::to_vec(ran) else {
            return;
        };
        let ttl = Ttl::seconds(KEPT_FOR).ok();
        if let Err(error) = self.cache.put(&key, &value, ttl).await {
            tracing::warn!(%error, %run, %node, "could not keep what this node answered");
        }
    }
}

/// A number below `total`, drawn from the operating system entropy source.
///
/// Half of `total` when it will not answer, as the gateway's own roll does: a member picked
/// without spreading is a worse placement and not a failure, and refusing the node over it would
/// turn a missing entropy source into a run that cannot start.
fn roll(total: u64) -> u64 {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        return total / 2;
    }
    u64::from_le_bytes(bytes) % total
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

    /// A member at `address` that said so just now.
    fn member(address: &str) -> ClusterMember {
        ClusterMember {
            address: address.parse().expect("an address"),
            at: Timestamp::now(),
        }
    }

    /// Three members, handed over in an order the store happened to scan them in.
    fn three() -> Vec<ClusterMember> {
        vec![
            member("10.0.0.7:8080"),
            member("10.0.0.3:8080"),
            member("10.0.0.5:8080"),
        ]
    }

    #[test]
    fn a_cluster_with_nobody_live_in_it_offers_nobody() {
        for strategy in [ClusterStrategy::Random, ClusterStrategy::RoundRobin] {
            assert_eq!(
                Spreading::new(strategy).pick(&[]),
                None,
                "{strategy} picked somebody out of nothing"
            );
        }
    }

    #[test]
    fn a_cluster_of_one_live_member_offers_that_member_however_it_picks() {
        for strategy in [ClusterStrategy::Random, ClusterStrategy::RoundRobin] {
            let spreading = Spreading::new(strategy);
            for _ in 0..8 {
                assert_eq!(
                    spreading.pick(&[member("10.0.0.3:8080")]),
                    Some("10.0.0.3:8080".parse().expect("an address")),
                    "{strategy}"
                );
            }
        }
    }

    /// The point of the default: work does not pile onto one machine.
    ///
    /// Two hundred draws over three members, so a member left out would be a one in `3·(2/3)^200`
    /// accident rather than a flaky test — which is to say the assertion is about the code.
    #[test]
    fn random_offers_every_live_member_over_enough_draws() {
        let spreading = Spreading::new(ClusterStrategy::Random);
        let live = three();

        let mut picked: Vec<String> = (0..200)
            .filter_map(|_| spreading.pick(&live))
            .map(|address| address.to_string())
            .collect();
        picked.sort_unstable();
        picked.dedup();

        assert_eq!(
            picked,
            ["10.0.0.3:8080", "10.0.0.5:8080", "10.0.0.7:8080"],
            "a live member was never offered a node"
        );
    }

    /// Each member once before any of them twice, which is what round-robin is for.
    #[test]
    fn round_robin_offers_each_live_member_in_turn_and_then_wraps() {
        let spreading = Spreading::new(ClusterStrategy::RoundRobin);
        let live = three();

        let picked: Vec<String> = (0..4)
            .filter_map(|_| spreading.pick(&live))
            .map(|address| address.to_string())
            .collect();

        assert_eq!(
            picked,
            [
                "10.0.0.3:8080",
                "10.0.0.5:8080",
                "10.0.0.7:8080",
                "10.0.0.3:8080",
            ]
        );
    }

    /// The order is the members', not the store's.
    ///
    /// A scan answers rows in whatever order it holds them, and `members` sorts by recency, which
    /// changes with every heartbeat. Round-robin over that order would revisit a member it had
    /// just used, so it imposes one of its own that every coordinator computes the same way.
    #[test]
    fn round_robins_order_does_not_depend_on_the_order_the_members_arrive_in() {
        let forwards = Spreading::new(ClusterStrategy::RoundRobin);
        let backwards = Spreading::new(ClusterStrategy::RoundRobin);
        let live = three();
        let mut reversed = three();
        reversed.reverse();

        let one: Vec<_> = (0..3).filter_map(|_| forwards.pick(&live)).collect();
        let other: Vec<_> = (0..3).filter_map(|_| backwards.pick(&reversed)).collect();

        assert_eq!(one, other);
    }

    /// What round-robin remembers is this machine's place in the order, not one run's, so two
    /// runs served by one deployment carry on from each other rather than both starting over.
    #[test]
    fn round_robin_is_remembered_across_the_runs_one_deployment_serves() {
        let spreading = Spreading::new(ClusterStrategy::RoundRobin);
        let live = three();

        let first = spreading.pick(&live).expect("a member");
        let second = spreading.pick(&live).expect("a member");

        assert_ne!(
            first, second,
            "a second run was offered the member the first one used"
        );
    }

    /// A member joining or leaving shifts the order, which is round-robin's known cost and is
    /// recorded here so it is a decision rather than a surprise.
    #[test]
    fn a_member_joining_shifts_where_round_robin_is_in_the_order() {
        let spreading = Spreading::new(ClusterStrategy::RoundRobin);

        let before = spreading
            .pick(&[member("10.0.0.3:8080"), member("10.0.0.7:8080")])
            .expect("a member");
        let mut joined = three();
        joined.push(member("10.0.0.1:8080"));
        let after = spreading.pick(&joined).expect("a member");

        assert_eq!(before.to_string(), "10.0.0.3:8080");
        // The second pick of four members, in the new order, rather than the second of two.
        assert_eq!(after.to_string(), "10.0.0.3:8080");
    }
}
