use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use url::Url;

use crate::error::ConfigError;
use crate::secret::Secret;
use crate::upstream::UpstreamConfig;

const JWT_SECRET_MIN_BYTES: usize = 32;

/// Everything the proxy is configured with.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Where the proxy listens.
    pub server: ServerConfig,
    /// Which storage backend holds tenants, users and grants.
    pub storage: StorageConfig,
    /// Which cache backend holds responses and the system's own short lived state.
    pub cache: CacheConfig,
    /// How callers prove who they are.
    pub auth: AuthConfig,
    /// Logs, traces and metrics.
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    /// The upstream llm endpoints that can be routed to.
    #[serde(default, rename = "upstream")]
    pub upstreams: Vec<UpstreamConfig>,
}

impl Config {
    /// Reads and validates a configuration file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: PathBuf::from(path),
            source,
        })?;
        Self::parse(&text)
    }

    /// Parses and validates configuration that is already in memory.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let secret_len = self.auth.jwt.secret.len();
        if secret_len < JWT_SECRET_MIN_BYTES {
            return Err(ConfigError::WeakJwtSecret {
                len: secret_len,
                min: JWT_SECRET_MIN_BYTES,
            });
        }
        for (index, upstream) in self.upstreams.iter().enumerate() {
            if self.upstreams[..index]
                .iter()
                .any(|earlier| earlier.id == upstream.id)
            {
                return Err(ConfigError::DuplicateUpstream {
                    id: upstream.id.clone(),
                });
            }
        }
        Ok(())
    }
}

/// Where the proxy listens.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Address the downstream listener binds to.
    pub listen: SocketAddr,
}

/// Which storage backend holds tenants, users and grants.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case", deny_unknown_fields)]
pub enum StorageConfig {
    /// Standalone deployment, backed by a local file.
    Sqlite {
        /// Path of the database file.
        path: PathBuf,
    },
    /// Cluster deployment, backed by a shared server.
    Postgres {
        /// Connection url, which carries the password.
        url: Secret,
    },
}

/// Which cache backend holds responses and the system's own short lived state.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case", deny_unknown_fields)]
pub enum CacheConfig {
    /// Standalone deployment, backed by a local database.
    Sled {
        /// Path of the database directory.
        path: PathBuf,
    },
    /// Cluster deployment, backed by a server every node shares.
    Redis {
        /// Connection url, which carries the password.
        url: Secret,
    },
}

/// How callers prove who they are.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Web session tokens.
    pub jwt: JwtConfig,
    /// How long a nonce stays unusable after it is spent.
    #[serde(default = "default_nonce_ttl")]
    pub nonce_ttl: Seconds,
}

/// Web session tokens.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JwtConfig {
    /// Issuer claim written into, and required of, every token.
    pub issuer: String,
    /// Signing key.
    pub secret: Secret,
    /// How long a freshly issued token stays valid.
    #[serde(default = "default_jwt_ttl")]
    pub ttl: Seconds,
}

/// Logs, traces and metrics.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    /// Lowest level that reaches the log.
    #[serde(default)]
    pub log_level: LogLevel,
    /// Collector traces and metrics are exported to, when one is configured.
    #[serde(default)]
    pub otlp_endpoint: Option<Url>,
}

/// Lowest level that reaches the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Everything, including per request detail.
    Trace,
    /// Detail useful while diagnosing a fault.
    Debug,
    /// Ordinary operational events.
    #[default]
    Info,
    /// Something recovered from.
    Warn,
    /// Something that failed.
    Error,
}

/// A duration written in the file as a whole number of seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(transparent)]
pub struct Seconds(u64);

impl Seconds {
    /// Wraps a whole number of seconds.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The number of seconds.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The same span as a `Duration`.
    pub const fn as_duration(self) -> Duration {
        Duration::from_secs(self.0)
    }
}

impl From<LogLevel> for tracing_core::Level {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Trace => Self::TRACE,
            LogLevel::Debug => Self::DEBUG,
            LogLevel::Info => Self::INFO,
            LogLevel::Warn => Self::WARN,
            LogLevel::Error => Self::ERROR,
        }
    }
}

impl From<LogLevel> for tracing_core::LevelFilter {
    fn from(level: LogLevel) -> Self {
        Self::from_level(level.into())
    }
}

fn default_nonce_ttl() -> Seconds {
    Seconds::new(300)
}

