use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use http::HeaderMap;
use ip_core::{Checksum, PluginOrder};
use ip_gateway::{BodyProcessor, HeaderProcessor, ProcessorError};

use crate::abi::Transformed;
use crate::headers::{decode_headers, encode_headers};
use crate::host::PluginHost;

/// A loaded wasm plugin serving as a header processor.
pub struct WasmHeaderProcessor {
    host: Arc<PluginHost>,
    checksum: Checksum,
    order: PluginOrder,
}

impl WasmHeaderProcessor {
    /// Runs the plugin under `checksum`, which must already be loaded into `host`.
    pub fn new(host: Arc<PluginHost>, checksum: Checksum, order: PluginOrder) -> Self {
        Self {
            host,
            checksum,
            order,
        }
    }
}

#[async_trait]
impl HeaderProcessor for WasmHeaderProcessor {
    fn order(&self) -> PluginOrder {
        self.order
    }

    async fn process(&self, headers: &mut HeaderMap) -> Result<(), ProcessorError> {
        let block = run(
            &self.host,
            self.checksum,
            Bytes::from(encode_headers(headers)),
        )
        .await?;
        *headers = decode_headers(&block).map_err(|error| {
            ProcessorError::failed(format!(
                "plugin {} returned headers that do not parse: {error}",
                self.checksum
            ))
        })?;
        Ok(())
    }
}

/// A loaded wasm plugin serving as a body processor.
pub struct WasmBodyProcessor {
    host: Arc<PluginHost>,
    checksum: Checksum,
    order: PluginOrder,
}

impl WasmBodyProcessor {
    /// Runs the plugin under `checksum`, which must already be loaded into `host`.
    pub fn new(host: Arc<PluginHost>, checksum: Checksum, order: PluginOrder) -> Self {
        Self {
            host,
            checksum,
            order,
        }
    }
}

#[async_trait]
impl BodyProcessor for WasmBodyProcessor {
    fn order(&self) -> PluginOrder {
        self.order
    }

    async fn process(&self, body: Bytes) -> Result<Bytes, ProcessorError> {
        run(&self.host, self.checksum, body).await.map(Bytes::from)
    }
}

