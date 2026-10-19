//! Where a conversation's opening lives, in whichever family wrote it.
//!
//! A turn is recognised by a digest of two things: the standing instruction and the
//! first thing the user said. Both are hashed as the json they arrived as, so that one
//! conversation keeps one anchor for as long as a client keeps sending it — which makes
//! *where those two values live* the only dialect-specific question, and the answer to
//! it is here rather than in the gateway.
//!
//! Nothing is parsed into [`Conversation`](crate::Conversation) for this. The anchor is
//! read on every request of a sticky route, and building a whole conversation — every
//! message, every block, every tool schema cloned — to hash two fields and then throw
//! it away would be a cost paid on the hot path for nothing. These are borrowed.

use serde_json::Value;

/// The two values a conversation is anchored on, and how much conversation there is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Anchoring<'a> {
    /// The standing instruction as its family carries it, absent when there is none.
    pub system: Option<&'a Value>,
    /// The first thing the user said, as its family carries it.
    pub opening: Option<&'a Value>,
    /// How many turns the body holds, which is what a compaction shortens.
    pub messages: usize,
    /// Whether this is one question on its own or a conversation already under way.
    pub exchange: Exchange,
}

/// Whether a body is a single unanswered question, or an exchange in progress.
///
/// **This is not `messages == 1`**, and the difference is a whole family wide. The
/// count is of the body's array, which holds the standing instruction in one family
/// and not in the other, so the same single question reads as one message written one
/// way and two written the other:
///
/// | body | `messages` | `exchange` |
/// |---|---|---|
/// | `{"system": "…", "messages": [user]}` | 1 | `OneQuestion` |
/// | `{"messages": [system, user]}` | 2 | `OneQuestion` |
/// | `{"messages": [user, assistant, user]}` | 3 | `Continuing` |
///
/// Counting messages to find a single turn therefore switches whatever depends on it
/// off for most of one family's traffic and leaves the other's working, which is the
/// kind of bug that gets found in a quarterly report rather than in a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exchange {
    /// Exactly one user turn, and nothing answered yet: a question standing alone.
    OneQuestion,
    /// Several user turns, or an answer already in the body.
    Continuing,
}

/// Finds the anchor's two halves, whichever family wrote the body.
///
/// Answers nothing for a body that is no conversation — an embeddings request, a
/// moderation call — and for a conversation that has no user turn yet, neither of which
/// is a turn to be sticky about.
#[must_use]
pub fn of(value: &Value) -> Option<Anchoring<'_>> {
    if let Some(messages) = value.get("messages").and_then(Value::as_array) {
        let opening = with_role(messages, "user", "content")?;
        return Some(Anchoring {
            // One family puts the instruction beside the conversation and the other
            // puts it first among the messages; both are this same instruction.
            system: value
                .get("system")
                .or_else(|| with_role(messages, "system", "content")),
            opening: Some(opening),
            messages: messages.len(),
            exchange: exchange_of(messages, "assistant"),
        });
    }
    if let Some(contents) = value.get("contents").and_then(Value::as_array) {
        let opening = with_role(contents, "user", "parts")?;
        return Some(Anchoring {
            system: value
                .get("systemInstruction")
                .and_then(|held| held.get("parts")),
            opening: Some(opening),
            messages: contents.len(),
            exchange: exchange_of(contents, "model"),
        });
    }
    None
}

/// Whether these entries are one question or an exchange, given what the family calls
/// the model's own turns — `assistant` where there are messages, `model` where there
/// are contents.
///
/// A tool result is an answer too, whichever way round it is spelled: it can only
/// exist because the model already asked for it.
fn exchange_of(entries: &[Value], answerer: &str) -> Exchange {
    let mut asked = 0usize;
    for entry in entries {
        match entry.get("role").and_then(Value::as_str) {
            // An entry naming no role is the user's, exactly as `with_role` reads it.
            None => asked += 1,
            Some("user") => asked += 1,
            Some(role) if role == answerer || role == "tool" || role == "function" => {
                return Exchange::Continuing;
            }
            // A standing instruction is neither asked nor answered.
            Some(_) => {}
        }
    }
    if asked == 1 {
        Exchange::OneQuestion
    } else {
        Exchange::Continuing
    }
}

