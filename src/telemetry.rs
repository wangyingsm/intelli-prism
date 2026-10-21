//! Logs, traces and metrics: the subscriber the process installs, and the collector it
//! exports to.
//!
//! Traces say what happened to one request. Metrics say what has been happening to all of
//! them, which is a different question and the only one a dashboard can ask: no amount of
//! sampled spans tells an operator their cache hit rate. Logs say what the process itself
//! had to report, which neither of the other two carries. All three go to the same
//! collector, because an operator configuring one endpoint has configured observability.

use std::future::Future;

use ip_config::{LogFile, LogsKept, SampleRatio, TelemetryConfig};
use ip_core::TraceId;
use opentelemetry::trace::{SpanId, TracerProvider as _};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{ExporterBuildError, WithExportConfig};
use opentelemetry_prometheus::PrometheusExporter;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkError;
use opentelemetry_sdk::logs::{LogExporter, SdkLoggerProvider};
use opentelemetry_sdk::metrics::SdkMeterProvider;
#[cfg(test)]
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::trace::{
    IdGenerator, RandomIdGenerator, Sampler, SdkTracerProvider, SpanExporter,
};
use prometheus::{Encoder, Registry, TextEncoder};
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_appender::rolling::{InitError, RollingFileAppender, Rotation};
use tracing_subscriber::Layer as _;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use url::Url;

/// What the service calls itself to the collector.
pub(crate) const SERVICE: &str = "intelli-prism";

tokio::task_local! {
    /// The trace the request being served was given, which the id generator names its trace by.
    static DRAWN: TraceId;
}

/// Serves `work` under the trace the edge drew, so what is exported carries the id the caller
/// was answered with rather than one the exporter invented.
pub async fn under<F: Future>(trace: TraceId, work: F) -> F::Output {
    DRAWN.scope(trace, work).await
}

/// What the process exports through, held until it ends.
///
/// Every provider is held rather than only registered, because each has to be told to
/// flush: a gateway that stops without one has thrown away whatever it had gathered since
/// the last export, and for metrics that is up to a whole interval of them.
pub struct Telemetry {
    provider: Option<SdkTracerProvider>,
    meter: Option<SdkMeterProvider>,
    scrape: Option<Registry>,
    logger: Option<SdkLoggerProvider>,
    /// Never read: held so the thread writing the file lives as long as the process,
    /// and flushes what is buffered when it does not.
    _rolling: Option<WorkerGuard>,
}

impl Telemetry {
    /// Sends what is recorded but not yet exported, and stops exporting.
    ///
    /// Each is flushed even if the others will not: one collector refusing a batch is no
    /// reason to drop the batch beside it.
    pub fn shutdown(&self) {
        if let Some(provider) = &self.provider
            && let Err(error) = provider.shutdown()
        {
            tracing::warn!(%error, "could not flush the traces");
        }
        if let Some(meter) = &self.meter
            && let Err(error) = meter.shutdown()
        {
            tracing::warn!(%error, "could not flush the metrics");
        }
        if let Some(logger) = &self.logger
            && let Err(error) = logger.shutdown()
        {
            tracing::warn!(%error, "could not flush the logs");
        }
    }

    /// The numbers as a scraper reads them, and nothing where none was configured.
    ///
    /// Gathered on demand rather than kept: what a scrape answers is the state of the
    /// instruments at the moment it asked, which is the whole difference between
    /// being scraped and pushing.
    #[must_use]
    pub fn gathered(&self) -> Option<String> {
        let registry = self.scrape.as_ref()?;
        let mut written = Vec::new();
        if let Err(error) = TextEncoder::new().encode(&registry.gather(), &mut written) {
            tracing::warn!(%error, "could not write the metrics out for a scrape");
            return None;
        }
        String::from_utf8(written).ok()
    }
}

