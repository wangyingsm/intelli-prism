//! Sending one node's work to another member, and reading back what it says.

use std::time::Duration;

use bytes::Bytes;
use http::{HeaderValue, Request, StatusCode, header};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use ip_config::Secret;
use ip_core::route::ClusterAddress;

use crate::dispatch::Dispatch;
use crate::error::ClusterError;
use crate::event::NodeEvent;

/// The path a member answers a dispatch on.
pub const DISPATCH_PATH: &str = "/_ip/cluster/node";

/// How long one *unfinished* event may be before this client refuses the rest of the stream.
///
/// A ceiling rather than no ceiling: an event arrives a chunk at a time and is held until it is
/// whole, so a member that opens an event and never closes it would otherwise be a member that
/// fills this machine's memory. It bounds only what is unfinished — every complete event is
/// handed back before this is looked at — so a fast member and a slow reader are not an overrun.
pub(crate) const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// How long a connection may take to establish.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The pooling http client this crate talks over.
type Pooled = Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>;

/// How a client reaches the other members of its own cluster, and how patient it is.
///
/// A composite rather than two parameters on every dispatch, and the one place the credential
/// lives. The member's address is not here: one client dispatches to every member, and which
/// member is the coordinator's to pick per node.
#[derive(Debug, Clone)]
pub struct ClusterSettings {
    /// The bearer every dispatch presents, which every member of one cluster shares.
    pub secret: Secret,
    /// How long a dispatched node may say nothing before its member is given up on.
    ///
    /// Between events rather than over the whole run: a node takes as many turns as its template
    /// allows and a model answers when it answers, so a ceiling on the whole of it would cut off
    /// the slow runs a cluster exists to spread. What is not allowed is silence.
    pub idle: Duration,
}

/// A client that dispatches nodes to the other members of this cluster.
///
/// One per deployment, holding a connection pool: a graph of thirty nodes dispatched one at a
/// time reuses one connection per member rather than opening thirty.
#[derive(Debug)]
pub struct ClusterClient {
    /// What it presents, and how long it waits.
    settings: ClusterSettings,
    /// The pool every dispatch goes over.
    http: Pooled,
}

impl ClusterClient {
    /// A client over the configured credential and patience.
    #[must_use]
    pub fn new(settings: ClusterSettings) -> Self {
        let http = pooled(settings.idle);
        Self { settings, http }
    }

    /// Sends one node's work to a member and answers the stream the member replies with.
    ///
    /// It returns as soon as the member has accepted the work, before the node has run, so that a
    /// failure *here* is a failure of a node that never started. That distinction is the whole of
    /// it: a coordinator may run such a node at home, because nothing has been spent, while a
    /// stream that breaks later may have paid for turns already taken.
    pub async fn dispatch(
        &self,
        member: &ClusterAddress,
        work: &Dispatch,
    ) -> Result<Dispatched, ClusterError> {
        let payload = serde_json::to_vec(work).map_err(|error| ClusterError::Malformed {
            detail: format!("the dispatch will not serialize: {error}"),
        })?;
        let presented = HeaderValue::from_str(&format!("Bearer {}", self.settings.secret.expose()))
            .map_err(|_| ClusterError::Unauthorized)?;
        let request = Request::builder()
            .method(http::Method::POST)
            .uri(format!("http://{member}{DISPATCH_PATH}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "text/event-stream")
            .header(header::AUTHORIZATION, presented)
            .body(Full::new(Bytes::from(payload)))
            .map_err(|error| ClusterError::Unreachable {
                detail: error.to_string(),
            })?;

        let answered = tokio::time::timeout(self.settings.idle, self.http.request(request))
            .await
            .map_err(|_| ClusterError::Unreachable {
                detail: format!("no answer within {:?}", self.settings.idle),
            })?
            .map_err(|error| ClusterError::Unreachable {
                detail: error.to_string(),
            })?;

        let status = answered.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(ClusterError::Unauthorized);
        }
        if !status.is_success() {
            return Err(ClusterError::Unreachable {
                detail: format!("the member answered {}", status.as_u16()),
            });
        }

        Ok(Dispatched {
            body: answered.into_body(),
            buffer: Vec::new(),
            idle: self.settings.idle,
            said: 0,
            done: false,
        })
    }
}

/// One node running on another member, as this machine hears it.
#[derive(Debug)]
pub struct Dispatched {
    /// The stream the member is answering on.
    body: Incoming,
    /// What has been read and not yet framed into an event.
    buffer: Vec<u8>,
    /// How long the member may say nothing before it is given up on.
    idle: Duration,
    /// How many events it has handed back.
    said: usize,
    /// Whether the member has said its last.
    done: bool,
}

impl Dispatched {
    /// The next thing the member said, or nothing once it has said its last.
    ///
    /// A stream that ends without an answer or a refusal is [`ClusterError::CutOff`] rather than
    /// a quiet end, because a node whose answer never arrived is not a node that answered
    /// nothing.
    pub async fn next(&mut self) -> Result<Option<NodeEvent>, ClusterError> {
        if self.done {
            return Ok(None);
        }
        loop {
            while let Some((carried, used)) = framed(&self.buffer) {
                self.buffer.drain(..used);
                if let Some(data) = carried {
                    let read: NodeEvent =
                        serde_json::from_str(&data).map_err(|error| ClusterError::Malformed {
                            detail: error.to_string(),
                        })?;
                    self.said += 1;
                    self.done = read.terminal();
                    return Ok(Some(read));
                }
            }
            if self.buffer.len() > MAX_EVENT_BYTES {
                return Err(ClusterError::Overlong {
                    max: MAX_EVENT_BYTES,
                });
            }
            match self.read().await? {
                Some(chunk) => self.buffer.extend_from_slice(&chunk),
                None => return Err(ClusterError::CutOff { after: self.said }),
            }
        }
    }

