//! The gateway itself: the rules in force, and one request carried through every stage.

use arc_swap::ArcSwap;
use http::header::CONTENT_TYPE;
use http::{Request, Response};
use ip_auth::Authority;
use ip_core::{Protocol, TraceId, TurnId};
use ip_storage::UsageStore;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tracing::{Instrument, Span, field};

use super::Flow;
use super::followed::{
    EGRESS_REQUEST, EGRESS_RESPONSE, Followed, INGRESS_REQUEST, INGRESS_RESPONSE, stage_span,
};
use super::headers::set_length;
use super::record::{Limiting, Pending, Recording};
use crate::body::{GatewayBody, from_bytes};
use crate::cache::{CachedResponse, ResponseCache};
use crate::error::{GatewayError, GatewayErrorKind};
use crate::limit::{Asking, Decision, Limiter, Limits};
use crate::processor::{ChainSource, FixedChains, ProcessorChain};
use crate::stage::{Authorized, BodyProcessed, ResponseBodyProcessed, Stage};
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
    usage: Option<Arc<dyn UsageStore>>,
    limiter: Option<Arc<Limiter>>,
    pending: Pending,
}

/// The rules in force: the table a request is routed by and the chains it is processed by.
///
/// The two are held together and replaced together, so no request is ever routed by one set
/// of rules and processed by another.
struct Routing {
    table: Arc<RoutingTable>,
    chains: Arc<dyn ChainSource>,
    limits: Limits,
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
                limits: Limits::default(),
            }),
            upstream,
            responses: None,
            usage: None,
            limiter: None,
            pending: Pending::default(),
        }
    }

    /// Puts a new table and new chains in force, for every request that starts after this.
    ///
    /// Readers never wait for this: a request in flight finishes on the rules it started
    /// with, and the next one picks up the new ones.
    pub fn replace(&self, table: RoutingTable, chains: Arc<dyn ChainSource>, limits: Limits) {
        self.routing.store(Arc::new(Routing {
            table: Arc::new(table),
            chains,
            limits,
        }));
    }

    /// Answers a request the cache already holds an answer for out of `responses`.
    pub fn caching(mut self, responses: ResponseCache) -> Self {
        self.responses = Some(responses);
        self
    }

    /// Records what every request costs, an answer out of the cache included.
    pub fn recording(mut self, usage: Arc<dyn UsageStore>) -> Self {
        self.usage = Some(usage);
        self
    }

    /// Checks every request against the limits in force, and counts what it spends.
    pub fn limiting(mut self, limiter: Arc<Limiter>) -> Self {
        self.limiter = Some(limiter);
        self
    }

    /// Puts the limits a node starts with in force, leaving its rules as they are.
    pub fn with_limits(self, limits: Limits) -> Self {
        let held = self.routing.load();
        self.routing.store(Arc::new(Routing {
            table: Arc::clone(&held.table),
            chains: Arc::clone(&held.chains),
            limits,
        }));
        self
    }

    /// Returns once every record of what this gateway answered has been written.
    ///
    /// A shutdown waits on this, so the last requests answered are recorded rather than
    /// dropped with the runtime.
    pub async fn settled(&self) {
        self.pending.settled().await;
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
        let started = Instant::now();
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
        let asking = Asking {
            tenant: followed.tenant.clone(),
            user: followed.user.clone(),
            api: api.clone(),
        };
        let followed = followed.serving(api);
        // Before the cache is asked, so an answer it holds still counts as a request made.
        self.admit(&routing.limits, &asking).await?;
        let recording = self.usage.as_ref().and_then(|store| {
            Recording::of(
                store,
                &self.pending,
                &followed,
                started,
                self.limiting_with(&routing.limits, &asking),
            )
        });

        let Some(responses) = &self.responses else {
            let done = self.upstream_answer(processed, &chains, &followed).await?;
            let answered = stage_span!(EGRESS_RESPONSE, followed);
            answered.record("hit", false);
            let sent = async { done.into_response() }.instrument(answered).await?;
            return Ok(measuring(sent, recording));
        };
        // A hit answers here, spending no tokens and running no response processor, as designed.
        let key = processed.response_key().await?;
        match responses.get(&key).await {
            Ok(Some(hit)) => {
                let answered = stage_span!(EGRESS_RESPONSE, followed);
                answered.record("hit", true);
                if let Some(recording) = recording {
                    recording.out_of_the_cache(hit.body());
                }
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
        let sent = async { done.into_response() }.instrument(answered).await?;
        Ok(measuring(sent, recording))
    }

    /// Refuses a request that has run out of what it may spend, or that cannot be counted.
    async fn admit(&self, limits: &Limits, asking: &Asking) -> Result<(), GatewayError> {
        let Some(limiter) = &self.limiter else {
            return Ok(());
        };
        let kind = match limiter.admit(limits, asking).await {
            Decision::Allowed => return Ok(()),
            Decision::Spent {
                counted,
                period,
                allowance,
                ends,
            } => GatewayErrorKind::Spent {
                counted,
                period,
                allowance,
                ends,
            },
            Decision::Unknown { detail } => GatewayErrorKind::Unmeasured { detail },
        };
        Err(GatewayError::new(Authorized::NAME, kind))
    }

    /// What the record of this request counts towards, when anything does.
    fn limiting_with(&self, limits: &Limits, asking: &Asking) -> Option<Limiting> {
        let limiter = self.limiter.as_ref()?;
        Some(Limiting::new(
            Arc::clone(limiter),
            limits.clone(),
            asking.clone(),
        ))
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

/// Measures what an answer costs as it goes out, when there is anywhere to record it.
fn measuring(
    response: Response<GatewayBody>,
    recording: Option<Recording>,
) -> Response<GatewayBody> {
    let Some(recording) = recording else {
        return response;
    };
    let (parts, body) = response.into_parts();
    Response::from_parts(parts, recording.measuring(body))
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
    use crate::limit::tests::Rows;
    use http::StatusCode;
    use http::header::CONTENT_LENGTH;
    use ip_core::{Served, TokenCount, Tokens};

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

        // Whether a span is worth opening is decided once per callsite for the whole process,
        // and a test that opened one with no subscriber in place decided it is not.
        static RECORDING: std::sync::Once = std::sync::Once::new();
        RECORDING.call_once(|| {
            let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry());
        });

        let opened = Opened::default();
        let subscriber = tracing_subscriber::registry().with(opened.clone());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let answered = tracing::subscriber::with_default(subscriber, || runtime.block_on(work));
        (answered, opened)
    }

    /// An anthropic answer, and the same answer as the events of a stream.
    const ANSWERED: &str =
        r#"{"model":"claude-opus-5","usage":{"input_tokens":120,"output_tokens":30}}"#;
    const EVENTS: [&str; 3] = [
        "event: message_start\ndata: {\"message\":{\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":120}}}\n\n",
        "event: content_block_delta\ndata: {\"delta\":{\"text\":\"hi\"}}\n\n",
        "event: message_delta\ndata: {\"usage\":{\"output_tokens\":30}}\n\n",
    ];

    #[tokio::test]
    async fn a_request_is_recorded_with_what_its_answer_said_it_cost() {
        let (recorder, mut recorded) = Ledger::new();
        let gateway = Gateway::new(
            table(),
            ProcessorChain::new(),
            FakeUpstream::answering(ANSWERED),
        )
        .recording(recorder);
        let context = context(granted());
        let trace = context.trace;
        let answered = gateway
            .handle(context, request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        assert_eq!(body_of(answered).await, ANSWERED);

        let row = recorded.recv().await.unwrap();
        assert_eq!(row.trace, trace);
        assert_eq!(row.tenant, tenant());
        assert_eq!(row.user, user());
        assert_eq!(row.api, api());
        assert_eq!(row.model.unwrap().as_str(), "claude-opus-5");
        assert_eq!(
            row.tokens,
            Tokens::new(TokenCount::new(120), TokenCount::new(30))
        );
        assert_eq!(row.served, Served::Upstream);
    }

    #[tokio::test]
    async fn a_streamed_answer_is_recorded_once_its_last_event_has_gone() {
        let (recorder, mut recorded) = Ledger::new();
        let gateway = Gateway::new(table(), ProcessorChain::new(), Arc::new(Streaming(&EVENTS)))
            .recording(recorder);
        let answered = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        assert!(recorded.try_recv().is_err(), "recorded before it was sent");

        assert_eq!(body_of(answered).await, EVENTS.concat());
        let row = recorded.recv().await.unwrap();
        assert_eq!(
            row.tokens,
            Tokens::new(TokenCount::new(120), TokenCount::new(30))
        );
        assert_eq!(row.model.unwrap().as_str(), "claude-opus-5");
        assert_eq!(row.served, Served::Upstream);
    }

    #[tokio::test]
    async fn an_answer_out_of_the_cache_is_recorded_as_spending_nothing() {
        let (recorder, mut recorded) = Ledger::new();
        let cache = Arc::new(ip_cache::SledCache::temporary().unwrap());
        let gateway = Gateway::new(
            table(),
            ProcessorChain::new(),
            FakeUpstream::answering(ANSWERED),
        )
        .caching(ResponseCache::new(cache, Ttl::seconds(60).unwrap()))
        .recording(recorder);

        let first = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        body_of(first).await;
        assert_eq!(recorded.recv().await.unwrap().served, Served::Upstream);

        let second = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        body_of(second).await;
        let hit = recorded.recv().await.unwrap();
        assert_eq!(hit.served, Served::Cache);
        assert_eq!(hit.tokens, Tokens::ZERO);
        assert_eq!(hit.model.unwrap().as_str(), "claude-opus-5");
    }

    #[tokio::test]
    async fn an_answer_that_says_nothing_about_its_cost_is_still_recorded() {
        let (recorder, mut recorded) = Ledger::new();
        let gateway = Gateway::new(
            table(),
            ProcessorChain::new(),
            FakeUpstream::answering("pong"),
        )
        .recording(recorder);
        let answered = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        body_of(answered).await;

        let row = recorded.recv().await.unwrap();
        assert_eq!(row.tokens, Tokens::ZERO);
        assert_eq!(row.model, None);
        assert_eq!(row.served, Served::Upstream);
    }

    #[tokio::test]
    async fn a_gateway_settles_only_once_every_record_is_written() {
        let (recorder, mut recorded) = Ledger::new();
        let gateway = Gateway::new(
            table(),
            ProcessorChain::new(),
            FakeUpstream::answering(ANSWERED),
        )
        .recording(recorder);
        let answered = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        body_of(answered).await;

        // The write is off the request path, so it has not run by the time the answer is sent.
        assert!(recorded.try_recv().is_err());
        gateway.settled().await;
        assert!(recorded.try_recv().is_ok(), "settled before it was written");
    }

    #[tokio::test]
    async fn a_gateway_with_nothing_to_write_settles_at_once() {
        let gateway = Gateway::new(
            table(),
            ProcessorChain::new(),
            FakeUpstream::answering(ANSWERED),
        );
        gateway.settled().await;
    }

    #[tokio::test]
    async fn an_answer_the_caller_abandons_is_recorded_all_the_same() {
        let (recorder, mut recorded) = Ledger::new();
        let gateway = Gateway::new(table(), ProcessorChain::new(), Arc::new(Streaming(&EVENTS)))
            .recording(recorder);
        let answered = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        drop(answered);

        let row = recorded.recv().await.unwrap();
        assert_eq!(row.served, Served::Upstream);
    }

    /// A gateway whose limits are checked, over counts held in memory.
    fn limited(upstream: Arc<FakeUpstream>, limits: Vec<ip_storage::Limit>) -> Gateway {
        let counters = Arc::new(crate::limit::tests::Held::default());
        let limiter = Limiter::new(counters, Rows::holding(0));
        Gateway::new(table(), ProcessorChain::new(), upstream)
            .limiting(Arc::new(limiter))
            .with_limits(Limits::new(limits))
    }

    /// One limit over the fixture tenant.
    fn allowing(
        counted: ip_core::Counted,
        period: ip_core::Period,
        allowance: u64,
    ) -> ip_storage::Limit {
        ip_storage::Limit {
            scope: ip_core::LimitScope::of_tenant(tenant()),
            counted,
            period,
            allowance: ip_core::Allowance::new(allowance).unwrap(),
            created_at: ip_core::Timestamp::now(),
        }
    }

    #[tokio::test]
    async fn a_request_past_a_rate_is_refused_with_when_to_ask_again() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = limited(
            Arc::clone(&upstream),
            vec![allowing(
                ip_core::Counted::Requests,
                ip_core::Period::Minute,
                1,
            )],
        );
        let first = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let refused = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap_err();
        assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(refused.retry_after().is_some_and(|seconds| seconds > 0));
        assert!(
            refused
                .public_reason()
                .is_some_and(|told| told.contains("requests per minute")),
            "told {:?}",
            refused.public_reason()
        );
        assert_eq!(upstream.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn what_an_answer_spends_is_counted_against_the_quota() {
        let upstream = FakeUpstream::answering(ANSWERED);
        let gateway = limited(
            upstream,
            vec![allowing(
                ip_core::Counted::Tokens,
                ip_core::Period::Month,
                100,
            )],
        );
        let (recorder, mut recorded) = Ledger::new();
        let gateway = gateway.recording(recorder);

        // The answer holds 150 tokens, which is past the 100 the month allows.
        let answered = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        body_of(answered).await;
        recorded.recv().await.unwrap();
        gateway.settled().await;

        let refused = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap_err();
        assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn a_quota_that_cannot_be_counted_stops_the_request() {
        let counters = Arc::new(crate::limit::tests::Held::default());
        counters.refusing();
        let gateway = Gateway::new(
            table(),
            ProcessorChain::new(),
            FakeUpstream::answering("pong"),
        )
        .limiting(Arc::new(Limiter::new(counters, Rows::holding(0))))
        .with_limits(Limits::new(vec![allowing(
            ip_core::Counted::Tokens,
            ip_core::Period::Month,
            100,
        )]));

        let refused = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap_err();
        assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(refused.retry_after(), None);
        assert_eq!(refused.public_reason(), None);
    }

    #[tokio::test]
    async fn an_answer_the_cache_holds_still_counts_as_a_request_made() {
        let upstream = FakeUpstream::answering("pong");
        let cache = Arc::new(ip_cache::SledCache::temporary().unwrap());
        let counters = Arc::new(crate::limit::tests::Held::default());
        let gateway = Gateway::new(table(), ProcessorChain::new(), upstream.clone())
            .caching(ResponseCache::new(cache, Ttl::seconds(60).unwrap()))
            .limiting(Arc::new(Limiter::new(counters, Rows::holding(0))))
            .with_limits(Limits::new(vec![allowing(
                ip_core::Counted::Requests,
                ip_core::Period::Minute,
                2,
            )]));

        for _ in 0..2 {
            let answered = gateway
                .handle(context(granted()), request("/anthropic/messages", "ask"))
                .await
                .unwrap();
            body_of(answered).await;
        }
        // The second was answered from the cache, and still counted.
        assert_eq!(upstream.seen.lock().unwrap().len(), 1);
        let refused = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap_err();
        assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn a_gateway_with_no_limiter_answers_whatever_arrives() {
        let gateway = Gateway::new(
            table(),
            ProcessorChain::new(),
            FakeUpstream::answering("pong"),
        )
        .with_limits(Limits::new(vec![allowing(
            ip_core::Counted::Requests,
            ip_core::Period::Minute,
            1,
        )]));
        for _ in 0..3 {
            assert!(
                gateway
                    .handle(context(granted()), request("/anthropic/messages", "ask"))
                    .await
                    .is_ok()
            );
        }
    }

    #[tokio::test]
    async fn the_limits_in_force_are_replaced_with_the_rules() {
        let gateway = limited(FakeUpstream::answering("pong"), Vec::new());
        assert!(
            gateway
                .handle(context(granted()), request("/anthropic/messages", "ask"))
                .await
                .is_ok()
        );
        gateway.replace(
            table(),
            Arc::new(FixedChains::new(ProcessorChain::new())),
            Limits::new(vec![allowing(
                ip_core::Counted::Requests,
                ip_core::Period::Minute,
                1,
            )]),
        );
        assert!(
            gateway
                .handle(context(granted()), request("/anthropic/messages", "ask"))
                .await
                .is_ok()
        );
        assert_eq!(
            gateway
                .handle(context(granted()), request("/anthropic/messages", "ask"))
                .await
                .unwrap_err()
                .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
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
