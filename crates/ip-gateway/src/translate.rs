//! Carrying a request across the gap between two provider families.
//!
//! A rule that names one family for its callers and another for its endpoints bridges
//! them: the request is read into the canonical shape on the way out and written in the
//! upstream's spelling, and the answer makes the same journey back. A rule that names
//! neither, or names one family on both sides, bridges nothing — and this module is
//! never reached for it, because translation is a thing an operator turns on for a
//! route and never a cost the gateway pays everywhere.
//!
//! Translation is best-effort by ruling. What maps is carried; what does not is
//! recorded in [`Dropped`] and left behind, so the request is served rather than
//! refused. Commit 07 is what makes that record reach the caller.

use ip_core::{Dialect, RouteRule};
use ip_protocol::{Conversation, Dropped, Event, ProtocolError};
use serde_json::Value;

/// The header naming what a crossing could not carry, on any answer that lost
/// something and on no other.
///
/// Translation is best-effort by ruling: a request whose tools the target family cannot
/// express is served without them, and the model answers never knowing it could have
/// called anything. From the client's side that is indistinguishable from a model that
/// chose not to. This header is what makes the difference visible without reading the
/// gateway's journal, which is why it carries a short machine-readable list and not
/// prose: a client that wants to react to it must be able to.
///
/// It is set before the response header chain runs, so a plugin sees it like any other
/// header and may act on it — rewrite it into an operator's own spelling, or count it.
/// A plugin that removes it is an operator choosing to, which is theirs to choose; the
/// gateway's part is to have said so.
pub(crate) const UNTRANSLATED: http::HeaderName =
    http::HeaderName::from_static("x-ip-untranslated");

/// The two families a rule carries a request between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Crossing {
    /// What the caller sent.
    from: Dialect,
    /// What the endpoints behind the rule speak.
    to: Dialect,
}

impl Crossing {
    /// What this rule bridges, or nothing when it bridges nothing: a rule that leaves
    /// either side unnamed, or names one family on both, forwards bytes untouched.
    pub(crate) fn of(rule: &RouteRule) -> Option<Self> {
        let (from, to) = (rule.caller_speaks?, rule.upstream_speaks?);
        (from != to).then_some(Self { from, to })
    }

    /// The family the caller speaks.
    pub(crate) const fn from(self) -> Dialect {
        self.from
    }

    /// The family the upstream speaks.
    pub(crate) const fn to(self) -> Dialect {
        self.to
    }
}

/// Reads a request the caller sent and writes it as the upstream expects.
///
/// Answers nothing for a body that is no conversation — an embeddings call, a
/// moderation request, anything else behind the same prefix — which travels untouched
/// because this layer has nothing to say about it.
pub(crate) fn request(crossing: Crossing, body: &[u8]) -> Option<(Conversation, Vec<u8>, Dropped)> {
    let (conversation, mut dropped) = read_conversation(crossing.from, body).ok()?;
    let (written, writing) = write_conversation(crossing.to, &conversation);
    dropped.absorb(&writing);
    Some((conversation, written.to_string().into_bytes(), dropped))
}

/// Reads an answer the upstream gave and writes it as the caller expects, which is the
/// same journey in the other direction.
pub(crate) fn answer(crossing: Crossing, body: &[u8]) -> Option<(Vec<u8>, Dropped)> {
    let (events, mut dropped) = read_answer(crossing.to, body).ok()?;
    let (written, writing) = write_answer(crossing.from, &events);
    dropped.absorb(&writing);
    Some((written.to_string().into_bytes(), dropped))
}

/// A request body, read by whichever family wrote it.
fn read_conversation(
    dialect: Dialect,
    body: &[u8],
) -> Result<(Conversation, Dropped), ProtocolError> {
    match dialect {
        Dialect::OpenAi => ip_protocol::openai::read(body),
        Dialect::Anthropic => ip_protocol::anthropic::read(body),
        Dialect::Gemini => ip_protocol::gemini::read(body),
    }
}

/// A request body, written as one family spells it.
fn write_conversation(dialect: Dialect, conversation: &Conversation) -> (Value, Dropped) {
    match dialect {
        Dialect::OpenAi => ip_protocol::openai::write(conversation),
        Dialect::Anthropic => ip_protocol::anthropic::write(conversation),
        Dialect::Gemini => ip_protocol::gemini::write(conversation),
    }
}

/// An answer, read by whichever family gave it.
fn read_answer(dialect: Dialect, body: &[u8]) -> Result<(Vec<Event>, Dropped), ProtocolError> {
    match dialect {
        Dialect::OpenAi => ip_protocol::openai::read_answer(body),
        Dialect::Anthropic => ip_protocol::anthropic::read_answer(body),
        Dialect::Gemini => ip_protocol::gemini::read_answer(body),
    }
}

