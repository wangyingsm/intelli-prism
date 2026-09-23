//! Logs and traces: the subscriber the process installs, and the collector it exports to.

use std::future::Future;

use ip_config::{SampleRatio, TelemetryConfig};
use ip_core::TraceId;
use opentelemetry::trace::{SpanId, TracerProvider as _};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
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

/// What the process exports traces through, held until it ends.
pub struct Telemetry {
    provider: Option<SdkTracerProvider>,
}

impl Telemetry {
    /// Sends what is recorded but not yet exported, and stops exporting.
    pub fn shutdown(&self) {
        if let Some(provider) = &self.provider
            && let Err(error) = provider.shutdown()
        {
            tracing::warn!(%error, "could not flush the traces");
        }
    }
}

/// Installs the log subscriber for the process, exporting traces when a collector is configured.
///
/// A collector that cannot be reached leaves the logs alone: the server serves without traces
/// rather than refusing to start.
pub fn install(config: &TelemetryConfig) -> Telemetry {
    let level = LevelFilter::from(config.log_level);
    let logs = tracing_subscriber::registry()
        .with(level)
        .with(tracing_subscriber::fmt::layer());
    let Some(endpoint) = &config.otlp_endpoint else {
        logs.init();
        return Telemetry { provider: None };
    };
    match exporting(endpoint, config.sample_ratio) {
        Ok(provider) => {
            let traces = tracing_opentelemetry::layer().with_tracer(provider.tracer(SERVICE));
            logs.with(traces).init();
            tracing::info!(%endpoint, ratio = config.sample_ratio.get(), "exporting traces");
            Telemetry {
                provider: Some(provider),
            }
        }
        Err(error) => {
            logs.init();
            tracing::error!(%error, %endpoint, "could not export traces to the collector");
            Telemetry { provider: None }
        }
    }
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

    /// Drives a future on this thread, so the spans it opens reach the subscriber installed here.
    fn futures<F: Future>(work: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(work)
    }
}
