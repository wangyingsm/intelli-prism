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
