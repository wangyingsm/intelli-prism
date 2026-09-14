//! The wasm plugin host: compiles plugins once and runs each call inside its limits.

pub mod abi;
pub mod adapter;
pub mod chains;
pub mod error;
pub mod headers;
pub mod host;
pub mod limits;
#[cfg(test)]
mod testing;

pub use abi::Transformed;
pub use adapter::{WasmBodyProcessor, WasmHeaderProcessor};
pub use chains::PluginChains;
pub use error::{ChainError, HeaderBlockError, PluginError};
pub use headers::{decode_headers, encode_headers};
pub use host::{Invocation, PluginHost};
pub use limits::PluginLimits;
