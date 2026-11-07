//! The loop: ask the model, run what it asked for, ask again.

use std::time::{Duration, Instant};

use ip_agents::AgentConfig;
use ip_core::TraceId;
use ip_gateway::Gateway;
use ip_protocol::{Block, Conversation, Message, Role, Sampling};

use crate::ask::{Target, take};
use crate::choice::Pinned;
use crate::id::RunId;
use crate::shed::Toolshed;
use crate::turn::{Spend, Stopped, Turn, TurnNumber};

/// Everything one run is driven with.
pub struct Running<'a> {
    /// What the run is followed under.
    pub id: RunId,
    /// The gateway its turns go through.
    pub gateway: &'a Gateway,
    /// Where those turns are sent, and as whom.
    pub target: &'a Target,
    /// The servers its tools live on, already open.
    pub shed: &'a Toolshed,
    /// The template it is running.
    pub config: &'a AgentConfig,
    /// What it settled on for a model and a family.
    pub pinned: &'a Pinned,
}

/// What a run came to.
///
/// A run that was refused partway is still a run: it took turns, spent tokens and may have
/// run tools, so it comes back as a [`Ran`] saying so rather than as an error. Nothing the
/// model or a tool does fails this call.
#[derive(Debug, Clone, PartialEq)]
pub struct Ran {
    /// What it was followed under.
    pub id: RunId,
    /// Every turn it took, in order.
    pub turns: Vec<Turn>,
    /// Why it ended.
    pub stopped: Stopped,
    /// What the whole of it spent.
    pub spend: Spend,
    /// What each turn was followed under, which is how a run is joined to what it cost.
    pub traces: Vec<TraceId>,
}

impl Ran {
    /// The answer: what the last turn said.
    ///
    /// The last turn is the whole answer because every turn before it ended by asking for a
    /// tool; a model's thinking-aloud on the way is in [`Ran::turns`] and not here.
    #[must_use]
    pub fn text(&self) -> String {
        self.turns.last().map(Turn::text).unwrap_or_default()
    }
}

/// Runs the template until the model answers, the turns run out, or the time does.
pub async fn run(running: &Running<'_>, input: &str) -> Ran {
    let config = running.config;
    let deadline = Instant::now() + seconds(config.time_limit().seconds());
    let per_turn = seconds(config.req_timeout().seconds());
    let mut conversation = opening(running, input);
    let mut turns: Vec<Turn> = Vec::new();
    let mut traces = Vec::new();
    let mut number = TurnNumber::FIRST;
    let mut stopped = Stopped::OutOfTurns {
        after: TurnNumber::FIRST,
    };

    for taken_so_far in 0..config.max_turns().get() {
        if taken_so_far > 0 {
            number = number.next();
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            stopped = Stopped::OutOfTime;
            break;
        }
        let asked = take(running.gateway, running.target, &conversation);
        let taken = match tokio::time::timeout(left.min(per_turn), asked).await {
            // Whichever clock ran out first is what the run is told it ran out of.
            Err(_) => {
                stopped = if left <= per_turn {
                    Stopped::OutOfTime
                } else {
                    Stopped::Refused {
                        why: format!(
                            "a turn took longer than the {}s this template allows one",
                            config.req_timeout().seconds()
                        ),
                    }
                };
                break;
            }
            Ok(Err(error)) => {
                stopped = Stopped::Refused {
                    why: error.to_string(),
                };
                break;
            }
            Ok(Ok(taken)) => taken,
        };
        traces.push(taken.trace);
        let turn = Turn {
            number,
            model: taken.gathered.model.clone(),
            blocks: answered(&taken.gathered),
            spend: Spend::new(taken.gathered.spent_or_zero(), ip_core::Money::ZERO),
        };
        conversation.messages.push(Message {
            role: Role::Assistant,
            blocks: turn.blocks.clone(),
        });
        let asks_again = turn.asks_for_a_tool();
        turns.push(turn);
        if !asks_again {
            stopped = Stopped::Answered;
            break;
        }
        let results = produced(running, turns.last().expect("the turn just pushed")).await;
        conversation.messages.push(Message {
            role: Role::User,
            blocks: results,
        });
        stopped = Stopped::OutOfTurns { after: number };
    }

    Ran {
        spend: turns.iter().map(|turn| turn.spend).sum(),
        id: running.id.clone(),
        turns,
        stopped,
        traces,
    }
}

/// A duration of whole seconds, as the template's own types count them.
fn seconds(count: u32) -> Duration {
    Duration::from_secs(u64::from(count))
}

/// The conversation a run opens with.
fn opening(running: &Running<'_>, input: &str) -> Conversation {
    Conversation {
        model: running.pinned.model.clone(),
        system: Some(running.config.prompt().as_str().to_owned()),
        messages: vec![Message::text(Role::User, input)],
        tools: running.shed.declared().to_vec(),
        sampling: Sampling::default(),
        // A run reads the whole of a turn before it can decide whether to take another.
        stream: false,
    }
}

