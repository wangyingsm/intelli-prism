//! What a graph run says while it runs.

use ip_run::{RunEvent, Spend, Stopped};
use serde_json::{Value, json};

use crate::id::{GraphRunId, NodeName};

/// One thing a graph run said, and which run said it.
///
/// The run id sits here rather than in every variant of [`GraphEvent`] because it is the one
/// thing every event carries: a stream is about one graph run, and an event lifted out of that
/// stream — written to a log, kept beside a persisted answer, matched against a dispatch that
/// was retried — still says which run it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphSaid {
    /// Which graph run said it.
    pub run: GraphRunId,
    /// What it said.
    pub event: GraphEvent,
}

impl GraphSaid {
    /// The name this is streamed under.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.event.name()
    }

    /// What it carries, as the json one event holds, the run it belongs to included.
    #[must_use]
    pub fn body(&self) -> Value {
        let mut body = self.event.body();
        if let Some(body) = body.as_object_mut() {
            body.insert("run".to_owned(), Value::String(self.run.to_hex()));
        }
        body
    }
}

/// One thing a graph run says.
///
/// A graph's own events and a node's are one vocabulary rather than two streams, because a reader
/// wants them interleaved: what the graph is doing is which nodes are running, and what *they* are
/// doing is turns. Every event names the node it belongs to, [`GraphEvent::Done`] excepted, which
/// is the graph's own.
#[derive(Debug, Clone, PartialEq)]
pub enum GraphEvent {
    /// A node has begun, on the input the graph built for it.
    Node {
        /// Which node.
        name: NodeName,
        /// What it was given.
        input: String,
    },
    /// A node is over.
    NodeDone {
        /// Which node.
        name: NodeName,
        /// What it answered.
        answer: String,
        /// Why its run ended.
        stopped: Stopped,
        /// What it spent.
        spend: Spend,
    },
    /// Something the node currently running said, as that run said it.
    Turn {
        /// Which node said it.
        node: NodeName,
        /// What it said.
        event: RunEvent,
    },
    /// The graph is over, and this is all of it.
    Done {
        /// What it answered: every node nothing reads from, keyed by name.
        answer: Value,
        /// The node that stopped it, where one did.
        stopped_at: Option<NodeName>,
        /// What the whole graph spent.
        spend: Spend,
    },
}

impl GraphEvent {
    /// The name this is streamed under.
    ///
    /// A node's turn-level event keeps `ip-run`'s own name for it, so a reader that already
    /// understands a template run's stream understands a graph's: what a graph adds is the `node`
    /// in the body, and two events of its own.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Node { .. } => "node",
            Self::NodeDone { .. } => "node_done",
            Self::Turn { event, .. } => event.name(),
            Self::Done { .. } => "done",
        }
    }

    /// What it carries, as the json one event holds.
    #[must_use]
    pub fn body(&self) -> Value {
        match self {
            Self::Node { name, input } => json!({"node": name.as_str(), "input": input}),
            Self::NodeDone {
                name,
                answer,
                stopped,
                spend,
            } => json!({
                "node": name.as_str(),
                "text": answer,
                "stop": stopped.name(),
                "why": stopped.to_string(),
                "tokens": tokens(*spend),
                "cost_micros": spend.cost.micros(),
            }),
            Self::Turn { node, event } => {
                let mut body = event.body();
                if let Some(body) = body.as_object_mut() {
                    body.insert("node".to_owned(), Value::String(node.to_string()));
                }
                body
            }
            Self::Done {
                answer,
                stopped_at,
                spend,
            } => json!({
                "answer": answer,
                "stopped_at": stopped_at.as_ref().map(NodeName::as_str),
                "tokens": tokens(*spend),
                "cost_micros": spend.cost.micros(),
            }),
        }
    }
}

/// What was spent, as both events that carry a total spell it.
fn tokens(spend: Spend) -> Value {
    json!({
        "input": spend.tokens.input.get(),
        "output": spend.tokens.output.get(),
    })
}

/// Where a graph run says what it is doing while it does it.
///
/// Saying is never allowed to fail a graph or to hold it up, for the reason `ip_run::Reporting`
/// gives: a watcher that has hung up is not the run's problem. It is a separate seam from that
/// one because both ends are in play at once — a graph says its own events through this, while
/// each node's run says its turns through `ip-run`'s, and whatever implements [`RunsNode`] is
/// what joins the two into one stream.
///
/// [`RunsNode`]: crate::RunsNode
pub trait GraphReporting: Send + Sync {
    /// Says one thing.
    fn say(&self, said: GraphSaid);
}

/// Says nothing, which is what a caller that only wants the answer needs.
impl GraphReporting for () {
    fn say(&self, _: GraphSaid) {}
}

