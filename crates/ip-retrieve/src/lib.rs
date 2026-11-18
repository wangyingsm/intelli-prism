//! What an agent may look something up in.
//!
//! A run's facts arrive in its input or sit in its prompt; a *retriever* is how a node asks for
//! one. This crate holds what a retriever **is** — the id a tool address names it by, where its
//! backend answers, the credential it requires, which of the five tools it offers, and what a
//! look-up may filter on — and nothing about how to reach one. The backends live where the wire
//! formats already do: `ip-drsg` speaks to a plane, `ip-qdrant` to a collection, and each
//! implements [`Retrieval`], which is declared here for that reason.
//!
//! It is this crate rather than either of those because of who needs these types:
//! `ip-storage` stores a retriever and depends on neither backend, and `ip-run` declares a
//! retriever's tools to a model without depending on either. The split is the one
//! `ip_gateway::embedder::Embedder` already makes for the inference runtime — a trait where the
//! work is described, the implementation where the dependency lives, and the binary wiring them
//! together.
//!
//! **A retriever is a tenant's, not a deployment's**, which is the one place this departs from
//! how an MCP server is managed. A server is an endpoint, and one id naming one server for
//! everybody is right for an endpoint; a retriever holds a tenant's own documents, and tenant B
//! must not reach tenant A's handbook by guessing an id. The owner lives beside the row in
//! `ip-storage`, as a template's does.
//!
//! **The one credential here is shaped so it cannot leak by accident**: [`RetrieverKey`] has no
//! `Serialize`, prints nothing in `Debug`, and zeroizes on drop, so neither [`Retriever`] nor
//! anything holding one can be serialized into a response without that being written on purpose.

#![forbid(unsafe_code)]

mod declare;
mod error;
mod id;
mod meta;
mod retriever;
mod source;
mod spelling;
mod tool;

pub use declare::{Declared, Direction, LookUp, Looked, Parsed, Retrieval};
pub use error::RetrieveError;
pub use id::RetrieverId;
pub use meta::{MetaField, MetaNote, MetaSchema};
pub use retriever::{
    MaxHits, Retriever, RetrieverAuth, RetrieverEndpoint, RetrieverKey, RetrieverName,
    RetrieverParts,
};
pub use source::{CollectionName, NodeLabel, PlaneName, Source, VectorName, VectorProperty};
pub use tool::{RetrieverTool, RetrieverTools};
