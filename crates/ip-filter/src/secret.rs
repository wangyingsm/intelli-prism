//! What an operator declares they do not want leaving.
//!
//! A declaration is text: a name, a way of matching, and optionally the arithmetic that
//! confirms a match. It is what a configuration file holds and what travels; compiling
//! it into something that can scan is [`crate::Detectors`]'s job, and that is the step
//! that can fail.

use crate::error::FilterError;
use crate::kind::Kind;

/// How a secret is recognised.
///
/// Four ways, because an operator knows their own secrets in different ways: some by
/// the exact word, some by what they start with, some by what they end with, and the
/// rest only by a pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "snake_case")
)]
pub enum Match {
    /// This text exactly, wherever it appears.
    Plain(String),
    /// Anything beginning with this. The match runs on to the end of what it is part
    /// of, so a prefix of `sk-` takes the whole key rather than the three characters
    /// that announced it.
    Prefix(String),
    /// Anything ending with this, reaching back to where it began.
    Suffix(String),
    /// Whatever this expression matches.
    Regexp(String),
}

impl Match {
    /// The text this was declared with, whichever way it matches.
    #[must_use]
    pub fn written(&self) -> &str {
        match self {
            Self::Plain(text) | Self::Prefix(text) | Self::Suffix(text) | Self::Regexp(text) => {
                text
            }
        }
    }

    /// The word this way of matching is written as.
    #[must_use]
    pub const fn how(&self) -> &'static str {
        match self {
            Self::Plain(_) => "plain",
            Self::Prefix(_) => "prefix",
            Self::Suffix(_) => "suffix",
            Self::Regexp(_) => "regexp",
        }
    }
}

/// Arithmetic that says a match really is what its shape suggested.
///
/// A shape alone cannot tell a card number from sixteen digits of telemetry. Where the
/// issuer published a check, naming it here is what lets the kind be switched on
/// without first measuring what it costs in false positives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "snake_case")
)]
pub enum Check {
    /// The Luhn checksum, as every payment card carries.
    Luhn,
    /// A mainland China resident identity number's check character, ISO 7064 mod 11-2.
    PrcId,
    /// An international bank account number's mod-97.
    Iban,
}

/// How far a match must run before it is believed.
///
/// A short prefix cannot tell a key from a word that opens the same way: `sk-SK` is a
/// locale and `sk-abcdefghij1234567890` is a key, and nothing but length separates
/// them. Where a provider issues long tokens behind a short prefix, saying so here is
/// what keeps the detector off ordinary text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "usize")
)]
pub struct Least(usize);

impl Least {
    /// A floor of this many characters, refusing nought, which is no floor at all.
    pub fn new(characters: usize) -> Result<Self, FilterError> {
        if characters == 0 {
            return Err(FilterError::EmptyLeast);
        }
        Ok(Self(characters))
    }

    /// How many characters it asks for.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }
}

impl TryFrom<usize> for Least {
    type Error = FilterError;

    fn try_from(characters: usize) -> Result<Self, Self::Error> {
        Self::new(characters)
    }
}

/// One declared secret: what it is called, how it is recognised, and what confirms it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(deny_unknown_fields)
)]
pub struct Secret {
    /// What a match of this is called, in configuration, in the placeholder and in the
    /// usage row.
    pub kind: Kind,
    /// How it is recognised.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub matching: Match,
    /// The arithmetic that confirms a match, where the issuer published one.
    #[cfg_attr(feature = "serde", serde(default))]
    pub check: Option<Check>,
    /// How far a match must run before it is believed, where a shape alone is too
    /// short to tell a secret from a word. Unset means any length the shape matched.
    #[cfg_attr(feature = "serde", serde(default))]
    pub least: Option<Least>,
}

impl Secret {
    /// A secret recognised this way, confirmed by nothing but its shape.
    #[must_use]
    pub fn new(kind: Kind, matching: Match) -> Self {
        Self {
            kind,
            matching,
            check: None,
            least: None,
        }
    }

    /// The same, confirmed by arithmetic of its own.
    #[must_use]
    pub fn confirmed_by(mut self, check: Check) -> Self {
        self.check = Some(check);
        self
    }

