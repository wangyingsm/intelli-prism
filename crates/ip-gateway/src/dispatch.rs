//! Choosing which endpoint behind a rule a request is sent to.
//!
//! Every count here is this node's own: no coordination between nodes, and nothing on the request
//! path but atomics. A cluster of like nodes spreads its traffic by each node spreading its own.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use ip_core::{Endpoint, Strategy, Weighted};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::table::Resolution;

/// What a latency no endpoint has answered yet counts as, in microseconds.
///
/// Not zero: a score of nothing would send every request to one endpoint nobody has tried,
/// where an untried endpoint should instead look like an idle one.
const UNKNOWN_LATENCY: u64 = 1_000;

/// One part in this many of a new latency replaces what an endpoint was taking before.
const SMOOTHING: u64 = 5;

/// How many answers are folded in before the folder looks for more.
const BATCH: usize = 128;

/// What one endpoint behind a rule is carrying.
///
/// Held beside the rule and read by index, so a dispatch decision costs no lock and no hashing.
#[derive(Debug, Default)]
struct EndpointLoad {
    /// Requests sent to it and not yet done with.
    in_flight: AtomicU32,
    /// What it has been taking, in microseconds, or zero until it has answered once.
    latency: AtomicU64,
}

impl EndpointLoad {
    /// What a request arriving now would wait: everything in front of it, at what this
    /// endpoint has been taking.
    fn score(&self) -> u64 {
        let latency = match self.latency.load(Ordering::Relaxed) {
            0 => UNKNOWN_LATENCY,
            latency => latency,
        };
        (u64::from(self.in_flight.load(Ordering::Relaxed)) + 1) * latency
    }

    /// Folds one answer's latency into what this endpoint takes.
    fn answered(&self, micros: u64) {
        let next = match self.latency.load(Ordering::Relaxed) {
            0 => micros,
            held => held - held / SMOOTHING + micros / SMOOTHING,
        };
        self.latency.store(next, Ordering::Relaxed);
    }
}

/// What a rule's endpoints look like to a dispatcher, and what they are carrying.
///
/// A rule changes far less often than it is used, so the live endpoints and the running total of
/// their shares are found once here rather than on the path of every request. The load sits here
/// too, one entry per endpoint by the same index, so reading it is an array index.
#[derive(Debug)]
pub struct Plan {
    /// Which endpoints a request may go to: those not drained, or every one when all are,
    /// since a rule must have somewhere to send a request.
    live: Vec<usize>,
    /// The running total of the live shares, so a draw is one search rather than a sum.
    shares: Vec<u64>,
    /// Where this rule's turn has reached. Its exact order between two requests does not
    /// matter, only that each takes a number of its own, so it is counted without a lock.
    turn: AtomicU64,
    /// What each endpoint is carrying, indexed as the rule's endpoints are.
    load: Vec<EndpointLoad>,
}

impl Plan {
    /// Works out how these endpoints are dispatched to.
    pub fn of(targets: &[Weighted]) -> Self {
        let mut live: Vec<usize> = targets
            .iter()
            .enumerate()
            .filter(|(_, target)| !target.weight.is_drained())
            .map(|(at, _)| at)
            .collect();
        if live.is_empty() {
            live = (0..targets.len()).collect();
        }
        let mut running = 0;
        let shares = live
            .iter()
            .map(|at| {
                running += u64::from(targets[*at].weight.get());
                running
            })
            .collect();
        Self {
            live,
            shares,
            turn: AtomicU64::new(0),
            load: targets.iter().map(|_| EndpointLoad::default()).collect(),
        }
    }

    /// The endpoints a request may go to.
    pub fn live(&self) -> &[usize] {
        &self.live
    }

    /// Every live share together, which is what a draw is made below.
    pub fn total(&self) -> u64 {
        self.shares.last().copied().unwrap_or(0)
    }

    /// The endpoint after the one this rule last used.
    pub fn in_turn(&self) -> usize {
        let turn = self.turn.fetch_add(1, Ordering::Relaxed);
        self.live[(turn % self.live.len() as u64) as usize]
    }

    /// The endpoint whose share `rolled` falls in.
    pub fn at_share(&self, rolled: u64) -> usize {
        let at = self.shares.partition_point(|running| *running <= rolled);
        self.live[at.min(self.live.len() - 1)]
    }

