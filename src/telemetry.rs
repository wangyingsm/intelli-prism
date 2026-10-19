//! Logs, traces and metrics: the subscriber the process installs, and the collector it
//! exports to.
//!
//! Traces say what happened to one request. Metrics say what has been happening to all of
//! them, which is a different question and the only one a dashboard can ask: no amount of
//! sampled spans tells an operator their cache hit rate. Both go to the same collector,
//! because an operator configuring one endpoint has configured observability.

use std::future::Future;

use ip_config::{SampleRatio, TelemetryConfig};
use ip_core::TraceId;
use opentelemetry::trace::{SpanId, TracerProvider as _};
use opentelemetry_otlp::{ExporterBuildError, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{SdkMeterProvider, exporter::PushMetricExporter};
use opentelemetry_sdk::trace::{
    IdGenerator, RandomIdGenerator, Sampler, SdkTracerProvider, SpanExporter,
};
use tracing_subscriber::filter::LevelFilter;
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
/// Both providers are held rather than only registered, because both have to be told to
/// flush: a gateway that stops without one has thrown away whatever it had gathered since
/// the last export, and for metrics that is up to a whole interval of them.
pub struct Telemetry {
    provider: Option<SdkTracerProvider>,
    meter: Option<SdkMeterProvider>,
}

impl Telemetry {
    /// Sends what is recorded but not yet exported, and stops exporting.
    ///
    /// Each is flushed even if the other will not: one collector refusing a batch is no
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
    }
}

/// Installs the log subscriber for the process, exporting traces and metrics when a
/// collector is configured.
///
/// A collector that cannot be reached leaves the logs alone: the server serves without
/// either rather than refusing to start. The two are built separately on purpose — a
/// collector that takes spans and refuses metrics, or the reverse, leaves the gateway
/// with whichever of them works instead of neither.
pub fn install(config: &TelemetryConfig) -> Telemetry {
    let level = LevelFilter::from(config.log_level);
    let logs = tracing_subscriber::registry()
        .with(level)
        .with(tracing_subscriber::fmt::layer());
    let Some(endpoint) = &config.otlp_endpoint else {
        logs.init();
        return Telemetry {
            provider: None,
            meter: None,
        };
    };
    let provider = match exporting(endpoint, config.sample_ratio) {
        Ok(provider) => {
            let traces = tracing_opentelemetry::layer().with_tracer(provider.tracer(SERVICE));
            logs.with(traces).init();
            tracing::info!(%endpoint, ratio = config.sample_ratio.get(), "exporting traces");
            Some(provider)
        }
        Err(error) => {
            logs.init();
            tracing::error!(%error, %endpoint, "could not export traces to the collector");
            None
        }
    };
    Telemetry {
        provider,
        meter: metering(endpoint),
    }
}

/// The meter provider, registered for the process so an instrument can be made wherever
/// one is needed, and handed back so it can be flushed when the process stops.
///
/// Registered globally, unlike the tracer, because that is how an instrument is reached:
/// the tracing layer carries spans to their provider for us, and there is no equivalent
/// carrying a counter to its meter. Nothing is registered when the exporter will not
/// build, so the global stays the no-op the sdk starts with and every instrument made
/// against it costs nothing.
fn metering(endpoint: &Url) -> Option<SdkMeterProvider> {
    match exporter(endpoint) {
        Ok(exporter) => {
            let meter = meters(exporter);
            opentelemetry::global::set_meter_provider(meter.clone());
            tracing::info!(%endpoint, "exporting metrics");
            Some(meter)
        }
        Err(error) => {
            tracing::error!(%error, %endpoint, "could not export metrics to the collector");
            None
        }
    }
}

/// The meter provider itself: a periodic reader over the exporter, under our service
/// name, which is what a collector files the numbers against.
pub(crate) fn meters<E: PushMetricExporter + 'static>(exporter: E) -> SdkMeterProvider {
    SdkMeterProvider::builder()
        .with_periodic_exporter(exporter)
        .with_resource(Resource::builder().with_service_name(SERVICE).build())
        .build()
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
    use super::*;
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
        };
        quiet.shutdown();
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
