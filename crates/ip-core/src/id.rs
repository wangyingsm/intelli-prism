use std::fmt;
use std::str::FromStr;

use crate::error::CoreError;

const ID_MAX_BYTES: usize = 64;
const NONCE_MIN_BYTES: usize = 8;
const NONCE_MAX_BYTES: usize = 128;

fn validate_id(kind: &'static str, raw: &str) -> Result<(), CoreError> {
    if raw.is_empty() {
        return Err(CoreError::Empty { kind });
    }
    if raw.len() > ID_MAX_BYTES {
        return Err(CoreError::TooLong {
            kind,
            len: raw.len(),
            max: ID_MAX_BYTES,
        });
    }
    match raw
        .chars()
        .find(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')))
    {
        Some(ch) => Err(CoreError::IllegalChar { kind, ch }),
        None => Ok(()),
    }
}

macro_rules! declare_id {
    ($name:ident, $kind:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
        #[serde(try_from = "String")]
        pub struct $name(Box<str>);

        impl $name {
            pub const KIND: &'static str = $kind;

            pub fn new(raw: &str) -> Result<Self, CoreError> {
                validate_id(Self::KIND, raw)?;
                Ok(Self(Box::from(raw)))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn as_bytes(&self) -> &[u8] {
                self.0.as_bytes()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = CoreError;

            fn from_str(raw: &str) -> Result<Self, Self::Err> {
                Self::new(raw)
            }
        }

        impl TryFrom<String> for $name {
            type Error = CoreError;

            fn try_from(raw: String) -> Result<Self, Self::Error> {
                validate_id(Self::KIND, &raw)?;
                Ok(Self(raw.into_boxed_str()))
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

declare_id!(TenantId, "tenant id");
declare_id!(UserId, "user id");
declare_id!(ApiId, "api id");

#[derive(Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct Nonce(Box<str>);

fn validate_nonce(raw: &str) -> Result<(), CoreError> {
    let kind = Nonce::KIND;
    if raw.len() < NONCE_MIN_BYTES {
        return Err(CoreError::TooShort {
            kind,
            len: raw.len(),
            min: NONCE_MIN_BYTES,
        });
    }
    if raw.len() > NONCE_MAX_BYTES {
        return Err(CoreError::TooLong {
            kind,
            len: raw.len(),
            max: NONCE_MAX_BYTES,
        });
    }
    match raw.chars().find(|ch| !ch.is_ascii_graphic()) {
        Some(ch) => Err(CoreError::IllegalChar { kind, ch }),
        None => Ok(()),
    }
}

impl Nonce {
    pub const KIND: &'static str = "nonce";

    pub fn new(raw: &str) -> Result<Self, CoreError> {
        validate_nonce(raw)?;
        Ok(Self(Box::from(raw)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl fmt::Debug for Nonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Nonce").field(&self.0).finish()
    }
}

impl fmt::Display for Nonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Nonce {
    type Err = CoreError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::new(raw)
    }
}

impl TryFrom<String> for Nonce {
    type Error = CoreError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        validate_nonce(&raw)?;
        Ok(Self(raw.into_boxed_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_legal_identifier_characters() {
        let id = TenantId::new("acme-corp_1.eu").unwrap();
        assert_eq!(id.as_str(), "acme-corp_1.eu");
    }

    #[test]
    fn rejects_empty_identifier() {
        assert_eq!(UserId::new(""), Err(CoreError::Empty { kind: "user id" }));
    }

    #[test]
    fn rejects_over_long_identifier() {
        let raw = "a".repeat(ID_MAX_BYTES + 1);
        assert_eq!(
            ApiId::new(&raw),
            Err(CoreError::TooLong {
                kind: "api id",
                len: ID_MAX_BYTES + 1,
                max: ID_MAX_BYTES,
            })
        );
    }

    #[test]
    fn rejects_identifier_with_separator() {
        assert_eq!(
            TenantId::new("acme/corp"),
            Err(CoreError::IllegalChar {
                kind: "tenant id",
                ch: '/',
            })
        );
    }

    #[test]
    fn rejects_short_nonce() {
        assert_eq!(
            Nonce::new("abc"),
            Err(CoreError::TooShort {
                kind: "nonce",
                len: 3,
                min: NONCE_MIN_BYTES,
            })
        );
    }

    #[test]
    fn rejects_nonce_with_whitespace() {
        assert_eq!(
            Nonce::new("nonce with space"),
            Err(CoreError::IllegalChar {
                kind: "nonce",
                ch: ' ',
            })
        );
    }

    #[test]
    fn accepts_base64_nonce() {
        assert!(Nonce::new("Zm9vYmFyYmF6cXV4").is_ok());
    }

    #[test]
    fn deserializes_through_validation() {
        let id: TenantId = serde_json::from_str("\"acme\"").unwrap();
        assert_eq!(id.as_str(), "acme");
        assert!(serde_json::from_str::<TenantId>("\"acme corp\"").is_err());
    }

    #[test]
    fn owned_conversion_still_validates() {
        assert_eq!(
            TenantId::try_from("acme corp".to_string()),
            Err(CoreError::IllegalChar {
                kind: "tenant id",
                ch: ' ',
            })
        );
        assert_eq!(
            Nonce::try_from("abc".to_string()),
            Err(CoreError::TooShort {
                kind: "nonce",
                len: 3,
                min: NONCE_MIN_BYTES,
            })
        );
        assert_eq!(
            TenantId::try_from("acme".to_string()).unwrap().as_str(),
            "acme"
        );
    }

    #[test]
    fn serializes_as_plain_string() {
        let id = UserId::new("alice").unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"alice\"");
    }
}
