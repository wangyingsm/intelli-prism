//! System configuration for the proxy.

pub mod config;
pub mod error;
pub mod secret;
pub mod upstream;

pub use config::{
    AuthConfig, CacheConfig, Config, DifficultyConfig, JwtConfig, LogFile, LogLevel, LogsKept,
    SampleRatio, Seconds, SemanticConfig, ServerConfig, StorageConfig, TelemetryConfig,
    UsageConfig,
};
pub use error::ConfigError;
pub use ip_core::ModelName;
pub use secret::Secret;
pub use upstream::{RouteOverride, Temperature, UpstreamConfig, UpstreamTarget};
