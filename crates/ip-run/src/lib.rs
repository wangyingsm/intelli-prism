//! Running an agent template: what one run of the loop is made of.
//!
//! An agent run is a conversation the gateway holds on a caller's behalf — the model answers,
//! asks for a tool, is given what the tool produced, and answers again — until it stops asking,
//! or the template's ceiling on turns or time is reached. This crate holds what that is made
//! of: the id it is followed under, what one [`Turn`] did, what the whole of it [`Spend`]t, why
//! it [`Stopped`], and the [`RunEvent`]s it says about itself on the way.
//!
//! Its types depend on the conversation and the model's own and on nothing else, so they can be
//! built and read in a test with no upstream, no store and no server. Taking a turn is the one
//! part that cannot: [`take`] asks the deployment's own gateway, so a run is routed, limited,
//! filtered, priced and recorded exactly as a proxied request is.

#![forbid(unsafe_code)]

mod ask;
mod choice;
mod error;
mod event;
#[cfg(test)]
mod fixtures;
mod id;
#[path = "loop_.rs"]
mod running;
mod shed;
mod tools;
mod turn;

pub use ask::{AsWhom, Taken, Target, take};
pub use choice::{Pinned, Requested};
pub use error::RunError;
pub use event::RunEvent;
pub use id::RunId;
pub use running::{Ran, Reporting, Running, run};
pub use shed::Toolshed;
pub use tools::{Addressed, Offered, Toolbox, servers_named};
pub use turn::{Spend, Stopped, Turn, TurnNumber};
