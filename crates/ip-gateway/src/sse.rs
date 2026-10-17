use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use http_body::Frame;
use http_body_util::BodyExt;
use tokio::sync::mpsc;

use crate::body::{BoxError, ChannelBody, GatewayBody};
use crate::error::ProcessorError;
use crate::filter::Filtering;
use crate::processor::BodyProcessor;
use ip_core::{Dialect, TenantId};
use ip_filter::Trickle;
use ip_protocol::{Event, Reading, Writing};

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

/// Carries a streamed answer across two families, event by event as they arrive.
///
/// Nothing is held back: each of the upstream's events becomes canonical events and
/// those become the caller's own, which go out at once. A crossing therefore costs a
/// client no time to first token, which is the whole reason this exists rather than
/// buffering the answer and translating it whole.
pub(crate) fn crossed_events(body: GatewayBody, caller: Dialect, upstream: Dialect) -> GatewayBody {
    let (sender, receiver) = mpsc::channel(BUFFERED_EVENTS);
    tokio::spawn(cross(body, caller, upstream, sender));
    ChannelBody::new(receiver).boxed_unsync()
}

async fn cross(mut body: GatewayBody, caller: Dialect, upstream: Dialect, sender: Sender) {
    let mut reading = Reading::new(upstream);
    let mut writing = Writing::new(caller);
    let mut pending = BytesMut::new();
    while let Some(frame) = body.frame().await {
        let frame = match frame {
            Ok(frame) => frame,
            Err(error) => {
                let _ = sender.send(Err(error)).await;
                return;
            }
        };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        pending.extend_from_slice(&data);
        if !cross_events(&mut pending, false, &mut reading, &mut writing, &sender).await {
            return;
        }
    }
    if !cross_events(&mut pending, true, &mut reading, &mut writing, &sender).await {
        return;
    }
    // A family that never said it had finished a tool call still asked for one.
    for event in reading.end() {
        if !send_event(&writing.event(&event), &sender).await {
            return;
        }
    }
    send_event(&writing.end(), &sender).await;
}

/// Crosses every complete event waiting in the buffer. False once the stream has to stop.
async fn cross_events(
    pending: &mut BytesMut,
    at_end: bool,
    reading: &mut Reading,
    writing: &mut Writing,
    sender: &Sender,
) -> bool {
    while let Some(end) = event_end(pending, at_end) {
        let block = pending.split_to(end);
        // Only a whole event is converted, so no split can land inside a character.
        let Ok(text) = std::str::from_utf8(&block) else {
            continue;
        };
        for event in reading.event(text) {
            if !send_event(&writing.event(&event), sender).await {
                return false;
            }
        }
    }
    true
}

/// Takes out of a streamed answer what must not leave, as it streams.
///
/// A model writes a secret token by token, so the text is held back a window at a time
/// and let out behind the stream. Everything that is not text — a tool call, a stop, a
/// cost — flushes whatever is held first, so nothing a caller reads arrives out of the
/// order the model said it in.
///
/// Events go through the canonical form to be read and written again, so a family's own
/// fields that the canonical form does not carry are not carried here either. That is
/// the price of filtering a stream, it applies only when filtering is switched on, and
/// an event the reader makes nothing of is forwarded exactly as it arrived rather than
/// dropped.
pub(crate) fn filtered_events(
    body: GatewayBody,
    speaks: Dialect,
    filtering: Arc<Filtering>,
    tenant: TenantId,
) -> GatewayBody {
    let (sender, receiver) = mpsc::channel(BUFFERED_EVENTS);
    tokio::spawn(sift(body, speaks, filtering, tenant, sender));
    ChannelBody::new(receiver).boxed_unsync()
}

