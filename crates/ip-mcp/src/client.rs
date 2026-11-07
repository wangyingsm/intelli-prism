//! Speaking JSON-RPC to one MCP server, over the streamable http transport.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderValue, Request, header};
use http_body_util::{BodyExt, Full, Limited};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

use crate::error::McpError;
use crate::rpc::{RpcRequest, RpcResponse};
use crate::server::{McpAuth, McpServer};
use crate::tool::Offering;

/// The version of the protocol this client asks for.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Every version this client will go on talking to a server over.
///
/// A server answering the handshake with an older one it still speaks is talked to in that one,
/// which is what the protocol's own negotiation asks for; anything not here is refused, because
/// a version this code has never seen is one it cannot promise to read.
const SPOKEN: [&str; 2] = ["2025-06-18", "2025-03-26"];

/// What this client calls itself to a server.
const CLIENT_NAME: &str = "intelli-prism";

/// The header a server names a conversation with, and every later call repeats.
const SESSION: &str = "mcp-session-id";

/// The header that tells a server which version the calls after the handshake are in.
const SPEAKING: &str = "mcp-protocol-version";

/// How many pages of one listing this will follow before giving up on the server.
const MAX_PAGES: usize = 32;

/// The pooling http client this crate talks over.
type Pooled = Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>;

/// How a client reaches a server, and how patient it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpSettings {
    /// How long a connection may take to establish.
    pub connect_timeout: Duration,
    /// How long one call may take before the server is given up on.
    pub request_timeout: Duration,
    /// How much of an answer this will read before refusing the rest.
    pub max_answer_bytes: usize,
}

impl Default for McpSettings {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            max_answer_bytes: 4 * 1024 * 1024,
        }
    }
}

/// What a server said about itself when it answered the handshake.
///
/// Unknown fields are ignored rather than refused: a server that also offers resources, prompts
/// or logging is not a server this cannot talk to, it is one offering more than this asks for.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Told {
    /// The version of the protocol it will speak, which is one this client speaks too.
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    /// What it offers.
    #[serde(default)]
    pub capabilities: Offered,
    /// What it calls itself, where it says.
    #[serde(rename = "serverInfo", default)]
    pub server_info: Option<ServerInfo>,
}

/// What a server says it offers.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct Offered {
    /// Present where the server has tools at all.
    #[serde(default)]
    pub tools: Option<ToolsOffered>,
}

/// What a server says about its tools.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct ToolsOffered {
    /// Whether it will say when its list changes. Nothing listens for that yet.
    #[serde(rename = "listChanged", default)]
    pub list_changed: bool,
}

/// What a server calls itself.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct ServerInfo {
    /// Its name, for a person reading a log.
    pub name: String,
    /// Its version, where it says.
    #[serde(default)]
    pub version: Option<String>,
}

/// One MCP server, handshaken and ready to be asked things.
pub struct McpClient {
    client: Pooled,
    server: McpServer,
    settings: McpSettings,
    told: Told,
    session: Option<HeaderValue>,
    next_id: AtomicU64,
}

impl McpClient {
    /// Reaches a server, agrees a protocol version with it, and tells it the handshake is done.
    ///
    /// Three messages, because the protocol asks for three: the call, the answer, and a
    /// notification saying this client is ready. A server never told that may answer the first
    /// real call with an error about not having been initialized.
    ///
    /// Nothing about the server is believed before this returns, which is why the client is
    /// built from what the handshake learned rather than filled in afterwards.
    pub async fn open(server: McpServer, settings: McpSettings) -> Result<Self, McpError> {
        let client = pooled(settings);
        let next_id = AtomicU64::new(1);
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        let handshake = RpcRequest::call(
            id,
            "initialize",
            serde_json::json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": CLIENT_NAME,
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        );
        let answered = carry(
            Carrying {
                client: &client,
                server: &server,
                settings: &settings,
                session: None,
                speaking: None,
            },
            &handshake,
        )
        .await?;
        let Some(message) = answered.message else {
            return Err(McpError::NotJsonRpc {
                detail: "the handshake was answered with nothing".to_owned(),
            });
        };
        let told: Told = serde_json::from_value(message.answer_to(id)?).map_err(|error| {
            McpError::NotJsonRpc {
                detail: error.to_string(),
            }
        })?;
        if !SPOKEN.contains(&told.protocol_version.as_str()) {
            return Err(McpError::UnsupportedProtocol {
                theirs: told.protocol_version,
                ours: PROTOCOL_VERSION,
            });
        }
        let session = answered.session;
        carry(
            Carrying {
                client: &client,
                server: &server,
                settings: &settings,
                session: session.as_ref(),
                speaking: Some(&told.protocol_version),
            },
            &RpcRequest::notify("notifications/initialized", serde_json::json!({})),
        )
        .await?;
        Ok(Self {
            client,
            server,
            settings,
            told,
            session,
            next_id,
        })
    }

