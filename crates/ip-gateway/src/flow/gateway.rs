//! The gateway itself: the rules in force, and one request carried through every stage.

use arc_swap::ArcSwap;
use http::header::CONTENT_TYPE;
use http::{Request, Response};
use ip_auth::Authority;
use ip_core::{Protocol, TraceId, TurnId};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{Instrument, Span, field};

use super::Flow;
use super::followed::{
    EGRESS_REQUEST, EGRESS_RESPONSE, Followed, INGRESS_REQUEST, INGRESS_RESPONSE, stage_span,
};
use super::headers::set_length;
use crate::body::{GatewayBody, from_bytes};
use crate::cache::{CachedResponse, ResponseCache};
use crate::error::GatewayError;
use crate::processor::{ChainSource, FixedChains, ProcessorChain};
use crate::stage::{BodyProcessed, ResponseBodyProcessed};
use crate::table::RoutingTable;
use crate::upstream::Upstream;

/// What the dataflow needs about a request beyond the request itself.
pub struct RequestContext {
    /// Who the caller is, with everything granted to it.
    pub authority: Authority,
    /// The scheme the request arrived over.
    pub protocol: Protocol,
    /// Where the gateway listens, supplying the port a `Host` header omits.
    pub listen: SocketAddr,
    /// What this request is followed by, drawn at the edge and never taken from a caller.
    pub trace: TraceId,
    /// The chat turn it belongs to, when the caller marked one.
    pub turn: Option<TurnId>,
}

/// The request dataflow: the stages of `DESIGN.md` run in order.
///
/// Authentication happens before a request reaches here, so the flow starts at
/// authorization and carries the identity it was handed.
pub struct Gateway {
    /// What a request is routed and processed by, replaced whole when the rules change.
    routing: ArcSwap<Routing>,
    upstream: Arc<dyn Upstream>,
    responses: Option<ResponseCache>,
}

/// The rules in force: the table a request is routed by and the chains it is processed by.
///
/// The two are held together and replaced together, so no request is ever routed by one set
/// of rules and processed by another.
struct Routing {
    table: Arc<RoutingTable>,
    chains: Arc<dyn ChainSource>,
}

impl Gateway {
    /// Builds the flow around a routing table, one plugin chain every request runs, and an
    /// upstream.
    pub fn new(
        table: RoutingTable,
        processors: ProcessorChain,
        upstream: Arc<dyn Upstream>,
    ) -> Self {
        Self::with_chains(table, Arc::new(FixedChains::new(processors)), upstream)
    }

    /// Builds the flow around a routing table, a source of each request's plugin chains,
    /// and an upstream.
    pub fn with_chains(
        table: RoutingTable,
        chains: Arc<dyn ChainSource>,
        upstream: Arc<dyn Upstream>,
    ) -> Self {
        Self {
            routing: ArcSwap::from_pointee(Routing {
                table: Arc::new(table),
                chains,
            }),
            upstream,
            responses: None,
        }
    }

    /// Puts a new table and new chains in force, for every request that starts after this.
    ///
    /// Readers never wait for this: a request in flight finishes on the rules it started
    /// with, and the next one picks up the new ones.
    pub fn replace(&self, table: RoutingTable, chains: Arc<dyn ChainSource>) {
        self.routing.store(Arc::new(Routing {
            table: Arc::new(table),
            chains,
        }));
    }

    /// Answers a request the cache already holds an answer for out of `responses`.
    pub fn caching(mut self, responses: ResponseCache) -> Self {
        self.responses = Some(responses);
        self
    }

    /// The rules this flow routes by, as they stand now.
    pub fn table(&self) -> Arc<RoutingTable> {
        Arc::clone(&self.routing.load().table)
    }

