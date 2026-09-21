use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{Method, StatusCode};
use ip_auth::role_of;
use ip_core::{Capability, CapabilityScope, Role, UserId};
use ip_storage::{AccountKind, Storage, StorageError};

use crate::auth::{consume_signing_headers, origin, signed_request};
use crate::cookie;
use crate::state::AppState;

/// The header a browser must send with anything that changes something.
///
/// A page on another site can make a browser send its cookies, but it cannot make it send
/// a header of its own without asking this server first, which it never does. Its value is
/// not read: sending it at all is the proof.
pub const CSRF_HEADER: &str = "x-ip-csrf";

/// How a caller of the management api proved who it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proof {
    /// A session cookie, which a browser sends by itself.
    Session,
    /// A signed request, which nothing sends by itself.
    Signature,
}

/// A caller of the management api, and what it may do.
#[derive(Debug, Clone)]
pub struct Manager {
    user: UserId,
    kind: AccountKind,
    proof: Proof,
}

impl Manager {
    /// Who is calling.
    pub fn user(&self) -> &UserId {
        &self.user
    }

    /// Whether this is the system administrator, which holds every capability.
    pub fn is_system_administrator(&self) -> bool {
        self.kind == AccountKind::SystemAdministrator
    }

    /// Whether this caller may do `capability` at `scope`, by role or by explicit grant.
    ///
    /// The tenant comes from the scope the endpoint names, not from the session, so a
    /// caller reaches exactly the tenant whose url it asked for.
    pub async fn allows(
        &self,
        store: &dyn Storage,
        capability: Capability,
        scope: &CapabilityScope,
    ) -> Result<bool, StorageError> {
        if self.is_system_administrator() {
            return Ok(true);
        }
        let role = match scope.tenant() {
            Some(tenant) => match store.membership(&self.user, tenant).await? {
                Some(membership) => role_of(self.kind, membership.standing, tenant.clone()),
                // Someone outside a tenant holds nothing inside it, whatever they were granted.
                None => return Ok(false),
            },
            None => Role::Member,
        };
        let granted = store.grants_of(&self.user).await?;
        Ok(role.implies(capability, scope) || granted.holds(capability, scope))
    }
}

impl FromRequestParts<AppState> for Manager {
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if let Some(signed) = signed_request(parts) {
            consume_signing_headers(parts);
            let origin = origin(parts);
            let identity = state
                .verifier()
                .verify(&signed, origin)
                .await
                .map_err(|error| {
                    tracing::warn!(%origin, %error, "refused a signed management request");
                    StatusCode::UNAUTHORIZED
                })?;
            return Self::of(state, identity.user, Proof::Signature).await;
        }

        let Some(token) = cookie::token(&parts.headers) else {
            return Err(StatusCode::UNAUTHORIZED);
        };
        let session = state.logins().session(token).await.map_err(|error| {
            tracing::warn!(%error, "refused a session");
            StatusCode::UNAUTHORIZED
        })?;
        let manager = Self::of(state, session.user().clone(), Proof::Session).await?;
        manager.guard_csrf(&parts.method, parts)?;
        Ok(manager)
    }
}

impl Manager {
    /// Reads the account behind an established identity.
    async fn of(state: &AppState, user: UserId, proof: Proof) -> Result<Self, StatusCode> {
        let stored = state
            .store()
            .user(&user)
            .await
            .map_err(|error| {
                tracing::error!(%error, "could not read the caller");
                StatusCode::INTERNAL_SERVER_ERROR
            })?
            .ok_or(StatusCode::UNAUTHORIZED)?;
        Ok(Self {
            user: stored.id,
            kind: stored.kind,
            proof,
        })
    }
}

impl Manager {
    /// Refuses a cookie authenticated write that no page of this server asked for.
    ///
    /// A signed request needs no such header: nothing sends one by itself, so no other site
    /// can make a browser produce it.
    fn guard_csrf(&self, method: &Method, parts: &Parts) -> Result<(), StatusCode> {
        if self.proof == Proof::Session
            && changes_something(method)
            && !parts.headers.contains_key(CSRF_HEADER)
        {
            tracing::warn!(%method, "refused a cookie request carrying no {CSRF_HEADER}");
            return Err(StatusCode::FORBIDDEN);
        }
        Ok(())
    }
}