    /// What the server said it is.
    pub fn told(&self) -> &Told {
        &self.told
    }

    /// Which server this is, as the deployment registered it.
    pub fn server(&self) -> &McpServer {
        &self.server
    }

    /// Every tool the server offers, and every one it offered that cannot be used.
    ///
    /// A server that declared no tools at all is not asked for any, which is what the protocol
    /// says to do: asking anyway earns an error from a server behaving correctly.
    ///
    /// A server may hand its tools over a page at a time, so this follows the cursor until the
    /// server names no further page — or until [`MAX_PAGES`], because a server this deployment
    /// does not run can name one forever.
    pub async fn tools(&self) -> Result<Offering, McpError> {
        if self.told.capabilities.tools.is_none() {
            return Ok(Offering::default());
        }
        let mut offering = Offering::default();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(cursor) => serde_json::json!({"cursor": cursor}),
                None => serde_json::json!({}),
            };
            let page = self.call("tools/list", params).await?;
            match page.get("tools").and_then(serde_json::Value::as_array) {
                Some(listed) => offering.extend_from(listed),
                None => {
                    return Err(McpError::NotATool {
                        detail: "a listing of tools held no tools".to_owned(),
                    });
                }
            }
            cursor = page
                .get("nextCursor")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            if cursor.is_none() {
                return Ok(offering);
            }
        }
        Err(McpError::TooManyPages { max: MAX_PAGES })
    }

    /// Asks the server for something, and reads what it answers.
    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let answered = carry(
            Carrying {
                client: &self.client,
                server: &self.server,
                settings: &self.settings,
                session: self.session.as_ref(),
                speaking: Some(&self.told.protocol_version),
            },
            &RpcRequest::call(id, method, params),
        )
        .await?;
        let Some(message) = answered.message else {
            return Err(McpError::NotJsonRpc {
                detail: "a call was answered with nothing".to_owned(),
            });
        };
        message.answer_to(id)
    }
}

/// Everything one message needs to reach a server, which is more than a method and its params.
struct Carrying<'a> {
    /// The connection pool to send over.
    client: &'a Pooled,
    /// Where to send it, and what the server requires.
    server: &'a McpServer,
    /// How long to wait, and how much to read.
    settings: &'a McpSettings,
    /// What the server named this conversation, before it has named one.
    session: Option<&'a HeaderValue>,
    /// The agreed protocol version, before one is agreed.
    speaking: Option<&'a str>,
}

/// What came back from one message.
struct Answered {
    /// The answer, where the message was one a server answers.
    message: Option<RpcResponse>,
    /// The conversation the server named, where it named one.
    session: Option<HeaderValue>,
}

