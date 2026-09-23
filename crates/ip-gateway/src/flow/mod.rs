//! The request dataflow: one request carried through the stages `DESIGN.md` names.

#[cfg(test)]
mod fixtures;
mod followed;
mod gateway;
mod headers;
mod run;
mod stages;

pub use gateway::{Gateway, RequestContext};
pub use stages::Flow;
