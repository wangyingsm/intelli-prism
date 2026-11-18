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

use crate::error::GraphError;
use crate::event::{GraphEvent, GraphReporting, GraphSaid, LoopRefusal};
use crate::graph::{GraphEdge, GraphNode, GraphOwner, LoopBack, RunGraph};
use crate::id::{Condition, GraphId, GraphRunId, NodeName};
use crate::order::RunOrder;
use crate::plane::GraphPlane;

/// What one node's run came to.
///
/// [`Spend`] and [`Stopped`] are `ip-run`'s own types rather than this crate's: a graph is a second
/// place a run is accounted for, and a second accounting is the one most likely to disagree with
/// the first.
///
/// It is what a node answers wherever a node runs, so it is also what comes back when one ran on
/// another machine — the same struct, not a wire copy of it, because nothing in it is a thing a
/// wire cannot carry.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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

/// One node's work, as whatever runs it is handed it.
///
/// A struct rather than four arguments because it is also the shape a dispatch takes: everything
/// a node needs to run travels *to* the work and its answer comes back, so a machine running a
/// node of somebody else's graph needs no access to that graph, its order, or what its siblings
/// answered. Only the coordinator holds those.
pub struct RunningNode {
    /// Which graph run this node belongs to.
    pub run: GraphRunId,
    /// The node to run.
    pub node: GraphNode,
    /// What the graph built for it: its parents' answers, keyed by name.
    pub input: String,
    /// What the graph can say to it while it runs.
    pub signals: watch::Receiver<RunSignal>,
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
    /// The signal in [`RunningNode`] is the graph's word to a node that is already running, which
    /// an implementation hands to `ip_run::run` and does not read itself. It is what keeps a
    /// sibling of a node that broke from going on taking turns nobody will read.
    async fn run(&self, running: RunningNode) -> NodeRan;
}

/// Whatever carries a graph's stop to the nodes of it that are not in this process.
///
/// A seam for the reason [`RunsNode`] is one: reaching the other members of a cluster is the
/// binary's to do and not a graph's. A graph knows only that it has stopped, and says so once.
///
/// The local stop is a [`watch`] channel and needs none of this — it is the nodes *elsewhere*
/// that cannot be reached by sharing a channel with them. A deployment with nobody else to tell
/// is told nothing, which is why [`Graphing::stopping`] is optional.
#[async_trait]
pub trait Stopping: Send + Sync {
    /// Says that this run has stopped.
    ///
    /// Answers nothing, because there is nothing a graph would do about a stop it could not
    /// pass on: the node it is stopping has already been paid for, and the run ends either way.
    /// An implementation that cannot reach the cluster says so in a log and returns.
    async fn stopped(&self, run: GraphRunId);
}

/// The key a node's answer asks to double back under.
///
/// A node unsatisfied with what a predecessor gave it answers `{"loop_back": "survey"}`. Whether
/// the loop happens is not its decision alone: the graph must declare it, and the cap must have
/// something left.
pub const LOOP_BACK_KEY: &str = "loop_back";

/// The key a node's answer names the branches it is taking under.
///
/// A list rather than a name, because a node may take any subset of the branches it declares: one,
/// several, or none at all. `{"branch": ["escalate", "notify"]}` takes both, and `{"branch": []}`
/// takes neither and ends the run with what that node said.
pub const BRANCH_KEY: &str = "branch";

/// The key a node's own run-again arrives under, holding what each node that sent work back said.
///
/// Keyed by the asking node, as fan-in is, because several nodes may declare a loop back to one
/// target and a run may have taken more than one of them. What sits under each name is everything
/// that node answered *except* [`LOOP_BACK_KEY`] — the ask is addressed to the engine and is not
/// the target's to read — with the attempt number put in beside it.
///
/// This is where a revision is asked for. A node that doubles back without saying what was wrong
/// has sent the work back to a node that will do the same thing again, so the amendment is the
/// point rather than an extra: `{"loop_back": "survey", "notes": "the figures are for the wrong
/// year"}` reaches the survey as `{"$again": {"review": {"notes": "…", "attempt": 2}}}`.
pub const AGAIN_KEY: &str = "$again";

/// The key a graph's own input arrives under when a node nothing feeds runs again.
///
/// A node nothing feeds is given the graph's input as it stands, there being no parent to key it
/// by. Once a loop back has been taken there *is* something to key by — the asking node — and its
/// answer must not replace the question the run was asked, so the two arrive together and the
/// input needs a key of its own. `$` is not a character a [`NodeName`] may hold, so this collides
/// with no node's name however a graph is written.
pub const GRAPH_INPUT_KEY: &str = "$input";

/// What a node that may send work back is told, beyond what its own template says.
///
/// The loop backs rather than the graph, because a member running one node of somebody else's
/// graph reads only those: it derives this for itself from the plane both machines share, so what
/// a node is told cannot differ between the machine that holds the graph and the machine that runs
/// the node.
///
/// Nothing, where the graph declares no loop back from it: a node that cannot double back is told
/// nothing about doubling back, so a template written for one graph is not read differently by
/// another.
///
/// **Why the engine writes this rather than the operator.** Judging whether an answer is good
/// enough is a role instruction, so it belongs in the prompt; but *which* steps this node may send
/// work back to, and how often, is in the graph, which the node never sees. An operator who wrote
/// it by hand would be copying the graph into prose and would have to keep the two in step —
/// renaming a node, or changing a cap, would leave a template confidently naming something that is
/// refused. Here it is derived, so it cannot drift.
///
/// What is deliberately *not* here: how much of each cap is left. Every node of a run is prepared
/// before the first one starts, so this text is written once and would be stale by the second
/// attempt. A node that asks when the cap is spent is told no, the stream says `spent`, and its
/// own answer stands — which costs a turn and breaks nothing.
#[must_use]
pub fn loop_advice(loops: &[LoopBack], node: &NodeName) -> Option<String> {
    let mut declared: Vec<&LoopBack> = loops.iter().filter(|back| &back.from == node).collect();
    if declared.is_empty() {
        return None;
    }
    declared.sort_by(|one, two| one.to.cmp(&two.to));
    let steps = declared
        .iter()
        .map(|back| format!("{} (at most {} times)", back.to, back.cap.get()))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "You may send this work back to an earlier step instead of answering, where what you were \
         given is not good enough to answer well. To do that, answer with nothing but this json: \
         {{\"{LOOP_BACK_KEY}\": \"<step>\", \"notes\": \"<what must change>\"}}. The notes are the \
         point of sending it back: a step given none will do the same thing again. Anything else \
         you put in that object reaches it too. The steps you may send it back to are: {steps}. \
         Answer normally wherever you can — a step you may not name, or one already sent work back \
         as often as it may be, is refused, and the answer you gave stands as it is."
    ))
}

