use std::fmt;

use ip_core::{ApiId, CoreError};
use url::Url;

use crate::error::ConfigError;
use crate::secret::Secret;

const MODEL_NAME_MAX_BYTES: usize = 128;

/// One upstream llm endpoint the proxy can route to.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    /// The id routing rules and api capabilities name this upstream by.
    pub id: ApiId,
    /// Base url the request is sent to.
    pub base_url: Url,
    /// Credential presented to the upstream.
    pub api_key: Secret,
    /// Model requested when the caller does not name one.
    pub model: ModelName,
    /// Sampling temperature applied when the caller does not set one.
    #[serde(default)]
    pub temperature: Option<Temperature>,
}

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

/// A sampling temperature inside the range every upstream accepts.
#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize)]
#[serde(try_from = "f64")]
pub struct Temperature(f32);

impl Temperature {
    /// Lowest temperature an upstream accepts.
    pub const MIN: f32 = 0.0;
    /// Highest temperature an upstream accepts.
    pub const MAX: f32 = 2.0;

    /// Wraps a temperature after checking it against the accepted range.
    pub fn new(value: f64) -> Result<Self, ConfigError> {
        if !value.is_finite() || value < Self::MIN as f64 || value > Self::MAX as f64 {
            return Err(ConfigError::Temperature {
                value,
                min: Self::MIN,
                max: Self::MAX,
            });
        }
        Ok(Self(value as f32))
    }

    /// The temperature as the upstream wants it.
    pub fn get(self) -> f32 {
        self.0
    }
}

impl TryFrom<f64> for Temperature {
    type Error = ConfigError;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::new(value)
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

    #[test]
    fn a_temperature_inside_the_range_is_kept() {
        assert_eq!(Temperature::new(0.7).unwrap().get(), 0.7_f32);
        assert_eq!(Temperature::new(0.0).unwrap().get(), Temperature::MIN);
        assert_eq!(Temperature::new(2.0).unwrap().get(), Temperature::MAX);
    }

    #[test]
    fn a_temperature_outside_the_range_is_refused() {
        for value in [-0.1, 2.1] {
            assert!(matches!(
                Temperature::new(value),
                Err(ConfigError::Temperature { .. })
            ));
        }
    }

    #[test]
    fn a_temperature_that_is_not_a_number_is_refused() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(matches!(
                Temperature::try_from(value),
                Err(ConfigError::Temperature { .. })
            ));
        }
    }
}
