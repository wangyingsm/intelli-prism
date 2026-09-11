//! The wasm plugin host: compiles plugins once and runs each call inside its limits.

pub mod error;
pub mod host;
pub mod limits;

pub use error::PluginError;
pub use host::{Invocation, PluginHost};
pub use limits::PluginLimits;