fn default_jwt_ttl() -> Seconds {
    Seconds::new(6 * 3600)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::{ModelName, Temperature};
    use ip_core::ApiId;

    const FULL: &str = r#"
[server]
listen = "127.0.0.1:8080"

[storage]
backend = "sqlite"
path = "/var/lib/intelli-prism/state.db"

[cache]
backend = "sled"
path = "/var/lib/intelli-prism/cache"

[auth.jwt]
issuer = "intelli-prism"
secret = "0123456789abcdef0123456789abcdef"
ttl = 900

[telemetry]
log_level = "debug"
otlp_endpoint = "http://localhost:4317/"

[[upstream]]
id = "anthropic"
base_url = "https://api.anthropic.com/"
api_key = "sk-secret-value"
model = "claude-opus-5"
temperature = 0.7
"#;

    const MINIMAL: &str = r#"
[server]
listen = "0.0.0.0:443"

[storage]
backend = "postgres"
url = "postgres://ip:pw@db/intelli_prism"

[cache]
backend = "redis"
url = "redis://:pw@cache:6379"

[auth.jwt]
issuer = "intelli-prism"
secret = "0123456789abcdef0123456789abcdef"
"#;

    #[test]
    fn parses_a_full_configuration() {
        let config = Config::parse(FULL).unwrap();
        assert_eq!(config.server.listen, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(
            config.storage,
            StorageConfig::Sqlite {
                path: PathBuf::from("/var/lib/intelli-prism/state.db"),
            }
        );
        assert_eq!(config.auth.jwt.ttl, Seconds::new(900));
        assert_eq!(config.telemetry.log_level, LogLevel::Debug);
        assert_eq!(config.upstreams.len(), 1);
        let upstream = &config.upstreams[0];
        assert_eq!(upstream.id, ApiId::new("anthropic").unwrap());
        assert_eq!(upstream.model, ModelName::new("claude-opus-5").unwrap());
        assert_eq!(upstream.temperature, Some(Temperature::new(0.7).unwrap()));
        assert_eq!(upstream.api_key.expose(), "sk-secret-value");
    }

    #[test]
    fn fills_in_defaults_for_absent_sections() {
        let config = Config::parse(MINIMAL).unwrap();
        assert_eq!(config.telemetry.log_level, LogLevel::Info);
        assert_eq!(config.telemetry.otlp_endpoint, None);
        assert_eq!(config.auth.nonce_ttl, Seconds::new(300));
        assert_eq!(config.auth.jwt.ttl, Seconds::new(6 * 3600));
        assert!(config.upstreams.is_empty());
    }

    #[test]
    fn selects_the_cache_backend_by_tag() {
        let config = Config::parse(MINIMAL).unwrap();
        let CacheConfig::Redis { url } = &config.cache else {
            panic!("expected the redis backend");
        };
        assert_eq!(url.expose(), "redis://:pw@cache:6379");
    }

    #[test]
    fn a_local_cache_is_named_by_path() {
        let config = Config::parse(FULL).unwrap();
        assert_eq!(
            config.cache,
            CacheConfig::Sled {
                path: PathBuf::from("/var/lib/intelli-prism/cache"),
            }
        );
    }

    #[test]
    fn selects_the_storage_backend_by_tag() {
        let config = Config::parse(MINIMAL).unwrap();
        let StorageConfig::Postgres { url } = &config.storage else {
            panic!("expected the postgres backend");
        };
        assert_eq!(url.expose(), "postgres://ip:pw@db/intelli_prism");
    }

    #[test]
    fn an_upstream_may_override_the_route_it_answers_on() {
        let text = format!(
            r#"{FULL}
[upstream.route]
protocol = "https"
host = "ai.corp.example"
port = 443
path = "/chat"
"#
        );
        let config = Config::parse(&text).unwrap();
        let key = config.upstreams[0]
            .route_key("127.0.0.1:8080".parse().unwrap())
            .unwrap();
        assert_eq!(key.to_string(), "https://ai.corp.example:443/chat");
    }

    #[test]
    fn an_upstream_without_an_override_derives_its_route() {
        let config = Config::parse(FULL).unwrap();
        assert!(config.upstreams[0].route.is_none());
        let key = config.upstreams[0].route_key(config.server.listen).unwrap();
        assert_eq!(key.to_string(), "http://127.0.0.1:8080/anthropic");
    }

    #[test]
    fn rejects_an_unknown_key() {
        let text = FULL.replace("issuer =", "issuerr =");
        assert!(matches!(Config::parse(&text), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn rejects_a_short_jwt_secret() {
        let text = MINIMAL.replace("0123456789abcdef0123456789abcdef", "tooshort");
        assert!(matches!(
            Config::parse(&text),
            Err(ConfigError::WeakJwtSecret { len: 8, min: 32 })
        ));
    }

    #[test]
    fn rejects_a_duplicate_upstream_id() {
        let text = format!(
            r#"{FULL}
[[upstream]]
id = "anthropic"
base_url = "https://example.invalid/"
api_key = "another-secret"
model = "claude-sonnet-5"
"#
        );
        match Config::parse(&text) {
            Err(ConfigError::DuplicateUpstream { id }) => {
                assert_eq!(id, ApiId::new("anthropic").unwrap());
            }
            other => panic!("expected a duplicate upstream, got {other:?}"),
        }
    }

    #[test]
    fn rejects_a_temperature_out_of_range() {
        let text = FULL.replace("temperature = 0.7", "temperature = 2.5");
        assert!(matches!(Config::parse(&text), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn rejects_a_malformed_identifier() {
        let text = FULL.replace("id = \"anthropic\"", "id = \"anthropic api\"");
        assert!(matches!(Config::parse(&text), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn reports_the_path_of_a_file_it_cannot_read() {
        match Config::load("/nonexistent/intelli-prism.toml") {
            Err(ConfigError::Read { path, .. }) => {
                assert_eq!(path, PathBuf::from("/nonexistent/intelli-prism.toml"));
            }
            other => panic!("expected a read error, got {other:?}"),
        }
    }

    #[test]
    fn every_log_level_converts_to_the_tracing_level() {
        for (ours, theirs) in [
            (LogLevel::Trace, tracing_core::Level::TRACE),
            (LogLevel::Debug, tracing_core::Level::DEBUG),
            (LogLevel::Info, tracing_core::Level::INFO),
            (LogLevel::Warn, tracing_core::Level::WARN),
            (LogLevel::Error, tracing_core::Level::ERROR),
        ] {
            assert_eq!(tracing_core::Level::from(ours), theirs);
            assert_eq!(
                tracing_core::LevelFilter::from(ours),
                tracing_core::LevelFilter::from_level(theirs)
            );
        }
    }

    #[test]
    fn no_secret_reaches_the_debug_output() {
        let rendered = format!("{:?}", Config::parse(FULL).unwrap());
        assert!(!rendered.contains("sk-secret-value"));
        assert!(!rendered.contains("0123456789abcdef"));
        assert!(rendered.contains("Secret(redacted)"));
    }
}
