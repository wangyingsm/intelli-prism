use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, RETRY_AFTER, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use ip_auth::{AuthError, Passphrase};
use ip_core::{TenantId, UtKey};
use ip_storage::Standing;

use super::store_refusal;
use crate::cookie;
use crate::manage::Manager;
use crate::state::AppState;

/// Where the web ui shows a caller its keys, under the prefix the router nests them at.
///
/// A key is shown to a session alone, never to a signed request, and only once the caller has
/// given its passphrase again: a cookie left in an unattended browser is not enough.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/user", post(user_key))
        .route("/tenant", post(tenant_key))
}

/// What revealing the caller's own key in a tenant is asked for.
#[derive(Debug, serde::Deserialize)]
pub struct UserKeyRequest {
    /// The tenant the key signs for.
    tenant: String,
    /// The caller's passphrase, given again.
    passphrase: String,
}

/// What revealing the key of the tenant the caller owns is asked for.
#[derive(Debug, serde::Deserialize)]
pub struct TenantKeyRequest {
    /// The owner's passphrase, given again.
    passphrase: String,
}

/// A key, shown once to the one it belongs to.
#[derive(Debug, serde::Serialize)]
pub struct KeyView {
    /// The tenant the key signs for.
    pub tenant: String,
    /// The account the key signs as, when it is one account's rather than the tenant's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// The key, in hex.
    pub key: String,
}

/// Shows the caller the key it signs with inside one tenant it is in.
async fn user_key(
    State(state): State<AppState>,
    manager: Manager,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    if !manager.is_session() {
        return not_for_signed_requests();
    }
    let Json(request) = match Json::<UserKeyRequest>::from_bytes(&body) {
        Ok(request) => request,
        Err(rejection) => return rejection.into_response(),
    };
    let Ok(tenant) = TenantId::new(&request.tenant) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match state.store().membership(manager.user(), &tenant).await {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return store_refusal(error),
    }
    if let Some(refusal) = recheck(&state, &manager, &headers, &request.passphrase).await {
        return refusal;
    }
    let stored = match state.store().tenant(&tenant).await {
        Ok(Some(stored)) => stored,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return store_refusal(error),
    };
    tracing::info!(user = %manager.user(), %tenant, "revealed a user key");
    shown(KeyView {
        tenant: tenant.to_string(),
        user: Some(manager.user().to_string()),
        key: UtKey::derive(manager.user(), &stored.key).to_hex(),
    })
}

/// Shows a tenant's owner the key of the tenant it owns, which every key in it derives from.
async fn tenant_key(
    State(state): State<AppState>,
    manager: Manager,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    if !manager.is_session() {
        return not_for_signed_requests();
    }
    let Json(request) = match Json::<TenantKeyRequest>::from_bytes(&body) {
        Ok(request) => request,
        Err(rejection) => return rejection.into_response(),
    };
    let owned = match state.store().memberships_of_user(manager.user()).await {
        Ok(memberships) => memberships
            .into_iter()
            .find(|held| held.standing == Standing::Owner),
        Err(error) => return store_refusal(error),
    };
    let Some(owned) = owned else {
        return StatusCode::FORBIDDEN.into_response();
    };
    if let Some(refusal) = recheck(&state, &manager, &headers, &request.passphrase).await {
        return refusal;
    }
    let stored = match state.store().tenant(&owned.tenant).await {
        Ok(Some(stored)) => stored,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return store_refusal(error),
    };
    tracing::info!(user = %manager.user(), tenant = %owned.tenant, "revealed a tenant key");
    shown(KeyView {
        tenant: owned.tenant.to_string(),
        user: None,
        key: stored.key.to_hex(),
    })
}

