//! Taking out of a body what an operator said must not leave.
//!
//! This is not a [`crate::processor::BodyProcessor`] and does not sit in the plugin
//! chain, for two reasons. A processor returns only bytes, so a finding would have
//! nowhere to go and the answer could not say what was taken out. And the chain is
//! assembled per tenant: anything in it can be configured away, where an operator's
//! floor is the one thing that must not be.
//!
//! It runs *after* the chain, so a plugin cannot introduce a secret the filter has
//! already walked past, and *before* the response key is taken, so what is cached is
//! what was redacted rather than what arrived.

use bytes::Bytes;
use http::{HeaderMap, HeaderName};
use ip_filter::{Detectors, Redaction, redact};

/// The header an answer carries to say what was taken out of its request.
pub const REDACTED: HeaderName = HeaderName::from_static("x-ip-redacted");

/// Why a body was not looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unscanned {
    /// Too large to hold, or travelling in a way that forbids holding it. Reading it
    /// would mean buffering what the gateway has already promised to stream.
    NotHeld,
    /// Not json. Every body this gateway is for is json, and walking the strings of
    /// something else would mean guessing where its text begins and ends.
    NotJson,
}

impl Unscanned {
    /// The word the answer's header carries.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::NotHeld => "unscanned: not held",
            Self::NotJson => "unscanned: not json",
        }
    }
}

/// What the filter made of one body.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Filtered {
    /// Looked at, and there was nothing to take out. The bytes went on untouched, which
    /// is what keeps the turn anchor where it was.
    #[default]
    Clean,
    /// Looked at, and this much was replaced.
    Redacted(Redaction),
    /// Not looked at, and why. Said out loud rather than passed over in silence: a
    /// filter with a hole an operator cannot see is worse than one they know about.
    Unscanned(Unscanned),
}

impl Filtered {
    /// What the answer's header should say, or nothing when there is nothing to report.
    #[must_use]
    pub fn said(&self) -> Option<String> {
        match self {
            Self::Clean => None,
            Self::Redacted(found) => Some(found.said()),
            Self::Unscanned(why) => Some(why.name().to_owned()),
        }
    }

    /// What was replaced, for a ledger that counts it.
    #[must_use]
    pub const fn redaction(&self) -> Option<&Redaction> {
        match self {
            Self::Redacted(found) => Some(found),
            Self::Clean | Self::Unscanned(_) => None,
        }
    }
}

/// The secrets in force, and the scan that applies them.
///
/// One set for now. The floor an operator configures and the rules a tenant adds to it
/// resolve to one of these per request.
pub struct Filtering {
    detectors: Detectors,
}

impl Filtering {
    /// Filters with these.
    #[must_use]
    pub const fn new(detectors: Detectors) -> Self {
        Self { detectors }
    }

    /// The body as it should go on, and what was taken out of it.
    ///
    /// A clean body comes back as `None`, so its bytes are forwarded exactly as they
    /// arrived: re-serialising one would move the turn anchor of every request the day
    /// the filter was switched on, and cost the upstream's own prompt cache with it.
    #[must_use]
    pub fn over(&self, bytes: &[u8]) -> (Option<Bytes>, Filtered) {
        let Ok(body) = serde_json::from_slice::<serde_json::Value>(bytes) else {
            return (None, Filtered::Unscanned(Unscanned::NotJson));
        };
        match redact(&self.detectors, &body) {
            None => (None, Filtered::Clean),
            Some((rewritten, found)) => (
                Some(Bytes::from(rewritten.to_string())),
                Filtered::Redacted(found),
            ),
        }
    }
}

/// Puts what the filter did on the answer, where an operator and a caller can both see
/// it. A clean request says nothing, because every answer carrying a header nobody
/// reads is a header nobody reads.
pub fn say(headers: &mut HeaderMap, filtered: &Filtered) {
    let Some(said) = filtered.said() else {
        return;
    };
    if let Ok(value) = http::HeaderValue::from_str(&said) {
        headers.insert(REDACTED, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ip_filter::Secret;

    fn filtering() -> Filtering {
        Filtering::new(Detectors::defaults().unwrap())
    }

    #[test]
    fn a_clean_body_is_not_rewritten_and_says_nothing() {
        let body = br#"{"messages":[{"content":"refactor the retry loop"}]}"#;
        let (rewritten, filtered) = filtering().over(body);
        assert!(
            rewritten.is_none(),
            "the bytes that arrived are the bytes that go"
        );
        assert_eq!(filtered, Filtered::Clean);
        assert_eq!(filtered.said(), None);
    }

    #[test]
    fn a_body_holding_a_secret_comes_back_without_it() {
        let body = br#"{"messages":[{"content":"my key is sk-abcdefghij1234"}]}"#;
        let (rewritten, filtered) = filtering().over(body);
        let sent = String::from_utf8(rewritten.expect("rewritten").to_vec()).unwrap();
        assert!(!sent.contains("sk-abcdefghij1234"));
        assert!(sent.contains("[redacted:openai-key:1]"));
        assert_eq!(filtered.said().unwrap(), "openai-key=1");
        assert_eq!(filtered.redaction().unwrap().total(), 1);
    }

    #[test]
    fn a_body_that_is_not_json_is_said_to_be_unscanned() {
        let (rewritten, filtered) = filtering().over(b"sk-abcdefghij1234 in plain text");
        assert!(rewritten.is_none());
        assert_eq!(filtered, Filtered::Unscanned(Unscanned::NotJson));
        assert_eq!(filtered.said().unwrap(), "unscanned: not json");
    }

    #[test]
    fn a_body_that_was_not_held_says_so_rather_than_passing_in_silence() {
        let filtered = Filtered::Unscanned(Unscanned::NotHeld);
        assert_eq!(filtered.said().unwrap(), "unscanned: not held");
        assert!(filtered.redaction().is_none());
    }

    #[test]
    fn the_header_is_set_only_when_there_is_something_to_say() {
        let mut headers = HeaderMap::new();
        say(&mut headers, &Filtered::Clean);
        assert!(headers.get(REDACTED).is_none());

        let (_, filtered) = filtering().over(br#"{"a":"ada@example.com"}"#);
        say(&mut headers, &filtered);
        assert_eq!(headers.get(REDACTED).unwrap(), "email=1");
    }

    /// The header carries a kind's name, and a name is checked where it is made, so no
    /// value an operator can configure can put a newline into an answer's headers.
    #[test]
    fn a_header_value_is_always_one_a_header_can_hold() {
        let detectors = Detectors::of(&Secret::defaults()).unwrap();
        let body = br#"{"a":"ada@example.com sk-abcdefghij1234 4242424242424242"}"#;
        let (_, filtered) = Filtering::new(detectors).over(body);
        let said = filtered.said().unwrap();
        assert!(http::HeaderValue::from_str(&said).is_ok(), "{said}");
        assert!(!said.contains('\n') && !said.contains('\r'));
    }

    #[test]
    fn filtering_with_nothing_declared_leaves_every_body_clean() {
        let none = Filtering::new(Detectors::of(&[]).unwrap());
        let (rewritten, filtered) = none.over(br#"{"a":"sk-abcdefghij1234"}"#);
        assert!(rewritten.is_none());
        assert_eq!(filtered, Filtered::Clean);
    }
}