async fn sift(
    mut body: GatewayBody,
    speaks: Dialect,
    filtering: Arc<Filtering>,
    tenant: TenantId,
    sender: Sender,
) {
    let mut reading = Reading::new(speaks);
    let mut writing = Writing::new(speaks);
    let mut trickle = Trickle::new(filtering.detectors_for(&tenant), filtering.window());
    let mut pending = BytesMut::new();
    let mut going = true;
    while going {
        let Some(frame) = body.frame().await else {
            break;
        };
        let frame = match frame {
            Ok(frame) => frame,
            Err(error) => {
                let _ = sender.send(Err(error)).await;
                return;
            }
        };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        pending.extend_from_slice(&data);
        going = sift_events(
            &mut pending,
            false,
            &mut reading,
            &mut writing,
            &mut trickle,
            &sender,
        )
        .await;
    }
    if going {
        going = sift_events(
            &mut pending,
            true,
            &mut reading,
            &mut writing,
            &mut trickle,
            &sender,
        )
        .await;
    }
    // Held text goes before anything that ends the answer, and before the reader's own
    // last word: text a caller never receives is a truncated answer, which is a worse
    // failure than one that was not filtered.
    if going {
        going = send_text(trickle.flush(), &mut writing, &sender).await;
    }
    if going {
        for event in reading.end() {
            if !send_event(&writing.event(&event), &sender).await {
                break;
            }
        }
        send_event(&writing.end(), &sender).await;
    }
    let found = trickle.found();
    if !found.is_empty() || trickle.overran() {
        // The count cannot ride the answer's headers, which went out before its first
        // event, so this line is where an operator reads what a stream gave up.
        tracing::info!(
            redacted = %found.said(),
            overran = trickle.overran(),
            "a streamed answer was filtered",
        );
    }
}

/// Filters every complete event waiting in the buffer. False once the stream has to stop.
async fn sift_events(
    pending: &mut BytesMut,
    at_end: bool,
    reading: &mut Reading,
    writing: &mut Writing,
    trickle: &mut Trickle<'_>,
    sender: &Sender,
) -> bool {
    while let Some(end) = event_end(pending, at_end) {
        let block = pending.split_to(end);
        // Only a whole event is read, so no split can land inside a character.
        let Ok(text) = std::str::from_utf8(&block) else {
            continue;
        };
        let read = reading.event(text);
        if read.is_empty() {
            // A keep-alive, or something this reader makes nothing of. Either way it is
            // the upstream's own bytes and they go on as they came.
            if sender.send(Ok(Frame::data(block.freeze()))).await.is_err() {
                return false;
            }
            continue;
        }
        for event in read {
            if !sift_event(event, writing, trickle, sender).await {
                return false;
            }
        }
    }
    true
}

/// One canonical event, with its text held back and everything else flushing it first.
async fn sift_event(
    event: Event,
    writing: &mut Writing,
    trickle: &mut Trickle<'_>,
    sender: &Sender,
) -> bool {
    match event {
        Event::Text(text) => send_text(trickle.push(&text), writing, sender).await,
        Event::ToolCall {
            id,
            name,
            arguments,
        } => {
            if !send_text(trickle.flush(), writing, sender).await {
                return false;
            }
            let arguments = trickle.value(&arguments).unwrap_or(arguments);
            let call = Event::ToolCall {
                id,
                name,
                arguments,
            };
            send_event(&writing.event(&call), sender).await
        }
        other => {
            if !send_text(trickle.flush(), writing, sender).await {
                return false;
            }
            send_event(&writing.event(&other), sender).await
        }
    }
}

/// Sends text on as an event of its own, when there is any to send.
async fn send_text(text: String, writing: &mut Writing, sender: &Sender) -> bool {
    if text.is_empty() {
        return true;
    }
    send_event(&writing.event(&Event::Text(text)), sender).await
}

/// Sends one family's event text on, when there is any. False once the caller is gone.
async fn send_event(text: &str, sender: &Sender) -> bool {
    if text.is_empty() {
        return true;
    }
    sender
        .send(Ok(Frame::data(Bytes::from(text.to_owned()))))
        .await
        .is_ok()
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
