//! The two spelling rules every name in this crate is checked against.
//!
//! Written once here rather than in each newtype, because this crate declares seven of them and
//! the rules are two. Each type keeps its own `KIND`, so a refusal still names the thing that
//! was wrong rather than the rule that caught it.

use crate::error::RetrieveError;

/// A key: at most `max` bytes of ascii alphanumerics, `-`, `_` and `.`.
///
/// The rule `ip-core` applies to a tenant or a user id, `ip-agents` to a template and `ip-mcp`
/// to a server, so an operator meets one spelling rule across the system. It is written again
/// rather than shared because this crate depends on no other: sharing it would make another
/// crate's error type part of how this one fails.
pub(crate) fn key(raw: &str, kind: &'static str, max: usize) -> Result<(), RetrieveError> {
    if raw.is_empty() {
        return Err(RetrieveError::Empty { kind });
    }
    if raw.len() > max {
        return Err(RetrieveError::TooLong {
            kind,
            len: raw.len(),
            max,
        });
    }
    match raw
        .chars()
        .find(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')))
    {
        Some(ch) => Err(RetrieveError::IllegalChar { kind, ch }),
        None => Ok(()),
    }
}

/// A line of prose: at most `max` bytes, no control character, and no padding at either edge.
///
/// For what a person writes and a person or a model reads — a retriever's name, a note about a
/// field. Refused rather than trimmed, for the reason a server's name is: what comes back is
/// what was written.
pub(crate) fn prose(raw: &str, kind: &'static str, max: usize) -> Result<(), RetrieveError> {
    if raw.is_empty() {
        return Err(RetrieveError::Empty { kind });
    }
    if raw.len() > max {
        return Err(RetrieveError::TooLong {
            kind,
            len: raw.len(),
            max,
        });
    }
    if raw.trim() != raw {
        return Err(RetrieveError::Untrimmed { kind });
    }
    match raw.chars().find(|ch| ch.is_control()) {
        Some(ch) => Err(RetrieveError::IllegalChar { kind, ch }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_takes_what_every_other_id_in_this_system_takes() {
        assert!(key("docs.v2_1-a", "k", 64).is_ok());
        assert_eq!(
            key("", "k", 64),
            Err(RetrieveError::Empty { kind: "k" }),
            "nothing is not a key"
        );
        assert_eq!(
            key("a b", "k", 64),
            Err(RetrieveError::IllegalChar { kind: "k", ch: ' ' }),
            "a space is not in the one spelling rule"
        );
        assert!(matches!(
            key("aaa", "k", 2),
            Err(RetrieveError::TooLong { len: 3, max: 2, .. })
        ));
    }

    #[test]
    fn prose_may_say_anything_but_pad_nothing() {
        assert!(prose("Which quarter the figure is for", "p", 160).is_ok());
        assert!(
            prose("季度", "p", 160).is_ok(),
            "any language a person writes"
        );
        assert_eq!(
            prose(" padded", "p", 160),
            Err(RetrieveError::Untrimmed { kind: "p" }),
            "padding is refused rather than trimmed away"
        );
        assert_eq!(
            prose("two\nlines", "p", 160),
            Err(RetrieveError::IllegalChar {
                kind: "p",
                ch: '\n'
            }),
            "a note is one line, because it is appended to a prompt"
        );
    }
}