/// Installs the log subscriber for the process, exporting logs, traces and metrics when
/// a collector is configured.
///
/// A collector that cannot be reached leaves the printed logs alone: the server serves
/// without the export rather than refusing to start, and whatever `docker logs` showed
/// before shows the same now. The three are built separately on purpose — a collector
/// that takes spans and refuses metrics, or any other pairing, leaves the gateway with
/// whichever of them works instead of none.
pub fn install(config: &TelemetryConfig) -> Telemetry {
    let rolled = config
        .log_file
        .as_ref()
        .map(|file| rolling(file, config.log_keep));
    let traced = config
        .otlp_endpoint
        .as_ref()
        .map(|endpoint| exporting(endpoint, config.sample_ratio));
    let shipped = config.otlp_endpoint.as_ref().map(shipping);
    let written = worked(&rolled).map(|rolled| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(rolled.writer.clone())
    });
    let traces = worked(&traced)
        .map(|provider| tracing_opentelemetry::layer().with_tracer(provider.tracer(SERVICE)));
    let lines = worked(&shipped)
        .map(|provider| OpenTelemetryTracingBridge::new(provider).with_filter(ours()));
    tracing_subscriber::registry()
        .with(LevelFilter::from(config.log_level))
        .with(tracing_subscriber::fmt::layer())
        .with(written)
        .with(traces)
        .with(lines)
        .init();
    if let (Some(endpoint), Some(traced)) = (&config.otlp_endpoint, &traced) {
        match traced {
            Ok(_) => {
                tracing::info!(%endpoint, ratio = config.sample_ratio.get(), "exporting traces");
            }
            Err(error) => {
                tracing::error!(%error, %endpoint, "could not export traces to the collector");
            }
        }
    }
    if let (Some(endpoint), Some(shipped)) = (&config.otlp_endpoint, &shipped) {
        match shipped {
            Ok(_) => tracing::info!(%endpoint, "exporting logs"),
            Err(error) => {
                tracing::error!(%error, %endpoint, "could not export logs to the collector");
            }
        }
    }
    if let (Some(file), Some(rolled)) = (&config.log_file, &rolled) {
        let path = file.directory().join(file.name());
        match rolled {
            Ok(_) => {
                tracing::info!(path = %path.display(), keep = config.log_keep.get(), "writing logs to a file");
            }
            Err(error) => {
                tracing::error!(%error, path = %path.display(), "could not open the log file");
            }
        }
    }
    let metered = metering(config);
    if let Some(metered) = &metered {
        // Registered globally, unlike the tracer, because that is how an instrument is
        // reached: nothing carries a counter to its meter the way the tracing layer
        // carries a span to its provider.
        opentelemetry::global::set_meter_provider(metered.meter.clone());
        tracing::info!(
            pushed = config.otlp_endpoint.is_some(),
            scraped = metered.registry.is_some(),
            "counting what the gateway does"
        );
    }
    let (meter, scrape) = match metered {
        Some(metered) => (Some(metered.meter), metered.registry),
        None => (None, None),
    };
    Telemetry {
        provider: traced.and_then(Result::ok),
        meter,
        scrape,
        logger: shipped.and_then(Result::ok),
        _rolling: rolled.and_then(Result::ok).map(|rolled| rolled.guard),
    }
}

/// What an attempt that was made and worked produced, and nothing for one that was not
/// made or did not.
fn worked<T, E>(outcome: &Option<Result<T, E>>) -> Option<&T> {
    outcome.as_ref().and_then(|outcome| outcome.as_ref().ok())
}

/// The file the lines are also written to, and the guard that keeps the thread writing
/// them alive.
struct Rolling {
    writer: NonBlocking,
    guard: WorkerGuard,
}

/// Opens the file the logs roll in: a new one each day, as many kept as were asked for,
/// and the writing done off the thread that logged.
fn rolling(file: &LogFile, kept: LogsKept) -> Result<Rolling, InitError> {
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(file.name())
        .max_log_files(kept.get())
        .build(file.directory())?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    Ok(Rolling { writer, guard })
}

