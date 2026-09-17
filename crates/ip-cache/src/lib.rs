//! Cache traits and the backends that satisfy them.

pub mod cache;
#[cfg(feature = "cluster-cache")]
pub mod cluster;
pub mod error;
pub mod key;
#[cfg(feature = "standalone-cache")]
pub mod standalone;
#[cfg(all(test, any(feature = "standalone-cache", feature = "cluster-cache")))]
mod suite;
pub mod ttl;

pub use cache::Cache;
#[cfg(feature = "cluster-cache")]
pub use cluster::{Prefix, RedisCache};
pub use error::CacheError;
pub use key::{CacheKey, CacheLevel};
#[cfg(feature = "standalone-cache")]
pub use standalone::SledCache;
pub use ttl::Ttl;
