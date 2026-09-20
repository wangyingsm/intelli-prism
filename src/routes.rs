use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, Request, Response, StatusCode, header::SET_COOKIE};
use axum::response::IntoResponse;
use axum::routing::{any, get, post};
use axum::{Json, Router};
use http_body_util::BodyExt;
use ip_auth::{Identity, Passphrase};
use ip_core::{Protocol, Role};
use ip_gateway::{GatewayBody, GatewayError, RequestContext};
use std::net::SocketAddr;

use crate::auth::Authenticated;
use crate::cookie;
use crate::state::AppState;

/// Every route the server serves.
///
/// The server's own endpoints sit under the reserved prefix so no routing rule can
/// claim them; every other path is proxied.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/_ip/healthz", get(healthz))
        .route("/_ip/login", post(login))
        .route("/_ip/logout", post(logout))
        .route("/_ip/whoami", get(whoami))
        .route("/_ip/{*rest}", any(reserved))
        .fallback(any(proxy))
        .with_state(state)
}

/// Who the caller is, once its signature checked out.
#[derive(Debug, serde::Serialize)]
pub struct WhoAmI {
    /// The user that signed.
    pub user: String,
    /// The tenant it signed for.
    pub tenant: String,
    /// What it is within that tenant.
    pub role: &'static str,
}

async fn healthz() -> &'static str {
    "ok"
}

async fn whoami(Authenticated(authority): Authenticated) -> Json<WhoAmI> {
    Json(WhoAmI::from(authority.identity().clone()))
}

/// What a login is asked for.
#[derive(serde::Deserialize)]
pub struct Credentials {
    /// Who is logging in.
    user: String,
    /// What they typed.
    passphrase: String,
}

