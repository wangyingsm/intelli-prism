//! Sending one node of a run graph to another member of this cluster.
//!
//! A run graph is a DAG of agent templates, and nothing about a node ties it to the machine that
//! read the graph. This crate is what carries one node's work to another member of the same
//! deployment and reads back what it said: the [`Dispatch`] that crosses the wire, the
//! [`NodeEvent`] vocabulary it answers in, and the [`ClusterClient`] that does it.
//!
//! **A dispatch carries intent, never a resolved permission.** Everything the receiving member
//! needs besides the dispatch — the template, the tools, the gateway, the prices, the caller's
//! grants — it reads from the database and the plane both machines share. So a member builds the
//! caller's authority itself rather than taking the coordinator's word for it, which is both the
//! only honest thing a wire could carry and the stronger posture of the two. [`Dispatch`]'s own
//! doc says what that rules out.
//!
//! What a member answers is the run's own [`ip_run::RunEvent`], not a second vocabulary beside
//! it, so a node that ran elsewhere streams what a node that ran here streams and a coordinator
//! has one path for both.
//!
//! The credential is [`ip_config::Secret`], which prints nothing, serializes not at all and
//! zeroizes on drop. **It is the whole of what authenticates one member to another, so a cluster
//! whose members are not on a network the deployment controls needs TLS in front of them**: this
//! client dials a member over plain http, because a member advertises a host and a port and
//! nothing about a scheme.

#![forbid(unsafe_code)]

mod client;
mod dispatch;
mod error;
mod event;
#[cfg(any(feature = "testing", test))]
pub mod testing;

pub use client::{ClusterClient, ClusterSettings, DISPATCH_PATH, Dispatched};
pub use dispatch::Dispatch;
pub use error::ClusterError;
pub use event::NodeEvent;
#[cfg(any(feature = "testing", test))]
pub use testing::{Answers, STUB_REFUSAL, STUB_SECRET, Seen, SilentMember, StubWorker};
