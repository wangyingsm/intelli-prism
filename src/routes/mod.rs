mod grant;
#[cfg(test)]
pub(crate) mod harness;
mod key;
mod member;
mod plugin;
mod route;
mod tenant;
mod usage;
mod user;

use axum::body::Body;
use axum::extract::{ConnectInfo, Extension, FromRequestParts, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Request, Response, StatusCode, header::SET_COOKIE};
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::routing::{any, get, post};
use axum::{Json, Router};
use http_body_util::BodyExt;
use ip_auth::{Identity, Passphrase};
use ip_core::{Capability, CapabilityScope, Protocol, Role, Timestamp, TraceId, TurnId};
use ip_gateway::{GatewayBody, GatewayError, RequestContext};
use ip_storage::{DEFAULT_PAGE_LIMIT, Page, Standing, StorageError};
use std::net::SocketAddr;
use tracing::Instrument;

use crate::auth::Authenticated;
use crate::cookie;
use crate::manage::{self, Manager};
use crate::state::AppState;
use crate::telemetry;

/// Every route the server serves.
///
/// The server's own endpoints sit under the reserved prefix so no routing rule can
/// claim them; every other path is proxied.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/_ip/healthz", get(healthz))
        .route("/_ip/login", post(login))
        .route("/_ip/logout", post(logout))
        .route("/_ip/session", get(session))
        .nest("/_ip/tenants", tenant::router())
        .nest("/_ip/tenants/{tenant}/members", member::router())
        .nest(
            "/_ip/tenants/{tenant}/members/{user}/grants",
            grant::member_router(),
        )
        .nest("/_ip/users", user::router())
        .nest("/_ip/users/{user}/grants", grant::account_router())
        .nest("/_ip/routes", route::router())
        .nest("/_ip/keys", key::router())
        .nest("/_ip/plugins", plugin::store_router())
        .nest("/_ip/plugin-rules", plugin::rules_router())
        .nest("/_ip/usage", usage::router())
        .route("/_ip/whoami", get(whoami))
        .route("/_ip/{*rest}", any(reserved))
        .fallback(any(proxy))
        .with_state(state)
        .layer(middleware::from_fn(traced))
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
    if !headers.contains_key(manage::CSRF_HEADER) {
        return StatusCode::FORBIDDEN.into_response();
    }
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

/// Who the caller is, and what it may do where.
#[derive(Debug, serde::Serialize)]
pub struct Whose {
    /// The account calling.
    pub user: String,
    /// Whether it holds every capability everywhere.
    pub system_administrator: bool,
    /// The tenants it is attached to, and what it may do inside each.
    pub tenants: Vec<Reach>,
}

/// One tenant the caller reaches, and what it holds there.
#[derive(Debug, serde::Serialize)]
pub struct Reach {
    /// Which tenant.
    pub tenant: String,
    /// What the caller is inside it.
    pub standing: &'static str,
    /// The capabilities it holds there, which is what a page draws its menu from.
    pub capabilities: Vec<&'static str>,
}

/// What a caller may hold inside one tenant, as opposed to against one api.
const TENANT_CAPABILITIES: [Capability; 3] = [
    Capability::UserMgr,
    Capability::SysAgent,
    Capability::Observer,
];

/// Every capability, so a name can be read back into one.
const CAPABILITIES: [Capability; 7] = [
    Capability::TenantMgr,
    Capability::UserMgr,
    Capability::ApiAccess,
    Capability::ApiAdvMgr,
    Capability::LimitMgr,
    Capability::SysAgent,
    Capability::Observer,
];

/// The name the api spells a capability with.
fn capability_name(capability: Capability) -> &'static str {
    match capability {
        Capability::TenantMgr => "tenant_mgr",
        Capability::UserMgr => "user_mgr",
        Capability::ApiAccess => "api_access",
        Capability::ApiAdvMgr => "api_adv_mgr",
        Capability::LimitMgr => "limit_mgr",
        Capability::SysAgent => "sys_agent",
        Capability::Observer => "observer",
    }
}

/// The capability a name spells, if it spells one.
fn capability_named(name: &str) -> Option<Capability> {
    CAPABILITIES
        .into_iter()
        .find(|capability| capability_name(*capability) == name)
}

