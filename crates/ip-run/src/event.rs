//! What a run says about itself while it happens.

use ip_core::ModelName;
use ip_protocol::{ToolCallId, ToolName};

use crate::turn::{Spend, Stopped, TurnNumber};

/// One thing a run has to say, as the endpoint streams it.
///
/// A call and its result are two events rather than one, because a tool is run *after* the call
/// is known and may take as long as it likes: a watcher that saw nothing until the result would
/// be told what the model asked for only once it no longer mattered.
#[derive(Debug, Clone, PartialEq)]
pub enum RunEvent {
    /// A turn has begun, and this is what will answer it.
    Turn {
        /// Which turn.
        number: TurnNumber,
        /// The model it is being put to.
        model: ModelName,
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
                "model": model.as_str(),
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
            model: ModelName::new("gpt-4o").unwrap(),
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
}
