//! The name of a model, as an upstream spells it and as a usage record carries it.

use std::fmt;

use crate::error::CoreError;

/// Longest model name any upstream is known to spell.
const MODEL_NAME_MAX_BYTES: usize = 128;

/// The name of a model as the upstream spells it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct ModelName(Box<str>);

impl ModelName {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "model name";

    /// Validates and wraps a raw model name.
    pub fn new(raw: &str) -> Result<Self, CoreError> {
        validate_model_name(raw)?;
        Ok(Self(Box::from(raw)))
    }

    /// The model name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn validate_model_name(raw: &str) -> Result<(), CoreError> {
    let kind = ModelName::KIND;
    if raw.is_empty() {
        return Err(CoreError::Empty { kind });
    }
    if raw.len() > MODEL_NAME_MAX_BYTES {
        return Err(CoreError::TooLong {
            kind,
            len: raw.len(),
            max: MODEL_NAME_MAX_BYTES,
        });
    }
    match raw.chars().find(|ch| !ch.is_ascii_graphic()) {
        Some(ch) => Err(CoreError::IllegalChar { kind, ch }),
        None => Ok(()),
    }
}

impl fmt::Display for ModelName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ModelName {
    type Error = CoreError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        validate_model_name(&raw)?;
        Ok(Self(raw.into_boxed_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_name_round_trips() {
        let name = ModelName::new("claude-opus-5").unwrap();
        assert_eq!(name.as_str(), "claude-opus-5");
        assert_eq!(name.to_string(), "claude-opus-5");
    }

    #[test]
    fn a_model_name_may_carry_a_vendor_prefix() {
        assert!(ModelName::new("anthropic/claude-opus-5").is_ok());
    }

    #[test]
    fn an_empty_model_name_is_refused() {
        assert_eq!(
            ModelName::new(""),
            Err(CoreError::Empty { kind: "model name" })
        );
    }

    #[test]
    fn an_over_long_model_name_is_refused() {
        let raw = "m".repeat(MODEL_NAME_MAX_BYTES + 1);
        assert_eq!(
            ModelName::new(&raw),
            Err(CoreError::TooLong {
                kind: "model name",
                len: MODEL_NAME_MAX_BYTES + 1,
                max: MODEL_NAME_MAX_BYTES,
            })
        );
    }

    #[test]
    fn a_model_name_with_a_space_is_refused() {
        assert_eq!(
            ModelName::new("claude opus"),
            Err(CoreError::IllegalChar {
                kind: "model name",
                ch: ' ',
            })
        );
    }

    #[test]
    fn an_owned_model_name_still_validates() {
        assert!(ModelName::try_from("claude-opus-5".to_string()).is_ok());
        assert!(ModelName::try_from("claude opus".to_string()).is_err());
    }
}