/// The machinery an export runs on, whose own logs the bridge must not carry.
const PLUMBING: [&str; 5] = ["opentelemetry", "tonic", "hyper", "h2", "tower"];

/// What the bridge ships: everything the process logs except what the export itself logs.
///
/// A collector that starts refusing batches makes the exporter say so, and a bridge that
/// shipped that line would hand it straight back to the exporter it came from. A failure
/// that feeds itself is the one failure a log pipeline must not have, so these targets are
/// printed and never exported.
fn ours() -> Targets {
    PLUMBING.iter().fold(
        Targets::new().with_default(LevelFilter::TRACE),
        |targets, target| targets.with_target(*target, LevelFilter::OFF),
    )
}

/// Builds the log exporter the configured collector is reached over, over the same
/// transport the spans take.
fn shipping(endpoint: &Url) -> Result<SdkLoggerProvider, ExporterBuildError> {
    let exporter = opentelemetry_otlp::LogExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint.as_str())
        .build()?;
    Ok(logging(exporter))
}

/// The logger provider itself: a batched exporter under our service name, which is what a
/// collector files the lines against.
pub(crate) fn logging<E: LogExporter + 'static>(exporter: E) -> SdkLoggerProvider {
    SdkLoggerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(Resource::builder().with_service_name(SERVICE).build())
        .build()
}

#[cfg(test)]
impl Telemetry {
    /// A process that only gathers for a scrape, for a test that only reads one.
    pub(crate) fn gathering_into(registry: Registry) -> Self {
        Self {
            provider: None,
            meter: None,
            scrape: Some(registry),
            logger: None,
            _rolling: None,
        }
    }

    /// A process exporting nothing at all, which is every deployment that configured
    /// no collector and no scrape.
    pub(crate) fn quiet() -> Self {
        Self {
            provider: None,
            meter: None,
            scrape: None,
            logger: None,
            _rolling: None,
        }
    }
}

/// The meter provider and, when a scrape was configured, the registry it gathers into.
pub(crate) struct Metering {
    meter: SdkMeterProvider,
    registry: Option<Registry>,
}

/// The meter provider, built over whichever readers were configured, and nothing at all
/// where neither was.
///
/// Both may be on at once: a push to a collector and a scrape of the same numbers are
/// two ways of asking the same provider, not two providers. Nothing is built where
/// neither was configured or neither would build, which leaves the global the no-op the
/// sdk starts with, so every instrument made against it costs nothing.
fn metering(config: &TelemetryConfig) -> Option<Metering> {
    let pushed = match config.otlp_endpoint.as_ref().map(exporter) {
        Some(Err(error)) => {
            tracing::error!(%error, "could not export metrics to the collector");
            None
        }
        pushed => pushed.and_then(Result::ok),
    };
    let pulled = match config.scrape.as_ref().map(|_| gathering()) {
        Some(Err(error)) => {
            tracing::error!(%error, "could not gather the metrics for a scrape");
            None
        }
        pulled => pulled.and_then(Result::ok),
    };
    if pushed.is_none() && pulled.is_none() {
        return None;
    }
    let mut builder = SdkMeterProvider::builder().with_resource(named());
    if let Some(exporter) = pushed {
        builder = builder.with_periodic_exporter(exporter);
    }
    let mut registry = None;
    if let Some((reader, gathers)) = pulled {
        builder = builder.with_reader(reader);
        registry = Some(gathers);
    }
    Some(Metering {
        meter: builder.build(),
        registry,
    })
}

/// The reader a scrape is answered from, and the registry it files the numbers in.
pub(crate) fn gathering() -> Result<(PrometheusExporter, Registry), OTelSdkError> {
    let registry = Registry::new();
    let reader = opentelemetry_prometheus::exporter()
        .with_registry(registry.clone())
        .build()?;
    Ok((reader, registry))
}

