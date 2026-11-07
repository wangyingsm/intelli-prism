//! What an MCP server is, before anything calls one.
//!
//! An agent's tools are reached over the Model Context Protocol, and a deployment registers the
//! servers its templates may name. This crate holds what one *is*: the id a template names it
//! by, where it answers, and the credential it requires. Speaking the protocol, listing a
//! server's tools and calling one are not here.
//!
//! It depends on no other crate in this workspace, `ip-core` included, for the reason
//! `ip-agents` does not: an MCP server is not a gateway concern, and a crate whose types are
//! this small should not make another crate's error type part of how it fails.
//!
//! **The one credential here is shaped so it cannot leak by accident**: [`McpToken`] has no
//! `Serialize`, prints nothing in `Debug`, and zeroizes on drop, so neither [`McpServer`] nor
//! anything holding one can be serialized into a response without that being written on purpose.

#![forbid(unsafe_code)]

mod error;
mod id;
mod server;

pub use error::McpError;
pub use id::McpId;
pub use server::{McpAuth, McpEndpoint, McpName, McpServer, McpToken};
