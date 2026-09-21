//! What every route group's tests build their server from.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, Response};
use ip_auth::{Passphrase, PassphraseHasher};
use ip_config::Config;
use ip_core::{PassphraseHash, UserId};
use ip_gateway::body::GatewayBody;
use ip_gateway::upstream::Upstream;
use ip_gateway::{Gateway, ProcessorChain, RoutingTable, UpstreamError};
use ip_storage::SqliteStore;

use crate::manage::CSRF_HEADER;
use crate::state::{AppState, Stores};

/// What every account in these tests logs in with.
pub const PASSPHRASE: &str = "correct horse staple";

/// A configuration that names nothing the tests open.
const CONFIG: &str = r#"
[server]
listen = "127.0.0.1:8080"

[storage]
backend = "sqlite"
path = "./unopened.db"

[cache]
backend = "sled"
path = "./unopened"

[auth.jwt]
issuer = "intelli-prism"
secret = "0123456789abcdef0123456789abcdef"
"#;

/// An upstream nothing in these tests reaches.
struct Unreachable;

#[async_trait::async_trait]
impl Upstream for Unreachable {
    async fn send(
        &self,
        _request: Request<GatewayBody>,
    ) -> Result<Response<GatewayBody>, UpstreamError> {
        Err(UpstreamError::new("nothing is sent upstream here"))
    }
}

/// The verifier every fixture account is created with.
pub fn hashed() -> PassphraseHash {
    PassphraseHasher::new()
        .hash(&Passphrase::new(PASSPHRASE).unwrap())
        .unwrap()
}

/// The server's own state, over a store the test has filled and a gateway that proxies
/// nothing.
pub fn state_over(store: SqliteStore) -> AppState {
    let listen: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let table = RoutingTable::build(&Config::parse(CONFIG).unwrap(), Vec::new()).unwrap();
    let gateway = Gateway::new(table, ProcessorChain::new(), Arc::new(Unreachable));
    AppState::with_parts(Stores::Sqlite(Arc::new(store)), gateway, listen)
}

/// The cookie a real login hands back.
pub async fn cookie_of(state: &AppState, user: &UserId) -> String {
    let token = state
        .logins()
        .log_in(user, &Passphrase::new(PASSPHRASE).unwrap())
        .await
        .unwrap();
    format!("ip_session={}", token.as_str())
}

/// A management request, carrying the header every cookie caller must send with one that
/// changes something.
pub fn request(method: &str, uri: &str, cookie: &str, body: Option<&str>) -> Request<Body> {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", cookie)
        .header(CSRF_HEADER, "1");
    match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

/// What a response carried.
pub async fn body_of(response: Response<Body>) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}
