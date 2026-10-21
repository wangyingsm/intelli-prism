use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ip_embed::MOST_HELD;
use serde::Deserialize;
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
    /// What is kept of the record of every request.
    #[serde(default)]
    pub usage: UsageConfig,
    /// The upstream llm endpoints that can be routed to.
    #[serde(default, rename = "upstream")]
    pub upstreams: Vec<UpstreamConfig>,
    /// The local judge of a turn's opening. Absent, nothing is ever classified and
    /// every request dispatches as it did before the judge existed.
    #[serde(default)]
    pub difficulty: Option<DifficultyConfig>,
    /// The larger model asked what the local margin could not call. Absent, an unsure
    /// opening is simply unclassified.
    #[serde(default)]
    pub arbiter: Option<ArbiterConfig>,
    /// What must not leave a request or its answer. Absent, nothing is looked for and
    /// every body travels exactly as it did before the filter existed.
    #[serde(default)]
    pub filter: Option<FilterConfig>,
    /// The cache that answers a question asked before in other words. Absent, the
    /// level holds nothing and no request is ever embedded.
    #[serde(default)]
    pub semantic: Option<SemanticConfig>,
}

/// The third cache level: the model that recognises a question, and how near two of
/// them must be before one may answer for the other.
///
/// Present at all, the level is on for every llm route. There is no per-route switch:
/// what a near miss is worth is one judgement about one deployment's traffic, and an
/// operator who needs it off for a route can leave the stanza out until they do not.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticConfig {
    /// The directory the embedding model was exported to: `model.onnx`,
    /// `tokenizer.json` and `embed.json` together are the whole artefact.
    pub artifact: std::path::PathBuf,
    /// How near two questions must mean the same thing before one is answered with
    /// the other's answer, inclusive at the boundary.
    ///
    /// No default, deliberately, exactly as the classifier's confidence has none.
    /// This number decides when the gateway hands one caller another's answer, and
    /// the only honest way to pick it is to watch `x-ip-nearness` against real
    /// traffic. A figure invented here would be a stranger's guess wearing the
    /// authority of a default.
    pub nearness: f32,
    /// How long an answer stands for a question other than the one that produced it.
    ///
    /// Its own span, not the response cache's: how long an answer is good for its own
    /// question is a different judgement from how long it may stand in for a
    /// different one, and the second is the shorter of the two more often than not.
    #[serde(default = "default_semantic_ttl")]
    pub ttl: Seconds,
    /// How many questions one tenant's index holds on one route before its least
    /// recently used go.
    ///
    /// A lookup is a scan, so this is a latency budget as much as a memory one; the
    /// default is comfortably inside what a scan answers in about a millisecond.
    #[serde(default = "default_semantic_capacity")]
    pub capacity: usize,
    /// Where `libonnxruntime` lives, when the build did not find one. The
    /// environment's `ORT_DYLIB_PATH` answers first.
    #[serde(default)]
    pub library: Option<std::path::PathBuf>,
}

fn default_semantic_ttl() -> Seconds {
    Seconds::new(3_600)
}

fn default_semantic_capacity() -> usize {
    4_096
}

/// The operator's floor: what must not leave, whoever is asking.
///
/// It is configuration and not a stored rule because a tenant must not be able to
/// switch off the filter that stops their own data leaving. A tenant may add to this;
/// nothing a tenant does takes anything out of it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilterConfig {
    /// Which of the secrets that ship are in force, by name.
    ///
    /// Left out, the ones whose issuer published a check are used: a card number, a
    /// resident identity number and an account number each confirm themselves, so they
    /// can be switched on without first measuring what they cost in false positives.
    /// The rest — the provider key shapes, the email address — are named deliberately
    /// or not at all.
    #[serde(default)]
    pub shipped: Option<Vec<String>>,
    /// Secrets of the operator's own, recognised by plain text, a prefix, a suffix or
    /// an expression. These are added to the shipped ones rather than replacing them.
    #[serde(default, rename = "secret")]
    pub secrets: Vec<ip_filter::Secret>,
    /// How many characters of a streamed answer are held back to be looked at.
    ///
    /// A model writes a secret token by token, so one is only seen by holding text
    /// until nothing is still growing into a match. This is what that costs in time to
    /// first token. Zero holds nothing and catches only what arrives whole in one
    /// event, which is most often nothing at all.
    #[serde(default = "held_back")]
    pub window: usize,
    /// What one tenant wants looked for beyond the floor. Added to it, never instead
    /// of it.
    #[serde(default, rename = "tenant")]
    pub tenants: Vec<TenantFilterConfig>,
}

