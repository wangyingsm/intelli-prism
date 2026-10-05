//! Choosing which endpoint behind a rule a request is sent to.
//!
//! Every count here is this node's own: no coordination between nodes, and nothing on the request
//! path but atomics. A cluster of like nodes spreads its traffic by each node spreading its own.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use http::HeaderMap;
use http::header::{CONTENT_LENGTH, CONTENT_TYPE, TRANSFER_ENCODING};
use ip_core::{Difficulty, Endpoint, Hedge, Replica, Strategy, Weighted};
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

/// The largest body the gateway may hold in memory for one request, in bytes.
///
/// One mebibyte. A chat completion is a few kilobytes, so the figure is generous for
/// everything this gateway is for while still refusing to hold three copies of a file.
/// It caps the racing that sends a body to several endpoints at once and the reading of a
/// turn's opening from §429 alike: both hold the body, so both accept the same bound.
const MAX_RACED_BODY: u64 = 1_048_576;

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

/// The endpoints one class of request may go to, and the running total of their shares.
///
/// Held as indices into the rule's endpoints, which is how the load is indexed too, so a
/// narrowed set never reads a score off the wrong endpoint.
#[derive(Debug)]
struct Tier {
    /// The endpoints themselves, each as it stands behind the rule.
    at: Vec<usize>,
    /// The running total of their shares, so a draw is one search rather than a sum.
    shares: Vec<u64>,
}

impl Tier {
    /// Wraps the endpoints, working out their running shares.
    fn of(targets: &[Weighted], at: Vec<usize>) -> Self {
        let mut running = 0;
        let shares = at
            .iter()
            .map(|at| {
                running += u64::from(targets[*at].weight.get());
                running
            })
            .collect();
        Self { at, shares }
    }

    /// What can serve one class: the endpoints whose ceiling is the lowest that carries it,
    /// since the cheapest machine that can do the work is the one that should. An endpoint
    /// that named no ceiling serves anything but never said it could, so it comes after every
    /// declared one; and when nothing carries the class at all, §427 sends the request to the
    /// most capable endpoint there is rather than refusing it.
    fn for_class(targets: &[Weighted], live: &[usize], class: Difficulty) -> Self {
        let served = |at: &usize| targets[*at].serves;
        let declaring = |ceiling: Difficulty| -> Vec<usize> {
            live.iter()
                .copied()
                .filter(|at| served(at) == Some(ceiling))
                .collect()
        };
        let at = match live
            .iter()
            .filter_map(served)
            .filter(|declared| *declared >= class)
            .min()
        {
            Some(lowest) => declaring(lowest),
            None => {
                let silent: Vec<usize> = live
                    .iter()
                    .copied()
                    .filter(|at| served(at).is_none())
                    .collect();
                if !silent.is_empty() {
                    return Self::of(targets, silent);
                }
                match live.iter().filter_map(served).max() {
                    Some(highest) => declaring(highest),
                    // Every live endpoint named nothing, which qualified above: unreachable,
                    // and a rule never stands behind no endpoints anyway.
                    None => live.to_vec(),
                }
            }
        };
        Self::of(targets, at)
    }

    /// How many endpoints this reaches.
    fn len(&self) -> usize {
        self.at.len()
    }

    /// Every share together, which is what a draw is made below.
    fn total(&self) -> u64 {
        self.shares.last().copied().unwrap_or(0)
    }

    /// The endpoint whose share `rolled` falls in.
    fn at_share(&self, rolled: u64) -> usize {
        let at = self.shares.partition_point(|running| *running <= rolled);
        self.at[at.min(self.at.len() - 1)]
    }
}

