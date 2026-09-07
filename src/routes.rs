use axum::routing::get;
use axum::{Json, Router};
use ip_auth::Identity;
use ip_core::Role;

use crate::auth::Authenticated;
use crate::state::AppState;

/// Every route the server serves.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/whoami", get(whoami))
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

async fn whoami(Authenticated(identity): Authenticated) -> Json<WhoAmI> {
    Json(WhoAmI::from(identity))
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
    use std::sync::Arc;

    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use ip_core::{Nonce, PassphraseHash, Signature, TenantId, TnKey, UserId, UtKey};
    use ip_storage::{
        AccountKind, Membership, MembershipStore, NewTenant, NewUser, SqliteStore, Standing,
        Storage, TenantStore, UserStore,
    };
    use tower::ServiceExt;

    use super::*;

    const REMOTE: [u8; 4] = [203, 0, 113, 7];
    const LOCAL: [u8; 4] = [127, 0, 0, 1];

    fn tenant_id() -> TenantId {
        TenantId::new("acme").unwrap()
    }

    fn user_id() -> UserId {
        UserId::new("alice").unwrap()
    }

    fn nonce() -> Nonce {
        Nonce::new("0123456789abcdef").unwrap()
    }

    async fn fixture(kind: AccountKind, standing: Standing) -> (Router, TnKey) {
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
        let state = AppState::with_store(Arc::new(store) as Arc<dyn Storage>);
        (router(state), key)
    }

    fn request(uri: &str, signature: Option<&str>, peer: [u8; 4]) -> Request<Body> {
        let mut builder = Request::builder().uri(uri);
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
            .insert(ConnectInfo(SocketAddr::from((peer, 40_000))));
        request
    }

    fn user_signature(key: &TnKey) -> String {
        Signature::of_user(&UtKey::derive(&user_id(), key), &nonce()).to_hex()
    }

    async fn body_of(response: axum::response::Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn health_needs_no_credentials() {
        let (router, _) = fixture(AccountKind::Regular, Standing::Member).await;
        let response = router
            .oneshot(request("/healthz", None, REMOTE))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await, "ok");
    }

    #[tokio::test]
    async fn an_unsigned_request_is_refused() {
        let (router, _) = fixture(AccountKind::Regular, Standing::Member).await;
        let response = router
            .oneshot(request("/whoami", None, REMOTE))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(body_of(response).await.is_empty());
    }

    #[tokio::test]
    async fn a_signed_request_names_its_caller() {
        let (router, key) = fixture(AccountKind::Regular, Standing::Member).await;
        let response = router
            .oneshot(request("/whoami", Some(&user_signature(&key)), REMOTE))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_of(response).await).unwrap();
        assert_eq!(body["user"], "alice");
        assert_eq!(body["tenant"], "acme");
        assert_eq!(body["role"], "member");
    }

    #[tokio::test]
    async fn a_tenant_owner_is_named_as_one() {
        let (router, key) = fixture(AccountKind::Regular, Standing::Owner).await;
        let response = router
            .oneshot(request("/whoami", Some(&user_signature(&key)), REMOTE))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&body_of(response).await).unwrap();
        assert_eq!(body["role"], "tenant_admin");
    }

    #[tokio::test]
    async fn a_signature_from_another_key_is_refused() {
        let (router, _) = fixture(AccountKind::Regular, Standing::Member).await;
        let stolen = TnKey::generate().unwrap();
        let response = router
            .oneshot(request("/whoami", Some(&user_signature(&stolen)), REMOTE))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_signature_that_is_not_hex_is_refused() {
        let (router, _) = fixture(AccountKind::Regular, Standing::Member).await;
        let response = router
            .oneshot(request("/whoami", Some("not-a-signature"), REMOTE))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_system_administrator_is_refused_off_localhost() {
        let (router, key) = fixture(AccountKind::SystemAdministrator, Standing::Member).await;
        let response = router
            .clone()
            .oneshot(request("/whoami", Some(&user_signature(&key)), REMOTE))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = router
            .oneshot(request("/whoami", Some(&user_signature(&key)), LOCAL))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_of(response).await).unwrap();
        assert_eq!(body["role"], "sysadmin");
    }

    #[tokio::test]
    async fn a_request_with_no_peer_address_is_treated_as_remote() {
        let (router, key) = fixture(AccountKind::SystemAdministrator, Standing::Member).await;
        let mut bare = Request::builder()
            .uri("/whoami")
            .header("x-ip-tnid", tenant_id().as_str())
            .header("x-ip-userid", user_id().as_str())
            .header("x-ip-nonce", nonce().as_str())
            .header("x-ip-signature", user_signature(&key))
            .body(Body::empty())
            .unwrap();
        bare.extensions_mut().remove::<ConnectInfo<SocketAddr>>();
        let response = router.oneshot(bare).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
