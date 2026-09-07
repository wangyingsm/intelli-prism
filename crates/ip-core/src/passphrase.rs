use std::fmt;

use crate::error::CoreError;

const HASH_MAX_BYTES: usize = 256;

/// A stored passphrase verifier in phc string form, never the passphrase itself.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct PassphraseHash(Box<str>);

impl PassphraseHash {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "passphrase hash";

    /// Validates and wraps a phc string produced by the hasher.
    pub fn new(raw: &str) -> Result<Self, CoreError> {
        validate(raw)?;
        Ok(Self(Box::from(raw)))
    }

    /// The phc string, as the hasher wants it back.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn validate(raw: &str) -> Result<(), CoreError> {
    let kind = PassphraseHash::KIND;
    if raw.is_empty() {
        return Err(CoreError::Empty { kind });
    }
    if raw.len() > HASH_MAX_BYTES {
        return Err(CoreError::TooLong {
            kind,
            len: raw.len(),
            max: HASH_MAX_BYTES,
        });
    }
    if !raw.starts_with('$') {
        return Err(CoreError::IllegalChar {
            kind,
            ch: raw.chars().next().unwrap_or_default(),
        });
    }
    match raw.chars().find(|ch| !ch.is_ascii_graphic()) {
        Some(ch) => Err(CoreError::IllegalChar { kind, ch }),
        None => Ok(()),
    }
}

impl TryFrom<String> for PassphraseHash {
    type Error = CoreError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        validate(&raw)?;
        Ok(Self(raw.into_boxed_str()))
    }
}

/// Redacted: a verifier is offline attackable, so it never reaches a log.
impl fmt::Debug for PassphraseHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PassphraseHash(redacted)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHC: &str =
        "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHR2YWx1ZQ$aGFzaGhhc2hoYXNoaGFzaGhhc2g";

    #[test]
    fn accepts_a_phc_string() {
        assert_eq!(PassphraseHash::new(PHC).unwrap().as_str(), PHC);
    }

    #[test]
    fn rejects_an_empty_hash() {
        assert_eq!(
            PassphraseHash::new(""),
            Err(CoreError::Empty {
                kind: "passphrase hash"
            })
        );
    }

    #[test]
    fn rejects_a_string_that_is_not_phc_form() {
        assert_eq!(
            PassphraseHash::new("argon2id$v=19"),
            Err(CoreError::IllegalChar {
                kind: "passphrase hash",
                ch: 'a',
            })
        );
    }

    #[test]
    fn rejects_an_over_long_hash() {
        let raw = format!("${}", "a".repeat(HASH_MAX_BYTES));
        assert_eq!(
            PassphraseHash::new(&raw),
            Err(CoreError::TooLong {
                kind: "passphrase hash",
                len: HASH_MAX_BYTES + 1,
                max: HASH_MAX_BYTES,
            })
        );
    }

    #[test]
    fn is_redacted_in_debug_output() {
        let hash = PassphraseHash::new(PHC).unwrap();
        assert_eq!(format!("{hash:?}"), "PassphraseHash(redacted)");
    }

    #[test]
    fn owned_conversion_still_validates() {
        assert!(PassphraseHash::try_from(PHC.to_string()).is_ok());
        assert!(PassphraseHash::try_from("plaintext".to_string()).is_err());
    }
}