/// The named field of the first entry with this role. An entry that names no role at
/// all is the user's, which is how one family writes a conversation's opening turn.
fn with_role<'a>(entries: &'a [Value], role: &str, field: &str) -> Option<&'a Value> {
    entries
        .iter()
        .find(|entry| match entry.get("role").and_then(Value::as_str) {
            Some(named) => named == role,
            None => role == "user",
        })?
        .get(field)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_two_families_that_speak_messages_anchor_on_the_same_values() {
        let openai = json!({"messages": [
            {"role": "system", "content": "you audit accounts"},
            {"role": "user", "content": "what did we spend?"},
        ]});
        let anthropic = json!({
            "system": "you audit accounts",
            "messages": [{"role": "user", "content": "what did we spend?"}],
        });
        let (one, two) = (of(&openai).unwrap(), of(&anthropic).unwrap());
        assert_eq!(one.system, two.system);
        assert_eq!(one.opening, two.opening);
        assert_eq!(
            (one.messages, two.messages),
            (2, 1),
            "the count is of the body, which is what a compaction shortens"
        );
    }

    #[test]
    fn the_family_that_speaks_contents_is_anchored_too() {
        let gemini = json!({
            "systemInstruction": {"parts": [{"text": "you audit accounts"}]},
            "contents": [{"role": "user", "parts": [{"text": "what did we spend?"}]}],
        });
        let found = of(&gemini).expect("a conversation with an opening");
        assert_eq!(found.system, Some(&json!([{"text": "you audit accounts"}])));
        assert_eq!(
            found.opening,
            Some(&json!([{"text": "what did we spend?"}]))
        );
        assert_eq!(found.messages, 1);
    }

    /// The trap the `Exchange` doc names, asserted rather than described: the same
    /// single question counts one message in one family and two in the other, and
    /// both are one question.
    #[test]
    fn the_same_single_question_counts_differently_and_is_one_question_either_way() {
        let anthropic = json!({
            "system": "you audit accounts",
            "messages": [{"role": "user", "content": "what did we spend?"}],
        });
        let openai = json!({"messages": [
            {"role": "system", "content": "you audit accounts"},
            {"role": "user", "content": "what did we spend?"},
        ]});
        let (one, two) = (of(&anthropic).unwrap(), of(&openai).unwrap());
        assert_eq!((one.messages, two.messages), (1, 2), "the counts differ");
        assert_eq!(one.exchange, Exchange::OneQuestion);
        assert_eq!(
            two.exchange,
            Exchange::OneQuestion,
            "counting messages would have read this one as a conversation under way"
        );
    }

    #[test]
    fn an_answer_already_in_the_body_makes_it_an_exchange() {
        for body in [
            json!({"messages": [
                {"role": "user", "content": "what did we spend?"},
                {"role": "assistant", "content": "four pounds"},
            ]}),
            json!({"contents": [
                {"role": "user", "parts": [{"text": "what did we spend?"}]},
                {"role": "model", "parts": [{"text": "four pounds"}]},
            ]}),
        ] {
            assert_eq!(of(&body).unwrap().exchange, Exchange::Continuing, "{body}");
        }
    }

    /// A tool result can only exist because the model already asked for it, so it is
    /// an answer in the body however it is spelled — and it arrives with the user's
    /// turn count still at one, which is what makes it worth its own case.
    #[test]
    fn a_tool_result_is_an_answer_too() {
        for role in ["tool", "function"] {
            let body = json!({"messages": [
                {"role": "user", "content": "what is the weather?"},
                {"role": role, "content": "{\"c\": 14}"},
            ]});
            assert_eq!(
                of(&body).unwrap().exchange,
                Exchange::Continuing,
                "a {role} message is an answer"
            );
        }
    }

    #[test]
    fn more_than_one_thing_asked_is_an_exchange_even_with_nothing_answered() {
        let body = json!({"messages": [
            {"role": "user", "content": "what did we spend?"},
            {"role": "user", "content": "and on what?"},
        ]});
        assert_eq!(of(&body).unwrap().exchange, Exchange::Continuing);
    }

    #[test]
    fn an_instruction_is_neither_asked_nor_answered() {
        // Every role a family uses for standing instruction must leave a single
        // question a single question, or the level switches itself off for whoever
        // writes one.
        for role in ["system", "developer"] {
            let body = json!({"messages": [
                {"role": role, "content": "you audit accounts"},
                {"role": "user", "content": "what did we spend?"},
            ]});
            assert_eq!(
                of(&body).unwrap().exchange,
                Exchange::OneQuestion,
                "a {role} entry should not make this an exchange"
            );
        }
    }

    #[test]
    fn a_lone_question_naming_no_role_is_still_one_question() {
        let gemini = json!({"contents": [{"parts": [{"text": "hello"}]}]});
        assert_eq!(of(&gemini).unwrap().exchange, Exchange::OneQuestion);
    }

    #[test]
    fn a_turn_whose_first_entry_names_no_role_is_the_users() {
        let gemini = json!({"contents": [{"parts": [{"text": "hello"}]}]});
        let found = of(&gemini).expect("an opening");
        assert_eq!(found.opening, Some(&json!([{"text": "hello"}])));
    }

    #[test]
    fn a_body_that_is_no_conversation_is_anchored_on_nothing() {
        assert_eq!(of(&json!({"input": "some text"})), None);
        assert_eq!(of(&json!({"messages": []})), None);
        assert_eq!(of(&json!({"contents": []})), None);
        assert_eq!(
            of(&json!({"messages": [{"role": "system", "content": "alone"}]})),
            None,
            "an instruction with nothing asked of it is no turn"
        );
    }

    #[test]
    fn a_conversation_with_no_instruction_is_anchored_on_its_opening_alone() {
        let body = json!({"messages": [{"role": "user", "content": "hi"}]});
        let found = of(&body).unwrap();
        assert_eq!(found.system, None);
        assert_eq!(found.opening, Some(&json!("hi")));
    }
}
