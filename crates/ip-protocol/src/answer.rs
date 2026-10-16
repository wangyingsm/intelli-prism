//! What a model's answer is, once the family that spelled it is forgotten.
//!
//! An answer is a sequence rather than a document, and a whole-body answer is the same
//! sequence produced at once. That is deliberate: a family's server-sent events and its
//! single json body are two spellings of one thing, so a reader of either produces these
//! events and a writer of either consumes them. Streaming is then a second sink for the
//! same sequence, not a second implementation of translation.

use ip_core::{ModelName, TokenCount, Tokens};

use crate::conversation::{ToolCallId, ToolName};

/// Why the model stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// It finished what it had to say.
    End,
    /// It reached the ceiling it was given.
    Length,
    /// It wants a tool run before it goes on.
    ToolUse,
    /// The provider withheld the answer.
    Filtered,
    /// A reason only one family gives, kept by name rather than guessed at.
    Other(String),
}

/// One thing that happened in an answer.
///
/// A whole answer read at once yields these in the same order a stream of it would:
/// started, then text and tool calls as they came, then stopped, then what it spent.
/// Nothing here distinguishes a delta from a whole — a text event is whatever text
/// arrived, and a reader of a complete body emits one of them.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The answer beginning, naming the model when the family says which.
    Started {
        /// The model that answered.
        model: Option<ModelName>,
    },
    /// Text, whole or a piece of it.
    Text(String),
    /// A tool the model wants run, whole: a reader that saw it arrive in fragments has
    /// already put it back together.
    ToolCall {
        /// What the result must quote.
        id: ToolCallId,
        /// Which tool.
        name: ToolName,
        /// The arguments, as the model produced them.
        arguments: serde_json::Value,
    },
    /// The answer ending, and why.
    Stopped(Stop),
    /// What it cost, which some families send in the last event and some in one of
    /// their own.
    Spent(Tokens),
}

/// An answer gathered back into its parts, which is what a whole-body writer needs.
///
/// Folding is where a sequence becomes a document: the text events join, the tool calls
/// collect, and the last stop and spend win. A writer for a family then has to decide
/// only how that family spells it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Gathered {
    /// The model that answered, when one was named.
    pub model: Option<ModelName>,
    /// Every text event, joined in order.
    pub text: String,
    /// Every tool call, in order.
    pub calls: Vec<Event>,
    /// Why it stopped, when it said.
    pub stop: Option<Stop>,
    /// What it spent, when it said.
    pub spent: Option<Tokens>,
}

impl Gathered {
    /// Folds a sequence of events into the answer they describe.
    #[must_use]
    pub fn of(events: &[Event]) -> Self {
        let mut gathered = Self::default();
        for event in events {
            match event {
                Event::Started { model } => {
                    if gathered.model.is_none() {
                        gathered.model.clone_from(model);
                    }
                }
                Event::Text(text) => gathered.text.push_str(text),
                call @ Event::ToolCall { .. } => gathered.calls.push(call.clone()),
                Event::Stopped(stop) => gathered.stop = Some(stop.clone()),
                Event::Spent(tokens) => gathered.spent = Some(*tokens),
            }
        }
        gathered
    }

    /// What it spent, or nothing spent, which is what a family that said nothing means
    /// to a writer that must put a number somewhere.
    #[must_use]
    pub fn spent_or_zero(&self) -> Tokens {
        self.spent
            .unwrap_or(Tokens::new(TokenCount::ZERO, TokenCount::ZERO))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str) -> Event {
        Event::ToolCall {
            id: ToolCallId::new(id).unwrap(),
            name: ToolName::new("search").unwrap(),
            arguments: serde_json::json!({"q": "rust"}),
        }
    }

    #[test]
    fn folding_joins_the_text_and_keeps_the_calls_in_order() {
        let events = vec![
            Event::Started {
                model: Some(ModelName::new("claude-opus-5").unwrap()),
            },
            Event::Text("forty-".to_owned()),
            call("call_1"),
            Event::Text("two".to_owned()),
            call("call_2"),
            Event::Stopped(Stop::ToolUse),
            Event::Spent(Tokens::new(TokenCount::new(10), TokenCount::new(2))),
        ];
        let gathered = Gathered::of(&events);
        assert_eq!(gathered.text, "forty-two");
        assert_eq!(gathered.calls, vec![call("call_1"), call("call_2")]);
        assert_eq!(gathered.stop, Some(Stop::ToolUse));
        assert_eq!(gathered.spent.unwrap().total(), 12);
        assert_eq!(
            gathered.model.unwrap().as_str(),
            "claude-opus-5",
            "the first model named is the one that answered"
        );
    }

    #[test]
    fn an_answer_that_said_nothing_of_itself_folds_to_nothing_spent() {
        let gathered = Gathered::of(&[Event::Text("bare".to_owned())]);
        assert_eq!(gathered.text, "bare");
        assert_eq!(gathered.stop, None);
        assert_eq!(gathered.spent, None);
        assert_eq!(gathered.spent_or_zero().total(), 0);
        assert_eq!(gathered.model, None);
    }

    #[test]
    fn a_later_stop_and_spend_win_and_a_later_model_does_not() {
        // A stream corrects its stop reason and totals as it goes; the model it named at
        // the start is the one that answered throughout.
        let events = vec![
            Event::Started {
                model: Some(ModelName::new("first").unwrap()),
            },
            Event::Stopped(Stop::Length),
            Event::Spent(Tokens::new(TokenCount::new(1), TokenCount::new(1))),
            Event::Started {
                model: Some(ModelName::new("second").unwrap()),
            },
            Event::Stopped(Stop::End),
            Event::Spent(Tokens::new(TokenCount::new(9), TokenCount::new(9))),
        ];
        let gathered = Gathered::of(&events);
        assert_eq!(gathered.stop, Some(Stop::End));
        assert_eq!(gathered.spent.unwrap().total(), 18);
        assert_eq!(gathered.model.unwrap().as_str(), "first");
    }
}