/// What the model answered, as the conversation holds it.
fn answered(gathered: &ip_protocol::Gathered) -> Vec<Block> {
    let mut blocks = Vec::with_capacity(gathered.calls.len() + 1);
    if !gathered.text.is_empty() {
        blocks.push(Block::Text(gathered.text.clone()));
    }
    for call in &gathered.calls {
        if let ip_protocol::Event::ToolCall {
            id,
            name,
            arguments,
        } = call
        {
            blocks.push(Block::ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            });
        }
    }
    blocks
}

/// Runs everything one turn asked for, in the order it asked.
///
/// One at a time rather than together: a model that asks for two tools usually means the
/// second to see what the first did, and nothing in a template says they may be run at once.
async fn produced(running: &Running<'_>, turn: &Turn) -> Vec<Block> {
    let asked: Vec<_> = turn
        .calls()
        .map(|(id, name, arguments)| (id.clone(), name.clone(), arguments.clone()))
        .collect();
    let mut results = Vec::with_capacity(asked.len());
    for (id, name, arguments) in asked {
        let arguments = match arguments {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        let produced = running.shed.run(&name, arguments).await;
        results.push(Block::ToolResult {
            id,
            content: produced.text,
        });
    }
    results
}

#[cfg(test)]
mod tests {
    use ip_agents::ToolSet;
    use ip_core::Dialect;

    use super::*;
    use crate::fixtures::{
        Fake, Serving, gateway, registered, said, settings, stub, target, template, wants,
    };
    use crate::shed::Toolshed;
    use std::sync::Arc;

    async fn shed_for(tools: &[&str], serving: Serving) -> (Toolshed, crate::fixtures::Called) {
        let (address, called) = stub(serving).await;
        let shed = Toolshed::open(
            &crate::fixtures::asking(tools),
            &[registered("warehouse", address)],
            settings(),
        )
        .await
        .unwrap();
        (shed, called)
    }

    async fn ran(upstream: Arc<Fake>, shed: &Toolshed, config: &AgentConfig) -> Ran {
        let gateway = gateway(upstream);
        let target = target(Dialect::Anthropic);
        run(
            &Running {
                id: RunId::generate().unwrap(),
                gateway: &gateway,
                target: &target,
                shed,
                config,
                pinned: &Pinned::default(),
            },
            "how many are left?",
        )
        .await
    }

    #[tokio::test]
    async fn a_model_that_answers_at_once_takes_one_turn() {
        let (shed, _) = shed_for(&["warehouse.sql"], Serving::Both).await;
        let upstream = Fake::answering(&said("four"));

        let ran = ran(
            Arc::clone(&upstream),
            &shed,
            &template(&["warehouse.sql"], 4, 60, 10),
        )
        .await;

        assert_eq!(ran.stopped, Stopped::Answered);
        assert_eq!(ran.turns.len(), 1);
        assert_eq!(ran.text(), "four");
        assert_eq!(upstream.asked(), 1);
    }

    /// The loop's whole point: what a tool produced goes back in and the model is asked again.
    #[tokio::test]
    async fn a_tool_the_model_asked_for_is_run_and_its_result_goes_back_in() {
        let (shed, called) = shed_for(&["warehouse.sql"], Serving::Both).await;
        let upstream = Fake::in_turn(&[
            &wants(
                "warehouse__sql",
                serde_json::json!({"query": "select count(*)"}),
            ),
            &said("four"),
        ]);

        let ran = ran(
            Arc::clone(&upstream),
            &shed,
            &template(&["warehouse.sql"], 4, 60, 10),
        )
        .await;

        assert_eq!(ran.stopped, Stopped::Answered);
        assert_eq!(ran.turns.len(), 2);
        assert_eq!(ran.text(), "four");
        let call = called
            .lock()
            .unwrap()
            .iter()
            .find(|one| one["method"] == "tools/call")
            .cloned()
            .expect("the tool was run");
        assert_eq!(call["params"]["name"], "sql");
        assert_eq!(call["params"]["arguments"]["query"], "select count(*)");
        let second = upstream.nth(1);
        let messages = second["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3, "the ask, the model's call, the result");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert!(
            messages[2]["content"][0]["content"]
                .as_str()
                .unwrap()
                .contains("select count(*)"),
            "the stub echoes the arguments, so the result carries them"
        );
    }

    /// A model that never stops asking is stopped by the template, not by the upstream.
    #[tokio::test]
    async fn a_model_that_keeps_asking_runs_out_of_turns() {
        let (shed, _) = shed_for(&["warehouse.sql"], Serving::Both).await;
        let upstream = Fake::answering(&wants("warehouse__sql", serde_json::json!({})));

        let ran = ran(
            Arc::clone(&upstream),
            &shed,
            &template(&["warehouse.sql"], 3, 60, 10),
        )
        .await;

        assert_eq!(
            ran.stopped,
            Stopped::OutOfTurns {
                after: TurnNumber::FIRST.next().next()
            }
        );
        assert_eq!(ran.turns.len(), 3);
        assert_eq!(
            upstream.asked(),
            3,
            "the ceiling is turns, not turns plus one"
        );
    }

    /// The run's own clock, which bounds every turn together rather than each one apart.
    #[tokio::test]
    async fn a_run_that_outlasts_its_time_limit_stops() {
        let (shed, _) = shed_for(&["warehouse.sql"], Serving::Both).await;
        let upstream = Fake::slow(
            &wants("warehouse__sql", serde_json::json!({})),
            Duration::from_millis(400),
        );

        let ran = ran(
            Arc::clone(&upstream),
            &shed,
            // The template's own types refuse a per-turn timeout longer than the run's
            // limit, so a run that outlasts its limit is one of several short turns.
            &template(&["warehouse.sql"], 50, 1, 1),
        )
        .await;

        assert_eq!(ran.stopped, Stopped::OutOfTime);
        assert!(!ran.turns.is_empty(), "it got some turns in first");
        assert!(ran.turns.len() < 50, "it did not run the ceiling out");
    }

    /// One turn's clock, which is shorter than the run's and so is what fires.
    #[tokio::test]
    async fn one_turn_outlasting_its_own_timeout_ends_the_run() {
        let (shed, _) = shed_for(&["warehouse.sql"], Serving::Both).await;
        let upstream = Fake::slow(&said("eventually"), Duration::from_secs(30));

        let ran = ran(
            Arc::clone(&upstream),
            &shed,
            &template(&["warehouse.sql"], 4, 600, 1),
        )
        .await;

        assert!(
            matches!(&ran.stopped, Stopped::Refused { why } if why.contains("longer than the 1s")),
            "{:?}",
            ran.stopped
        );
        assert!(ran.turns.is_empty(), "the turn never came back");
    }

    /// A tool that fails is told to the model, which gets another turn to do something else.
    #[tokio::test]
    async fn a_failing_tool_does_not_end_the_run() {
        let (shed, _) = shed_for(&["warehouse.sql"], Serving::RefusesCalls).await;
        let upstream = Fake::in_turn(&[
            &wants("warehouse__sql", serde_json::json!({})),
            &said("I could not look it up"),
        ]);

        let ran = ran(
            Arc::clone(&upstream),
            &shed,
            &template(&["warehouse.sql"], 4, 60, 10),
        )
        .await;

        assert_eq!(ran.stopped, Stopped::Answered);
        assert_eq!(ran.text(), "I could not look it up");
        let second = upstream.nth(1);
        let messages = second["messages"].as_array().unwrap();
        assert!(
            messages[2]["content"][0]["content"]
                .as_str()
                .unwrap()
                .contains("failed"),
            "the model is told the tool failed: {messages:?}"
        );
    }

    /// What the gateway refuses ends the run, because an identical turn would meet it again.
    #[tokio::test]
    async fn a_turn_the_gateway_refuses_ends_the_run() {
        let (shed, _) = shed_for(&["warehouse.sql"], Serving::Both).await;
        let upstream = Fake::holding(
            http::StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":"slow down"}"#,
        );

        let ran = ran(
            Arc::clone(&upstream),
            &shed,
            &template(&["warehouse.sql"], 4, 60, 10),
        )
        .await;

        assert!(
            matches!(&ran.stopped, Stopped::Refused { why } if why.contains("429")),
            "{:?}",
            ran.stopped
        );
        assert!(ran.turns.is_empty());
        assert_eq!(upstream.asked(), 1, "it did not try again");
    }

    #[tokio::test]
    async fn what_every_turn_spent_is_summed_over_the_run() {
        let (shed, _) = shed_for(&["warehouse.sql"], Serving::Both).await;
        let upstream = Fake::in_turn(&[
            &wants("warehouse__sql", serde_json::json!({})),
            &said("four"),
        ]);

        let ran = ran(upstream, &shed, &template(&["warehouse.sql"], 4, 60, 10)).await;

        // 9 in and 7 out asking for the tool, then 9 in and 2 out answering.
        assert_eq!(ran.spend.tokens.total(), 27);
        assert_eq!(ran.traces.len(), 2, "one trace a turn");
    }

    #[tokio::test]
    async fn the_template_and_its_tools_are_what_the_model_is_told() {
        let (shed, _) = shed_for(&["warehouse.sql", "warehouse.search"], Serving::Both).await;
        let upstream = Fake::answering(&said("four"));

        ran(
            Arc::clone(&upstream),
            &shed,
            &template(&["warehouse.sql", "warehouse.search"], 4, 60, 10),
        )
        .await;

        let sent = upstream.sent();
        assert_eq!(sent["system"], "Be helpful.");
        let names: Vec<&str> = sent["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["warehouse__sql", "warehouse__search"]);
        assert_eq!(sent["stream"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn a_template_with_no_tools_still_runs() {
        let shed = Toolshed::open(&ToolSet::default(), &[], settings())
            .await
            .unwrap();
        let upstream = Fake::answering(&said("four"));

        let ran = ran(Arc::clone(&upstream), &shed, &template(&[], 4, 60, 10)).await;

        assert_eq!(ran.stopped, Stopped::Answered);
        assert_eq!(ran.text(), "four");
    }
}
