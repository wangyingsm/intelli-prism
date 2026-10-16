//! One shape every provider family's conversations are read into and written out of.
//!
//! The gateway forwards bytes it does not read, and the places it does read a body have
//! each grown their own idea of what a request looks like. This crate is the one place
//! that knows: a reader and a writer per family, against a single canonical
//! [`Conversation`] and a single canonical sequence of answer [`Event`]s. Adding a
//! fourth family is one reader and one writer, not one pair per family already carried.
//!
//! Nothing here speaks http, holds a connection or knows a route. It turns bytes into a
//! shape and a shape back into bytes, which is why it can be tested with nothing else.
//!
//! Translation is best-effort: what maps is carried, what does not is recorded in
//! [`Dropped`] and left behind. Every reader and writer returns that record even where
//! it is empty, so nothing can quietly lose part of a request without being able to say
//! it did.

pub mod anchor;
pub mod answer;
pub mod anthropic;
pub mod conversation;
pub mod error;
pub mod gemini;
pub mod openai;

pub use answer::{Event, Gathered, Stop};
pub use conversation::{
    Block, Conversation, Dropped, Message, Role, Sampling, Tool, ToolCallId, ToolName,
};
pub use error::ProtocolError;
