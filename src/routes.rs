use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{Request, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{any, get};
use axum::{Json, Router};
use http_body_util::BodyExt;
use ip_auth::Identity;
use ip_core::{Protocol, Role};
use ip_gateway::{GatewayBody, GatewayError, RequestContext};
use std::net::SocketAddr;

use crate::auth::Authenticated;
use crate::state::AppState;

/// Every route the server serves.
///
/// The server's own endpoints sit under the reserved prefix so no routing rule can
/// claim them; every other path is proxied.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/_ip/healthz", get(healthz))
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
    use ip_gateway::{Gateway, ProcessorChain, RoutingTable, Upstream, UpstreamError};
    use ip_storage::{
        AccountKind, GrantStore, Membership, MembershipStore, NewTenant, NewUser, SqliteStore,
        Standing, Storage, TenantStore, UserStore,
    };
    use tower::ServiceExt;

    use super::*;

    const REMOTE: [u8; 4] = [203, 0, 113, 7];

    const CONFIG: &str = r#"
[server]
listen = "127.0.0.1:8080"

[storage]
backend = "sqlite"
path = "./test.db"

[auth.jwt]
issuer = "intelli-prism"
secret = "0123456789abcdef0123456789abcdef"
"#;

    /// Answers every request with a fixed body, recording the uri it was given.
    #[derive(Default)]
    struct FakeUpstream {
        seen: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Upstream for FakeUpstream {
        async fn send(
            &self,
            request: Request<ip_gateway::GatewayBody>,
        ) -> Result<Response<ip_gateway::GatewayBody>, UpstreamError> {
            self.seen.lock().unwrap().push(request.uri().to_string());
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
        let config = Config::parse(CONFIG).unwrap();
        let table = RoutingTable::build(&config, vec![rule()]).unwrap();
        let upstream = Arc::new(FakeUpstream::default());
        let gateway = Gateway::new(table, processors, upstream.clone());
        let listen: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let state = AppState::with_parts(Arc::new(store) as Arc<dyn Storage>, gateway, listen);
        (router(state), key, upstream)
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