/// Whether a method changes something, and so needs more than a cookie behind it.
fn changes_something(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::extract::State;
    use axum::http::Request;
    use axum::routing::get;
    use ip_auth::{Passphrase, PassphraseHasher};
    use ip_core::{Capability, Grant, Nonce, Signature, TenantId, TnKey, UtKey};
    use ip_gateway::UpstreamError;
    use ip_gateway::body::GatewayBody;
    use ip_gateway::upstream::Upstream;
    use ip_gateway::{Gateway, ProcessorChain, RoutingTable};
    use ip_storage::{
        Membership, MembershipStore, NewTenant, NewUser, SqliteStore, Standing, TenantStore,
        UserStore,
    };
    use tower::ServiceExt;

    use super::*;
    use crate::state::Stores;

    const PASSPHRASE: &str = "correct horse staple";

    fn tenant_id() -> TenantId {
        TenantId::new("acme").unwrap()
    }

    fn user_id() -> UserId {
        UserId::new("alice").unwrap()
    }

    /// Answers with who the caller is, so a test can see what the extractor made of it.
    async fn who(manager: Manager) -> String {
        format!("{}:{:?}", manager.user(), manager.proof)
    }

    /// Answers whether the caller may manage users in the one tenant these tests use.
    async fn may(State(state): State<AppState>, manager: Manager) -> String {
        let scope = CapabilityScope::Tenant {
            user: manager.user().clone(),
            tenant: tenant_id(),
        };
        manager
            .allows(state.store().as_ref(), Capability::UserMgr, &scope)
            .await
            .unwrap()
            .to_string()
    }

    /// A routing table with no rule in it, built the way the server builds one.
    fn empty_table() -> RoutingTable {
        let config = ip_config::Config::parse(
            r#"
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
"#,
        )
        .unwrap();
        RoutingTable::build(&config, Vec::new()).unwrap()
    }

    /// An upstream nothing in these tests reaches.
    struct Unreachable;

    #[async_trait::async_trait]
    impl Upstream for Unreachable {
        async fn send(
            &self,
            _request: axum::http::Request<GatewayBody>,
        ) -> Result<axum::http::Response<GatewayBody>, UpstreamError> {
            Err(UpstreamError::new("nothing is sent upstream here"))
        }
    }

    /// A router and the state behind it, over a store holding one tenant and one user.
    async fn fixture(kind: AccountKind, standing: Standing) -> (Router, AppState, TnKey) {
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
                passphrase: PassphraseHasher::new()
                    .hash(&Passphrase::new(PASSPHRASE).unwrap())
                    .unwrap(),
                kind,
            })
            .await
            .unwrap();
        store
            .attach(Membership {
                user: user_id(),
                tenant: tenant_id(),
                standing,
            })
            .await
            .unwrap();

        let listen: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let gateway = Gateway::new(empty_table(), ProcessorChain::new(), Arc::new(Unreachable));
        let state = AppState::with_parts(Stores::Sqlite(Arc::new(store)), gateway, listen);
        let router = Router::new()
            .route("/who", get(who).post(who))
            .route("/may", get(may))
            .with_state(state.clone());
        (router, state, key)
    }

    /// The cookie a real login hands back.
    async fn cookie_of(state: &AppState) -> String {
        let token = state
            .logins()
            .log_in(&user_id(), &Passphrase::new(PASSPHRASE).unwrap())
            .await
            .unwrap();
        format!("ip_session={}", token.as_str())
    }

    fn request(method: &str, uri: &str, cookie: Option<&str>, csrf: bool) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        if csrf {
            builder = builder.header(CSRF_HEADER, "1");
        }
        builder.body(Body::empty()).unwrap()
    }

    /// A request signed the way an api caller signs one.
    fn signed(key: &TnKey) -> Request<Body> {
        let nonce = Nonce::new("0123456789abcdef").unwrap();
        let signature = Signature::of_user(&UtKey::derive(&user_id(), key), &nonce);
        Request::builder()
            .uri("/who")
            .header("x-ip-tnid", tenant_id().as_str())
            .header("x-ip-userid", user_id().as_str())
            .header("x-ip-nonce", nonce.as_str())
            .header("x-ip-signature", signature.to_hex())
            .body(Body::empty())
            .unwrap()
    }

    async fn body_of(response: axum::http::Response<Body>) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn a_request_with_nothing_behind_it_is_refused() {
        let (router, ..) = fixture(AccountKind::Regular, Standing::Member).await;
        let response = router
            .oneshot(request("GET", "/who", None, false))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_cookie_nobody_issued_is_refused() {
        let (router, ..) = fixture(AccountKind::Regular, Standing::Member).await;
        let response = router
            .oneshot(request("GET", "/who", Some("ip_session=a.b.c"), false))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_session_cookie_names_its_user() {
        let (router, state, _) = fixture(AccountKind::Regular, Standing::Member).await;
        let cookie = cookie_of(&state).await;
        let response = router
            .oneshot(request("GET", "/who", Some(&cookie), false))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await, "alice:Session");
    }

    #[tokio::test]
    async fn a_session_that_was_ended_is_refused() {
        let (router, state, _) = fixture(AccountKind::Regular, Standing::Member).await;
        let cookie = cookie_of(&state).await;
        let token = cookie.trim_start_matches("ip_session=").to_owned();
        let session = state.logins().session(&token).await.unwrap();
        state.logins().end(&session).await.unwrap();
        let response = router
            .oneshot(request("GET", "/who", Some(&cookie), false))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_cookie_alone_may_not_change_anything() {
        let (router, state, _) = fixture(AccountKind::Regular, Standing::Member).await;
        let cookie = cookie_of(&state).await;
        let refused = router
            .clone()
            .oneshot(request("POST", "/who", Some(&cookie), false))
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);

        let allowed = router
            .oneshot(request("POST", "/who", Some(&cookie), true))
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_signed_request_needs_no_header_of_its_own() {
        let (router, _, key) = fixture(AccountKind::Regular, Standing::Member).await;
        let response = router.oneshot(signed(&key)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await, "alice:Signature");
    }

    #[tokio::test]
    async fn the_system_administrator_may_do_anything() {
        let (router, state, _) = fixture(AccountKind::SystemAdministrator, Standing::Member).await;
        let cookie = cookie_of(&state).await;
        let response = router
            .oneshot(request("GET", "/may", Some(&cookie), false))
            .await
            .unwrap();
        assert_eq!(body_of(response).await, "true");
    }

    #[tokio::test]
    async fn an_owner_manages_users_in_its_own_tenant() {
        let (router, state, _) = fixture(AccountKind::Regular, Standing::Owner).await;
        let cookie = cookie_of(&state).await;
        let response = router
            .oneshot(request("GET", "/may", Some(&cookie), false))
            .await
            .unwrap();
        assert_eq!(body_of(response).await, "true");
    }

    #[tokio::test]
    async fn an_ordinary_member_manages_nobody() {
        let (router, state, _) = fixture(AccountKind::Regular, Standing::Member).await;
        let cookie = cookie_of(&state).await;
        let response = router
            .oneshot(request("GET", "/may", Some(&cookie), false))
            .await
            .unwrap();
        assert_eq!(body_of(response).await, "false");
    }

    #[tokio::test]
    async fn a_member_granted_the_capability_holds_it() {
        let (router, state, _) = fixture(AccountKind::Regular, Standing::Member).await;
        let scope = CapabilityScope::Tenant {
            user: user_id(),
            tenant: tenant_id(),
        };
        state
            .store()
            .grant(&Grant::new(Capability::UserMgr, scope).unwrap())
            .await
            .unwrap();
        let cookie = cookie_of(&state).await;
        let response = router
            .oneshot(request("GET", "/may", Some(&cookie), false))
            .await
            .unwrap();
        assert_eq!(body_of(response).await, "true");
    }

    #[tokio::test]
    async fn someone_outside_a_tenant_holds_nothing_inside_it() {
        let (_, state, _) = fixture(AccountKind::Regular, Standing::Member).await;
        let outsider = UserId::new("bob").unwrap();
        state
            .store()
            .create_user(NewUser {
                id: outsider.clone(),
                passphrase: PassphraseHasher::new()
                    .hash(&Passphrase::new(PASSPHRASE).unwrap())
                    .unwrap(),
                kind: AccountKind::Regular,
            })
            .await
            .unwrap();
        // Granted inside the tenant, but attached to no tenant at all.
        let scope = CapabilityScope::Tenant {
            user: outsider.clone(),
            tenant: tenant_id(),
        };
        state
            .store()
            .grant(&Grant::new(Capability::UserMgr, scope.clone()).unwrap())
            .await
            .unwrap();
        let manager = Manager {
            user: outsider,
            kind: AccountKind::Regular,
            proof: Proof::Session,
        };
        assert!(
            !manager
                .allows(state.store().as_ref(), Capability::UserMgr, &scope)
                .await
                .unwrap()
        );
    }
}
