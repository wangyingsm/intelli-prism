use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use http_body::Frame;
use http_body_util::BodyExt;
use tokio::sync::mpsc;

use crate::body::{BoxError, ChannelBody, GatewayBody};
use crate::error::ProcessorError;
use crate::processor::BodyProcessor;

/// Events a slow caller may leave waiting before the upstream read is held back.
const BUFFERED_EVENTS: usize = 16;

type Sender = mpsc::Sender<Result<Frame<Bytes>, BoxError>>;

/// Runs every server sent event of a streamed body through a chunk chain, one event at a time.
pub(crate) fn stream_chunks(body: GatewayBody, chain: Vec<Arc<dyn BodyProcessor>>) -> GatewayBody {
    let (sender, receiver) = mpsc::channel(BUFFERED_EVENTS);
    tokio::spawn(pump(body, chain, sender));
    ChannelBody::new(receiver).boxed_unsync()
}

async fn pump(mut body: GatewayBody, chain: Vec<Arc<dyn BodyProcessor>>, sender: Sender) {
    let mut pending = BytesMut::new();
    while let Some(frame) = body.frame().await {
        let frame = match frame {
            Ok(frame) => frame,
            Err(error) => {
                let _ = sender.send(Err(error)).await;
                return;
            }
        };
        match frame.into_data() {
            Ok(data) => {
                pending.extend_from_slice(&data);
                if !forward_events(&mut pending, false, &chain, &sender).await {
                    return;
                }
            }
            Err(trailers) => {
                if !forward_events(&mut pending, true, &chain, &sender).await
                    || !flush(&mut pending, &sender).await
                    || sender.send(Ok(trailers)).await.is_err()
                {
                    return;
                }
            }
        }
    }
    if forward_events(&mut pending, true, &chain, &sender).await {
        flush(&mut pending, &sender).await;
    }
}

/// Sends every complete event through the chain and on. False once the stream has to stop.
async fn forward_events(
    pending: &mut BytesMut,
    at_end: bool,
    chain: &[Arc<dyn BodyProcessor>],
    sender: &Sender,
) -> bool {
    while let Some(end) = event_end(pending, at_end) {
        let event = pending.split_to(end).freeze();
        let frame = match run_chain(chain, event).await {
            Ok(event) => Ok(Frame::data(event)),
            Err(ProcessorError::Refused { reason }) => {
                let _ = sender.send(Ok(Frame::data(error_event(&reason)))).await;
                return false;
            }
            Err(ProcessorError::Failed { detail }) => {
                tracing::error!(%detail, "a chunk plugin failed mid-stream");
                let failed: BoxError = Box::new(ProcessorError::Failed { detail });
                let _ = sender.send(Err(failed)).await;
                return false;
            }
        };
        if sender.send(frame).await.is_err() {
            return false;
        }
    }
    true
}

/// Sends what is left of an unfinished event on untouched, since a client discards it anyway.
async fn flush(pending: &mut BytesMut, sender: &Sender) -> bool {
    if pending.is_empty() {
        return true;
    }
    sender
        .send(Ok(Frame::data(pending.split().freeze())))
        .await
        .is_ok()
}

async fn run_chain(
    chain: &[Arc<dyn BodyProcessor>],
    mut event: Bytes,
) -> Result<Bytes, ProcessorError> {
    for processor in chain {
        event = processor.process(event).await?;
    }
    Ok(event)
}

/// The event that tells a streaming caller a plugin refused. Each line of the reason gets
/// its own data line, so no reason can break the framing and add events of its own.
pub(crate) fn error_event(reason: &str) -> Bytes {
    let mut event = String::from("event: error\n");
    for line in reason.replace("\r\n", "\n").split(['\n', '\r']) {
        event.push_str("data: ");
        event.push_str(line);
        event.push('\n');
    }
    event.push('\n');
    Bytes::from(event)
}

/// What sits at one position of a buffer, as far as line endings go.
enum Ending {
    /// A line ending of this many bytes.
    Line(usize),
    /// Anything that ends no line.
    Not,
    /// A carriage return at the edge of what has arrived, which may yet be CRLF.
    Unknown,
}