    /// The same, believed only once it runs this far.
    #[must_use]
    pub fn at_least(mut self, least: Least) -> Self {
        self.least = Some(least);
        self
    }

    /// The secrets this gateway knows about without being told: the shapes providers
    /// issue, the numbers whose issuers published a check, and the two that nearly
    /// every deployment wants.
    ///
    /// They are a starting point and not a policy. An operator takes the ones that suit
    /// them, names their own beside them, and the order here is only the order overlaps
    /// are settled in.
    #[must_use]
    pub fn defaults() -> Vec<Self> {
        let named = |raw: &str| {
            Kind::new(raw).unwrap_or_else(|_| unreachable!("{raw} is a name written in this file"))
        };
        let floor = |characters: usize| {
            Least::new(characters)
                .unwrap_or_else(|_| unreachable!("{characters} is a floor written in this file"))
        };
        vec![
            // The whole block, not the opening line: a key is useless to redact by its
            // header and catastrophic to let through by its body.
            Self::new(
                named("private-key"),
                Match::Regexp(
                    r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----"
                        .to_owned(),
                ),
            ),
            // The shapes providers actually issue, each as a prefix because that is how
            // an operator thinks of them and how the next provider will be added.
            // Each with the floor its provider's tokens clear by a wide margin, because
            // a short prefix also opens ordinary text: `sk-SK` is a locale. The floors
            // are well under the real lengths — an openai key runs past fifty — so they
            // separate prose from secrets without guessing at a format that may change.
            Self::new(named("openai-key"), Match::Prefix("sk-".to_owned())).at_least(floor(13)),
            Self::new(named("github-token"), Match::Prefix("ghp_".to_owned())).at_least(floor(20)),
            Self::new(named("aws-key"), Match::Prefix("AKIA".to_owned())).at_least(floor(20)),
            Self::new(named("google-key"), Match::Prefix("AIza".to_owned())).at_least(floor(30)),
            // Not a bare `xox` prefix, which the other four can afford and this one
            // cannot: `xoxo` ends an ordinary English sentence. Every token Slack
            // issues carries a dash after its kind, and is far longer than the ten
            // characters required here. `xapp-` is the app-level one, which shares
            // nothing with the others but the vendor.
            Self::new(
                named("slack-token"),
                Match::Regexp(r"\b(?:xox[abeprs]|xapp)-[A-Za-z0-9-]{10,}".to_owned()),
            ),
            // Written without spaces. A spaced account is common in prose and a shape
            // loose enough to reach it also reaches half a paragraph; mod-97 would have
            // carried that, the shape will not.
            Self::new(
                named("iban"),
                Match::Regexp(r"\b[A-Z]{2}[0-9]{2}[A-Z0-9]{11,30}\b".to_owned()),
            )
            .confirmed_by(Check::Iban),
            Self::new(
                named("prc-id"),
                Match::Regexp(r"\b[0-9]{17}[0-9Xx]\b".to_owned()),
            )
            .confirmed_by(Check::PrcId),
            Self::new(
                named("credit-card"),
                Match::Regexp(r"\b[0-9](?:[ -]?[0-9]){12,18}\b".to_owned()),
            )
            .confirmed_by(Check::Luhn),
            Self::new(
                named("email"),
                Match::Regexp(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,24}\b".to_owned()),
            ),
        ]
    }

    /// The default secret of this name, for a configuration that takes some of them and
    /// leaves the rest.
    #[must_use]
    pub fn default_named(name: &str) -> Option<Self> {
        Self::defaults()
            .into_iter()
            .find(|secret| secret.kind.as_str() == name)
    }

    /// Every name [`Secret::defaults`] carries, for an error message that has to say
    /// what an operator could have written instead.
    #[must_use]
    pub fn default_names() -> Vec<String> {
        Self::defaults()
            .into_iter()
            .map(|secret| secret.kind.as_str().to_owned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_two_defaults_share_a_name() {
        let mut names = Secret::default_names();
        let was = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), was, "two default secrets answer to one name");
    }