/// One tenant's own secrets, on top of the floor every tenant passes through.
///
/// A tenant may ask for more to be looked for. Nothing here takes anything out of the
/// floor: naming no secrets at all leaves the floor exactly as it was, which is what
/// makes it a floor rather than a default.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantFilterConfig {
    /// Whose these are.
    pub tenant: ip_core::TenantId,
    /// Shipped secrets this tenant wants beyond the floor's.
    #[serde(default)]
    pub shipped: Vec<String>,
    /// Secrets of this tenant's own.
    #[serde(default, rename = "secret")]
    pub secrets: Vec<ip_filter::Secret>,
}

/// The window an operator who says nothing gets, which is the gateway's own default.
const fn held_back() -> usize {
    ip_filter::WINDOW
}

impl FilterConfig {
    /// Every secret in force: the shipped ones this names, and the operator's own.
    ///
    /// # Errors
    ///
    /// [`ConfigError::UnknownSecret`] when it names a shipped secret that does not
    /// exist, and [`ConfigError::Filter`] when one of its own will not compile.
    pub fn secrets(&self) -> Result<Vec<ip_filter::Secret>, ConfigError> {
        let mut secrets = match &self.shipped {
            // Named deliberately, one at a time.
            Some(names) => names
                .iter()
                .map(|name| {
                    ip_filter::Secret::default_named(name).ok_or_else(|| {
                        ConfigError::UnknownSecret {
                            value: name.clone(),
                            carried: ip_filter::Secret::default_names().join(", "),
                        }
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            // Said nothing, so the ones that confirm themselves.
            None => ip_filter::Secret::defaults()
                .into_iter()
                .filter(|secret| secret.check.is_some())
                .collect(),
        };
        secrets.extend(self.secrets.iter().cloned());
        Ok(secrets)
    }

    /// Everything in force for one tenant: the floor, and then whatever that tenant
    /// asked for on top of it.
    ///
    /// The floor comes first and is never consulted about: a tenant naming nothing
    /// gets exactly the floor, and a tenant naming something gets the floor and that.
    ///
    /// # Errors
    ///
    /// [`ConfigError::UnknownSecret`] when the tenant names a shipped secret that does
    /// not exist.
    pub fn secrets_for(
        &self,
        tenant: &ip_core::TenantId,
    ) -> Result<Vec<ip_filter::Secret>, ConfigError> {
        let mut secrets = self.secrets()?;
        let Some(theirs) = self.tenants.iter().find(|one| &one.tenant == tenant) else {
            return Ok(secrets);
        };
        for name in &theirs.shipped {
            secrets.push(ip_filter::Secret::default_named(name).ok_or_else(|| {
                ConfigError::UnknownSecret {
                    value: name.clone(),
                    carried: ip_filter::Secret::default_names().join(", "),
                }
            })?);
        }
        secrets.extend(theirs.secrets.iter().cloned());
        Ok(secrets)
    }

    /// Every tenant that asked for something of its own.
    #[must_use]
    pub fn named_tenants(&self) -> Vec<&ip_core::TenantId> {
        self.tenants.iter().map(|one| &one.tenant).collect()
    }
}

/// The model an unsure opening is arbitrated by, and how hard it is asked: the route
/// the question travels, the model named in it, and the time it is given before the
/// request it describes is sent without an answer.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArbiterConfig {
    /// The model the question names: served by the pool the route leads to, asked
    /// exactly as a caller's request would ask it.
    pub model: ip_core::ModelName,
    /// The path the question is resolved by, under the authority the caller arrived
    /// with: a gateway serving several hostnames needs this route registered behind
    /// each of them.
    pub ask_path: ip_core::AbsPath,
    /// The declared upstream whose credential the question presents. Naming one is
    /// deliberate: an arbitrating route replicated over several endpoints presents this
    /// upstream's key all the same.
    pub upstream: ip_core::ApiId,
    /// How long the answer is given to arrive before it is abandoned. This is latency
    /// the opening request of an unsure conversation pays — the turn's first request
    /// waits it, once, and every later request of that turn pays nothing. Choosing this
    /// number chooses a tail latency. Two seconds is the default because the arbiter
    /// improves routing and must never become a dependency of serving.
    #[serde(default = "default_arbiter_timeout")]
    pub timeout: Seconds,
}

fn default_arbiter_timeout() -> Seconds {
    Seconds::new(2)
}

/// Past this, waiting on the arbiter is waiting on an outside model instead of being
/// briefly delayed by one.
const ARBITER_TIMEOUT_MAX_SECONDS: u64 = 30;

/// The gate between a request and a class: where the weights live, and how sure the
/// judge must be before it commits to the class it names.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DifficultyConfig {
    /// The directory the model was exported to: `model.onnx`, `tokenizer.json` and
    /// `gate.json` together are the whole artefact.
    pub artifact: std::path::PathBuf,
    /// How far the top class must stand over the runner-up for the class to be acted
    /// on. Below it the judge refers, and the request dispatches unclassified — the
    /// gate is the whole design, so it has no default: the operator chooses, in the
    /// open, against the validation set the artefact shipped with.
    pub confidence: f64,
    /// Where `libonnxruntime` lives, when the build did not find one. The environment's
    /// `ORT_DYLIB_PATH` answers first.
    #[serde(default)]
    pub library: Option<std::path::PathBuf>,
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
        // A confidence outside 0.0..=1.0 is a mistyped margin, not a decision: the
        // judgement belongs here, with the rest of the config's shape, where it costs
        // a parse — not the model's 1.17 GB, which `Confidence::new` would otherwise
        // have been loaded behind before it ever said so.
        if let Some(difficulty) = &self.difficulty
            && !(0.0..=1.0).contains(&difficulty.confidence)
        {
            return Err(ConfigError::DifficultyConfidence {
                value: difficulty.confidence,
            });
        }
        // The same judgement as the confidence above, and for a sharper reason: this
        // number decides when one caller is handed another's answer. Outside the unit
        // it admits everything or nothing, and a capacity of nothing is a level that
        // can never hit, written as though it were a setting.
        if let Some(semantic) = &self.semantic {
            if !(0.0..=1.0).contains(&semantic.nearness) {
                return Err(ConfigError::SemanticNearness {
                    value: semantic.nearness,
                });
            }
            if !(1..=MOST_HELD).contains(&semantic.capacity) {
                return Err(ConfigError::SemanticCapacity {
                    value: semantic.capacity,
                    limit: MOST_HELD,
                });
            }
        }
        // A timeout outside a second to half a minute is a shape mistake, refused where
        // the rest of the config's shape is refused: zero asks the arbiter nothing, and
        // anything past the limit makes a billed outside model a hard dependency of
        // serving, which is the one thing an arbiter may never be.
        if let Some(arbiter) = &self.arbiter {
            let secs = arbiter.timeout.get();
            if !(1..=ARBITER_TIMEOUT_MAX_SECONDS).contains(&secs) {
                return Err(ConfigError::ArbiterTimeout {
                    value: secs,
                    limit: ARBITER_TIMEOUT_MAX_SECONDS,
                });
            }
            // The credential the question presents belongs to a declared upstream, so
            // the reference has to resolve before startup, not at the first question.
            match self
                .upstreams
                .iter()
                .find(|upstream| upstream.id == arbiter.upstream)
            {
                None => {
                    return Err(ConfigError::ArbiterUpstream {
                        id: arbiter.upstream.clone(),
                    });
                }
                // An empty key would send an empty credential for the life of the
                // process, every question refused, arbitration silently off. Checked
                // for the arbitrating upstream only: the field has been carried and
                // read by nothing until here, so other upstreams may hold one today.
                Some(upstream) if upstream.api_key.is_empty() => {
                    return Err(ConfigError::ArbiterKey {
                        id: arbiter.upstream.clone(),
                    });
                }
                Some(_) => {}
            }
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
        /// Seconds a cached response stays. Absent caches no response at all.
        #[serde(default)]
        response_ttl: Option<Seconds>,
        /// Bytes the response cache may hold before its least recently used entries go.
        #[serde(default)]
        response_max_bytes: Option<u64>,
        /// Bytes the semantic cache may hold before its least recently used entries go.
        #[serde(default)]
        semantic_max_bytes: Option<u64>,
        /// Bytes the system cache may hold before its least recently used entries go.
        #[serde(default)]
        system_max_bytes: Option<u64>,
    },
    /// Cluster deployment, backed by a server every node shares.
    Redis {
        /// Connection url, which carries the password.
        url: Secret,
        /// Seconds a cached response stays. Absent caches no response at all.
        #[serde(default)]
        response_ttl: Option<Seconds>,
    },
}

impl CacheConfig {
    /// How long a cached response stays, when responses are cached at all.
    pub fn response_ttl(&self) -> Option<Seconds> {
        match self {
            Self::Sled { response_ttl, .. } | Self::Redis { response_ttl, .. } => *response_ttl,
        }
    }
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
#[derive(Debug, Clone, PartialEq, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    /// Lowest level that reaches the log.
    #[serde(default)]
    pub log_level: LogLevel,
    /// Collector logs, traces and metrics are exported to, when one is configured.
    #[serde(default)]
    pub otlp_endpoint: Option<Url>,
    /// Share of requests whose trace is exported, for an operator who finds every one too costly.
    #[serde(default)]
    pub sample_ratio: SampleRatio,
    /// File the logs are also written to, rolled daily. Unset, they are printed and
    /// nothing else, which is what a deployment with a collector wants and what a
    /// standalone one loses on restart.
    #[serde(default)]
    pub log_file: Option<LogFile>,
    /// How many rolled files are kept beside the live one.
    #[serde(default)]
    pub log_keep: LogsKept,
}

/// Where the logs are written, as a directory that exists and a name within it.
///
/// Checked when the file is read rather than when the first line is written: the
/// appender makes a missing directory instead of refusing, so a mistyped path would
/// become a tree of logs nobody ever looks in.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(try_from = "PathBuf")]
pub struct LogFile {
    directory: PathBuf,
    name: String,
}

impl LogFile {
    /// Takes a path apart into the directory to write in and the name to write under.
    ///
    /// # Errors
    ///
    /// [`ConfigError::LogFile`] when the path names no file, when its directory does
    /// not exist, or when the path is itself a directory.
    pub fn new(path: PathBuf) -> Result<Self, ConfigError> {
        let refuse = |why: &str| {
            Err(ConfigError::LogFile {
                path: path.clone(),
                why: why.to_owned(),
            })
        };
        if path.is_dir() {
            return refuse("that is a directory");
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return refuse("a log file has a name");
        };
        let name = name.to_owned();
        let directory = match path.parent() {
            Some(parent) if parent.as_os_str().is_empty() => PathBuf::from("."),
            Some(parent) => parent.to_path_buf(),
            None => return refuse("a log file has a name"),
        };
        if !directory.is_dir() {
            return refuse("its directory does not exist");
        }
        Ok(Self { directory, name })
    }

    /// The directory the files are rolled in.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The name the live file is written under, which the rolled ones extend.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl TryFrom<PathBuf> for LogFile {
    type Error = ConfigError;

    fn try_from(path: PathBuf) -> Result<Self, Self::Error> {
        Self::new(path)
    }
}

/// How many rolled log files are kept beside the live one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(try_from = "u16")]
pub struct LogsKept(u16);

impl LogsKept {
    /// Keeping none of them is keeping no history at all, so one is the fewest.
    pub const FEWEST: u16 = 1;
    /// A year of daily files, past which the log directory is the thing filling the disk.
    pub const MOST: u16 = 365;

    /// Wraps a count after checking it is one.
    ///
    /// # Errors
    ///
    /// [`ConfigError::LogsKept`] when the count is outside [`LogsKept::FEWEST`] to
    /// [`LogsKept::MOST`].
    pub fn new(value: u16) -> Result<Self, ConfigError> {
        if !(Self::FEWEST..=Self::MOST).contains(&value) {
            return Err(ConfigError::LogsKept {
                value,
                limit: Self::MOST,
            });
        }
        Ok(Self(value))
    }

    /// The count as the appender wants it.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0 as usize
    }
}

impl Default for LogsKept {
    /// A week, which is what an operator looking into yesterday's incident needs.
    fn default() -> Self {
        Self(7)
    }
}

impl TryFrom<u16> for LogsKept {
    type Error = ConfigError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// The share of requests whose trace is exported, from none of them to all.
#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize)]
#[serde(try_from = "f64")]
pub struct SampleRatio(f64);

impl SampleRatio {
    /// Exports no trace at all.
    pub const NONE: f64 = 0.0;
    /// Exports every request's trace, which is what an unconfigured server does.
    pub const ALL: f64 = 1.0;

