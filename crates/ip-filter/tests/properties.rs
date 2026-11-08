//! What must hold for every input, not for the ones somebody thought of.
//!
//! The unit tests beside the code say what happens to a particular string. These say
//! what must be true of all of them, which is where a filter earns its keep: a secret
//! surviving is a leak whether or not anyone wrote a test for that exact text, and the
//! streaming carry has a correctness condition — the same answer however the pieces
//! fall — that no finite set of examples can stand in for.
//!
//! Everything here goes through the public api, so a property that holds here holds for
//! a caller.

use ip_filter::{Detectors, Kind, Match, Secret, Trickle, redact};
use proptest::prelude::*;
use serde_json::{Value, json};

/// Long enough that no match in a test string outruns it, so a split can never change
/// the answer for a reason this suite is not testing.
const WIDE: usize = 4096;

fn detectors() -> Detectors {
    Detectors::defaults().expect("the shipped detectors build")
}

/// Text that cannot accidentally be, or run into, one of the secrets below.
///
/// Letters, spaces and a little punctuation, with no `@`, no digit run and nothing that
/// can extend a token. A generator free to emit `sk-` would plant secrets the test did
/// not mean to plant, and one free to emit digits beside a card number would change
/// whether that number passes Luhn — both would fail the property for the wrong reason.
fn around() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[a-z ,.!?\u{4e00}-\u{9fa5}]{0,80}")
        .expect("the generator's own pattern is a regex")
}

/// One of each kind of secret this build ships, written as a model or a caller would.
fn secret() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("sk-abcdefghij1234567890".to_owned()),
        Just("ghp_123456789012345678901234567890123456".to_owned()),
        Just("AKIAIOSFODNN7EXAMPLE".to_owned()),
        Just("ada@example.com".to_owned()),
        Just("4242424242424242".to_owned()),
        Just("11010519491231002X".to_owned()),
        Just("GB82WEST12345698765432".to_owned()),
    ]
}

/// Everything a trickle lets out, pushed in pieces of this width.
fn streamed(detectors: &Detectors, window: usize, text: &str, width: usize) -> String {
    let mut trickle = Trickle::new(detectors, window);
    let mut out = String::new();
    let mut piece = String::new();
    for c in text.chars() {
        piece.push(c);
        if piece.chars().count() >= width {
            out.push_str(&trickle.push(&piece));
            piece.clear();
        }
    }
    out.push_str(&trickle.push(&piece));
    out.push_str(&trickle.flush());
    out
}

