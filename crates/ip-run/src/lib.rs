//! Running an agent template: what one run of the loop is made of.
//!
//! An agent run is a conversation the gateway holds on a caller's behalf — the model answers,
//! asks for a tool, is given what the tool produced, and answers again — until it stops asking,
//! or the template's ceiling on turns or time is reached. This crate holds what that is made
//! of: the id it is followed under, what one [`Turn`] did, what the whole of it [`Spend`]t, why
//! it [`Stopped`], and the [`RunEvent`]s it says about itself on the way.
//!
//! It depends on the conversation and the model's own types, and on nothing of the gateway: the
//! loop is driven from above rather than from here, so these types can be built and read in a
//! test with no upstream, no store and no server.

#![forbid(unsafe_code)]

mod choice;
mod error;
mod event;
mod id;
mod turn;

pub use choice::{Pinned, Requested};
pub use error::RunError;
pub use event::RunEvent;
pub use id::RunId;
pub use turn::{Spend, Stopped, Turn, TurnNumber};