    /// The live endpoint a new request would wait least behind.
    ///
    /// A scan of the live endpoints' atomics: no lock, and the count it reads is the true one,
    /// so a burst of requests is spread rather than piled onto whoever was lightest a moment ago.
    pub fn lightest(&self) -> usize {
        let mut lightest = self.live[0];
        let mut least = u64::MAX;
        for at in &self.live {
            let score = self.load[*at].score();
            if score < least {
                least = score;
                lightest = *at;
            }
        }
        lightest
    }

    /// What this endpoint has been taking, in microseconds, or nothing until it has answered.
    #[cfg(test)]
    fn taking(&self, at: usize) -> Option<u64> {
        match self.load[at].latency.load(Ordering::Relaxed) {
            0 => None,
            latency => Some(latency),
        }
    }
}

/// One answer, for the folder to work into what its endpoint takes.
#[derive(Debug)]
struct Answered {
    plan: Arc<Plan>,
    at: usize,
    micros: u64,
}

/// Picks an endpoint for each request, and counts the request against it.
///
/// What an endpoint takes is worked out away from here: a finished request says how long it
/// waited and moves on, and the folder does the arithmetic in batches.
#[derive(Debug)]
pub struct Dispatcher {
    answered: mpsc::UnboundedSender<Answered>,
}

impl Dispatcher {
    /// A dispatcher, and the folder that keeps what it learns up to date.
    pub fn new() -> (Self, Folding) {
        let (answered, answers) = mpsc::unbounded_channel();
        (Self { answered }, Folding { answers })
    }

    /// Chooses where this request goes, counting it against that endpoint until it is done.
    pub fn choose(&self, resolution: &Resolution) -> Chosen {
        let plan = resolution.plan();
        let at = match resolution.rule().strategy {
            Strategy::RoundRobin => plan.in_turn(),
            Strategy::Ratio => Self::by_share(plan),
            Strategy::LeastLoad => plan.lightest(),
        };
        plan.load[at].in_flight.fetch_add(1, Ordering::Relaxed);
        Chosen {
            endpoint: resolution.targets()[at].endpoint.clone(),
            plan: Arc::clone(plan),
            at,
            answered: self.answered.clone(),
            started: Instant::now(),
        }
    }

    /// An endpoint drawn in proportion to the shares.
    fn by_share(plan: &Plan) -> usize {
        match plan.total() {
            // Every live endpoint is drained, which the plan already widened to all of them.
            0 => plan.live()[0],
            total => plan.at_share(roll(total)),
        }
    }
}

/// Works what each endpoint takes into the plan it belongs to, a batch of answers at a time.
///
/// The arithmetic is here rather than in the request that finished, and this is the seam any
/// further rebalancing belongs on: the plans hold their own numbers, so nothing else does.
#[derive(Debug)]
pub struct Folding {
    answers: mpsc::UnboundedReceiver<Answered>,
}

impl Folding {
    /// Folds in whatever has arrived, up to a batch, reporting how many. Answers nothing once
    /// every dispatcher is gone.
    pub async fn fold(&mut self) -> usize {
        let mut batch = Vec::with_capacity(BATCH);
        let folded = self.answers.recv_many(&mut batch, BATCH).await;
        for answer in batch {
            answer.plan.load[answer.at].answered(answer.micros);
        }
        folded
    }

    /// Keeps folding for as long as the server runs.
    pub fn keep_folding(mut self) -> JoinHandle<()> {
        tokio::spawn(async move { while self.fold().await > 0 {} })
    }
}

/// A number below `total`, drawn afresh. Falls to the middle when entropy is unavailable.
fn roll(total: u64) -> u64 {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        return total / 2;
    }
    u64::from_le_bytes(bytes) % total
}

/// The endpoint a request was sent to, counted against it until this is dropped.
#[derive(Debug)]
pub struct Chosen {
    endpoint: Endpoint,
    plan: Arc<Plan>,
    at: usize,
    answered: mpsc::UnboundedSender<Answered>,
    started: Instant,
}

impl Chosen {
    /// Where the request goes.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
}

