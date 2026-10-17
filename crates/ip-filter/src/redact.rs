//! Putting a placeholder where a secret was.
//!
//! The placeholder is `[redacted:<kind>:<n>]`, where `n` counts the distinct values of
//! that kind in order of first appearance. Three properties come out of that shape, and
//! each is load-bearing somewhere else:
//!
//! **It is a pure function of the body.** The same body redacts to the same bytes
//! however many times it is sent, which is what the turn anchor leans on: the anchor is
//! a hash of the body, and a placeholder carrying anything random would give every
//! request a new turn and cost the upstream's own prompt cache as well as ours.
//!
//! **It leaks nothing.** A hash of the secret, even truncated, would be stable too, and
//! would hand the upstream a few bits of the very thing being hidden. An ordinal says
//! only how many came before it.
//!
//! **It cannot break the body it is written into.** [`crate::Kind`] allows lower case
//! letters, digits and hyphens, so a placeholder holds no quote and no backslash. It is
//! therefore valid inside any json string — including a tool call's arguments, which
//! are json serialised into a string, and which stay parseable after a secret inside
//! them is replaced.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use crate::detect::Detectors;
use crate::kind::Kind;

/// What one body's redaction replaced, counted by kind.
///
/// Counts and kinds, never values. A filter that writes secrets to a log or a usage row
/// has moved the leak rather than closed it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Redaction {
    counted: BTreeMap<Kind, usize>,
}

impl Redaction {
    /// How many of each kind were replaced, by kind name.
    #[must_use]
    pub const fn counted(&self) -> &BTreeMap<Kind, usize> {
        &self.counted
    }

    /// How many were replaced in all.
    #[must_use]
    pub fn total(&self) -> usize {
        self.counted.values().sum()
    }

    /// Adds another pass's count to this one, so a request and its answer are reported
    /// as one tally rather than two a caller has to add up.
    pub fn absorb(&mut self, other: &Self) {
        for (kind, count) in &other.counted {
            *self.counted.entry(kind.clone()).or_insert(0) += count;
        }
    }

