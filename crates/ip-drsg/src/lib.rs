//! A JSON-RPC client for one dr-strange plane.
//!
//! A run graph is stored in a dr-strange plane rather than in this project's own tables, so the
//! graph routes reach it over the wire. This crate is that reach and nothing else: it holds the
//! JSON-RPC envelope, the six methods a graph needs, and the types that say where a plane is and
//! what it is called. What a graph *is* lives in `ip-graph`, and what a query asks for is built
//! by its caller — a plan arrives here as json and is passed through.
//!
//! One client is bound to one plane, named once in [`DrsgSettings`], because a deployment has one
//! graph store. **The plane is never a caller's argument**, so a route cannot be made to read
//! another deployment's graphs by passing a different name.
//!
//! The credential is [`ip_config::Secret`], which prints nothing, serializes not at all and
//! zeroizes on drop. This crate presents it to the store it was configured for and nowhere else,
//! and a test asserts that neither a `Debug` nor a refusal ever carries it.

#![forbid(unsafe_code)]

mod client;
mod error;
mod plane;
mod rpc;
#[cfg(any(feature = "testing", test))]
pub mod testing;

pub use client::{DrsgClient, DrsgSettings};
pub use error::DrsgError;
pub use plane::{DrsgEndpoint, PlaneName};
pub use rpc::{RpcFailure, RpcRequest, RpcResponse, VERSION};
#[cfg(any(feature = "testing", test))]
pub use testing::{Replies, Seen, SilentPlane, StubPlane};