/// A provider that only pushes, which is what [`metering`] builds when only a collector
/// is configured and what a test asserting the push half builds directly.
#[cfg(test)]
pub(crate) fn meters<E: PushMetricExporter + 'static>(exporter: E) -> SdkMeterProvider {
    SdkMeterProvider::builder()
        .with_periodic_exporter(exporter)
        .with_resource(named())
        .build()
}

/// What the service calls itself to whatever reads its telemetry.
fn named() -> Resource {
    Resource::builder().with_service_name(SERVICE).build()
}

/// The metric exporter the configured collector is reached over, over the same transport
/// the spans take.
fn exporter(endpoint: &Url) -> Result<opentelemetry_otlp::MetricExporter, ExporterBuildError> {
    opentelemetry_otlp::MetricExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint.as_str())
        .build()
}

/// Builds the exporter the configured collector is reached over.
fn exporting(
    endpoint: &Url,
    ratio: SampleRatio,
) -> Result<SdkTracerProvider, opentelemetry_otlp::ExporterBuildError> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint.as_str())
        .build()?;
    Ok(provider(exporter, ratio))
}

/// The tracer provider itself: our ids, the configured share of them, under our service name.
pub(crate) fn provider<E: SpanExporter + 'static>(
    exporter: E,
    ratio: SampleRatio,
) -> SdkTracerProvider {
    SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_sampler(Sampler::TraceIdRatioBased(ratio.get()))
        .with_id_generator(Drawn::default())
        .with_resource(Resource::builder().with_service_name(SERVICE).build())
        .build()
}

/// Names each trace by the id the edge drew for the request, and each span at random.
#[derive(Debug, Default)]
struct Drawn(RandomIdGenerator);

impl IdGenerator for Drawn {
    fn new_trace_id(&self) -> opentelemetry::trace::TraceId {
        DRAWN
            .try_with(|trace| opentelemetry::trace::TraceId::from_bytes(*trace.as_bytes()))
            .unwrap_or_else(|_| self.0.new_trace_id())
    }

