//! The proxy gateway: routing and the request dataflow.

pub mod body;
pub mod error;
pub mod flow;
pub mod hyper_upstream;
pub mod processor;
pub mod stage;
pub mod table;
pub mod upstream;

pub use body::{BoxError, GatewayBody};
pub use error::{GatewayError, GatewayErrorKind, RouteError};
pub use flow::{Gateway, RequestContext};
pub use hyper_upstream::{HyperUpstream, UpstreamSettings};
pub use processor::{
    BodyProcessor, HeaderProcessor, ProcessorChain, ProcessorError, ProcessorOrder,
};
pub use stage::Stage;
pub use table::{Resolution, RoutingTable, request_key};
pub use upstream::{Upstream, UpstreamError};
