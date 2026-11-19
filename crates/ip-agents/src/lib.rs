//! Agent templates.
//!
//! A template is what a system agent is before anything runs one: the [`AgentId`] an operator
//! spells, the [`AgentConfig`] it would run under, and the [`Sandbox`] it would see, held together
//! by [`AgentTemplate`]. A template is stored, served and edited; initializing one is somebody
//! else's job.
//!
//! Nothing here runs an agent, opens a sandbox, calls a tool or touches a network. That is the
//! point of the crate being its own: an agent is not a gateway primitive, nothing on the request
//! path reads a template, and what a template may say can therefore be argued with in a unit test
//! rather than through a running agent.
//!
//! It depends on no other crate of this workspace, `ip-core` included.

#![forbid(unsafe_code)]

pub mod config;
pub mod error;
pub mod id;
pub mod sandbox;
pub mod template;

pub use config::{
    AgentConfig, AgentConfigParts, AgentRole, BestEffortStrategy, ChosenApi, ChosenModel,
    ExtraPrompt, MaxTurns, Prompt, ReqTimeout, TimeLimit, ToolName, ToolSet,
};
pub use error::AgentError;
pub use id::AgentId;
pub use sandbox::{Entries, ObjectLocation, PathSegment, Sandbox, SandboxEntry};
pub use template::{AgentName, AgentTemplate};
