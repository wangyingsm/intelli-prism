//! The gateway itself: the rules in force, and one request carried through every stage.

use arc_swap::ArcSwap;
use http::header::CONTENT_TYPE;
use http::{Request, Response};
use ip_auth::Authority;
use ip_cache::Cache;
use ip_core::{Decider, Hedge, Protocol, Replica, TraceId, TurnId};
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
use crate::arbiter::Arbiter;
use crate::body::{GatewayBody, from_bytes};
use crate::cache::{CachedResponse, ResponseCache};
use crate::classifier::Classifier;
use crate::dispatch::Dispatcher;
use crate::error::{GatewayError, GatewayErrorKind};
use crate::flow::stages::Egress;
use crate::limit::{Asking, Decision, Limiter, Limits};
use crate::processor::{ChainSource, FixedChains, ProcessorChain};
use crate::stage::{Authorized, BodyProcessed, Dispatched, ResponseBodyProcessed, Stage};
use crate::table::RoutingTable;
use crate::turns::TurnAffinity;
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
    dispatcher: Arc<Dispatcher>,
    /// Where the turns that pass through are kept to the endpoints they landed on.
    turns: Option<TurnAffinity>,
    /// The local judge of a turn's opening, when one is configured: consulted for a turn
    /// that names no class and remembers none, once.
    classifier: Option<Arc<dyn Classifier>>,
    /// The larger model asked when the judge's margin fell short, once per turn, under
    /// a deadline of its own.
    arbiter: Option<Arc<Arbiter>>,
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
            // Nothing folds what this one learns until a caller supplies one that is folded for.
            dispatcher: Arc::new(Dispatcher::new().0),
            turns: None,
            classifier: None,
            arbiter: None,
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

    /// Dispatches with this one, whose folder keeps what each endpoint takes up to date.
    pub fn dispatching(mut self, dispatcher: Arc<Dispatcher>) -> Self {
        self.dispatcher = dispatcher;
        self
    }

    /// Keeps each chat turn on the endpoint it landed on, for as long as the turn lasts, and
    /// remembers the landing in `turns`: the short-lived state is the cache's kind, and this
    /// writes through the same handle the responses do. A request that marks no turn with
    /// `x-ip-turn` is dispatched as though none of this were configured.
    pub fn sticky_turns(mut self, turns: Arc<dyn Cache>) -> Self {
        self.turns = Some(TurnAffinity::new(turns, &self.pending));
        self
    }

    /// Sends the openings the judge cannot call to a larger model, under a deadline of
    /// its own: what it answers rides the dispatch and the turn's memory like a local
    /// judgement, and a model that is slow, refusing or incoherent leaves the request
    /// unclassified.
    pub fn arbitrating(mut self, arbiter: Arc<Arbiter>) -> Self {
        self.arbiter = Some(arbiter);
        self
    }

    /// Asks this judge what a new turn's opening is, when nothing else has said: the
    /// class it names rides the dispatch and the turn's memory, and the rest of the
    /// turn costs nothing. Refused, silent or broken, it leaves a request unclassified.
    pub fn classifying(mut self, classifier: Arc<dyn Classifier>) -> Self {
        self.classifier = Some(classifier);
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
        // Read once here, where the header chain left it, so both answers — the cache's and
        // the upstream's — record the same class the request was dispatched at. Nothing
        // but the request itself has named one at this point, which is why a cache hit
        // records the request as the level that decided.
        let named = processed
            .class()
            .map(|class| Dispatched::at(class, Decider::Request))
            .unwrap_or(Dispatched::NONE);
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
            let (done, dispatched) = self
                .upstream_answer(
                    processed,
                    &routing.table,
                    &chains,
                    &followed,
                    recording.clone(),
                )
                .await?;
            let served_by = done.served().clone();
            let raced = done.raced();
            let answered = stage_span!(EGRESS_RESPONSE, followed);
            answered.record("hit", false);
            let sent = async { done.into_response() }.instrument(answered).await?;
            return Ok(measuring(sent, recording, served_by, raced, dispatched));
        };
        // A hit answers here, spending no tokens and running no response processor, as designed.
        let key = processed.response_key().await?;
        match responses.get(&key).await {
            Ok(Some(hit)) => {
                let answered = stage_span!(EGRESS_RESPONSE, followed);
                answered.record("hit", true);
                if let Some(recording) = recording {
                    recording.out_of_the_cache(hit.body(), named);
                }
                return Ok(answered.in_scope(|| answer_with(hit)));
            }
            Ok(None) => {}
            // A cache that cannot answer costs a round trip upstream, never the request itself.
            Err(error) => tracing::warn!(%error, "could not read the response cache"),
        }
        let (mut done, dispatched) = self
            .upstream_answer(
                processed,
                &routing.table,
                &chains,
                &followed,
                recording.clone(),
            )
            .await?;
        let served_by = done.served().clone();
        let raced = done.raced();
        if let Some((answer, ttl)) = done.cacheable(responses.ttl()).await?
            && let Err(error) = responses.put(&key, &answer, ttl).await
        {
            tracing::warn!(%error, "could not keep a response in the cache");
        }
        let answered = stage_span!(EGRESS_RESPONSE, followed);
        answered.record("hit", false);
        let sent = async { done.into_response() }.instrument(answered).await?;
        // The row is measured against what went upstream: a turn's remembered class is
        // the class it was dispatched at, whatever named it.
        Ok(measuring(sent, recording, served_by, raced, dispatched))
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
        table: &RoutingTable,
        chains: &Arc<ProcessorChain>,
        followed: &Followed,
        recording: Option<Recording>,
    ) -> Result<(Flow<ResponseBodyProcessed>, Dispatched), GatewayError> {
        let egress = Egress::new(self.upstream.as_ref(), &self.dispatcher, table)
            .with_turns(self.turns.as_ref())
            .with_classifier(self.classifier.clone())
            .with_arbiter(self.arbiter.clone())
            .with_recording(recording);
        let forwarded = processed
            .forward(egress)
            .instrument(stage_span!(EGRESS_REQUEST, followed))
            .await?;
        // What the turn's verdict was worth, if it was worth anything: read out of the
        // flow before the response stages take it apart, because this is the class the
        // request went upstream at, whatever its header said, and the level that named it.
        let dispatched = forwarded.dispatched();
        let response = async {
            forwarded
                .process_response_headers(chains.response_headers())
                .await?
                .process_response_body(chains)
                .await
        }
        .instrument(stage_span!(INGRESS_RESPONSE, followed))
        .await?;
        Ok((response, dispatched))
    }
}

