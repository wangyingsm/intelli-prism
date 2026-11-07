//! What the tests of this crate build a run out of: a gateway with a fake upstream, and a
//! stub MCP server over a real socket.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::header::CONTENT_TYPE;
use http_body_util::{BodyExt, Full};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use ip_agents::{
    AgentConfig, AgentConfigParts, AgentRole, MaxTurns, Prompt, ReqTimeout, TimeLimit,
    ToolName as TemplateToolName, ToolSet,
};
use ip_auth::{Authority, Identity};
use ip_core::{
    AbsPath, ApiId, Capability, CapabilityScope, Dialect, Grant, Grants, Protocol, Role, TenantId,
    UserId,
};
use ip_gateway::{
    Gateway, GatewayBody, ProcessorChain, RoutingTable, Upstream, UpstreamError, from_bytes,
};
use ip_mcp::{McpAuth, McpEndpoint, McpId, McpName, McpServer, McpSettings};
use tokio::net::TcpListener;

use crate::ask::Target;

/// A deployment that routes `/anthropic` to one upstream and holds a credential for it, which
/// is what makes a turn the deployment's own request rather than a proxied one.
pub(crate) const CONFIG: &str = r#"
[server]
listen = "127.0.0.1:8080"

[storage]
backend = "sqlite"
path = "./test.db"

[cache]
backend = "sled"
path = "./test-cache"

[auth.jwt]
issuer = "intelli-prism"
secret = "0123456789abcdef0123456789abcdef"

[[upstream]]
id = "anthropic"
base_url = "https://api.example.com/v1"
api_key = "sk-deployment"
model = "claude-opus-5"
"#;

/// An answer in the anthropic family that says only this text and ends.
pub(crate) fn said(text: &str) -> String {
    serde_json::json!({
        "id": "msg_said",
        "model": "claude-opus-5",
        "role": "assistant",
        "stop_reason": "end_turn",
        "content": [{"type": "text", "text": text}],
        "usage": {"input_tokens": 9, "output_tokens": 2}
    })
    .to_string()
}

/// An answer that asks for one tool and stops for it.
pub(crate) fn wants(tool: &str, arguments: serde_json::Value) -> String {
    serde_json::json!({
        "id": "msg_wants",
        "model": "claude-opus-5",
        "role": "assistant",
        "stop_reason": "tool_use",
        "content": [
            {"type": "tool_use", "id": "call_1", "name": tool, "input": arguments}
        ],
        "usage": {"input_tokens": 9, "output_tokens": 7}
    })
    .to_string()
}

/// Records what it was sent, and answers with whatever it was given, in order.
pub(crate) struct Fake {
    pub(crate) seen: Mutex<Vec<(String, http::HeaderMap, Bytes)>>,
    answers: Mutex<VecDeque<(http::StatusCode, String)>>,
    held: Mutex<(http::StatusCode, String)>,
    delay: Duration,
}

impl Fake {
    /// Answers with this, however many times it is asked.
    pub(crate) fn answering(answer: &str) -> Arc<Self> {
        Self::holding(http::StatusCode::OK, answer)
    }

    /// Answers with each of these in turn, and with the last one thereafter.
    pub(crate) fn in_turn(answers: &[&str]) -> Arc<Self> {
        let queued: VecDeque<_> = answers
            .iter()
            .map(|answer| (http::StatusCode::OK, (*answer).to_owned()))
            .collect();
        let last = queued
            .back()
            .cloned()
            .unwrap_or((http::StatusCode::OK, String::new()));
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            answers: Mutex::new(queued),
            held: Mutex::new(last),
            delay: Duration::ZERO,
        })
    }

    /// Answers with this status and body, however many times it is asked.
    pub(crate) fn holding(status: http::StatusCode, answer: &str) -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            answers: Mutex::new(VecDeque::new()),
            held: Mutex::new((status, answer.to_owned())),
            delay: Duration::ZERO,
        })
    }

    /// The same, but it takes this long about it.
    pub(crate) fn slow(answer: &str, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            answers: Mutex::new(VecDeque::new()),
            held: Mutex::new((http::StatusCode::OK, answer.to_owned())),
            delay,
        })
    }

    /// How many times it was asked.
    pub(crate) fn asked(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    /// The body of the last request, as json.
    pub(crate) fn sent(&self) -> serde_json::Value {
        self.nth(self.asked() - 1)
    }

    /// The body of the nth request, as json.
    pub(crate) fn nth(&self, which: usize) -> serde_json::Value {
        let seen = self.seen.lock().unwrap();
        let (_, _, body) = seen.get(which).expect("it was not asked that many times");
        serde_json::from_slice(body).expect("what went out is json")
    }
}

#[async_trait::async_trait]
impl Upstream for Fake {
    async fn send(
        &self,
        request: http::Request<GatewayBody>,
    ) -> Result<http::Response<GatewayBody>, UpstreamError> {
        let uri = request.uri().to_string();
        let headers = request.headers().clone();
        let body = request.into_body().collect().await.unwrap().to_bytes();
        self.seen.lock().unwrap().push((uri, headers, body));
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        let (status, answer) = self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| self.held.lock().unwrap().clone());
        Ok(http::Response::builder()
            .status(status)
            .header(CONTENT_TYPE, "application/json")
            .body(from_bytes(Bytes::from(answer)))
            .unwrap())
    }
}

/// A gateway routed by what [`CONFIG`] declares.
pub(crate) fn gateway(upstream: Arc<Fake>) -> Gateway {
    let config = ip_config::Config::parse(CONFIG).unwrap();
    Gateway::new(
        RoutingTable::build(&config, Vec::new()).unwrap(),
        ProcessorChain::new(),
        upstream,
    )
}