    fn new_span_id(&self) -> SpanId {
        self.0.new_span_id()
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;
    use ip_config::ScrapeConfig;
    use opentelemetry::logs::{AnyValue, Severity};
    use opentelemetry_sdk::logs::InMemoryLogExporter;
    use opentelemetry_sdk::trace::InMemorySpanExporter;
    use tracing_subscriber::Registry;

    /// Records what a provider of ours exports, under the subscriber a test installs for itself.
    fn watching(
        ratio: f64,
    ) -> (
        InMemorySpanExporter,
        SdkTracerProvider,
        impl tracing::Subscriber,
    ) {
        let exporter = InMemorySpanExporter::default();
        let provider = provider(exporter.clone(), SampleRatio::new(ratio).unwrap());
        let subscriber = Registry::default()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer(SERVICE)));
        (exporter, provider, subscriber)
    }

    #[test]
    fn a_span_is_exported_under_the_trace_the_request_was_answered_with() {
        let trace = TraceId::generate().unwrap();
        let (exporter, provider, subscriber) = watching(SampleRatio::ALL);
        tracing::subscriber::with_default(subscriber, || {
            futures(under(trace, async {
                tracing::info_span!("request").in_scope(|| {});
            }));
        });
        provider.force_flush().unwrap();

        let exported = exporter.get_finished_spans().unwrap();
        assert_eq!(exported.len(), 1);
        assert_eq!(
            exported[0].span_context.trace_id().to_string(),
            trace.to_hex()
        );
    }

    #[test]
    fn a_ratio_of_none_exports_nothing() {
        let trace = TraceId::generate().unwrap();
        let (exporter, provider, subscriber) = watching(SampleRatio::NONE);
        tracing::subscriber::with_default(subscriber, || {
            futures(under(trace, async {
                tracing::info_span!("request").in_scope(|| {});
            }));
        });
        provider.force_flush().unwrap();

        assert!(exporter.get_finished_spans().unwrap().is_empty());
    }

    #[test]
    fn a_span_opened_outside_a_request_is_traced_all_the_same() {
        let (exporter, provider, subscriber) = watching(SampleRatio::ALL);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info_span!("startup").in_scope(|| {});
        });
        provider.force_flush().unwrap();

        let exported = exporter.get_finished_spans().unwrap();
        assert_eq!(exported.len(), 1);
        assert!(exported[0].span_context.trace_id() != opentelemetry::trace::TraceId::INVALID);
    }

    /// What a counter made against a provider of our shape actually reaches, which is
    /// the whole of what this commit does: an instrument, a flush, and a number at the
    /// far end carrying the labels it was given.
    #[test]
    fn a_counter_reaches_the_exporter_when_the_process_stops() {
        use opentelemetry::KeyValue;
        use opentelemetry::metrics::MeterProvider as _;
        use opentelemetry_sdk::metrics::InMemoryMetricExporter;
        use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};

        let exporter = InMemoryMetricExporter::default();
        let provider = meters(exporter.clone());
        let counter = provider
            .meter(SERVICE)
            .u64_counter("ip_asked_total")
            .build();
        counter.add(2, &[KeyValue::new("tenant", "acme")]);
        counter.add(1, &[KeyValue::new("tenant", "globex")]);
        // Stopped the way the process stops it, rather than by reaching for the
        // provider: what is being asserted is that a gateway shutting down does not
        // throw away the interval it had gathered, and that is `Telemetry`'s job.
        //
        // Bound rather than dropped on this line, and every assertion below made while
        // it is still alive. A provider flushes when it drops, so a temporary here
        // would export the same numbers whether or not `shutdown` touched the meter at
        // all — the test would pass with the flush deleted, which it must not.
        let telemetry = Telemetry {
            provider: None,
            meter: Some(provider),
            scrape: None,
            logger: None,
            _rolling: None,
        };
        telemetry.shutdown();

        let exported = exporter.get_finished_metrics().unwrap();
        assert!(
            !exported.is_empty(),
            "stopping the process should have flushed what was gathered"
        );
        assert!(
            exported.iter().any(|held| {
                held.resource()
                    .iter()
                    .any(|(key, value)| key.as_str() == "service.name" && value.as_str() == SERVICE)
            }),
            "the collector files these under the service that sent them"
        );
        let scopes: Vec<_> = exported
            .iter()
            .flat_map(|held| held.scope_metrics())
            .collect();
        let metric = scopes
            .iter()
            .flat_map(|scope| scope.metrics())
            .find(|metric| metric.name() == "ip_asked_total")
            .expect("the counter reached the exporter");
        let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else {
            panic!("a counter exports as a sum");
        };
        let mut by_tenant: Vec<(String, u64)> = sum
            .data_points()
            .map(|point| {
                let tenant = point
                    .attributes()
                    .find(|at| at.key.as_str() == "tenant")
                    .expect("the label it was given")
                    .value
                    .to_string();
                (tenant, point.value())
            })
            .collect();
        by_tenant.sort();
        assert_eq!(
            by_tenant,
            vec![("acme".to_owned(), 2), ("globex".to_owned(), 1)],
            "each label is its own series, which is what the tenant ruling costs and buys"
        );
        // Everything above was read while this was alive, so none of it can have been
        // the provider's own flush-on-drop. Dropped here, deliberately and last.
        drop(telemetry);
    }

    /// A process nobody configured a collector for exports neither, and stopping is
    /// still well-behaved rather than a panic on an absent provider.
    #[test]
    fn an_unconfigured_process_exports_nothing_and_stops_cleanly() {
        let quiet = Telemetry {
            provider: None,
            meter: None,
            scrape: None,
            logger: None,
            _rolling: None,
        };
        quiet.shutdown();
    }

    /// Records what the bridge ships, under the subscriber a test installs for itself.
    fn listening() -> (
        InMemoryLogExporter,
        SdkLoggerProvider,
        impl tracing::Subscriber,
    ) {
        let exporter = InMemoryLogExporter::default();
        let provider = logging(exporter.clone());
        let subscriber = Registry::default()
            .with(OpenTelemetryTracingBridge::new(&provider).with_filter(ours()));
        (exporter, provider, subscriber)
    }

    /// What one log line becomes at the far end, which is the whole of what this commit
    /// does: a line the process wrote, carrying its level and its words, filed under the
    /// service that wrote it.
    #[test]
    fn a_log_line_reaches_the_collector() {
        let (exporter, provider, subscriber) = listening();
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(target: "ip_gateway::flow", "the upstream refused");
        });
        provider.force_flush().unwrap();

        let shipped = exporter.get_emitted_logs().unwrap();
        assert_eq!(shipped.len(), 1);
        let line = &shipped[0];
        assert_eq!(line.record.severity_number(), Some(Severity::Error));
        assert_eq!(
            line.record.target().map(Cow::as_ref),
            Some("ip_gateway::flow")
        );
        let Some(AnyValue::String(body)) = line.record.body() else {
            panic!("a line carries what was written");
        };
        assert_eq!(body.as_str(), "the upstream refused");
        assert!(
            line.resource
                .iter()
                .any(|(key, value)| key.as_str() == "service.name" && value.as_str() == SERVICE),
            "the collector files these under the service that sent them"
        );
    }

    /// The guard that keeps a failing export from feeding itself: the exporter's own
    /// complaint is printed and never shipped, while the line beside it is shipped.
    #[test]
    fn the_export_does_not_ship_its_own_complaints() {
        let (exporter, provider, subscriber) = listening();
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(target: "opentelemetry", "the collector refused the batch");
            tracing::error!(target: "tonic::transport", "the channel is gone");
            tracing::error!(target: "ip_gateway", "the upstream refused");
        });
        provider.force_flush().unwrap();

        let shipped = exporter.get_emitted_logs().unwrap();
        let targets: Vec<_> = shipped
            .iter()
            .filter_map(|line| line.record.target().map(Cow::to_string))
            .collect();
        assert_eq!(
            targets,
            vec!["ip_gateway".to_owned()],
            "a line the exporter wrote about its own failure must not go back to it"
        );
    }

    /// Stopping flushes the logs too, and the provider is reached through `Telemetry`
    /// rather than directly, so the assertion is about what the process does on its way
    /// out. Read while it is still alive, because a provider also flushes when dropped.
    #[test]
    fn stopping_flushes_the_logs() {
        let exporter = InMemoryLogExporter::default();
        let telemetry = Telemetry {
            provider: None,
            meter: None,
            scrape: None,
            logger: Some(logging(exporter.clone())),
            _rolling: None,
        };
        telemetry.shutdown();

        assert!(
            exporter.is_shutdown_called(),
            "stopping the process should have flushed what was written"
        );
        drop(telemetry);
    }

    /// Counts one request against a provider and hands back what a scrape would read.
    fn scraped(config: &TelemetryConfig) -> Option<String> {
        use opentelemetry::KeyValue;
        use opentelemetry::metrics::MeterProvider as _;

        let metered = metering(config)?;
        metered
            .meter
            .meter(SERVICE)
            .u64_counter("ip.requests")
            .build()
            .add(1, &[KeyValue::new("tenant", "acme")]);
        Telemetry {
            provider: None,
            meter: Some(metered.meter),
            scrape: metered.registry,
            logger: None,
            _rolling: None,
        }
        .gathered()
    }

    /// A process nobody asked for numbers from builds no provider at all, so the
    /// global stays the no-op the sdk starts with and an instrument costs nothing.
    #[test]
    fn neither_pushing_nor_scraping_builds_no_provider() {
        assert!(metering(&TelemetryConfig::default()).is_none());
    }

    /// A deployment that scrapes and pushes to nobody still counts, which is the whole
    /// point of the endpoint: before this, no collector meant no instruments.
    #[test]
    fn a_scrape_alone_is_enough_to_be_counted() {
        let config = TelemetryConfig {
            scrape: Some(ScrapeConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                token: None,
            }),
            ..TelemetryConfig::default()
        };
        let numbers = scraped(&config).expect("a provider, and a registry to read it from");

        assert!(
            numbers.contains("ip_requests_total"),
            "the sdk's dotted name comes out in Prometheus spelling: {numbers}"
        );
        assert!(numbers.contains(r#"tenant="acme""#), "{numbers}");
    }

    /// Both readers on one provider: the collector's periodic export does not take the
    /// numbers away from the scrape, and neither reader knows about the other.
    #[tokio::test]
    async fn pushing_and_scraping_are_not_an_either_or() {
        let config = TelemetryConfig {
            otlp_endpoint: Some("http://127.0.0.1:4317".parse().unwrap()),
            scrape: Some(ScrapeConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                token: None,
            }),
            ..TelemetryConfig::default()
        };
        let numbers = scraped(&config).expect("a provider built over both readers");

        assert!(
            numbers.contains("ip_requests_total"),
            "a collector configured beside the scrape leaves the scrape answering: {numbers}"
        );
    }

    /// A deployment that only pushes gathers nothing, and says so rather than serving
    /// an empty page that reads like a quiet gateway.
    #[tokio::test]
    async fn pushing_alone_gathers_nothing_to_scrape() {
        let config = TelemetryConfig {
            otlp_endpoint: Some("http://127.0.0.1:4317".parse().unwrap()),
            ..TelemetryConfig::default()
        };
        let metered = metering(&config).expect("a provider for the collector");

        assert!(
            metered.registry.is_none(),
            "nothing gathers where nobody asked to be scraped"
        );
    }

    /// What a standalone deployment gets instead of a collector: the line is on disk,
    /// where it outlives the process that wrote it.
    #[test]
    fn a_line_is_written_to_the_rolling_file() {
        let at = std::env::temp_dir().join(format!("ip-rolling-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&at);
        std::fs::create_dir_all(&at).unwrap();
        let file = LogFile::new(at.join("gateway.log")).unwrap();

        let rolled = rolling(&file, LogsKept::default()).unwrap();
        let subscriber = Registry::default().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(rolled.writer.clone()),
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!("the upstream refused");
        });
        // The writing happens on another thread, and dropping the guard is what waits
        // for it: read before this and the file is empty for reasons of timing.
        drop(rolled);

        let written: Vec<_> = std::fs::read_dir(&at)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(written.len(), 1, "one file a day, and this is the first");
        let name = written[0].file_name().unwrap().to_str().unwrap().to_owned();
        assert!(
            name.starts_with("gateway.log."),
            "the day is written into the name: {name}"
        );
        let held = std::fs::read_to_string(&written[0]).unwrap();
        assert!(held.contains("the upstream refused"), "{held}");
        assert!(
            held.contains("ERROR"),
            "the level is written beside it: {held}"
        );
        let _ = std::fs::remove_dir_all(&at);
    }

    /// A file that cannot be opened is reported and nothing else: the printed logs are
    /// untouched and the server serves.
    #[test]
    fn a_file_that_cannot_be_opened_is_refused_rather_than_made() {
        let at = std::env::temp_dir().join(format!("ip-rolling-gone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&at);
        std::fs::create_dir_all(&at).unwrap();
        let file = LogFile::new(at.join("gateway.log")).unwrap();
        std::fs::remove_dir_all(&at).unwrap();
        std::fs::write(&at, b"not a directory").unwrap();

        assert!(rolling(&file, LogsKept::default()).is_err());
        let _ = std::fs::remove_file(&at);
    }

    /// Drives a future on this thread, so the spans it opens reach the subscriber installed here.
    fn futures<F: Future>(work: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(work)
    }
}