    /// How many events it has handed back, which is how a coordinator knows whether a node that
    /// broke had started taking turns.
    #[must_use]
    pub const fn said(&self) -> usize {
        self.said
    }

    /// The next chunk the member sent, or nothing once the stream is over.
    async fn read(&mut self) -> Result<Option<Bytes>, ClusterError> {
        let idle = self.idle;
        loop {
            let framed = tokio::time::timeout(idle, self.body.frame())
                .await
                .map_err(|_| ClusterError::Unreachable {
                    detail: format!("nothing said for {idle:?}"),
                })?;
            match framed {
                None => return Ok(None),
                Some(Err(error)) => {
                    return Err(ClusterError::Unreachable {
                        detail: error.to_string(),
                    });
                }
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        return Ok(Some(data));
                    }
                }
            }
        }
    }
}

/// The first whole event in what has been read: what it carries, and how much it used up.
///
/// Nothing until the blank line that ends the event has arrived whole, which is what keeps an
/// event whole across the chunks it was sent in. The lines are read on the way past rather than
/// sliced out afterwards, because where an event ends and where its last line ends are not the
/// same place: a stream written with bare carriage returns would otherwise carry one into the
/// data.
///
/// A frame of no data lines at all is a keep-alive and carries nothing, which is not the same as
/// one whose data will not read back.
fn framed(buffer: &[u8]) -> Option<(Option<String>, usize)> {
    let mut lines: Vec<&str> = Vec::new();
    let mut at = 0;
    while at < buffer.len() {
        let (ends, next) = line_end(buffer, at)?;
        if ends == at {
            let carried = (!lines.is_empty()).then(|| lines.join("\n"));
            return Some((carried, next));
        }
        if let Some(value) = data_line(&buffer[at..ends]) {
            lines.push(value);
        }
        at = next;
    }
    None
}

/// Where the line starting at `at` ends, and where the next one begins.
fn line_end(buffer: &[u8], at: usize) -> Option<(usize, usize)> {
    let mut over = at;
    while over < buffer.len() {
        match buffer[over] {
            b'\n' => return Some((over, over + 1)),
            // A carriage return at the very edge of a read may yet turn out to be a CRLF.
            b'\r' if over + 1 == buffer.len() => return None,
            b'\r' if buffer[over + 1] == b'\n' => return Some((over, over + 2)),
            b'\r' => return Some((over, over + 1)),
            _ => over += 1,
        }
    }
    None
}