    /// Wraps a share after checking it is one.
    pub fn new(value: f64) -> Result<Self, ConfigError> {
        if !value.is_finite() || !(Self::NONE..=Self::ALL).contains(&value) {
            return Err(ConfigError::SampleRatio { value });
        }
        Ok(Self(value))
    }

    /// The share as the sampler wants it.
    pub fn get(self) -> f64 {
        self.0
    }
}

impl Default for SampleRatio {
    fn default() -> Self {
        Self(Self::ALL)
    }
}

impl TryFrom<f64> for SampleRatio {
    type Error = ConfigError;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// What is kept of the record of every request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageConfig {
    /// How long a usage row is kept. Unset keeps every row, which is the default: a record of
    /// what was spent is a business record, and deleting one is an operator's decision.
    #[serde(default)]
    pub retention: Option<Seconds>,
    /// How often rows past their retention are swept away.
    #[serde(default = "default_sweep_every")]
    pub sweep_every: Seconds,
}

/// How often the sweeper runs when the file names no interval.
const fn default_sweep_every() -> Seconds {
    Seconds::new(3600)
}

impl Default for UsageConfig {
    fn default() -> Self {
        Self {
            retention: None,
            sweep_every: default_sweep_every(),
        }
    }
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
    use crate::upstream::Temperature;
    use ip_core::ApiId;
    use ip_core::ModelName;

