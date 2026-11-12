//! What a run says about itself while it happens.

use ip_core::ModelName;
use ip_protocol::{ToolCallId, ToolName};

use crate::turn::{Spend, Stopped, TurnNumber};

/// One thing a run has to say, as the endpoint streams it.
///
/// A call and its result are two events rather than one, because a tool is run *after* the call
/// is known and may take as long as it likes: a watcher that saw nothing until the result would
/// be told what the model asked for only once it no longer mattered.
///
/// Its json tag is the word [`RunEvent::name`] streams it under, and its fields are its own —
/// which is what [`RunEvent::body`] is not. `body` is written for a reader and loses what a reader
/// does not need: a `done` body says `out_of_turns` and a sentence, and the turn it stopped after
/// is gone. So an event that has to be read back — one that crossed a wire — is read back from
/// this, and `body` stays the thing a person's stream carries.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunEvent {
    /// A turn has begun, and this is what will answer it.
    Turn {
        /// Which turn.
        number: TurnNumber,
        /// The model it is being put to, where one is known.
        model: Option<ModelName>,
    },
    /// The model asked for a tool to be run.
    ToolCall {
        /// What the result will quote.
        id: ToolCallId,
        /// Which tool, as the template names it.
        name: ToolName,
        /// The arguments the model chose.
        arguments: serde_json::Value,
    },
    /// A tool answered.
    ToolResult {
        /// The call this answers.
        id: ToolCallId,
        /// What it produced, as the model will be given it.
        text: String,
        /// Whether the tool called this a failure.
        failed: bool,
    },
    /// Text the model produced.
    Text {
        /// What it said.
        text: String,
    },
    /// The run is over, and this is all of it.
    Done {
        /// The last thing the model said.
        text: String,
        /// Why it stopped.
        stopped: Stopped,
        /// What the whole run spent.
        spend: Spend,
    },
}