/// What a node that may choose where the work goes next is told, beyond what its own template says.
///
/// Takes every edge and filters, rather than taking the branches alone, so the one function serves
/// both callers: the machine holding the graph passes [`RunGraph::edges`] and a member running one
/// node of somebody else's graph passes the branch read of the plane they share. An edge without a
/// condition is not a branch and is not mentioned — it delivers whatever this node answers.
///
/// Nothing, where no branch leaves this node, for [`loop_advice`]'s reason: a node that chooses
/// nothing is told nothing about choosing, so one template reads the same in any graph.
///
/// **The case spelled out at length is the one where nothing holds**, and that is not politeness.
/// A node told it may choose will choose, and M28's example ended with a model sending work back
/// while stating in the same breath that the work had passed. So taking no branch is given a shape
/// of its own — `{"branch": []}`, an answer and not an omission — and said in as many words to be
/// a complete answer rather than a failure, which is what this milestone ruled it is.
#[must_use]
pub fn branch_advice(edges: &[GraphEdge], node: &NodeName) -> Option<String> {
    let mut declared: Vec<(&GraphEdge, &Condition)> = edges
        .iter()
        .filter(|edge| &edge.from == node)
        .filter_map(|edge| edge.condition.as_ref().map(|when| (edge, when)))
        .collect();
    if declared.is_empty() {
        return None;
    }
    declared.sort_by(|one, two| one.0.to.cmp(&two.0.to));
    let steps = declared
        .iter()
        .map(|(edge, when)| format!("{} (when {})", edge.to, when))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "Which steps run after you is yours to decide. To send the work on, answer with nothing \
         but this json: {{\"{BRANCH_KEY}\": [\"<step>\"], \"notes\": \"<anything those steps \
         should know>\"}}. Judge each step below on its own and name every one whose condition \
         holds — one of them, several, or all. Anything else you put in that object reaches them \
         too. The steps you may name are: {steps}. If no condition holds, name none of them: \
         answer {{\"{BRANCH_KEY}\": []}} and nothing further runs, which is a complete answer and \
         not a failure. A step that is not listed is refused, and the answer you gave stands as it \
         is."
    ))
}

/// Everything a node is told about the graph it is running in, in the order it is told.
///
/// One function rather than each caller joining the pieces, because the callers are two machines:
/// a node dispatched elsewhere must read exactly what it would have read had the coordinator run
/// it, and the order the pieces arrive in is part of what it reads.
///
/// Loop back first, where a node carries both. What to do with what arrived comes before where to
/// send what you make of it.
#[must_use]
pub fn graph_advice(loops: &[LoopBack], edges: &[GraphEdge], node: &NodeName) -> Option<String> {
    let said: Vec<String> = [loop_advice(loops, node), branch_advice(edges, node)]
        .into_iter()
        .flatten()
        .collect();
    (!said.is_empty()).then(|| said.join("\n\n"))
}

/// Whatever can say which nodes one node's answer reaches.
///
/// A seam for the reason [`RunsNode`] and [`Stopping`] are seams, although what implements it here
/// is in this crate: the rules below — which nodes a taken loop runs again, what the cap stops,
/// what a refused ask says — are then tested without a store or a socket.
///
/// A graph run with none of this refuses every loop a node asks for and says so, rather than
/// failing: a deployment that has not wired one has a graph it can still run.
#[async_trait]
pub trait Downstream: Send + Sync {
    /// Every node the named one's answer reaches by feeding, itself included.
    async fn downstream(
        &self,
        owner: &GraphOwner,
        graph: &GraphId,
        node: &NodeName,
    ) -> Result<BTreeSet<NodeName>, GraphError>;
}

#[async_trait]
impl Downstream for GraphPlane {
    async fn downstream(
        &self,
        owner: &GraphOwner,
        graph: &GraphId,
        node: &NodeName,
    ) -> Result<BTreeSet<NodeName>, GraphError> {
        GraphPlane::downstream(self, owner, graph, node).await
    }
}

/// A graph, the order it runs in, and whatever runs one node of it.
pub struct Graphing {
    /// What this run is followed under.
    pub id: GraphRunId,
    /// The graph to run.
    pub graph: RunGraph,
    /// The order its nodes run in, as the plane answered it.
    pub order: RunOrder,
    /// What runs one node.
    pub runner: Arc<dyn RunsNode>,
    /// What the graph as a whole was asked.
    pub input: String,
    /// Where it says what it is doing as it does it.
    pub reporting: Arc<dyn GraphReporting>,
    /// What carries the stop to the nodes of this run that are running elsewhere, where any are.
    pub stopping: Option<Arc<dyn Stopping>>,
    /// What says which nodes a taken loop back runs again, where a run has it.
    pub plane: Option<Arc<dyn Downstream>>,
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
///
/// A node something has doubled back to is given [`AGAIN_KEY`] as well, holding what each node
/// that sent the work back said about it. That is the whole of what a loop is worth — a node runs
/// again *knowing what the work downstream of it made of its last answer* — and it is why the
/// asker's words are kept rather than dropped with its place in the run. Where that node is also
/// one nothing feeds, the graph's own input comes with it under [`GRAPH_INPUT_KEY`], because the
/// question the run was asked is not the asker's to replace.
fn input_for(
    graphing: &Graphing,
    name: &NodeName,
    answered: &BTreeMap<NodeName, NodeRan>,
    taken: &BTreeMap<(NodeName, NodeName), u32>,
) -> String {
    let mut from: Map<String, Value> = Map::new();
    for parent in graphing.graph.parents_of(name) {
        let answer = answered
            .get(parent)
            .map_or_else(String::new, |ran| ran.answer.clone());
        from.insert(parent.to_string(), Value::String(answer));
    }
    let fed = !from.is_empty();
    let mut again: Map<String, Value> = Map::new();
    for back in graphing.graph.loops() {
        if &back.to != name {
            continue;
        }
        let count = taken
            .get(&(back.from.clone(), back.to.clone()))
            .copied()
            .unwrap_or_default();
        if count == 0 {
            continue;
        }
        if let Some(ran) = answered.get(&back.from) {
            again.insert(back.from.to_string(), amendment(&ran.answer, count));
        }
    }
    if !again.is_empty() {
        from.insert(AGAIN_KEY.to_owned(), Value::Object(again));
    }
    if from.is_empty() {
        return graphing.input.clone();
    }
    if !fed {
        from.insert(
            GRAPH_INPUT_KEY.to_owned(),
            Value::String(graphing.input.clone()),
        );
    }
    Value::Object(from).to_string()
}

/// What one node that sent the work back said about it, as the node running again is given it.
///
/// Everything the asker answered except the ask itself, which keeps whatever shape that node chose
/// — a sentence under `notes`, a list of what is missing, a corrected figure — rather than one
/// field this crate picked. Nothing is dropped, so a template and its prompt can agree on a shape
/// this code does not have to know.
///
/// An answer that is not a json object carries no amendment and still counts as an attempt: a
/// model that asked for a loop back wrote an object, so the only way here is a node whose answer
/// changed shape between the ask and this read.
fn amendment(answer: &str, taken: u32) -> Value {
    let mut said = match serde_json::from_str::<Value>(answer) {
        Ok(Value::Object(said)) => said,
        _ => Map::new(),
    };
    said.remove(LOOP_BACK_KEY);
    // Put in last, so a node that answered with an `attempt` of its own cannot overwrite the count.
    said.insert("attempt".to_owned(), json!(taken + 1));
    Value::Object(said)
}

/// The node an answer asks to send the work back to, where it asks for one.
///
/// An answer that is not json, or json carrying no [`LOOP_BACK_KEY`], asks for nothing and is not
/// refused: most answers are prose and are meant to be, so this is read as a question that was
/// never put rather than one answered badly.
fn asked_for(answer: &str) -> Option<String> {
    let said: Value = serde_json::from_str(answer).ok()?;
    said.get(LOOP_BACK_KEY)?.as_str().map(str::to_owned)
}

/// The loop back an ask resolves to, or why it does not resolve to one.
fn wanted<'a>(
    graph: &'a RunGraph,
    asker: &NodeName,
    asked: &str,
    taken: &BTreeMap<(NodeName, NodeName), u32>,
) -> Result<&'a LoopBack, LoopRefusal> {
    let target = NodeName::new(asked).map_err(|_| LoopRefusal::Unknown)?;
    if !graph.nodes().iter().any(|node| node.name == target) {
        return Err(LoopRefusal::Unknown);
    }
    let back = graph
        .loop_back(asker, &target)
        .ok_or(LoopRefusal::Undeclared)?;
    let already = taken
        .get(&(asker.clone(), target))
        .copied()
        .unwrap_or_default();
    if already >= back.cap.get() {
        return Err(LoopRefusal::Spent);
    }
    Ok(back)
}

