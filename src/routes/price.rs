use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use ip_core::{ModelName, Price, Rate, Timestamp};
use ip_storage::{NewPrice, Priced};

use super::store_refusal;
use crate::manage::Manager;
use crate::state::AppState;

/// What each model costs, under the prefix the router nests it at.
///
/// Prices are the deployment's, not a tenant's: they are what a vendor charges for a
/// model, and every node prices every request by the same list. So only the system
/// administrator manages them, exactly as with the routing rules.
pub fn router() -> Router<AppState> {
    Router::new().route("/", get(list).put(put).delete(remove))
}

/// One model's price, as the api reports it.
#[derive(Debug, serde::Serialize)]
pub struct PriceView<'a> {
    /// The model priced.
    pub model: &'a str,
    /// Micros of the deployment's currency per million tokens sent.
    pub input: Rate,
    /// Micros of the deployment's currency per million tokens answered with.
    pub output: Rate,
    /// When the price was set, in seconds since the unix epoch.
    pub created_at: Timestamp,
}

/// A price as the api takes it.
#[derive(Debug, serde::Deserialize)]
struct Quoted {
    /// The model to price.
    model: ModelName,
    /// Micros of the deployment's currency per million tokens sent.
    input: Rate,
    /// Micros of the deployment's currency per million tokens answered with.
    output: Rate,
}

/// The model a price is being removed from.
#[derive(Debug, serde::Deserialize)]
struct Named {
    /// The model to stop pricing.
    model: ModelName,
}

/// Every price set, newest first.
///
/// Unpaged, unlike the other lists: there is one row per model a deployment prices, which
/// is tens of rows and is read by a human deciding what to change.
async fn list(State(state): State<AppState>, manager: Manager) -> Response<Body> {
    if !manager.is_system_administrator() {
        return StatusCode::FORBIDDEN.into_response();
    }
    match state.store().prices().await {
        Ok(held) => Json(held.iter().map(view).collect::<Vec<_>>()).into_response(),
        Err(error) => store_refusal(error),
    }
}

/// Sets a model's price, replacing whatever it cost before.
async fn put(State(state): State<AppState>, manager: Manager, body: Bytes) -> Response<Body> {
    if !manager.is_system_administrator() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Json(quoted) = match Json::<Quoted>::from_bytes(&body) {
        Ok(quoted) => quoted,
        Err(rejection) => return rejection.into_response(),
    };
    let setting = NewPrice {
        model: quoted.model,
        price: Price::new(quoted.input, quoted.output),
    };
    match state.store().put_price(setting).await {
        Ok(priced) => {
            state.feed().after_change().await;
            // The price as stored, so the caller learns when it was set without asking
            // again — the shape a limit answers a caller with.
            (StatusCode::CREATED, Json(view(&priced))).into_response()
        }
        Err(error) => store_refusal(error),
    }
}

/// Stops pricing a model.
///
/// What it costs becomes unknown rather than nothing: the rows written afterwards say so,
/// and no request is suddenly recorded as free.
async fn remove(State(state): State<AppState>, manager: Manager, body: Bytes) -> Response<Body> {
    if !manager.is_system_administrator() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Json(named) = match Json::<Named>::from_bytes(&body) {
        Ok(named) => named,
        Err(rejection) => return rejection.into_response(),
    };
    match state.store().remove_price(&named.model).await {
        Ok(()) => {
            state.feed().after_change().await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => store_refusal(error),
    }
}

/// A stored price as the api reports it.
fn view(priced: &Priced) -> PriceView<'_> {
    PriceView {
        model: priced.model.as_str(),
        input: priced.price.input,
        output: priced.price.output,
        created_at: priced.created_at,
    }
}

#[cfg(test)]
mod tests {
    use ip_core::{TenantId, TnKey, UserId};
    use ip_storage::{AccountKind, NewTenant, NewUser, SqliteStore, TenantStore, UserStore};
    use tower::ServiceExt;

    use super::super::harness::{body_of, cookie_of, hashed, request, state_over};
    use super::*;

    fn id(raw: &str) -> UserId {
        UserId::new(raw).unwrap()
    }