    const FULL: &str = r#"
[server]
listen = "127.0.0.1:8080"

[storage]
backend = "sqlite"
path = "/var/lib/intelli-prism/state.db"

[cache]
backend = "sled"
path = "/var/lib/intelli-prism/cache"
response_ttl = 60
response_max_bytes = 1048576
system_max_bytes = 4096

[auth.jwt]
issuer = "intelli-prism"
secret = "0123456789abcdef0123456789abcdef"
ttl = 900

[usage]
retention = 2592000
sweep_every = 600

[telemetry]
log_level = "debug"
otlp_endpoint = "http://localhost:4317/"
sample_ratio = 0.25

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
        assert_eq!(config.usage.retention, Some(Seconds::new(2_592_000)));
        assert_eq!(config.usage.sweep_every, Seconds::new(600));
        assert_eq!(
            config.telemetry.sample_ratio,
            SampleRatio::new(0.25).unwrap()
        );
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
        assert_eq!(config.telemetry.sample_ratio, SampleRatio::default());
        assert_eq!(config.telemetry.log_file, None);
        assert_eq!(config.telemetry.log_keep, LogsKept::default());
        assert_eq!(config.usage, UsageConfig::default());
        assert_eq!(config.usage.retention, None);
        assert_eq!(config.auth.nonce_ttl, Seconds::new(300));
        assert_eq!(config.auth.jwt.ttl, Seconds::new(6 * 3600));
        assert!(config.upstreams.is_empty());
    }

