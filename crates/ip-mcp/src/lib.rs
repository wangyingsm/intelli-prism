//! MCP servers: what one is, and how to talk to one.
//!
//! An agent's tools are reached over the Model Context Protocol, and a deployment registers the
//! servers its templates may name. This crate holds what one *is* — the id a template names it
//! by, where it answers, the credential it requires — and, in [`McpClient`], the JSON-RPC
//! exchange that reaches it over the streamable http transport.
//!
//! It depends on no other crate in this workspace, `ip-core` included, for the reason
//! `ip-agents` does not: an MCP server is not a gateway concern, and a crate whose types are
//! this small should not make another crate's error type part of how it fails. That holds for
//! the client too, which is why what a server offers is answered in this crate's own types and
//! turned into the gateway's where both are in scope.
//!
//! **The one credential here is shaped so it cannot leak by accident**: [`McpToken`] has no
//! `Serialize`, prints nothing in `Debug`, and zeroizes on drop, so neither [`McpServer`] nor
//! anything holding one can be serialized into a response without that being written on purpose.
//! The client presents it to the server it belongs to and nowhere else.

#![forbid(unsafe_code)]

mod client;
mod error;
mod id;
mod rpc;
mod server;
mod tool;

pub use client::{
    McpClient, McpSettings, Offered, PROTOCOL_VERSION, ServerInfo, Told, ToolsOffered,
};
pub use error::McpError;
pub use id::McpId;
pub use rpc::{RpcFailure, RpcRequest, RpcResponse};
pub use server::{McpAuth, McpEndpoint, McpName, McpServer, McpToken};
pub use tool::{DroppedTool, McpTool, McpToolName, Offering};
