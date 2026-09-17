//! Cache traits and the backends that satisfy them.

pub mod cache;
pub mod error;
pub mod key;
#[cfg(feature = "standalone-cache")]
pub mod standalone;
#[cfg(all(test, feature = "standalone-cache"))]
mod suite;
pub mod ttl;

pub use cache::Cache;
pub use error::CacheError;
pub use key::{CacheKey, CacheLevel};
#[cfg(feature = "standalone-cache")]
pub use standalone::SledCache;
pub use ttl::Ttl;
