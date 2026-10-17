//! Redacting text that arrives a piece at a time.
//!
//! A streamed answer is written token by token, so a secret is written token by token
//! too: `sk-`, then `abcdefg`, then `hij1234`. Each piece is clean on its own, and a
//! filter that looks at one piece at a time finds nothing at all. For the case this
//! exists to stop, looking at each piece on its own is not a weaker answer — it is no
//! answer.
//!
//! So text is held back until it is known that nothing is still growing into a match,
//! and that holding is what this does. The cost cannot be designed away: a caller
//! cannot be handed a token the instant it arrives *and* be told whether it is the
//! first three characters of a key. The window is how much of that cost an operator is
//! willing to pay, and a window of zero is the per-piece behaviour, honestly named.
//!
//! A match longer than the window outruns it and goes out in pieces. That is what a
//! window means, it is reported by [`Trickle::overran`], and the remedy is a larger one.

use serde_json::Value;

use crate::detect::{Detectors, Finding};
use crate::redact::{Redaction, Tally, with_tally};

/// The most text held at once, whatever the window, so a match that never finishes
/// cannot grow the carry without end.
const HOLD_MOST: usize = 8 * 1024;

/// A redactor that reads text in pieces and lets it out a piece behind.
///
/// The ordinals and the tally span the whole stream, so one secret is one placeholder
/// however many pieces it arrived in and however far apart they were.
pub struct Trickle<'a> {
    detectors: &'a Detectors,
    window: usize,
    carry: String,
    tally: Tally,
    found: Redaction,
    overran: bool,
}

impl<'a> Trickle<'a> {
    /// Holds back `window` characters while looking for these.
    ///
    /// A window of zero holds nothing: every piece is redacted alone and let straight
    /// out, which catches only what arrives whole in one of them.
    #[must_use]
    pub fn new(detectors: &'a Detectors, window: usize) -> Self {
        Self {
            detectors,
            window,
            carry: String::new(),
            tally: Tally::default(),
            found: Redaction::default(),
            overran: false,
        }
    }

    /// Takes the next piece and gives back what is now safe to send.
    #[must_use]
    pub fn push(&mut self, text: &str) -> String {
        self.carry.push_str(text);
        let held = self.carry.chars().count();
        // A match that keeps growing would otherwise hold the carry open for ever, so
        // past this much the oldest text goes out whether or not it finished a match.
        let window = if held > HOLD_MOST {
            self.overran = true;
            0
        } else {
            self.window
        };
        let mut boundary = leaving(&self.carry, window);
        let findings = self.detectors.find(&self.carry);
        // Anything still growing across the boundary is held whole: releasing its first
        // half would send out the beginning of a secret and redact only the rest.
        for finding in &findings {
            if finding.at().start < boundary && finding.at().end > boundary {
                boundary = finding.at().start;
            }
        }
        self.release(boundary, &findings)
    }

    /// Gives back everything still held, redacted as far as it can be.
    ///
    /// Called once, at the end. Whatever is held and never released is an answer the
    /// caller never finishes reading, which is worse than one that was not filtered.
    #[must_use]
    pub fn flush(&mut self) -> String {
        let findings = self.detectors.find(&self.carry);
        let boundary = self.carry.len();
        self.release(boundary, &findings)
    }

    /// A whole value — a tool call's arguments — redacted against this stream's own
    /// ordinals, or nothing when it held none. Text and tool calls share the numbering,
    /// so one secret reads as one placeholder wherever in the answer it turns up.
    #[must_use]
    pub fn value(&mut self, body: &Value) -> Option<Value> {
        with_tally(self.detectors, body, &mut self.tally, &mut self.found)
    }

    /// What has been replaced so far.
    #[must_use]
    pub const fn found(&self) -> &Redaction {
        &self.found
    }

    /// Whether text was let out before it could be judged, because a match grew past
    /// what may be held. An operator seeing this wants a larger window.
    #[must_use]
    pub const fn overran(&self) -> bool {
        self.overran
    }

    /// Lets out everything before `boundary`, with the findings inside it replaced.
    fn release(&mut self, boundary: usize, findings: &[Finding]) -> String {
        if boundary == 0 {
            return String::new();
        }
        let carry = std::mem::take(&mut self.carry);
        let mut built = String::with_capacity(boundary);
        let mut from = 0;
        for finding in findings.iter().filter(|one| one.at().end <= boundary) {
            built.push_str(&carry[from..finding.at().start]);
            built.push_str(&self.tally.placeholder(finding.kind(), finding.text(&carry)));
            self.found.count(finding.kind());
            from = finding.at().end;
        }
        built.push_str(&carry[from..boundary]);
        self.carry = carry[boundary..].to_owned();
        built
    }
}

