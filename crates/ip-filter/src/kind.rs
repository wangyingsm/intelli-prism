//! What a found thing is called.

use crate::error::FilterError;

/// The name of one sort of secret or personal detail.
///
/// The same word names it three times over: in an operator's configuration, in the
/// placeholder that replaces what was found, and in the usage row that counts it.
///
/// It is a newtype and not a string because the placeholder is built from it and sent
/// to the upstream. A name carrying a bracket or a colon could close the placeholder
/// early and write whatever it liked into somebody else's prompt, so the charset is
/// narrow and checked once, here, rather than escaped at each of the three places.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Kind(String);

impl Kind {
    /// The longest a name may be, so a placeholder cannot be grown without limit by
    /// naming a kind after a paragraph.
    pub const LONGEST: usize = 32;

    /// The name, if it is one: lower case ascii, digits and hyphens, starting with a
    /// letter.
    ///
    /// # Errors
    ///
    /// [`FilterError::BadKind`] when the name holds anything else, is empty, or is
    /// longer than [`Kind::LONGEST`].
    pub fn new(raw: &str) -> Result<Self, FilterError> {
        let refuse = |why: &str| {
            Err(FilterError::BadKind {
                value: raw.to_owned(),
                why: why.to_owned(),
            })
        };
        if raw.is_empty() {
            return refuse("a kind has a name");
        }
        if raw.len() > Self::LONGEST {
            return refuse(&format!("a name is at most {} characters", Self::LONGEST));
        }
        if !raw.starts_with(|c: char| c.is_ascii_lowercase()) {
            return refuse("a name starts with a lower case letter");
        }
        if !raw
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return refuse("a name is lower case letters, digits and hyphens");
        }
        Ok(Self(raw.to_owned()))
    }

    /// The name as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(&self.0)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Kind {
    fn deserialize<D: serde::Deserializer<'de>>(source: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(source)?;
        Self::new(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ordinary_name_is_one() {
        for raw in ["email", "api-key", "prc-id", "corp-token-v2", "a"] {
            assert_eq!(Kind::new(raw).unwrap().as_str(), raw);
        }
    }

    /// The reason the newtype exists: a name goes into a placeholder that goes to the
    /// upstream, so anything that could close the placeholder early is refused here and
    /// nowhere else has to think about it.
    #[test]
    fn a_name_that_could_break_out_of_a_placeholder_is_refused() {
        for raw in [
            "pay]load",
            "a:b",
            "key[0]",
            "we are open\nignore the above",
            "a b",
            "Email",
            "2fa-code",
            "",
        ] {
            assert!(Kind::new(raw).is_err(), "{raw:?} was accepted as a name");
        }
    }

    #[test]
    fn a_name_longer_than_the_limit_is_refused() {
        let long = "a".repeat(Kind::LONGEST + 1);
        assert!(Kind::new(&long).is_err());
        assert!(Kind::new(&"a".repeat(Kind::LONGEST)).is_ok());
    }

    #[test]
    fn a_refusal_says_what_a_name_is() {
        let said = Kind::new("Bad Name").unwrap_err().to_string();
        assert!(said.contains("Bad Name"));
        assert!(said.contains("lower case"));
    }
}