    /// Whether anything was replaced at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.counted.is_empty()
    }

    /// Counts one more of this kind.
    pub(crate) fn count(&mut self, kind: &Kind) {
        *self.counted.entry(kind.clone()).or_insert(0) += 1;
    }

    /// What was redacted, as a header or a log line says it: `api-key=2, email=1`, the
    /// kinds in a settled order so two gateways word it the same way.
    #[must_use]
    pub fn said(&self) -> String {
        self.counted
            .iter()
            .map(|(kind, count)| format!("{kind}={count}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The ordinals handed out so far, so one value is one placeholder wherever it appears.
///
/// This holds the matched text for the length of one redaction and is dropped with it.
/// Nothing here is returned, logged or stored: it exists only so that the same address
/// twice in one conversation reads as the same person to the model.
#[derive(Default)]
pub(crate) struct Tally {
    given: HashMap<(Kind, String), usize>,
    next: HashMap<Kind, usize>,
}

impl Tally {
    /// The placeholder for this value, the same one every time it is asked for.
    pub(crate) fn placeholder(&mut self, kind: &Kind, value: &str) -> String {
        let key = (kind.clone(), value.to_owned());
        let at = if let Some(at) = self.given.get(&key) {
            *at
        } else {
            let at = self.next.entry(kind.clone()).or_insert(0);
            *at += 1;
            let at = *at;
            self.given.insert(key, at);
            at
        };
        format!("[redacted:{kind}:{at}]")
    }
}

/// Every secret in this body, replaced; or nothing, when there were none.
///
/// Returning nothing rather than an unchanged copy is the point: a body that matched
/// nothing must go on as the bytes that arrived. Re-serialising it would move its turn
/// anchor the day the filter was switched on, for every request, whether or not it
/// carried a secret.
///
/// Every string value is scanned, at any depth. Keys are not: a key is the shape of the
/// message rather than what somebody wrote in it.
#[must_use]
pub fn redact(detectors: &Detectors, body: &Value) -> Option<(Value, Redaction)> {
    // The common request carries no secret at all, so it is asked the cheap question
    // first and never pays for a copy of the tree it does not need.
    if !holds(detectors, body) {
        return None;
    }
    let mut tally = Tally::default();
    let mut found = Redaction::default();
    let rewritten = rewrite(detectors, body, &mut tally, &mut found);
    Some((rewritten, found))
}

/// Whether any string anywhere in this body holds something.
fn holds(detectors: &Detectors, body: &Value) -> bool {
    match body {
        Value::String(text) => !detectors.find(text).is_empty(),
        Value::Array(items) => items.iter().any(|item| holds(detectors, item)),
        Value::Object(fields) => fields.values().any(|field| holds(detectors, field)),
        _ => false,
    }
}

/// The same body with every secret replaced.
fn rewrite(detectors: &Detectors, body: &Value, tally: &mut Tally, found: &mut Redaction) -> Value {
    match body {
        Value::String(text) => Value::String(replace(detectors, text, tally, found)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| rewrite(detectors, item, tally, found))
                .collect(),
        ),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(name, field)| (name.clone(), rewrite(detectors, field, tally, found)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// One string with every secret in it replaced.
fn replace(detectors: &Detectors, text: &str, tally: &mut Tally, found: &mut Redaction) -> String {
    let findings = detectors.find(text);
    if findings.is_empty() {
        return text.to_owned();
    }
    let mut built = String::with_capacity(text.len());
    let mut from = 0;
    for finding in findings {
        built.push_str(&text[from..finding.at().start]);
        built.push_str(&tally.placeholder(finding.kind(), finding.text(text)));
        *found.counted.entry(finding.kind().clone()).or_insert(0) += 1;
        from = finding.at().end;
    }
    built.push_str(&text[from..]);
    built
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::{Match, Secret};
    use serde_json::json;

    fn named(raw: &str) -> Kind {
        Kind::new(raw).unwrap()
    }

    fn defaults() -> Detectors {
        Detectors::defaults().unwrap()
    }

    /// A body with nothing in it comes back as nothing at all, so the caller sends the
    /// bytes that arrived and the turn anchor does not move.
    #[test]
    fn a_body_holding_no_secret_is_not_rewritten() {
        let body = json!({
            "model": "a-model",
            "messages": [{"role": "user", "content": "refactor the retry loop in src/main.rs"}],
        });
        assert!(redact(&defaults(), &body).is_none());
    }

    #[test]
    fn a_secret_becomes_a_placeholder() {
        let body = json!({"messages": [{"content": "my key is sk-abcdefghij1234"}]});
        let (rewritten, found) = redact(&defaults(), &body).unwrap();
        assert_eq!(
            rewritten["messages"][0]["content"],
            json!("my key is [redacted:openai-key:1]")
        );
        assert_eq!(found.total(), 1);
        assert_eq!(found.said(), "openai-key=1");
    }

    /// The same value twice is the same person twice, and the model is left able to say
    /// so.
    #[test]
    fn one_value_gets_one_placeholder_wherever_it_appears() {
        let body = json!({
            "messages": [
                {"content": "write to ada@example.com"},
                {"content": "and copy ada@example.com again"},
            ],
        });
        let (rewritten, found) = redact(&defaults(), &body).unwrap();
        assert_eq!(
            rewritten["messages"][0]["content"],
            json!("write to [redacted:email:1]")
        );
        assert_eq!(
            rewritten["messages"][1]["content"],
            json!("and copy [redacted:email:1] again")
        );
        assert_eq!(found.counted()[&named("email")], 2, "both were replaced");
    }

    #[test]
    fn different_values_are_numbered_in_the_order_they_appear() {
        let body = json!({"content": "ada@example.com then bob@example.org then ada@example.com"});
        let (rewritten, _) = redact(&defaults(), &body).unwrap();
        assert_eq!(
            rewritten["content"],
            json!("[redacted:email:1] then [redacted:email:2] then [redacted:email:1]")
        );
    }

    #[test]
    fn each_kind_is_numbered_on_its_own() {
        let body = json!({"content": "ada@example.com sk-abcdefghij1234 bob@example.org"});
        let (rewritten, found) = redact(&defaults(), &body).unwrap();
        assert_eq!(
            rewritten["content"],
            json!("[redacted:email:1] [redacted:openai-key:1] [redacted:email:2]")
        );
        assert_eq!(found.said(), "email=2, openai-key=1");
    }

    /// The anchor is a hash of the body, so this is what keeps a turn together.
    #[test]
    fn the_same_body_redacts_to_the_same_bytes_every_time() {
        let body = json!({"content": "ada@example.com and sk-abcdefghij1234 and 4242424242424242"});
        let once = redact(&defaults(), &body).unwrap().0.to_string();
        let twice = redact(&defaults(), &body).unwrap().0.to_string();
        assert_eq!(once, twice);
    }

    /// Nothing of the secret may survive into what is sent, not even a few bits of it.
    #[test]
    fn no_part_of_the_secret_is_left_in_the_body() {
        let body = json!({"content": "key sk-abcdefghij1234 and card 4242424242424242"});
        let (rewritten, _) = redact(&defaults(), &body).unwrap();
        let sent = rewritten.to_string();
        assert!(!sent.contains("sk-abcdefghij1234"));
        assert!(!sent.contains("abcdefghij1234"));
        assert!(!sent.contains("4242424242424242"));
    }

    /// A tool call's arguments are json serialised into a string. Replacing a secret
    /// inside them must leave something the upstream can still parse, which is why the
    /// placeholder carries no quote and no backslash.
    #[test]
    fn a_tool_calls_arguments_still_parse_after_redaction() {
        let arguments = json!({"token": "sk-abcdefghij1234", "city": "Paris"}).to_string();
        let body = json!({
            "messages": [{
                "role": "assistant",
                "tool_calls": [{
                    "function": {"name": "lookup", "arguments": arguments},
                }],
            }],
        });
        let (rewritten, _) = redact(&defaults(), &body).unwrap();
        let after = rewritten["messages"][0]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("the arguments are still a string");
        let parsed: Value = serde_json::from_str(after).expect("and still parse as json");
        assert_eq!(parsed["token"], json!("[redacted:openai-key:1]"));
        assert_eq!(parsed["city"], json!("Paris"), "the rest is untouched");
    }

    /// A key is the shape of the message, not what somebody wrote in it.
    #[test]
    fn keys_are_left_alone() {
        let body = json!({"ada@example.com": "a note with no secret in it"});
        assert!(redact(&defaults(), &body).is_none());
    }

    #[test]
    fn everything_that_is_not_a_string_survives_untouched() {
        let body = json!({
            "content": "ada@example.com",
            "temperature": 0.7,
            "max_tokens": 256,
            "stream": false,
            "stop": null,
            "tools": [],
        });
        let (rewritten, _) = redact(&defaults(), &body).unwrap();
        assert_eq!(rewritten["temperature"], json!(0.7));
        assert_eq!(rewritten["max_tokens"], json!(256));
        assert_eq!(rewritten["stream"], json!(false));
        assert_eq!(rewritten["stop"], json!(null));
        assert_eq!(rewritten["tools"], json!([]));
    }

    #[test]
    fn a_secret_nested_deeply_is_still_found() {
        let body = json!({"a": [{"b": [{"c": "sk-abcdefghij1234"}]}]});
        let (rewritten, found) = redact(&defaults(), &body).unwrap();
        assert_eq!(
            rewritten["a"][0]["b"][0]["c"],
            json!("[redacted:openai-key:1]")
        );
        assert_eq!(found.total(), 1);
    }

    #[test]
    fn an_operators_own_secret_is_redacted_like_any_other() {
        let detectors = Detectors::of(&[Secret::new(
            named("corp-token"),
            Match::Prefix("MYCORP-".to_owned()),
        )])
        .unwrap();
        let body = json!({"content": "use MYCORP-abc123 for that"});
        let (rewritten, found) = redact(&detectors, &body).unwrap();
        assert_eq!(
            rewritten["content"],
            json!("use [redacted:corp-token:1] for that")
        );
        assert_eq!(found.said(), "corp-token=1");
    }

    /// Multibyte text around a secret must come through whole, which indexing by byte
    /// offsets only does if the offsets are right.
    #[test]
    fn text_around_a_secret_survives_in_any_script() {
        let body = json!({"content": "密钥是 sk-abcdefghij1234，请不要外传"});
        let (rewritten, _) = redact(&defaults(), &body).unwrap();
        assert_eq!(
            rewritten["content"],
            json!("密钥是 [redacted:openai-key:1]，请不要外传")
        );
    }

    #[test]
    fn nothing_to_look_for_means_nothing_is_rewritten() {
        let none = Detectors::of(&[]).unwrap();
        let body = json!({"content": "sk-abcdefghij1234"});
        assert!(redact(&none, &body).is_none());
    }

    #[test]
    fn a_redaction_that_found_nothing_says_nothing() {
        let empty = Redaction::default();
        assert_eq!(empty.total(), 0);
        assert!(empty.is_empty());
        assert_eq!(empty.said(), "");
    }

    /// A request and its answer are one tally, so a caller reads what was taken out of
    /// the exchange rather than adding two numbers up.
    #[test]
    fn two_passes_add_up_to_one_tally() {
        let going = json!({"content": "ada@example.com and sk-abcdefghij1234"});
        let coming = json!({"content": "bob@example.org"});
        let (_, mut found) = redact(&defaults(), &going).unwrap();
        let (_, back) = redact(&defaults(), &coming).unwrap();
        found.absorb(&back);
        assert_eq!(found.counted()[&named("email")], 2);
        assert_eq!(found.counted()[&named("openai-key")], 1);
        assert_eq!(found.total(), 3);
        assert_eq!(found.said(), "email=2, openai-key=1");
    }

    #[test]
    fn absorbing_nothing_changes_nothing() {
        let body = json!({"content": "ada@example.com"});
        let (_, mut found) = redact(&defaults(), &body).unwrap();
        let was = found.clone();
        found.absorb(&Redaction::default());
        assert_eq!(found, was);
    }
}
