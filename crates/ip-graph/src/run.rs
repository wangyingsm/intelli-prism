//! Running a graph: one generation at a time, each node on what its parents answered.
//!
//! The order comes from the plane and the nodes of one generation have no edge between them, so a
//! generation is run at once and the next waits for the whole of it. What a node is given is a
//! json object keyed by the parents that fed it, which is how a node's prompt asks for one answer
//! out of several by the name the graph calls that answer's node.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use ip_core::TraceId;
use ip_run::{RunSignal, Spend, Stopped};
use serde_json::{Map, Value, json};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::graph::{GraphNode, RunGraph};
use crate::id::NodeName;
use crate::order::RunOrder;

/// What one node's run came to.
///
/// [`Spend`] and [`Stopped`] are `ip-run`'s own types rather than this crate's: a graph is a second
/// place a run is accounted for, and a second accounting is the one most likely to disagree with
/// the first.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeRan {
    /// What the node answered.
    pub answer: String,
    /// Why its run ended.
    pub stopped: Stopped,
    /// What it spent.
    pub spend: Spend,
    /// What each of its turns is followed under.
    pub traces: Vec<TraceId>,
}

/// Whatever runs one node of a graph.
///
/// A seam rather than a direct call to `ip_run::run`, because running a node needs the
/// deployment's own gateway — its routing, its limits, its filter, its prices — which is the
/// binary's to hold and not a graph's. A graph says which template to run on what input; an
/// implementation of this says how. It is also what lets the rules below — the keying, the
/// concurrency, the stop — be tested without an upstream, a store or a server.
#[async_trait]
pub trait RunsNode: Send + Sync {
    /// Runs one node's template on the input the graph built for it.
    ///
    /// Answers with what the run came to, never with an error: a node that was refused partway is
    /// a node that ran, and whether that stops the graph is [`run`]'s to decide.
    ///
    /// The signal is the graph's word to a node that is already running, which an implementation
    /// hands to `ip_run::run` and does not read itself. It is what keeps a sibling of a node that
    /// broke from going on taking turns nobody will read.
    async fn run(
        &self,
        node: GraphNode,
        input: String,
        signals: watch::Receiver<RunSignal>,
    ) -> NodeRan;
}

/// A graph, the order it runs in, and whatever runs one node of it.
pub struct Graphing {
    /// The graph to run.
    pub graph: RunGraph,
    /// The order its nodes run in, as the plane answered it.
    pub order: RunOrder,
    /// What runs one node.
    pub runner: Arc<dyn RunsNode>,
    /// What the graph as a whole was asked.
    pub input: String,
}

/// What running a whole graph came to.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphRan {
    /// The graph's answer: what each node nothing reads from said, keyed by its name.
    ///
    /// Keyed even where a graph has one such node, for the reason a node's input is keyed even
    /// where it has one parent: a shape that is always the same is one a caller can read without
    /// knowing which graph it asked.
    pub answer: Value,
    /// What each node that ran came to.
    pub nodes: BTreeMap<NodeName, NodeRan>,
    /// The node that stopped the graph, where one did.
    pub stopped_at: Option<NodeName>,
    /// What the whole graph spent, every node summed.
    pub spend: Spend,
    /// What every turn of every node is followed under, in the order the nodes finished.
    pub traces: Vec<TraceId>,
}

/// What a node is given: its parents' answers, keyed by the name the graph calls each parent.
///
/// A node nothing feeds is given the graph's own input, since there is no parent to key it by.
fn input_for(
    graphing: &Graphing,
    name: &NodeName,
    answered: &BTreeMap<NodeName, NodeRan>,
) -> String {
    let mut from: Map<String, Value> = Map::new();
    for parent in graphing.graph.parents_of(name) {
        let answer = answered
            .get(parent)
            .map_or_else(String::new, |ran| ran.answer.clone());
        from.insert(parent.to_string(), Value::String(answer));
    }
    if from.is_empty() {
        return graphing.input.clone();
    }
    Value::Object(from).to_string()
}