    #[test]
    fn a_default_can_be_taken_by_name() {
        assert_eq!(
            Secret::default_named("credit-card").unwrap().check,
            Some(Check::Luhn)
        );
        assert!(Secret::default_named("nothing-like-this").is_none());
    }

    /// Every number whose issuer published a check must name it, or the default set
    /// ships a kind that fires on any string of the right length.
    #[test]
    fn every_default_that_could_be_confirmed_is() {
        for name in ["credit-card", "prc-id", "iban"] {
            let secret = Secret::default_named(name).unwrap();
            assert!(secret.check.is_some(), "{name} ships without its check");
        }
    }

    /// The shape an operator actually writes. This is the reason the set is configured
    /// rather than built in, so the configuration itself is what is asserted — not the
    /// Rust that happens to sit behind it.
    #[cfg(feature = "serde")]
    #[test]
    fn an_operator_declares_their_own_secrets_in_four_ways() {
        #[derive(serde::Deserialize)]
        struct Filter {
            secret: Vec<Secret>,
        }
        let written = r#"
            [[secret]]
            kind = "corp-token"
            prefix = "MYCORP-"

            [[secret]]
            kind = "staff-mail"
            suffix = "@corp.example"

            [[secret]]
            kind = "codename"
            plain = "Project Vellum"

            [[secret]]
            kind = "ticket"
            regexp = 'SEC-[0-9]{4,}'

            [[secret]]
            kind = "account"
            regexp = '\b[0-9]{13,19}\b'
            check = "luhn"
        "#;
        let read: Filter = toml::from_str(written).expect("the declarations read");
        assert_eq!(read.secret.len(), 5);
        assert_eq!(read.secret[0].kind.as_str(), "corp-token");
        assert_eq!(read.secret[0].matching, Match::Prefix("MYCORP-".to_owned()));
        assert_eq!(
            read.secret[1].matching,
            Match::Suffix("@corp.example".to_owned())
        );
        assert_eq!(
            read.secret[2].matching,
            Match::Plain("Project Vellum".to_owned())
        );
        assert_eq!(read.secret[3].matching.how(), "regexp");
        assert_eq!(read.secret[3].check, None);
        assert_eq!(read.secret[4].check, Some(Check::Luhn));
    }

    /// A name that could close the placeholder early is refused while the configuration
    /// is read, which is the only place it can be caught before it reaches an upstream.
    #[cfg(feature = "serde")]
    #[test]
    fn a_declaration_naming_a_bad_kind_does_not_read() {
        let written = r#"
            kind = "pay]load:x"
            plain = "x"
        "#;
        let refused = toml::from_str::<Secret>(written).unwrap_err().to_string();
        assert!(refused.contains("not a name for a kind"), "{refused}");
    }

    #[cfg(feature = "serde")]
    #[test]
    fn a_declaration_with_no_way_of_matching_does_not_read() {
        assert!(toml::from_str::<Secret>(r#"kind = "lonely""#).is_err());
    }

    /// A field name nobody defined must not be passed over. `check_with = "luhn"` quietly ignored is a
    /// kind that fires on every number of the right length, and the operator is told
    /// nothing. Worth asserting rather than trusting, because `deny_unknown_fields`
    /// and `flatten` have a history of not working together.
    #[cfg(feature = "serde")]
    #[test]
    fn a_field_nobody_defined_is_refused_rather_than_passed_over() {
        let written = r#"
            kind = "account"
            regexp = '[0-9]+'
            check_with = "luhn"
        "#;
        let read = toml::from_str::<Secret>(written);
        assert!(
            read.is_err(),
            "a typo was accepted, and the check it meant to name is silently absent"
        );
    }

    #[test]
    fn how_a_secret_matches_is_said_in_a_word() {
        assert_eq!(Match::Plain("x".to_owned()).how(), "plain");
        assert_eq!(Match::Prefix("x".to_owned()).how(), "prefix");
        assert_eq!(Match::Suffix("x".to_owned()).how(), "suffix");
        assert_eq!(Match::Regexp("x".to_owned()).how(), "regexp");
        assert_eq!(Match::Prefix("sk-".to_owned()).written(), "sk-");
    }
}