    /// A directory under the temporary one, distinct per test and per process.
    fn scratch(tag: &str) -> PathBuf {
        let at = std::env::temp_dir().join(format!("ip-log-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&at);
        std::fs::create_dir_all(&at).expect("a scratch directory");
        at
    }

    #[test]
    fn a_configured_log_file_is_read_and_checked_as_the_file_is_parsed() {
        let at = scratch("parsed");
        let written = format!(
            "{MINIMAL}\n[telemetry]\nlog_file = \"{}/gateway.log\"\nlog_keep = 30\n",
            at.display()
        );
        let config = Config::parse(&written).unwrap();
        let file = config.telemetry.log_file.unwrap();
        assert_eq!(file.directory(), at);
        assert_eq!(file.name(), "gateway.log");
        assert_eq!(config.telemetry.log_keep.get(), 30);

        let nowhere =
            format!("{MINIMAL}\n[telemetry]\nlog_file = \"/no/such/place/gateway.log\"\n");
        assert!(
            Config::parse(&nowhere).is_err(),
            "a log file nothing can be written to stops the file being read"
        );
    }

    #[test]
    fn a_log_file_is_a_directory_that_exists_and_a_name_in_it() {
        let at = scratch("exists");
        let file = LogFile::new(at.join("gateway.log")).unwrap();
        assert_eq!(file.directory(), at);
        assert_eq!(file.name(), "gateway.log");
    }

    #[test]
    fn a_log_file_in_a_directory_that_is_not_there_is_refused() {
        let at = scratch("absent").join("nowhere");
        let refusal = LogFile::new(at.join("gateway.log")).unwrap_err();
        assert!(
            matches!(&refusal, ConfigError::LogFile { why, .. } if why == "its directory does not exist"),
            "a mistyped directory must be said out loud, not created: {refusal}"
        );
    }

    #[test]
    fn a_directory_is_not_a_log_file() {
        let at = scratch("directory");
        let refusal = LogFile::new(at.clone()).unwrap_err();
        assert!(
            matches!(&refusal, ConfigError::LogFile { why, .. } if why == "that is a directory")
        );
    }

    #[test]
    fn a_path_naming_no_file_is_refused() {
        let refusal = LogFile::new(PathBuf::from("/")).unwrap_err();
        assert!(matches!(refusal, ConfigError::LogFile { .. }));
    }

    #[test]
    fn a_bare_name_is_written_in_the_working_directory() {
        let file = LogFile::new(PathBuf::from("gateway.log")).unwrap();
        assert_eq!(file.directory(), Path::new("."));
        assert_eq!(file.name(), "gateway.log");
    }

    #[test]
    fn a_count_of_kept_files_outside_the_limits_is_refused() {
        for value in [0, LogsKept::MOST + 1] {
            assert!(matches!(
                LogsKept::new(value),
                Err(ConfigError::LogsKept { .. })
            ));
        }
        assert_eq!(LogsKept::new(LogsKept::FEWEST).unwrap().get(), 1);
        assert_eq!(LogsKept::new(LogsKept::MOST).unwrap().get(), 365);
        assert_eq!(LogsKept::default().get(), 7);
    }

    #[test]
    fn a_sample_ratio_outside_a_share_is_refused() {
        for value in [-0.1, 1.1, f64::NAN, f64::INFINITY] {
            assert!(matches!(
                SampleRatio::new(value),
                Err(ConfigError::SampleRatio { .. })
            ));
        }
        assert_eq!(SampleRatio::new(0.0).unwrap().get(), 0.0);
        assert_eq!(SampleRatio::new(1.0).unwrap().get(), 1.0);
    }

    #[test]
    fn selects_the_cache_backend_by_tag() {
        let config = Config::parse(MINIMAL).unwrap();
        let CacheConfig::Redis { url, .. } = &config.cache else {
            panic!("expected the redis backend");
        };
        assert_eq!(url.expose(), "redis://:pw@cache:6379");
    }

    #[test]
    fn a_cache_that_names_no_response_ttl_caches_no_response() {
        assert_eq!(Config::parse(MINIMAL).unwrap().cache.response_ttl(), None);
        assert_eq!(
            Config::parse(FULL).unwrap().cache.response_ttl(),
            Some(Seconds::new(60))
        );
    }

    #[test]
    fn a_local_cache_is_named_by_path() {
        let config = Config::parse(FULL).unwrap();
        assert_eq!(
            config.cache,
            CacheConfig::Sled {
                path: PathBuf::from("/var/lib/intelli-prism/cache"),
                response_ttl: Some(Seconds::new(60)),
                response_max_bytes: Some(1_048_576),
                semantic_max_bytes: None,
                system_max_bytes: Some(4_096),
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
    fn an_arbiter_defaults_its_wait_to_two_seconds() {
        let text = format!(
            "{FULL}\n[arbiter]\nmodel = \"oracle\"\nask_path = \"/arbiter\"\nupstream = \"anthropic\"\n"
        );
        let config = Config::parse(&text).unwrap();
        assert_eq!(config.arbiter.unwrap().timeout.get(), 2);
    }

    #[test]
    fn rejects_an_arbiter_that_is_given_no_time_to_answer() {
        let text = format!(
            "{FULL}\n[arbiter]\nmodel = \"oracle\"\nask_path = \"/arbiter\"\nupstream = \"anthropic\"\ntimeout = 0\n"
        );
        assert!(matches!(
            Config::parse(&text),
            Err(ConfigError::ArbiterTimeout { value: 0, .. })
        ));
    }

    #[test]
    fn rejects_an_arbiter_whose_wait_is_a_hard_dependency() {
        let text = format!(
            "{FULL}\n[arbiter]\nmodel = \"oracle\"\nask_path = \"/arbiter\"\nupstream = \"anthropic\"\ntimeout = 600\n"
        );
        assert!(matches!(
            Config::parse(&text),
            Err(ConfigError::ArbiterTimeout { value: 600, .. })
        ));
    }

    #[test]
    fn rejects_an_arbiter_whose_upstream_has_no_key_to_present() {
        let text = format!(
            "{}\n[arbiter]\nmodel = \"oracle\"\nask_path = \"/arbiter\"\nupstream = \"anthropic\"\n",
            FULL.replace("sk-secret-value", "")
        );
        assert!(matches!(
            Config::parse(&text),
            Err(ConfigError::ArbiterKey { .. })
        ));
    }

    #[test]
    fn rejects_an_arbiter_naming_an_upstream_that_is_not_declared() {
        let text = format!(
            "{FULL}\n[arbiter]\nmodel = \"oracle\"\nask_path = \"/arbiter\"\nupstream = \"nowhere\"\n"
        );
        assert!(matches!(
            Config::parse(&text),
            Err(ConfigError::ArbiterUpstream { .. })
        ));
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

    /// A confidence beyond 1 would pass every classification and never refer, and
    /// finding out only after a gigabyte of weights has loaded is the wrong place to
    /// learn it — the range check is config shape, not model state.
    #[test]
    fn rejects_a_difficulty_confidence_outside_its_range() {
        let bad = |confidence: &str| {
            let text = format!(
                "{FULL}\n[difficulty]\nartifact = \"/var/lib/ip/difficulty\"\nconfidence = {confidence}\n"
            );
            Config::parse(&text)
        };
        assert!(matches!(
            bad("1.5"),
            Err(ConfigError::DifficultyConfidence { value }) if (value - 1.5).abs() < f64::EPSILON
        ));
        assert!(matches!(
            bad("-0.1"),
            Err(ConfigError::DifficultyConfidence { .. })
        ));
        assert!(bad("0.4").is_ok());
    }

    /// What a whole stanza resolves to, not merely that it parses: every field read
    /// back as the value it was written as, which is the half a parse test misses.
    #[test]
    fn a_whole_semantic_section_reads_as_what_it_says() {
        let text = format!(
            r#"{FULL}
[semantic]
artifact = "/var/lib/ip/embed"
nearness = 0.97
ttl = 900
capacity = 2048
library = "/usr/lib/libonnxruntime.so"
"#
        );
        let semantic = Config::parse(&text).unwrap().semantic.unwrap();
        assert_eq!(semantic.artifact, PathBuf::from("/var/lib/ip/embed"));
        assert!((semantic.nearness - 0.97).abs() < f32::EPSILON);
        assert_eq!(semantic.ttl.get(), 900);
        assert_eq!(semantic.capacity, 2048);
        assert_eq!(
            semantic.library,
            Some(PathBuf::from("/usr/lib/libonnxruntime.so"))
        );
    }

    /// The two fields that may be left out, and what standing in for them means.
    #[test]
    fn a_semantic_section_may_leave_the_span_and_the_bound_unsaid() {
        let text =
            format!("{FULL}\n[semantic]\nartifact = \"/var/lib/ip/embed\"\nnearness = 0.97\n");
        let semantic = Config::parse(&text).unwrap().semantic.unwrap();
        assert_eq!(semantic.ttl.get(), 3_600, "an hour");
        assert_eq!(semantic.capacity, 4_096);
        assert_eq!(semantic.library, None);
    }

    /// No stanza, no level: nothing is embedded and nothing is answered by meaning.
    #[test]
    fn a_file_naming_no_semantic_section_switches_the_level_off() {
        assert!(Config::parse(MINIMAL).unwrap().semantic.is_none());
    }

    /// The number that decides when one caller is handed another's answer. Outside
    /// the unit it answers everything from the cache or nothing, and learning that
    /// after the weights have loaded is the wrong place to learn it.
    #[test]
    fn rejects_a_semantic_nearness_outside_its_range() {
        let bad = |nearness: &str| {
            let text = format!(
                "{FULL}\n[semantic]\nartifact = \"/var/lib/ip/embed\"\nnearness = {nearness}\n"
            );
            Config::parse(&text)
        };
        assert!(matches!(
            bad("1.5"),
            Err(ConfigError::SemanticNearness { value }) if (value - 1.5).abs() < f32::EPSILON
        ));
        assert!(matches!(
            bad("-0.1"),
            Err(ConfigError::SemanticNearness { .. })
        ));
        // Both ends are meaningful: one admits only an exact meaning, zero admits
        // whatever the index finds.
        assert!(bad("0.0").is_ok());
        assert!(bad("1.0").is_ok());
    }

    #[test]
    fn rejects_a_semantic_index_that_holds_nothing_or_too_much() {
        let bad = |capacity: usize| {
            let text = format!(
                "{FULL}\n[semantic]\nartifact = \"/var/lib/ip/embed\"\nnearness = 0.97\ncapacity = {capacity}\n"
            );
            Config::parse(&text)
        };
        assert!(matches!(
            bad(0),
            Err(ConfigError::SemanticCapacity { value: 0, .. })
        ));
        assert!(matches!(
            bad(MOST_HELD + 1),
            Err(ConfigError::SemanticCapacity { limit, .. }) if limit == MOST_HELD
        ));
        assert!(bad(1).is_ok(), "one is a bound, if a pointless one");
        assert!(bad(MOST_HELD).is_ok(), "the bound itself is allowed");
    }

    #[test]
    fn rejects_a_semantic_section_naming_no_artifact() {
        let text = format!("{FULL}\n[semantic]\nnearness = 0.97\n");
        assert!(matches!(Config::parse(&text), Err(ConfigError::Parse(_))));
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

    /// Said nothing about which secrets are in force, so the ones that confirm
    /// themselves are. Those are the three that can be switched on without first
    /// measuring what they cost, which is why they and not the others are the default.
    #[test]
    fn a_filter_that_names_nothing_takes_the_ones_that_check_themselves() {
        let filter: FilterConfig = toml::from_str("").unwrap();
        let named: Vec<String> = filter
            .secrets()
            .unwrap()
            .iter()
            .map(|one| one.kind.as_str().to_owned())
            .collect();
        assert_eq!(named, vec!["iban", "prc-id", "credit-card"]);
        assert_eq!(filter.window, ip_filter::WINDOW);
    }

    #[test]
    fn a_filter_naming_shipped_secrets_takes_those_and_no_others() {
        let filter: FilterConfig = toml::from_str(r#"shipped = ["email"]"#).unwrap();
        let secrets = filter.secrets().unwrap();
        assert_eq!(secrets.len(), 1);
        assert_eq!(secrets[0].kind.as_str(), "email");
    }

    /// An operator's own are added to the shipped ones, never instead of them: the
    /// floor is a floor.
    #[test]
    fn an_operators_own_secrets_are_added_to_the_shipped_ones() {
        let filter: FilterConfig = toml::from_str(
            r#"
            shipped = ["email"]

            [[secret]]
            kind = "corp-token"
            prefix = "MYCORP-"
            "#,
        )
        .unwrap();
        let named: Vec<String> = filter
            .secrets()
            .unwrap()
            .iter()
            .map(|one| one.kind.as_str().to_owned())
            .collect();
        assert_eq!(named, vec!["email", "corp-token"]);
    }

    /// Taking none of the shipped ones is a thing an operator may mean.
    #[test]
    fn a_filter_may_take_none_of_the_shipped_secrets() {
        let filter: FilterConfig = toml::from_str(
            r#"
            shipped = []

            [[secret]]
            kind = "corp-token"
            prefix = "MYCORP-"
            "#,
        )
        .unwrap();
        let secrets = filter.secrets().unwrap();
        assert_eq!(secrets.len(), 1);
        assert_eq!(secrets[0].kind.as_str(), "corp-token");
    }

    /// Refusing beats running without it: an operator who asked for a filter and
    /// silently got none is worse off than one whose gateway will not start.
    #[test]
    fn a_filter_naming_a_secret_that_does_not_ship_is_refused_by_name() {
        let filter: FilterConfig = toml::from_str(r#"shipped = ["social-security"]"#).unwrap();
        let said = filter.secrets().unwrap_err().to_string();
        assert!(said.contains("social-security"), "{said}");
        assert!(
            said.contains("credit-card"),
            "it must say what is carried: {said}"
        );
    }

    #[test]
    fn a_filter_whose_own_secret_will_not_compile_is_refused() {
        let filter: FilterConfig = toml::from_str(
            r#"
            [[secret]]
            kind = "broken"
            regexp = "([unclosed"
            "#,
        )
        .unwrap();
        let secrets = filter.secrets().unwrap();
        let said = ip_filter::Detectors::of(&secrets).unwrap_err().to_string();
        assert!(said.contains("broken"), "{said}");
    }

    #[test]
    fn a_window_an_operator_sets_is_the_window_they_get() {
        let filter: FilterConfig = toml::from_str("window = 0").unwrap();
        assert_eq!(filter.window, 0);
    }

    #[test]
    fn a_configuration_with_no_filter_section_has_none() {
        let config = Config::parse(FULL).unwrap();
        assert!(config.filter.is_none());
    }

    const WITH_TENANTS: &str = r#"
        shipped = ["email"]

        [[secret]]
        kind = "corp-token"
        prefix = "MYCORP-"

        [[tenant]]
        tenant = "acme"
        shipped = ["credit-card"]

        [[tenant.secret]]
        kind = "acme-id"
        prefix = "ACME-"

        [[tenant]]
        tenant = "quiet"
        "#;

    fn named(raw: &str) -> ip_core::TenantId {
        ip_core::TenantId::new(raw).unwrap()
    }

    fn kinds_for(filter: &FilterConfig, tenant: &str) -> Vec<String> {
        filter
            .secrets_for(&named(tenant))
            .unwrap()
            .iter()
            .map(|one| one.kind.as_str().to_owned())
            .collect()
    }

    #[test]
    fn a_tenants_own_secrets_come_after_the_floors() {
        let filter: FilterConfig = toml::from_str(WITH_TENANTS).unwrap();
        assert_eq!(
            kinds_for(&filter, "acme"),
            vec!["email", "corp-token", "credit-card", "acme-id"]
        );
    }

    /// The property that makes a floor a floor. A tenant that asks for nothing is
    /// scanned for everything the floor names, and there is no way to write a tenant
    /// block that takes anything out of it.
    #[test]
    fn a_tenant_that_asks_for_nothing_still_gets_the_whole_floor() {
        let filter: FilterConfig = toml::from_str(WITH_TENANTS).unwrap();
        let floor = kinds_for(&filter, "nobody-configured-me");
        assert_eq!(floor, vec!["email", "corp-token"]);
        assert_eq!(
            kinds_for(&filter, "quiet"),
            floor,
            "a tenant block naming nothing narrows nothing"
        );
    }

    /// Every tenant is scanned for at least what the floor names, whatever they wrote.
    #[test]
    fn no_tenant_is_scanned_for_less_than_the_floor() {
        let filter: FilterConfig = toml::from_str(WITH_TENANTS).unwrap();
        let floor = kinds_for(&filter, "nobody-configured-me");
        for tenant in ["acme", "quiet", "someone-else"] {
            let theirs = kinds_for(&filter, tenant);
            for kind in &floor {
                assert!(
                    theirs.contains(kind),
                    "{tenant} is not scanned for {kind}, which the floor names"
                );
            }
        }
    }

    #[test]
    fn a_tenant_naming_a_secret_that_does_not_ship_is_refused() {
        let filter: FilterConfig = toml::from_str(
            r#"
            [[tenant]]
            tenant = "acme"
            shipped = ["social-security"]
            "#,
        )
        .unwrap();
        let said = filter.secrets_for(&named("acme")).unwrap_err().to_string();
        assert!(said.contains("social-security"), "{said}");
    }

    #[test]
    fn the_tenants_that_asked_for_something_are_named() {
        let filter: FilterConfig = toml::from_str(WITH_TENANTS).unwrap();
        let named: Vec<&str> = filter
            .named_tenants()
            .into_iter()
            .map(ip_core::TenantId::as_str)
            .collect();
        assert_eq!(named, vec!["acme", "quiet"]);
    }
}
