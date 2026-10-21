//! The numbers, served where a scraper can come and take them.
//!
//! A listener of its own, on an address of its own, serving one path. What it answers
//! carries a tenant label, which is tenant data, so where it is bound is the whole of
//! the access control a deployment gets for free — the token is for the scraper that
//! has to cross a network to reach it.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use ip_config::{ScrapeConfig, Secret};

use crate::error::StartupError;
use crate::telemetry::Telemetry;

/// What a scraper is answered with, as Prometheus spells the exposition format.
const EXPOSITION: &str = "text/plain; version=0.0.4; charset=utf-8";

/// What the one route reads from.
#[derive(Clone)]
struct Scraping {
    telemetry: Arc<Telemetry>,
    token: Option<Secret>,
}

/// Binds the scrape listener and serves it until the process ends.
///
/// # Errors
///
/// [`StartupError::Bind`] when the address will not bind. A deployment that asked to
/// be scraped and cannot be is a misconfiguration, not a degraded mode: the operator
/// is told at startup rather than left with a dashboard that never fills.
pub async fn serve(
    config: &ScrapeConfig,
    telemetry: Arc<Telemetry>,
) -> Result<tokio::task::JoinHandle<()>, StartupError> {
    let address = config.listen;
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|source| StartupError::Bind { address, source })?;
    tracing::info!(%address, guarded = config.token.is_some(), "serving the numbers to scrape");
    let serving = router(config, telemetry);
    Ok(tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, serving).await {
            tracing::error!(%error, "the scrape listener stopped");
        }
    }))
}

/// The one route a scraper calls. Everything else on this listener is a 404.
fn router(config: &ScrapeConfig, telemetry: Arc<Telemetry>) -> Router {
    Router::new()
        .route("/metrics", get(gathered))
        .with_state(Scraping {
            telemetry,
            token: config.token.clone(),
        })
}

/// Answers with what the instruments hold at the moment of asking.
async fn gathered(State(scraping): State<Scraping>, headers: HeaderMap) -> Response {
    if !carries(scraping.token.as_ref(), &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match scraping.telemetry.gathered() {
        Some(numbers) => ([(header::CONTENT_TYPE, EXPOSITION)], numbers).into_response(),
        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// Whether the caller carries the token the operator set, and true where they set none.
///
/// The comparison is `Secret`'s own, which is constant time: a token checked with a
/// short circuiting equality tells whoever is guessing how much of it they had right.
fn carries(token: Option<&Secret>, headers: &HeaderMap) -> bool {
    let Some(token) = token else {
        return true;
    };
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|carried| &Secret::from(carried) == token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use opentelemetry::KeyValue;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::SdkMeterProvider;
    use tower::ServiceExt;

    /// A gateway that has counted one request, and the listener that serves it.
    ///
    /// The provider comes back with it and must outlive the asking: a provider that
    /// drops is a provider that has shut down, and a shut down reader gathers nothing.
    fn serving(token: Option<&str>) -> (Router, SdkMeterProvider) {
        let (reader, registry) = crate::telemetry::gathering().unwrap();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        provider
            .meter("test")
            .u64_counter("ip.requests")
            .build()
            .add(1, &[KeyValue::new("tenant", "acme")]);
        let config = ScrapeConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            token: token.map(Secret::from),
        };
        (
            router(&config, Arc::new(Telemetry::gathering_into(registry))),
            provider,
        )
    }

    /// Asks the listener for one path, carrying whatever the caller carries.
    async fn asked(router: Router, path: &str, bearer: Option<&str>) -> (StatusCode, String) {
        let mut request = Request::builder().uri(path);
        if let Some(bearer) = bearer {
            request = request.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        let answered = router
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = answered.status();
        let body = axum::body::to_bytes(answered.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn a_scraper_is_answered_with_the_numbers() {
        let (listener, _provider) = serving(None);
        let (status, body) = asked(listener, "/metrics", None).await;

        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("ip_requests_total"),
            "the collector's own suffix, as Prometheus spells it: {body}"
        );
        assert!(
            body.contains(r#"tenant="acme""#),
            "the labels come out with it: {body}"
        );
    }

    #[tokio::test]
    async fn the_exposition_says_what_it_is() {
        let (listener, _provider) = serving(None);
        let answered = listener
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            answered
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some(EXPOSITION),
            "a scraper reads the format from the header"
        );
    }

    #[tokio::test]
    async fn a_scraper_carrying_the_token_is_answered_and_one_without_it_is_not() {
        let (listener, _provider) = serving(Some("let-me-in"));
        let (status, _) = asked(listener, "/metrics", Some("let-me-in")).await;
        assert_eq!(status, StatusCode::OK);

        let (listener, _provider) = serving(Some("let-me-in"));
        let (status, _) = asked(listener, "/metrics", Some("let-me-nope")).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a wrong token is no token"
        );

        let (listener, _provider) = serving(Some("let-me-in"));
        let (status, body) = asked(listener, "/metrics", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(
            body.is_empty(),
            "a refusal says nothing about what it is guarding"
        );
    }

    #[tokio::test]
    async fn nothing_else_is_served_on_this_listener() {
        let (listener, _provider) = serving(None);
        let (status, _) = asked(listener, "/", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (listener, _provider) = serving(None);
        let (status, _) = asked(listener, "/anthropic/messages", None).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "the data plane is not on this port"
        );
    }

    #[tokio::test]
    async fn a_gateway_gathering_nothing_says_so_rather_than_answering_emptily() {
        let config = ScrapeConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            token: None,
        };
        let quiet = router(&config, Arc::new(Telemetry::quiet()));

        let (status, _) = asked(quiet, "/metrics", None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }
}