    /// Carries one request through every stage, in the order the types allow.
    ///
    /// Each stage runs in a span of its own, all under the trace the request arrived with, so
    /// what a request did is read back from one place.
    pub async fn handle(
        &self,
        context: RequestContext,
        request: Request<GatewayBody>,
    ) -> Result<Response<GatewayBody>, GatewayError> {
        // One load, so the request is routed and processed by the same rules throughout.
        let routing = self.routing.load();
        let followed = Followed::of(&context);
        let taking_in = stage_span!(INGRESS_REQUEST, followed);

        let taken = async {
            let authorized = Flow::received(context, request).authorize(&routing.table)?;
            let api = authorized.resolution().rule().api.clone();
            Span::current().record("api", field::display(&api));
            let chains = routing
                .chains
                .chains_for(authorized.context().authority.identity(), &api);
            let processed = authorized
                .process_headers(chains.request_headers())
                .await?
                .process_body(&chains)
                .await?;
            Ok::<_, GatewayError>((processed, chains, api))
        }
        .instrument(taking_in)
        .await;
        let (mut processed, chains, api) = taken?;
        let followed = followed.serving(api);

        let Some(responses) = &self.responses else {
            let done = self.upstream_answer(processed, &chains, &followed).await?;
            let answered = stage_span!(EGRESS_RESPONSE, followed);
            answered.record("hit", false);
            return async { done.into_response() }.instrument(answered).await;
        };
        // A hit answers here, spending no tokens and running no response processor, as designed.
        let key = processed.response_key().await?;
        match responses.get(&key).await {
            Ok(Some(hit)) => {
                let answered = stage_span!(EGRESS_RESPONSE, followed);
                answered.record("hit", true);
                return Ok(answered.in_scope(|| answer_with(hit)));
            }
            Ok(None) => {}
            // A cache that cannot answer costs a round trip upstream, never the request itself.
            Err(error) => tracing::warn!(%error, "could not read the response cache"),
        }
        let mut done = self.upstream_answer(processed, &chains, &followed).await?;
        if let Some((answer, ttl)) = done.cacheable(responses.ttl()).await?
            && let Err(error) = responses.put(&key, &answer, ttl).await
        {
            tracing::warn!(%error, "could not keep a response in the cache");
        }
        let answered = stage_span!(EGRESS_RESPONSE, followed);
        answered.record("hit", false);
        async { done.into_response() }.instrument(answered).await
    }

    /// Sends the request on and reads the answer back, each in its own span.
    async fn upstream_answer(
        &self,
        processed: Flow<BodyProcessed>,
        chains: &Arc<ProcessorChain>,
        followed: &Followed,
    ) -> Result<Flow<ResponseBodyProcessed>, GatewayError> {
        let forwarded = processed
            .forward(self.upstream.as_ref())
            .instrument(stage_span!(EGRESS_REQUEST, followed))
            .await?;
        async {
            forwarded
                .process_response_headers(chains.response_headers())
                .await?
                .process_response_body(chains)
                .await
        }
        .instrument(stage_span!(INGRESS_RESPONSE, followed))
        .await
    }
}