/// Says it down a channel, dropping it if nobody is listening any more.
impl GraphReporting for tokio::sync::mpsc::UnboundedSender<GraphSaid> {
    fn say(&self, said: GraphSaid) {
        let _ = self.send(said);
    }
}

#[cfg(test)]
mod tests {
    use ip_core::{Money, TokenCount, Tokens};
    use ip_run::TurnNumber;

    use super::*;

    fn name(raw: &str) -> NodeName {
        NodeName::new(raw).expect("a name a node may carry")
    }

    fn spent() -> Spend {
        Spend::new(
            Tokens::new(TokenCount::new(11), TokenCount::new(22)),
            Money::from_micros(330),
        )
    }

    #[test]
    fn a_graphs_own_events_say_which_node_they_are_about() {
        let started = GraphEvent::Node {
            name: name("intake"),
            input: "a ticket".to_owned(),
        };
        assert_eq!(started.name(), "node");
        assert_eq!(
            started.body(),
            json!({"node": "intake", "input": "a ticket"})
        );

        let over = GraphEvent::NodeDone {
            name: name("intake"),
            answer: "sorted".to_owned(),
            stopped: Stopped::Answered,
            spend: spent(),
        };
        assert_eq!(over.name(), "node_done");
        assert_eq!(
            over.body(),
            json!({
                "node": "intake",
                "text": "sorted",
                "stop": "answered",
                "why": "the model answered",
                "tokens": {"input": 11, "output": 22},
                "cost_micros": 330,
            })
        );
    }

    /// A reader that understands a template run's stream understands a graph's.
    #[test]
    fn a_nodes_turn_keeps_its_own_name_and_gains_the_node() {
        let said = GraphEvent::Turn {
            node: name("reply"),
            event: RunEvent::Turn {
                number: TurnNumber::FIRST,
                model: None,
            },
        };
        assert_eq!(said.name(), "turn", "a turn was renamed on its way out");
        assert_eq!(
            said.body(),
            json!({"number": 1, "model": null, "node": "reply"})
        );
    }

    #[test]
    fn a_graph_that_was_stopped_says_which_node_stopped_it() {
        let through = GraphEvent::Done {
            answer: json!({"reply": "sorted"}),
            stopped_at: None,
            spend: spent(),
        };
        assert_eq!(through.name(), "done");
        assert_eq!(through.body()["stopped_at"], Value::Null);
        assert_eq!(through.body()["answer"], json!({"reply": "sorted"}));

        let stopped = GraphEvent::Done {
            answer: json!({}),
            stopped_at: Some(name("intake")),
            spend: spent(),
        };
        assert_eq!(stopped.body()["stopped_at"], "intake");
        assert_eq!(stopped.body()["cost_micros"], 330);
    }

    fn said(event: GraphEvent) -> GraphSaid {
        GraphSaid {
            run: GraphRunId::generate().expect("a graph run id"),
            event,
        }
    }

    #[test]
    fn saying_nothing_is_what_a_caller_that_only_wants_the_answer_does() {
        GraphReporting::say(
            &(),
            said(GraphEvent::Done {
                answer: json!({}),
                stopped_at: None,
                spend: Spend::NOTHING,
            }),
        );
    }

    #[test]
    fn a_watcher_that_has_hung_up_is_not_the_graphs_problem() {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        drop(receiver);
        sender.say(said(GraphEvent::Node {
            name: name("intake"),
            input: String::new(),
        }));
    }

    /// Every event of a stream says which run it is from, whatever kind of event it is.
    #[test]
    fn every_event_carries_the_run_it_belongs_to() {
        let run = GraphRunId::generate().expect("a graph run id");
        for event in [
            GraphEvent::Node {
                name: name("intake"),
                input: String::new(),
            },
            GraphEvent::NodeDone {
                name: name("intake"),
                answer: String::new(),
                stopped: Stopped::Answered,
                spend: Spend::NOTHING,
            },
            GraphEvent::Turn {
                node: name("intake"),
                event: RunEvent::Text {
                    text: "four".to_owned(),
                },
            },
            GraphEvent::Done {
                answer: json!({}),
                stopped_at: None,
                spend: Spend::NOTHING,
            },
        ] {
            let said = GraphSaid { run, event };
            assert_eq!(
                said.body()["run"],
                run.to_hex(),
                "{} carried no run",
                said.name()
            );
        }
    }

    /// Hex, 32 characters of it, and never two the same.
    #[test]
    fn a_graph_run_is_named_by_something_nothing_else_will_be() {
        let one = GraphRunId::generate().expect("a graph run id");
        let two = GraphRunId::generate().expect("another");
        assert_ne!(one, two);
        assert_eq!(one.to_hex().len(), 32);
        assert_eq!(one.to_string(), one.to_hex());
        assert!(one.to_hex().chars().all(|ch| ch.is_ascii_hexdigit()));
    }
}
