use std::fmt;
use std::net::SocketAddr;

use ip_core::{
    AbsPath, ApiId, CoreError, Endpoint, Host, Port, Protocol, RouteKey, RouteRule, RouteTarget,
};
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
    /// Overrides the key this upstream would otherwise derive.
    #[serde(default)]
    pub route: Option<RouteOverride>,
}

/// Replaces part of the key a configured upstream derives from the listen address.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteOverride {
    /// What arrives, when it is not plain http.
    #[serde(default)]
    pub protocol: Option<Protocol>,
    /// The hostname callers reach the gateway by.
    #[serde(default)]
    pub host: Option<Host>,
    /// The port callers reach the gateway on.
    #[serde(default)]
    pub port: Option<Port>,
    /// The path root this upstream answers under.
    #[serde(default)]
    pub path: Option<AbsPath>,
}

impl UpstreamConfig {
    /// The key this upstream answers on: the listen address with the id as path root,
    /// with anything the `route` block names taking precedence.
    pub fn route_key(&self, listen: SocketAddr) -> Result<RouteKey, ConfigError> {
        let override_of = self.route.clone().unwrap_or_default();
        let protocol = override_of.protocol.unwrap_or(Protocol::Http);
        let host = match override_of.host {
            Some(host) => host,
            None => Host::new(&listen.ip().to_string())?,
        };
        let port = match override_of.port {
            Some(port) => port,
            None => Port::new(listen.port())?,
        };
        let path = match override_of.path {
            Some(path) => path,
            None => AbsPath::new(&format!("/{}", self.id))?,
        };
        Ok(RouteKey::new(Endpoint::new(protocol, host, port, path)))
    }

    /// Where a matched request is sent, read from `base_url`.
    pub fn route_target(&self) -> Result<RouteTarget, ConfigError> {
        let host = self
            .base_url
            .host_str()
            .ok_or_else(|| ConfigError::UpstreamHost {
                id: self.id.clone(),
            })?;
        let port =
            self.base_url
                .port_or_known_default()
                .ok_or_else(|| ConfigError::UpstreamPort {
                    id: self.id.clone(),
                })?;
        Ok(RouteTarget::new(Endpoint::new(
            self.base_url.scheme().parse()?,
            Host::new(host)?,
            Port::new(port)?,
            AbsPath::new(self.base_url.path())?,
        )))
    }

    /// The rule this upstream contributes to the routing table.
    pub fn route_rule(&self, listen: SocketAddr) -> Result<RouteRule, ConfigError> {
        Ok(RouteRule {
            api: self.id.clone(),
            key: self.route_key(listen)?,
            target: self.route_target()?,
        })
    }
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

    fn upstream(base_url: &str, route: Option<RouteOverride>) -> UpstreamConfig {
        UpstreamConfig {
            id: ApiId::new("anthropic").unwrap(),
            base_url: base_url.parse().unwrap(),
            api_key: crate::Secret::from("sk-x"),
            model: ModelName::new("claude-opus-5").unwrap(),
            temperature: None,
            route,
        }
    }

    fn listen() -> SocketAddr {
        "127.0.0.1:8080".parse().unwrap()
    }

    #[test]
    fn a_key_is_derived_from_the_listen_address_and_the_id() {
        let key = upstream("https://api.anthropic.com/v1", None)
            .route_key(listen())
            .unwrap();
        assert_eq!(key.to_string(), "http://127.0.0.1:8080/anthropic");
    }

    #[test]
    fn a_target_is_read_from_the_base_url() {
        let target = upstream("https://api.anthropic.com/v1", None)
            .route_target()
            .unwrap();
        assert_eq!(target.to_string(), "https://api.anthropic.com:443/v1");
    }

    #[test]
    fn a_base_url_with_no_path_targets_the_root() {
        let target = upstream("https://api.anthropic.com", None)
            .route_target()
            .unwrap();
        assert_eq!(target.primary().path.as_str(), "/");
    }

    #[test]
    fn an_explicit_port_in_the_base_url_wins_over_the_scheme_default() {
        let target = upstream("https://llm.corp:8443/v1", None)
            .route_target()
            .unwrap();
        assert_eq!(target.primary().port.get(), 8443);
    }

    #[test]
    fn an_override_replaces_only_what_it_names() {
        let key = upstream(
            "https://api.anthropic.com/v1",
            Some(RouteOverride {
                host: Some(Host::new("ai.corp.example").unwrap()),
                ..RouteOverride::default()
            }),
        )
        .route_key(listen())
        .unwrap();
        assert_eq!(key.to_string(), "http://ai.corp.example:8080/anthropic");
    }

    #[test]
    fn an_override_can_replace_the_whole_key() {
        let key = upstream(
            "https://api.anthropic.com/v1",
            Some(RouteOverride {
                protocol: Some(Protocol::Https),
                host: Some(Host::new("ai.corp.example").unwrap()),
                port: Some(Port::new(443).unwrap()),
                path: Some(AbsPath::new("/chat").unwrap()),
            }),
        )
        .route_key(listen())
        .unwrap();
        assert_eq!(key.to_string(), "https://ai.corp.example:443/chat");
    }

    #[test]
    fn a_rule_pairs_the_derived_key_with_the_base_url() {
        let rule = upstream("https://api.anthropic.com/v1", None)
            .route_rule(listen())
            .unwrap();
        assert_eq!(rule.key.to_string(), "http://127.0.0.1:8080/anthropic");
        assert_eq!(rule.target.to_string(), "https://api.anthropic.com:443/v1");
    }

    #[test]
    fn a_rule_serves_the_api_the_upstream_names() {
        let rule = upstream("https://api.anthropic.com/v1", None)
            .route_rule(listen())
            .unwrap();
        assert_eq!(rule.api, ApiId::new("anthropic").unwrap());
    }

    #[test]
    fn a_base_url_that_names_no_host_is_refused() {
        assert!(matches!(
            upstream("file:///models", None).route_target(),
            Err(ConfigError::UpstreamHost { .. })
        ));
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
