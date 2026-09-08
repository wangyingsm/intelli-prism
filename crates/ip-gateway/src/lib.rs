//! The proxy gateway: routing and the request dataflow.

pub mod error;
pub mod table;

pub use error::RouteError;
pub use table::{Resolution, RoutingTable, request_key};