/// Who the session belongs to, which is what a page asks for once it has logged in.
async fn session(State(state): State<AppState>, manager: Manager) -> Response<Body> {
    let store = state.store().as_ref();
    let attached = match store.memberships_of_user(manager.user()).await {
        Ok(attached) => attached,
        Err(error) => {
            tracing::error!(%error, "could not read what the caller is attached to");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut tenants = Vec::with_capacity(attached.len());
    for membership in attached {
        let mut capabilities = Vec::new();
        for capability in TENANT_CAPABILITIES {
            let scope = CapabilityScope::Tenant {
                user: manager.user().clone(),
                tenant: membership.tenant.clone(),
            };
            match manager.allows(store, capability, &scope).await {
                Ok(true) => capabilities.push(capability_name(capability)),
                Ok(false) => {}
                Err(error) => {
                    tracing::error!(%error, "could not read what the caller holds");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            }
        }
        tenants.push(Reach {
            tenant: membership.tenant.to_string(),
            standing: standing_name(membership.standing),
            capabilities,
        });
    }

    Json(Whose {
        user: manager.user().to_string(),
        system_administrator: manager.is_system_administrator(),
        tenants,
    })
    .into_response()
}

/// The name a standing is shown under.
fn standing_name(standing: Standing) -> &'static str {
    match standing {
        Standing::Owner => "owner",
        Standing::Member => "member",
    }
}

/// What a caller is told when the store refused a management request. Only the reasons it
/// can act on carry one; everything else is the server's own problem and goes to the log.
fn store_refusal(error: StorageError) -> Response<Body> {
    match error {
        StorageError::Conflict { .. } | StorageError::InUse { .. } => {
            (StatusCode::CONFLICT, error.to_string()).into_response()
        }
        StorageError::NotFound { .. } => StatusCode::NOT_FOUND.into_response(),
        error => {
            tracing::error!(%error, "the store could not carry a management request");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Which page of a list a request asks for. Every part may be left out.
#[derive(Debug, Default, serde::Deserialize)]
pub struct PageQuery {
    /// How many records at most; 20 when left out, and never more than 100.
    limit: Option<u32>,
    /// How many to skip first; none when left out.
    offset: Option<u32>,
    /// Only records created after this moment, in unix seconds; every record when left out.
    after: Option<i64>,
}

/// The page of a list a request asks for, newest first.
#[derive(Debug, Clone, Copy)]
pub struct Paged(pub Page);

impl<S: Send + Sync> FromRequestParts<S> for Paged {
    type Rejection = Response<Body>;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Query(query) = Query::<PageQuery>::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        let after = match query.after {
            Some(seconds) => Some(Timestamp::from_unix_seconds(seconds).map_err(|error| {
                (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response()
            })?),
            None => None,
        };
        Ok(Self(Page::new(
            query.limit.unwrap_or(DEFAULT_PAGE_LIMIT),
            query.offset.unwrap_or(0),
            after,
        )))
    }
}

/// Anything else under the reserved prefix belongs to no endpoint and is never proxied.
async fn reserved() -> StatusCode {
    StatusCode::NOT_FOUND
}

/// The header a caller marks a chat turn with, which the gateway groups a trace by.
const HEADER_TURN: &str = "x-ip-turn";

/// The header every answer carries the trace it was followed under in.
const HEADER_TRACE: &str = "x-ip-trace";

/// Follows every request under an id of this gateway's own, and says which in the answer.
///
/// It sits outside every endpoint, so a request refused before it reaches one is followed
/// too: an answer nobody can name is an answer nobody can ask about.
async fn traced(mut request: Request<Body>, next: Next) -> Response<Body> {
    let Ok(trace) = TraceId::generate() else {
        tracing::error!("could not draw a trace id");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    request.extensions_mut().insert(trace);
    // The span is opened and closed inside the scope, which is where the exporter reads the id
    // to name its trace by.
    let mut answered = telemetry::under(trace, async move {
        let serving = tracing::info_span!(
            "request",
            trace = %trace,
            method = %request.method(),
            path = request.uri().path(),
        );
        next.run(request).instrument(serving).await
    })
    .await;
    // The gateway never continues a trace it was sent, so this is how a caller correlates.
    if let Ok(value) = HeaderValue::from_str(&trace.to_hex()) {
        answered.headers_mut().insert(HEADER_TRACE, value);
    }
    answered
}

/// Carries an authenticated request through the gateway dataflow.
async fn proxy(
    State(state): State<AppState>,
    Authenticated(authority): Authenticated,
    ConnectInfo(_peer): ConnectInfo<SocketAddr>,
    Extension(trace): Extension<TraceId>,
    mut request: Request<Body>,
) -> Response<Body> {
    let context = RequestContext {
        authority,
        protocol: Protocol::Http,
        listen: state.listen(),
        trace,
        turn: turn_of(&mut request),
    };
    match state.gateway().handle(context, into_gateway(request)).await {
        Ok(response) => response.map(Body::new),
        Err(error) => refuse(error),
    }
}

/// The turn the caller marked, with the header taken off so no upstream sees our own.
///
/// A mark that is not a turn id is dropped rather than refused: it groups telemetry, and no
/// request is worth failing over how its trace is filed.
fn turn_of(request: &mut Request<Body>) -> Option<TurnId> {
    let marked = request.headers_mut().remove(HEADER_TURN)?;
    let turn = marked.to_str().ok().and_then(|raw| TurnId::new(raw).ok());
    if turn.is_none() {
        tracing::warn!("a request marked a turn that is not a turn id");
    }
    turn
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
        Standing, TenantStore, UserStore,
    };
    use ip_storage::{NewPlugin, PluginOwner, PluginRuleStore, PluginStore};
    use tower::ServiceExt;

    use super::*;
    use crate::state::Stores;

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
        AppState::with_parts(Stores::Sqlite(Arc::new(store)), gateway, listen)
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
        logout_request_with(cookie, true)
    }

    fn logout_request_with(cookie: Option<&str>, csrf: bool) -> Request<Body> {
        let mut builder = Request::builder().method("POST").uri("/_ip/logout");
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        if csrf {
            builder = builder.header(manage::CSRF_HEADER, "1");
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

    #[test]
    fn every_capability_reads_back_from_its_own_name() {
        for capability in CAPABILITIES {
            assert_eq!(
                capability_named(capability_name(capability)),
                Some(capability)
            );
        }
        assert_eq!(capability_named("root"), None);
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
    async fn a_session_says_who_it_belongs_to() {
        let router = fixture_that_can_log_in().await;
        let opened = router
            .clone()
            .oneshot(login_request("alice", "correct horse staple"))
            .await
            .unwrap();
        let cookie = session_cookie(&opened);
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/_ip/session")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_of(response).await;
        assert!(body.contains(r#""user":"alice""#), "{body}");
        assert!(body.contains(r#""system_administrator":false"#), "{body}");
        assert!(body.contains(r#""tenant":"acme""#), "{body}");
        assert!(body.contains(r#""standing":"member""#), "{body}");
        assert!(body.contains(r#""capabilities":[]"#), "{body}");
    }

    #[tokio::test]
    async fn a_session_nobody_opened_says_nothing() {
        let router = fixture_that_can_log_in().await;
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/_ip/session")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_logout_another_site_started_is_refused() {
        let router = fixture_that_can_log_in().await;
        let opened = router
            .clone()
            .oneshot(login_request("alice", "correct horse staple"))
            .await
            .unwrap();
        let cookie = session_cookie(&opened);
        // A page elsewhere can make the browser send the cookie, but not the header.
        let forged = router
            .clone()
            .oneshot(logout_request_with(Some(&cookie), false))
            .await
            .unwrap();
        assert_eq!(forged.status(), StatusCode::FORBIDDEN);
        // And the session it tried to end still works.
        let ended = router.oneshot(logout_request(Some(&cookie))).await.unwrap();
        assert_eq!(ended.status(), StatusCode::NO_CONTENT);
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
                owner: PluginOwner::Tenant(tenant_id()),
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

    #[tokio::test]
    async fn every_answer_carries_the_trace_it_was_followed_under() {
        let (router, key, _) = fixture(true).await;
        let carried = router
            .clone()
            .oneshot(request("/anthropic/messages", Some(&signature(&key))))
            .await
            .unwrap();
        assert_eq!(carried.status(), StatusCode::OK);
        let trace = carried.headers()[HEADER_TRACE].to_str().unwrap().to_owned();
        assert!(
            TraceId::from_hex(&trace).is_ok(),
            "{trace} is not a trace id"
        );

        // A refusal is followed the same way, which is what a caller asks about.
        let refused = router
            .oneshot(request("/nowhere", Some(&signature(&key))))
            .await
            .unwrap();
        assert_ne!(refused.status(), StatusCode::OK);
        let other = refused.headers()[HEADER_TRACE].to_str().unwrap();
        assert!(TraceId::from_hex(other).is_ok());
        assert_ne!(other, trace, "two requests shared one trace");
    }

    #[test]
    fn every_span_exported_for_a_request_is_followed_by_its_trace() {
        use ip_config::SampleRatio;
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::trace::InMemorySpanExporter;
        use tracing_subscriber::Registry;
        use tracing_subscriber::layer::SubscriberExt;

        let exporter = InMemorySpanExporter::default();
        let provider = crate::telemetry::provider(exporter.clone(), SampleRatio::default());
        let subscriber = Registry::default().with(
            tracing_opentelemetry::layer().with_tracer(provider.tracer(crate::telemetry::SERVICE)),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let trace = tracing::subscriber::with_default(subscriber, || {
            runtime.block_on(async {
                let (router, key, _) = fixture(true).await;
                let answered = router
                    .oneshot(request("/anthropic/messages", Some(&signature(&key))))
                    .await
                    .unwrap();
                assert_eq!(answered.status(), StatusCode::OK);
                answered.headers()[HEADER_TRACE]
                    .to_str()
                    .unwrap()
                    .to_owned()
            })
        });
        provider.force_flush().unwrap();

        let stages = [
            "request",
            "ingress.request",
            "egress.request",
            "ingress.response",
            "egress.response",
        ];
        let exported = exporter.get_finished_spans().unwrap();
        for stage in stages {
            let span = exported
                .iter()
                .find(|span| span.name == stage)
                .unwrap_or_else(|| panic!("{stage} was not exported"));
            assert_eq!(
                span.span_context.trace_id().to_string(),
                trace,
                "{stage} was exported under another trace"
            );
        }
    }

    #[tokio::test]
    async fn a_request_the_server_answers_is_recorded_in_the_store() {
        use ip_storage::{UsageRowId, UsageStore};

        let (store, key) = seeded_store(true).await;
        let store = Arc::new(store);
        let gateway = Gateway::new(
            table(),
            ProcessorChain::new(),
            Arc::new(FakeUpstream::default()),
        )
        .recording(Arc::clone(&store) as Arc<dyn UsageStore>);
        let listen: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let state = AppState::with_parts(Stores::Sqlite(Arc::clone(&store)), gateway, listen);
        let router = router(state);

        let answered = router
            .oneshot(request("/anthropic/messages", Some(&signature(&key))))
            .await
            .unwrap();
        assert_eq!(answered.status(), StatusCode::OK);
        let trace = answered.headers()[HEADER_TRACE]
            .to_str()
            .unwrap()
            .to_owned();
        body_of(answered).await;

        // The row is written off the request path, so it lands a moment after the answer does.
        let mut recorded = None;
        for _ in 0..100 {
            recorded = store.usage(UsageRowId::new(1)).await.unwrap();
            if recorded.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let recorded = recorded.expect("nothing was recorded");
        assert_eq!(recorded.trace.to_hex(), trace);
        assert_eq!(recorded.tenant, tenant_id());
        assert_eq!(recorded.user, user_id());
    }

    #[tokio::test]
    async fn an_endpoint_of_the_gateway_s_own_is_followed_too() {
        let (router, ..) = fixture(true).await;
        let answered = router
            .oneshot(
                Request::builder()
                    .uri("/_ip/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(answered.status(), StatusCode::OK);
        assert!(TraceId::from_hex(answered.headers()[HEADER_TRACE].to_str().unwrap()).is_ok());
    }

    #[tokio::test]
    async fn the_turn_a_caller_marks_never_reaches_the_upstream() {
        let (router, key, upstream) = fixture(true).await;
        let mut marked = request("/anthropic/messages", Some(&signature(&key)));
        marked
            .headers_mut()
            .insert(HEADER_TURN, HeaderValue::from_static("turn-42"));

        let answered = router.oneshot(marked).await.unwrap();
        assert_eq!(answered.status(), StatusCode::OK);
        let sent = upstream.headers.lock().unwrap();
        assert!(
            sent[0].get(HEADER_TURN).is_none(),
            "the turn the caller marked reached the upstream"
        );
    }

    #[test]
    fn a_mark_that_is_not_a_turn_id_is_dropped_with_the_header() {
        let marked = |value: &'static str| {
            let mut request = Request::builder().body(Body::empty()).unwrap();
            request
                .headers_mut()
                .insert(HEADER_TURN, HeaderValue::from_static(value));
            request
        };
        let mut good = marked("turn-42");
        assert_eq!(turn_of(&mut good), Some(TurnId::new("turn-42").unwrap()));
        assert!(good.headers().get(HEADER_TURN).is_none());

        let mut bad = marked("not a turn id");
        assert_eq!(turn_of(&mut bad), None);
        assert!(bad.headers().get(HEADER_TURN).is_none());

        let mut unmarked = Request::builder().body(Body::empty()).unwrap();
        assert_eq!(turn_of(&mut unmarked), None);
    }
}
