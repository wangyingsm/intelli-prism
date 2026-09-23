//! Running a chain of plugins over a request or a response.

use bytes::Bytes;
use http::HeaderMap;
use http_body_util::BodyExt;
use std::sync::Arc;

use crate::body::GatewayBody;
use crate::error::{GatewayError, GatewayErrorKind};
use crate::processor::{BodyProcessor, HeaderProcessor};
use crate::stage::StageName;

/// Runs a header chain in order, reporting a refusal as the stage it happened in.
pub(super) async fn run_headers(
    stage: StageName,
    chain: &[Arc<dyn HeaderProcessor>],
    headers: &mut HeaderMap,
) -> Result<(), GatewayError> {
    for processor in chain {
        processor
            .process(headers)
            .await
            .map_err(|error| GatewayError::new(stage, error.into()))?;
    }
    Ok(())
}

/// Reads a whole body, reporting a stream that failed part way as the stage that read it.
pub(super) async fn collect(stage: StageName, body: GatewayBody) -> Result<Bytes, GatewayError> {
    Ok(body
        .collect()
        .await
        .map_err(|error| {
            GatewayError::new(
                stage,
                GatewayErrorKind::Malformed {
                    detail: error.to_string(),
                },
            )
        })?
        .to_bytes())
}

/// Runs a body chain, or hands back `None` when the body may pass through untouched.
///
/// Whether it passes is read from the chain itself, so a body can only be buffered when
/// something is actually there to rewrite it.
pub(super) async fn run_body(
    stage: StageName,
    chain: &[Arc<dyn BodyProcessor>],
    body: &mut GatewayBody,
) -> Result<Option<Bytes>, GatewayError> {
    if chain.is_empty() {
        return Ok(None);
    }
    let read_stage = match stage {
        StageName::BodyProcess => StageName::BodyRead,
        _ => StageName::ResponseBodyRead,
    };
    let taken = std::mem::replace(body, crate::body::empty());
    let mut bytes = taken
        .collect()
        .await
        .map_err(|error| {
            GatewayError::new(
                read_stage,
                GatewayErrorKind::Malformed {
                    detail: error.to_string(),
                },
            )
        })?
        .to_bytes();
    for processor in chain {
        bytes = processor
            .process(bytes)
            .await
            .map_err(|error| GatewayError::new(stage, error.into()))?;
    }
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {

    use super::super::Gateway;
    use super::super::fixtures::*;
    use crate::error::ProcessorError;
    use crate::processor::{ChainSource, HeaderProcessor, ProcessorChain};
    use crate::stage::StageName;
    use bytes::Bytes;
    use http::StatusCode;
    use http::header::CONTENT_LENGTH;
    use http::{HeaderMap, HeaderValue};
    use ip_auth::Identity;
    use ip_core::UserId;
    use ip_core::{ApiId, PluginOrder};
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn a_request_header_plugin_rewrites_what_goes_upstream() {
        let upstream = FakeUpstream::answering("pong");
        let processors =
            ProcessorChain::new().with_request_header(SetHeader::new("x-ip-trace", "yes"));
        let gateway = Gateway::new(table(), processors, upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap();
        assert_eq!(
            upstream.last().headers.get("x-ip-trace").unwrap(),
            HeaderValue::from_static("yes")
        );
    }

    /// Counts the requests that reach it, and passes each on untouched.
    #[derive(Default)]
    struct Counter(AtomicUsize);

    #[async_trait::async_trait]
    impl HeaderProcessor for Counter {
        fn order(&self) -> PluginOrder {
            PluginOrder::new(100)
        }

        async fn process(&self, _: &mut HeaderMap) -> Result<(), ProcessorError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_request_that_fails_authorization_never_reaches_a_plugin() {
        for (authority, path, status) in [
            (ungranted(), "/anthropic", StatusCode::FORBIDDEN),
            (granted(), "/elsewhere", StatusCode::NOT_FOUND),
        ] {
            let counter = Arc::new(Counter::default());
            let processors = ProcessorChain::new().with_request_header(counter.clone());
            let gateway = Gateway::new(table(), processors, FakeUpstream::answering("pong"));
            let error = gateway
                .handle(context(authority), request(path, ""))
                .await
                .unwrap_err();
            assert_eq!(error.status(), status);
            assert_eq!(
                counter.0.load(Ordering::Relaxed),
                0,
                "a plugin ran for {path}"
            );
        }
    }

    /// Marks one user's responses, and remembers every request it was asked about.
    struct PerUser {
        marked: UserId,
        asked: Mutex<Vec<(UserId, ApiId)>>,
    }

    impl PerUser {
        fn marking(user: &str) -> Arc<Self> {
            Arc::new(Self {
                marked: UserId::new(user).unwrap(),
                asked: Mutex::new(Vec::new()),
            })
        }
    }

    impl ChainSource for PerUser {
        fn chains_for(&self, identity: &Identity, api: &ApiId) -> Arc<ProcessorChain> {
            self.asked
                .lock()
                .unwrap()
                .push((identity.user.clone(), api.clone()));
            let chain = if identity.user == self.marked {
                ProcessorChain::new().with_response_body(Marker::at(10, "-marked"))
            } else {
                ProcessorChain::new()
            };
            Arc::new(chain)
        }
    }

    #[tokio::test]
    async fn each_request_runs_the_chains_its_source_picks() {
        for (marked, expected) in [("alice", "pong-marked"), ("bob", "pong")] {
            let gateway = Gateway::with_chains(
                table(),
                PerUser::marking(marked),
                FakeUpstream::answering("pong"),
            );
            let response = gateway
                .handle(context(granted()), request("/anthropic", ""))
                .await
                .unwrap();
            assert_eq!(
                body_of(response).await,
                expected,
                "chains marked for {marked}"
            );
        }
    }

    #[tokio::test]
    async fn the_source_is_asked_about_the_caller_and_the_routed_api() {
        let source = PerUser::marking("alice");
        let gateway =
            Gateway::with_chains(table(), source.clone(), FakeUpstream::answering("pong"));
        gateway
            .handle(context(granted()), request("/anthropic/messages", ""))
            .await
            .unwrap();
        assert_eq!(source.asked.lock().unwrap().as_slice(), [(user(), api())]);
    }

    #[tokio::test]
    async fn a_forbidden_request_never_asks_for_chains() {
        let source = PerUser::marking("alice");
        let gateway =
            Gateway::with_chains(table(), source.clone(), FakeUpstream::answering("pong"));
        let error = gateway
            .handle(context(ungranted()), request("/anthropic", ""))
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::FORBIDDEN);
        assert!(source.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failing_plugin_stops_the_flow_at_its_stage() {
        let processors = ProcessorChain::new()
            .with_request_header(Arc::new(Stop(ProcessorError::failed("plugin crashed"))));
        let gateway = Gateway::new(table(), processors, FakeUpstream::answering("pong"));
        let error = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap_err();
        assert_eq!(error.stage(), StageName::HeaderProcess);
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error.public_reason(), None);
    }

    #[tokio::test]
    async fn a_refusing_plugin_is_forbidden_with_its_reason() {
        let upstream = FakeUpstream::answering("pong");
        let processors = ProcessorChain::new()
            .with_request_header(Arc::new(Stop(ProcessorError::refused("blocked by policy"))));
        let gateway = Gateway::new(table(), processors, upstream.clone());
        let error = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::FORBIDDEN);
        assert_eq!(error.public_reason(), Some("blocked by policy"));
        assert!(upstream.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_request_body_plugin_transforms_what_goes_upstream() {
        let upstream = FakeUpstream::answering("pong");
        let processors = ProcessorChain::new().with_request_body(Marker::at(10, "-marked"));
        let gateway = Gateway::new(table(), processors, upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", "payload"))
            .await
            .unwrap();
        assert_eq!(upstream.last().body, Bytes::from("payload-marked"));
    }

    #[tokio::test]
    async fn a_chunk_plugin_does_not_force_the_body_to_be_buffered() {
        let processors = ProcessorChain::new().with_response_chunk(Marker::at(10, "-chunk"));
        assert!(processors.passes_response_body());
        let gateway = Gateway::new(table(), processors, FakeUpstream::answering("pong"));
        let response = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap();
        assert_eq!(body_of(response).await, "pong");
    }

    #[tokio::test]
    async fn a_rewritten_request_body_carries_its_new_length() {
        let upstream = FakeUpstream::answering("pong");
        let processors = ProcessorChain::new().with_request_body(Marker::at(10, "-marked"));
        let gateway = Gateway::new(table(), processors, upstream.clone());
        let mut request = request("/anthropic", "payload");
        request
            .headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from_static("7"));
        gateway.handle(context(granted()), request).await.unwrap();
        let sent = upstream.last();
        assert_eq!(sent.body, Bytes::from("payload-marked"));
        assert_eq!(sent.headers.get(CONTENT_LENGTH).unwrap(), "14");
    }

    #[tokio::test]
    async fn a_rewritten_response_body_carries_its_new_length() {
        let upstream = FakeUpstream::answering_with_headers("pong", vec![("content-length", "4")]);
        let processors = ProcessorChain::new().with_response_body(Marker::at(10, "-seen"));
        let gateway = Gateway::new(table(), processors, upstream);
        let response = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap();
        assert_eq!(response.headers().get(CONTENT_LENGTH).unwrap(), "9");
        assert_eq!(body_of(response).await, "pong-seen");
    }

    #[tokio::test]
    async fn an_untouched_body_keeps_its_length() {
        let upstream = FakeUpstream::answering_with_headers("pong", vec![("content-length", "4")]);
        let gateway = Gateway::new(table(), ProcessorChain::new(), upstream);
        let response = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap();
        assert_eq!(response.headers().get(CONTENT_LENGTH).unwrap(), "4");
    }

    #[tokio::test]
    async fn an_event_stream_runs_each_event_through_the_chunk_chain() {
        let upstream = FakeUpstream::answering_with_headers(
            "data: a\n\ndata: b\n\n",
            vec![
                ("content-type", "text/event-stream; charset=utf-8"),
                ("content-length", "18"),
            ],
        );
        let processors = ProcessorChain::new().with_response_chunk(Marker::at(10, "-seen"));
        let gateway = Gateway::new(table(), processors, upstream);
        let response = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap();
        assert!(response.headers().get(CONTENT_LENGTH).is_none());
        assert_eq!(body_of(response).await, "data: a\n\n-seendata: b\n\n-seen");
    }

    #[tokio::test]
    async fn a_response_that_is_not_an_event_stream_skips_the_chunk_chain() {
        let upstream = FakeUpstream::answering_with_headers(
            "pong",
            vec![("content-type", "application/json")],
        );
        let processors = ProcessorChain::new().with_response_chunk(Marker::at(10, "-seen"));
        let gateway = Gateway::new(table(), processors, upstream);
        let response = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap();
        assert_eq!(body_of(response).await, "pong");
    }

    #[tokio::test]
    async fn an_event_stream_with_no_chunk_plugins_passes_untouched() {
        let upstream = FakeUpstream::answering_with_headers(
            "data: a\n\n",
            vec![
                ("content-type", "text/event-stream"),
                ("content-length", "9"),
            ],
        );
        let gateway = Gateway::new(table(), ProcessorChain::new(), upstream);
        let response = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap();
        assert_eq!(response.headers().get(CONTENT_LENGTH).unwrap(), "9");
        assert_eq!(body_of(response).await, "data: a\n\n");
    }

    #[tokio::test]
    async fn with_no_body_plugin_the_body_is_never_read() {
        let upstream = FakeUpstream::answering("pong");
        let processors = ProcessorChain::new();
        assert!(processors.passes_request_body());
        assert!(processors.passes_response_body());
        let gateway = Gateway::new(table(), processors, upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", "payload"))
            .await
            .unwrap();
        assert_eq!(upstream.last().body, Bytes::from("payload"));
    }

    #[tokio::test]
    async fn a_response_body_plugin_transforms_what_goes_back() {
        let processors = ProcessorChain::new().with_response_body(Marker::at(10, "-seen"));
        let gateway = Gateway::new(table(), processors, FakeUpstream::answering("pong"));
        let response = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap();
        assert_eq!(body_of(response).await, "pong-seen");
    }

    #[tokio::test]
    async fn plugins_run_highest_order_first() {
        let upstream = FakeUpstream::answering("pong");
        let processors = ProcessorChain::new()
            .with_request_body(Marker::at(10, "-low"))
            .with_request_body(Marker::at(200, "-high"));
        let gateway = Gateway::new(table(), processors, upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", "body"))
            .await
            .unwrap();
        assert_eq!(upstream.last().body, Bytes::from("body-high-low"));
    }
}
