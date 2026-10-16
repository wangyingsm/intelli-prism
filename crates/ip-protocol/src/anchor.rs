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
        });
    }
    None
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