/// What a rule's endpoints look like to a dispatcher, and what they are carrying.
///
/// A rule changes far less often than it is used, so the live endpoints and the running total of
/// their shares are found once here rather than on the path of every request — as are the
/// endpoints each difficulty class narrows to. The load sits here too, one entry per endpoint by
/// the same index, so reading it is an array index.
#[derive(Debug)]
pub struct Plan {
    /// Where an unclassified request may go: those not drained, or every one when all are,
    /// since a rule must have somewhere to send a request.
    live: Tier,
    /// What each class in `Difficulty::ALL` order may be served by, worked out once.
    tiers: [Tier; 5],
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
        let tiers = Difficulty::ALL.map(|class| Tier::for_class(targets, &live, class));
        let live = Tier::of(targets, live);
        Self {
            live,
            tiers,
            turn: AtomicU64::new(0),
            load: targets.iter().map(|_| EndpointLoad::default()).collect(),
        }
    }

    /// The endpoints a request may go to.
    ///
    /// Test only: a classified request is dispatched from its tier, never from the live set.
    #[cfg(test)]
    pub fn live(&self) -> &[usize] {
        &self.live.at
    }

    /// Where a request naming this class may go: every live endpoint when it names none.
    fn tier(&self, class: Option<Difficulty>) -> &Tier {
        match class {
            None => &self.live,
            Some(class) => &self.tiers[class.position()],
        }
    }

    /// Every live share together, which is what a draw is made below.
    ///
    /// Test only: dispatch answers from the request's tier, and the live total would be
    /// the wrong set for a classified request.
    #[cfg(test)]
    pub fn total(&self) -> u64 {
        self.live.total()
    }

    /// The endpoint of this tier after the one this rule last used.
    fn in_turn(&self, tier: &Tier) -> usize {
        let turn = self.turn.fetch_add(1, Ordering::Relaxed);
        tier.at[(turn % tier.len() as u64) as usize]
    }

    /// The live endpoint whose share `rolled` falls in.
    ///
    /// Test only, as `total` is: a draw for a classified request runs on its tier.
    #[cfg(test)]
    pub fn at_share(&self, rolled: u64) -> usize {
        self.live.at_share(rolled)
    }

    /// The endpoint of this tier a new request would wait least behind.
    ///
    /// A scan of the tier's atomics: no lock, and the count it reads is the true one, so a
    /// burst of requests is spread rather than piled onto whoever was lightest a moment ago.
    fn lightest(&self, tier: &Tier) -> usize {
        let mut lightest = tier.at[0];
        let mut least = u64::MAX;
        for at in &tier.at {
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

/// One request as dispatch sees it: what its rule allows, and what the request asks for.
#[derive(Debug)]
pub struct Dispatching<'a> {
    /// The rule that matched, and the endpoints it stands behind.
    resolution: &'a Resolution,
    /// The class the caller named, if it named one. Nothing narrows an unclassified request.
    class: Option<Difficulty>,
    /// Where this request's turn landed before, if it marked a turn and the memory kept one.
    landed: Option<&'a Replica>,
}

impl<'a> Dispatching<'a> {
    /// The pair a dispatcher is given: this resolution, narrowed to this class.
    pub fn new(resolution: &'a Resolution, class: Option<Difficulty>) -> Self {
        Self {
            resolution,
            class,
            landed: None,
        }
    }

    /// Where the turn this request belongs to landed last time, which is where it goes again
    /// as long as that endpoint still stands behind the rule, unsunk and able to carry what
    /// is asked now. It replaces what the strategy would have picked, for this request only:
    /// a turn is not consulted over an endpoint that cannot serve it.
    #[must_use]
    pub fn landed_on(self, landed: Option<&'a Replica>) -> Self {
        Self { landed, ..self }
    }

    /// The rule that matched, and the endpoints it stands behind.
    pub fn resolution(&self) -> &Resolution {
        self.resolution
    }

    /// Where the remembered endpoint stands among this rule's targets, if this request could
    /// still have been sent there: the rule stands behind it, it is not drained, and what it
    /// serves covers the class being asked. A turn whose endpoint fails any of these is not
    /// kept, and the request is dispatched as though it were new.
    fn landed_at(&self) -> Option<usize> {
        let landed = self.landed?;
        self.resolution.targets().iter().position(|target| {
            Replica::of(&target.endpoint) == *landed
                && !target.weight.is_drained()
                && match (self.class, target.serves) {
                    (Some(asked), Some(ceiling)) => ceiling >= asked,
                    _ => true,
                }
        })
    }
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
    pub fn choose(&self, dispatching: &Dispatching) -> Chosen {
        let plan = dispatching.resolution.plan();
        // Where a turn landed goes ahead of the choosing, and is chosen instead of: no tier is
        // narrowed through and no strategy consulted, so the rule's turn is not spent on a
        // request its turn has already answered for.
        let at = match dispatching.landed_at() {
            Some(at) => at,
            None => {
                let tier = plan.tier(dispatching.class);
                match dispatching.resolution.rule().strategy {
                    Strategy::RoundRobin => plan.in_turn(tier),
                    Strategy::Ratio => Self::by_share(tier),
                    Strategy::LeastLoad => plan.lightest(tier),
                }
            }
        };
        plan.load[at].in_flight.fetch_add(1, Ordering::Relaxed);
        Chosen {
            endpoint: dispatching.resolution.targets()[at].endpoint.clone(),
            plan: Arc::clone(plan),
            at,
            answered: self.answered.clone(),
            started: Instant::now(),
            abandoned: false,
        }
    }

    /// Chooses the several endpoints a request races across, counting each against
    /// itself until it is done with.
    ///
    /// In launch order, first arm first: where the turn landed if the request belongs to one,
    /// and the endpoint the rule would have picked alone otherwise; the rest are drawn without
    /// repeating it, cut down to how many of the endpoints the request's class leaves in play.
    pub fn choose_many(&self, dispatching: &Dispatching, hedge: Hedge) -> Vec<Chosen> {
        let plan = dispatching.resolution.plan();
        let tier = plan.tier(dispatching.class);
        let count = usize::from(hedge.get()).min(tier.len());
        let slots = match dispatching.resolution.rule().strategy {
            Strategy::RoundRobin => (0..count).map(|_| plan.in_turn(tier)).collect(),
            Strategy::LeastLoad => lightest_arms(plan, tier, count),
            Strategy::Ratio => {
                let mut slots = vec![Self::by_share(tier)];
                extra_arms(plan, tier, &mut slots, count);
                slots
            }
        };
        // The memory moves the first arm of the race; it does not thin it. What the strategy
        // drew comes after the turn's endpoint in the order it drew it, and the count the
        // class and the hedge allowed is the count that races.
        let slots = match dispatching.landed_at() {
            Some(at) => {
                let mut raced = vec![at];
                raced.extend(slots.into_iter().filter(|arm| *arm != at));
                raced.truncate(count);
                raced
            }
            None => slots,
        };
        let started = Instant::now();
        slots
            .into_iter()
            .map(|at| {
                plan.load[at].in_flight.fetch_add(1, Ordering::Relaxed);
                Chosen {
                    endpoint: dispatching.resolution.targets()[at].endpoint.clone(),
                    plan: Arc::clone(plan),
                    at,
                    answered: self.answered.clone(),
                    started,
                    abandoned: false,
                }
            })
            .collect()
    }

    /// An endpoint drawn in proportion to the shares.
    fn by_share(tier: &Tier) -> usize {
        match tier.total() {
            // Every endpoint of this tier is drained, which the plan already widened to all.
            0 => tier.at[0],
            total => tier.at_share(roll(total)),
        }
    }
}

/// The `count` lightest endpoints of a tier, one score read per endpoint.
///
/// `count` never reaches beyond a small bound, so the lightest are kept in place as the
/// scores come by rather than the whole tier being sorted.
fn lightest_arms(plan: &Plan, tier: &Tier, count: usize) -> Vec<usize> {
    let mut lightest: Vec<(u64, usize)> = Vec::with_capacity(count);
    for at in &tier.at {
        lightest.push((plan.load[*at].score(), *at));
        lightest.sort_by_key(|(score, _)| *score);
        lightest.truncate(count);
    }
    lightest.into_iter().map(|(_, at)| at).collect()
}

/// A ratio rule's arms after the first was drawn, rotated through what is left of the tier.
///
/// The draw sends the rule's *load* to the endpoint its share names; the extra arms
/// are insurance, not load, so re-drawing them by share would only skew the proportions
/// the operator asked for. They instead rotate, one step per request, through the tier's
/// endpoints in turn order, so that over many requests every endpoint carries an equal
/// share of the insurance.
fn extra_arms(plan: &Plan, tier: &Tier, arms: &mut Vec<usize>, up_to: usize) {
    if arms.len() == up_to {
        return;
    }
    let rest: Vec<usize> = tier
        .at
        .iter()
        .copied()
        .filter(|at| !arms.contains(at))
        .collect();
    let at = (plan.turn.fetch_add(1, Ordering::Relaxed) % rest.len() as u64) as usize;
    for step in 0..rest.len() {
        if arms.len() == up_to {
            break;
        }
        arms.push(rest[(at + step) % rest.len()]);
    }
}

/// The hedge that will actually be used for this request: the rule's, or none.
///
/// Racing sends the body once per arm, so it is refused when the body cannot be held in
/// memory. A declared length past the cap, one declared but not as a number, an unknown
/// size, and a file transport all stand in the way.
pub fn may_race(headers: &HeaderMap, asked: Hedge) -> Hedge {
    if asked == Hedge::NONE {
        return asked;
    }
    if may_hold(headers) {
        return asked;
    }
    Hedge::NONE
}

/// Whether the body behind these headers may be held in memory whole.
///
/// One policy with two callers: the racing of a request, which sends the held body to
/// several endpoints, and §429's turn recognition, which reads a conversation's opening out
/// of it. A request it refuses streams through the gateway untouched, by both.
#[must_use]
pub fn may_hold(headers: &HeaderMap) -> bool {
    !file_transport(headers) && length_ok(headers)
}

/// Whether the request is a file transport, whatever its declared length.
///
/// The primary type is compared up to its first parameter, case-insensitively, so `boundary`
/// and `charset` after it cannot defeat the match.
fn file_transport(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .and_then(|media_type| media_type.split('/').next())
        .is_some_and(|primary| primary.trim().eq_ignore_ascii_case("multipart"))
}

/// Whether the declared body fits in memory.
///
/// With no length at all, a body is still refused unless the encoding says there is none:
/// a chunked request could be small, and an unknown size cannot be checked against the cap.
fn length_ok(headers: &HeaderMap) -> bool {
    match headers.get(CONTENT_LENGTH) {
        None => headers.get(TRANSFER_ENCODING).is_none(),
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|length| length <= MAX_RACED_BODY),
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
    /// Whether it was abandoned, so its span is not reported when it is dropped.
    abandoned: bool,
}

impl Chosen {
    /// Where the request goes.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Say this arm was abandoned, so dropping it still comes off the count but reports
    /// no latency.
    ///
    /// An abandoned arm is dropped when the winner answers, and the span it held is the
    /// winner's, not its own; folded in, it would make the endpoint that had not
    /// answered look as quick as the one that did.
    pub fn abandon(&mut self) {
        self.abandoned = true;
    }
}

impl Drop for Chosen {
    fn drop(&mut self) {
        self.plan.load[self.at]
            .in_flight
            .fetch_sub(1, Ordering::Relaxed);
        if self.abandoned {
            return;
        }
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

    use http::HeaderName;

    use ip_core::{
        AbsPath, ApiId, Endpoint, HEDGE_MOST, Hedge, Host, Port, Protocol, RouteKey, RouteRule,
        RouteTarget, Weight,
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

    /// A rule over `behind`, each host with the share it takes and the ceiling it serves.
    fn resolution(strategy: Strategy, behind: &[(&str, u32, Option<Difficulty>)]) -> Resolution {
        resolution_at("/anthropic", strategy, behind)
    }

    /// The same, under a key of its own, for a test about two rules.
    fn resolution_at(
        path: &str,
        strategy: Strategy,
        behind: &[(&str, u32, Option<Difficulty>)],
    ) -> Resolution {
        let weighted = behind
            .iter()
            .map(|(host, weight, serves)| {
                Weighted::serving(endpoint(host, "/v1"), Weight::new(*weight), *serves)
            })
            .collect();
        let rule = RouteRule {
            api: ApiId::new("anthropic").unwrap(),
            key: RouteKey::new(endpoint("gateway.local", path)),
            target: RouteTarget::from_weighted(weighted).unwrap(),
            strategy,
            hedge: ip_core::Hedge::default(),
        };
        let table = RoutingTable::from_rules(vec![rule]);
        table
            .resolve(&RouteKey::new(endpoint("gateway.local", path)))
            .expect("the rule carries its own key")
    }

    /// Which host each of `rounds` requests naming `class` went to.
    fn sent_by_class(
        dispatcher: &Dispatcher,
        resolution: &Resolution,
        class: Option<Difficulty>,
        rounds: usize,
    ) -> Vec<String> {
        (0..rounds)
            .map(|_| {
                let chosen = dispatcher.choose(&Dispatching::new(resolution, class));
                chosen.endpoint().host.as_str().to_owned()
            })
            .collect()
    }

    /// Which host each of `rounds` unclassified requests went to.
    fn sent_to(dispatcher: &Dispatcher, resolution: &Resolution, rounds: usize) -> Vec<String> {
        sent_by_class(dispatcher, resolution, None, rounds)
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

    /// How many of `rounds` requests naming `class` each host took.
    fn counted_by_class(
        dispatcher: &Dispatcher,
        resolution: &Resolution,
        class: Option<Difficulty>,
        rounds: usize,
    ) -> HashMap<String, usize> {
        let mut counted = HashMap::new();
        for host in sent_by_class(dispatcher, resolution, class, rounds) {
            *counted.entry(host).or_insert(0) += 1;
        }
        counted
    }

    #[test]
    fn one_endpoint_takes_every_request_whatever_the_strategy() {
        for strategy in [Strategy::RoundRobin, Strategy::LeastLoad, Strategy::Ratio] {
            let resolution = resolution(strategy, &[("only.example.com", 1, None)]);
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
            &[("one.example.com", 1, None), ("two.example.com", 1, None)],
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
            &[("one.example.com", 1, None), ("two.example.com", 1, None)],
        );
        let (dispatcher, _folding) = Dispatcher::new();
        // Another rule's turn starts at its own beginning, not where this one has reached.
        sent_to(&dispatcher, &first, 3);
        let second = resolution_at(
            "/openai",
            Strategy::RoundRobin,
            &[
                ("three.example.com", 1, None),
                ("four.example.com", 1, None),
            ],
        );
        assert_eq!(sent_to(&dispatcher, &second, 1), ["three.example.com"]);
    }

    #[test]
    fn a_share_of_the_traffic_is_roughly_the_share_it_was_given() {
        let resolution = resolution(
            Strategy::Ratio,
            &[("one.example.com", 1, None), ("three.example.com", 3, None)],
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
                &[
                    ("live.example.com", 1, None),
                    ("drained.example.com", 0, None),
                ],
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
            &[("one.example.com", 0, None), ("two.example.com", 0, None)],
        );
        let (dispatcher, _folding) = Dispatcher::new();
        assert_eq!(sent_to(&dispatcher, &resolution, 2).len(), 2);
    }

    /// Which host each arm of a many choice naming `class` goes to, in launch order.
    fn arms_by_class(
        resolution: &Resolution,
        class: Option<Difficulty>,
        hedge: Hedge,
    ) -> Vec<String> {
        let (dispatcher, _folding) = Dispatcher::new();
        dispatcher
            .choose_many(&Dispatching::new(resolution, class), hedge)
            .into_iter()
            .map(|arm| arm.endpoint().host.as_str().to_owned())
            .collect()
    }

    /// Which host each arm of an unclassified many choice goes to, in launch order.
    fn arms_to(resolution: &Resolution, hedge: Hedge) -> Vec<String> {
        arms_by_class(resolution, None, hedge)
    }

    #[test]
    fn two_arms_over_three_live_endpoints_are_two_distinct_ones() {
        for strategy in [Strategy::RoundRobin, Strategy::LeastLoad, Strategy::Ratio] {
            let resolution = resolution(
                strategy,
                &[
                    ("one.example.com", 1, None),
                    ("two.example.com", 1, None),
                    ("three.example.com", 1, None),
                ],
            );
            let taken = arms_to(&resolution, Hedge::new(2).unwrap());
            assert_eq!(taken.len(), 2, "{strategy}");
            assert_ne!(taken[0], taken[1], "{strategy}: {taken:?}");
            if !matches!(strategy, Strategy::Ratio) {
                // A fresh, loadless plan starts at its first endpoint.
                assert_eq!(taken[0], "one.example.com", "{strategy}");
            }
        }
    }

    #[test]
    fn one_arm_gives_what_choose_gives() {
        for strategy in [Strategy::RoundRobin, Strategy::LeastLoad, Strategy::Ratio] {
            let behind = [("only.example.com", 1, None), ("down.example.com", 0, None)];
            let alone = resolution(strategy, &behind);
            let raced = resolution(strategy, &behind);
            let (by_one, _) = Dispatcher::new();
            let (by_many, _) = Dispatcher::new();
            let single = by_one.choose(&Dispatching::new(&alone, None));
            let arms = by_many.choose_many(&Dispatching::new(&raced, None), Hedge::NONE);
            assert_eq!(arms.len(), 1, "{strategy}");
            assert_eq!(
                arms[0].endpoint().host.as_str(),
                single.endpoint().host.as_str()
            );
            assert_eq!(
                arms[0].endpoint().host.as_str(),
                "only.example.com",
                "{strategy}"
            );
        }
    }

    #[test]
    fn least_load_leaves_out_the_endpoint_carrying_a_request() {
        let resolution = resolution(
            Strategy::LeastLoad,
            &[
                ("one.example.com", 1, None),
                ("two.example.com", 1, None),
                ("three.example.com", 1, None),
            ],
        );
        let (dispatcher, _folding) = Dispatcher::new();
        let held = dispatcher.choose(&Dispatching::new(&resolution, None));
        assert_eq!(held.endpoint().host.as_str(), "one.example.com");
        let arms =
            dispatcher.choose_many(&Dispatching::new(&resolution, None), Hedge::new(2).unwrap());
        let taken: Vec<&str> = arms
            .iter()
            .map(|arm| arm.endpoint().host.as_str())
            .collect();
        assert_eq!(taken, ["two.example.com", "three.example.com"]);
        drop(held);
    }

    #[test]
    fn a_hedge_wider_than_the_live_endpoints_is_cut_to_them() {
        for strategy in [Strategy::RoundRobin, Strategy::LeastLoad, Strategy::Ratio] {
            let resolution = resolution(
                strategy,
                &[("one.example.com", 1, None), ("two.example.com", 1, None)],
            );
            let taken = arms_to(&resolution, Hedge::new(HEDGE_MOST).unwrap());
            assert_eq!(taken.len(), 2, "{strategy}");
            assert_ne!(taken[0], taken[1], "{strategy}: {taken:?}");
        }
    }

    #[test]
    fn a_drained_endpoint_is_never_an_arm() {
        for strategy in [Strategy::RoundRobin, Strategy::LeastLoad, Strategy::Ratio] {
            let resolution = resolution(
                strategy,
                &[
                    ("one.example.com", 1, None),
                    ("drained.example.com", 0, None),
                    ("two.example.com", 1, None),
                ],
            );
            let taken = arms_to(&resolution, Hedge::new(2).unwrap());
            for host in &taken {
                assert_ne!(*host, "drained.example.com", "{strategy}: {taken:?}");
            }
        }
    }

    #[test]
    fn a_rule_whose_endpoints_are_all_drained_still_answers_with_arms() {
        for strategy in [Strategy::RoundRobin, Strategy::LeastLoad, Strategy::Ratio] {
            let resolution = resolution(
                strategy,
                &[("one.example.com", 0, None), ("two.example.com", 0, None)],
            );
            let taken = arms_to(&resolution, Hedge::new(2).unwrap());
            assert_eq!(taken.len(), 2, "{strategy}");
            assert_ne!(taken[0], taken[1], "{strategy}: {taken:?}");
        }
    }

    #[test]
    fn an_unclassified_request_is_narrowed_by_no_ceiling_whatever_the_strategy() {
        // The path every request takes until a classifier exists: no class named, every
        // live endpoint in play whoever it serves.
        let behind = &[
            ("easy.example.com", 1, Some(Difficulty::Easy)),
            ("hard.example.com", 1, Some(Difficulty::Hard)),
            ("silent.example.com", 1, None),
        ];
        let (dispatcher, _folding) = Dispatcher::new();
        let round_robin = resolution(Strategy::RoundRobin, behind);
        assert_eq!(
            sent_to(&dispatcher, &round_robin, 6),
            [
                "easy.example.com",
                "hard.example.com",
                "silent.example.com",
                "easy.example.com",
                "hard.example.com",
                "silent.example.com"
            ]
        );
        // Nothing is loaded, so least load answers with the first, exactly as it always did.
        let least_load = resolution(Strategy::LeastLoad, behind);
        assert_eq!(
            sent_to(&dispatcher, &least_load, 2),
            ["easy.example.com", "easy.example.com"]
        );
        let ratio = resolution(Strategy::Ratio, behind);
        let counted = counted(&dispatcher, &ratio, 3_000);
        for host in ["easy.example.com", "hard.example.com", "silent.example.com"] {
            assert!((900..1_100).contains(&counted[host]), "{counted:?}");
        }
    }

    #[test]
    fn a_class_goes_to_the_lowest_ceiling_that_carries_it() {
        let behind = &[
            ("easy.example.com", 1, Some(Difficulty::Easy)),
            ("hard.example.com", 1, Some(Difficulty::Hard)),
            ("research.example.com", 1, Some(Difficulty::Research)),
        ];
        let (dispatcher, _folding) = Dispatcher::new();
        let resolution = resolution(Strategy::RoundRobin, behind);
        assert_eq!(
            sent_by_class(&dispatcher, &resolution, Some(Difficulty::Hard), 3),
            ["hard.example.com"; 3]
        );
        // A median request goes to `hard` because nothing between them declared.
        assert_eq!(
            sent_by_class(&dispatcher, &resolution, Some(Difficulty::Median), 1),
            ["hard.example.com"]
        );
        assert_eq!(
            sent_by_class(&dispatcher, &resolution, Some(Difficulty::Research), 2),
            ["research.example.com"; 2]
        );
    }

    #[test]
    fn an_endpoint_below_the_class_is_never_sent_the_request() {
        let behind = &[
            ("easy.example.com", 1, Some(Difficulty::Easy)),
            ("median.example.com", 1, Some(Difficulty::Median)),
        ];
        let (dispatcher, _folding) = Dispatcher::new();
        let resolution = resolution(Strategy::RoundRobin, behind);
        let counted = counted_by_class(&dispatcher, &resolution, Some(Difficulty::Median), 50);
        assert_eq!(counted.get("easy.example.com"), None);
        assert_eq!(counted["median.example.com"], 50);
    }

    #[test]
    fn a_drained_endpoint_that_could_serve_the_class_is_still_never_sent_it() {
        let behind = &[
            ("live.example.com", 1, Some(Difficulty::Research)),
            ("drained.example.com", 0, Some(Difficulty::Research)),
        ];
        let (dispatcher, _folding) = Dispatcher::new();
        let resolution = resolution(Strategy::RoundRobin, behind);
        for class in [None, Some(Difficulty::Hard), Some(Difficulty::Research)] {
            let counted = counted_by_class(&dispatcher, &resolution, class, 40);
            assert_eq!(counted.get("drained.example.com"), None, "{class:?}");
            assert_eq!(counted["live.example.com"], 40, "{class:?}");
        }
    }

    #[test]
    fn an_endpoint_with_no_ceiling_serves_every_class_and_yields_to_what_declared_it() {
        let behind = &[
            ("silent.example.com", 1, None),
            ("hard.example.com", 1, Some(Difficulty::Hard)),
        ];
        let (dispatcher, _folding) = Dispatcher::new();
        let resolution = resolution(Strategy::RoundRobin, behind);
        // One that declared it can serve the class beats one that never said.
        let counted = counted_by_class(&dispatcher, &resolution, Some(Difficulty::Hard), 40);
        assert_eq!(counted.get("silent.example.com"), None);
        assert_eq!(counted["hard.example.com"], 40);
        // Nothing declared carries research, so the silent one serves it after all.
        assert_eq!(
            sent_by_class(&dispatcher, &resolution, Some(Difficulty::Research), 20),
            ["silent.example.com"; 20]
        );
    }

    #[test]
    fn a_class_no_endpoint_carries_goes_to_the_most_capable_there_is() {
        let behind = &[
            ("easy.example.com", 1, Some(Difficulty::Easy)),
            ("median.example.com", 1, Some(Difficulty::Median)),
        ];
        let (dispatcher, _folding) = Dispatcher::new();
        let resolution = resolution(Strategy::RoundRobin, behind);
        assert_eq!(
            sent_by_class(&dispatcher, &resolution, Some(Difficulty::Research), 20),
            ["median.example.com"; 20]
        );
        // A rule drained to nothing widens to all of them, and the fallback stays inside it.
        let drained = resolution_at(
            "/anthropic",
            Strategy::RoundRobin,
            &[
                ("easy.example.com", 0, Some(Difficulty::Easy)),
                ("median.example.com", 0, Some(Difficulty::Median)),
            ],
        );
        assert_eq!(
            sent_by_class(&dispatcher, &drained, Some(Difficulty::Research), 1),
            ["median.example.com"]
        );
    }

    #[test]
    fn within_a_tier_the_rule_s_own_strategy_still_decides() {
        let (dispatcher, _folding) = Dispatcher::new();
        let shared = resolution(
            Strategy::RoundRobin,
            &[
                ("one.example.com", 1, Some(Difficulty::Hard)),
                ("two.example.com", 1, Some(Difficulty::Hard)),
                ("three.example.com", 1, Some(Difficulty::Hard)),
            ],
        );
        assert_eq!(
            sent_by_class(&dispatcher, &shared, Some(Difficulty::Hard), 4),
            [
                "one.example.com",
                "two.example.com",
                "three.example.com",
                "one.example.com"
            ]
        );
        let shares = resolution(
            Strategy::Ratio,
            &[
                ("one.example.com", 1, Some(Difficulty::Hard)),
                ("three.example.com", 3, Some(Difficulty::Hard)),
            ],
        );
        let counted = counted_by_class(&dispatcher, &shares, Some(Difficulty::Hard), 4_000);
        assert!(
            (800..1_200).contains(&counted["one.example.com"]),
            "{counted:?}"
        );
        assert!(
            (2_800..3_200).contains(&counted["three.example.com"]),
            "{counted:?}"
        );
    }

    #[test]
    fn a_hedge_is_cut_to_what_the_class_qualifies_not_to_what_lives() {
        let behind = &[
            ("one.example.com", 1, Some(Difficulty::Hard)),
            ("two.example.com", 1, Some(Difficulty::Hard)),
            ("easy.example.com", 1, Some(Difficulty::Easy)),
        ];
        for strategy in [Strategy::RoundRobin, Strategy::LeastLoad, Strategy::Ratio] {
            let resolution = resolution(strategy, behind);
            let taken = arms_by_class(
                &resolution,
                Some(Difficulty::Hard),
                Hedge::new(HEDGE_MOST).unwrap(),
            );
            assert_eq!(taken.len(), 2, "{strategy}: {taken:?}");
            assert_ne!(taken[0], taken[1], "{strategy}: {taken:?}");
            for host in taken {
                assert_ne!(host, "easy.example.com", "{strategy} raced below the class");
            }
        }
    }

    #[test]
    fn a_tier_counts_load_against_the_endpoint_it_named() {
        let behind = &[
            ("easy.example.com", 1, Some(Difficulty::Easy)),
            ("hard.example.com", 1, Some(Difficulty::Hard)),
        ];
        let (dispatcher, _folding) = Dispatcher::new();
        let resolution = resolution(Strategy::RoundRobin, behind);
        let held = dispatcher.choose(&Dispatching::new(&resolution, Some(Difficulty::Hard)));
        assert_eq!(held.endpoint().host.as_str(), "hard.example.com");
        let plan = resolution.plan();
        assert_eq!(plan.load[0].in_flight.load(Ordering::Relaxed), 0);
        assert_eq!(plan.load[1].in_flight.load(Ordering::Relaxed), 1);
        drop(held);
        assert_eq!(plan.load[1].in_flight.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn every_arm_counts_against_its_endpoint_while_it_is_held() {
        let resolution = resolution(
            Strategy::RoundRobin,
            &[("one.example.com", 1, None), ("two.example.com", 1, None)],
        );
        let (dispatcher, _folding) = Dispatcher::new();
        let plan = resolution.plan();
        let arms =
            dispatcher.choose_many(&Dispatching::new(&resolution, None), Hedge::new(2).unwrap());
        assert_eq!(plan.load[0].in_flight.load(Ordering::Relaxed), 1);
        assert_eq!(plan.load[1].in_flight.load(Ordering::Relaxed), 1);
        drop(arms);
        assert_eq!(plan.load[0].in_flight.load(Ordering::Relaxed), 0);
        assert_eq!(plan.load[1].in_flight.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn extra_arms_rotate_through_every_live_endpoint() {
        let targets = vec![
            Weighted::new(endpoint("one.example.com", "/v1")),
            Weighted::new(endpoint("two.example.com", "/v1")),
            Weighted::new(endpoint("three.example.com", "/v1")),
        ];
        let plan = Plan::of(&targets);
        let mut drawn = Vec::new();
        for _ in 0..6 {
            let mut arms = vec![0usize];
            extra_arms(&plan, plan.tier(None), &mut arms, 2);
            drawn.push(arms[1]);
        }
        // The same first arm, repeated calls: the extra arm covers every live
        // endpoint before it repeats.
        assert_eq!(drawn, [1, 2, 1, 2, 1, 2]);
    }

    #[test]
    fn a_ratio_rule_spreads_the_insurance_across_its_endpoints() {
        let resolution = resolution(
            Strategy::Ratio,
            &[
                ("one.example.com", 1, None),
                ("two.example.com", 1, None),
                ("three.example.com", 1, None),
            ],
        );
        let (dispatcher, _folding) = Dispatcher::new();
        let mut carried: HashMap<String, usize> = HashMap::new();
        for _ in 0..300 {
            for arm in
                dispatcher.choose_many(&Dispatching::new(&resolution, None), Hedge::new(2).unwrap())
            {
                *carried
                    .entry(arm.endpoint().host.as_str().to_owned())
                    .or_insert(0) += 1;
            }
        }
        // One draw in three makes a first arm and one rotation in three makes an
        // extra arm, so each endpoint is in about two races in three of 300; the
        // old walk left the last endpoint in one in three. The bound is the middle
        // of the two, well away from either side because the draw is random and the
        // rotation alternates, so it cannot flake.
        for host in ["one.example.com", "two.example.com", "three.example.com"] {
            assert!(carried[host] > 150, "{host}: {carried:?}");
        }
    }

    /// The headers `may_race` decides on, built directly.
    fn heads(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        headers
    }

    #[test]
    fn a_rule_that_does_not_hedge_is_never_raced() {
        for headers in [
            heads(&[]),
            heads(&[("content-length", "10")]),
            heads(&[
                ("content-type", "multipart/form-data"),
                ("content-length", "10"),
            ]),
        ] {
            assert_eq!(may_race(&headers, Hedge::NONE), Hedge::NONE);
        }
    }

    #[test]
    fn a_hedging_rule_races_a_small_declared_body_at_what_it_asked_for() {
        let asked = Hedge::new(3).unwrap();
        let headers = heads(&[("content-length", "1000")]);
        assert_eq!(may_race(&headers, asked), asked);
    }

    #[test]
    fn a_body_exactly_at_the_cap_is_raced_and_one_byte_over_is_not() {
        let asked = Hedge::new(2).unwrap();
        let at_cap = heads(&[("content-length", &MAX_RACED_BODY.to_string())]);
        assert_eq!(may_race(&at_cap, asked), asked);
        let over = heads(&[("content-length", &(MAX_RACED_BODY + 1).to_string())]);
        assert_eq!(may_race(&over, asked), Hedge::NONE);
    }

    #[test]
    fn an_unknown_size_is_not_raced() {
        let asked = Hedge::new(2).unwrap();
        let chunked = heads(&[("transfer-encoding", "chunked")]);
        assert_eq!(may_race(&chunked, asked), Hedge::NONE);
        let junk = heads(&[("content-length", "n/a")]);
        assert_eq!(may_race(&junk, asked), Hedge::NONE);
    }

    #[test]
    fn a_file_transport_is_not_raced_whatever_its_length() {
        let asked = Hedge::new(2).unwrap();
        for (name, content_type) in [
            ("content-type", "multipart/form-data"),
            ("content-type", "multipart/form-data; boundary=abc123"),
            ("Content-Type", "Multipart/Mixed; charset=utf-8"),
        ] {
            let headers = heads(&[(name, content_type), ("content-length", "10")]);
            assert_eq!(
                may_race(&headers, asked),
                Hedge::NONE,
                "'{name}: {content_type}'"
            );
        }
    }

    #[test]
    fn a_request_with_no_body_is_raced_when_the_rule_asks() {
        let asked = Hedge::new(2).unwrap();
        let bare = heads(&[]);
        assert_eq!(may_race(&bare, asked), asked);
        let empty = heads(&[("content-length", "0")]);
        assert_eq!(may_race(&empty, asked), asked);
    }

    #[test]
    fn the_least_loaded_endpoint_is_the_one_carrying_least() {
        let resolution = resolution(
            Strategy::LeastLoad,
            &[("one.example.com", 1, None), ("two.example.com", 1, None)],
        );
        let (dispatcher, _folding) = Dispatcher::new();
        // Nothing is in flight, so the first is as good as the second.
        let held = dispatcher.choose(&Dispatching::new(&resolution, None));
        assert_eq!(held.endpoint().host.as_str(), "one.example.com");
        // With that one carrying a request, the next goes elsewhere.
        assert_eq!(
            dispatcher
                .choose(&Dispatching::new(&resolution, None))
                .endpoint()
                .host
                .as_str(),
            "two.example.com"
        );
        drop(held);
    }

    #[test]
    fn a_request_that_is_done_stops_counting_against_its_endpoint() {
        let resolution = resolution(
            Strategy::LeastLoad,
            &[("one.example.com", 1, None), ("two.example.com", 1, None)],
        );
        let (dispatcher, _folding) = Dispatcher::new();
        drop(dispatcher.choose(&Dispatching::new(&resolution, None)));
        // The first is free again, and answered quickly, so it is still the lightest.
        assert_eq!(
            dispatcher
                .choose(&Dispatching::new(&resolution, None))
                .endpoint()
                .host
                .as_str(),
            "one.example.com"
        );
    }

    #[tokio::test]
    async fn an_endpoint_that_has_been_slow_carries_less() {
        let resolution = resolution(
            Strategy::LeastLoad,
            &[
                ("slow.example.com", 1, None),
                ("quick.example.com", 1, None),
            ],
        );
        let (dispatcher, mut folding) = Dispatcher::new();
        // The first request goes to the first endpoint, both being unknown, and takes a while.
        let held = dispatcher.choose(&Dispatching::new(&resolution, None));
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
        let resolution = resolution(Strategy::LeastLoad, &[("only.example.com", 1, None)]);
        let (dispatcher, mut folding) = Dispatcher::new();
        for _ in 0..40 {
            drop(dispatcher.choose(&Dispatching::new(&resolution, None)));
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

    #[tokio::test]
    async fn an_abandoned_arm_teaches_no_latency_to_the_folder() {
        let resolution = resolution(Strategy::LeastLoad, &[("only.example.com", 1, None)]);
        let (dispatcher, mut folding) = Dispatcher::new();
        let mut arm = dispatcher.choose(&Dispatching::new(&resolution, None));
        arm.abandon();
        std::thread::sleep(std::time::Duration::from_millis(20));
        drop(arm);
        drop(dispatcher); // No sender left, so the fold can answer at all.
        assert_eq!(folding.fold().await, 0);
        assert_eq!(
            resolution.plan().taking(0),
            None,
            "an arm that never answered should have taught nothing"
        );
    }

    #[tokio::test]
    async fn an_abandoned_arm_leaves_a_recorded_latency_unchanged() {
        let resolution = resolution(Strategy::LeastLoad, &[("only.example.com", 1, None)]);
        let (dispatcher, mut folding) = Dispatcher::new();
        let held = dispatcher.choose(&Dispatching::new(&resolution, None));
        std::thread::sleep(std::time::Duration::from_millis(30));
        drop(held);
        assert_eq!(folding.fold().await, 1);
        let recorded = resolution
            .plan()
            .taking(0)
            .expect("the first answer was folded in");

        let mut arm = dispatcher.choose(&Dispatching::new(&resolution, None));
        arm.abandon();
        std::thread::sleep(std::time::Duration::from_millis(10));
        drop(arm);
        drop(dispatcher);
        assert_eq!(folding.fold().await, 0);
        assert_eq!(resolution.plan().taking(0), Some(recorded));
    }

    #[test]
    fn an_abandoned_arm_still_comes_off_the_in_flight_count() {
        let resolution = resolution(Strategy::LeastLoad, &[("only.example.com", 1, None)]);
        let (dispatcher, _folding) = Dispatcher::new();
        let plan = resolution.plan();
        let mut arm = dispatcher.choose(&Dispatching::new(&resolution, None));
        assert_eq!(plan.load[0].in_flight.load(Ordering::Relaxed), 1);
        arm.abandon();
        drop(arm);
        assert_eq!(plan.load[0].in_flight.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn abandoning_one_arm_leaves_the_other_arms_reporting() {
        let resolution = resolution(
            Strategy::LeastLoad,
            &[("one.example.com", 1, None), ("two.example.com", 1, None)],
        );
        let (dispatcher, mut folding) = Dispatcher::new();
        let mut arms =
            dispatcher.choose_many(&Dispatching::new(&resolution, None), Hedge::new(2).unwrap());
        arms[0].abandon();
        std::thread::sleep(std::time::Duration::from_millis(20));
        drop(arms);
        assert_eq!(folding.fold().await, 1);
        let plan = resolution.plan();
        assert_eq!(plan.taking(0), None, "the abandoned arm taught nothing");
        assert!(plan.taking(1).is_some(), "the held arm still reported");
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
            hedge: ip_core::Hedge::default(),
        }]);
        let key = RouteKey::new(endpoint("gateway.local", "/anthropic"));
        let (dispatcher, _folding) = Dispatcher::new();

        // Each request resolves afresh, and the turn is the rule's rather than the resolution's.
        let mut sent = Vec::new();
        for _ in 0..4 {
            let resolution = table.resolve(&key).unwrap();
            sent.push(
                dispatcher
                    .choose(&Dispatching::new(&resolution, None))
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
    fn a_plan_narrows_each_class_to_what_can_serve_it() {
        let targets = vec![
            Weighted::serving(
                endpoint("easy.example.com", "/v1"),
                Weight::new(1),
                Some(Difficulty::Easy),
            ),
            Weighted::serving(
                endpoint("hard.example.com", "/v1"),
                Weight::new(1),
                Some(Difficulty::Hard),
            ),
            Weighted::serving(endpoint("silent.example.com", "/v1"), Weight::new(1), None),
        ];
        let plan = Plan::of(&targets);
        assert_eq!(plan.tier(None).at, [0, 1, 2]);
        assert_eq!(plan.tier(Some(Difficulty::Easy)).at, [0]);
        // The next declared ceiling above what is asked, with the silent one behind it.
        assert_eq!(plan.tier(Some(Difficulty::Routine)).at, [1]);
        assert_eq!(plan.tier(Some(Difficulty::Hard)).at, [1]);
        // Nothing declared carries research, so the silent endpoint serves it.
        assert_eq!(plan.tier(Some(Difficulty::Research)).at, [2]);
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

    /// The endpoint a turn remembers having landed on.
    fn replica(host: &str) -> Replica {
        Replica::of(&endpoint(host, "/v1"))
    }

    /// Where a request went when its turn landed on `landed_on` before it.
    fn sent_landing(
        dispatcher: &Dispatcher,
        resolution: &Resolution,
        class: Option<Difficulty>,
        landed_on: &Replica,
    ) -> String {
        let chosen =
            dispatcher.choose(&Dispatching::new(resolution, class).landed_on(Some(landed_on)));
        chosen.endpoint().host.as_str().to_owned()
    }

    #[test]
    fn a_turn_lands_where_the_turn_before_it_landed() {
        let resolution = resolution(
            Strategy::RoundRobin,
            &[("one.example.com", 1, None), ("two.example.com", 1, None)],
        );
        let (dispatcher, _folding) = Dispatcher::new();
        for _ in 0..3 {
            assert_eq!(
                sent_landing(&dispatcher, &resolution, None, &replica("two.example.com")),
                "two.example.com",
                "an unclassified turn moves off the endpoint it landed on"
            );
        }
    }

    #[test]
    fn a_request_that_marks_no_turn_is_dispatched_as_it_always_was() {
        // The regression of §429 over all three strategies: a request landed nowhere is a
        // request untouched. The rotation of a round-robin is the sharpest witness, so it is
        // held exact; the other two are held to choosing only among what the rule stands
        // behind, as every test above them already holds their own shape.
        for strategy in [Strategy::RoundRobin, Strategy::LeastLoad, Strategy::Ratio] {
            let resolution = resolution(
                strategy,
                &[
                    ("one.example.com", 1, None),
                    ("two.example.com", 1, None),
                    ("three.example.com", 1, None),
                ],
            );
            let (dispatcher, _folding) = Dispatcher::new();
            let hosts = (0..6)
                .map(|_| {
                    dispatcher
                        .choose(&Dispatching::new(&resolution, None).landed_on(None))
                        .endpoint()
                        .host
                        .as_str()
                        .to_owned()
                })
                .collect::<Vec<_>>();
            if strategy == Strategy::RoundRobin {
                assert_eq!(
                    hosts,
                    [
                        "one.example.com",
                        "two.example.com",
                        "three.example.com",
                        "one.example.com",
                        "two.example.com",
                        "three.example.com",
                    ]
                );
            } else {
                for host in &hosts {
                    assert!(
                        ["one.example.com", "two.example.com", "three.example.com"]
                            .contains(&host.as_str()),
                        "{host} is behind no rule"
                    );
                }
            }
        }
    }

    #[test]
    fn a_landing_does_not_spend_the_rule_turn_it_landed_on() {
        // A landed request goes without consulting the strategy, so the strategy resumes where
        // it was, not a turn further on.
        let resolution = resolution(
            Strategy::RoundRobin,
            &[("one.example.com", 1, None), ("two.example.com", 1, None)],
        );
        let (dispatcher, _folding) = Dispatcher::new();
        for _ in 0..3 {
            sent_landing(&dispatcher, &resolution, None, &replica("two.example.com"));
        }
        assert_eq!(
            sent_to(&dispatcher, &resolution, 4),
            [
                "one.example.com",
                "two.example.com",
                "one.example.com",
                "two.example.com",
            ]
        );
    }

    #[test]
    fn a_ceiling_below_the_class_asked_is_not_where_the_turn_lands() {
        let resolution = resolution(
            Strategy::RoundRobin,
            &[
                ("easy.example.com", 1, Some(Difficulty::Easy)),
                ("hard.example.com", 1, Some(Difficulty::Hard)),
            ],
        );
        let (dispatcher, _folding) = Dispatcher::new();
        assert_eq!(
            sent_landing(
                &dispatcher,
                &resolution,
                Some(Difficulty::Hard),
                &replica("easy.example.com"),
            ),
            "hard.example.com",
            "a turn was kept on an endpoint that cannot serve what it now asks"
        );
        // A ceiling that covers what is asked, at or above it, is where the turn stays.
        assert_eq!(
            sent_landing(
                &dispatcher,
                &resolution,
                Some(Difficulty::Hard),
                &replica("hard.example.com"),
            ),
            "hard.example.com"
        );
        assert_eq!(
            sent_landing(
                &dispatcher,
                &resolution,
                Some(Difficulty::Easy),
                &replica("easy.example.com"),
            ),
            "easy.example.com"
        );
    }

    #[test]
    fn a_higher_ceiling_than_the_class_asked_is_where_the_turn_lands_too() {
        // The tiers narrow by the lowest ceiling that covers the class; a turn already on a
        // higher one is not dragged out of it, because the question §429 asks of the endpoint
        // is whether it can serve, not whether it was the cheapest that could.
        let resolution = resolution(
            Strategy::RoundRobin,
            &[
                ("hard.example.com", 1, Some(Difficulty::Hard)),
                ("research.example.com", 1, Some(Difficulty::Research)),
            ],
        );
        let (dispatcher, _folding) = Dispatcher::new();
        assert_eq!(
            sent_landing(
                &dispatcher,
                &resolution,
                Some(Difficulty::Hard),
                &replica("research.example.com"),
            ),
            "research.example.com"
        );
    }

    #[test]
    fn a_drained_landing_is_no_landing() {
        let resolution = resolution(
            Strategy::RoundRobin,
            &[("one.example.com", 0, None), ("two.example.com", 1, None)],
        );
        let (dispatcher, _folding) = Dispatcher::new();
        assert_eq!(
            sent_landing(&dispatcher, &resolution, None, &replica("one.example.com")),
            "two.example.com",
            "a turn was kept on an endpoint that has since drained"
        );
    }

    #[test]
    fn a_rewritten_rule_lands_the_turn_fresh() {
        let resolution = resolution(
            Strategy::RoundRobin,
            &[("one.example.com", 1, None), ("two.example.com", 1, None)],
        );
        let (dispatcher, _folding) = Dispatcher::new();
        assert_eq!(
            sent_landing(&dispatcher, &resolution, None, &replica("gone.example.com")),
            "one.example.com",
            "a rule rewritten away from its turn's endpoint must land the turn afresh"
        );
    }

    #[test]
    fn what_a_race_may_hold_is_what_a_turn_may_read_from() {
        let asked = Hedge::new(2).unwrap();
        for (name, headers) in [
            ("a small declared body", heads(&[("content-length", "40")])),
            ("an undeclared body", heads(&[])),
        ] {
            assert!(may_hold(&headers), "{name} is holdable");
            assert_eq!(may_race(&headers, asked), asked, "{name} may race");
        }
        for (name, headers) in [
            ("past the cap", heads(&[("content-length", "2000000")])),
            (
                "a file transport",
                heads(&[("content-type", "multipart/form-data")]),
            ),
            (
                "chunked with no length",
                heads(&[("transfer-encoding", "chunked")]),
            ),
        ] {
            assert!(!may_hold(&headers), "{name} is not holdable");
            assert_eq!(
                may_race(&headers, asked),
                Hedge::NONE,
                "{name} may not race"
            );
        }
    }

    #[test]
    fn a_race_starts_where_the_turn_landed() {
        let resolution = resolution(
            Strategy::RoundRobin,
            &[
                ("one.example.com", 1, None),
                ("two.example.com", 1, None),
                ("three.example.com", 1, None),
            ],
        );
        let (dispatcher, _folding) = Dispatcher::new();
        let arms = arms_by_class(&resolution, None, Hedge::new(3).unwrap());
        assert_ne!(
            arms[0], "two.example.com",
            "the test wants a first arm to move"
        );
        let raced = dispatcher
            .choose_many(
                &Dispatching::new(&resolution, None).landed_on(Some(&replica("two.example.com"))),
                Hedge::new(3).unwrap(),
            )
            .iter()
            .map(|arm| arm.endpoint().host.as_str().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(raced[0], "two.example.com");
        assert_eq!(
            raced.len(),
            3,
            "the rest of the race was thinned by the memory: {raced:?}"
        );
        let mut rest: Vec<_> = raced[1..].to_vec();
        rest.sort();
        assert_eq!(rest, ["one.example.com", "three.example.com"]);
    }

    #[test]
    fn a_sticky_race_still_races_the_rest() {
        let resolution = resolution(
            Strategy::RoundRobin,
            &[("one.example.com", 1, None), ("two.example.com", 1, None)],
        );
        let (dispatcher, _folding) = Dispatcher::new();
        let raced = dispatcher
            .choose_many(
                &Dispatching::new(&resolution, None).landed_on(Some(&replica("two.example.com"))),
                Hedge::new(2).unwrap(),
            )
            .iter()
            .map(|arm| arm.endpoint().host.as_str().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(raced, ["two.example.com", "one.example.com"]);
    }
}
