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

    /// The jwt secret is too short to sign tokens safely.
    #[error("the jwt secret is {len} bytes, under the {min} byte minimum")]
    WeakJwtSecret { len: usize, min: usize },

    /// A temperature outside the range every upstream accepts.
    #[error("temperature {value} is outside {min} to {max}")]
    Temperature { value: f64, min: f32, max: f32 },
}