/// The nodes nothing reads from, which between them are what the graph answered.
fn answer_of(graph: &RunGraph, answered: &BTreeMap<NodeName, NodeRan>) -> Value {
    let read_from: BTreeSet<&NodeName> = graph.edges().iter().map(|edge| &edge.from).collect();
    let mut answer = Map::new();
    for node in graph.nodes() {
        if read_from.contains(&node.name) {
            continue;
        }
        if let Some(ran) = answered.get(&node.name) {
            answer.insert(node.name.to_string(), Value::String(ran.answer.clone()));
        }
    }
    Value::Object(answer)
}

/// Runs the graph, a generation at a time, until it is through or a node stops it.
///
/// A node that does not end [`Stopped::Answered`] stops the graph: nothing further is started, its
/// siblings are told to stop, and what is already running is awaited rather than abandoned. The
/// three are one decision. Abandoning a sibling mid-turn would lose the tokens it had already
/// asked for *and* the record of them, so it is awaited; but letting it run its template out would
/// pay for turns nobody will read, so it is told. Between turns is where those two meet, and
/// [`RunSignal`] is what carries it.
///
/// A sibling stopped that way comes back [`Stopped::CutShort`] and is never named as the cause:
/// [`GraphRan::stopped_at`] is the node that broke, not the ones that were stopped because it did.
/// The result carries everything spent up to that point — the shape `ip_run::Ran` has for the same
/// reason, that a run refused partway is still a run.
pub async fn run(graphing: &Graphing) -> GraphRan {
    let mut answered: BTreeMap<NodeName, NodeRan> = BTreeMap::new();
    let mut done: BTreeSet<NodeName> = BTreeSet::new();
    let mut traces: Vec<TraceId> = Vec::new();
    let mut spend = Spend::NOTHING;
    let mut stopped_at = None;
    let (stop, listen) = watch::channel(RunSignal::Carry);

    while stopped_at.is_none() {
        let ready: Vec<NodeName> = graphing
            .order
            .ready_after(&done)
            .into_iter()
            .cloned()
            .collect();
        if ready.is_empty() {
            break;
        }
        let mut running = JoinSet::new();
        for name in ready {
            let Some(node) = graphing
                .graph
                .nodes()
                .iter()
                .find(|node| node.name == name)
                .cloned()
            else {
                // An order the graph does not match is `GraphPlane::order`'s to refuse; here it
                // can only mean the two were taken from different graphs.
                continue;
            };
            let input = input_for(graphing, &name, &answered);
            let runner = Arc::clone(&graphing.runner);
            let listen = listen.clone();
            running.spawn(async move { (name, runner.run(node, input, listen).await) });
        }

        // Every one of them is awaited, the stop included: a node already running is paid for.
        while let Some(finished) = running.join_next().await {
            let Ok((name, ran)) = finished else {
                continue;
            };
            spend = spend + ran.spend;
            traces.extend(ran.traces.iter().cloned());
            if !matches!(ran.stopped, Stopped::Answered | Stopped::CutShort) && stopped_at.is_none()
            {
                stopped_at = Some(name.clone());
                // Said while the rest of the generation is still running, which is the only
                // moment it can save a turn.
                let _ = stop.send(RunSignal::Stop);
            }
            done.insert(name.clone());
            answered.insert(name, ran);
        }
    }

    GraphRan {
        answer: match stopped_at {
            Some(_) => json!({}),
            None => answer_of(&graphing.graph, &answered),
        },
        nodes: answered,
        stopped_at,
        spend,
        traces,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use ip_agents::AgentId;
    use ip_core::{Money, TenantId, TokenCount, Tokens};

    use super::*;
    use crate::graph::{GraphEdge, GraphOwner, MaxNodes, RunGraphParts};
    use crate::id::{GraphId, GraphName};
    use crate::order::{Generation, Placed};

    /// Long enough that a generation run one node at a time takes twice it, and short enough that
    /// a suite does not notice. The lesson it encodes: an assertion on the *order* nodes finished
    /// in cannot see whether they ran at once, and only a clock can.
    const SLOW: Duration = Duration::from_millis(120);

    /// A runner that answers with the input it was given, so a test can read what was keyed.
    ///
    /// It records when each node started and finished, which is what the concurrency case reads.
    struct Echo {
        /// How long each turn takes.
        takes: Duration,
        /// How many turns a node takes before it answers.
        turns: u32,
        /// Nodes that stop the graph rather than answering, and why.
        refuses: BTreeSet<String>,
        /// What ran, and when, in the order it finished.
        seen: Mutex<Vec<(String, Instant, Instant)>>,
        /// When the first node started, which every span below is measured from.
        opened: Instant,
    }

    impl Echo {
        fn new(takes: Duration, turns: u32, refuses: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                takes,
                turns,
                refuses: refuses.iter().map(|name| (*name).to_owned()).collect(),
                seen: Mutex::new(Vec::new()),
                opened: Instant::now(),
            })
        }

        /// How long the whole run took, by the clock the runner itself kept.
        fn spans(&self) -> Vec<(String, Duration, Duration)> {
            self.seen
                .lock()
                .expect("the record is not poisoned")
                .iter()
                .map(|(name, from, to)| {
                    (
                        name.clone(),
                        from.duration_since(self.opened),
                        to.duration_since(self.opened),
                    )
                })
                .collect()
        }
    }

    #[async_trait]
    impl RunsNode for Echo {
        /// Takes its turns one sleep at a time, reading the signal before each one as
        /// `ip_run::run` does, so that a node told to stop reports [`Stopped::CutShort`] having
        /// taken the turns before the word reached it and no more.
        ///
        /// A node that refuses does so on its first turn, before a sibling has reached its
        /// second: otherwise which of the two gets there first is a race, and a test of the
        /// signal would pass or fail on the scheduler.
        async fn run(
            &self,
            node: GraphNode,
            input: String,
            signals: watch::Receiver<RunSignal>,
        ) -> NodeRan {
            let from = Instant::now();
            let refused = self.refuses.contains(node.name.as_str());
            let mut cut = false;
            if !refused {
                for turn in 0..self.turns {
                    if turn > 0 && *signals.borrow() == RunSignal::Stop {
                        cut = true;
                        break;
                    }
                    tokio::time::sleep(self.takes).await;
                }
            }
            let to = Instant::now();
            self.seen.lock().expect("the record is not poisoned").push((
                node.name.to_string(),
                from,
                to,
            ));
            NodeRan {
                answer: format!("{} saw {input}", node.name),
                stopped: if refused {
                    Stopped::Refused {
                        why: "the fixture refuses this node".to_owned(),
                    }
                } else if cut {
                    Stopped::CutShort
                } else {
                    Stopped::Answered
                },
                spend: Spend::NOTHING,
                traces: Vec::new(),
            }
        }
    }

    fn name(raw: &str) -> NodeName {
        NodeName::new(raw).expect("a name a node may carry")
    }

    fn node(raw: &str) -> GraphNode {
        GraphNode {
            name: name(raw),
            agent: AgentId::new(raw).expect("a template id"),
        }
    }

    fn edge(from: &str, to: &str) -> GraphEdge {
        GraphEdge {
            from: name(from),
            to: name(to),
        }
    }

    fn graph(nodes: &[&str], edges: &[(&str, &str)]) -> RunGraph {
        RunGraph::checked(RunGraphParts {
            id: GraphId::new("triage").expect("a graph id"),
            name: GraphName::new("Support triage").expect("a graph name"),
            owner: GraphOwner::Tenant(TenantId::new("acme").expect("a tenant id")),
            nodes: nodes.iter().map(|raw| node(raw)).collect(),
            edges: edges.iter().map(|(from, to)| edge(from, to)).collect(),
            max_nodes: MaxNodes::new(8).expect("a ceiling"),
        })
        .expect("the fixture's own graph holds together")
    }

    fn order(placed: &[(&str, u32)]) -> RunOrder {
        RunOrder::checked(
            placed
                .iter()
                .map(|&(raw, generation)| Placed {
                    name: name(raw),
                    generation: Generation::new(generation),
                })
                .collect(),
        )
        .expect("an order placing each node once")
    }

    fn graphing(graph: RunGraph, order: RunOrder, runner: Arc<Echo>) -> Graphing {
        Graphing {
            graph,
            order,
            runner,
            input: "a ticket".to_owned(),
        }
    }

    #[tokio::test]
    async fn a_chain_passes_one_answer_to_the_next() {
        let ran = run(&graphing(
            graph(&["intake", "reply"], &[("intake", "reply")]),
            order(&[("intake", 0), ("reply", 1)]),
            Echo::new(Duration::ZERO, 1, &[]),
        ))
        .await;
        assert_eq!(
            ran.nodes[&name("intake")].answer,
            "intake saw a ticket",
            "the first node was not given the graph's own input"
        );
        assert_eq!(
            ran.nodes[&name("reply")].answer,
            r#"reply saw {"intake":"intake saw a ticket"}"#,
            "the second node was not given its parent's answer, keyed"
        );
        assert_eq!(ran.stopped_at, None);
    }

    /// Ruling 2: one parent is an object with one key, so a prompt asks the same way however many
    /// parents a node has.
    #[tokio::test]
    async fn a_fan_in_is_given_every_parent_under_its_own_name() {
        let ran = run(&graphing(
            graph(
                &["left", "right", "merge"],
                &[("left", "merge"), ("right", "merge")],
            ),
            order(&[("left", 0), ("right", 0), ("merge", 1)]),
            Echo::new(Duration::ZERO, 1, &[]),
        ))
        .await;
        let saw = &ran.nodes[&name("merge")].answer;
        assert!(saw.contains(r#""left":"left saw a ticket""#), "{saw}");
        assert!(saw.contains(r#""right":"right saw a ticket""#), "{saw}");
    }

    /// Asserted by the clock, not by the order the two finished in: an order assertion cannot see
    /// whether two nodes overlapped.
    #[tokio::test]
    async fn the_middle_of_a_diamond_runs_at_once_rather_than_in_turn() {
        let runner = Echo::new(SLOW, 1, &[]);
        let whole = Instant::now();
        run(&graphing(
            graph(
                &["intake", "left", "right", "merge"],
                &[
                    ("intake", "left"),
                    ("intake", "right"),
                    ("left", "merge"),
                    ("right", "merge"),
                ],
            ),
            order(&[("intake", 0), ("left", 1), ("right", 1), ("merge", 2)]),
            Arc::clone(&runner),
        ))
        .await;
        let took = whole.elapsed();
        assert!(
            took < SLOW * 4,
            "four nodes in three generations took {took:?}, which is one at a time"
        );
        let spans = runner.spans();
        let middle: Vec<&(String, Duration, Duration)> = spans
            .iter()
            .filter(|(name, _, _)| name == "left" || name == "right")
            .collect();
        assert_eq!(middle.len(), 2);
        assert!(
            middle[0].1 < middle[1].2 && middle[1].1 < middle[0].2,
            "the middle pair did not overlap: {middle:?}"
        );
    }

    /// Ruling 3: nothing further starts, and what is running is awaited rather than abandoned.
    #[tokio::test]
    async fn a_refused_node_stops_the_graph_and_its_sibling_still_finishes() {
        let runner = Echo::new(SLOW, 2, &["left"]);
        let ran = run(&graphing(
            graph(
                &["left", "right", "merge"],
                &[("left", "merge"), ("right", "merge")],
            ),
            order(&[("left", 0), ("right", 0), ("merge", 1)]),
            Arc::clone(&runner),
        ))
        .await;
        assert_eq!(ran.stopped_at, Some(name("left")));
        assert!(
            ran.nodes.contains_key(&name("right")),
            "the sibling already running was abandoned"
        );
        assert!(
            !ran.nodes.contains_key(&name("merge")),
            "the graph went on past the node that stopped it"
        );
        assert_eq!(
            runner.spans().len(),
            2,
            "a node was started after the graph had stopped"
        );
        assert_eq!(
            ran.nodes[&name("right")].stopped,
            Stopped::CutShort,
            "the sibling was left to run its template out"
        );
        assert_eq!(
            ran.stopped_at,
            Some(name("left")),
            "the sibling that was cut short was named as the cause"
        );
    }

    /// The whole point of the signal: a sibling of a node that broke takes no further turn.
    #[tokio::test]
    async fn a_sibling_of_a_node_that_broke_stops_before_its_next_turn() {
        let runner = Echo::new(SLOW, 2, &["left"]);
        run(&graphing(
            graph(
                &["left", "right", "merge"],
                &[("left", "merge"), ("right", "merge")],
            ),
            order(&[("left", 0), ("right", 0), ("merge", 1)]),
            Arc::clone(&runner),
        ))
        .await;
        let spans = runner.spans();
        let right = spans
            .iter()
            .find(|(name, _, _)| name == "right")
            .expect("the sibling ran");
        assert!(
            right.2 - right.1 < SLOW * 2,
            "the sibling took both of its turns: {right:?}"
        );
    }

    #[tokio::test]
    async fn a_graph_answers_with_every_node_nothing_reads_from() {
        let ran = run(&graphing(
            graph(
                &["intake", "left", "right"],
                &[("intake", "left"), ("intake", "right")],
            ),
            order(&[("intake", 0), ("left", 1), ("right", 1)]),
            Echo::new(Duration::ZERO, 1, &[]),
        ))
        .await;
        assert_eq!(
            ran.answer,
            json!({
                "left": "left saw {\"intake\":\"intake saw a ticket\"}",
                "right": "right saw {\"intake\":\"intake saw a ticket\"}",
            }),
            "a graph with two ends answered with one of them"
        );
    }

    #[tokio::test]
    async fn a_graph_that_was_stopped_answers_with_nothing_rather_than_half() {
        let ran = run(&graphing(
            graph(&["intake", "reply"], &[("intake", "reply")]),
            order(&[("intake", 0), ("reply", 1)]),
            Echo::new(Duration::ZERO, 1, &["intake"]),
        ))
        .await;
        assert_eq!(ran.stopped_at, Some(name("intake")));
        assert_eq!(ran.answer, json!({}));
    }

    #[tokio::test]
    async fn what_a_graph_spent_is_every_node_of_it_summed() {
        /// A runner that spends one token of each kind per node.
        struct Costs;

        #[async_trait]
        impl RunsNode for Costs {
            async fn run(
                &self,
                _node: GraphNode,
                _input: String,
                _signals: watch::Receiver<RunSignal>,
            ) -> NodeRan {
                NodeRan {
                    answer: String::new(),
                    stopped: Stopped::Answered,
                    spend: Spend::new(
                        Tokens::new(TokenCount::new(2), TokenCount::new(3)),
                        Money::ZERO,
                    ),
                    traces: Vec::new(),
                }
            }
        }

        let ran = run(&Graphing {
            graph: graph(&["a", "b", "c"], &[("a", "b"), ("b", "c")]),
            order: order(&[("a", 0), ("b", 1), ("c", 2)]),
            runner: Arc::new(Costs),
            input: "a ticket".to_owned(),
        })
        .await;
        assert_eq!(ran.spend.tokens.input.get(), 6);
        assert_eq!(ran.spend.tokens.output.get(), 9);
    }
}