/// Runs one call on the blocking pool, since a call may compute for its whole deadline.
/// A deliberate refusal stays a refusal; every other way a call ends badly is a failure.
async fn run(
    host: &Arc<PluginHost>,
    checksum: Checksum,
    input: Bytes,
) -> Result<Vec<u8>, ProcessorError> {
    let host = Arc::clone(host);
    // The call leaves this task, so the span goes with it: what a plugin logs belongs to the
    // request that ran it.
    let span = tracing::Span::current();
    let outcome = tokio::task::spawn_blocking(move || {
        span.in_scope(|| host.instantiate(&checksum)?.transform(&input))
    })
    .await
    .map_err(|error| {
        ProcessorError::failed(format!("plugin {checksum} did not finish: {error}"))
    })?;
    match outcome {
        Ok(Transformed::Output(bytes)) => Ok(bytes),
        Ok(Transformed::Refused(reason)) => Err(ProcessorError::refused(reason)),
        Err(error) => Err(ProcessorError::failed(error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use http::HeaderValue;
    use ip_gateway::ProcessorChain;

    use super::*;
    use crate::limits::PluginLimits;
    use crate::testing::*;

    /// Appends one header line, `x-plugin: seen`, to whatever block it is given.
    const APPEND_HEADER: &str = r#"
  (data (i32.const 16) "x-plugin: seen\0d\0a")
  (func (export "transform") (param $ptr i32) (param $len i32) (result i64)
    (local $out i32)
    (local.set $out (call $alloc (i32.add (local.get $len) (i32.const 16))))
    (memory.copy (local.get $out) (local.get $ptr) (local.get $len))
    (memory.copy (i32.add (local.get $out) (local.get $len)) (i32.const 16) (i32.const 16))
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $out)) (i64.const 32))
      (i64.extend_i32_u (i32.add (local.get $len) (i32.const 16)))))"#;

    /// Refuses whatever it is given.
    const REFUSE: &str = r#"
  (data (i32.const 16) "blocked by policy")
  (func (export "transform") (param i32 i32) (result i64)
    (i64.or (i64.shl (i64.const 1) (i64.const 63))
            (i64.or (i64.shl (i64.const 16) (i64.const 32)) (i64.const 17))))"#;

    /// Answers every header block with a line that is not a header.
    const GARBLE: &str = r#"
  (data (i32.const 16) "not a header")
  (func (export "transform") (param i32 i32) (result i64)
    (i64.or (i64.shl (i64.const 16) (i64.const 32)) (i64.const 12)))"#;

    const SPIN: &str = r#"
  (func (export "transform") (param i32 i32) (result i64)
    (loop (br 0))
    (i64.const 0))"#;

    fn plugged(host: &Arc<PluginHost>, transform: &str) -> Checksum {
        load(host, &guest(transform)).unwrap()
    }

    fn order() -> PluginOrder {
        PluginOrder::new(100)
    }

    #[tokio::test]
    async fn a_body_plugin_rewrites_the_body() {
        let host = Arc::new(host());
        let processor = WasmBodyProcessor::new(Arc::clone(&host), plugged(&host, BANG), order());
        assert_eq!(
            processor.process(Bytes::from_static(b"hello")).await,
            Ok(Bytes::from_static(b"hello!"))
        );
    }

    #[tokio::test]
    async fn a_header_plugin_rewrites_the_headers() {
        let host = Arc::new(host());
        let processor =
            WasmHeaderProcessor::new(Arc::clone(&host), plugged(&host, APPEND_HEADER), order());
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        processor.process(&mut headers).await.unwrap();
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
        assert_eq!(headers.get("x-plugin").unwrap(), "seen");
        assert_eq!(headers.len(), 2);
    }

    #[tokio::test]
    async fn what_a_plugin_logs_belongs_to_the_request_that_ran_it() {
        use tracing::Instrument;

        let heard = crate::testing::heard();
        let host = Arc::new(host());
        let checksum = load(&host, &talking(1)).unwrap();
        let processor = WasmBodyProcessor::new(Arc::clone(&host), checksum, order());
        let answered = processor
            .process(Bytes::from_static(b"logged from a stage"))
            .instrument(tracing::info_span!("ingress.request"))
            .await;

        assert_eq!(answered, Ok(Bytes::from_static(b"logged from a stage")));
        // The call runs on a blocking thread, so this is the span having gone with it.
        assert_eq!(heard.saying("logged from a stage").span, "ingress.request");
    }

    #[tokio::test]
    async fn a_refusing_plugin_becomes_a_refusal() {
        let host = Arc::new(host());
        let processor = WasmBodyProcessor::new(Arc::clone(&host), plugged(&host, REFUSE), order());
        assert_eq!(
            processor.process(Bytes::from_static(b"anything")).await,
            Err(ProcessorError::refused("blocked by policy"))
        );
    }

    #[tokio::test]
    async fn a_plugin_that_is_not_loaded_fails() {
        let processor =
            WasmBodyProcessor::new(Arc::new(host()), Checksum::of(b"never loaded"), order());
        assert!(matches!(
            processor.process(Bytes::new()).await,
            Err(ProcessorError::Failed { .. })
        ));
    }

    #[tokio::test]
    async fn headers_that_do_not_parse_fail_the_request() {
        let host = Arc::new(host());
        let processor =
            WasmHeaderProcessor::new(Arc::clone(&host), plugged(&host, GARBLE), order());
        let mut headers = HeaderMap::new();
        let Err(ProcessorError::Failed { detail }) = processor.process(&mut headers).await else {
            panic!("an unparseable header block must fail the request");
        };
        assert!(detail.contains("line 1"), "{detail}");
    }

    #[tokio::test]
    async fn a_plugin_that_runs_out_of_fuel_fails_rather_than_refuses() {
        let host = Arc::new(
            PluginHost::new(PluginLimits {
                fuel: 50_000,
                deadline: Duration::from_secs(30),
                ..PluginLimits::default()
            })
            .unwrap(),
        );
        let processor = WasmBodyProcessor::new(Arc::clone(&host), plugged(&host, SPIN), order());
        assert!(matches!(
            processor.process(Bytes::from_static(b"anything")).await,
            Err(ProcessorError::Failed { .. })
        ));
    }

    #[test]
    fn the_adapters_slot_into_a_processor_chain() {
        let host = Arc::new(host());
        let checksum = Checksum::of(b"any plugin");
        let chain = ProcessorChain::new()
            .with_request_header(Arc::new(WasmHeaderProcessor::new(
                Arc::clone(&host),
                checksum,
                PluginOrder::new(120),
            )))
            .with_request_body(Arc::new(WasmBodyProcessor::new(host, checksum, order())));
        assert_eq!(chain.request_headers()[0].order(), PluginOrder::new(120));
        assert_eq!(chain.request_body()[0].order(), order());
    }
}
