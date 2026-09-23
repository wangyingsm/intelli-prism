//! Cache traits and the backends that satisfy them.

pub mod cache;
#[cfg(feature = "cluster-cache")]
pub mod cluster;
pub mod error;
#[cfg(all(
    feature = "testing",
    any(feature = "standalone-cache", feature = "cluster-cache")
))]
pub mod failing;
pub mod key;
pub mod limits;
#[cfg(any(feature = "standalone-cache", feature = "cluster-cache"))]
pub mod publication;
#[cfg(feature = "standalone-cache")]
pub mod standalone;
#[cfg(all(test, any(feature = "standalone-cache", feature = "cluster-cache")))]
mod suite;
pub mod ttl;

pub use cache::Cache;
#[cfg(feature = "cluster-cache")]
pub use cluster::{Prefix, RedisCache};
pub use error::CacheError;
#[cfg(all(
    feature = "testing",
    any(feature = "standalone-cache", feature = "cluster-cache")
))]
pub use failing::FailingCache;
pub use key::{CacheKey, CacheLevel};
pub use limits::{LevelLimits, MaxBytes};
#[cfg(any(feature = "standalone-cache", feature = "cluster-cache"))]
pub use publication::{CacheBackend, Publication, Published};
#[cfg(feature = "standalone-cache")]
pub use standalone::SledCache;
pub use ttl::Ttl;
