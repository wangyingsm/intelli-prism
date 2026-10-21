//! What a text holds that must not leave it.

use std::ops::Range;

use regex::Regex;

use crate::check;
use crate::error::FilterError;
use crate::kind::Kind;
use crate::secret::{Check, Match, Secret};

/// What a prefix or suffix match reaches through.
///
/// A credential is one run of these characters, so a prefix of `sk-` takes the key it
/// announced rather than the three characters it is written with. The dot is in because
/// a token with three parts is one secret, not three.
const fn runs_on(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '/' | '=' | '~')
}

/// Punctuation a run must not end on, because prose puts it there and the secret does
/// not: "the key is sk-abc." is a key and a full stop.
const TRAILING: [char; 5] = ['.', ',', ';', ':', '!'];

/// One thing found in a text: what it is, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    kind: Kind,
    at: Range<usize>,
}

impl Finding {
    /// What was found.
    #[must_use]
    pub const fn kind(&self) -> &Kind {
        &self.kind
    }

    /// Where it sits, as byte offsets into the text it was found in.
    #[must_use]
    pub const fn at(&self) -> &Range<usize> {
        &self.at
    }

    /// What was found, cut from the text it was found in.
    ///
    /// # Panics
    ///
    /// If handed a different text from the one this was found in, and the offsets fall
    /// inside a character there.
    #[must_use]
    pub fn text<'a>(&self, from: &'a str) -> &'a str {
        &from[self.at.start..self.at.end]
    }
}

/// One declared secret, compiled into something that can scan.
#[derive(Debug)]
struct Looking {
    kind: Kind,
    how: How,
    check: Option<Check>,
}

/// A way of matching, ready to run.
#[derive(Debug)]
enum How {
    Plain(String),
    Prefix(String),
    Suffix(String),
    Regexp(Regex),
}

/// The secrets in force, compiled once.
///
/// Built from declarations rather than from a list in this file, so an operator's own
/// secrets are found the same way the ones that ship are. [`Secret::defaults`] is a
/// starting point they may take whole, take part of, or ignore.
#[derive(Debug)]
pub struct Detectors {
    looking: Vec<Looking>,
}

impl Detectors {
    /// The secrets this gateway knows about without being told.
    ///
    /// # Errors
    ///
    /// Never in practice: the defaults are written in this crate and a test compiles
    /// every one of them. The result is kept so that taking the defaults and adding to
    /// them is one kind of call rather than two.
    pub fn defaults() -> Result<Self, FilterError> {
        Self::of(&Secret::defaults())
    }