    /// A store holding root, the system administrator, and alice, who is not.
    async fn store() -> SqliteStore {
        let store = SqliteStore::in_memory().await.unwrap();
        store
            .create_tenant(NewTenant {
                id: TenantId::new("acme").unwrap(),
                key: TnKey::generate().unwrap(),
            })
            .await
            .unwrap();
        for (user, kind) in [
            ("root", AccountKind::SystemAdministrator),
            ("alice", AccountKind::Regular),
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
        store
    }

    async fn call(
        router: &Router,
        state: &AppState,
        caller: &str,
        method: &str,
        body: Option<&str>,
    ) -> axum::http::Response<Body> {
        router
            .clone()
            .oneshot(request(
                method,
                "/_ip/prices",
                &cookie_of(state, &id(caller)).await,
                body,
            ))
            .await
            .unwrap()
    }

    async fn listed(router: &Router, state: &AppState) -> serde_json::Value {
        let response = call(router, state, "root", "GET", None).await;
        assert_eq!(response.status(), StatusCode::OK);
        serde_json::from_str(&body_of(response).await).unwrap()
    }

    const OPUS: &str = r#"{"model": "claude-opus-5", "input": 3000000, "output": 15000000}"#;

    #[tokio::test]
    async fn the_administrator_prices_a_model_and_reads_it_back() {
        let state = state_over(store().await);
        let router = crate::routes::router(state.clone());

        let set = call(&router, &state, "root", "PUT", Some(OPUS)).await;
        assert_eq!(set.status(), StatusCode::CREATED);
        let answered: serde_json::Value = serde_json::from_str(&body_of(set).await).unwrap();
        assert_eq!(answered["model"], "claude-opus-5");
        assert!(answered["created_at"].is_number(), "set, and when");

        let listed = listed(&router, &state).await;
        assert_eq!(listed.as_array().unwrap().len(), 1);
        assert_eq!(listed[0]["model"], "claude-opus-5");
        assert_eq!(listed[0]["input"], 3_000_000);
        assert_eq!(listed[0]["output"], 15_000_000);
        assert!(listed[0]["created_at"].is_number());
    }

    #[tokio::test]
    async fn pricing_a_model_again_replaces_what_it_cost() {
        let state = state_over(store().await);
        let router = crate::routes::router(state.clone());
        call(&router, &state, "root", "PUT", Some(OPUS)).await;

        let again = r#"{"model": "claude-opus-5", "input": 1000000, "output": 5000000}"#;
        assert_eq!(
            call(&router, &state, "root", "PUT", Some(again))
                .await
                .status(),
            StatusCode::CREATED
        );

        let listed = listed(&router, &state).await;
        assert_eq!(listed.as_array().unwrap().len(), 1, "one model, one price");
        assert_eq!(listed[0]["input"], 1_000_000);
    }

    #[tokio::test]
    async fn a_price_is_removed_and_removing_one_that_is_absent_says_so() {
        let state = state_over(store().await);
        let router = crate::routes::router(state.clone());
        call(&router, &state, "root", "PUT", Some(OPUS)).await;
        let named = r#"{"model": "claude-opus-5"}"#;

        let removed = call(&router, &state, "root", "DELETE", Some(named)).await;
        assert_eq!(removed.status(), StatusCode::NO_CONTENT);
        assert!(listed(&router, &state).await.as_array().unwrap().is_empty());

        let again = call(&router, &state, "root", "DELETE", Some(named)).await;
        assert_eq!(again.status(), StatusCode::NOT_FOUND);
    }

    /// A price past the ceiling is refused by the type, before any store sees it.
    #[tokio::test]
    async fn a_rate_no_vendor_charges_is_refused() {
        let state = state_over(store().await);
        let router = crate::routes::router(state.clone());
        let absurd = r#"{"model": "claude-opus-5", "input": 9000000000000, "output": 1}"#;

        let refused = call(&router, &state, "root", "PUT", Some(absurd)).await;

        assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(listed(&router, &state).await.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_model_name_that_is_not_one_is_refused() {
        let state = state_over(store().await);
        let router = crate::routes::router(state.clone());
        let empty = r#"{"model": "", "input": 1, "output": 1}"#;

        let refused = call(&router, &state, "root", "PUT", Some(empty)).await;

        assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// Prices are the deployment's, so an ordinary account neither reads nor sets them.
    #[tokio::test]
    async fn nobody_but_the_administrator_touches_a_price() {
        let state = state_over(store().await);
        let router = crate::routes::router(state.clone());

        for (method, body) in [
            ("GET", None),
            ("PUT", Some(OPUS)),
            ("DELETE", Some(r#"{"model": "claude-opus-5"}"#)),
        ] {
            let refused = call(&router, &state, "alice", method, body).await;
            assert_eq!(
                refused.status(),
                StatusCode::FORBIDDEN,
                "{method} answered {}",
                refused.status()
            );
        }
    }
}