/// Opens a session for a caller that proves who it is, and hands back its cookie.
///
/// Every refusal is the same status: telling a wrong user from a wrong passphrase would
/// say which accounts exist.
async fn login(
    State(state): State<AppState>,
    Json(credentials): Json<Credentials>,
) -> Response<Body> {
    let opened = async {
        let user = ip_core::UserId::new(&credentials.user).ok()?;
        let passphrase = Passphrase::new(&credentials.passphrase).ok()?;
        state.logins().log_in(&user, &passphrase).await.ok()
    }
    .await;
    let Some(token) = opened else {
        tracing::warn!(user = %credentials.user, "refused a login");
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(SET_COOKIE, cookie::set(&token, state.session_seconds()));
    response
}

/// Ends the session the cookie carries, and clears the cookie.
///
/// The token cannot be taken back, so the session is remembered as ended until the token
/// would have run out anyway.
async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response<Body> {
    let Some(token) = cookie::token(&headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Ok(session) = state.logins().session(token).await else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if let Err(error) = state.logins().end(&session).await {
        tracing::error!(%error, "could not end a session");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(SET_COOKIE, cookie::clear());
    response
}

/// Anything else under the reserved prefix belongs to no endpoint and is never proxied.
async fn reserved() -> StatusCode {
    StatusCode::NOT_FOUND
}

/// Carries an authenticated request through the gateway dataflow.
async fn proxy(
    State(state): State<AppState>,
    Authenticated(authority): Authenticated,
    ConnectInfo(_peer): ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> Response<Body> {
    let context = RequestContext {
        authority,
        protocol: Protocol::Http,
        listen: state.listen(),
    };
    match state.gateway().handle(context, into_gateway(request)).await {
        Ok(response) => response.map(Body::new),
        Err(error) => refuse(error),
    }
}

fn into_gateway(request: Request<Body>) -> Request<GatewayBody> {
    request.map(|body| body.map_err(Into::into).boxed_unsync())
}

/// The caller is told the status, plus a plugin's own reason when it refused on purpose;
/// every other reason goes to the log.
fn refuse(error: GatewayError) -> Response<Body> {
    let status = error.status();
    if status.is_server_error() {
        tracing::error!(stage = %error.stage(), %error, "the gateway could not carry a request");
    } else {
        tracing::warn!(stage = %error.stage(), %error, "the gateway refused a request");
    }
    match error.public_reason() {
        Some(reason) => (status, reason.to_owned()).into_response(),
        None => (status, ()).into_response(),
    }
}

impl From<Identity> for WhoAmI {
    fn from(identity: Identity) -> Self {
        Self {
            user: identity.user.to_string(),
            tenant: identity.tenant.to_string(),
            role: role_name(&identity.role),
        }
    }
}

fn role_name(role: &Role) -> &'static str {
    match role {
        Role::SysAdmin => "sysadmin",
        Role::TenantAdmin(_) => "tenant_admin",
        Role::Member => "member",
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use bytes::Bytes;
    use http_body_util::BodyExt;
    use ip_config::Config;
    use ip_core::{
        AbsPath, ApiId, Capability, CapabilityScope, Endpoint, Grant, Host, Nonce, PassphraseHash,
        Port, Protocol, RouteKey, RouteRule, RouteTarget, Signature, TenantId, TnKey, UserId,
        UtKey,
    };
    use ip_core::{NewPluginRule, PluginKind, PluginOrder, PluginScope};
    use ip_gateway::{Gateway, ProcessorChain, RoutingTable, Upstream, UpstreamError};
    use ip_plugin::{PluginChains, PluginHost, PluginLimits};
    use ip_storage::{
        AccountKind, GrantStore, Membership, MembershipStore, NewTenant, NewUser, SqliteStore,
        Standing, Storage, TenantStore, UserStore,
    };
    use ip_storage::{NewPlugin, PluginRuleStore, PluginStore};
    use tower::ServiceExt;

    use super::*;

    const REMOTE: [u8; 4] = [203, 0, 113, 7];

    const CONFIG: &str = r#"
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
"#;

    /// Answers every request with a fixed body, recording the uri it was given.
    #[derive(Default)]
    struct FakeUpstream {
        seen: Mutex<Vec<String>>,
        headers: Mutex<Vec<axum::http::HeaderMap>>,
    }

    #[async_trait::async_trait]
    impl Upstream for FakeUpstream {
        async fn send(
            &self,
            request: Request<ip_gateway::GatewayBody>,
        ) -> Result<Response<ip_gateway::GatewayBody>, UpstreamError> {
            self.seen.lock().unwrap().push(request.uri().to_string());
            self.headers.lock().unwrap().push(request.headers().clone());
            Ok(Response::builder()
                .status(StatusCode::OK)
                .body(ip_gateway::body::from_bytes(Bytes::from_static(
                    b"from upstream",
                )))
                .unwrap())
        }
    }

    fn tenant_id() -> TenantId {
        TenantId::new("acme").unwrap()
    }

    fn user_id() -> UserId {
        UserId::new("alice").unwrap()
    }

    fn api_id() -> ApiId {
        ApiId::new("anthropic").unwrap()
    }

    fn nonce() -> Nonce {
        Nonce::new("0123456789abcdef").unwrap()
    }

    fn endpoint(host: &str, port: u16, path: &str) -> Endpoint {
        Endpoint::new(
            Protocol::Http,
            Host::new(host).unwrap(),
            Port::new(port).unwrap(),
            AbsPath::new(path).unwrap(),
        )
    }

    fn rule() -> RouteRule {
        RouteRule {
            api: api_id(),
            key: RouteKey::new(endpoint("gateway.local", 8080, "/anthropic")),
            target: RouteTarget::new(endpoint("upstream.local", 80, "/v1")),
        }
    }

    async fn fixture(granted: bool) -> (Router, TnKey, Arc<FakeUpstream>) {
        fixture_with(granted, ProcessorChain::new()).await
    }

    async fn fixture_with(
        granted: bool,
        processors: ProcessorChain,
    ) -> (Router, TnKey, Arc<FakeUpstream>) {
        let (store, key) = seeded_store(granted).await;
        let upstream = Arc::new(FakeUpstream::default());
        let gateway = Gateway::new(table(), processors, upstream.clone());
        (router(state_over(store, gateway)), key, upstream)
    }

    /// A store holding the tenant, the user and its membership, and the grant when asked for.
    async fn seeded_store(granted: bool) -> (SqliteStore, TnKey) {
        let store = SqliteStore::in_memory().await.unwrap();
        let key = TnKey::generate().unwrap();
        store
            .create_tenant(NewTenant {
                id: tenant_id(),
                key: key.clone(),
            })
            .await
            .unwrap();
        store
            .create_user(NewUser {
                id: user_id(),
                passphrase: PassphraseHash::new("$argon2id$v=19$m=8,t=1,p=1$c2FsdA$aGFzaA")
                    .unwrap(),
                kind: AccountKind::Regular,
            })
            .await
            .unwrap();
        store
            .attach(Membership {
                user: user_id(),
                tenant: tenant_id(),
                standing: Standing::Member,
            })
            .await
            .unwrap();
        if granted {
            store
                .grant(
                    &Grant::new(
                        Capability::ApiAccess,
                        CapabilityScope::Api {
                            user: user_id(),
                            tenant: tenant_id(),
                            api: api_id(),
                        },
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
        }
        (store, key)
    }

    fn table() -> RoutingTable {
        let config = Config::parse(CONFIG).unwrap();
        RoutingTable::build(&config, vec![rule()]).unwrap()
    }

    fn state_over(store: SqliteStore, gateway: Gateway) -> AppState {
        let listen: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        AppState::with_parts(Arc::new(store) as Arc<dyn Storage>, gateway, listen)
    }

    fn signature(key: &TnKey) -> String {
        Signature::of_user(&UtKey::derive(&user_id(), key), &nonce()).to_hex()
    }

    fn request(uri: &str, signature: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder()
            .uri(uri)
            .header("host", "gateway.local:8080");
        if let Some(signature) = signature {
            builder = builder
                .header("x-ip-tnid", tenant_id().as_str())
                .header("x-ip-userid", user_id().as_str())
                .header("x-ip-nonce", nonce().as_str())
                .header("x-ip-signature", signature);
        }
        let mut request = builder.body(Body::empty()).unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from((REMOTE, 40_000))));
        request
    }

    async fn body_of(response: Response<Body>) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// A router over a store whose user really has the passphrase the login tests type.
    async fn fixture_that_can_log_in() -> Router {
        let (store, _) = seeded_store(true).await;
        let hashed = ip_auth::PassphraseHasher::new()
            .hash(&Passphrase::new("correct horse staple").unwrap())
            .unwrap();
        store.set_passphrase(&user_id(), &hashed).await.unwrap();
        let gateway = Gateway::new(
            table(),
            ProcessorChain::new(),
            Arc::new(FakeUpstream::default()),
        );
        router(state_over(store, gateway))
    }

    fn login_request(user: &str, passphrase: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/_ip/login")
            .header("content-type", "application/json")
            .body(Body::from(format!(
                r#"{{"user":"{user}","passphrase":"{passphrase}"}}"#
            )))
            .unwrap()
    }

    fn logout_request(cookie: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().method("POST").uri("/_ip/logout");
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        builder.body(Body::empty()).unwrap()
    }

    /// The `ip_session=...` pair out of a response, ready to send back as a cookie.
    fn session_cookie(response: &Response<Body>) -> String {
        response
            .headers()
            .get(SET_COOKIE)
            .expect("a session cookie")
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    }

    #[tokio::test]
    async fn the_right_passphrase_opens_a_session() {
        let router = fixture_that_can_log_in().await;
        let response = router
            .oneshot(login_request("alice", "correct horse staple"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let cookie = response.headers()[SET_COOKIE].to_str().unwrap().to_owned();
        for attribute in [
            "ip_session=",
            "HttpOnly",
            "Secure",
            "SameSite=Strict",
            "Max-Age=3600",
        ] {
            assert!(
                cookie.contains(attribute),
                "{attribute} is missing from {cookie}"
            );
        }
    }

    #[tokio::test]
    async fn the_wrong_passphrase_opens_nothing() {
        let router = fixture_that_can_log_in().await;
        let response = router
            .oneshot(login_request("alice", "incorrect horse staple"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get(SET_COOKIE).is_none());
    }

    #[tokio::test]
    async fn a_user_that_does_not_exist_is_refused_the_same_way() {
        let router = fixture_that_can_log_in().await;
        let response = router
            .oneshot(login_request("nobody", "correct horse staple"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get(SET_COOKIE).is_none());
    }

    #[tokio::test]
    async fn a_login_that_is_not_even_credentials_is_refused() {
        let router = fixture_that_can_log_in().await;
        let malformed = Request::builder()
            .method("POST")
            .uri("/_ip/login")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let response = router.oneshot(malformed).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn a_logout_ends_the_session_and_clears_the_cookie() {
        let router = fixture_that_can_log_in().await;
        let opened = router
            .clone()
            .oneshot(login_request("alice", "correct horse staple"))
            .await
            .unwrap();
        let cookie = session_cookie(&opened);

        let ended = router
            .clone()
            .oneshot(logout_request(Some(&cookie)))
            .await
            .unwrap();
        assert_eq!(ended.status(), StatusCode::NO_CONTENT);
        assert!(
            ended.headers()[SET_COOKIE]
                .to_str()
                .unwrap()
                .contains("Max-Age=0")
        );

        // The token still parses, so only the ending keeps it from being spent again.
        let again = router.oneshot(logout_request(Some(&cookie))).await.unwrap();
        assert_eq!(again.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_logout_without_a_session_is_refused() {
        let router = fixture_that_can_log_in().await;
        assert_eq!(
            router
                .clone()
                .oneshot(logout_request(None))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let forged = router
            .oneshot(logout_request(Some("ip_session=a.b.c")))
            .await
            .unwrap();
        assert_eq!(forged.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn health_needs_no_credentials() {
        let (router, _, _) = fixture(true).await;
        let response = router.oneshot(request("/_ip/healthz", None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await, "ok");
    }

    #[tokio::test]
    async fn an_unsigned_request_is_refused() {
        let (router, _, _) = fixture(true).await;
        let response = router.oneshot(request("/_ip/whoami", None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(body_of(response).await.is_empty());
    }

    #[tokio::test]
    async fn a_signed_request_names_its_caller() {
        let (router, key, _) = fixture(true).await;
        let response = router
            .oneshot(request("/_ip/whoami", Some(&signature(&key))))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_of(response).await).unwrap();
        assert_eq!(body["user"], "alice");
        assert_eq!(body["tenant"], "acme");
        assert_eq!(body["role"], "member");
    }

    #[tokio::test]
    async fn a_request_signed_with_another_key_is_refused() {
        let (router, _, upstream) = fixture(true).await;
        let stolen = TnKey::generate().unwrap();
        let response = router
            .oneshot(request("/anthropic/messages", Some(&signature(&stolen))))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(upstream.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_signed_and_granted_request_is_proxied() {
        let (router, key, upstream) = fixture(true).await;
        let response = router
            .oneshot(request("/anthropic/messages", Some(&signature(&key))))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await, "from upstream");
        assert_eq!(
            upstream.seen.lock().unwrap().as_slice(),
            ["http://upstream.local:80/v1/messages"]
        );
    }

    #[tokio::test]
    async fn the_signing_headers_never_leave_the_gateway() {
        let (router, key, upstream) = fixture(true).await;
        let mut signed = request("/anthropic/messages", Some(&signature(&key)));
        signed
            .headers_mut()
            .insert("x-app-trace", axum::http::HeaderValue::from_static("kept"));
        let response = router.oneshot(signed).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let sent = upstream.headers.lock().unwrap();
        for name in ["x-ip-tnid", "x-ip-userid", "x-ip-signature", "x-ip-nonce"] {
            assert!(sent[0].get(name).is_none(), "{name} reached the upstream");
        }
        assert_eq!(sent[0].get("x-app-trace").unwrap(), "kept");
    }

    #[tokio::test]
    async fn an_unsigned_proxied_request_never_reaches_the_upstream() {
        let (router, _, upstream) = fixture(true).await;
        let response = router
            .oneshot(request("/anthropic/messages", None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(upstream.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_caller_holding_no_api_access_never_reaches_the_upstream() {
        let (router, key, upstream) = fixture(false).await;
        let response = router
            .oneshot(request("/anthropic/messages", Some(&signature(&key))))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(upstream.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_path_no_rule_carries_is_not_found() {
        let (router, key, _) = fixture(true).await;
        let response = router
            .oneshot(request("/elsewhere", Some(&signature(&key))))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// Stops every request with the error it was built with.
    struct Stop(ip_gateway::ProcessorError);

    #[async_trait::async_trait]
    impl ip_gateway::HeaderProcessor for Stop {
        fn order(&self) -> ip_core::PluginOrder {
            ip_core::PluginOrder::new(100)
        }

        async fn process(
            &self,
            _: &mut axum::http::HeaderMap,
        ) -> Result<(), ip_gateway::ProcessorError> {
            Err(self.0.clone())
        }
    }

    #[tokio::test]
    async fn a_plugin_refusal_reaches_the_caller_with_its_reason() {
        let refusing = ProcessorChain::new().with_request_header(Arc::new(Stop(
            ip_gateway::ProcessorError::refused("blocked by acme policy"),
        )));
        let (router, key, upstream) = fixture_with(true, refusing).await;
        let response = router
            .oneshot(request("/anthropic/messages", Some(&signature(&key))))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(body_of(response).await, "blocked by acme policy");
        assert!(upstream.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_plugin_failure_tells_the_caller_nothing() {
        let failing = ProcessorChain::new().with_request_header(Arc::new(Stop(
            ip_gateway::ProcessorError::failed("wasm trapped at offset 0x2a"),
        )));
        let (router, key, _) = fixture_with(true, failing).await;
        let response = router
            .oneshot(request("/anthropic/messages", Some(&signature(&key))))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body_of(response).await.is_empty());
    }

    /// Keeps to the plugin abi and answers every body with the same words.
    const REWRITE: &str = r#"(module
  (memory (export "memory") 1)
  (data (i32.const 16) "rewritten by wasm")
  (func (export "alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "dealloc") (param i32 i32))
  (func (export "transform") (param i32 i32) (result i64)
    (i64.or (i64.shl (i64.const 16) (i64.const 32)) (i64.const 17))))"#;

    #[tokio::test]
    async fn a_stored_wasm_plugin_rewrites_a_proxied_response() {
        let (store, key) = seeded_store(true).await;
        let plugin = store
            .put_plugin(NewPlugin {
                kind: PluginKind::RespBody,
                wasm: wat::parse_str(REWRITE).unwrap(),
            })
            .await
            .unwrap();
        store
            .put_rule(
                NewPluginRule::new(
                    plugin.checksum,
                    PluginOrder::new(100),
                    PluginScope::Tenant {
                        tenant: tenant_id(),
                        user: None,
                        api: None,
                    },
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let host = Arc::new(PluginHost::new(PluginLimits::default()).unwrap());
        let chains = PluginChains::load(host, &store, &store).await.unwrap();
        let upstream = Arc::new(FakeUpstream::default());
        let gateway = Gateway::with_chains(table(), Arc::new(chains), upstream.clone());
        let response = router(state_over(store, gateway))
            .oneshot(request("/anthropic/messages", Some(&signature(&key))))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await, "rewritten by wasm");
        assert_eq!(upstream.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_reserved_prefix_is_never_proxied() {
        let (router, key, upstream) = fixture(true).await;
        let response = router
            .oneshot(request("/_ip/anything", Some(&signature(&key))))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(upstream.seen.lock().unwrap().is_empty());
    }
}
