use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use http::HeaderMap;
use ip_core::PluginOrder;

use crate::error::ProcessorError;

/// A plugin that rewrites headers.
#[async_trait]
pub trait HeaderProcessor: Send + Sync {
    /// Where this plugin sits in its chain.
    fn order(&self) -> PluginOrder;

    /// Rewrites the headers in place.
    async fn process(&self, headers: &mut HeaderMap) -> Result<(), ProcessorError>;
}

/// A plugin that rewrites a body, or one chunk of a streamed one.
#[async_trait]
pub trait BodyProcessor: Send + Sync {
    /// Where this plugin sits in its chain.
    fn order(&self) -> PluginOrder;

    /// Turns the body it was given into the body that goes on.
    async fn process(&self, body: Bytes) -> Result<Bytes, ProcessorError>;
}

/// The plugin chains of one flow, one per point the dataflow can act at.
///
/// An empty body chain is what lets a body pass through untouched, so leaving one
/// empty is the zero copy path rather than a missing feature.
#[derive(Default, Clone)]
pub struct ProcessorChain {
    request_headers: Vec<Arc<dyn HeaderProcessor>>,
    request_body: Vec<Arc<dyn BodyProcessor>>,
    response_headers: Vec<Arc<dyn HeaderProcessor>>,
    response_body: Vec<Arc<dyn BodyProcessor>>,
    response_chunk: Vec<Arc<dyn BodyProcessor>>,
}

impl ProcessorChain {
    /// A chain that changes nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a plugin to the request header chain.
    pub fn with_request_header(mut self, processor: Arc<dyn HeaderProcessor>) -> Self {
        insert_header(&mut self.request_headers, processor);
        self
    }

    /// Adds a plugin to the request body chain.
    pub fn with_request_body(mut self, processor: Arc<dyn BodyProcessor>) -> Self {
        insert_body(&mut self.request_body, processor);
        self
    }

    /// Adds a plugin to the response header chain.
    pub fn with_response_header(mut self, processor: Arc<dyn HeaderProcessor>) -> Self {
        insert_header(&mut self.response_headers, processor);
        self
    }

    /// Adds a plugin to the response body chain.
    pub fn with_response_body(mut self, processor: Arc<dyn BodyProcessor>) -> Self {
        insert_body(&mut self.response_body, processor);
        self
    }

    /// Adds a plugin to the response chunk chain, which runs per streamed chunk.
    pub fn with_response_chunk(mut self, processor: Arc<dyn BodyProcessor>) -> Self {
        insert_body(&mut self.response_chunk, processor);
        self
    }

    /// The request header chain, highest order first.
    pub fn request_headers(&self) -> &[Arc<dyn HeaderProcessor>] {
        &self.request_headers
    }

    /// The request body chain, highest order first.
    pub fn request_body(&self) -> &[Arc<dyn BodyProcessor>] {
        &self.request_body
    }

    /// The response header chain, highest order first.
    pub fn response_headers(&self) -> &[Arc<dyn HeaderProcessor>] {
        &self.response_headers
    }

    /// The response body chain, highest order first.
    pub fn response_body(&self) -> &[Arc<dyn BodyProcessor>] {
        &self.response_body
    }

    /// The response chunk chain, highest order first.
    ///
    /// Known gap: the dataflow does not run this chain yet. Running it means mapping
    /// frames as they stream rather than buffering, which lands with the plugin host.
    pub fn response_chunk(&self) -> &[Arc<dyn BodyProcessor>] {
        &self.response_chunk
    }

    /// Whether a request body may pass through without being read into memory.
    pub fn passes_request_body(&self) -> bool {
        self.request_body.is_empty()
    }

    /// Whether a response body may pass through without being read into memory.
    pub fn passes_response_body(&self) -> bool {
        self.response_body.is_empty()
    }
}

fn insert_header(chain: &mut Vec<Arc<dyn HeaderProcessor>>, processor: Arc<dyn HeaderProcessor>) {
    chain.push(processor);
    chain.sort_by_key(|processor| std::cmp::Reverse(processor.order()));
}

fn insert_body(chain: &mut Vec<Arc<dyn BodyProcessor>>, processor: Arc<dyn BodyProcessor>) {
    chain.push(processor);
    chain.sort_by_key(|processor| std::cmp::Reverse(processor.order()));
}