/// Measures what an answer costs as it goes out, when there is anywhere to record it.
fn measuring(
    response: Response<GatewayBody>,
    recording: Option<Recording>,
    served_by: Replica,
    raced: Hedge,
    dispatched: Dispatched,
) -> Response<GatewayBody> {
    let Some(recording) = recording else {
        return response;
    };
    let (parts, body) = response.into_parts();
    Response::from_parts(
        parts,
        recording.measuring(body, served_by, raced, dispatched),
    )
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
    use http::HeaderValue;
    use http::StatusCode;
    use http::header::{CONTENT_LENGTH, CONTENT_TYPE};
    use ip_core::{Hedge, RouteKey, Served, TokenCount, Tokens};

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
    async fn a_recorded_request_names_the_class_it_was_dispatched_at() {
        let (recorder, mut recorded) = Ledger::new();
        let gateway = Gateway::new(
            ceiled_table(&[("hard.example.com", Some(ip_core::Difficulty::Hard))]),
            ProcessorChain::new(),
            FakeUpstream::answering("pong"),
        )
        .recording(recorder);

        let mut asked = request("/anthropic/messages", "ask");
        asked
            .headers_mut()
            .insert("x-ip-difficulty", HeaderValue::from_static("hard"));
        let answered = gateway.handle(context(granted()), asked).await.unwrap();
        body_of(answered).await;
        let row = recorded.recv().await.unwrap();
        assert_eq!(row.dispatched_class, Some(ip_core::Difficulty::Hard));

        // A request that named none records none, its row sitting beside the first.
        let answered = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        body_of(answered).await;
        let row = recorded.recv().await.unwrap();
        assert_eq!(row.dispatched_class, None);
    }

    /// The counterpart on the row of `a_header_processor_naming_the_class_is_honoured`: the
    /// row keeps what the processor set, not the nothing the request arrived with.
    #[tokio::test]
    async fn a_class_named_by_a_header_processor_is_recorded_on_the_row() {
        let (recorder, mut recorded) = Ledger::new();
        let gateway = Gateway::new(
            ceiled_table(&[
                ("silent.example.com", None),
                ("hard.example.com", Some(ip_core::Difficulty::Hard)),
            ]),
            ProcessorChain::new().with_request_header(SetHeader::new("x-ip-difficulty", "hard")),
            FakeUpstream::answering("pong"),
        )
        .recording(recorder);

        let answered = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        body_of(answered).await;
        let row = recorded.recv().await.unwrap();
        assert_eq!(row.dispatched_class, Some(ip_core::Difficulty::Hard));
    }

    /// The one a reader gets wrong: a cache answer keeps the class the request named, since
    /// the class is the request's own, while naming no endpoint, since none was reached.
    #[tokio::test]
    async fn an_answer_out_of_the_cache_records_the_class_it_named() {
        let (recorder, mut recorded) = Ledger::new();
        let cache = Arc::new(ip_cache::SledCache::temporary().unwrap());
        let gateway = Gateway::new(
            table(),
            ProcessorChain::new(),
            FakeUpstream::answering(ANSWERED),
        )
        .caching(ResponseCache::new(cache, Ttl::seconds(60).unwrap()))
        .recording(recorder);
        let hard = |mut req: http::Request<crate::body::GatewayBody>| {
            req.headers_mut()
                .insert("x-ip-difficulty", HeaderValue::from_static("hard"));
            req
        };

        let first = gateway
            .handle(
                context(granted()),
                hard(request("/anthropic/messages", "ask")),
            )
            .await
            .unwrap();
        body_of(first).await;
        assert_eq!(recorded.recv().await.unwrap().served, Served::Upstream);

        let second = gateway
            .handle(
                context(granted()),
                hard(request("/anthropic/messages", "ask")),
            )
            .await
            .unwrap();
        body_of(second).await;
        let hit = recorded.recv().await.unwrap();
        assert_eq!(hit.served, Served::Cache);
        assert_eq!(hit.served_by, None, "a cache answer reached no endpoint");
        assert_eq!(hit.dispatched_class, Some(ip_core::Difficulty::Hard));
        assert_eq!(
            hit.decided_by,
            Some(ip_core::Decider::Request),
            "who answered says nothing about who decided"
        );
    }

    /// A class that cannot be read is refused where the header chain ends, before anything
    /// is recorded: the request never reaches the recording at all.
    #[tokio::test]
    async fn a_request_refused_for_an_unreadable_class_writes_no_row() {
        let (recorder, mut recorded) = Ledger::new();
        let gateway = Gateway::new(
            table(),
            ProcessorChain::new(),
            FakeUpstream::answering("pong"),
        )
        .recording(recorder);
        let mut asked = request("/anthropic/messages", "ask");
        asked
            .headers_mut()
            .insert("x-ip-difficulty", HeaderValue::from_static("epic"));

        let refused = gateway.handle(context(granted()), asked).await.unwrap_err();
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
        gateway.settled().await;
        assert!(
            recorded.try_recv().is_err(),
            "a request refused before dispatch wrote a row anyway"
        );
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

    /// A class the gateway cannot read is refused when the headers finish, before the
    /// admission check, so the request is never counted against a limit and never reaches
    /// the upstream.
    #[tokio::test]
    async fn a_request_naming_a_class_that_cannot_be_read_costs_no_limit_count() {
        let upstream = FakeUpstream::answering("pong");
        let counters = Arc::new(crate::limit::tests::Held::default());
        let limiter = Limiter::new(counters.clone(), Rows::holding(0));
        let gateway = Gateway::new(table(), ProcessorChain::new(), upstream.clone())
            .limiting(Arc::new(limiter))
            .with_limits(Limits::new(vec![allowing(
                ip_core::Counted::Requests,
                ip_core::Period::Minute,
                100,
            )]));
        let mut asked = request("/anthropic/messages", "ask");
        asked
            .headers_mut()
            .insert("x-ip-difficulty", HeaderValue::from_static("epic"));

        let refused = gateway.handle(context(granted()), asked).await.unwrap_err();
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
        assert!(
            !counters.saw_anything(),
            "a request that could never proceed was counted against a limit"
        );
        assert!(upstream.seen.lock().unwrap().is_empty());
    }

    /// The class is read once the request header chain has run, so a processor that sets
    /// it routes the request — which is how a classifier of one's own plugs in.
    #[tokio::test]
    async fn a_header_processor_naming_the_class_is_honoured() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(
            ceiled_table(&[
                ("silent.example.com", None),
                ("hard.example.com", Some(ip_core::Difficulty::Hard)),
            ]),
            ProcessorChain::new().with_request_header(SetHeader::new("x-ip-difficulty", "hard")),
            upstream.clone(),
        );
        let answered = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        assert_eq!(answered.status(), StatusCode::OK);
        let sent = upstream.seen.lock().unwrap();
        assert_eq!(sent.len(), 1);
        // No class in the request itself: the header processor named it, and the read saw that.
        assert_eq!(sent[0].host, "hard.example.com:443");
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

    #[tokio::test]
    async fn a_recorded_request_names_the_endpoint_that_answered_it() {
        let (recorder, mut recorded) = Ledger::new();
        let gateway = Gateway::new(
            replicated_table(ip_core::Strategy::RoundRobin),
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
        let served = row.served_by.expect("an endpoint answered");
        assert_eq!(served.host.as_str(), "one.example.com");
        assert_eq!(served.port.get(), 443);
        // Not raced, so the row says one endpoint was asked.
        assert_eq!(row.raced_across, Hedge::default());

        // The next goes to the other endpoint, and the row says so.
        let answered = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        body_of(answered).await;
        let row = recorded.recv().await.unwrap();
        assert_eq!(
            row.served_by.expect("an endpoint answered").host.as_str(),
            "two.example.com"
        );
        assert_eq!(row.raced_across, Hedge::default());
    }

    #[tokio::test]
    async fn a_hedged_rule_records_how_many_arms_it_raced_across() {
        let (recorder, mut recorded) = Ledger::new();
        let gateway = Gateway::new(
            hedged_replicated_table(ip_core::Strategy::RoundRobin, Hedge::new(2).unwrap()),
            ProcessorChain::new(),
            FakeUpstream::answering("pong"),
        )
        .recording(recorder);

        // Either arm may win the race, and the row says two were asked whichever did.
        for _ in 0..2 {
            let answered = gateway
                .handle(context(granted()), request("/anthropic/messages", "ask"))
                .await
                .unwrap();
            body_of(answered).await;
            let row = recorded.recv().await.unwrap();
            assert!(row.served_by.is_some());
            assert_eq!(row.raced_across, Hedge::new(2).unwrap());
        }
    }

    #[tokio::test]
    async fn a_rule_asked_for_three_with_two_endpoints_records_two() {
        let (recorder, mut recorded) = Ledger::new();
        let gateway = Gateway::new(
            hedged_replicated_table(ip_core::Strategy::RoundRobin, Hedge::new(3).unwrap()),
            ProcessorChain::new(),
            FakeUpstream::answering("pong"),
        )
        .recording(recorder);

        let answered = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        body_of(answered).await;
        // The rule asked for three, only two stand behind it, and the row records what
        // happened rather than what was asked.
        assert_eq!(
            recorded.recv().await.unwrap().raced_across,
            Hedge::new(2).unwrap()
        );
    }

    #[tokio::test]
    async fn a_request_refused_a_race_records_one() {
        let (recorder, mut recorded) = Ledger::new();
        let gateway = Gateway::new(
            hedged_replicated_table(ip_core::Strategy::RoundRobin, Hedge::new(2).unwrap()),
            ProcessorChain::new(),
            FakeUpstream::answering("pong"),
        )
        .recording(recorder);

        let mut file = request("/anthropic/messages", "ask");
        file.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_static("multipart/form-data; boundary=x"),
        );
        gateway.handle(context(granted()), file).await.unwrap();
        assert_eq!(
            recorded.recv().await.unwrap().raced_across,
            Hedge::default()
        );

        // A body declared past the cap is refused a race the same way.
        let mut oversized = request("/anthropic/messages", "ask");
        oversized
            .headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from_static("2000000"));
        gateway.handle(context(granted()), oversized).await.unwrap();
        assert_eq!(
            recorded.recv().await.unwrap().raced_across,
            Hedge::default()
        );
    }

    #[tokio::test]
    async fn a_request_the_cache_answered_names_no_endpoint() {
        let (recorder, mut recorded) = Ledger::new();
        let cache = Arc::new(ip_cache::SledCache::temporary().unwrap());
        let gateway = Gateway::new(
            replicated_table(ip_core::Strategy::RoundRobin),
            ProcessorChain::new(),
            FakeUpstream::answering("pong"),
        )
        .caching(ResponseCache::new(cache, Ttl::seconds(60).unwrap()))
        .recording(recorder);

        for _ in 0..2 {
            let answered = gateway
                .handle(context(granted()), request("/anthropic/messages", "ask"))
                .await
                .unwrap();
            body_of(answered).await;
        }
        assert!(recorded.recv().await.unwrap().served_by.is_some());
        let hit = recorded.recv().await.unwrap();
        assert_eq!(hit.served, ip_core::Served::Cache);
        assert_eq!(hit.served_by, None);
        // An answer out of the cache was raced across one endpoint: nothing was reached.
        assert_eq!(hit.raced_across, Hedge::default());
    }

    #[tokio::test]
    async fn a_replicated_rule_sends_its_requests_round_the_endpoints() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(
            replicated_table(ip_core::Strategy::RoundRobin),
            ProcessorChain::new(),
            upstream.clone(),
        );
        for _ in 0..4 {
            gateway
                .handle(context(granted()), request("/anthropic/messages", "ask"))
                .await
                .unwrap();
        }
        let hosts: Vec<String> = upstream
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|seen| seen.host.clone())
            .collect();
        assert_eq!(
            hosts,
            [
                "one.example.com:443",
                "two.example.com:443",
                "one.example.com:443",
                "two.example.com:443"
            ]
        );
    }

    #[tokio::test]
    async fn a_turn_is_served_twice_by_the_endpoint_it_landed_on() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(
            replicated_table(ip_core::Strategy::RoundRobin),
            ProcessorChain::new(),
            upstream.clone(),
        )
        .sticky_turns(Arc::new(ip_cache::SledCache::temporary().unwrap()));

        for _ in 0..2 {
            gateway
                .handle(
                    in_turn(granted(), "turn-1"),
                    request("/anthropic/messages", "ask"),
                )
                .await
                .unwrap();
            gateway.settled().await;
        }
        let hosts = hosts_of(&upstream);
        assert_eq!(hosts.len(), 2);
        assert_eq!(
            hosts[0], hosts[1],
            "round-robin alone would have sent the second request of the turn elsewhere"
        );
    }

    #[tokio::test]
    async fn another_turn_is_dispatched_as_though_the_first_had_never_been() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(
            replicated_table(ip_core::Strategy::RoundRobin),
            ProcessorChain::new(),
            upstream.clone(),
        )
        .sticky_turns(Arc::new(ip_cache::SledCache::temporary().unwrap()));

        for turn in ["turn-1", "turn-2", "turn-1"] {
            gateway
                .handle(
                    in_turn(granted(), turn),
                    request("/anthropic/messages", "ask"),
                )
                .await
                .unwrap();
            gateway.settled().await;
        }
        let hosts = hosts_of(&upstream);
        assert_eq!(hosts.len(), 3);
        assert_ne!(
            hosts[1], hosts[0],
            "a second turn shared the first one's landing"
        );
        assert_eq!(hosts[2], hosts[0], "the first turn forgot where it landed");
    }

    #[tokio::test]
    async fn a_request_that_marks_no_turn_is_dispatched_as_it_always_was() {
        // The regression at the flow level: with the memory configured and no turn marked,
        // the rotation is bit for bit the one the same table makes with memory unconfigured.
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(
            replicated_table(ip_core::Strategy::RoundRobin),
            ProcessorChain::new(),
            upstream.clone(),
        )
        .sticky_turns(Arc::new(ip_cache::SledCache::temporary().unwrap()));
        for _ in 0..4 {
            gateway
                .handle(context(granted()), request("/anthropic/messages", "ask"))
                .await
                .unwrap();
        }
        assert_eq!(
            hosts_of(&upstream),
            [
                "one.example.com:443",
                "two.example.com:443",
                "one.example.com:443",
                "two.example.com:443"
            ]
        );
    }

    #[tokio::test]
    async fn a_turn_whose_memory_cannot_answer_is_still_served() {
        let memory =
            ip_cache::FailingCache::new(Arc::new(ip_cache::SledCache::temporary().unwrap()));
        memory.fail_now();
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(
            replicated_table(ip_core::Strategy::RoundRobin),
            ProcessorChain::new(),
            upstream.clone(),
        )
        .sticky_turns(Arc::new(memory));

        for _ in 0..2 {
            let answered = gateway
                .handle(
                    in_turn(granted(), "turn-1"),
                    request("/anthropic/messages", "ask"),
                )
                .await
                .unwrap();
            assert_eq!(answered.status(), StatusCode::OK);
            gateway.settled().await;
        }
        let hosts = hosts_of(&upstream);
        assert_ne!(
            hosts[0], hosts[1],
            "a memory that cannot answer was kept to anyway"
        );
    }

    #[tokio::test]
    async fn a_hedged_turn_races_from_where_the_turn_landed() {
        // Two of three endpoints race; the first request's first arm refuses, so the turn
        // lands on the second. The second request starts its race from that landing — not
        // from where the round-robin had reached — and still races what remains of the draw.
        let upstream = FakeUpstream::answering_each(&[
            (
                "one.example.com:443",
                Behaviour {
                    delay: std::time::Duration::ZERO,
                    status: StatusCode::OK,
                    body: "one",
                    fail: true,
                },
            ),
            (
                "two.example.com:443",
                Behaviour {
                    delay: std::time::Duration::from_millis(30),
                    status: StatusCode::OK,
                    body: "two",
                    fail: false,
                },
            ),
            (
                "three.example.com:443",
                Behaviour {
                    delay: std::time::Duration::from_millis(80),
                    status: StatusCode::OK,
                    body: "three",
                    fail: false,
                },
            ),
        ]);
        let gateway = Gateway::new(
            hedged_table(
                ip_core::Strategy::RoundRobin,
                ip_core::Hedge::new(2).unwrap(),
                &["one.example.com", "two.example.com", "three.example.com"],
            ),
            ProcessorChain::new(),
            upstream.clone(),
        )
        .sticky_turns(Arc::new(ip_cache::SledCache::temporary().unwrap()));

        for _ in 0..2 {
            let answered = gateway
                .handle(
                    in_turn(granted(), "turn-1"),
                    request("/anthropic/messages", "ask"),
                )
                .await
                .unwrap();
            assert_eq!(body_of(answered).await, "two");
            gateway.settled().await;
        }
        assert_eq!(
            hosts_of(&upstream),
            [
                "one.example.com:443",
                "two.example.com:443",
                "two.example.com:443",
                "three.example.com:443",
            ],
            "the second race neither started at the landing nor raced the rest"
        );
    }
    #[tokio::test]
    async fn an_answer_inside_the_delay_never_starts_the_second_arm() {
        let (recorder, mut recorded) = Ledger::new();
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(delayed_table(400), ProcessorChain::new(), upstream.clone())
            .recording(recorder);

        let answered = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        body_of(answered).await;
        let row = recorded.recv().await.unwrap();
        // The first arm answered well inside the wait, so the second never went out, and
        // the row says one: what left the gateway, not what the rule asked for.
        assert_eq!(hosts_of(&upstream).len(), 1);
        assert_eq!(row.raced_across, Hedge::default());
    }

    #[tokio::test]
    async fn an_arm_slower_than_the_delay_sees_the_next_one_start() {
        let (recorder, mut recorded) = Ledger::new();
        let upstream = FakeUpstream::answering_each(&[
            (
                "one.example.com:443",
                Behaviour {
                    delay: std::time::Duration::from_millis(250),
                    status: StatusCode::OK,
                    body: "one",
                    fail: false,
                },
            ),
            (
                "two.example.com:443",
                Behaviour {
                    delay: std::time::Duration::ZERO,
                    status: StatusCode::OK,
                    body: "two",
                    fail: false,
                },
            ),
        ]);
        let gateway = Gateway::new(delayed_table(40), ProcessorChain::new(), upstream.clone())
            .recording(recorder);

        let answered = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        // The first arm was still holding when the wait ran out, the second started at
        // once the wait said to, answered at once, and won.
        assert_eq!(body_of(answered).await, "two");
        let row = recorded.recv().await.unwrap();
        assert_eq!(row.raced_across, Hedge::new(2).unwrap());
        assert_eq!(hosts_of(&upstream).len(), 2);
    }

    #[tokio::test]
    async fn an_arm_that_fails_starts_the_next_without_waiting() {
        let upstream = FakeUpstream::answering_each(&[
            (
                "one.example.com:443",
                Behaviour {
                    delay: std::time::Duration::ZERO,
                    status: StatusCode::OK,
                    body: "one",
                    fail: true,
                },
            ),
            (
                "two.example.com:443",
                Behaviour {
                    delay: std::time::Duration::from_millis(30),
                    status: StatusCode::OK,
                    body: "two",
                    fail: false,
                },
            ),
        ]);
        let gateway = Gateway::new(
            delayed_table(3_000),
            ProcessorChain::new(),
            upstream.clone(),
        );

        // The wait is three seconds and the answer arrives in far less: a lost arm is
        // information, not a reason to wait. The margin is wide enough for a loaded
        // machine and narrow enough to show the wait was never kept.
        let started = std::time::Instant::now();
        let answered = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        assert_eq!(body_of(answered).await, "two");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(1_500),
            "a lost arm made the race wait out its delay"
        );
    }

    #[tokio::test]
    async fn a_default_delay_still_starts_every_arm_at_once() {
        let (recorder, mut recorded) = Ledger::new();
        // No delay in the rule at all: the regression guard for the path nearly all
        // traffic takes, which must behave exactly as it did before waits existed.
        let gateway = Gateway::new(
            hedged_replicated_table(ip_core::Strategy::RoundRobin, Hedge::new(2).unwrap()),
            ProcessorChain::new(),
            FakeUpstream::answering("pong"),
        )
        .recording(recorder);

        let answered = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        body_of(answered).await;
        // Both arms went out together — the row says two whether or not the first won.
        assert_eq!(
            recorded.recv().await.unwrap().raced_across,
            Hedge::new(2).unwrap()
        );
    }

    #[tokio::test]
    async fn an_arm_that_never_starts_gives_its_load_back() {
        let upstream = FakeUpstream::answering_each(&[
            (
                "one.example.com:443",
                Behaviour {
                    delay: std::time::Duration::from_millis(30),
                    status: StatusCode::OK,
                    body: "one",
                    fail: false,
                },
            ),
            (
                "two.example.com:443",
                Behaviour {
                    delay: std::time::Duration::ZERO,
                    status: StatusCode::OK,
                    body: "two",
                    fail: false,
                },
            ),
        ]);
        let table = delayed_table(400);
        let gateway = Gateway::new(table.clone(), ProcessorChain::new(), upstream);

        // The first arm answers inside the wait, so the second is chosen, held for the
        // whole wait, and never started. What it claimed at the choosing is what it owes
        // back at the abandoning, and the plan's own counters are the only place that
        // shows it: round-robin rotates the choosing, so no traffic decision consults a
        // score, and a claim kept by an arm that never went out would sit on its
        // endpoint unseen.
        let answered = gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        body_of(answered).await;
        let key = RouteKey::new(endpoint(
            Protocol::Http,
            "gateway.local",
            8080,
            "/anthropic",
        ));
        let resolution = table.resolve(&key).expect("the rule stands");
        assert_eq!(
            resolution.plan().in_flight(),
            vec![0; 2],
            "an arm that never went out kept its claim"
        );
    }

    #[tokio::test]
    async fn a_harder_class_in_the_same_turn_moves_the_turn() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(
            ceiled_table(&[
                ("easy.example.com", Some(ip_core::Difficulty::Easy)),
                ("hard.example.com", Some(ip_core::Difficulty::Hard)),
            ]),
            ProcessorChain::new(),
            upstream.clone(),
        )
        .sticky_turns(Arc::new(ip_cache::SledCache::temporary().unwrap()));

        for class in ["easy", "hard", "easy"] {
            let mut asked = request("/anthropic/messages", "ask");
            asked
                .headers_mut()
                .insert("x-ip-difficulty", HeaderValue::from_static(class));
            gateway
                .handle(in_turn(granted(), "turn-1"), asked)
                .await
                .unwrap();
            gateway.settled().await;
        }
        // The turn leaves an endpoint that cannot take the harder ask, moves to one that
        // can, and stays there when the easier one returns: §429 asks only whether the
        // endpoint can serve what is asked, never whether a cheaper one could serve less.
        assert_eq!(
            hosts_of(&upstream),
            [
                "easy.example.com:443",
                "hard.example.com:443",
                "hard.example.com:443",
            ]
        );
    }

    /// A conversation body: one system prompt, then this exchange.
    fn conversation(turns: &[(&str, &str)]) -> String {
        let messages = turns
            .iter()
            .map(|(role, content)| format!("{{\"role\": \"{role}\", \"content\": \"{content}\"}}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{{\"model\": \"claude\", \"system\": \"talk\", \"messages\": [{messages}]}}")
    }

    #[tokio::test]
    async fn a_conversation_is_a_turn_without_anyone_being_told_so() {
        // The test this commit exists for: two requests of one conversation, neither
        // carrying `x-ip-turn`, where round robin alone would have sent the second
        // elsewhere.
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(
            replicated_table(ip_core::Strategy::RoundRobin),
            ProcessorChain::new(),
            upstream.clone(),
        )
        .sticky_turns(Arc::new(ip_cache::SledCache::temporary().unwrap()));

        for turns in [
            &[("user", "hi")][..],
            &[("user", "hi"), ("assistant", "hello"), ("user", "again")][..],
        ] {
            gateway
                .handle(
                    context(granted()),
                    request("/anthropic/messages", &conversation(turns)),
                )
                .await
                .unwrap();
            gateway.settled().await;
        }
        let hosts = hosts_of(&upstream);
        assert_eq!(hosts.len(), 2);
        assert_eq!(
            hosts[0], hosts[1],
            "two requests of one conversation, neither marked, reached different endpoints"
        );
    }

    #[tokio::test]
    async fn a_body_that_may_not_be_held_is_neither_read_nor_made_sticky() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(
            replicated_table(ip_core::Strategy::RoundRobin),
            ProcessorChain::new(),
            upstream.clone(),
        )
        .sticky_turns(Arc::new(ip_cache::SledCache::temporary().unwrap()));
        let turns = [("user", "hi")];

        for _ in 0..2 {
            let mut asking = request("/anthropic/messages", &conversation(&turns));
            asking
                .headers_mut()
                .insert("transfer-encoding", HeaderValue::from_static("chunked"));
            gateway.handle(context(granted()), asking).await.unwrap();
            gateway.settled().await;
        }
        let hosts = hosts_of(&upstream);
        assert_eq!(hosts.len(), 2, "one send each, nothing raced or repeated");
        assert_ne!(
            hosts[0], hosts[1],
            "a body that may not be held was made sticky anyway"
        );
    }

    #[tokio::test]
    async fn a_marked_turn_is_the_turn_whatever_the_conversation_says() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(
            replicated_table(ip_core::Strategy::RoundRobin),
            ProcessorChain::new(),
            upstream.clone(),
        )
        .sticky_turns(Arc::new(ip_cache::SledCache::temporary().unwrap()));

        for turns in [
            &[("user", "hi")][..],
            &[("user", "something else entirely")][..],
        ] {
            gateway
                .handle(
                    in_turn(granted(), "turn-9"),
                    request("/anthropic/messages", &conversation(turns)),
                )
                .await
                .unwrap();
            gateway.settled().await;
        }
        let hosts = hosts_of(&upstream);
        assert_eq!(
            hosts[0], hosts[1],
            "a caller that named its turn was overruled by the conversation's own anchor"
        );
    }

    #[tokio::test]
    async fn a_compaction_starts_a_new_turn_though_its_anchor_survives() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(
            replicated_table(ip_core::Strategy::RoundRobin),
            ProcessorChain::new(),
            upstream.clone(),
        )
        .sticky_turns(Arc::new(ip_cache::SledCache::temporary().unwrap()));

        // The opening survives the compaction — same system prompt, same first user
        // message — so only the count tells the summarised history from a longer one.
        for turns in [
            &[("user", "hi"), ("assistant", "hello"), ("user", "again")][..],
            &[("user", "hi"), ("user", "again")][..],
            &[("user", "hi"), ("user", "again"), ("assistant", "twice")][..],
        ] {
            gateway
                .handle(
                    context(granted()),
                    request("/anthropic/messages", &conversation(turns)),
                )
                .await
                .unwrap();
            gateway.settled().await;
        }
        let hosts = hosts_of(&upstream);
        assert_ne!(
            hosts[1], hosts[0],
            "the shrunk history continued the turn it compacted away"
        );
        assert_eq!(
            hosts[2], hosts[1],
            "the turn the compaction started did not stay the turn it landed on"
        );
    }

    /// The hosts an upstream was sent to, in the order it was sent to them.
    fn hosts_of(upstream: &FakeUpstream) -> Vec<String> {
        upstream
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|seen| seen.host.clone())
            .collect()
    }

    #[tokio::test]
    async fn what_a_dispatcher_learns_reaches_it_through_its_folder() {
        let upstream = FakeUpstream::answering("pong");
        let (dispatcher, mut folding) = Dispatcher::new();
        let gateway = Gateway::new(
            replicated_table(ip_core::Strategy::LeastLoad),
            ProcessorChain::new(),
            upstream,
        )
        .dispatching(Arc::new(dispatcher));

        gateway
            .handle(context(granted()), request("/anthropic/messages", "ask"))
            .await
            .unwrap();
        // One answer went back, so one is there to fold.
        assert_eq!(folding.fold().await, 1);
    }

    #[tokio::test]
    async fn an_answer_out_of_the_cache_reaches_no_endpoint_at_all() {
        let upstream = FakeUpstream::answering("pong");
        let cache = Arc::new(ip_cache::SledCache::temporary().unwrap());
        let gateway = Gateway::new(
            replicated_table(ip_core::Strategy::RoundRobin),
            ProcessorChain::new(),
            upstream.clone(),
        )
        .caching(ResponseCache::new(cache, Ttl::seconds(60).unwrap()));

        for _ in 0..3 {
            let answered = gateway
                .handle(context(granted()), request("/anthropic/messages", "ask"))
                .await
                .unwrap();
            body_of(answered).await;
        }
        // Only the first reached an endpoint, so the turn moved once.
        assert_eq!(upstream.seen.lock().unwrap().len(), 1);
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
        assert_eq!(
            opened.span(EGRESS_REQUEST).unwrap()["served"],
            "api.example.com:443"
        );
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

    // ——— a turn's remembered class dispatches the rest of the turn (M13) ———

    /// A request that names a class of its own, in headers, not memory.
    fn class_request(path: &str, body: &str, class: &str) -> Request<GatewayBody> {
        let mut request = request(path, body);
        request.headers_mut().insert(
            http::header::HeaderName::from_static("x-ip-difficulty"),
            http::HeaderValue::from_str(class).expect("a class name is a header value"),
        );
        request
    }

    /// A gateway over a replicated round-robin rule, sticky by a memory of its own, and
    /// the end its rows come back on.
    fn recording_turns(
        upstream: &Arc<FakeUpstream>,
    ) -> (
        Gateway,
        tokio::sync::mpsc::UnboundedReceiver<ip_storage::Usage>,
    ) {
        let (recorder, recorded) = Ledger::new();
        let gateway = Gateway::new(
            replicated_table(ip_core::Strategy::RoundRobin),
            ProcessorChain::new(),
            upstream.clone(),
        )
        .sticky_turns(Arc::new(ip_cache::SledCache::temporary().unwrap()))
        .recording(recorder);
        (gateway, recorded)
    }

    // ——— classification: asked once, trusted once, never in the way (M13) ———

    /// The opening this conversation carries, so the leak assertion knows what to look
    /// for in everything that comes back.
    const OPENING_PROMPT: &str = "please audit the quarterly accounts for me";

    /// A conversation body with a system prompt of its own — every conversation has a
    /// different opening, and so does every turn worth telling apart.
    fn conversation_with_system(system: &str, turns: &[(&str, &str)]) -> String {
        let messages = turns
            .iter()
            .map(|(role, content)| format!("{{\"role\": \"{role}\", \"content\": \"{content}\"}}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{{\"model\": \"claude\", \"system\": \"{system}\", \"messages\": [{messages}]}}")
    }

    /// A gateway over a replicated round-robin rule, sticky by a memory of its own,
    /// judged by `judge`, and the end its rows come back on.
    fn judged_turns(
        upstream: &Arc<FakeUpstream>,
        judge: Arc<crate::flow::fixtures::CountingClassifier>,
    ) -> (
        Gateway,
        tokio::sync::mpsc::UnboundedReceiver<ip_storage::Usage>,
        Arc<ip_cache::SledCache>,
    ) {
        let (recorder, recorded) = Ledger::new();
        let cache = Arc::new(ip_cache::SledCache::temporary().unwrap());
        let gateway = Gateway::new(
            replicated_table(ip_core::Strategy::RoundRobin),
            ProcessorChain::new(),
            upstream.clone(),
        )
        .sticky_turns(cache.clone())
        .classifying(judge)
        .recording(recorder);
        (gateway, recorded, cache)
    }

    #[tokio::test]
    async fn a_new_turn_is_classified_once_and_the_rest_of_it_costs_nothing() {
        let upstream = FakeUpstream::answering("pong");
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::naming(
            ip_core::Difficulty::Hard,
        ));
        let (gateway, mut recorded, _memory) = judged_turns(&upstream, judge.clone());
        ask(
            &gateway,
            &mut recorded,
            in_turn(granted(), "turn-audit"),
            request(
                "/anthropic/messages",
                &conversation_with_system("you audit accounts", &[("user", OPENING_PROMPT)]),
            ),
        )
        .await;
        assert_eq!(judge.calls(), 1, "a new turn is classified once");
        let second = ask(
            &gateway,
            &mut recorded,
            in_turn(granted(), "turn-audit"),
            request(
                "/anthropic/messages",
                &conversation_with_system(
                    "you audit accounts",
                    &[
                        ("user", OPENING_PROMPT),
                        ("assistant", "pong"),
                        ("user", "and now?"),
                    ],
                ),
            ),
        )
        .await;
        assert_eq!(
            judge.calls(),
            1,
            "the turn remembers, so nothing is asked again"
        );
        assert_eq!(
            second.dispatched_class,
            Some(ip_core::Difficulty::Hard),
            "and the turn's remembered class rides the second request"
        );
    }

    /// The economics of the feature, unmarked: a conversation that never claims a
    /// turn is still the same turn request to request, and its judged class rides the
    /// second request from memory rather than a second forward pass.
    #[tokio::test]
    async fn an_unmarked_conversation_is_classified_once_and_remembered() {
        let upstream = FakeUpstream::answering("pong");
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::naming(
            ip_core::Difficulty::Hard,
        ));
        let (gateway, mut recorded, _memory) = judged_turns(&upstream, judge.clone());
        ask(
            &gateway,
            &mut recorded,
            context(granted()),
            request(
                "/anthropic/messages",
                &conversation_with_system("you audit accounts", &[("user", OPENING_PROMPT)]),
            ),
        )
        .await;
        assert_eq!(judge.calls(), 1, "a new conversation is classified once");
        let second = ask(
            &gateway,
            &mut recorded,
            context(granted()),
            request(
                "/anthropic/messages",
                &conversation_with_system("you audit accounts", &[("user", OPENING_PROMPT)]),
            ),
        )
        .await;
        assert_eq!(
            judge.calls(),
            1,
            "the judged conversation answers from memory"
        );
        assert_eq!(
            second.dispatched_class,
            Some(ip_core::Difficulty::Hard),
            "and it dispatches at the class that conversation earned"
        );
    }

    #[tokio::test]
    async fn a_request_that_names_its_class_is_never_classified() {
        let upstream = FakeUpstream::answering("pong");
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::naming(
            ip_core::Difficulty::Easy,
        ));
        let (gateway, mut recorded, _memory) = judged_turns(&upstream, judge.clone());
        ask(
            &gateway,
            &mut recorded,
            context(granted()),
            class_request(
                "/anthropic/messages",
                &conversation(&[("user", "I say hard")]),
                "hard",
            ),
        )
        .await;
        assert_eq!(
            judge.calls(),
            0,
            "a statement is believed, not double-checked"
        );
    }

    #[tokio::test]
    async fn a_judge_that_refuses_leaves_the_request_unclassified() {
        let upstream = FakeUpstream::answering("pong");
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::silent());
        let (gateway, mut recorded, _memory) = judged_turns(&upstream, judge.clone());
        let row = ask(
            &gateway,
            &mut recorded,
            context(granted()),
            request(
                "/anthropic/messages",
                &conversation(&[("user", "anything will do")]),
            ),
        )
        .await;
        assert_eq!(
            row.dispatched_class, None,
            "refused means unclassified, as today"
        );
        assert_eq!(judge.calls(), 1, "it was still asked once");
    }

    #[tokio::test]
    async fn a_body_without_a_conversation_is_not_classified() {
        let upstream = FakeUpstream::answering("pong");
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::naming(
            ip_core::Difficulty::Hard,
        ));
        let (gateway, mut recorded, _memory) = judged_turns(&upstream, judge.clone());
        ask(
            &gateway,
            &mut recorded,
            context(granted()),
            request(
                "/anthropic/messages",
                "{\"model\": \"claude\", \"prompt\": \"a plain completion\"}",
            ),
        )
        .await;
        assert_eq!(
            judge.calls(),
            0,
            "a body without a first user turn asks no question"
        );
    }

    #[tokio::test]
    async fn the_opening_reaches_the_judge_and_nothing_written_afterwards() {
        let upstream = FakeUpstream::answering("pong");
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::naming(
            ip_core::Difficulty::Hard,
        ));
        let (gateway, mut recorded, memory) = judged_turns(&upstream, judge.clone());
        ask(
            &gateway,
            &mut recorded,
            in_turn(granted(), "turn-leak"),
            request(
                "/anthropic/messages",
                &conversation_with_system("you audit accounts", &[("user", OPENING_PROMPT)]),
            ),
        )
        .await;
        // The judge saw it — that is the whole point of the seam.
        assert!(judge.last.lock().unwrap().contains(OPENING_PROMPT));
        // The row does not carry it, and the second request proves the verdict came
        // back from memory: the judge is never asked again.
        let second = ask(
            &gateway,
            &mut recorded,
            in_turn(granted(), "turn-leak"),
            request(
                "/anthropic/messages",
                &conversation_with_system(
                    "you audit accounts",
                    &[
                        ("user", OPENING_PROMPT),
                        ("assistant", "pong"),
                        ("user", "next"),
                    ],
                ),
            ),
        )
        .await;
        assert!(
            !format!("{second:?}").contains(OPENING_PROMPT),
            "a row that repeats a prompt"
        );
        assert_eq!(
            judge.calls(),
            1,
            "the verdict rode back from memory, not the judge"
        );
        // And the memory itself, read back as stored bytes: the verdict is the digest
        // and its counts, and neither of the prompts that made them. The row's Debug
        // could not betray a prompt even if the verdict carried one — Opening hides
        // its text by hand — so this is the assertion that actually guards the leak.
        let stored = memory
            .get(&crate::turns::key(
                granted().identity(),
                &TurnId::new("turn-leak").unwrap(),
            ))
            .await
            .expect("the cache answers")
            .expect("the turn's verdict is stored");
        let stored = String::from_utf8_lossy(&stored);
        assert!(
            !stored.contains(OPENING_PROMPT),
            "the verdict repeats the prompt"
        );
        assert!(
            !stored.contains("you audit accounts"),
            "the verdict repeats the system prompt"
        );
    }

    // ——— arbitration: the margin's second level (M13) ———

    /// One word of ruling, in the shape an answer arrives in.
    const RULING: &str = r#"{"choices":[{"message":{"content":"hard"}}]}"#;

    /// A table with the turn's route and a route for the arbitrating question.
    fn arbitrating_table() -> RoutingTable {
        let turn = rule(endpoint(
            ip_core::Protocol::Https,
            "api.example.com",
            443,
            "/v1",
        ));
        let ask = ip_core::RouteRule {
            api: arbitration_api(),
            key: RouteKey::new(endpoint(
                ip_core::Protocol::Http,
                "gateway.local",
                8080,
                "/arbiter",
            )),
            target: ip_core::RouteTarget::new(endpoint(
                ip_core::Protocol::Https,
                "arbiter.example.com",
                443,
                "/ask",
            )),
            strategy: ip_core::Strategy::default(),
            hedge: Hedge::default(),
            hedge_delay: ip_core::HedgeDelay::default(),
            caller_speaks: None,
            upstream_speaks: None,
        };
        RoutingTable::from_rules(vec![turn, ask])
    }

    /// A gateway over the arbitrating table, sticky, recording, judged by `judge` — and
    /// arbitrating, when one is configured.
    fn arbitrating_gateway(
        judge: Arc<crate::flow::fixtures::CountingClassifier>,
        upstream: Arc<dyn Upstream>,
        arbiter: Option<Arbiter>,
    ) -> (
        Gateway,
        tokio::sync::mpsc::UnboundedReceiver<ip_storage::Usage>,
    ) {
        let (recorder, recorded) = Ledger::new();
        let mut gateway = Gateway::new(arbitrating_table(), ProcessorChain::new(), upstream)
            .sticky_turns(Arc::new(ip_cache::SledCache::temporary().unwrap()))
            .classifying(judge)
            .recording(recorder);
        if let Some(arbiter) = arbiter {
            gateway = gateway.arbitrating(Arc::new(arbiter));
        }
        (gateway, recorded)
    }

    /// The api the arbitrating route alone carries, so its row can be told from the
    /// caller's traffic in the same ledger.
    fn arbitration_api() -> ip_core::ApiId {
        ip_core::ApiId::new("arbitration").unwrap()
    }

    /// A question about the judge's opening, with the time it is given.
    fn arbiter(timeout: std::time::Duration) -> Arbiter {
        Arbiter::new(
            ip_core::ModelName::new("oracle").unwrap(),
            ip_core::AbsPath::new("/arbiter").unwrap(),
            timeout,
            ip_config::Secret::from("sk-oracle"),
        )
    }

    /// An upstream where only the arbitrating endpoint is named, and it answers the same
    /// way every time; the turn's own route answers with an empty success.
    fn asking_only(behaviour: Behaviour) -> Arc<FakeUpstream> {
        FakeUpstream::answering_each(&[("arbiter.example.com:443", behaviour)])
    }

    /// How many questions the arbitrating endpoint has been sent.
    fn questions_to(upstream: &FakeUpstream) -> Vec<(String, String)> {
        upstream
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.host.starts_with("arbiter."))
            .map(|seen| {
                (
                    seen.uri.clone(),
                    String::from_utf8_lossy(&seen.body).into_owned(),
                )
            })
            .collect()
    }

    fn answered(body: &'static str) -> Behaviour {
        Behaviour {
            delay: std::time::Duration::ZERO,
            status: StatusCode::OK,
            body,
            fail: false,
        }
    }

    /// One request through an arbitrating gateway: its answer drained, and the caller's
    /// row out of the ledger — the arbitrating call leaves a row of its own there, told
    /// apart by the api of the route it travelled, and skipped here.
    async fn asked(
        gateway: &Gateway,
        recorded: &mut tokio::sync::mpsc::UnboundedReceiver<ip_storage::Usage>,
        context: RequestContext,
        request: Request<GatewayBody>,
    ) -> (ip_storage::Usage, Vec<ip_storage::Usage>) {
        let answered = gateway.handle(context, request).await.unwrap();
        body_of(answered).await;
        gateway.settled().await;
        let mut others = Vec::new();
        loop {
            let row = recorded.recv().await.expect("a row per request");
            if row.api == api() {
                return (row, others);
            }
            others.push(row);
        }
    }

    #[tokio::test]
    async fn an_unsure_opening_is_arbitrated_and_dispatched_at_the_ruling() {
        let upstream = asking_only(answered(RULING));
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::silent());
        let (gateway, mut recorded) = arbitrating_gateway(
            judge.clone(),
            upstream.clone(),
            Some(arbiter(std::time::Duration::from_secs(5))),
        );
        let (row, _) = asked(
            &gateway,
            &mut recorded,
            context(granted()),
            request(
                "/anthropic/messages",
                &conversation_with_system("you audit accounts", &[("user", OPENING_PROMPT)]),
            ),
        )
        .await;
        assert_eq!(
            row.dispatched_class,
            Some(ip_core::Difficulty::Hard),
            "the turn is dispatched at the ruling it was given"
        );
        let asked = questions_to(&upstream);
        assert_eq!(asked.len(), 1, "the ruling was asked for once");
        assert!(
            asked[0].1.contains(OPENING_PROMPT),
            "the question carries the opening the judge could not call"
        );
        assert!(
            asked[0].1.contains("oracle"),
            "the question names the arbiter's model"
        );
        // The judge counted one ask over the whole exchange: its own, from this
        // request. The question goes out unjudged and marked, so even an `ask_path`
        // resolving back here would leave it unjudged — see
        // `a_marked_question_passes_the_margin_by`.
        assert_eq!(
            judge.calls(),
            1,
            "the local judge was asked once, not twice"
        );
    }

    /// The other dialect: an anthropic endpoint answers `content`, not `choices`, and a
    /// gateway that reads one shape only would call the other unreadable and classify
    /// nothing with the shipped example.
    #[tokio::test]
    async fn a_ruling_in_the_other_dialect_is_read_as_well() {
        let upstream = asking_only(Behaviour {
            delay: std::time::Duration::ZERO,
            status: StatusCode::OK,
            body: r#"{"content":[{"type":"text","text":"hard"}]}"#,
            fail: false,
        });
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::silent());
        let (gateway, mut recorded) = arbitrating_gateway(
            judge,
            upstream,
            Some(arbiter(std::time::Duration::from_secs(5))),
        );
        let (row, _) = asked(
            &gateway,
            &mut recorded,
            context(granted()),
            request(
                "/anthropic/messages",
                &conversation_with_system("you audit accounts", &[("user", OPENING_PROMPT)]),
            ),
        )
        .await;
        assert_eq!(
            row.dispatched_class,
            Some(ip_core::Difficulty::Hard),
            "content[0].text is a ruling where choices[0].message.content is"
        );
    }

    /// A question that arrives at the flow carrying the arbitrating marker — which is
    /// what happens when an `ask_path` resolves back into this gateway or a peer — is
    /// left alone: not judged, not arbitrated, just routed. Without the marker's being
    /// honoured, such a question would be found unsure and arbitrated again, over the
    /// network, without bound.
    #[tokio::test]
    async fn a_marked_question_passes_the_margin_by() {
        let upstream = asking_only(answered(RULING));
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::naming(
            ip_core::Difficulty::Hard,
        ));
        let (gateway, mut recorded) = arbitrating_gateway(
            judge.clone(),
            upstream.clone(),
            Some(arbiter(std::time::Duration::from_secs(5))),
        );
        let mut question = request(
            "/anthropic/messages",
            &conversation_with_system("you audit accounts", &[("user", OPENING_PROMPT)]),
        );
        question.headers_mut().insert(
            crate::arbiter::ARBITRATING,
            http::HeaderValue::from_static("1"),
        );
        let (row, _) = asked(&gateway, &mut recorded, context(granted()), question).await;
        assert_eq!(judge.calls(), 0, "a marked question is no opening to judge");
        assert!(
            questions_to(&upstream).is_empty(),
            "a marked question never becomes another question"
        );
        assert_eq!(
            row.dispatched_class, None,
            "and it is dispatched as it arrived"
        );
    }

    /// The arbiter's call is a billed thing asked on a caller's behalf, so it is a row
    /// where every other call is written — same identity, its own route, endpoint and
    /// model.
    #[tokio::test]
    async fn the_arbiters_call_is_recorded_where_the_answers_are() {
        let upstream = asking_only(answered(RULING));
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::silent());
        let (gateway, mut recorded) = arbitrating_gateway(
            judge,
            upstream,
            Some(arbiter(std::time::Duration::from_secs(5))),
        );
        let (row, mut asking) = asked(
            &gateway,
            &mut recorded,
            context(granted()),
            request(
                "/anthropic/messages",
                &conversation_with_system("you audit accounts", &[("user", OPENING_PROMPT)]),
            ),
        )
        .await;
        assert_eq!(row.dispatched_class, Some(ip_core::Difficulty::Hard));
        // Drain for both rows rather than trusting which the ledger wrote first.
        while let Ok(extra) = recorded.try_recv() {
            asking.push(extra);
        }
        assert_eq!(
            asking.len(),
            1,
            "one question was asked, one row it leaves, beside the caller's own"
        );
        let asking = asking.pop().expect("the arbitrating row");
        assert_eq!(
            asking.api,
            arbitration_api(),
            "recorded under the route it travelled"
        );
        assert_eq!(
            asking.model.map(|model| model.as_str().to_owned()),
            Some("oracle".to_owned()),
            "the model asked by name"
        );
        assert_eq!(
            asking.served,
            ip_core::Served::Upstream,
            "an endpoint answered it"
        );
        let served_by = format!("{:?}", asking.served_by.expect("which one"));
        assert!(served_by.contains("arbiter.example.com"), "{served_by}");
    }

    /// The question presents the arbitrating upstream's credential — both header
    /// spellings, so one answer serves either provider — and its own marker, and never
    /// anything the caller carried.
    /// Chosen policy, not inherited behaviour: the arbitrating call was asked because
    /// the caller's turn was unsure, and its tokens therefore spend against the
    /// caller's token limits as the caller's own answer would have.
    #[tokio::test]
    async fn the_arbiters_spend_is_the_callers_spend() {
        let ruling = r#"{"usage":{"prompt_tokens":10,"completion_tokens":1},"choices":[{"message":{"content":"hard"}}]}"#;
        let upstream = asking_only(answered(ruling));
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::silent());
        let counters = Arc::new(crate::limit::tests::Held::default());
        let limiter = Limiter::new(counters.clone(), Rows::holding(0));
        let (mut gateway, mut recorded) = arbitrating_gateway(
            judge,
            upstream,
            Some(arbiter(std::time::Duration::from_secs(5))),
        );
        gateway = gateway
            .limiting(Arc::new(limiter))
            .with_limits(Limits::new(vec![allowing(
                ip_core::Counted::Tokens,
                ip_core::Period::Month,
                1_000,
            )]));
        let (row, others) = asked(
            &gateway,
            &mut recorded,
            context(granted()),
            request(
                "/anthropic/messages",
                &conversation_with_system("you audit accounts", &[("user", OPENING_PROMPT)]),
            ),
        )
        .await;
        assert_eq!(row.dispatched_class, Some(ip_core::Difficulty::Hard));
        let asking = others
            .into_iter()
            .find(|row| row.api == arbitration_api())
            .expect("the arbitrating row");
        assert_eq!(asking.tokens.total(), 11, "ten in, one out");
        assert_eq!(
            counters.spent_total(),
            11,
            "the arbiter's tokens spent against the caller's limits, once"
        );
    }

    #[tokio::test]
    async fn the_question_presents_the_upstreams_key() {
        let upstream = asking_only(answered(RULING));
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::silent());
        let (gateway, mut recorded) = arbitrating_gateway(
            judge,
            upstream.clone(),
            Some(arbiter(std::time::Duration::from_secs(5))),
        );
        asked(
            &gateway,
            &mut recorded,
            context(granted()),
            request(
                "/anthropic/messages",
                &conversation_with_system("you audit accounts", &[("user", OPENING_PROMPT)]),
            ),
        )
        .await;
        let seen = upstream.seen.lock().unwrap();
        let asked = seen
            .iter()
            .find(|seen| seen.host.starts_with("arbiter."))
            .expect("the question went out");
        assert_eq!(
            asked.headers[http::header::AUTHORIZATION].to_str().unwrap(),
            "Bearer sk-oracle"
        );
        assert_eq!(
            asked.headers["x-api-key"].to_str().unwrap(),
            "sk-oracle",
            "spelled both ways, for either provider answers"
        );
        assert_eq!(
            asked.headers[crate::arbiter::ARBITRATING].to_str().unwrap(),
            "1"
        );
    }

    /// An upstream that answers the arbitrating route with a body that never arrives:
    /// status and headers at once, no frame ever, and the sender held on purpose — a
    /// half-dead model fails in this shape far more often than before its status line.
    struct Trickling;

    #[async_trait::async_trait]
    impl Upstream for Trickling {
        async fn send(
            &self,
            request: Request<GatewayBody>,
        ) -> Result<Response<GatewayBody>, crate::error::UpstreamError> {
            let host = request
                .headers()
                .get(http::header::HOST)
                .map(|value| value.to_str().unwrap_or_default().to_owned())
                .unwrap_or_default();
            if host.starts_with("arbiter.") {
                let (sender, receiver) = tokio::sync::mpsc::channel(1);
                // Holding the sender *is* the stall: drop it and the body would end
                // cleanly empty. Nothing sends, nothing drops.
                std::mem::forget(sender);
                return Response::builder()
                    .status(StatusCode::OK)
                    .body(http_body_util::BodyExt::boxed_unsync(
                        crate::body::ChannelBody::new(receiver),
                    ))
                    .map_err(|error| crate::error::UpstreamError::new(error.to_string()));
            }
            Ok(Response::builder()
                .status(StatusCode::OK)
                .body(crate::body::from_bytes(bytes::Bytes::from_static(b"pong")))
                .unwrap())
        }
    }

    /// The deadline is over the whole question — ask, answer, read — not just over the
    /// headers. An arbiter that answers 200 and then trickles is the failure ruling 3
    /// exists to forbid, and it must cost the caller its opening request only up to the
    /// timeout, leaving it served and unclassified, with no row for an answer that
    /// never came.
    #[tokio::test]
    async fn a_stalled_answer_is_left_behind_like_a_silent_one() {
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::silent());
        let (gateway, mut recorded) = arbitrating_gateway(
            judge,
            Arc::new(Trickling),
            Some(arbiter(std::time::Duration::from_millis(25))),
        );
        let started = std::time::Instant::now();
        let answered = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            gateway.handle(
                context(granted()),
                request(
                    "/anthropic/messages",
                    &conversation_with_system("you audit accounts", &[("user", OPENING_PROMPT)]),
                ),
            ),
        )
        .await
        .expect("the stalled arbiter ends at its deadline, not at the caller's patience")
        .expect("the request is served");
        body_of(answered).await;
        gateway.settled().await;
        assert!(
            started.elapsed() < std::time::Duration::from_millis(900),
            "the caller waited the arbiter out, not the stall"
        );
        let row = recorded.recv().await.expect("the caller's row");
        assert_eq!(row.dispatched_class, None, "the stall is no ruling");
        assert!(
            recorded.try_recv().is_err(),
            "an answer that never arrived is no call to record"
        );
    }

    /// The question asks in the taxonomy's own words: every class's rubric line is in
    /// it, the work-not-length guard is in it, and the opening sits beneath as the data
    /// judged. A hand-written prompt could drift from what the classifier was trained
    /// with; a generated one cannot.
    #[tokio::test]
    async fn the_question_asks_in_the_taxonomys_own_words() {
        let upstream = asking_only(answered(RULING));
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::silent());
        let (gateway, mut recorded) = arbitrating_gateway(
            judge,
            upstream.clone(),
            Some(arbiter(std::time::Duration::from_secs(5))),
        );
        asked(
            &gateway,
            &mut recorded,
            context(granted()),
            request(
                "/anthropic/messages",
                &conversation_with_system("you audit accounts", &[("user", OPENING_PROMPT)]),
            ),
        )
        .await;
        let seen = upstream.seen.lock().unwrap();
        let asked = seen
            .iter()
            .find(|seen| seen.host.starts_with("arbiter."))
            .expect("the question went out");
        let asked = String::from_utf8_lossy(&asked.body);
        for class in ip_core::Difficulty::ALL {
            assert!(
                asked.contains(class.rubric()),
                "the question is missing {class}'s rubric line"
            );
        }
        assert!(
            asked.contains(
                "Judge the work the request implies, not how long the request is: \
                 a short question can imply hard work."
            ),
            "the work-not-length guard is part of the contract, whole"
        );
        assert!(
            asked.contains(OPENING_PROMPT),
            "the opening is what is judged"
        );
    }

    /// With the rubric in the prompt, a model that restates its instructions before
    /// answering puts class words on the page that are not its ruling. A reply naming
    /// two classes names none.
    #[tokio::test]
    async fn a_reply_naming_two_classes_names_none() {
        let upstream = asking_only(answered(
            r#"{"choices":[{"message":{"content":"somewhere between easy and hard"}}]}"#,
        ));
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::silent());
        let (gateway, mut recorded) = arbitrating_gateway(
            judge,
            upstream,
            Some(arbiter(std::time::Duration::from_secs(5))),
        );
        let (row, _) = asked(
            &gateway,
            &mut recorded,
            context(granted()),
            request(
                "/anthropic/messages",
                &conversation_with_system("you audit accounts", &[("user", OPENING_PROMPT)]),
            ),
        )
        .await;
        assert_eq!(
            row.dispatched_class, None,
            "easy and hard together is prose, and prose is not a ruling"
        );
    }

    #[tokio::test]
    async fn a_confident_opening_never_asks_the_arbiter() {
        let upstream = asking_only(answered(RULING));
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::naming(
            ip_core::Difficulty::Hard,
        ));
        let (gateway, mut recorded) = arbitrating_gateway(
            judge.clone(),
            upstream.clone(),
            Some(arbiter(std::time::Duration::from_secs(5))),
        );
        let (_, _) = asked(
            &gateway,
            &mut recorded,
            context(granted()),
            request(
                "/anthropic/messages",
                &conversation_with_system("you audit accounts", &[("user", OPENING_PROMPT)]),
            ),
        )
        .await;
        assert!(
            questions_to(&upstream).is_empty(),
            "a margin that committed never asks"
        );
    }

    #[tokio::test]
    async fn the_arbiter_is_asked_once_per_turn() {
        let upstream = asking_only(answered(RULING));
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::silent());
        let (gateway, mut recorded) = arbitrating_gateway(
            judge.clone(),
            upstream.clone(),
            Some(arbiter(std::time::Duration::from_secs(5))),
        );
        let (_, _) = asked(
            &gateway,
            &mut recorded,
            context(granted()),
            request(
                "/anthropic/messages",
                &conversation_with_system("you audit accounts", &[("user", OPENING_PROMPT)]),
            ),
        )
        .await;
        let (second, _) = asked(
            &gateway,
            &mut recorded,
            context(granted()),
            request(
                "/anthropic/messages",
                &conversation_with_system("you audit accounts", &[("user", OPENING_PROMPT)]),
            ),
        )
        .await;
        assert_eq!(
            questions_to(&upstream).len(),
            1,
            "a ruling is paid for once per conversation, not per request"
        );
        assert_eq!(
            second.dispatched_class,
            Some(ip_core::Difficulty::Hard),
            "the turn dispatches at the ruling it already has"
        );
    }

    #[tokio::test]
    async fn an_arbiter_that_refuses_leaves_the_request_unclassified() {
        let upstream = asking_only(Behaviour {
            delay: std::time::Duration::ZERO,
            status: StatusCode::SERVICE_UNAVAILABLE,
            body: "overloaded",
            fail: false,
        });
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::silent());
        let (gateway, mut recorded) = arbitrating_gateway(
            judge,
            upstream,
            Some(arbiter(std::time::Duration::from_secs(5))),
        );
        let (row, _) = asked(
            &gateway,
            &mut recorded,
            context(granted()),
            request(
                "/anthropic/messages",
                &conversation_with_system("you audit accounts", &[("user", OPENING_PROMPT)]),
            ),
        )
        .await;
        assert_eq!(row.dispatched_class, None, "a refusal is no ruling");
    }

    #[tokio::test]
    async fn a_slow_arbiter_is_left_behind() {
        let upstream = asking_only(Behaviour {
            delay: std::time::Duration::from_secs(30),
            status: StatusCode::OK,
            body: RULING,
            fail: false,
        });
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::silent());
        let (gateway, mut recorded) = arbitrating_gateway(
            judge,
            upstream,
            Some(arbiter(std::time::Duration::from_millis(25))),
        );
        let started = std::time::Instant::now();
        let (row, others) = asked(
            &gateway,
            &mut recorded,
            context(granted()),
            request(
                "/anthropic/messages",
                &conversation_with_system("you audit accounts", &[("user", OPENING_PROMPT)]),
            ),
        )
        .await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "the caller waits the arbiter out, not the answer"
        );
        assert_eq!(row.dispatched_class, None, "a silence is no ruling");
        assert!(
            others.is_empty() && recorded.try_recv().is_err(),
            "a question outlived by its deadline is no call to record"
        );
    }

    #[tokio::test]
    async fn prose_is_not_a_ruling() {
        let upstream = asking_only(answered(
            r#"{"choices":[{"message":{"content":"I would say it depends on the weather"}}]}"#,
        ));
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::silent());
        let (gateway, mut recorded) = arbitrating_gateway(
            judge,
            upstream,
            Some(arbiter(std::time::Duration::from_secs(5))),
        );
        let (row, _) = asked(
            &gateway,
            &mut recorded,
            context(granted()),
            request(
                "/anthropic/messages",
                &conversation_with_system("you audit accounts", &[("user", OPENING_PROMPT)]),
            ),
        )
        .await;
        assert_eq!(
            row.dispatched_class, None,
            "prose that names no class is no class"
        );
    }

    #[tokio::test]
    async fn an_unreadable_answer_is_not_a_ruling() {
        let upstream = asking_only(answered("not even json"));
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::silent());
        let (gateway, mut recorded) = arbitrating_gateway(
            judge,
            upstream,
            Some(arbiter(std::time::Duration::from_secs(5))),
        );
        let (row, _) = asked(
            &gateway,
            &mut recorded,
            context(granted()),
            request(
                "/anthropic/messages",
                &conversation_with_system("you audit accounts", &[("user", OPENING_PROMPT)]),
            ),
        )
        .await;
        assert_eq!(
            row.dispatched_class, None,
            "an answer that must be parsed apart is no ruling"
        );
    }

    #[tokio::test]
    async fn a_gateway_without_an_arbiter_asks_nothing() {
        let upstream = FakeUpstream::answering("pong");
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::silent());
        let (gateway, mut recorded) = arbitrating_gateway(judge, upstream.clone(), None);
        for _ in 0..2 {
            let (row, _) = asked(
                &gateway,
                &mut recorded,
                context(granted()),
                request(
                    "/anthropic/messages",
                    &conversation_with_system("you audit accounts", &[("user", OPENING_PROMPT)]),
                ),
            )
            .await;
            assert_eq!(row.dispatched_class, None);
        }
        assert_eq!(
            upstream.seen.lock().unwrap().len(),
            2,
            "two requests, two upstream calls, and no questions asked of anything else"
        );
    }

    // ——— a route bridges two families (M14) ———

    use ip_core::Dialect;

    /// A gateway whose one rule bridges `caller` to `upstream`, answering with `answer`.
    fn bridging(
        caller: Dialect,
        upstream: Dialect,
        answer: &'static str,
    ) -> (Gateway, Arc<FakeUpstream>) {
        let sent = FakeUpstream::answering(answer);
        let gateway = Gateway::new(
            crate::flow::fixtures::bridging_table(caller, upstream),
            ProcessorChain::new(),
            sent.clone(),
        );
        (gateway, sent)
    }

    /// What the upstream was actually handed.
    fn received(upstream: &FakeUpstream) -> Vec<u8> {
        upstream.seen.lock().unwrap()[0].body.to_vec()
    }

    const IN_OPENAI: &str = r#"{"model":"shared","max_tokens":50,"messages":[{"role":"system","content":"you audit accounts"},{"role":"user","content":"hello"}]}"#;

    /// The property that quietly rots: a rule that bridges nothing hands the upstream
    /// the caller's own bytes, not a re-serialisation of them. Translation is a thing
    /// an operator turns on for a route, never a tax the gateway pays everywhere.
    #[tokio::test]
    async fn a_route_that_bridges_nothing_forwards_the_bytes_it_was_handed() {
        // Spaced and ordered exactly as no writer of ours would write it.
        let odd = "{ \"messages\" : [ {\"content\":\"hi\",\"role\":\"user\"} ] , \"model\":\"m\" }";
        for table in [
            crate::flow::fixtures::table(),
            crate::flow::fixtures::bridging_table(Dialect::OpenAi, Dialect::OpenAi),
        ] {
            let upstream = FakeUpstream::answering(ANSWERED);
            let gateway = Gateway::new(table, ProcessorChain::new(), upstream.clone());
            let answered = gateway
                .handle(context(granted()), request("/anthropic/messages", odd))
                .await
                .unwrap();
            body_of(answered).await;
            assert_eq!(
                String::from_utf8(received(&upstream)).unwrap(),
                odd,
                "a rule that bridges nothing touched the bytes"
            );
        }
    }

    #[tokio::test]
    async fn a_caller_of_one_family_is_served_by_another() {
        let answered = r#"{"type":"message","role":"assistant","model":"shared","content":[{"type":"text","text":"forty-two"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":2}}"#;
        let (gateway, upstream) = bridging(Dialect::OpenAi, Dialect::Anthropic, answered);
        let response = gateway
            .handle(
                context(granted()),
                request("/anthropic/messages", IN_OPENAI),
            )
            .await
            .unwrap();
        let back: serde_json::Value = serde_json::from_str(&body_of(response).await).unwrap();

        let sent: serde_json::Value = serde_json::from_slice(&received(&upstream)).unwrap();
        assert_eq!(
            sent["system"],
            serde_json::json!("you audit accounts"),
            "the upstream was sent its own family's shape"
        );
        assert!(sent.get("choices").is_none());
        assert_eq!(
            back["choices"][0]["message"]["content"],
            serde_json::json!("forty-two"),
            "and the caller got its own back"
        );
        assert_eq!(back["usage"]["prompt_tokens"], serde_json::json!(3));
    }

    #[tokio::test]
    async fn every_crossing_carries_a_conversation_both_ways() {
        let answers = |dialect| match dialect {
            Dialect::OpenAi => {
                r#"{"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#
            }
            Dialect::Anthropic => {
                r#"{"type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}"#
            }
            Dialect::Gemini => {
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}]}"#
            }
        };
        for upstream_speaks in [Dialect::Anthropic, Dialect::Gemini] {
            let (gateway, upstream) =
                bridging(Dialect::OpenAi, upstream_speaks, answers(upstream_speaks));
            let response = gateway
                .handle(
                    context(granted()),
                    request("/anthropic/messages", IN_OPENAI),
                )
                .await
                .unwrap();
            let back: serde_json::Value = serde_json::from_str(&body_of(response).await).unwrap();
            assert_eq!(
                back["choices"][0]["message"]["content"],
                serde_json::json!("ok"),
                "a crossing into {upstream_speaks} did not come home"
            );
            assert!(
                !received(&upstream).is_empty(),
                "{upstream_speaks} was sent nothing"
            );
        }
    }

    /// A tool call is the thing agent traffic actually sends, and its id is the only
    /// join between a call and the result that answers it.
    #[tokio::test]
    async fn a_tool_call_survives_a_crossing_in_both_directions() {
        let answered = r#"{"type":"message","role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"search","input":{"q":"rust"}}],"stop_reason":"tool_use"}"#;
        let asked = r#"{"model":"shared","max_tokens":50,"messages":[{"role":"user","content":"search"}],"tools":[{"type":"function","function":{"name":"search","parameters":{"type":"object"}}}]}"#;
        let (gateway, upstream) = bridging(Dialect::OpenAi, Dialect::Anthropic, answered);
        let response = gateway
            .handle(context(granted()), request("/anthropic/messages", asked))
            .await
            .unwrap();
        let back: serde_json::Value = serde_json::from_str(&body_of(response).await).unwrap();

        let sent: serde_json::Value = serde_json::from_slice(&received(&upstream)).unwrap();
        assert_eq!(sent["tools"][0]["name"], serde_json::json!("search"));
        assert_eq!(
            sent["tools"][0]["input_schema"]["type"],
            serde_json::json!("object")
        );

        let call = &back["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(call["id"], serde_json::json!("call_1"));
        assert_eq!(call["function"]["name"], serde_json::json!("search"));
        assert_eq!(
            back["choices"][0]["finish_reason"],
            serde_json::json!("tool_calls")
        );
    }

    /// Not everything behind a prefix is a conversation, and what this layer cannot
    /// read it must not touch.
    #[tokio::test]
    async fn a_body_that_is_no_conversation_crosses_untouched() {
        let embeddings = r#"{"model":"text-embedding-3-small","input":"some text"}"#;
        let (gateway, upstream) = bridging(Dialect::OpenAi, Dialect::Gemini, ANSWERED);
        let answered = gateway
            .handle(
                context(granted()),
                request("/anthropic/messages", embeddings),
            )
            .await
            .unwrap();
        body_of(answered).await;
        assert_eq!(String::from_utf8(received(&upstream)).unwrap(), embeddings);
    }

    /// A translated body is a different size, and a content length that still describes
    /// the old one is a hang rather than an error.
    #[tokio::test]
    async fn a_crossing_restates_how_long_the_body_is() {
        let (gateway, upstream) = bridging(Dialect::OpenAi, Dialect::Anthropic, ANSWERED);
        let answered = gateway
            .handle(
                context(granted()),
                request("/anthropic/messages", IN_OPENAI),
            )
            .await
            .unwrap();
        body_of(answered).await;
        let seen = upstream.seen.lock().unwrap();
        let stated: usize = seen[0].headers[http::header::CONTENT_LENGTH]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(stated, seen[0].body.len());
        assert_ne!(stated, IN_OPENAI.len(), "the body really did change size");
    }

    /// Until a stream can cross, saying so is better than handing a caller the other
    /// family's events.
    #[tokio::test]
    async fn a_streamed_answer_cannot_cross_yet_and_says_so() {
        let streaming = r#"{"model":"shared","max_tokens":50,"stream":true,"messages":[{"role":"user","content":"hello"}]}"#;
        let (gateway, upstream) = bridging(Dialect::OpenAi, Dialect::Anthropic, ANSWERED);
        let refused = gateway
            .handle(
                context(granted()),
                request("/anthropic/messages", streaming),
            )
            .await
            .expect_err("a stream cannot cross yet");
        assert_eq!(refused.status(), StatusCode::NOT_IMPLEMENTED);
        assert!(
            upstream.seen.lock().unwrap().is_empty(),
            "nothing was sent upstream to be half-translated"
        );
    }

    /// And a stream on a route that bridges nothing is untouched, as it always was.
    #[tokio::test]
    async fn a_streamed_answer_on_a_route_that_bridges_nothing_is_served() {
        let streaming =
            r#"{"model":"m","stream":true,"messages":[{"role":"user","content":"hello"}]}"#;
        let upstream = FakeUpstream::answering(ANSWERED);
        let gateway = Gateway::new(
            crate::flow::fixtures::table(),
            ProcessorChain::new(),
            upstream.clone(),
        );
        let answered = gateway
            .handle(
                context(granted()),
                request("/anthropic/messages", streaming),
            )
            .await
            .expect("a stream that crosses nothing is served");
        body_of(answered).await;
        assert_eq!(String::from_utf8(received(&upstream)).unwrap(), streaming);
    }

    // ——— the row says which level decided (M13) ———

    /// The same conversation as [`opening`], written by the family that calls its
    /// messages `contents` — which the gateway could not read at all before M14.
    fn gemini_opening() -> Request<GatewayBody> {
        let body = serde_json::json!({
            "systemInstruction": {"parts": [{"text": "you audit accounts"}]},
            "contents": [{"role": "user", "parts": [{"text": OPENING_PROMPT}]}],
        });
        request("/anthropic/messages", &body.to_string())
    }

    /// A conversation this family sends is judged like any other: before M14 it had no
    /// opening, so the judge was never asked and the row carried no class at all.
    #[tokio::test]
    async fn a_conversation_of_contents_and_parts_is_classified() {
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::naming(
            ip_core::Difficulty::Hard,
        ));
        let (gateway, mut recorded) =
            arbitrating_gateway(judge.clone(), asking_only(answered(RULING)), None);
        let (row, _) = asked(
            &gateway,
            &mut recorded,
            context(granted()),
            gemini_opening(),
        )
        .await;
        assert_eq!(judge.calls(), 1, "the judge was given an opening to read");
        assert_eq!(row.dispatched_class, Some(ip_core::Difficulty::Hard));
        assert_eq!(row.decided_by, Some(ip_core::Decider::Classifier));
    }

    /// And arbitrated, when the local margin falls short.
    #[tokio::test]
    async fn a_conversation_of_contents_and_parts_is_arbitrated() {
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::silent());
        let (gateway, mut recorded) = arbitrating_gateway(
            judge,
            asking_only(answered(RULING)),
            Some(arbiter(std::time::Duration::from_secs(5))),
        );
        let (row, _) = asked(
            &gateway,
            &mut recorded,
            context(granted()),
            gemini_opening(),
        )
        .await;
        assert_eq!(row.dispatched_class, Some(ip_core::Difficulty::Hard));
        assert_eq!(row.decided_by, Some(ip_core::Decider::Arbiter));
    }

    /// And its turn is remembered, so the rest of the conversation costs nothing.
    #[tokio::test]
    async fn a_conversation_of_contents_and_parts_rides_its_turn() {
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::naming(
            ip_core::Difficulty::Hard,
        ));
        let (gateway, mut recorded) =
            arbitrating_gateway(judge.clone(), asking_only(answered(RULING)), None);
        asked(
            &gateway,
            &mut recorded,
            context(granted()),
            gemini_opening(),
        )
        .await;
        let (second, _) = asked(
            &gateway,
            &mut recorded,
            context(granted()),
            gemini_opening(),
        )
        .await;
        assert_eq!(judge.calls(), 1, "one judgement across the conversation");
        assert_eq!(second.decided_by, Some(ip_core::Decider::Turn));
    }

    /// The arbiter may be any model a route leads to, so its ruling is read whichever
    /// family answered. Before M14 this function knew two shapes by hand.
    #[tokio::test]
    async fn a_ruling_is_read_whichever_family_answers() {
        for (family, ruling) in [
            ("openai", RULING),
            (
                "anthropic",
                r#"{"content":[{"type":"text","text":"hard"}]}"#,
            ),
            (
                "gemini",
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hard"}]},"finishReason":"STOP"}]}"#,
            ),
        ] {
            let judge = Arc::new(crate::flow::fixtures::CountingClassifier::silent());
            let (gateway, mut recorded) = arbitrating_gateway(
                judge,
                asking_only(answered(ruling)),
                Some(arbiter(std::time::Duration::from_secs(5))),
            );
            let (row, _) = asked(&gateway, &mut recorded, context(granted()), opening()).await;
            assert_eq!(
                row.dispatched_class,
                Some(ip_core::Difficulty::Hard),
                "{family} answered a ruling the gateway could not read"
            );
        }
    }

    /// A conversation the fixtures above judge, as a caller would send it.
    fn opening() -> Request<GatewayBody> {
        request(
            "/anthropic/messages",
            &conversation_with_system("you audit accounts", &[("user", OPENING_PROMPT)]),
        )
    }

    #[tokio::test]
    async fn a_class_the_request_named_is_decided_by_the_request() {
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::naming(
            ip_core::Difficulty::Easy,
        ));
        let (gateway, mut recorded) =
            arbitrating_gateway(judge.clone(), asking_only(answered(RULING)), None);
        let mut named = opening();
        named
            .headers_mut()
            .insert("x-ip-difficulty", HeaderValue::from_static("hard"));
        let (row, _) = asked(&gateway, &mut recorded, context(granted()), named).await;
        assert_eq!(row.dispatched_class, Some(ip_core::Difficulty::Hard));
        assert_eq!(row.decided_by, Some(ip_core::Decider::Request));
        assert_eq!(
            judge.calls(),
            0,
            "a request that named its class is judged by nothing"
        );
    }

    /// The rule a reader gets wrong: the second request of a turn is decided by the
    /// turn, not by the classifier that ran for the first. Counting classifier rows is
    /// how an operator reads their real local call rate, and a remembered verdict
    /// recorded as a classifier call would inflate it by the length of every
    /// conversation.
    #[tokio::test]
    async fn a_remembered_class_is_decided_by_the_turn_not_the_judge_that_named_it() {
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::naming(
            ip_core::Difficulty::Hard,
        ));
        let (gateway, mut recorded) =
            arbitrating_gateway(judge.clone(), asking_only(answered(RULING)), None);
        let (first, _) = asked(&gateway, &mut recorded, context(granted()), opening()).await;
        assert_eq!(first.dispatched_class, Some(ip_core::Difficulty::Hard));
        assert_eq!(
            first.decided_by,
            Some(ip_core::Decider::Classifier),
            "the opening was judged locally"
        );

        let (second, _) = asked(&gateway, &mut recorded, context(granted()), opening()).await;
        assert_eq!(second.dispatched_class, Some(ip_core::Difficulty::Hard));
        assert_eq!(
            second.decided_by,
            Some(ip_core::Decider::Turn),
            "the turn remembered it; the judge was not asked again"
        );
        assert_eq!(judge.calls(), 1, "one judgement across the conversation");
    }

    #[tokio::test]
    async fn a_class_the_arbiter_ruled_is_decided_by_the_arbiter() {
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::silent());
        let (gateway, mut recorded) = arbitrating_gateway(
            judge,
            asking_only(answered(RULING)),
            Some(arbiter(std::time::Duration::from_secs(5))),
        );
        let (row, _) = asked(&gateway, &mut recorded, context(granted()), opening()).await;
        assert_eq!(row.dispatched_class, Some(ip_core::Difficulty::Hard));
        assert_eq!(row.decided_by, Some(ip_core::Decider::Arbiter));
    }

    #[tokio::test]
    async fn a_request_nothing_could_decide_records_neither() {
        let judge = Arc::new(crate::flow::fixtures::CountingClassifier::silent());
        let (gateway, mut recorded) =
            arbitrating_gateway(judge, asking_only(answered(RULING)), None);
        let (row, _) = asked(&gateway, &mut recorded, context(granted()), opening()).await;
        assert_eq!(row.dispatched_class, None);
        assert_eq!(
            row.decided_by, None,
            "no class, and so no level that named one"
        );
    }

    /// One request sent, its answer drained (recording finishes with the body), and the
    /// row it left behind.
    async fn ask(
        gateway: &Gateway,
        recorded: &mut tokio::sync::mpsc::UnboundedReceiver<ip_storage::Usage>,
        context: RequestContext,
        request: Request<GatewayBody>,
    ) -> ip_storage::Usage {
        let answered = gateway.handle(context, request).await.unwrap();
        body_of(answered).await;
        gateway.settled().await;
        recorded.recv().await.expect("a row per request")
    }

    #[tokio::test]
    async fn a_turns_second_request_dispatches_at_the_class_its_first_named() {
        let upstream = FakeUpstream::answering("pong");
        let (gateway, mut recorded) = recording_turns(&upstream);
        ask(
            &gateway,
            &mut recorded,
            in_turn(granted(), "turn-1"),
            class_request("/anthropic/messages", "ask the first", "hard"),
        )
        .await;
        // The turn's memory fills a silence: the second request names nothing, and the
        // class of its turn dispatches it.
        let second = ask(
            &gateway,
            &mut recorded,
            in_turn(granted(), "turn-1"),
            request("/anthropic/messages", "and again"),
        )
        .await;
        assert_eq!(second.dispatched_class, Some(ip_core::Difficulty::Hard));
        let hosts = hosts_of(&upstream);
        assert_eq!(hosts[0], hosts[1], "the turn's landing holds as ever");
    }

    #[tokio::test]
    async fn a_second_request_naming_its_own_class_outremembers_the_turn() {
        let upstream = FakeUpstream::answering("pong");
        let (gateway, mut recorded) = recording_turns(&upstream);
        ask(
            &gateway,
            &mut recorded,
            in_turn(granted(), "turn-1"),
            class_request("/anthropic/messages", "hard opener", "hard"),
        )
        .await;
        let second = ask(
            &gateway,
            &mut recorded,
            in_turn(granted(), "turn-1"),
            class_request("/anthropic/messages", "and now easy", "easy"),
        )
        .await;
        assert_eq!(second.dispatched_class, Some(ip_core::Difficulty::Easy));
        // Ruled both ways: a class the request names is believed, and the endpoint it
        // reached before the class was ever outremembered — the landing still holds.
        let hosts = hosts_of(&upstream);
        assert_eq!(hosts[0], hosts[1]);
    }

    #[tokio::test]
    async fn a_class_survives_the_loss_of_the_endpoint_it_was_remembered_with() {
        let upstream = FakeUpstream::answering("pong");
        // One serves routine at most, two research. A turn that opened research can only
        // have landed on two; the point of the test is that losing the endpoint does not
        // lose the class — it still dispatches at research wherever research lives, never
        // unclassified through one.
        let (recorder, mut recorded) = Ledger::new();
        let gateway = Gateway::new(
            capped_table(&[
                ("one.example.com", ip_core::Difficulty::Routine),
                ("two.example.com", ip_core::Difficulty::Research),
            ]),
            ProcessorChain::new(),
            upstream.clone(),
        )
        .sticky_turns(Arc::new(ip_cache::SledCache::temporary().unwrap()))
        .recording(recorder);
        ask(
            &gateway,
            &mut recorded,
            in_turn(granted(), "turn-1"),
            class_request("/anthropic/messages", "research on two", "research"),
        )
        .await;
        // Drain the landing away: the endpoint stands behind the rule no longer.
        // The table now stands behind one alone: two, the landing and the ceiling it
        // served, is gone from the rule entirely.
        gateway.replace(
            capped_table(&[("one.example.com", ip_core::Difficulty::Routine)]),
            Arc::new(FixedChains::new(ProcessorChain::new())),
            Limits::default(),
        );
        let second = ask(
            &gateway,
            &mut recorded,
            in_turn(granted(), "turn-1"),
            request("/anthropic/messages", "more research"),
        )
        .await;
        assert_eq!(second.dispatched_class, Some(ip_core::Difficulty::Research));
        assert_eq!(hosts_of(&upstream)[1], "one.example.com:443");
    }

    #[tokio::test]
    async fn a_compaction_ends_the_class_with_the_turn() {
        let upstream = FakeUpstream::answering("pong");
        let (gateway, mut recorded) = recording_turns(&upstream);
        // Unmarked requests: the conversation's opening names the turn, and five messages
        // that shrink to four are §429's compaction — the turn ends, and a fresh one that
        // nothing classified begins.
        let opening = r#"{"model":"claude-opus-5","messages":[
            {"role":"user","content":"m1"},{"role":"user","content":"m2"},
            {"role":"user","content":"m3"},{"role":"user","content":"m4"},
            {"role":"user","content":"m5"}]}"#;
        let shrunk = r#"{"model":"claude-opus-5","messages":[
            {"role":"user","content":"m1"},{"role":"user","content":"m2"},
            {"role":"user","content":"m3"},{"role":"user","content":"m4"}]}"#;
        let first = ask(
            &gateway,
            &mut recorded,
            context(granted()),
            class_request("/anthropic/messages", opening, "hard"),
        )
        .await;
        assert_eq!(first.dispatched_class, Some(ip_core::Difficulty::Hard));
        let second = ask(
            &gateway,
            &mut recorded,
            context(granted()),
            request("/anthropic/messages", shrunk),
        )
        .await;
        // The turn ended with its own; the request that starts the new one is classified
        // afresh — which today, with nothing classifying, is no class at all.
        assert_eq!(second.dispatched_class, None);
    }

    // ——— the shape of the answer above, held to the letter (M13) ———

    #[tokio::test]
    async fn three_silences_all_dispatch_at_the_class_the_turn_remembers() {
        // One naming, three answers at it: the class rides every request of the turn,
        // and the turn keeps re-remembering what it was dispatched at so a second
        // silence finds the same verdict the first did.
        let upstream = FakeUpstream::answering("pong");
        let (gateway, mut recorded) = recording_turns(&upstream);
        ask(
            &gateway,
            &mut recorded,
            in_turn(granted(), "turn-1"),
            class_request("/anthropic/messages", "routine opener", "routine"),
        )
        .await;
        for round in 0..2 {
            let row = ask(
                &gateway,
                &mut recorded,
                in_turn(granted(), "turn-1"),
                request("/anthropic/messages", "silence"),
            )
            .await;
            assert_eq!(
                row.dispatched_class,
                Some(ip_core::Difficulty::Routine),
                "the {round}th silence after a named turn dispatched unclassified"
            );
        }
        // And the turn was served by its landing the whole way, exactly once each.
        assert_eq!(hosts_of(&upstream).len(), 3);
    }

    #[tokio::test]
    async fn a_named_class_overrules_a_remembered_one_for_that_request_only() {
        // Statement, silence, statement: the turn survives every statement's passing
        // without adopting it. The turn's own class is routine throughout; the middle
        // easy request dispatched easy, and the silence after it rode routine again.
        let upstream = FakeUpstream::answering("pong");
        let (gateway, mut recorded) = recording_turns(&upstream);
        ask(
            &gateway,
            &mut recorded,
            in_turn(granted(), "turn-1"),
            class_request("/anthropic/messages", "opener", "routine"),
        )
        .await;
        let easy = ask(
            &gateway,
            &mut recorded,
            in_turn(granted(), "turn-1"),
            class_request("/anthropic/messages", "easy detour", "easy"),
        )
        .await;
        assert_eq!(easy.dispatched_class, Some(ip_core::Difficulty::Easy));
        let after = ask(
            &gateway,
            &mut recorded,
            in_turn(granted(), "turn-1"),
            request("/anthropic/messages", "back to it"),
        )
        .await;
        // The turn takes up the last statement's class, this implementation's shape:
        // statements do not fight the turn's memory (they dispatch at themselves, as the
        // row shows), and what the turn rides afterwards is the class its most recent
        // naming left behind.
        assert_eq!(after.dispatched_class, Some(ip_core::Difficulty::Easy));
    }

    #[tokio::test]
    async fn two_turns_carry_two_classes_without_touching_each_other() {
        // The sticky memory holds two verdicts, one per conversation, each named once:
        // neither turn reads the other's class and each second request dispatches at
        // its own.
        let upstream = FakeUpstream::answering("pong");
        let (gateway, mut recorded) = recording_turns(&upstream);
        ask(
            &gateway,
            &mut recorded,
            in_turn(granted(), "turn-a"),
            class_request("/anthropic/messages", "a opens routine", "routine"),
        )
        .await;
        ask(
            &gateway,
            &mut recorded,
            in_turn(granted(), "turn-b"),
            class_request("/anthropic/messages", "b opens research", "research"),
        )
        .await;
        let a = ask(
            &gateway,
            &mut recorded,
            in_turn(granted(), "turn-a"),
            request("/anthropic/messages", "a asks again"),
        )
        .await;
        let b = ask(
            &gateway,
            &mut recorded,
            in_turn(granted(), "turn-b"),
            request("/anthropic/messages", "b asks again"),
        )
        .await;
        assert_eq!(a.dispatched_class, Some(ip_core::Difficulty::Routine));
        assert_eq!(b.dispatched_class, Some(ip_core::Difficulty::Research));
    }

    #[tokio::test]
    async fn a_request_on_no_turn_at_all_behaves_as_it_always_did() {
        // The regression guard: a request alone dispatches by the rule and records the
        // class it named, which is all a request without a turn has ever done.
        let upstream = FakeUpstream::answering("pong");
        let (recorder, mut recorded) = Ledger::new();
        let gateway = Gateway::new(
            replicated_table(ip_core::Strategy::RoundRobin),
            ProcessorChain::new(),
            upstream.clone(),
        )
        .recording(recorder);
        for _ in 0..3 {
            ask(
                &gateway,
                &mut recorded,
                context(granted()),
                class_request("/anthropic", "ask", "easy"),
            )
            .await;
        }
        // Round robin over two, held exact: nobody turns this request's plan, so the
        // rotation turns exactly as it did before any of this.
        assert_eq!(
            hosts_of(&upstream),
            vec![
                "one.example.com:443",
                "two.example.com:443",
                "one.example.com:443",
            ]
        );
    }

    #[tokio::test]
    async fn a_row_names_a_class_the_turn_remembers_as_its_own_dispatch() {
        // The record reads what went upstream at, not only what the header named: the
        // silence's row names a class its own request never spoke.
        let upstream = FakeUpstream::answering("pong");
        let (gateway, mut recorded) = recording_turns(&upstream);
        let first = ask(
            &gateway,
            &mut recorded,
            in_turn(granted(), "turn-1"),
            class_request("/anthropic/messages", "open at median", "median"),
        )
        .await;
        let second = ask(
            &gateway,
            &mut recorded,
            in_turn(granted(), "turn-1"),
            request("/anthropic/messages", "silence"),
        )
        .await;
        assert_eq!(first.dispatched_class, Some(ip_core::Difficulty::Median));
        assert_eq!(second.dispatched_class, Some(ip_core::Difficulty::Median));
        assert_ne!(
            first.trace, second.trace,
            "two requests, one turn, one class between them"
        );
    }
}