fn ending_at(buffer: &[u8], at: usize, at_end: bool) -> Ending {
    match buffer.get(at) {
        Some(b'\n') => Ending::Line(1),
        Some(b'\r') => match buffer.get(at + 1) {
            Some(b'\n') => Ending::Line(2),
            Some(_) => Ending::Line(1),
            None if at_end => Ending::Line(1),
            None => Ending::Unknown,
        },
        Some(_) => Ending::Not,
        None if at_end => Ending::Not,
        None => Ending::Unknown,
    }
}

/// Where the first complete event in `buffer` ends, just past the blank line that closes it.
/// An event ends at two line endings in a row, each of which may be LF, CRLF or CR.
fn event_end(buffer: &[u8], at_end: bool) -> Option<usize> {
    let mut at = 0;
    while at < buffer.len() {
        match ending_at(buffer, at, at_end) {
            Ending::Line(first) => match ending_at(buffer, at + first, at_end) {
                Ending::Line(second) => return Some(at + first + second),
                Ending::Unknown => return None,
                Ending::Not => at += first,
            },
            Ending::Unknown => return None,
            Ending::Not => at += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use ip_core::PluginOrder;

    use super::*;

    /// Passes every event on untouched, remembering each one it saw.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<Bytes>>);

    #[async_trait]
    impl BodyProcessor for Recorder {
        fn order(&self) -> PluginOrder {
            PluginOrder::new(100)
        }

        async fn process(&self, event: Bytes) -> Result<Bytes, ProcessorError> {
            self.0.lock().unwrap().push(event.clone());
            Ok(event)
        }
    }

    /// Passes events on until the `at`th, which it stops with `error`.
    struct StopAt {
        at: usize,
        seen: AtomicUsize,
        error: ProcessorError,
    }

    #[async_trait]
    impl BodyProcessor for StopAt {
        fn order(&self) -> PluginOrder {
            PluginOrder::new(100)
        }

        async fn process(&self, event: Bytes) -> Result<Bytes, ProcessorError> {
            if self.seen.fetch_add(1, Ordering::Relaxed) + 1 == self.at {
                return Err(self.error.clone());
            }
            Ok(event)
        }
    }

    fn framed(frames: &[&'static [u8]]) -> GatewayBody {
        let (sender, receiver) = mpsc::channel(frames.len().max(1));
        for frame in frames {
            sender
                .try_send(Ok(Frame::data(Bytes::from_static(frame))))
                .unwrap();
        }
        ChannelBody::new(receiver).boxed_unsync()
    }

    fn seen(recorder: &Recorder) -> Vec<Bytes> {
        recorder.0.lock().unwrap().clone()
    }

    async fn streamed(
        body: GatewayBody,
        chain: Vec<Arc<dyn BodyProcessor>>,
    ) -> Result<Bytes, BoxError> {
        Ok(stream_chunks(body, chain).collect().await?.to_bytes())
    }

    #[tokio::test]
    async fn an_event_split_across_frames_reaches_the_chain_whole() {
        let recorder = Arc::new(Recorder::default());
        let output = streamed(
            framed(&[b"data: {\"de", b"lta\":1}\n\ndata: ", b"2\n\n"]),
            vec![recorder.clone()],
        )
        .await
        .unwrap();
        assert_eq!(
            seen(&recorder),
            vec![
                Bytes::from_static(b"data: {\"delta\":1}\n\n"),
                Bytes::from_static(b"data: 2\n\n"),
            ]
        );
        assert_eq!(
            output,
            Bytes::from_static(b"data: {\"delta\":1}\n\ndata: 2\n\n")
        );
    }

    #[tokio::test]
    async fn every_kind_of_line_ending_closes_an_event() {
        let recorder = Arc::new(Recorder::default());
        streamed(
            framed(&[b"a\r\n\r\nb\r\rc\n\nd\r\n\n"]),
            vec![recorder.clone()],
        )
        .await
        .unwrap();
        assert_eq!(
            seen(&recorder),
            vec![
                Bytes::from_static(b"a\r\n\r\n"),
                Bytes::from_static(b"b\r\r"),
                Bytes::from_static(b"c\n\n"),
                Bytes::from_static(b"d\r\n\n"),
            ]
        );
    }

    #[tokio::test]
    async fn a_crlf_split_across_frames_is_one_line_ending() {
        let recorder = Arc::new(Recorder::default());
        streamed(framed(&[b"data: x\r", b"\n\r\n"]), vec![recorder.clone()])
            .await
            .unwrap();
        assert_eq!(
            seen(&recorder),
            vec![Bytes::from_static(b"data: x\r\n\r\n")]
        );
    }

    #[tokio::test]
    async fn a_carriage_return_left_at_the_end_still_closes_an_event() {
        let recorder = Arc::new(Recorder::default());
        streamed(framed(&[b"data: 1\r\r"]), vec![recorder.clone()])
            .await
            .unwrap();
        assert_eq!(seen(&recorder), vec![Bytes::from_static(b"data: 1\r\r")]);
    }

    #[tokio::test]
    async fn an_unfinished_event_at_the_end_passes_untouched() {
        let recorder = Arc::new(Recorder::default());
        let output = streamed(framed(&[b"data: 1\n\ndata: half"]), vec![recorder.clone()])
            .await
            .unwrap();
        assert_eq!(seen(&recorder), vec![Bytes::from_static(b"data: 1\n\n")]);
        assert_eq!(output, Bytes::from_static(b"data: 1\n\ndata: half"));
    }

    #[tokio::test]
    async fn a_refusal_mid_stream_ends_with_an_error_event() {
        let refusing = Arc::new(StopAt {
            at: 2,
            seen: AtomicUsize::new(0),
            error: ProcessorError::refused("blocked by policy"),
        });
        let output = streamed(
            framed(&[b"data: 1\n\ndata: 2\n\ndata: 3\n\n"]),
            vec![refusing],
        )
        .await
        .unwrap();
        assert_eq!(
            output,
            Bytes::from_static(b"data: 1\n\nevent: error\ndata: blocked by policy\n\n")
        );
    }

    #[tokio::test]
    async fn a_failure_mid_stream_aborts_the_body() {
        let failing = Arc::new(StopAt {
            at: 2,
            seen: AtomicUsize::new(0),
            error: ProcessorError::failed("wasm trapped"),
        });
        assert!(
            streamed(framed(&[b"data: 1\n\ndata: 2\n\n"]), vec![failing])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn trailers_follow_the_last_event() {
        let (sender, receiver) = mpsc::channel(2);
        sender
            .try_send(Ok(Frame::data(Bytes::from_static(b"data: 1\n\n"))))
            .unwrap();
        let mut trailers = http::HeaderMap::new();
        trailers.insert("grpc-status", http::HeaderValue::from_static("0"));
        sender.try_send(Ok(Frame::trailers(trailers))).unwrap();
        drop(sender);
        let body = ChannelBody::new(receiver).boxed_unsync();
        let collected = stream_chunks(body, vec![Arc::new(Recorder::default())])
            .collect()
            .await
            .unwrap();
        assert_eq!(
            collected.trailers().unwrap().get("grpc-status").unwrap(),
            "0"
        );
        assert_eq!(collected.to_bytes(), Bytes::from_static(b"data: 1\n\n"));
    }

    #[test]
    fn half_an_event_has_no_end_yet() {
        assert_eq!(event_end(b"data: 1\n", false), None);
        assert_eq!(event_end(b"data: 1\r", false), None);
        assert_eq!(event_end(b"data: 1\n\ndata: 2", false), Some(9));
    }

    #[test]
    fn a_multi_line_reason_becomes_one_data_line_per_line() {
        assert_eq!(
            error_event("first line\nsecond line"),
            Bytes::from_static(b"event: error\ndata: first line\ndata: second line\n\n")
        );
    }

    #[test]
    fn a_reason_cannot_smuggle_in_an_event_of_its_own() {
        for reason in [
            "x\n\nevent: admin\ndata: y",
            "x\r\revent: admin",
            "x\r\n\r\nevent: admin",
        ] {
            let event = String::from_utf8(error_event(reason).to_vec()).unwrap();
            assert!(event.starts_with("event: error\n"), "{event:?}");
            assert_eq!(event.matches("\n\n").count(), 1, "{event:?}");
            assert!(event.ends_with("\n\n"), "{event:?}");
            assert!(!event.contains('\r'), "{event:?}");
            assert!(!event.contains("\nevent: admin"), "{event:?}");
        }
    }
}