/// Asks the caller for its passphrase again, and answers what it is told when that fails.
///
/// A lock ends the session as well: whoever is guessing may hold a cookie that is not theirs,
/// and the one it belongs to only has to log in again.
async fn recheck(
    state: &AppState,
    manager: &Manager,
    headers: &HeaderMap,
    passphrase: &str,
) -> Option<Response<Body>> {
    let Ok(passphrase) = Passphrase::new(passphrase) else {
        return Some(StatusCode::UNPROCESSABLE_ENTITY.into_response());
    };
    match state.logins().recheck(manager.user(), &passphrase).await {
        Ok(()) => None,
        Err(AuthError::RecheckRefused) => {
            tracing::warn!(user = %manager.user(), "refused a key reveal on a wrong passphrase");
            Some((StatusCode::FORBIDDEN, "passphrase does not match").into_response())
        }
        Err(AuthError::RecheckLocked) => {
            tracing::warn!(user = %manager.user(), "locked key reveals after too many wrong passphrases");
            Some(locked(state, headers).await)
        }
        Err(error) => {
            tracing::error!(%error, "could not check a passphrase given again");
            Some(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

/// Ends the session a lock was reached in, and says when asking again can work.
async fn locked(state: &AppState, headers: &HeaderMap) -> Response<Body> {
    if let Some(token) = cookie::token(headers)
        && let Ok(session) = state.logins().session(token).await
        && let Err(error) = state.logins().end(&session).await
    {
        tracing::error!(%error, "could not end a session that reached the reveal lock");
    }
    let wait = state.logins().recheck_limit().window().whole_seconds();
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        "too many wrong passphrases; log in again later",
    )
        .into_response();
    let headers = response.headers_mut();
    headers.insert(SET_COOKIE, cookie::clear());
    if let Ok(wait) = HeaderValue::from_str(&wait.to_string()) {
        headers.insert(RETRY_AFTER, wait);
    }
    response
}

fn not_for_signed_requests() -> Response<Body> {
    (
        StatusCode::FORBIDDEN,
        "keys are shown to a logged in session alone",
    )
        .into_response()
}

/// A key as the web ui is handed it, kept out of every cache on the way.
fn shown(view: KeyView) -> Response<Body> {
    let mut response = Json(view).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
mod tests {
    use axum::http::Request;
    use ip_core::{Nonce, Signature, TnKey, UserId};
    use ip_storage::{
        AccountKind, Membership, MembershipStore, NewTenant, NewUser, SqliteStore, TenantStore,
        UserStore,
    };
    use tower::ServiceExt;

    use super::super::harness::{PASSPHRASE, body_of, cookie_of, hashed, request, state_over};
    use super::*;

    fn id(raw: &str) -> UserId {
        UserId::new(raw).unwrap()
    }

    fn tenant(raw: &str) -> TenantId {
        TenantId::new(raw).unwrap()
    }

    /// The server's own router over acme, owned by alice with bob in it, and globex, owned by
    /// dave. Root is the system administrator. Hands back acme's key as well.
    async fn fixture() -> (Router, AppState, TnKey) {
        let store = SqliteStore::in_memory().await.unwrap();
        let acme_key = TnKey::generate().unwrap();
        for (name, key) in [
            ("acme", acme_key.clone()),
            ("globex", TnKey::generate().unwrap()),
        ] {
            store
                .create_tenant(NewTenant {
                    id: tenant(name),
                    key,
                })
                .await
                .unwrap();
        }
        for (user, kind) in [
            ("root", AccountKind::SystemAdministrator),
            ("alice", AccountKind::Regular),
            ("bob", AccountKind::Regular),
            ("dave", AccountKind::Regular),
        ] {
            store
                .create_user(NewUser {
                    id: id(user),
                    passphrase: hashed(),
                    kind,
                })
                .await
                .unwrap();
        }
        for (user, in_tenant, standing) in [
            ("alice", "acme", Standing::Owner),
            ("bob", "acme", Standing::Member),
            ("dave", "globex", Standing::Owner),
        ] {
            store
                .attach(Membership {
                    user: id(user),
                    tenant: tenant(in_tenant),
                    standing,
                })
                .await
                .unwrap();
        }
        let state = state_over(store);
        (crate::routes::router(state.clone()), state, acme_key)
    }

    fn asking(tenant: Option<&str>, passphrase: &str) -> String {
        let mut body = serde_json::json!({"passphrase": passphrase});
        if let Some(tenant) = tenant {
            body["tenant"] = tenant.into();
        }
        body.to_string()
    }

    async fn reveal(
        router: &Router,
        cookie: &str,
        which: &str,
        body: &str,
    ) -> axum::http::Response<Body> {
        router
            .clone()
            .oneshot(request(
                "POST",
                &format!("/_ip/keys/{which}"),
                cookie,
                Some(body),
            ))
            .await
            .unwrap()
    }

    async fn json(response: axum::http::Response<Body>) -> serde_json::Value {
        serde_json::from_str(&body_of(response).await).unwrap()
    }

    #[tokio::test]
    async fn a_member_sees_its_own_key_once_it_gives_its_passphrase_again() {
        let (router, state, acme_key) = fixture().await;
        let cookie = cookie_of(&state, &id("bob")).await;
        let response = reveal(&router, &cookie, "user", &asking(Some("acme"), PASSPHRASE)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
        let body = json(response).await;
        assert_eq!(body["key"], UtKey::derive(&id("bob"), &acme_key).to_hex());
        assert_eq!(body["user"], "bob");
        assert_eq!(body["tenant"], "acme");
    }

    #[tokio::test]
    async fn an_owner_sees_the_key_of_the_tenant_it_owns() {
        let (router, state, acme_key) = fixture().await;
        let cookie = cookie_of(&state, &id("alice")).await;
        let response = reveal(&router, &cookie, "tenant", &asking(None, PASSPHRASE)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
        let body = json(response).await;
        assert_eq!(body["key"], acme_key.to_hex());
        assert_eq!(body["tenant"], "acme");
        assert!(body.get("user").is_none());
    }

    #[tokio::test]
    async fn a_wrong_passphrase_shows_nothing() {
        let (router, state, _) = fixture().await;
        let cookie = cookie_of(&state, &id("bob")).await;
        let response = reveal(
            &router,
            &cookie,
            "user",
            &asking(Some("acme"), "incorrect horse staple"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(!body_of(response).await.contains("key"));
    }

    #[tokio::test]
    async fn no_key_is_shown_for_a_tenant_the_caller_is_not_in() {
        let (router, state, _) = fixture().await;
        let cookie = cookie_of(&state, &id("bob")).await;
        for tenant in ["globex", "nowhere"] {
            let response =
                reveal(&router, &cookie, "user", &asking(Some(tenant), PASSPHRASE)).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{tenant}");
        }
    }

    #[tokio::test]
    async fn only_an_owner_sees_a_tenant_key() {
        let (router, state, _) = fixture().await;
        for caller in ["bob", "root"] {
            let cookie = cookie_of(&state, &id(caller)).await;
            let response = reveal(&router, &cookie, "tenant", &asking(None, PASSPHRASE)).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{caller}");
        }
    }

    #[tokio::test]
    async fn a_signed_request_is_shown_no_key_whatever_it_sends() {
        let (router, _, acme_key) = fixture().await;
        let nonce = Nonce::new("0123456789abcdef").unwrap();
        let signature = Signature::of_user(&UtKey::derive(&id("bob"), &acme_key), &nonce);
        let signed = Request::builder()
            .method("POST")
            .uri("/_ip/keys/user")
            .header("x-ip-tnid", "acme")
            .header("x-ip-userid", "bob")
            .header("x-ip-nonce", nonce.as_str())
            .header("x-ip-signature", signature.to_hex())
            .header("content-type", "application/json")
            .body(Body::from(asking(Some("acme"), PASSPHRASE)))
            .unwrap();
        let response = router.oneshot(signed).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = body_of(response).await;
        assert_eq!(body, "keys are shown to a logged in session alone");
        assert!(!body.contains(&acme_key.to_hex()));
    }

    #[tokio::test]
    async fn too_many_wrong_passphrases_lock_reveals_and_end_the_session() {
        let (router, state, _) = fixture().await;
        let cookie = cookie_of(&state, &id("bob")).await;
        let wrong = asking(Some("acme"), "incorrect horse staple");
        for _ in 1..ip_auth::MAX_RECHECK_ATTEMPTS.get() {
            let refused = reveal(&router, &cookie, "user", &wrong).await;
            assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        }
        let locked = reveal(&router, &cookie, "user", &wrong).await;
        assert_eq!(locked.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(locked.headers()[RETRY_AFTER], "900");
        assert!(
            locked.headers()[SET_COOKIE]
                .to_str()
                .unwrap()
                .contains("Max-Age=0")
        );

        let same_cookie = reveal(&router, &cookie, "user", &asking(Some("acme"), PASSPHRASE)).await;
        assert_eq!(same_cookie.status(), StatusCode::UNAUTHORIZED);

        let fresh = cookie_of(&state, &id("bob")).await;
        let still_locked = reveal(&router, &fresh, "user", &asking(Some("acme"), PASSPHRASE)).await;
        assert_eq!(still_locked.status(), StatusCode::TOO_MANY_REQUESTS);
    }
}
