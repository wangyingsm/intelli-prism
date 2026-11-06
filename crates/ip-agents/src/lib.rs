//! Agent templates.
//!
//! A template is what a system agent is before anything runs one: the [`AgentId`] an operator
//! spells, and — as the rest of this crate lands — the sandbox it would see and the configuration
//! it would run under. A template is stored, served and edited; initializing one is somebody
//! else's job.
//!
//! Nothing here runs an agent, opens a sandbox, calls a tool or touches a network. That is the
//! point of the crate being its own: an agent is not a gateway primitive, nothing on the request
//! path reads a template, and what a template may say can therefore be argued with in a unit test
//! rather than through a running agent.
//!
//! It depends on no other crate of this workspace, `ip-core` included.

#![forbid(unsafe_code)]

pub mod error;
pub mod id;

pub use error::AgentError;
pub use id::AgentId;
