use std::io;
use std::path::PathBuf;

use ip_core::{ApiId, CoreError};

/// Every way configuration can fail to load.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The configuration file could not be read.
    #[error("cannot read {path:?}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// The configuration file is not valid toml, or does not match the schema.
    #[error("cannot parse configuration")]
    Parse(#[from] toml::de::Error),

    /// A value inside the file failed its own validation.
    #[error(transparent)]
    Value(#[from] CoreError),

    /// Two upstreams claim the same id, so a routing rule could not name one of them.
    #[error("upstream {id} is declared more than once")]
    DuplicateUpstream { id: ApiId },

    /// The filter names a secret this build does not ship.
    #[error("no secret ships under the name {value}; this build carries {carried}")]
    UnknownSecret {
        /// The name that was asked for.
        value: String,
        /// Every name this build does carry.
        carried: String,
    },

    /// A secret the operator declared will not compile into something that can scan.
    #[error(transparent)]
    Filter(#[from] ip_filter::FilterError),

    /// The arbiter's timeout is outside the range in which waiting on it is a delay and
    /// not a dependency.
    #[error("the arbiter's timeout of {value} seconds is outside 1 to {limit}")]
    ArbiterTimeout { value: u64, limit: u64 },

    /// The arbiter's credential names an upstream this configuration never declared.
    #[error("the arbiter names upstream {id}, which this configuration does not declare")]
    ArbiterUpstream { id: ApiId },

    /// The upstream the arbiter names carries no credential: every question would go
    /// out with nothing to present.
    #[error("the arbiter names upstream {id}, whose api_key is empty")]
    ArbiterKey { id: ApiId },

    /// The jwt secret is too short to sign tokens safely.
    #[error("the jwt secret is {len} bytes, under the {min} byte minimum")]
    WeakJwtSecret { len: usize, min: usize },

    /// The upstream's base url names no host to send to.
    #[error("upstream {id} has no host in its base url")]
    UpstreamHost {
        /// The upstream that cannot be reached.
        id: ApiId,
    },

    /// The upstream's base url names no port, and its scheme implies none.
    #[error("upstream {id} has no port in its base url")]
    UpstreamPort {
        /// The upstream that cannot be reached.
        id: ApiId,
    },

    /// A temperature outside the range every upstream accepts.
    #[error("temperature {value} is outside {min} to {max}")]
    Temperature { value: f64, min: f32, max: f32 },

    /// A difficulty confidence that is not a margin.
    #[error("difficulty confidence {value} is not between 0 and 1")]
    DifficultyConfidence {
        /// What the file asked for.
        value: f64,
    },

    /// A nearness threshold that is not one. Outside the unit it answers every
    /// question from the cache or none, and this is the number that decides when one
    /// caller is handed another's answer.
    #[error("semantic nearness {value} is not between 0 and 1")]
    SemanticNearness {
        /// What the file asked for.
        value: f32,
    },

    /// An index holding nothing, which can never hit, or more than a scan answers in
    /// any useful time.
    #[error("a semantic index holds between 1 and {limit} questions, not {value}")]
    SemanticCapacity {
        /// What the file asked for.
        value: usize,
        /// The most that will be held.
        limit: usize,
    },

    /// A trace sampling ratio that is not a share.
    #[error("sample ratio {value} is not between 0 and 1")]
    SampleRatio {
        /// What the file asked for.
        value: f64,
    },
}