/// The byte offset that leaves this many characters at the end.
fn leaving(text: &str, chars_at_end: usize) -> usize {
    if chars_at_end == 0 {
        return text.len();
    }
    let total = text.chars().count();
    if total <= chars_at_end {
        return 0;
    }
    text.char_indices()
        .nth(total - chars_at_end)
        .map_or(text.len(), |(at, _)| at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kind::Kind;
    use crate::secret::{Match, Secret};

    fn defaults() -> Detectors {
        Detectors::defaults().unwrap()
    }

    /// Everything the trickle let out, in order, pushed one piece at a time.
    fn streamed(detectors: &Detectors, window: usize, pieces: &[&str]) -> (String, Redaction) {
        let mut trickle = Trickle::new(detectors, window);
        let mut out = String::new();
        for piece in pieces {
            out.push_str(&trickle.push(piece));
        }
        out.push_str(&trickle.flush());
        (out, trickle.found().clone())
    }

    /// The reason this file exists: a key arriving in three pieces is still one key.
    #[test]
    fn a_secret_split_across_pieces_is_caught() {
        let (out, found) = streamed(&defaults(), 64, &["my key is sk-", "abcdefg", "hij1234 ok"]);
        assert_eq!(out, "my key is [redacted:openai-key:1] ok");
        assert_eq!(found.total(), 1);
    }

    /// One character at a time is the worst case and the realistic one.
    #[test]
    fn a_secret_arriving_one_character_at_a_time_is_caught() {
        let text = "here is sk-abcdefghij1234 and that is all";
        let pieces: Vec<String> = text.chars().map(|c| c.to_string()).collect();
        let borrowed: Vec<&str> = pieces.iter().map(String::as_str).collect();
        let (out, found) = streamed(&defaults(), 64, &borrowed);
        assert_eq!(out, "here is [redacted:openai-key:1] and that is all");
        assert_eq!(found.total(), 1);
    }

    /// Nothing may be swallowed. Text held back and never let out is a truncated
    /// answer, which is a worse failure than an unfiltered one.
    #[test]
    fn everything_pushed_comes_out_when_there_is_nothing_to_redact() {
        let pieces = ["the quick ", "brown fox ", "jumps over ", "the lazy dog"];
        let (out, found) = streamed(&defaults(), 64, &pieces);
        assert_eq!(out, pieces.concat());
        assert!(found.is_empty());
    }

    #[test]
    fn a_partial_match_that_never_completes_is_let_out_at_the_end() {
        let (out, found) = streamed(&defaults(), 64, &["an address like ada@exa"]);
        assert_eq!(out, "an address like ada@exa");
        assert!(found.is_empty());
    }

    /// Replaced once, not once per piece and not twice.
    #[test]
    fn a_straddling_match_is_replaced_exactly_once() {
        let (out, found) = streamed(&defaults(), 64, &["ada@exa", "mple.com done"]);
        assert_eq!(out, "[redacted:email:1] done");
        assert_eq!(found.total(), 1);
    }

    /// The ordinals span the stream, so the same secret far apart is one placeholder.
    #[test]
    fn one_secret_in_two_distant_pieces_gets_one_placeholder() {
        let filler = "x".repeat(200);
        let (out, found) = streamed(
            &defaults(),
            32,
            &[
                "mail ada@example.com ",
                &filler,
                " and ada@example.com again",
            ],
        );
        assert_eq!(found.counted()[&Kind::new("email").unwrap()], 2);
        assert!(out.starts_with("mail [redacted:email:1] "));
        assert!(out.ends_with(" and [redacted:email:1] again"));
    }

    #[test]
    fn two_different_secrets_are_numbered_across_the_stream() {
        let (out, _) = streamed(
            &defaults(),
            32,
            &["ada@example.com then ", "bob@example.org"],
        );
        assert_eq!(out, "[redacted:email:1] then [redacted:email:2]");
    }

    /// A window of zero is per-piece filtering, named honestly: it holds nothing back,
    /// so it catches what arrives whole and nothing else.
    #[test]
    fn a_window_of_zero_holds_nothing() {
        let detectors = defaults();
        let mut trickle = Trickle::new(&detectors, 0);
        assert_eq!(
            trickle.push("all of sk-abcdefghij1234 at once"),
            "all of [redacted:openai-key:1] at once"
        );
        assert_eq!(trickle.flush(), "", "a window of zero never holds anything");
    }

    /// What per-piece filtering costs, written down rather than left to be discovered.
    /// The three characters that announce a key are a match on their own, so they are
    /// replaced — and the key material behind them, in the next piece, goes out whole.
    /// This is the failure a window exists to prevent, and the test says so plainly.
    #[test]
    fn a_window_of_zero_redacts_the_announcement_and_lets_the_key_through() {
        let detectors = defaults();
        let mut trickle = Trickle::new(&detectors, 0);
        assert_eq!(trickle.push("split sk-"), "split [redacted:openai-key:1]");
        assert_eq!(
            trickle.push("abcdefghij1234"),
            "abcdefghij1234",
            "the key itself went out: this is why the default window is not zero"
        );
    }

    /// A match that never finishes must not hold the carry open for ever.
    #[test]
    fn a_match_that_grows_without_end_is_let_out_and_reported() {
        let detectors = Detectors::of(&[Secret::new(
            Kind::new("blob").unwrap(),
            Match::Prefix("AAA".to_owned()),
        )])
        .unwrap();
        let mut trickle = Trickle::new(&detectors, 64);
        let mut out = String::new();
        out.push_str(&trickle.push("AAA"));
        for _ in 0..40 {
            out.push_str(&trickle.push(&"B".repeat(512)));
        }
        out.push_str(&trickle.flush());
        assert!(trickle.overran(), "an endless match must be reported");
        assert!(
            out.len() > HOLD_MOST,
            "the text was let out rather than held for ever"
        );
    }

    /// The carry is bounded whatever arrives, or a long answer is a memory leak.
    #[test]
    fn the_carry_never_grows_past_the_cap() {
        let detectors = Detectors::of(&[Secret::new(
            Kind::new("blob").unwrap(),
            Match::Prefix("AAA".to_owned()),
        )])
        .unwrap();
        let mut trickle = Trickle::new(&detectors, 64);
        let _ = trickle.push("AAA");
        for _ in 0..40 {
            let _ = trickle.push(&"B".repeat(512));
            assert!(
                trickle.carry.chars().count() <= HOLD_MOST + 512,
                "the carry grew to {}",
                trickle.carry.chars().count()
            );
        }
    }

    /// A release must land between characters, or a long multibyte answer panics.
    #[test]
    fn a_release_never_splits_a_character() {
        let detectors = defaults();
        let mut trickle = Trickle::new(&detectors, 8);
        let mut out = String::new();
        for _ in 0..20 {
            out.push_str(&trickle.push("密钥很长"));
        }
        out.push_str(&trickle.flush());
        assert_eq!(out, "密钥很长".repeat(20));
    }

    #[test]
    fn a_secret_between_multibyte_text_is_caught_across_pieces() {
        let (out, found) = streamed(&defaults(), 32, &["密钥是 sk-abc", "defghij1234，不要外传"]);
        assert_eq!(out, "密钥是 [redacted:openai-key:1]，不要外传");
        assert_eq!(found.total(), 1);
    }

    #[test]
    fn nothing_pushed_means_nothing_flushed() {
        let detectors = defaults();
        let mut trickle = Trickle::new(&detectors, 64);
        assert_eq!(trickle.flush(), "");
        assert!(trickle.found().is_empty());
        assert!(!trickle.overran());
    }

    #[test]
    fn an_empty_piece_changes_nothing() {
        let detectors = defaults();
        let mut trickle = Trickle::new(&detectors, 4);
        assert_eq!(trickle.push(""), "");
        assert_eq!(trickle.push("abcdefgh"), "abcd");
        assert_eq!(trickle.push(""), "");
        assert_eq!(trickle.flush(), "efgh");
    }

    /// Whatever the window and whatever the split, the text that comes out is the text
    /// that went in with its secrets replaced — never more, never less.
    #[test]
    fn the_same_text_comes_out_however_it_was_split() {
        let text = "mail ada@example.com about sk-abcdefghij1234 and card 4242424242424242";
        let whole = streamed(&defaults(), 64, &[text]).0;
        for width in [1, 2, 3, 7, 13, 64] {
            let pieces: Vec<String> = text
                .as_bytes()
                .chunks(width)
                .map(|piece| String::from_utf8_lossy(piece).into_owned())
                .collect();
            let borrowed: Vec<&str> = pieces.iter().map(String::as_str).collect();
            assert_eq!(
                streamed(&defaults(), 64, &borrowed).0,
                whole,
                "split every {width} bytes read differently"
            );
        }
    }
}