/// Carries one message to a server and reads what came back.
async fn carry(carrying: Carrying<'_>, message: &RpcRequest) -> Result<Answered, McpError> {
    let payload = serde_json::to_vec(message).map_err(|error| McpError::NotJsonRpc {
        detail: error.to_string(),
    })?;
    let mut request = Request::builder()
        .method(http::Method::POST)
        .uri(carrying.server.endpoint.as_str())
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream");
    if let McpAuth::Bearer { token } = &carrying.server.auth {
        let presented =
            HeaderValue::from_str(&format!("Bearer {}", token.expose())).map_err(|error| {
                McpError::Unreachable {
                    detail: format!("the credential will not go in a header: {error}"),
                }
            })?;
        request = request.header(header::AUTHORIZATION, presented);
    }
    if let Some(session) = carrying.session {
        request = request.header(SESSION, session);
    }
    if let Some(speaking) = carrying.speaking {
        request = request.header(SPEAKING, speaking);
    }
    let request = request
        .body(Full::new(Bytes::from(payload)))
        .map_err(|error| McpError::Unreachable {
            detail: error.to_string(),
        })?;

    let answered = tokio::time::timeout(
        carrying.settings.request_timeout,
        carrying.client.request(request),
    )
    .await
    .map_err(|_| McpError::Unreachable {
        detail: format!(
            "no answer within {} seconds",
            carrying.settings.request_timeout.as_secs()
        ),
    })?
    .map_err(|error| McpError::Unreachable {
        detail: error.to_string(),
    })?;

    if !answered.status().is_success() {
        return Err(McpError::Refused {
            status: answered.status().as_u16(),
        });
    }
    let session = answered.headers().get(SESSION).cloned();
    let streamed = answered
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|kind| kind.to_str().ok())
        .is_some_and(|kind| kind.starts_with("text/event-stream"));
    let body = Limited::new(answered.into_body(), carrying.settings.max_answer_bytes)
        .collect()
        .await
        .map_err(|_| McpError::AnswerTooLong {
            max: carrying.settings.max_answer_bytes,
        })?
        .to_bytes();
    if body.is_empty() {
        return Ok(Answered {
            message: None,
            session,
        });
    }
    let text = String::from_utf8_lossy(&body);
    let Some(message) = (if streamed {
        first_event(&text)
    } else {
        Some(text.as_ref())
    }) else {
        return Ok(Answered {
            message: None,
            session,
        });
    };
    let message = serde_json::from_str(message).map_err(|error| McpError::NotJsonRpc {
        detail: error.to_string(),
    })?;
    Ok(Answered {
        message: Some(message),
        session,
    })
}

/// The data of the first server-sent event in a stream, where there is one.
///
/// A POST over this transport is answered with one message, so the first event carries it; what
/// followed would be a server pushing on a connection nothing here keeps open.
fn first_event(text: &str) -> Option<&str> {
    text.lines()
        .find_map(|line| line.strip_prefix("data:").map(str::trim_start))
}

