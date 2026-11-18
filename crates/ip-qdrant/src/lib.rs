//! A REST client for one qdrant.
//!
//! A vector database is one more wire format, so it takes a crate of its own, as `ip-mcp` does
//! for MCP and `ip-drsg` for JSON-RPC. This crate is that reach and nothing else: two calls —
//! what a collection says about its vectors, and what is nearest a query — with the types that
//! say where a store is, what it requires and what it answered.
//!
//! **It is REST over hyper rather than qdrant's own client**, for the reason every protocol in
//! this tree is: the official client brings tonic and the whole gRPC stack, two http stacks in
//! one binary, and what is used of it here is two endpoints.
//!
//! **One interface, and `/points/search` is not it.** Qdrant's current api answers a look-up at
//! `POST /collections/{c}/points/query`, with the hits at `result.points[]` rather than at
//! `result[]`; `search`, `recommend` and `discover` were dropped from its `OpenAPI` spec and from
//! its own JS client at 1.19.0. A test names that, so nobody reintroduces the older shape from
//! memory.
//!
//! **What a model may say about a look-up is the filter and nothing else.** The collection, the
//! vector and the limit come from the retriever an operator registered and from its own ceiling,
//! so there is no field on [`Query`] a filter could widen: see [`Filter`] for why the conditions
//! inside it are passed through and the envelope around them is not.
//!
//! The credential is [`ip_config::Secret`], which prints nothing, serializes not at all and
//! zeroizes on drop. This crate presents it in the `api-key` header qdrant takes it in, to the
//! store it was configured for and nowhere else, and a test asserts that neither a `Debug` nor a
//! refusal ever carries it.

#![forbid(unsafe_code)]

mod client;
mod error;
mod look;
#[cfg(any(feature = "testing", test))]
pub mod testing;

pub use client::{QdrantClient, QdrantSettings};
pub use error::QdrantError;
pub use look::{Collection, Distance, Filter, Found, PointId, Query};
#[cfg(any(feature = "testing", test))]
pub use testing::{Answers, SilentQdrant, StubQdrant};