proptest! {
    /// The property the whole milestone rests on: whatever is written around it, a
    /// secret does not come out the other side.
    #[test]
    fn a_secret_never_survives_a_body(
        before in around(),
        secret in secret(),
        after in around(),
    ) {
        let text = format!("{} {} {}", before, secret, after);
        let body = json!({"messages": [{"content": text}]});
        let (rewritten, found) = redact(&detectors(), &body).expect("a secret is in there");
        prop_assert!(!rewritten.to_string().contains(&secret));
        prop_assert!(found.total() >= 1);
    }

    /// And not out of a stream either, however the pieces fall.
    #[test]
    fn a_secret_never_survives_a_stream(
        before in around(),
        secret in secret(),
        after in around(),
        width in 1usize..24,
    ) {
        let text = format!("{} {} {}", before, secret, after);
        let out = streamed(&detectors(), WIDE, &text, width);
        prop_assert!(!out.contains(&secret), "survived at width {width}: {out}");
    }

    /// The carry's correctness condition. However the text is cut up, the answer is the
    /// answer it would have given if it had seen the whole thing at once.
    #[test]
    fn a_stream_reads_the_same_however_it_is_split(
        text in proptest::string::string_regex("[a-z @.\\-0-9]{0,120}").unwrap(),
        width in 1usize..16,
    ) {
        let detectors = detectors();
        let whole = streamed(&detectors, WIDE, &text, text.chars().count().max(1));
        let split = streamed(&detectors, WIDE, &text, width);
        prop_assert_eq!(split, whole, "width {} read differently", width);
    }

    /// Nothing may be swallowed. Text held back and never let out is an answer the
    /// caller never finishes reading, which is a worse failure than an unfiltered one.
    #[test]
    fn a_stream_with_nothing_to_redact_comes_out_exactly_as_it_went_in(
        text in around(),
        width in 1usize..16,
    ) {
        let out = streamed(&detectors(), WIDE, &text, width);
        prop_assert_eq!(out, text);
    }

    /// The turn anchor is a hash of the body, so the same body must redact to the same
    /// bytes every time or a conversation falls apart.
    ///
    /// The secrets are planted rather than hoped for. A generator of arbitrary text
    /// never produces a valid card number or address by chance, so this would compare
    /// two refusals and pass whatever the placeholder did.
    #[test]
    fn redaction_is_a_function_of_its_input(
        before in around(),
        first in secret(),
        middle in around(),
        second in secret(),
    ) {
        let text = format!("{} {} {} {}", before, first, middle, second);
        let body = json!({"content": text});
        let (once, _) = redact(&detectors(), &body).expect("two secrets are in there");
        let (twice, _) = redact(&detectors(), &body).expect("two secrets are in there");
        prop_assert_eq!(once.to_string(), twice.to_string());
    }

    /// A placeholder is not itself a secret, so filtering what has already been
    /// filtered changes nothing. Were this false, a body crossing two gateways would be
    /// redacted twice over and the second pass would mangle the first.
    #[test]
    fn redacting_what_is_already_redacted_changes_nothing(
        before in around(),
        secret in secret(),
    ) {
        let body = json!({"content": format!("{} {}", before, secret)});
        let (once, _) = redact(&detectors(), &body).expect("a secret is in there");
        prop_assert!(redact(&detectors(), &once).is_none(), "{once}");
    }

    /// A clean body must pass as the bytes that arrived, never re-serialised: that is
    /// what keeps every turn's anchor where it was when the filter was switched on.
    #[test]
    fn a_body_with_nothing_in_it_is_left_alone(text in around()) {
        let body = json!({"a": {"b": [text, 1, null, true]}});
        prop_assert!(redact(&detectors(), &body).is_none());
    }

    /// Redaction rewrites strings and nothing else: the same keys, the same shape, the
    /// same numbers. A filter that reorders or drops a field breaks the request it was
    /// meant to protect.
    #[test]
    fn redaction_keeps_the_shape_of_a_body(
        before in around(),
        secret in secret(),
        n in any::<i32>(),
    ) {
        let body = json!({
            "model": "a-model",
            "n": n,
            "on": true,
            "nothing": null,
            "messages": [{"role": "user", "content": format!("{} {}", before, secret)}],
            "tools": [],
        });
        let (rewritten, _) = redact(&detectors(), &body).expect("a secret is in there");
        prop_assert_eq!(shape(&body), shape(&rewritten));
        prop_assert_eq!(&rewritten["n"], &json!(n));
        prop_assert_eq!(&rewritten["on"], &json!(true));
        prop_assert_eq!(&rewritten["nothing"], &json!(null));
        prop_assert_eq!(&rewritten["model"], &json!("a-model"));
        prop_assert_eq!(&rewritten["messages"][0]["role"], &json!("user"));
    }

    /// Arbitrary text, including every kind of character, must not panic the scan. The
    /// offsets this works in are bytes and the text is not, so this is the property a
    /// hand-written example is least likely to catch.
    #[test]
    fn no_text_can_panic_the_scan(text in "\\PC*") {
        let detectors = detectors();
        let findings = detectors.find(&text);
        for finding in &findings {
            // Both ends land between characters, or slicing would already have panicked.
            prop_assert!(text.is_char_boundary(finding.at().start));
            prop_assert!(text.is_char_boundary(finding.at().end));
            prop_assert!(!finding.text(&text).is_empty());
        }
        for pair in findings.windows(2) {
            prop_assert!(pair[0].at().end <= pair[1].at().start, "findings overlap");
        }
        let _ = redact(&detectors, &json!({"a": text}));
    }

    /// And neither can it panic the carry, at any width.
    #[test]
    fn no_text_can_panic_the_carry(text in "\\PC*", width in 1usize..8, window in 0usize..32) {
        let _ = streamed(&detectors(), window, &text, width);
    }

    /// An operator's own secret is found wherever it is put, by every way of matching.
    /// A prefix reaches through the token it announced, which is the part most easily
    /// got wrong.
    #[test]
    fn an_operators_own_secret_is_found_however_it_is_written(
        before in around(),
        after in around(),
        tail in proptest::string::string_regex("[A-Za-z0-9]{1,20}").unwrap(),
    ) {
        let kind = Kind::new("corp").unwrap();
        for matching in [
            Match::Prefix("MYCORP-".to_owned()),
            Match::Plain(format!("MYCORP-{}", tail)),
            Match::Regexp("MYCORP-[A-Za-z0-9]+".to_owned()),
        ] {
            let detectors = Detectors::of(&[Secret::new(kind.clone(), matching)]).unwrap();
            let planted = format!("MYCORP-{}", tail);
            let body = json!({"a": format!("{} {} {}", before, planted, after)});
            let (rewritten, found) = redact(&detectors, &body).expect("it is in there");
            prop_assert!(!rewritten.to_string().contains(&planted));
            prop_assert_eq!(found.total(), 1);
        }
    }
}

/// A body's shape: every path to a leaf, and what kind of leaf it is, with no values of
/// its strings. Two bodies with the same shape differ only in what was written in them.
fn shape(body: &Value) -> Vec<String> {
    fn walk(at: &str, body: &Value, into: &mut Vec<String>) {
        match body {
            Value::Object(fields) => {
                for (name, field) in fields {
                    walk(&format!("{at}.{name}"), field, into);
                }
            }
            Value::Array(items) => {
                for (n, item) in items.iter().enumerate() {
                    walk(&format!("{at}[{n}]"), item, into);
                }
            }
            Value::String(_) => into.push(format!("{at}:string")),
            Value::Number(_) => into.push(format!("{at}:number")),
            Value::Bool(_) => into.push(format!("{at}:bool")),
            Value::Null => into.push(format!("{at}:null")),
        }
    }
    let mut found = Vec::new();
    walk("", body, &mut found);
    found
}
