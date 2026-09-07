use std::fmt;

use subtle::ConstantTimeEq;
use zeroize::Zeroize;

/// A credential read from configuration: exposed only on demand, never printed or serialized.
#[derive(Clone, Eq, serde::Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// The plaintext credential.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The plaintext credential as bytes.
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    /// Length of the credential in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the credential is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for Secret {
    fn from(raw: String) -> Self {
        Self(raw)
    }
}

impl From<&str> for Secret {
    fn from(raw: &str) -> Self {
        Self(raw.to_owned())
    }
}

/// Constant time: a secret is never compared with a short circuiting equality.
impl PartialEq for Secret {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_bytes().ct_eq(other.0.as_bytes()).into()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(redacted)")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_only_through_the_named_accessor() {
        let secret = Secret::from("hunter2hunter2hunter2");
        assert_eq!(secret.expose(), "hunter2hunter2hunter2");
        assert_eq!(secret.len(), 21);
    }

    #[test]
    fn exposes_its_bytes_for_hashing() {
        assert_eq!(Secret::from("s3cret").as_bytes(), b"s3cret");
    }

    #[test]
    fn knows_when_it_holds_nothing() {
        assert!(Secret::from(String::new()).is_empty());
        assert!(!Secret::from("s3cret".to_owned()).is_empty());
    }

    #[test]
    fn is_redacted_in_debug_output() {
        let secret = Secret::from("hunter2");
        assert_eq!(format!("{secret:?}"), "Secret(redacted)");
    }

    #[test]
    fn compares_by_value() {
        assert_eq!(Secret::from("same"), Secret::from("same"));
        assert_ne!(Secret::from("same"), Secret::from("other"));
        assert_ne!(Secret::from("same"), Secret::from("same-but-longer"));
    }

    #[test]
    fn deserializes_from_a_bare_string() {
        let secret: Secret = toml::from_str::<Wrapper>("value = \"s3cret\"")
            .unwrap()
            .value;
        assert_eq!(secret.expose(), "s3cret");
    }

    #[derive(serde::Deserialize)]
    struct Wrapper {
        value: Secret,
    }
}