/// Where a run's turns go, as an account that may use that api.
pub(crate) fn target(speaks: Dialect) -> Target {
    let user = UserId::new("alice").unwrap();
    let tenant = TenantId::new("acme").unwrap();
    let scope = CapabilityScope::Api {
        user: user.clone(),
        tenant: tenant.clone(),
        api: ApiId::new("anthropic").unwrap(),
    };
    let grants: Grants = [Grant::new(Capability::ApiAccess, scope).unwrap()]
        .into_iter()
        .collect();
    Target {
        path: AbsPath::new("/anthropic/messages").unwrap(),
        speaks,
        authority: Authority::new(
            Identity {
                user,
                tenant,
                role: Role::Member,
            },
            grants,
        ),
        listen: "127.0.0.1:8080".parse().unwrap(),
        protocol: Protocol::Http,
    }
}

/// A template with these tools and these ceilings.
pub(crate) fn template(tools: &[&str], max_turns: u16, time_limit: u32, req: u32) -> AgentConfig {
    AgentConfig::new(AgentConfigParts {
        role: AgentRole::System,
        prompt: Prompt::new("Be helpful.").unwrap(),
        tool_set: asking(tools),
        max_turns: MaxTurns::new(max_turns).unwrap(),
        time_limit: TimeLimit::new(time_limit).unwrap(),
        req_timeout: ReqTimeout::new(req).unwrap(),
        use_best_effort: false,
        best_effort_strategy: None,
        model: None,
        api: None,
    })
    .unwrap()
}

/// A tool set naming these.
pub(crate) fn asking(tools: &[&str]) -> ToolSet {
    ToolSet::new(
        tools
            .iter()
            .map(|one| TemplateToolName::new(one).unwrap())
            .collect(),
    )
    .unwrap()
}

/// What a stub MCP server does when it is asked.
#[derive(Clone, Copy)]
pub(crate) enum Serving {
    /// Offers `sql` and `search`, and echoes a call's arguments back as text.
    Both,
    /// Offers `sql` only.
    SqlOnly,
    /// Handshakes, then refuses every call.
    RefusesCalls,
    /// Offers `sql`, and takes [`SLOW`] over every call.
    SlowCalls,
}

/// How long [`Serving::SlowCalls`] takes over one call.
pub(crate) const SLOW: Duration = Duration::from_millis(200);

/// Every json-rpc message a stub server was sent.
pub(crate) type Called = Arc<Mutex<Vec<serde_json::Value>>>;

async fn answer(
    called: Called,
    serving: Serving,
    request: hyper::Request<hyper::body::Incoming>,
) -> Result<hyper::Response<Full<Bytes>>, std::convert::Infallible> {
    let body = request.into_body().collect().await.unwrap().to_bytes();
    let asked: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    let method = asked["method"].as_str().unwrap_or_default().to_owned();
    let id = asked["id"].clone();
    called.lock().unwrap().push(asked.clone());
    let result = match method.as_str() {
        "initialize" => serde_json::json!({
            "protocolVersion": ip_mcp::PROTOCOL_VERSION,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "stub", "version": "1"}
        }),
        "notifications/initialized" => {
            return Ok(hyper::Response::builder()
                .status(202)
                .body(Full::new(Bytes::new()))
                .unwrap());
        }
        "tools/list" => {
            let schema = serde_json::json!({"type": "object", "properties": {}});
            let mut tools = vec![serde_json::json!({
                "name": "sql", "description": "runs sql", "inputSchema": schema
            })];
            if matches!(serving, Serving::Both) {
                tools.push(serde_json::json!({
                    "name": "search", "description": "searches", "inputSchema": schema
                }));
            }
            serde_json::json!({ "tools": tools })
        }
        "tools/call" => {
            if matches!(serving, Serving::SlowCalls) {
                tokio::time::sleep(SLOW).await;
            }
            if matches!(serving, Serving::RefusesCalls) {
                let refused = serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32602, "message": "it will not"}
                });
                return Ok(hyper::Response::builder()
                    .header("content-type", "application/json")
                    .body(Full::new(Bytes::from(refused.to_string())))
                    .unwrap());
            }
            serde_json::json!({
                "content": [{"type": "text", "text": asked["params"]["arguments"].to_string()}]
            })
        }
        _ => serde_json::json!({}),
    };
    let answered = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
    Ok(hyper::Response::builder()
        .header("content-type", "application/json")
        .header("mcp-session-id", "stub-session")
        .body(Full::new(Bytes::from(answered.to_string())))
        .unwrap())
}

/// An MCP server on a real socket, answering as `serving` says.
pub(crate) async fn stub(serving: Serving) -> (SocketAddr, Called) {
    let called: Called = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let kept = Arc::clone(&called);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let kept = Arc::clone(&kept);
            tokio::spawn(async move {
                let _ = http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(move |request| answer(Arc::clone(&kept), serving, request)),
                    )
                    .await;
            });
        }
    });
    (address, called)
}

/// That server, as the registry holds it.
pub(crate) fn registered(id: &str, address: SocketAddr) -> McpServer {
    McpServer {
        id: McpId::new(id).unwrap(),
        name: McpName::new("Stub").unwrap(),
        endpoint: McpEndpoint::new(&format!("http://{address}/mcp")).unwrap(),
        auth: McpAuth::None,
    }
}

/// Settings patient enough that a test's own clock is what fails it.
pub(crate) fn settings() -> McpSettings {
    McpSettings {
        connect_timeout: Duration::from_secs(2),
        request_timeout: Duration::from_secs(2),
        max_answer_bytes: 64 * 1024,
    }
}
