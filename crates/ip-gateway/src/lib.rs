//! The proxy gateway: routing and the request dataflow.

pub mod body;
pub mod error;
pub mod flow;
pub mod hyper_upstream;
pub mod processor;
mod sse;
pub mod stage;
pub mod table;
pub mod upstream;

pub use body::{BoxError, GatewayBody};
pub use error::{GatewayError, GatewayErrorKind, ProcessorError, RouteError, UpstreamError};
pub use flow::{Flow, Gateway, RequestContext};
pub use hyper_upstream::{HyperUpstream, UpstreamSettings};
pub use processor::{BodyProcessor, ChainSource, FixedChains, HeaderProcessor, ProcessorChain};
pub use stage::{
    Authorized, BodyProcessed, Forwarded, HeadersProcessed, Received, ResponseBodyProcessed,
    ResponseHeadersProcessed, Routed, Stage, StageName,
};
pub use table::{Resolution, RoutingTable, request_key};
pub use upstream::Upstream;