impl Drop for Chosen {
    fn drop(&mut self) {
        self.plan.load[self.at]
            .in_flight
            .fetch_sub(1, Ordering::Relaxed);
        // The folder does the arithmetic; a request that is done only says how long it waited.
        let _ = self.answered.send(Answered {
            plan: Arc::clone(&self.plan),
            at: self.at,
            // At least one: zero is how an endpoint that has never answered is told apart
            // from one that answers faster than the clock can say.
            micros: u64::try_from(self.started.elapsed().as_micros())
                .unwrap_or(u64::MAX)
                .max(1),
        });
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ip_core::{
        AbsPath, ApiId, Endpoint, Host, Port, Protocol, RouteKey, RouteRule, RouteTarget, Weight,
    };

    use super::*;
    use crate::table::RoutingTable;

    fn endpoint(host: &str, path: &str) -> Endpoint {
        Endpoint::new(
            Protocol::Https,
            Host::new(host).unwrap(),
            Port::new(443).unwrap(),
            AbsPath::new(path).unwrap(),
        )
    }

    /// A rule over `behind`, each host with the share it takes.
    fn resolution(strategy: Strategy, behind: &[(&str, u32)]) -> Resolution {
        resolution_at("/anthropic", strategy, behind)
    }

    /// The same, under a key of its own, for a test about two rules.
    fn resolution_at(path: &str, strategy: Strategy, behind: &[(&str, u32)]) -> Resolution {
        let weighted = behind
            .iter()
            .map(|(host, weight)| Weighted::weighing(endpoint(host, "/v1"), Weight::new(*weight)))
            .collect();
        let rule = RouteRule {
            api: ApiId::new("anthropic").unwrap(),
            key: RouteKey::new(endpoint("gateway.local", path)),
            target: RouteTarget::from_weighted(weighted).unwrap(),
            strategy,
        };
        let table = RoutingTable::from_rules(vec![rule]);
        table
            .resolve(&RouteKey::new(endpoint("gateway.local", path)))
            .expect("the rule carries its own key")
    }

    /// Which host each of `rounds` requests went to.
    fn sent_to(dispatcher: &Dispatcher, resolution: &Resolution, rounds: usize) -> Vec<String> {
        (0..rounds)
            .map(|_| {
                let chosen = dispatcher.choose(resolution);
                chosen.endpoint().host.as_str().to_owned()
            })
            .collect()
    }

    /// How many of `rounds` requests each host took.
    fn counted(
        dispatcher: &Dispatcher,
        resolution: &Resolution,
        rounds: usize,
    ) -> HashMap<String, usize> {
        let mut counted = HashMap::new();
        for host in sent_to(dispatcher, resolution, rounds) {
            *counted.entry(host).or_insert(0) += 1;
        }
        counted
    }

    #[test]
    fn one_endpoint_takes_every_request_whatever_the_strategy() {
        for strategy in [Strategy::RoundRobin, Strategy::LeastLoad, Strategy::Ratio] {
            let resolution = resolution(strategy, &[("only.example.com", 1)]);
            let (dispatcher, _folding) = Dispatcher::new();
            assert_eq!(
                sent_to(&dispatcher, &resolution, 3),
                ["only.example.com"; 3]
            );
        }
    }

    #[test]
    fn dispatching_in_turn_goes_round_the_endpoints() {
        let resolution = resolution(
            Strategy::RoundRobin,
            &[("one.example.com", 1), ("two.example.com", 1)],
        );
        let (dispatcher, _folding) = Dispatcher::new();
        assert_eq!(
            sent_to(&dispatcher, &resolution, 5),
            [
                "one.example.com",
                "two.example.com",
                "one.example.com",
                "two.example.com",
                "one.example.com"
            ]
        );
    }

    #[test]
    fn a_turn_belongs_to_the_rule_rather_than_to_the_dispatcher() {
        let first = resolution_at(
            "/anthropic",
            Strategy::RoundRobin,
            &[("one.example.com", 1), ("two.example.com", 1)],
        );
        let (dispatcher, _folding) = Dispatcher::new();
        // Another rule's turn starts at its own beginning, not where this one has reached.
        sent_to(&dispatcher, &first, 3);
        let second = resolution_at(
            "/openai",
            Strategy::RoundRobin,
            &[("three.example.com", 1), ("four.example.com", 1)],
        );
        assert_eq!(sent_to(&dispatcher, &second, 1), ["three.example.com"]);
    }

    #[test]
    fn a_share_of_the_traffic_is_roughly_the_share_it_was_given() {
        let resolution = resolution(
            Strategy::Ratio,
            &[("one.example.com", 1), ("three.example.com", 3)],
        );
        let (dispatcher, _folding) = Dispatcher::new();
        let counted = counted(&dispatcher, &resolution, 4_000);
        let quarter = counted["one.example.com"];
        let rest = counted["three.example.com"];
        assert!(
            (800..1_200).contains(&quarter),
            "a quarter share took {quarter} of 4000"
        );
        assert!(
            (2_800..3_200).contains(&rest),
            "the rest took {rest} of 4000"
        );
    }

    #[test]
    fn a_drained_endpoint_is_sent_nothing_by_any_strategy() {
        for strategy in [Strategy::RoundRobin, Strategy::LeastLoad, Strategy::Ratio] {
            let resolution = resolution(
                strategy,
                &[("live.example.com", 1), ("drained.example.com", 0)],
            );
            let (dispatcher, _folding) = Dispatcher::new();
            let counted = counted(&dispatcher, &resolution, 200);
            assert_eq!(counted.get("drained.example.com"), None, "{strategy}");
            assert_eq!(counted["live.example.com"], 200);
        }
    }

    #[test]
    fn a_target_drained_to_nothing_still_takes_its_requests() {
        let resolution = resolution(
            Strategy::Ratio,
            &[("one.example.com", 0), ("two.example.com", 0)],
        );
        let (dispatcher, _folding) = Dispatcher::new();
        assert_eq!(sent_to(&dispatcher, &resolution, 2).len(), 2);
    }

    #[test]
    fn the_least_loaded_endpoint_is_the_one_carrying_least() {
        let resolution = resolution(
            Strategy::LeastLoad,
            &[("one.example.com", 1), ("two.example.com", 1)],
        );
        let (dispatcher, _folding) = Dispatcher::new();
        // Nothing is in flight, so the first is as good as the second.
        let held = dispatcher.choose(&resolution);
        assert_eq!(held.endpoint().host.as_str(), "one.example.com");
        // With that one carrying a request, the next goes elsewhere.
        assert_eq!(
            dispatcher.choose(&resolution).endpoint().host.as_str(),
            "two.example.com"
        );
        drop(held);
    }

    #[test]
    fn a_request_that_is_done_stops_counting_against_its_endpoint() {
        let resolution = resolution(
            Strategy::LeastLoad,
            &[("one.example.com", 1), ("two.example.com", 1)],
        );
        let (dispatcher, _folding) = Dispatcher::new();
        drop(dispatcher.choose(&resolution));
        // The first is free again, and answered quickly, so it is still the lightest.
        assert_eq!(
            dispatcher.choose(&resolution).endpoint().host.as_str(),
            "one.example.com"
        );
    }

    #[tokio::test]
    async fn an_endpoint_that_has_been_slow_carries_less() {
        let resolution = resolution(
            Strategy::LeastLoad,
            &[("slow.example.com", 1), ("quick.example.com", 1)],
        );
        let (dispatcher, mut folding) = Dispatcher::new();
        // The first request goes to the first endpoint, both being unknown, and takes a while.
        let held = dispatcher.choose(&resolution);
        assert_eq!(held.endpoint().host.as_str(), "slow.example.com");
        std::thread::sleep(std::time::Duration::from_millis(20));
        drop(held);
        assert_eq!(folding.fold().await, 1);
        assert!(
            resolution
                .plan()
                .taking(0)
                .is_some_and(|micros| micros > 10_000)
        );

        let counted = counted(&dispatcher, &resolution, 20);
        assert_eq!(counted.get("slow.example.com"), None, "counted {counted:?}");
        assert_eq!(counted["quick.example.com"], 20);
    }

    #[tokio::test]
    async fn what_an_endpoint_takes_settles_towards_what_it_keeps_answering_in() {
        let resolution = resolution(Strategy::LeastLoad, &[("only.example.com", 1)]);
        let (dispatcher, mut folding) = Dispatcher::new();
        for _ in 0..40 {
            drop(dispatcher.choose(&resolution));
        }
        // Every answer is folded in, in batches, and the average lands near the truth.
        let mut folded = 0;
        while folded < 40 {
            folded += folding.fold().await;
        }
        let taking = resolution
            .plan()
            .taking(0)
            .expect("an answer was folded in");
        assert!(
            taking < 5_000,
            "an answer at once was taken as {taking} micros"
        );
    }

    #[tokio::test]
    async fn a_folder_with_no_dispatcher_left_stops_folding() {
        let (dispatcher, mut folding) = Dispatcher::new();
        drop(dispatcher);
        assert_eq!(folding.fold().await, 0);
    }

    #[test]
    fn a_roll_stays_inside_what_it_was_asked_for() {
        for total in [1, 2, 7, 1_000] {
            for _ in 0..50 {
                assert!(roll(total) < total);
            }
        }
    }

    #[test]
    fn two_requests_to_one_rule_take_their_turns_from_the_same_count() {
        let table = RoutingTable::from_rules(vec![RouteRule {
            api: ApiId::new("anthropic").unwrap(),
            key: RouteKey::new(endpoint("gateway.local", "/anthropic")),
            target: RouteTarget::from_weighted(vec![
                Weighted::new(endpoint("one.example.com", "/v1")),
                Weighted::new(endpoint("two.example.com", "/v1")),
            ])
            .unwrap(),
            strategy: Strategy::RoundRobin,
        }]);
        let key = RouteKey::new(endpoint("gateway.local", "/anthropic"));
        let (dispatcher, _folding) = Dispatcher::new();

        // Each request resolves afresh, and the turn is the rule's rather than the resolution's.
        let mut sent = Vec::new();
        for _ in 0..4 {
            let resolution = table.resolve(&key).unwrap();
            sent.push(
                dispatcher
                    .choose(&resolution)
                    .endpoint()
                    .host
                    .as_str()
                    .to_owned(),
            );
        }
        assert_eq!(
            sent,
            [
                "one.example.com",
                "two.example.com",
                "one.example.com",
                "two.example.com"
            ]
        );
    }

    #[test]
    fn a_plan_is_worked_out_once_for_the_endpoints_a_rule_holds() {
        let targets = vec![
            Weighted::weighing(endpoint("one.example.com", "/v1"), Weight::new(1)),
            Weighted::weighing(endpoint("two.example.com", "/v1"), Weight::new(3)),
        ];
        let plan = Plan::of(&targets);
        assert_eq!(plan.live(), [0, 1]);
        assert_eq!(plan.total(), 4);
        // The first share covers one draw, the second the other three.
        assert_eq!(plan.at_share(0), 0);
        assert_eq!(plan.at_share(1), 1);
        assert_eq!(plan.at_share(3), 1);
    }

    #[test]
    fn a_plan_leaves_out_what_is_drained() {
        let targets = vec![
            Weighted::weighing(endpoint("drained.example.com", "/v1"), Weight::DRAINED),
            Weighted::new(endpoint("live.example.com", "/v1")),
        ];
        let plan = Plan::of(&targets);
        assert_eq!(plan.live(), [1]);
        assert_eq!(plan.total(), 1);
        assert_eq!(plan.at_share(0), 1);
    }

    #[test]
    fn a_plan_over_nothing_but_drained_endpoints_holds_them_all() {
        let targets = vec![
            Weighted::weighing(endpoint("one.example.com", "/v1"), Weight::DRAINED),
            Weighted::weighing(endpoint("two.example.com", "/v1"), Weight::DRAINED),
        ];
        let plan = Plan::of(&targets);
        assert_eq!(plan.live(), [0, 1]);
        assert_eq!(plan.total(), 0);
    }

    #[test]
    fn a_draw_at_the_very_top_of_the_shares_still_names_an_endpoint() {
        let targets = vec![
            Weighted::new(endpoint("one.example.com", "/v1")),
            Weighted::new(endpoint("two.example.com", "/v1")),
        ];
        let plan = Plan::of(&targets);
        assert_eq!(plan.at_share(plan.total()), 1);
        assert_eq!(plan.at_share(u64::MAX), 1);
    }
}