/// The connection pool every message to one server goes over.
fn pooled(settings: McpSettings) -> Pooled {
    install_crypto_provider();
    let mut http = HttpConnector::new();
    http.set_connect_timeout(Some(settings.connect_timeout));
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
    use std::convert::Infallible;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use http::{Response, StatusCode};
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use tokio::net::TcpListener;

    use super::*;
    use crate::id::McpId;
    use crate::server::{McpEndpoint, McpName, McpToken};

    /// The credential the server below requires, where a test gives it one.
    const TOKEN: &str = "wh-live-9f2c4a7b1e";

    /// What the stub server saw of one message.
    #[derive(Debug, Clone)]
    struct Seen {
        method: String,
        authorization: Option<String>,
        session: Option<String>,
        speaking: Option<String>,
        accept: Option<String>,
    }

    /// Everything the stub saw, in the order it saw it.
    type Record = Arc<Mutex<Vec<Seen>>>;

    /// How the stub answers a call. Every one of these is a server somebody really runs.
    #[derive(Debug, Clone, Copy)]
    enum Answers {
        /// Properly, in json, naming a session.
        Plain,
        /// Properly, but naming no session: the transport allows it.
        WithoutSession,
        /// Properly, as one server-sent event.
        Streamed,
        /// Properly, in a version of the protocol it chose.
        Version(&'static str),
        /// With a json-rpc error.
        Failure,
        /// With something that is not json-rpc at all.
        Garbage,
        /// With an answer to another call.
        WrongId,
        /// With a status and no message.
        Status,
        /// With more than the caller will read.
        Long,
        /// Saying it has no tools at all.
        Toolless,
    }

    /// What the stub offers when it is asked for its tools.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Listing {
        /// Two tools a model can be told about.
        Plain,
        /// One good tool and three this gateway will not declare.
        Unusable,
        /// Two pages, the first naming the second.
        Paged,
        /// A page that always names another.
        Forever,
        /// An answer that holds no tools at all.
        Empty,
    }

    fn handshake_result(version: &str, answers: Answers) -> serde_json::Value {
        let capabilities = match answers {
            Answers::Toolless => serde_json::json!({"resources": {}}),
            _ => serde_json::json!({"tools": {"listChanged": true}}),
        };
        serde_json::json!({
            "protocolVersion": version,
            "capabilities": capabilities,
            "serverInfo": {"name": "Warehouse", "version": "1.2.3"},
        })
    }

    fn listed(listing: Listing, cursor: Option<&str>) -> serde_json::Value {
        match listing {
            Listing::Plain => serde_json::json!({"tools": [
                {"name": "get_stock", "description": "What is in the warehouse",
                 "inputSchema": {"type": "object", "properties": {"sku": {"type": "string"}}}},
                {"name": "ping"},
            ]}),
            Listing::Unusable => serde_json::json!({"tools": [
                {"name": "get_stock"},
                {"name": "stock.get"},
                {"name": ""},
                {"description": "nameless"},
            ]}),
            Listing::Paged => match cursor {
                None => serde_json::json!({
                    "tools": [{"name": "get_stock"}],
                    "nextCursor": "page-2",
                }),
                Some(_) => serde_json::json!({"tools": [{"name": "ping"}]}),
            },
            Listing::Forever => serde_json::json!({
                "tools": [{"name": "ping"}],
                "nextCursor": "and-another",
            }),
            Listing::Empty => serde_json::json!({"nothing": "here"}),
        }
    }

    async fn answer(
        record: Record,
        answers: Answers,
        listing: Listing,
        request: Request<hyper::body::Incoming>,
    ) -> Result<Response<Full<Bytes>>, Infallible> {
        let (parts, body) = request.into_parts();
        let got = |name: &str| {
            parts
                .headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let seen = Seen {
            method: String::new(),
            authorization: got(header::AUTHORIZATION.as_str()),
            session: got(SESSION),
            speaking: got(SPEAKING),
            accept: got(header::ACCEPT.as_str()),
        };
        let body = body.collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        record.lock().unwrap().push(Seen {
            method: body["method"].as_str().unwrap_or_default().to_owned(),
            ..seen
        });

        // A notification is answered with nothing, as the protocol says.
        let Some(id) = body.get("id").cloned() else {
            return Ok(Response::builder()
                .status(StatusCode::ACCEPTED)
                .body(Full::new(Bytes::new()))
                .unwrap());
        };
        let result = if body["method"] == "initialize" {
            handshake_result(
                match answers {
                    Answers::Version(version) => version,
                    _ => PROTOCOL_VERSION,
                },
                answers,
            )
        } else if matches!(answers, Answers::Long) {
            serde_json::json!({"padding": "x".repeat(4096)})
        } else if body["method"] == "tools/list" {
            listed(
                listing,
                body["params"].get("cursor").and_then(|it| it.as_str()),
            )
        } else {
            serde_json::json!({"asked": body["method"].clone()})
        };

        let mut built = Response::builder().status(StatusCode::OK);
        if !matches!(answers, Answers::WithoutSession) {
            built = built.header(SESSION, "s-1");
        }
        Ok(match answers {
            Answers::Status => Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Full::new(Bytes::new()))
                .unwrap(),
            Answers::Garbage => built
                .header(header::CONTENT_TYPE, "text/html")
                .body(Full::new(Bytes::from_static(b"<html>not me</html>")))
                .unwrap(),
            Answers::Streamed => {
                let message = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
                built
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .body(Full::new(Bytes::from(format!(
                        "event: message\ndata: {message}\n\n"
                    ))))
                    .unwrap()
            }
            Answers::Failure => json_body(
                built,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32000, "message": "the warehouse is closed"},
                }),
            ),
            Answers::WrongId => json_body(
                built,
                &serde_json::json!({"jsonrpc": "2.0", "id": 99, "result": result}),
            ),
            Answers::Plain
            | Answers::WithoutSession
            | Answers::Version(_)
            | Answers::Long
            | Answers::Toolless => json_body(
                built,
                &serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
            ),
        })
    }

    fn json_body(
        built: http::response::Builder,
        payload: &serde_json::Value,
    ) -> Response<Full<Bytes>> {
        built
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(payload.to_string())))
            .unwrap()
    }

    async fn stub(answers: Answers, listing: Listing) -> (SocketAddr, Record) {
        let record: Record = Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let kept = Arc::clone(&record);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let kept = Arc::clone(&kept);
                tokio::spawn(async move {
                    let _ = http1::Builder::new()
                        .serve_connection(
                            TokioIo::new(stream),
                            service_fn(move |request| {
                                answer(Arc::clone(&kept), answers, listing, request)
                            }),
                        )
                        .await;
                });
            }
        });
        (address, record)
    }

    /// Accepts the connection and answers nothing, so the client's own clock is what ends it.
    async fn silent() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        address
    }

    fn server(address: SocketAddr, auth: McpAuth) -> McpServer {
        McpServer {
            id: McpId::new("warehouse").unwrap(),
            name: McpName::new("Warehouse tools").unwrap(),
            endpoint: McpEndpoint::new(&format!("http://{address}/mcp")).unwrap(),
            auth,
        }
    }

    fn settings() -> McpSettings {
        McpSettings {
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_millis(300),
            max_answer_bytes: 64 * 1024,
        }
    }

    async fn opened(answers: Answers) -> (Result<McpClient, McpError>, Record) {
        let (address, record) = stub(answers, Listing::Plain).await;
        let client = McpClient::open(server(address, McpAuth::None), settings()).await;
        (client, record)
    }

    async fn offering(listing: Listing) -> (Result<Offering, McpError>, Record) {
        let (address, record) = stub(Answers::Plain, listing).await;
        let client = McpClient::open(server(address, McpAuth::None), settings())
            .await
            .unwrap();
        let offered = client.tools().await;
        (offered, record)
    }

    #[tokio::test]
    async fn a_handshake_says_what_the_server_is_and_what_it_offers() {
        let (client, _) = opened(Answers::Plain).await;
        let client = client.unwrap();

        let told = client.told();
        assert_eq!(told.protocol_version, PROTOCOL_VERSION);
        assert_eq!(
            told.server_info,
            Some(ServerInfo {
                name: "Warehouse".to_owned(),
                version: Some("1.2.3".to_owned()),
            })
        );
        assert_eq!(
            told.capabilities.tools,
            Some(ToolsOffered { list_changed: true })
        );
        assert_eq!(client.server().id.as_str(), "warehouse");
    }

    /// The handshake is three messages, and the third is the one a server waits for.
    #[tokio::test]
    async fn the_server_is_told_the_handshake_is_done() {
        let (client, record) = opened(Answers::Plain).await;
        client.unwrap();

        let seen = record.lock().unwrap().clone();
        let methods: Vec<&str> = seen.iter().map(|one| one.method.as_str()).collect();
        assert_eq!(methods, ["initialize", "notifications/initialized"]);
        assert_eq!(
            seen[0].accept.as_deref(),
            Some("application/json, text/event-stream"),
            "a server may answer either way, so both are asked for"
        );
    }

    /// The session the server named, carried on everything after the handshake and not before.
    #[tokio::test]
    async fn the_conversation_a_server_names_is_named_back_to_it() {
        let (client, record) = opened(Answers::Plain).await;
        let client = client.unwrap();
        client
            .call("tools/list", serde_json::json!({}))
            .await
            .unwrap();

        let seen = record.lock().unwrap().clone();
        assert_eq!(seen[0].session, None, "nothing was named yet");
        assert_eq!(seen[1].session.as_deref(), Some("s-1"));
        assert_eq!(seen[2].session.as_deref(), Some("s-1"));
        assert_eq!(
            seen[0].speaking, None,
            "no version is agreed until the server answers"
        );
        assert_eq!(seen[2].speaking.as_deref(), Some(PROTOCOL_VERSION));
    }

    #[tokio::test]
    async fn a_server_that_names_no_conversation_is_talked_to_anyway() {
        let (client, record) = opened(Answers::WithoutSession).await;
        let client = client.unwrap();
        client
            .call("tools/list", serde_json::json!({}))
            .await
            .unwrap();

        assert!(
            record
                .lock()
                .unwrap()
                .iter()
                .all(|one| one.session.is_none()),
            "a session nobody named was named anyway"
        );
    }

    #[tokio::test]
    async fn a_credential_is_presented_to_the_server_that_requires_one() {
        let (address, record) = stub(Answers::Plain, Listing::Plain).await;
        let auth = McpAuth::Bearer {
            token: McpToken::new(TOKEN).unwrap(),
        };
        McpClient::open(server(address, auth), settings())
            .await
            .unwrap();

        let seen = record.lock().unwrap().clone();
        assert!(
            seen.iter()
                .all(|one| one.authorization.as_deref() == Some(&format!("Bearer {TOKEN}"))),
            "{seen:?}"
        );
    }

    #[tokio::test]
    async fn a_server_that_requires_nothing_is_asked_without_one() {
        let (_, record) = opened(Answers::Plain).await;
        assert!(
            record
                .lock()
                .unwrap()
                .iter()
                .all(|one| one.authorization.is_none()),
            "a credential was presented to a server that asked for none"
        );
    }

    #[tokio::test]
    async fn an_answer_sent_as_an_event_is_read_like_any_other() {
        let (client, _) = opened(Answers::Streamed).await;
        let client = client.unwrap();
        assert_eq!(client.told().protocol_version, PROTOCOL_VERSION);

        let answered = client.call("ping", serde_json::json!({})).await.unwrap();
        assert_eq!(answered["asked"], "ping");
    }

    #[tokio::test]
    async fn a_server_speaking_a_version_this_also_speaks_is_spoken_to_in_it() {
        let (client, record) = opened(Answers::Version("2025-03-26")).await;
        let client = client.unwrap();

        assert_eq!(client.told().protocol_version, "2025-03-26");
        assert_eq!(
            record.lock().unwrap()[1].speaking.as_deref(),
            Some("2025-03-26"),
            "the version agreed is the one spoken, not the one asked for"
        );
    }

    #[tokio::test]
    async fn a_server_speaking_a_version_this_does_not_is_refused() {
        let (client, _) = opened(Answers::Version("1999-01-01")).await;
        assert_eq!(
            client.err(),
            Some(McpError::UnsupportedProtocol {
                theirs: "1999-01-01".to_owned(),
                ours: PROTOCOL_VERSION,
            })
        );
    }

    #[tokio::test]
    async fn a_server_that_will_not_answer_says_why_and_that_is_carried_back() {
        let (client, _) = opened(Answers::Failure).await;
        assert_eq!(
            client.err(),
            Some(McpError::ServerRefused {
                code: -32000,
                message: "the warehouse is closed".to_owned(),
            })
        );
    }

    #[tokio::test]
    async fn something_that_is_not_json_rpc_is_not_a_server_this_talks_to() {
        let (client, _) = opened(Answers::Garbage).await;
        assert!(
            matches!(client, Err(McpError::NotJsonRpc { .. })),
            "{:?}",
            client.err()
        );
    }

    #[tokio::test]
    async fn an_answer_to_another_call_is_refused() {
        let (client, _) = opened(Answers::WrongId).await;
        assert_eq!(client.err(), Some(McpError::AnsweredSomethingElse));
    }

    #[tokio::test]
    async fn a_server_answering_with_a_status_is_refused_by_it() {
        let (client, _) = opened(Answers::Status).await;
        assert_eq!(client.err(), Some(McpError::Refused { status: 500 }));
    }

    #[tokio::test]
    async fn a_server_that_never_answers_is_given_up_on() {
        let address = silent().await;
        let client = McpClient::open(server(address, McpAuth::None), settings()).await;
        assert!(
            matches!(client, Err(McpError::Unreachable { ref detail }) if detail.contains("within")),
            "{:?}",
            client.err()
        );
    }

    #[tokio::test]
    async fn nothing_listening_is_a_server_that_cannot_be_reached() {
        // Bound and dropped, so the port is one nothing answers on.
        let address = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        let client = McpClient::open(server(address, McpAuth::None), settings()).await;
        assert!(
            matches!(client, Err(McpError::Unreachable { .. })),
            "{:?}",
            client.err()
        );
    }

    /// A server this deployment does not run may answer with as much as it likes.
    #[tokio::test]
    async fn an_answer_past_what_this_will_read_is_refused() {
        let (address, _) = stub(Answers::Long, Listing::Plain).await;
        let client = McpClient::open(
            server(address, McpAuth::None),
            McpSettings {
                max_answer_bytes: 512,
                ..settings()
            },
        )
        .await
        .unwrap();

        assert_eq!(
            client.call("tools/list", serde_json::json!({})).await,
            Err(McpError::AnswerTooLong { max: 512 })
        );
    }

    #[tokio::test]
    async fn every_tool_a_server_offers_is_read() {
        let (offered, record) = offering(Listing::Plain).await;
        let offered = offered.unwrap();

        assert_eq!(offered.dropped, []);
        let names: Vec<&str> = offered.tools.iter().map(|it| it.name.as_str()).collect();
        assert_eq!(names, ["get_stock", "ping"]);
        assert_eq!(
            offered.tools[0].description.as_deref(),
            Some("What is in the warehouse")
        );
        assert_eq!(
            record.lock().unwrap()[2].method,
            "tools/list",
            "the listing is asked for by its own method"
        );
    }

    /// One tool nobody can be told about does not take the listing down with it.
    #[tokio::test]
    async fn a_tool_this_cannot_declare_is_dropped_and_the_rest_is_offered() {
        let (offered, _) = offering(Listing::Unusable).await;
        let offered = offered.unwrap();

        let names: Vec<&str> = offered.tools.iter().map(|it| it.name.as_str()).collect();
        assert_eq!(names, ["get_stock"]);
        assert_eq!(offered.dropped.len(), 3);
        assert_eq!(offered.dropped[0].name, "stock.get");
        assert_eq!(
            offered.dropped[0].why,
            McpError::IllegalChar {
                kind: crate::McpToolName::KIND,
                ch: '.',
            }
        );
    }

    #[tokio::test]
    async fn a_server_handing_its_tools_over_a_page_at_a_time_is_read_to_the_end() {
        let (offered, record) = offering(Listing::Paged).await;

        let offered = offered.unwrap();
        let names: Vec<&str> = offered.tools.iter().map(|it| it.name.as_str()).collect();
        assert_eq!(names, ["get_stock", "ping"]);
        assert_eq!(
            record.lock().unwrap().len(),
            4,
            "two handshake messages and two pages"
        );
    }

    #[tokio::test]
    async fn a_server_that_pages_forever_is_given_up_on() {
        let (offered, record) = offering(Listing::Forever).await;

        assert_eq!(offered, Err(McpError::TooManyPages { max: MAX_PAGES }));
        assert_eq!(record.lock().unwrap().len(), 2 + MAX_PAGES);
    }

    #[tokio::test]
    async fn a_listing_that_lists_nothing_is_not_a_listing() {
        let (offered, _) = offering(Listing::Empty).await;
        assert!(
            matches!(offered, Err(McpError::NotATool { .. })),
            "{offered:?}"
        );
    }

    /// The protocol says not to ask, and a server behaving correctly answers an error if asked.
    #[tokio::test]
    async fn a_server_that_declared_no_tools_is_not_asked_for_any() {
        let (client, record) = opened(Answers::Toolless).await;
        let client = client.unwrap();
        assert_eq!(client.told().capabilities.tools, None);

        assert_eq!(client.tools().await, Ok(Offering::default()));
        assert_eq!(
            record.lock().unwrap().len(),
            2,
            "the handshake and nothing else"
        );
    }

    #[test]
    fn the_first_event_of_a_stream_is_the_answer_to_the_call() {
        assert_eq!(
            first_event("event: message\ndata: {\"a\":1}\n\ndata: {\"b\":2}\n\n"),
            Some("{\"a\":1}")
        );
        assert_eq!(first_event(": a comment\n\n"), None);
        assert_eq!(first_event(""), None);
    }
}