    /// Exactly these, in the order declared.
    ///
    /// # Errors
    ///
    /// [`FilterError::BadPattern`] for an expression that will not compile, and
    /// [`FilterError::EmptyPattern`] for one that would match everywhere.
    pub fn of(secrets: &[Secret]) -> Result<Self, FilterError> {
        let looking = secrets
            .iter()
            .map(|secret| {
                let how = secret.matching.how();
                let kind = secret.kind.as_str().to_owned();
                let written = secret.matching.written();
                if written.is_empty() {
                    return Err(FilterError::EmptyPattern { kind, how });
                }
                let compiled = match &secret.matching {
                    Match::Plain(text) => How::Plain(text.clone()),
                    Match::Prefix(text) => How::Prefix(text.clone()),
                    Match::Suffix(text) => How::Suffix(text.clone()),
                    Match::Regexp(pattern) => {
                        How::Regexp(Regex::new(pattern).map_err(|error| {
                            FilterError::BadPattern {
                                kind,
                                how,
                                detail: error.to_string(),
                            }
                        })?)
                    }
                };
                Ok(Looking {
                    kind: secret.kind.clone(),
                    how: compiled,
                    check: secret.check,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { looking })
    }

    /// The kinds this is looking for, in the order declared.
    #[must_use]
    pub fn kinds(&self) -> Vec<&Kind> {
        self.looking.iter().map(|one| &one.kind).collect()
    }

    /// Everything in this text, ordered by where it starts and never overlapping.
    ///
    /// Two kinds can claim the same characters — eighteen digits are a resident id and,
    /// if the sum falls right, a card number. The longer match wins, and an equal one
    /// goes to whichever was declared first, so the answer depends on the operator's
    /// own order rather than on which detector happened to run.
    #[must_use]
    pub fn find(&self, text: &str) -> Vec<Finding> {
        let mut found: Vec<(usize, Finding)> = Vec::new();
        for (declared, looking) in self.looking.iter().enumerate() {
            for at in looking.how.spans(text) {
                if confirmed(looking.check, &text[at.clone()]) {
                    found.push((
                        declared,
                        Finding {
                            kind: looking.kind.clone(),
                            at,
                        },
                    ));
                }
            }
        }
        found.sort_by(|(left_at, left), (right_at, right)| {
            left.at
                .start
                .cmp(&right.at.start)
                .then((right.at.end - right.at.start).cmp(&(left.at.end - left.at.start)))
                .then(left_at.cmp(right_at))
        });
        let mut kept: Vec<Finding> = Vec::with_capacity(found.len());
        for (_, finding) in found {
            if kept
                .last()
                .is_none_or(|last| last.at.end <= finding.at.start)
            {
                kept.push(finding);
            }
        }
        kept
    }
}

impl How {
    /// Every span of this text that matches.
    fn spans(&self, text: &str) -> Vec<Range<usize>> {
        match self {
            Self::Plain(want) => text
                .match_indices(want.as_str())
                .map(|(at, hit)| at..at + hit.len())
                .collect(),
            Self::Prefix(want) => text
                .match_indices(want.as_str())
                .map(|(at, hit)| at..run_end(text, at + hit.len()))
                .collect(),
            Self::Suffix(want) => text
                .match_indices(want.as_str())
                .map(|(at, hit)| run_start(text, at)..at + hit.len())
                .collect(),
            Self::Regexp(pattern) => pattern.find_iter(text).map(|hit| hit.range()).collect(),
        }
    }
}

/// Where the run a prefix started carries on to, with any prose punctuation left behind.
fn run_end(text: &str, from: usize) -> usize {
    let mut end = from
        + text[from..]
            .find(|c: char| !runs_on(c))
            .unwrap_or(text.len() - from);
    while end > from && text[..end].ends_with(TRAILING) {
        end -= 1;
    }
    end
}

/// Where the run a suffix ends began.
fn run_start(text: &str, before: usize) -> usize {
    text[..before].rfind(|c: char| !runs_on(c)).map_or(0, |at| {
        at + text[at..].chars().next().map_or(1, char::len_utf8)
    })
}

/// Whether a candidate is confirmed by arithmetic of its own. One declared without a
/// check is taken at its shape.
fn confirmed(check: Option<Check>, text: &str) -> bool {
    match check {
        None => true,
        Some(Check::Luhn) => check::luhn(text),
        Some(Check::PrcId) => check::prc_id(text),
        Some(Check::Iban) => check::iban(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Detectors {
        Detectors::defaults().unwrap()
    }

    fn named(raw: &str) -> Kind {
        Kind::new(raw).unwrap()
    }

    fn found(text: &str) -> Vec<&str> {
        all()
            .find(text)
            .into_iter()
            .map(|one| one.text(text))
            .collect()
    }

    fn kinds_in(text: &str) -> Vec<String> {
        all()
            .find(text)
            .into_iter()
            .map(|one| one.kind().to_string())
            .collect()
    }

    /// Every default compiles. They are written in this crate, so the only day this can
    /// fail is the day one is edited, which is exactly the day to hear about it.
    #[test]
    fn every_default_compiles() {
        assert!(Detectors::defaults().is_ok());
        for secret in Secret::defaults() {
            assert!(
                Detectors::of(std::slice::from_ref(&secret)).is_ok(),
                "{} does not compile",
                secret.kind
            );
        }
    }

    #[test]
    fn a_prefix_takes_the_whole_of_what_it_announced() {
        assert_eq!(
            found("my key is sk-abcdefghij1234 and it leaked"),
            vec!["sk-abcdefghij1234"]
        );
        assert_eq!(found("AKIAIOSFODNN7EXAMPLE"), vec!["AKIAIOSFODNN7EXAMPLE"]);
    }

    /// A slack token is matched by the shape slack issues, not by three letters.
    ///
    /// `xoxo` ends an ordinary English sentence, and a detector that redacted it would
    /// rewrite a caller's prose. The shape and the length floor are both load-bearing:
    /// the shape refuses the word, the floor refuses `xoxb-hi`.
    #[test]
    fn a_slack_token_is_matched_and_a_fond_farewell_is_not() {
        for token in [
            "xoxb-123456789012-123456789012-abcdefghijklmnopqrst",
            "xoxp-0000000000-0000000000-abcdefghij",
            "xoxa-2-abcdefghijklmno",
            // The app-level token, which shares nothing with the others but the vendor.
            "xapp-1-A01234ABCDE-1234567890123-abcdef0123456789",
        ] {
            assert_eq!(found(token), vec![token], "a real token went through");
        }

        for prose in [
            "xoxo",
            "xox",
            "hugs and xoxo",
            "xoxb-hi",
            "xoxq-abcdefghijkl",
            "xapp-hi",
            "xapps-abcdefghijkl",
        ] {
            assert!(
                found(prose).is_empty(),
                "{prose} was taken for a slack token"
            );
        }
    }

    /// Prose puts a full stop after a key and the key does not end with one.
    #[test]
    fn a_prefix_match_leaves_the_punctuation_behind() {
        assert_eq!(found("the key is sk-abcdef1234."), vec!["sk-abcdef1234"]);
        assert_eq!(found("use sk-abcdef1234, then stop"), vec!["sk-abcdef1234"]);
    }

    /// A token with three dotted parts is one secret, not three.
    #[test]
    fn a_prefix_match_runs_through_dots_inside_a_token() {
        let detectors =
            Detectors::of(&[Secret::new(named("jwt"), Match::Prefix("eyJ".to_owned()))]).unwrap();
        let text = "Authorization: Bearer eyJhbGci.eyJzdWIi.c2lnbmF0dXJl here";
        let at = detectors.find(text);
        assert_eq!(at[0].text(text), "eyJhbGci.eyJzdWIi.c2lnbmF0dXJl");
    }

    #[test]
    fn a_prefix_at_the_very_end_of_a_text_is_still_found() {
        assert_eq!(found("key sk-abcdef1234"), vec!["sk-abcdef1234"]);
    }

    #[test]
    fn a_suffix_reaches_back_to_where_its_run_began() {
        let detectors = Detectors::of(&[Secret::new(
            named("corp-mail"),
            Match::Suffix("@corp.example".to_owned()),
        )])
        .unwrap();
        let text = "write to ada@corp.example and bob@elsewhere.com";
        let at = detectors.find(text);
        assert_eq!(at.len(), 1);
        assert_eq!(at[0].text(text), "ada@corp.example");
    }

    #[test]
    fn a_suffix_at_the_very_start_of_a_text_is_still_found() {
        let detectors =
            Detectors::of(&[Secret::new(named("tail"), Match::Suffix("-end".to_owned()))]).unwrap();
        let text = "abc-end rest";
        let at = detectors.find(text);
        assert_eq!(at[0].text(text), "abc-end");
    }

    #[test]
    fn a_plain_match_takes_exactly_what_it_was_given() {
        let detectors = Detectors::of(&[Secret::new(
            named("codename"),
            Match::Plain("Project Vellum".to_owned()),
        )])
        .unwrap();
        let text = "do not mention Project Vellum to anyone";
        let at = detectors.find(text);
        assert_eq!(at[0].text(text), "Project Vellum");
    }

    #[test]
    fn a_regexp_matches_what_it_says() {
        let detectors = Detectors::of(&[Secret::new(
            named("ticket"),
            Match::Regexp(r"SEC-[0-9]{4,}".to_owned()),
        )])
        .unwrap();
        assert_eq!(detectors.find("see SEC-12345 and SEC-9").len(), 1);
    }

    #[test]
    fn a_regexp_that_will_not_compile_is_refused_by_name() {
        let refused = Detectors::of(&[Secret::new(
            named("broken"),
            Match::Regexp("([unclosed".to_owned()),
        )])
        .unwrap_err();
        let said = refused.to_string();
        assert!(said.contains("broken"), "{said}");
        assert!(said.contains("regexp"), "{said}");
    }

    #[test]
    fn an_empty_pattern_is_refused_rather_than_matching_everywhere() {
        for matching in [
            Match::Plain(String::new()),
            Match::Prefix(String::new()),
            Match::Suffix(String::new()),
            Match::Regexp(String::new()),
        ] {
            let how = matching.how();
            let refused = Detectors::of(&[Secret::new(named("empty"), matching)]).unwrap_err();
            assert!(
                matches!(refused, FilterError::EmptyPattern { .. }),
                "an empty {how} was accepted"
            );
        }
    }

    #[test]
    fn a_check_is_what_separates_a_card_from_a_long_number() {
        assert_eq!(
            kinds_in("pay with 4242 4242 4242 4242"),
            vec!["credit-card"]
        );
        assert!(kinds_in("the id is 9007199254740991").is_empty());
    }

    #[test]
    fn a_shape_without_its_check_would_fire_on_anything_of_that_length() {
        let unchecked = Detectors::of(&[Secret::new(
            named("card"),
            Match::Regexp(r"\b[0-9](?:[ -]?[0-9]){12,18}\b".to_owned()),
        )])
        .unwrap();
        assert_eq!(
            unchecked.find("the id is 9007199254740991").len(),
            1,
            "this is why the default names its check"
        );
    }

    #[test]
    fn a_private_key_is_taken_whole() {
        let text = "here:\n-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\nx+y/z\n-----END RSA PRIVATE KEY-----\nthanks";
        let taken = found(text);
        assert_eq!(taken.len(), 1);
        assert!(taken[0].starts_with("-----BEGIN RSA PRIVATE KEY-----"));
        assert!(taken[0].ends_with("-----END RSA PRIVATE KEY-----"));
        assert!(taken[0].contains("MIIEowIBAAKCAQEA"));
    }

    #[test]
    fn a_resident_id_is_found_and_preferred_over_a_card() {
        assert_eq!(kinds_in("身份证 11010519491231002X"), vec!["prc-id"]);
    }

    /// Nothing in ordinary prose or an ordinary code sample, which is the test that says
    /// whether an operator can leave this switched on.
    #[test]
    fn ordinary_text_holds_nothing() {
        for text in [
            "please refactor the retry loop in src/main.rs, it has been flaky since Tuesday",
            "SELECT id, created_at FROM orders WHERE total > 1000 LIMIT 20",
            "我们有 40 万行 Java 分布在 12 个服务里，想逐步迁到 Go",
            "the build id is 20260101120000000 and the run took 1699999999 ms",
            "version 1.13.1, commit 36decc22220d0018b3160ba59d6aae73367bb436",
        ] {
            assert!(kinds_in(text).is_empty(), "fired on: {text}");
        }
    }

    #[test]
    fn findings_come_back_in_the_order_they_appear() {
        let text = "ada@example.com then bob@example.org then 4242424242424242";
        let starts: Vec<_> = all().find(text).iter().map(|one| one.at().start).collect();
        let mut sorted = starts.clone();
        sorted.sort_unstable();
        assert_eq!(starts, sorted);
    }

    /// Two kinds claiming one span must not both come back, or a replacement built from
    /// the findings would corrupt the text it rewrites.
    #[test]
    fn findings_never_overlap() {
        let text = "11010519491231002X 4242424242424242 sk-abcdefghij1234 ada@example.com";
        let found = all().find(text);
        for pair in found.windows(2) {
            assert!(
                pair[0].at().end <= pair[1].at().start,
                "{:?} overlaps {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    /// The operator's own order settles a tie, which is the only way they can steer it.
    #[test]
    fn the_first_declared_wins_a_span_two_kinds_claim() {
        let text = "ada@example.com";
        let mine = Secret::new(named("mine"), Match::Suffix("@example.com".to_owned()));
        let theirs = Secret::default_named("email").unwrap();
        let first = Detectors::of(&[mine.clone(), theirs.clone()]).unwrap();
        let second = Detectors::of(&[theirs, mine]).unwrap();
        assert_eq!(first.find(text)[0].kind().as_str(), "mine");
        assert_eq!(second.find(text)[0].kind().as_str(), "email");
    }

    #[test]
    fn asking_for_nothing_finds_nothing() {
        let none = Detectors::of(&[]).unwrap();
        assert!(none.kinds().is_empty());
        assert!(none.find("sk-abcdefghij1234").is_empty());
    }

    /// The same text twice runs to the same answer, which is what the turn anchor leans
    /// on once this drives a replacement.
    #[test]
    fn the_same_text_is_found_the_same_way_twice() {
        let text = "ada@example.com, 4242424242424242, GB82WEST12345698765432";
        assert_eq!(all().find(text), all().find(text));
    }

    /// A prefix or suffix walking a run must land on a character boundary, or indexing
    /// the text with what it returns panics on the next request carrying Chinese.
    #[test]
    fn a_run_through_multibyte_text_lands_on_character_boundaries() {
        let detectors = Detectors::of(&[
            Secret::new(named("pre"), Match::Prefix("sk-".to_owned())),
            Secret::new(named("suf"), Match::Suffix("-end".to_owned())),
        ])
        .unwrap();
        for text in [
            "密钥是 sk-abcdef1234，不要外传",
            "标识 abc-end 结束",
            "sk-abc密钥",
            "密钥sk-abc",
        ] {
            for finding in detectors.find(text) {
                assert!(
                    text.is_char_boundary(finding.at().start)
                        && text.is_char_boundary(finding.at().end),
                    "{:?} is not a boundary in {text}",
                    finding.at()
                );
                let _ = finding.text(text);
            }
        }
    }
}