/// Answers from what the cache held, without any stage after the request body running.
fn answer_with(hit: CachedResponse) -> Response<GatewayBody> {
    let mut response = Response::new(from_bytes(hit.body().clone()));
    if let Some(content_type) = hit.content_type() {
        response
            .headers_mut()
            .insert(CONTENT_TYPE, content_type.clone());
    }
    set_length(response.headers_mut(), hit.body().len());
    response
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use http::StatusCode;
    use http::header::CONTENT_LENGTH;

    use bytes::Bytes;
    use std::collections::HashMap;
    use std::future::Future;
    use std::sync::Mutex;

    use ip_cache::Ttl;

    #[tokio::test]
    async fn a_repeated_request_is_answered_without_the_upstream() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = caching_gateway(upstream.clone());
        let first = gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(body_of(first).await, "pong");
        let second = gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(body_of(second).await, "pong");
        assert_eq!(
            upstream.seen.lock().unwrap().len(),
            1,
            "the upstream was called twice"
        );
    }

    #[tokio::test]
    async fn a_response_that_refuses_to_be_stored_is_not_kept() {
        let upstream =
            FakeUpstream::answering_with_headers("pong", vec![("cache-control", "no-store")]);
        let gateway = caching_gateway(upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(
            upstream.seen.lock().unwrap().len(),
            2,
            "an upstream that said no-store was cached anyway"
        );
    }

    #[tokio::test]
    async fn a_max_age_shorter_than_the_default_expires_the_answer_sooner() {
        let upstream =
            FakeUpstream::answering_with_headers("pong", vec![("cache-control", "max-age=1")]);
        let cache = Arc::new(ip_cache::SledCache::temporary().unwrap());
        let gateway = Gateway::new(table(), ProcessorChain::new(), upstream.clone())
            .caching(ResponseCache::new(cache, Ttl::seconds(600).unwrap()));
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(upstream.seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_max_age_longer_than_the_default_keeps_the_answer_longer() {
        let upstream =
            FakeUpstream::answering_with_headers("pong", vec![("cache-control", "max-age=600")]);
        let cache = Arc::new(ip_cache::SledCache::temporary().unwrap());
        let gateway = Gateway::new(table(), ProcessorChain::new(), upstream.clone())
            .caching(ResponseCache::new(cache, Ttl::seconds(1).unwrap()));
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(upstream.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_response_header_plugin_may_still_decide_what_is_kept() {
        let upstream = FakeUpstream::answering("pong");
        let cache = Arc::new(ip_cache::SledCache::temporary().unwrap());
        let processors =
            ProcessorChain::new().with_response_header(SetHeader::new("cache-control", "no-store"));
        let gateway = Gateway::new(table(), processors, upstream.clone())
            .caching(ResponseCache::new(cache, Ttl::seconds(600).unwrap()));
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(upstream.seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_hit_runs_no_response_processor() {
        let upstream = FakeUpstream::answering("pong");
        let cache = Arc::new(ip_cache::SledCache::temporary().unwrap());
        let processors =
            ProcessorChain::new().with_response_header(SetHeader::new("x-processed", "yes"));
        let gateway = Gateway::new(table(), processors, upstream.clone())
            .caching(ResponseCache::new(cache, Ttl::seconds(60).unwrap()));
        let first = gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(first.headers()["x-processed"], "yes");
        let hit = gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert!(
            hit.headers().get("x-processed").is_none(),
            "a hit ran the response header chain"
        );
    }

    #[tokio::test]
    async fn a_cached_answer_carries_the_content_type_it_was_kept_with() {
        let upstream =
            FakeUpstream::answering_with_headers("{}", vec![("content-type", "application/json")]);
        let gateway = caching_gateway(upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        let hit = gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(hit.headers()[CONTENT_TYPE], "application/json");
        assert_eq!(hit.headers()[CONTENT_LENGTH], "2");
        assert_eq!(body_of(hit).await, "{}");
    }

    #[tokio::test]
    async fn another_body_is_not_answered_from_the_cache() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = caching_gateway(upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        gateway
            .handle(context(granted()), request("/anthropic", "ask again"))
            .await
            .unwrap();
        assert_eq!(upstream.seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn another_tenant_is_not_answered_from_the_cache() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = caching_gateway(upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        gateway
            .handle(context(other_tenant()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(
            upstream.seen.lock().unwrap().len(),
            2,
            "one tenant read another's answer"
        );
    }

    #[tokio::test]
    async fn the_request_still_reaches_the_upstream_whole_when_it_is_keyed() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = caching_gateway(upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(upstream.last().body, Bytes::from_static(b"ask"));
    }

    #[tokio::test]
    async fn a_response_that_failed_is_not_kept() {
        let upstream = FakeUpstream::answering_with(StatusCode::INTERNAL_SERVER_ERROR, "boom");
        let gateway = caching_gateway(upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(upstream.seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn an_event_stream_is_not_kept() {
        let upstream = FakeUpstream::answering_with_headers(
            "data: one\n\n",
            vec![("content-type", "text/event-stream")],
        );
        let gateway = caching_gateway(upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        gateway
            .handle(context(granted()), request("/anthropic", "ask"))
            .await
            .unwrap();
        assert_eq!(upstream.seen.lock().unwrap().len(), 2);
    }

    /// One span, by the id it was opened under, its name and the fields it ended up carrying.
    type Recorded = (tracing::span::Id, String, HashMap<String, String>);

    /// Every span that was opened.
    #[derive(Clone, Default)]
    struct Opened(Arc<Mutex<Vec<Recorded>>>);

    impl Opened {
        /// The fields the span of this name carries, or nothing when none was opened.
        fn span(&self, name: &str) -> Option<HashMap<String, String>> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .find(|(_, opened, _)| opened == name)
                .map(|(_, _, fields)| fields.clone())
        }

        /// The name of every span that was opened, in the order they were.
        fn names(&self) -> Vec<String> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|(_, name, _)| name.clone())
                .collect()
        }
    }

    impl<S> tracing_subscriber::Layer<S> for Opened
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            id: &tracing::span::Id,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = HashMap::new();
            attrs.record(&mut Wrote(&mut fields));
            self.0
                .lock()
                .unwrap()
                .push((id.clone(), attrs.metadata().name().to_owned(), fields));
        }

        fn on_record(
            &self,
            id: &tracing::span::Id,
            values: &tracing::span::Record<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut opened = self.0.lock().unwrap();
            if let Some((_, _, fields)) = opened.iter_mut().find(|(held, _, _)| held == id) {
                values.record(&mut Wrote(fields));
            }
        }
    }

    /// Writes down what a span carries, whichever type it carries it as.
    struct Wrote<'a>(&'a mut HashMap<String, String>);

    impl tracing::field::Visit for Wrote<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.insert(
                field.name().to_owned(),
                format!("{value:?}").replace('"', ""),
            );
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.insert(field.name().to_owned(), value.to_owned());
        }

        fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
            self.0.insert(field.name().to_owned(), value.to_string());
        }
    }

    /// Runs `work` to completion with every span it opens written down.
    fn watching<F: Future>(work: F) -> (F::Output, Opened) {
        use tracing_subscriber::layer::SubscriberExt;

        let opened = Opened::default();
        let subscriber = tracing_subscriber::registry().with(opened.clone());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let answered = tracing::subscriber::with_default(subscriber, || runtime.block_on(work));
        (answered, opened)
    }

    #[test]
    fn every_stage_opens_a_span_under_the_trace_the_request_arrived_with() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(table(), ProcessorChain::new(), upstream);
        let context = context(granted());
        let trace = context.trace.to_hex();

        let (answered, opened) = watching(async move {
            gateway
                .handle(context, request("/anthropic/messages", "ping"))
                .await
        });
        assert!(answered.is_ok());

        let names = opened.names();
        for stage in [
            INGRESS_REQUEST,
            EGRESS_REQUEST,
            INGRESS_RESPONSE,
            EGRESS_RESPONSE,
        ] {
            assert!(names.contains(&stage.to_owned()), "{stage} opened no span");
            let fields = opened.span(stage).unwrap();
            assert_eq!(fields["trace"], trace, "{stage} was followed elsewhere");
            assert_eq!(fields["tenant"], tenant().to_string());
            assert_eq!(fields["user"], user().to_string());
        }
        assert_eq!(
            opened.span(INGRESS_REQUEST).unwrap()["api"],
            api().to_string()
        );
        assert_eq!(opened.span(EGRESS_RESPONSE).unwrap()["hit"], "false");
    }

    #[test]
    fn an_answer_out_of_the_cache_says_so_on_the_span_that_sends_it() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = caching_gateway(upstream);
        let (_, _) = watching(async {
            gateway
                .handle(context(granted()), request("/anthropic/messages", "ping"))
                .await
                .unwrap();
        });

        let (_, opened) = watching(async {
            gateway
                .handle(context(granted()), request("/anthropic/messages", "ping"))
                .await
                .unwrap();
        });
        assert_eq!(opened.span(EGRESS_RESPONSE).unwrap()["hit"], "true");
        assert!(
            opened.span(EGRESS_REQUEST).is_none(),
            "an answer out of the cache went to the upstream"
        );
    }
}