impl RunEvent {
    /// The name this is streamed under.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Turn { .. } => "turn",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolResult { .. } => "tool_result",
            Self::Text { .. } => "text",
            Self::Done { .. } => "done",
        }
    }

    /// What it carries, as the json one event holds.
    ///
    /// Built here rather than derived, because what it is built from — a tool name, a call id —
    /// are the protocol crate's types and are not written for a wire of ours.
    #[must_use]
    pub fn body(&self) -> serde_json::Value {
        match self {
            Self::Turn { number, model } => serde_json::json!({
                "number": number.get(),
                "model": model.as_ref().map(ModelName::as_str),
            }),
            Self::ToolCall {
                id,
                name,
                arguments,
            } => serde_json::json!({
                "id": id.as_str(),
                "name": name.as_str(),
                "arguments": arguments,
            }),
            Self::ToolResult { id, text, failed } => serde_json::json!({
                "id": id.as_str(),
                "text": text,
                "failed": failed,
            }),
            Self::Text { text } => serde_json::json!({"text": text}),
            Self::Done {
                text,
                stopped,
                spend,
            } => serde_json::json!({
                "text": text,
                "stop": stopped.name(),
                "why": stopped.to_string(),
                "tokens": {
                    "input": spend.tokens.input.get(),
                    "output": spend.tokens.output.get(),
                },
                "cost_micros": spend.cost.micros(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use ip_core::{Money, TokenCount, Tokens};

    use super::*;

    #[test]
    fn a_turn_says_which_one_it_is_and_what_is_answering_it() {
        let event = RunEvent::Turn {
            number: TurnNumber::FIRST.next(),
            model: Some(ModelName::new("gpt-4o").unwrap()),
        };

        assert_eq!(event.name(), "turn");
        assert_eq!(
            event.body(),
            serde_json::json!({"number": 2, "model": "gpt-4o"})
        );
    }

    /// Two events, because the tool runs between them and may take as long as it likes.
    #[test]
    fn a_call_and_its_result_are_two_events_tied_by_one_id() {
        let call = RunEvent::ToolCall {
            id: ToolCallId::new("c1").unwrap(),
            name: ToolName::new("warehouse.get_stock").unwrap(),
            arguments: serde_json::json!({"sku": "A-1"}),
        };
        let result = RunEvent::ToolResult {
            id: ToolCallId::new("c1").unwrap(),
            text: "41 crates".to_owned(),
            failed: false,
        };

        assert_eq!(call.name(), "tool_call");
        assert_eq!(
            call.body(),
            serde_json::json!({
                "id": "c1",
                "name": "warehouse.get_stock",
                "arguments": {"sku": "A-1"},
            })
        );
        assert_eq!(result.name(), "tool_result");
        assert_eq!(
            result.body(),
            serde_json::json!({"id": "c1", "text": "41 crates", "failed": false})
        );
        assert_eq!(call.body()["id"], result.body()["id"]);
    }

    #[test]
    fn text_is_what_the_model_said_and_nothing_else() {
        let event = RunEvent::Text {
            text: "Looking it up.".to_owned(),
        };

        assert_eq!(event.name(), "text");
        assert_eq!(event.body(), serde_json::json!({"text": "Looking it up."}));
    }

    #[test]
    fn the_last_event_carries_the_whole_run() {
        let event = RunEvent::Done {
            text: "41 crates in bay 3.".to_owned(),
            stopped: Stopped::OutOfTurns {
                after: TurnNumber::FIRST,
            },
            spend: Spend::new(
                Tokens::new(TokenCount::new(120), TokenCount::new(34)),
                Money::from_micros(5_600),
            ),
        };

        assert_eq!(event.name(), "done");
        assert_eq!(
            event.body(),
            serde_json::json!({
                "text": "41 crates in bay 3.",
                "stop": "out_of_turns",
                "why": "still asking for tools after 1 turns",
                "tokens": {"input": 120, "output": 34},
                "cost_micros": 5_600,
            })
        );
    }

    /// A run that spent nothing says so, rather than leaving the field out.
    #[test]
    fn a_run_that_cost_nothing_still_says_what_it_cost() {
        let event = RunEvent::Done {
            text: String::new(),
            stopped: Stopped::Answered,
            spend: Spend::NOTHING,
        };

        assert_eq!(event.body()["cost_micros"], 0);
        assert_eq!(event.body()["tokens"]["input"], 0);
        assert_eq!(event.body()["stop"], "answered");
    }

    /// Every variant, since what a wire loses it loses one variant at a time.
    #[test]
    fn every_event_survives_being_written_down_and_read_back() {
        let events = [
            RunEvent::Turn {
                number: TurnNumber::FIRST.next(),
                model: Some(ModelName::new("gpt-4o").unwrap()),
            },
            RunEvent::Turn {
                number: TurnNumber::FIRST,
                model: None,
            },
            RunEvent::ToolCall {
                id: ToolCallId::new("c1").unwrap(),
                name: ToolName::new("warehouse.get_stock").unwrap(),
                arguments: serde_json::json!({"sku": "A-1"}),
            },
            RunEvent::ToolResult {
                id: ToolCallId::new("c1").unwrap(),
                text: "41 crates".to_owned(),
                failed: true,
            },
            RunEvent::Text {
                text: "Looking it up.".to_owned(),
            },
            RunEvent::Done {
                text: "41 crates in bay 3.".to_owned(),
                stopped: Stopped::OutOfTurns {
                    after: TurnNumber::FIRST.next(),
                },
                spend: Spend::new(
                    Tokens::new(TokenCount::new(120), TokenCount::new(34)),
                    Money::from_micros(5_600),
                ),
            },
        ];

        for event in events {
            let written = serde_json::to_string(&event).unwrap();
            assert_eq!(
                serde_json::from_str::<RunEvent>(&written).unwrap(),
                event,
                "{written} did not come back as what it went out as"
            );
        }
    }

    /// What `body` drops and this keeps: the turn a run was stopped after.
    ///
    /// The reason the two forms are both here rather than one serving both purposes.
    #[test]
    fn the_json_form_keeps_the_turn_the_body_throws_away() {
        let event = RunEvent::Done {
            text: String::new(),
            stopped: Stopped::OutOfTurns {
                after: TurnNumber::FIRST.next(),
            },
            spend: Spend::NOTHING,
        };

        assert_eq!(event.body()["stop"], "out_of_turns");
        assert_eq!(event.body().get("after"), None);
        let read: RunEvent = serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
        assert_eq!(read, event);
    }

    /// One vocabulary, not two: the json tag is the word the event is streamed under.
    #[test]
    fn an_events_json_tag_is_the_name_it_streams_under() {
        for event in [
            RunEvent::Turn {
                number: TurnNumber::FIRST,
                model: None,
            },
            RunEvent::ToolCall {
                id: ToolCallId::new("c1").unwrap(),
                name: ToolName::new("search").unwrap(),
                arguments: serde_json::Value::Null,
            },
            RunEvent::ToolResult {
                id: ToolCallId::new("c1").unwrap(),
                text: String::new(),
                failed: false,
            },
            RunEvent::Text {
                text: String::new(),
            },
            RunEvent::Done {
                text: String::new(),
                stopped: Stopped::Answered,
                spend: Spend::NOTHING,
            },
        ] {
            let written: serde_json::Value = serde_json::to_value(&event).unwrap();
            let tag = written
                .as_object()
                .and_then(|written| written.keys().next().cloned())
                .expect("an externally tagged event is an object of one key");
            assert_eq!(tag, event.name());
        }
    }
}