/// What one line of an event carries, where it is a data line at all.
fn data_line(line: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(line).ok()?;
    let value = text.strip_prefix("data:")?;
    Some(value.strip_prefix(' ').unwrap_or(value))
}

/// The connection pool every dispatch to every member goes over.
fn pooled(idle: Duration) -> Pooled {
    install_crypto_provider();
    let mut http = HttpConnector::new();
    http.set_connect_timeout(Some(CONNECT_TIMEOUT.min(idle)));
    http.enforce_http(false);
    let connector = HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .wrap_connector(http);
    Client::builder(TokioExecutor::new()).build(connector)
}

/// Rustls needs one process wide provider; a second install is another caller winning the race.
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    use crate::testing::{
        Answers, STUB_REFUSAL, STUB_SECRET, SilentMember, StubWorker, stub_answer, stub_dispatch,
        stub_settings, stub_stream, vacant_address,
    };

    use super::*;

    /// Everything a member said, until it answered or something went wrong.
    async fn heard(mut dispatched: Dispatched) -> Result<Vec<NodeEvent>, ClusterError> {
        let mut heard = Vec::new();
        while let Some(event) = dispatched.next().await? {
            heard.push(event);
        }
        Ok(heard)
    }

    #[tokio::test]
    async fn a_dispatched_node_says_what_it_did_and_then_what_it_came_to() {
        let worker = StubWorker::start(Answers::Ran).await;

        let dispatched = worker
            .client()
            .dispatch(&worker.address(), &stub_dispatch())
            .await
            .expect("a member that is listening takes the work");

        assert_eq!(
            heard(dispatched).await.expect("a member that answers"),
            stub_stream()
        );
    }

    /// The work itself crossed the wire, and so did the secret.
    #[tokio::test]
    async fn a_member_is_handed_the_work_and_the_clusters_own_secret() {
        let worker = StubWorker::start(Answers::Ran).await;
        let work = stub_dispatch();

        let dispatched = worker
            .client()
            .dispatch(&worker.address(), &work)
            .await
            .expect("a member that is listening takes the work");
        drop(heard(dispatched).await);

        let seen = worker.only();
        assert_eq!(
            seen.authorization.as_deref(),
            Some(format!("Bearer {STUB_SECRET}").as_str())
        );
        assert_eq!(seen.path, DISPATCH_PATH);
        assert_eq!(
            serde_json::from_value::<Dispatch>(seen.work).expect("the work it was handed"),
            work
        );
    }

    /// One deployment's members share one secret, so this is misconfiguration and not a retry.
    #[tokio::test]
    async fn a_member_holding_another_secret_refuses_the_dispatch() {
        let worker = StubWorker::start(Answers::Unauthorized).await;

        assert_eq!(
            worker
                .client()
                .dispatch(&worker.address(), &stub_dispatch())
                .await
                .err(),
            Some(ClusterError::Unauthorized)
        );
    }

    /// Nothing was spent, so this is the failure a coordinator may run the node at home after.
    #[tokio::test]
    async fn a_member_that_will_not_run_the_node_says_so_as_its_only_event() {
        let worker = StubWorker::start(Answers::Refused).await;

        let dispatched = worker
            .client()
            .dispatch(&worker.address(), &stub_dispatch())
            .await
            .expect("a member that is listening takes the work");

        assert_eq!(
            heard(dispatched).await.expect("a member that refuses"),
            vec![NodeEvent::Refused(STUB_REFUSAL.to_owned())]
        );
    }

    /// A member that died mid-run. Told apart from one that answered nothing, and carrying how
    /// much it had said, because that is what decides whether the node may be run again.
    #[tokio::test]
    async fn a_member_that_stops_talking_is_not_a_member_that_answered() {
        let worker = StubWorker::start(Answers::CutOff).await;

        let dispatched = worker
            .client()
            .dispatch(&worker.address(), &stub_dispatch())
            .await
            .expect("a member that is listening takes the work");

        assert_eq!(
            heard(dispatched).await.err(),
            Some(ClusterError::CutOff { after: 1 })
        );
    }

    #[tokio::test]
    async fn a_body_that_is_not_an_event_is_refused_rather_than_guessed_at() {
        let worker = StubWorker::start(Answers::Garbage).await;

        let dispatched = worker
            .client()
            .dispatch(&worker.address(), &stub_dispatch())
            .await
            .expect("a member that is listening takes the work");

        assert!(matches!(
            heard(dispatched).await,
            Err(ClusterError::Malformed { .. })
        ));
    }

    /// An event held until it is whole is an event that can be made to never be whole.
    #[tokio::test]
    async fn an_event_that_never_ends_ends_the_stream() {
        let worker = StubWorker::start(Answers::Unending).await;

        let dispatched = worker
            .client()
            .dispatch(&worker.address(), &stub_dispatch())
            .await
            .expect("a member that is listening takes the work");

        assert_eq!(
            heard(dispatched).await.err(),
            Some(ClusterError::Overlong {
                max: MAX_EVENT_BYTES
            })
        );
    }

    /// A member that accepts the connection and says nothing at all: the client's own clock is
    /// what ends it, since a node that never starts never streams.
    #[tokio::test]
    async fn a_member_that_never_answers_is_given_up_on() {
        let silent = SilentMember::start().await;

        let sent = silent
            .client()
            .dispatch(&silent.address(), &stub_dispatch())
            .await;

        assert!(matches!(sent.err(), Some(ClusterError::Unreachable { .. })));
    }

    /// The whole of the point of a per-event clock: a run that takes longer than one event's
    /// patience is not a run that failed, as long as it keeps saying something.
    #[tokio::test]
    async fn a_slow_node_is_waited_for_as_long_as_it_keeps_saying_something() {
        let worker = StubWorker::start(Answers::Slowly).await;

        let dispatched = worker
            .client()
            .dispatch(&worker.address(), &stub_dispatch())
            .await
            .expect("a member that is listening takes the work");

        let heard = heard(dispatched)
            .await
            .expect("a member that answers slowly");
        assert_eq!(heard.last(), Some(&NodeEvent::Answered(stub_answer())));
    }

    /// Nothing is reachable at a port nothing is listening on, and nothing was spent there.
    #[tokio::test]
    async fn a_member_that_is_not_there_is_unreachable() {
        let client = ClusterClient::new(stub_settings());

        assert!(matches!(
            client
                .dispatch(&vacant_address().await, &stub_dispatch())
                .await
                .err(),
            Some(ClusterError::Unreachable { .. })
        ));
    }

    /// Every line ending a stream may be written with, and the data clear of all three of them.
    #[test]
    fn an_event_ends_at_the_first_blank_line_however_that_line_is_written() {
        assert_eq!(framed(b"data: 1\n\ndata: 2\n\n"), Some((data("1"), 9)));
        assert_eq!(framed(b"data: 1\r\n\r\ndata: 2"), Some((data("1"), 11)));
        assert_eq!(framed(b"data: 1\r\rdata: 2"), Some((data("1"), 9)));
    }

    #[test]
    fn half_an_event_is_not_an_event_yet() {
        assert_eq!(framed(b"data: 1\n"), None);
        assert_eq!(framed(b"data: 1\n\r"), None, "a CR may yet be a CRLF");
        assert_eq!(framed(b""), None);
    }

    #[test]
    fn a_data_line_is_read_whether_or_not_it_is_written_with_a_space() {
        assert_eq!(framed(b"data: 1\n\n"), Some((data("1"), 9)));
        assert_eq!(framed(b"data:1\n\n"), Some((data("1"), 8)));
        assert_eq!(framed(b"event: said\ndata: 1\n\n"), Some((data("1"), 21)));
        assert_eq!(framed(b"data: a\ndata: b\n\n"), Some((data("a\nb"), 17)));
    }

    /// A keep-alive says nothing, which is not the same as saying something unreadable.
    #[test]
    fn a_frame_of_no_data_carries_nothing_rather_than_an_empty_event() {
        assert_eq!(framed(b": keep alive\n\n"), Some((None, 14)));
        assert_eq!(framed(b"event: said\n\n"), Some((None, 13)));
    }

    /// What a framed event carries, as these tests spell it.
    fn data(carried: &str) -> Option<String> {
        Some(carried.to_owned())
    }
}