/// An answer, written as one family spells it.
fn write_answer(dialect: Dialect, events: &[Event]) -> (Value, Dropped) {
    match dialect {
        Dialect::OpenAi => ip_protocol::openai::write_answer(events),
        Dialect::Anthropic => ip_protocol::anthropic::write_answer(events),
        Dialect::Gemini => ip_protocol::gemini::write_answer(events),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ip_core::{AbsPath, ApiId, Endpoint, Host, Port, Protocol, RouteKey, RouteTarget};
    use serde_json::json;

    fn rule(caller: Option<Dialect>, upstream: Option<Dialect>) -> RouteRule {
        let endpoint = Endpoint::new(
            Protocol::Https,
            Host::new("api.example.com").unwrap(),
            Port::new(443).unwrap(),
            AbsPath::new("/v1").unwrap(),
        );
        RouteRule {
            api: ApiId::new("anthropic").unwrap(),
            key: RouteKey::new(endpoint.clone()),
            target: RouteTarget::new(endpoint),
            strategy: ip_core::Strategy::default(),
            hedge: ip_core::Hedge::default(),
            hedge_delay: ip_core::HedgeDelay::default(),
            caller_speaks: caller,
            upstream_speaks: upstream,
        }
    }

    #[test]
    fn a_rule_bridges_only_when_it_names_two_different_families() {
        assert_eq!(Crossing::of(&rule(None, None)), None);
        assert_eq!(Crossing::of(&rule(Some(Dialect::OpenAi), None)), None);
        assert_eq!(Crossing::of(&rule(None, Some(Dialect::Gemini))), None);
        assert_eq!(
            Crossing::of(&rule(Some(Dialect::OpenAi), Some(Dialect::OpenAi))),
            None,
            "one family on both sides is no crossing"
        );
        let crossing =
            Crossing::of(&rule(Some(Dialect::OpenAi), Some(Dialect::Gemini))).expect("a crossing");
        assert_eq!(
            (crossing.from(), crossing.to()),
            (Dialect::OpenAi, Dialect::Gemini)
        );
    }

    #[test]
    fn a_request_crosses_and_its_answer_comes_back_the_other_way() {
        let crossing =
            Crossing::of(&rule(Some(Dialect::OpenAi), Some(Dialect::Anthropic))).unwrap();
        let sent = json!({
            "model": "shared",
            "max_tokens": 50,
            "messages": [
                {"role": "system", "content": "you audit accounts"},
                {"role": "user", "content": "hello"},
            ],
        });
        let (_, written, dropped) =
            request(crossing, sent.to_string().as_bytes()).expect("a crossing");
        assert!(dropped.is_empty(), "{dropped}");
        let crossed: Value = serde_json::from_slice(&written).unwrap();
        assert_eq!(crossed["system"], json!("you audit accounts"));
        assert_eq!(crossed["messages"].as_array().unwrap().len(), 1);

        let answered = json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "forty-two"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 2},
        });
        let (back, dropped) = answer(crossing, answered.to_string().as_bytes()).expect("an answer");
        assert!(dropped.is_empty(), "{dropped}");
        let home: Value = serde_json::from_slice(&back).unwrap();
        assert_eq!(home["choices"][0]["message"]["content"], json!("forty-two"));
        assert_eq!(home["choices"][0]["finish_reason"], json!("stop"));
        assert_eq!(home["usage"]["prompt_tokens"], json!(3));
    }

    #[test]
    fn a_body_that_is_no_conversation_does_not_cross() {
        let crossing = Crossing::of(&rule(Some(Dialect::OpenAi), Some(Dialect::Gemini))).unwrap();
        let embeddings = json!({"model": "text-embedding-3-small", "input": "some text"});
        assert!(request(crossing, embeddings.to_string().as_bytes()).is_none());
        assert!(request(crossing, b"not json").is_none());
        assert!(answer(crossing, b"not an answer either").is_none());
    }

    /// What a crossing cannot carry is recorded rather than refused, which is the
    /// ruling this milestone was given.
    #[test]
    fn what_a_crossing_cannot_carry_is_recorded() {
        let crossing =
            Crossing::of(&rule(Some(Dialect::OpenAi), Some(Dialect::Anthropic))).unwrap();
        // No ceiling was asked for, and the family it crosses into demands one.
        let sent = json!({"messages": [{"role": "user", "content": "hello"}], "seed": 7});
        let (_, _, dropped) = request(crossing, sent.to_string().as_bytes()).unwrap();
        assert_eq!(dropped.names(), vec!["max_tokens", "seed"]);
    }
}