/// Takes whatever loop backs the generation that just ran asked for, answering what runs again.
///
/// Asked once the generation is whole rather than as each node finishes, because a generation is
/// what may run at once: taking a node out of the done set while its siblings are still in flight
/// would have them finish into a set that no longer says they ran.
///
/// Every ask is answered in the stream, the refusals included. A run carries on either way — a
/// loop that is refused leaves the answer the node gave standing, which is what a graph without
/// loop backs would have done with it.
async fn looped(
    graphing: &Graphing,
    just_ran: &[NodeName],
    answered: &BTreeMap<NodeName, NodeRan>,
    taken: &mut BTreeMap<(NodeName, NodeName), u32>,
    say: &impl Fn(GraphEvent),
) -> BTreeSet<NodeName> {
    let mut again = BTreeSet::new();
    for asker in just_ran {
        let Some(asked) = answered.get(asker).and_then(|ran| asked_for(&ran.answer)) else {
            continue;
        };
        let refused = |why| GraphEvent::LoopRefused {
            from: asker.clone(),
            asked: asked.clone(),
            why,
        };
        let (to, cap) = match wanted(&graphing.graph, asker, &asked, taken) {
            Ok(back) => (back.to.clone(), back.cap.get()),
            Err(why) => {
                say(refused(why));
                continue;
            }
        };
        let Some(plane) = graphing.plane.as_ref() else {
            say(refused(LoopRefusal::Unreachable));
            continue;
        };
        let reached = plane
            .downstream(graphing.graph.owner(), graphing.graph.id(), &to)
            .await;
        match reached {
            Ok(reached) => {
                let count = taken.entry((asker.clone(), to.clone())).or_default();
                *count += 1;
                say(GraphEvent::LoopBack {
                    from: asker.clone(),
                    to,
                    taken: *count,
                    cap,
                });
                again.extend(reached);
            }
            // The store's own message is not carried into the stream: the event vocabulary has no
            // room for one, and this crate has no logger to put it in.
            Err(_) => say(refused(LoopRefusal::Unreachable)),
        }
    }
    again
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
/// A node that asked to double back where the graph declares it, and where the cap has something
/// left, has the target and everything downstream of it run again — the asker included, since the
/// asker is downstream of what it sent the work back to. **A node that stopped the graph does not
/// also get to ask**: the stop is read first, and the asks of that generation are not looked at.
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
    let mut taken: BTreeMap<(NodeName, NodeName), u32> = BTreeMap::new();
    let mut stopped_at = None;
    let (stop, listen) = watch::channel(RunSignal::Carry);
    let say = |event| {
        graphing.reporting.say(GraphSaid {
            run: graphing.id,
            event,
        });
    };

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
            let input = input_for(graphing, &name, &answered, &taken);
            say(GraphEvent::Node {
                name: name.clone(),
                input: input.clone(),
            });
            let runner = Arc::clone(&graphing.runner);
            let running_node = RunningNode {
                run: graphing.id,
                node,
                input,
                signals: listen.clone(),
            };
            running.spawn(async move { (name, runner.run(running_node).await) });
        }

        let mut just_ran: Vec<NodeName> = Vec::new();
        // Every one of them is awaited, the stop included: a node already running is paid for.
        while let Some(finished) = running.join_next().await {
            let Ok((name, ran)) = finished else {
                continue;
            };
            spend = spend + ran.spend;
            traces.extend(ran.traces.iter().cloned());
            say(GraphEvent::NodeDone {
                name: name.clone(),
                answer: ran.answer.clone(),
                stopped: ran.stopped.clone(),
                spend: ran.spend,
            });
            if !matches!(ran.stopped, Stopped::Answered | Stopped::CutShort) && stopped_at.is_none()
            {
                stopped_at = Some(name.clone());
                // Said while the rest of the generation is still running, which is the only
                // moment it can save a turn — and awaited for that reason, before the siblings
                // still in flight are reaped.
                let _ = stop.send(RunSignal::Stop);
                if let Some(stopping) = &graphing.stopping {
                    stopping.stopped(graphing.id).await;
                }
            }
            done.insert(name.clone());
            just_ran.push(name.clone());
            answered.insert(name, ran);
        }

        if stopped_at.is_none() {
            // Name order, so two nodes of one generation asking are resolved in an order a test
            // can state rather than the order the scheduler reaped them in.
            just_ran.sort();
            for again in looped(graphing, &just_ran, &answered, &mut taken, &say).await {
                done.remove(&again);
            }
        }
    }

    let ran = GraphRan {
        answer: match stopped_at {
            Some(_) => json!({}),
            None => answer_of(&graphing.graph, &answered),
        },
        nodes: answered,
        stopped_at,
        spend,
        traces,
    };
    say(GraphEvent::Done {
        answer: ran.answer.clone(),
        stopped_at: ran.stopped_at.clone(),
        spend: ran.spend,
    });
    ran
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use ip_agents::AgentId;
    use ip_core::{Money, TenantId, TokenCount, Tokens};

    use super::*;
    use crate::graph::{GraphEdge, GraphOwner, LoopCap, MaxNodes, RunGraphParts};
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
        async fn run(&self, running: RunningNode) -> NodeRan {
            let RunningNode {
                node,
                input,
                signals,
                ..
            } = running;
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
            condition: None,
        }
    }

    /// The same edge, carrying the condition that makes it a branch.
    fn branch(from: &str, to: &str, when: &str) -> GraphEdge {
        GraphEdge {
            from: name(from),
            to: name(to),
            condition: Some(Condition::new(when).expect("a condition")),
        }
    }

    fn graph(nodes: &[&str], edges: &[(&str, &str)]) -> RunGraph {
        RunGraph::checked(RunGraphParts {
            id: GraphId::new("triage").expect("a graph id"),
            name: GraphName::new("Support triage").expect("a graph name"),
            owner: GraphOwner::Tenant(TenantId::new("acme").expect("a tenant id")),
            nodes: nodes.iter().map(|raw| node(raw)).collect(),
            edges: edges.iter().map(|(from, to)| edge(from, to)).collect(),
            loops: Vec::new(),
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
            id: GraphRunId::generate().expect("a graph run id"),
            graph,
            order,
            runner,
            input: "a ticket".to_owned(),
            reporting: Arc::new(()),
            stopping: None,
            plane: None,
        }
    }

    /// The same, over a sink a test can read back in the order it was said.
    fn watched(
        graph: RunGraph,
        order: RunOrder,
        runner: Arc<Echo>,
    ) -> (Graphing, tokio::sync::mpsc::UnboundedReceiver<GraphSaid>) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        (
            Graphing {
                id: GraphRunId::generate().expect("a graph run id"),
                graph,
                order,
                runner,
                input: "a ticket".to_owned(),
                reporting: Arc::new(sender),
                stopping: None,
                plane: None,
            },
            receiver,
        )
    }

    /// A run of a graph that declares loop backs, over whatever answers what they run again.
    fn looping(
        graph: RunGraph,
        order: RunOrder,
        runner: Arc<Doubling>,
        plane: Option<Arc<dyn Downstream>>,
    ) -> (Graphing, tokio::sync::mpsc::UnboundedReceiver<GraphSaid>) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        (
            Graphing {
                id: GraphRunId::generate().expect("a graph run id"),
                graph,
                order,
                runner,
                input: "a ticket".to_owned(),
                reporting: Arc::new(sender),
                stopping: None,
                plane,
            },
            receiver,
        )
    }

    /// A node that asks to double back a given number of times, then answers plainly.
    ///
    /// Every input it was handed is kept in the order it was handed them, which is what a test of
    /// what a node runs *again* on reads.
    struct Doubling {
        /// How many more times each node asks, by its name.
        asks: Mutex<BTreeMap<String, u32>>,
        /// What they ask to go back to, as they spell it.
        back_to: String,
        /// Which of them stop the graph, asking as they do.
        refuses: BTreeSet<String>,
        /// Every node that ran and what it was given, in order.
        seen: Mutex<Vec<(String, String)>>,
    }

    impl Doubling {
        fn new(asks: &[(&str, u32)], back_to: &str, refuses: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                asks: Mutex::new(
                    asks.iter()
                        .map(|&(node, times)| (node.to_owned(), times))
                        .collect(),
                ),
                back_to: back_to.to_owned(),
                refuses: refuses.iter().map(|&node| node.to_owned()).collect(),
                seen: Mutex::new(Vec::new()),
            })
        }

        /// Every node that ran and what it was given, in order.
        fn runs(&self) -> Vec<(String, String)> {
            self.seen
                .lock()
                .expect("the record is not poisoned")
                .clone()
        }

        /// The names alone, which is the run order.
        fn order(&self) -> Vec<String> {
            self.runs().into_iter().map(|(node, _)| node).collect()
        }
    }

    #[async_trait]
    impl RunsNode for Doubling {
        async fn run(&self, running: RunningNode) -> NodeRan {
            let node = running.node.name.to_string();
            self.seen
                .lock()
                .expect("the record is not poisoned")
                .push((node.clone(), running.input));
            let asking = {
                let mut asks = self.asks.lock().expect("the record is not poisoned");
                let left = asks.entry(node.clone()).or_default();
                let asking = *left > 0;
                *left = left.saturating_sub(1);
                asking
            };
            NodeRan {
                answer: if asking {
                    json!({
                        LOOP_BACK_KEY: self.back_to,
                        "notes": format!("{node} wants another pass"),
                    })
                    .to_string()
                } else {
                    format!("{node} is content")
                },
                stopped: if self.refuses.contains(&node) {
                    Stopped::Refused {
                        why: "the fixture refuses this node".to_owned(),
                    }
                } else {
                    Stopped::Answered
                },
                spend: Spend::NOTHING,
                traces: Vec::new(),
            }
        }
    }

    /// What a plane answers for a loop back's target, scripted per node.
    struct Reaching {
        /// What each node's answer reaches, itself included, by name.
        reaches: BTreeMap<String, Vec<String>>,
        /// Whether it answers at all.
        answers: bool,
        /// Which nodes it was asked about, in order.
        asked: Mutex<Vec<String>>,
    }

    impl Reaching {
        fn over(reaches: &[(&str, &[&str])]) -> Arc<Self> {
            Arc::new(Self {
                reaches: reaches
                    .iter()
                    .map(|&(node, reached)| {
                        (
                            node.to_owned(),
                            reached.iter().map(|&name| name.to_owned()).collect(),
                        )
                    })
                    .collect(),
                answers: true,
                asked: Mutex::new(Vec::new()),
            })
        }

        /// One that will not answer, which is a store that cannot be reached.
        fn silent() -> Arc<Self> {
            Arc::new(Self {
                reaches: BTreeMap::new(),
                answers: false,
                asked: Mutex::new(Vec::new()),
            })
        }

        fn asked(&self) -> Vec<String> {
            self.asked
                .lock()
                .expect("the record is not poisoned")
                .clone()
        }
    }

    #[async_trait]
    impl Downstream for Reaching {
        async fn downstream(
            &self,
            _: &GraphOwner,
            _: &GraphId,
            node: &NodeName,
        ) -> Result<BTreeSet<NodeName>, GraphError> {
            self.asked
                .lock()
                .expect("the record is not poisoned")
                .push(node.to_string());
            if !self.answers {
                return Err(GraphError::Unreadable {
                    detail: "a fixture that will not answer".to_owned(),
                });
            }
            Ok(self
                .reaches
                .get(node.as_str())
                .map(|reached| reached.iter().map(|raw| name(raw)).collect())
                .unwrap_or_default())
        }
    }

    /// A graph of two where the second may send the work back to the first.
    fn doubling_back(cap: u32) -> RunGraph {
        looping_graph(
            &["intake", "reply"],
            &[("intake", "reply")],
            &[("reply", "intake", cap)],
        )
    }

    /// A graph with loop backs declared on it.
    fn looping_graph(
        nodes: &[&str],
        edges: &[(&str, &str)],
        loops: &[(&str, &str, u32)],
    ) -> RunGraph {
        RunGraph::checked(RunGraphParts {
            id: GraphId::new("triage").expect("a graph id"),
            name: GraphName::new("Support triage").expect("a graph name"),
            owner: GraphOwner::Tenant(TenantId::new("acme").expect("a tenant id")),
            nodes: nodes.iter().map(|raw| node(raw)).collect(),
            edges: edges.iter().map(|(from, to)| edge(from, to)).collect(),
            loops: loops
                .iter()
                .map(|&(from, to, cap)| LoopBack {
                    from: name(from),
                    to: name(to),
                    cap: LoopCap::new(cap).expect("a cap a loop back may carry"),
                })
                .collect(),
            max_nodes: MaxNodes::new(8).expect("a ceiling"),
        })
        .expect("the fixture's own graph holds together")
    }

    /// Every loop event a run said, as the one word it is named by and what it carries.
    ///
    /// The event's own body rather than the stream's, which carries the run id a test cannot know.
    fn loops_said(said: &[GraphSaid]) -> Vec<(String, Value)> {
        said.iter()
            .filter(|one| matches!(one.name(), "loop_back" | "loop_refused"))
            .map(|one| (one.name().to_owned(), one.event.body()))
            .collect()
    }

    /// Every event a run said, by name, in the order it said them.
    fn said(mut receiver: tokio::sync::mpsc::UnboundedReceiver<GraphSaid>) -> Vec<GraphSaid> {
        let mut said = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            said.push(event);
        }
        said
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

    /// Everything the stop was carried to, in the order it was carried.
    #[derive(Default)]
    struct Carried {
        told: Mutex<Vec<GraphRunId>>,
    }

    #[async_trait]
    impl Stopping for Carried {
        async fn stopped(&self, run: GraphRunId) {
            self.told
                .lock()
                .expect("the record is not poisoned")
                .push(run);
        }
    }

    /// The nodes of this run that are not in this process are told, once, and told which run.
    #[tokio::test]
    async fn a_graph_that_stops_says_so_to_whatever_carries_it_elsewhere() {
        let carried = Arc::new(Carried::default());
        let mut graphing = graphing(
            graph(
                &["left", "right", "merge"],
                &[("left", "merge"), ("right", "merge")],
            ),
            order(&[("left", 0), ("right", 0), ("merge", 1)]),
            Echo::new(SLOW, 2, &["left"]),
        );
        graphing.stopping = Some(Arc::clone(&carried) as Arc<dyn Stopping>);
        let id = graphing.id;

        run(&graphing).await;

        assert_eq!(
            *carried.told.lock().expect("the record is not poisoned"),
            vec![id],
            "a stop was carried more than once, or under the wrong run"
        );
    }

    /// A graph that finishes carries nothing: there is no stop to pass on.
    #[tokio::test]
    async fn a_graph_that_answers_carries_no_stop() {
        let carried = Arc::new(Carried::default());
        let mut graphing = graphing(
            graph(&["a", "b"], &[("a", "b")]),
            order(&[("a", 0), ("b", 1)]),
            Echo::new(Duration::ZERO, 1, &[]),
        );
        graphing.stopping = Some(Arc::clone(&carried) as Arc<dyn Stopping>);

        let ran = run(&graphing).await;

        assert_eq!(ran.stopped_at, None);
        assert!(
            carried
                .told
                .lock()
                .expect("the record is not poisoned")
                .is_empty()
        );
    }

    /// A deployment with nobody else to tell runs exactly as it did before the stop existed.
    #[tokio::test]
    async fn a_graph_with_nothing_to_carry_the_stop_still_stops_itself() {
        let ran = run(&graphing(
            graph(
                &["left", "right", "merge"],
                &[("left", "merge"), ("right", "merge")],
            ),
            order(&[("left", 0), ("right", 0), ("merge", 1)]),
            Echo::new(SLOW, 2, &["left"]),
        ))
        .await;

        assert_eq!(ran.stopped_at, Some(name("left")));
        assert_eq!(ran.nodes[&name("right")].stopped, Stopped::CutShort);
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
    async fn a_run_says_each_node_beginning_and_ending_and_then_the_graph() {
        let (graphing, watcher) = watched(
            graph(&["intake", "reply"], &[("intake", "reply")]),
            order(&[("intake", 0), ("reply", 1)]),
            Echo::new(Duration::ZERO, 1, &[]),
        );
        let ran = run(&graphing).await;
        let events = said(watcher);

        let names: Vec<&str> = events.iter().map(GraphSaid::name).collect();
        assert_eq!(
            names,
            ["node", "node_done", "node", "node_done", "done"],
            "a node ended after the next had begun, or the graph spoke out of turn"
        );
        // The input a node was given is said as it is given, which is what makes a stream
        // readable: ruling 2's keying is visible rather than inferred from the answer.
        match &events[2].event {
            GraphEvent::Node { name, input } => {
                assert_eq!(name.as_str(), "reply");
                assert_eq!(input, r#"{"intake":"intake saw a ticket"}"#);
            }
            other => panic!("{other:?}"),
        }
        match &events.last().expect("a graph says it is over").event {
            GraphEvent::Done {
                answer,
                stopped_at,
                spend,
            } => {
                assert_eq!(answer, &ran.answer, "the stream and the result disagree");
                assert_eq!(stopped_at, &None);
                assert_eq!(spend, &ran.spend);
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_graph_that_was_stopped_says_so_in_the_event_that_ends_it() {
        let (graphing, watcher) = watched(
            graph(&["intake", "reply"], &[("intake", "reply")]),
            order(&[("intake", 0), ("reply", 1)]),
            Echo::new(Duration::ZERO, 1, &["intake"]),
        );
        run(&graphing).await;
        let events = said(watcher);

        assert_eq!(
            events.iter().map(GraphSaid::name).collect::<Vec<&str>>(),
            ["node", "node_done", "done"],
            "the graph went on past the node that stopped it"
        );
        match &events.last().expect("a graph says it is over").event {
            GraphEvent::Done { stopped_at, .. } => {
                assert_eq!(stopped_at.as_ref().map(NodeName::as_str), Some("intake"));
            }
            other => panic!("{other:?}"),
        }
    }

    /// A generation says every node of it before any of them is done.
    #[tokio::test]
    async fn a_generation_announces_all_of_itself_before_any_of_it_finishes() {
        let (graphing, watcher) = watched(
            graph(
                &["left", "right", "merge"],
                &[("left", "merge"), ("right", "merge")],
            ),
            order(&[("left", 0), ("right", 0), ("merge", 1)]),
            Echo::new(Duration::ZERO, 1, &[]),
        );
        run(&graphing).await;

        let names: Vec<&str> = said(watcher).iter().map(GraphSaid::name).collect();
        assert_eq!(
            names,
            [
                "node",
                "node",
                "node_done",
                "node_done",
                "node",
                "node_done",
                "done"
            ],
            "the pair of a generation was not both begun before either ended"
        );
    }

    #[tokio::test]
    async fn what_a_graph_spent_is_every_node_of_it_summed() {
        /// A runner that spends one token of each kind per node.
        struct Costs;

        #[async_trait]
        impl RunsNode for Costs {
            async fn run(&self, _running: RunningNode) -> NodeRan {
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
            id: GraphRunId::generate().expect("a graph run id"),
            graph: graph(&["a", "b", "c"], &[("a", "b"), ("b", "c")]),
            order: order(&[("a", 0), ("b", 1), ("c", 2)]),
            runner: Arc::new(Costs),
            input: "a ticket".to_owned(),
            reporting: Arc::new(()),
            stopping: None,
            plane: None,
        })
        .await;
        assert_eq!(ran.spend.tokens.input.get(), 6);
        assert_eq!(ran.spend.tokens.output.get(), 9);
    }

    /// What a node came to is the same struct wherever it ran, so it has to survive the trip.
    #[test]
    fn what_a_node_came_to_survives_being_written_down_and_read_back() {
        let ran = NodeRan {
            answer: "41 crates in bay 3.".to_owned(),
            stopped: Stopped::Refused {
                why: "the gateway said no".to_owned(),
            },
            spend: Spend::new(
                Tokens::new(TokenCount::new(120), TokenCount::new(34)),
                Money::from_micros(5_600),
            ),
            traces: vec![
                TraceId::generate().expect("a trace id"),
                TraceId::generate().expect("another trace id"),
            ],
        };

        let written = serde_json::to_string(&ran).expect("an answer writes down");
        assert_eq!(
            serde_json::from_str::<NodeRan>(&written).expect("an answer reads back"),
            ran
        );
    }

    /// A node that ran and spent nothing is not a node that is missing its accounting.
    #[test]
    fn a_node_that_came_to_nothing_still_says_so() {
        let ran = NodeRan {
            answer: String::new(),
            stopped: Stopped::CutShort,
            spend: Spend::NOTHING,
            traces: Vec::new(),
        };

        let written = serde_json::to_string(&ran).expect("an answer writes down");
        assert_eq!(
            serde_json::from_str::<NodeRan>(&written).expect("an answer reads back"),
            ran
        );
    }

    /// The target and everything after it, the asker included, and the asker is after it.
    #[tokio::test]
    async fn a_node_that_asks_to_double_back_has_the_target_and_what_follows_run_again() {
        let runner = Doubling::new(&[("reply", 1)], "intake", &[]);
        let plane = Reaching::over(&[("intake", &["intake", "reply"])]);
        let (graphing, events) = looping(
            doubling_back(2),
            order(&[("intake", 0), ("reply", 1)]),
            Arc::clone(&runner),
            Some(Arc::clone(&plane) as Arc<dyn Downstream>),
        );

        let ran = run(&graphing).await;

        assert_eq!(
            runner.order(),
            ["intake", "reply", "intake", "reply"],
            "the loop back did not run the target and what follows it again"
        );
        assert_eq!(
            plane.asked(),
            ["intake"],
            "the nodes to run again were not asked for once, about the target"
        );
        assert_eq!(
            loops_said(&said(events)),
            [(
                "loop_back".to_owned(),
                json!({"node": "reply", "back_to": "intake", "taken": 1, "cap": 2}),
            )]
        );
        assert_eq!(ran.stopped_at, None);
        assert_eq!(
            ran.nodes[&name("reply")].answer,
            "reply is content",
            "ruling 4: what the run came to is the last attempt, not the first"
        );
    }

    /// Ruling: a node that runs again is told what the work downstream made of its last answer.
    #[tokio::test]
    async fn a_node_run_again_is_given_the_askers_answer_and_the_question_as_well() {
        let runner = Doubling::new(&[("reply", 1)], "intake", &[]);
        let (graphing, _events) = looping(
            doubling_back(2),
            order(&[("intake", 0), ("reply", 1)]),
            Arc::clone(&runner),
            Some(Reaching::over(&[("intake", &["intake", "reply"])]) as Arc<dyn Downstream>),
        );

        run(&graphing).await;

        let runs = runner.runs();
        assert_eq!(runs[0], ("intake".to_owned(), "a ticket".to_owned()));
        let again: Value =
            serde_json::from_str(&runs[2].1).expect("a node run again is given json");
        assert_eq!(
            again,
            json!({
                AGAIN_KEY: {
                    "reply": {"notes": "reply wants another pass", "attempt": 2},
                },
                GRAPH_INPUT_KEY: "a ticket",
            }),
            "a node nothing feeds lost the question the run was asked"
        );
        let after: Value = serde_json::from_str(&runs[3].1).expect("a fed node is given json");
        assert_eq!(
            after,
            json!({"intake": "intake is content"}),
            "the node after the target was not given the target's new answer"
        );
    }

    /// The cap is what makes a loop terminate, so it is asserted by count and by what is said.
    #[tokio::test]
    async fn a_cap_stops_a_node_that_keeps_asking() {
        let runner = Doubling::new(&[("reply", 9)], "intake", &[]);
        let (graphing, events) = looping(
            doubling_back(2),
            order(&[("intake", 0), ("reply", 1)]),
            Arc::clone(&runner),
            Some(Reaching::over(&[("intake", &["intake", "reply"])]) as Arc<dyn Downstream>),
        );

        run(&graphing).await;

        assert_eq!(
            runner.order(),
            ["intake", "reply", "intake", "reply", "intake", "reply"],
            "a cap of two did not stop the loop after two"
        );
        let loops = loops_said(&said(events));
        assert_eq!(loops.len(), 3, "{loops:?}");
        assert_eq!(loops[0].1["taken"], 1);
        assert_eq!(loops[1].1["taken"], 2);
        assert_eq!(loops[2].0, "loop_refused");
        assert_eq!(loops[2].1["refusal"], "spent");
    }

    /// A graph that declares no loop back refuses one, rather than taking a node's word for it.
    #[tokio::test]
    async fn an_undeclared_loop_back_is_refused_and_changes_nothing() {
        let runner = Doubling::new(&[("reply", 1)], "intake", &[]);
        let plane = Reaching::over(&[("intake", &["intake", "reply"])]);
        let (graphing, events) = looping(
            looping_graph(&["intake", "reply"], &[("intake", "reply")], &[]),
            order(&[("intake", 0), ("reply", 1)]),
            Arc::clone(&runner),
            Some(Arc::clone(&plane) as Arc<dyn Downstream>),
        );

        run(&graphing).await;

        assert_eq!(runner.order(), ["intake", "reply"]);
        assert!(
            plane.asked().is_empty(),
            "an undeclared ask reached the store"
        );
        let loops = loops_said(&said(events));
        assert_eq!(loops[0].1["refusal"], "undeclared");
        assert_eq!(loops[0].1["back_to"], "intake");
    }

    /// Both ways a name can name no node: one the graph has not got, and one that is no name.
    #[tokio::test]
    async fn a_loop_back_to_something_that_is_not_a_node_of_the_graph_is_refused() {
        for asked in ["survey", "two words"] {
            let runner = Doubling::new(&[("reply", 1)], asked, &[]);
            let (graphing, events) = looping(
                doubling_back(2),
                order(&[("intake", 0), ("reply", 1)]),
                Arc::clone(&runner),
                Some(Reaching::over(&[]) as Arc<dyn Downstream>),
            );

            run(&graphing).await;

            assert_eq!(
                runner.order(),
                ["intake", "reply"],
                "{asked} ran something again"
            );
            let loops = loops_said(&said(events));
            assert_eq!(loops[0].1["refusal"], "unknown", "{asked}");
            assert_eq!(loops[0].1["back_to"], asked);
        }
    }

    /// A run with nothing to ask, and a store that will not answer, read the same from the stream.
    #[tokio::test]
    async fn a_loop_nothing_can_work_out_is_refused_rather_than_failing_the_run() {
        for plane in [None, Some(Reaching::silent() as Arc<dyn Downstream>)] {
            let runner = Doubling::new(&[("reply", 1)], "intake", &[]);
            let (graphing, events) = looping(
                doubling_back(2),
                order(&[("intake", 0), ("reply", 1)]),
                Arc::clone(&runner),
                plane,
            );

            let ran = run(&graphing).await;

            assert_eq!(runner.order(), ["intake", "reply"]);
            assert_eq!(
                ran.stopped_at, None,
                "a loop that could not be worked out failed the run"
            );
            let loops = loops_said(&said(events));
            assert_eq!(loops[0].1["refusal"], "unreachable");
        }
    }

    /// Stop beats loop: the node that broke the graph does not get to ask for another pass.
    #[tokio::test]
    async fn a_node_that_stops_the_graph_does_not_also_double_back() {
        let runner = Doubling::new(&[("reply", 1)], "intake", &["reply"]);
        let plane = Reaching::over(&[("intake", &["intake", "reply"])]);
        let (graphing, events) = looping(
            doubling_back(2),
            order(&[("intake", 0), ("reply", 1)]),
            Arc::clone(&runner),
            Some(Arc::clone(&plane) as Arc<dyn Downstream>),
        );

        let ran = run(&graphing).await;

        assert_eq!(ran.stopped_at, Some(name("reply")));
        assert_eq!(
            runner.order(),
            ["intake", "reply"],
            "a stopped graph ran a node again"
        );
        assert!(
            plane.asked().is_empty(),
            "a stopped graph asked about a loop back"
        );
        assert!(
            loops_said(&said(events)).is_empty(),
            "a stopped graph said it was looping"
        );
    }

    /// Only what the target reaches: a branch beside it keeps the answer it had.
    #[tokio::test]
    async fn a_node_the_target_does_not_reach_is_not_run_again() {
        let runner = Doubling::new(&[("review", 1)], "draft", &[]);
        let (graphing, _events) = looping(
            looping_graph(
                &["survey", "draft", "review", "aside"],
                &[
                    ("survey", "draft"),
                    ("draft", "review"),
                    ("survey", "aside"),
                ],
                &[("review", "draft", 2)],
            ),
            order(&[("survey", 0), ("draft", 1), ("aside", 1), ("review", 2)]),
            Arc::clone(&runner),
            Some(Reaching::over(&[("draft", &["draft", "review"])]) as Arc<dyn Downstream>),
        );

        run(&graphing).await;

        let order = runner.order();
        assert_eq!(
            order.iter().filter(|node| *node == "aside").count(),
            1,
            "a node the loop does not reach ran again: {order:?}"
        );
        assert_eq!(order.iter().filter(|node| *node == "draft").count(), 2);
        assert_eq!(order.iter().filter(|node| *node == "review").count(), 2);
    }

    /// The amendment is the point of a loop, so what crosses is pinned shape by shape.
    ///
    /// Everything the asker said travels except the ask itself: this crate does not name the
    /// field a template writes its revision in, so a prompt and a template can agree on one
    /// without a change here.
    #[test]
    fn an_amendment_carries_everything_the_asker_said_but_the_ask() {
        assert_eq!(
            amendment(
                r#"{"loop_back": "survey", "notes": "the year is wrong"}"#,
                1
            ),
            json!({"notes": "the year is wrong", "attempt": 2}),
            "the ask reached the node it was not addressed to, or the notes did not"
        );
        assert_eq!(
            amendment(
                r#"{"loop_back": "survey", "missing": ["eu"], "confidence": 0.4}"#,
                2
            ),
            json!({"missing": ["eu"], "confidence": 0.4, "attempt": 3}),
            "a template's own shape did not survive"
        );
        assert_eq!(
            amendment(r#"{"loop_back": "survey"}"#, 1),
            json!({"attempt": 2}),
            "an ask with nothing said about it should still count as an attempt"
        );
        assert_eq!(
            amendment("not json at all", 1),
            json!({"attempt": 2}),
            "an answer that is not an object should carry no amendment and still count"
        );
        assert_eq!(
            amendment(r#"{"loop_back": "survey", "attempt": 99}"#, 1),
            json!({"attempt": 2}),
            "a node overwrote the count of its own attempts"
        );
    }

    /// A node that has not been doubled back to is given no [`AGAIN_KEY`] at all.
    #[tokio::test]
    async fn a_node_on_its_first_run_is_given_nothing_about_a_loop_that_declares_it() {
        let runner = Doubling::new(&[("reply", 1)], "intake", &[]);
        let (graphing, _events) = looping(
            doubling_back(2),
            order(&[("intake", 0), ("reply", 1)]),
            Arc::clone(&runner),
            Some(Reaching::over(&[("intake", &["intake", "reply"])]) as Arc<dyn Downstream>),
        );

        run(&graphing).await;

        assert_eq!(
            runner.runs()[0].1,
            "a ticket",
            "a node declared a loop back's target was told so before anything asked"
        );
    }

    /// A node is told what it may ask for, and told it in terms of the graph it is in.
    #[test]
    fn a_node_that_may_send_work_back_is_told_which_steps_and_how_often() {
        let graph = looping_graph(
            &["survey", "draft", "review"],
            &[("survey", "draft"), ("draft", "review")],
            &[("review", "survey", 2), ("review", "draft", 1)],
        );

        let told =
            loop_advice(graph.loops(), &name("review")).expect("the node declares two loop backs");

        assert!(
            told.contains(r#"{"loop_back": "<step>", "notes": "<what must change>"}"#),
            "the shape to answer in was not spelled out: {told}"
        );
        assert!(
            told.contains("draft (at most 1 times), survey (at most 2 times)"),
            "the steps and their caps were not named, in name order: {told}"
        );
        assert!(
            told.contains("notes are the point"),
            "nothing told the node that a revision needs saying: {told}"
        );
    }

    /// A node with nothing to ask for is told nothing, so one template reads the same in any graph.
    #[test]
    fn a_node_that_may_not_send_work_back_is_told_nothing() {
        let looping = looping_graph(
            &["intake", "reply"],
            &[("intake", "reply")],
            &[("reply", "intake", 2)],
        );

        assert_eq!(loop_advice(looping.loops(), &name("intake")), None);
        assert!(loop_advice(looping.loops(), &name("reply")).is_some());
        assert_eq!(
            loop_advice(graph(&["only"], &[]).loops(), &name("only")),
            None,
            "a graph with no loop back at all told a node about one"
        );
    }

    /// A node is told what it may choose between, in the words the graph's conditions are written
    /// in, and told it as a list because any subset of them may be taken.
    #[test]
    fn a_node_that_may_choose_what_runs_next_is_told_the_steps_and_their_conditions() {
        let edges = [
            branch("triage", "refund", "the customer asks for their money back"),
            branch("triage", "escalate", "the complaint names a legal risk"),
            edge("refund", "close"),
        ];

        let told = branch_advice(&edges, &name("triage")).expect("the node declares two branches");

        assert!(
            told.contains(
                r#"{"branch": ["<step>"], "notes": "<anything those steps should know>"}"#
            ),
            "the shape to answer in was not spelled out: {told}"
        );
        assert!(
            told.contains(
                "escalate (when the complaint names a legal risk), refund (when the customer asks \
                 for their money back)"
            ),
            "the steps and their conditions were not named, in name order: {told}"
        );
        assert!(
            told.contains("several, or all"),
            "nothing said more than one branch may be taken: {told}"
        );
    }

    /// The case the M28 example was tuned for: taking nothing has a shape and a name of its own.
    ///
    /// A node told it may choose will choose unless the no-branch case is as concrete as the
    /// others, so this asserts both halves of it — the json to answer with, and that it is not a
    /// failure.
    #[test]
    fn taking_no_branch_is_spelled_out_as_an_answer_rather_than_an_omission() {
        let edges = [branch("triage", "refund", "the customer asks for a refund")];

        let told = branch_advice(&edges, &name("triage")).expect("the node declares a branch");

        assert!(
            told.contains(r#"{"branch": []}"#),
            "the way to take no branch was not spelled out: {told}"
        );
        assert!(
            told.contains("not a failure"),
            "taking no branch was left looking like a failure: {told}"
        );
    }

    /// An edge without a condition is not a branch, so a plain chain tells its nodes nothing.
    #[test]
    fn a_node_with_no_branch_leaving_it_is_told_nothing_about_choosing() {
        let edges = [
            edge("intake", "reply"),
            branch("reply", "escalate", "it is still unresolved"),
        ];

        assert_eq!(
            branch_advice(&edges, &name("intake")),
            None,
            "a node whose only edge delivers unconditionally was told it may choose"
        );
        assert!(branch_advice(&edges, &name("reply")).is_some());
        assert_eq!(
            branch_advice(
                graph(&["intake", "reply"], &[("intake", "reply")]).edges(),
                &name("intake")
            ),
            None,
            "a graph with no branch at all told a node about one"
        );
    }

    /// A node may carry both, and then it is told both — the loop back first.
    ///
    /// The order is asserted because it is what a member running this node elsewhere must read
    /// too: what to do with what arrived comes before where to send what you make of it.
    #[test]
    fn a_node_that_may_both_double_back_and_choose_is_told_about_both_in_that_order() {
        let loops = [LoopBack {
            from: name("review"),
            to: name("draft"),
            cap: LoopCap::new(2).expect("a cap"),
        }];
        let edges = [
            edge("draft", "review"),
            branch("review", "publish", "the draft is ready to go out"),
        ];
        let node = name("review");

        let told = graph_advice(&loops, &edges, &node).expect("the node carries both");

        let doubling = told
            .find("send this work back")
            .expect("the loop back advice");
        let choosing = told
            .find("Which steps run after you")
            .expect("the branch advice");
        assert!(
            doubling < choosing,
            "the branch was put before the loop back: {told}"
        );
        assert_eq!(
            graph_advice(&loops, &edges, &name("draft")),
            None,
            "a node that neither doubles back nor chooses was told something"
        );
        assert_eq!(
            graph_advice(&[], &edges, &node).as_deref(),
            branch_advice(&edges, &node).as_deref(),
            "a node that only chooses was told more than the branch advice"
        );
    }

    /// The claim [`GRAPH_INPUT_KEY`] rests on, which is why it is spelled with a `$`.
    #[test]
    fn the_key_the_question_arrives_under_can_be_no_nodes_name() {
        for reserved in [GRAPH_INPUT_KEY, AGAIN_KEY] {
            assert!(
                NodeName::new(reserved).is_err(),
                "{reserved} is a name a node could be given"
            );
        }
    }
}
