use std::time::Duration;

use async_trait::async_trait;
use http::{Request, Response};
use http_body_util::BodyExt;
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

use crate::body::GatewayBody;
use crate::upstream::{Upstream, UpstreamError};

/// How the upstream connection pool behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpstreamSettings {
    /// How long a connection may take to establish.
    pub connect_timeout: Duration,
    /// How long the whole exchange may take before the caller is told the upstream failed.
    pub request_timeout: Duration,
    /// How long an unused pooled connection is kept.
    pub pool_idle_timeout: Duration,
    /// How many idle connections are kept per upstream host.
    pub pool_max_idle_per_host: usize,
}

impl Default for UpstreamSettings {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(300),
            pool_idle_timeout: Duration::from_secs(90),
            pool_max_idle_per_host: 32,
        }
    }
}

/// Carries requests upstream over hyper, pooling connections and streaming both bodies.
#[derive(Clone)]
pub struct HyperUpstream {
    client: Client<hyper_rustls::HttpsConnector<HttpConnector>, GatewayBody>,
    request_timeout: Duration,
}

impl HyperUpstream {
    /// Builds a client at the default settings.
    pub fn new() -> Result<Self, UpstreamError> {
        Self::with_settings(UpstreamSettings::default())
    }

    /// Builds a client at chosen settings.
    pub fn with_settings(settings: UpstreamSettings) -> Result<Self, UpstreamError> {
        install_crypto_provider();
        let mut http = HttpConnector::new();
        http.set_connect_timeout(Some(settings.connect_timeout));
        http.enforce_http(false);
        let connector = HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(http);
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(settings.pool_idle_timeout)
            .pool_max_idle_per_host(settings.pool_max_idle_per_host)
            .build(connector);
        Ok(Self {
            client,
            request_timeout: settings.request_timeout,
        })
    }
}

/// Rustls needs one process wide provider; a second install is another caller winning the race.
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[async_trait]
impl Upstream for HyperUpstream {
    async fn send(
        &self,
        request: Request<GatewayBody>,
    ) -> Result<Response<GatewayBody>, UpstreamError> {
        let sent = tokio::time::timeout(self.request_timeout, self.client.request(request))
            .await
            .map_err(|_| {
                UpstreamError::new(format!(
                    "no answer within {} seconds",
                    self.request_timeout.as_secs()
                ))
            })?
            .map_err(|error| UpstreamError::new(error.to_string()))?;
        Ok(sent.map(|body| body.map_err(Into::into).boxed_unsync()))
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::net::SocketAddr;

    use bytes::Bytes;
    use http::{HeaderValue, StatusCode};
    use http_body_util::Full;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use tokio::net::TcpListener;

    use super::*;
    use crate::body::from_bytes;

    /// Answers with what it was sent, so the test can see what crossed the wire.
    async fn echo(
        request: Request<hyper::body::Incoming>,
    ) -> Result<Response<Full<Bytes>>, Infallible> {
        let method = request.method().clone();
        let path = request.uri().path().to_owned();
        let trace = request
            .headers()
            .get("x-ip-trace")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("-")
            .to_owned();
        let body = request.into_body().collect().await.unwrap().to_bytes();
        let rendered = format!("{method} {path} {trace} {}", String::from_utf8_lossy(&body));
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("x-upstream", "yes")
            .body(Full::new(Bytes::from(rendered)))
            .unwrap())
    }

    async fn echo_server() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service_fn(echo))
                        .await;
                });
            }
        });
        address
    }

    /// Accepts the connection and never answers, so the client's own clock is what ends it.
    async fn silent_server() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        address
    }

    fn request(address: SocketAddr, path: &str, body: &str) -> Request<GatewayBody> {
        Request::builder()
            .method("POST")
            .uri(format!("http://{address}{path}"))
            .header("x-ip-trace", "yes")
            .body(from_bytes(Bytes::from(body.to_owned())))
            .unwrap()
    }

    async fn body_of(response: Response<GatewayBody>) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn a_request_reaches_the_upstream_and_the_answer_comes_back() {
        let address = echo_server().await;
        let upstream = HyperUpstream::new().unwrap();
        let response = upstream
            .send(request(address, "/v1/messages", "payload"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("x-upstream"),
            Some(&HeaderValue::from_static("yes"))
        );
        assert_eq!(body_of(response).await, "POST /v1/messages yes payload");
    }

    #[tokio::test]
    async fn a_pooled_client_carries_more_than_one_request() {
        let address = echo_server().await;
        let upstream = HyperUpstream::new().unwrap();
        for index in 0..3 {
            let response = upstream
                .send(request(address, "/v1", &index.to_string()))
                .await
                .unwrap();
            assert_eq!(body_of(response).await, format!("POST /v1 yes {index}"));
        }
    }

    #[tokio::test]
    async fn an_upstream_that_is_not_listening_is_reported() {
        let upstream = HyperUpstream::new().unwrap();
        let unreachable: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let error = upstream
            .send(request(unreachable, "/v1", ""))
            .await
            .unwrap_err();
        assert!(!error.detail.is_empty());
    }

    #[tokio::test]
    async fn an_upstream_that_never_answers_gives_up() {
        let address = silent_server().await;
        let upstream = HyperUpstream::with_settings(UpstreamSettings {
            request_timeout: Duration::from_millis(150),
            ..UpstreamSettings::default()
        })
        .unwrap();
        let error = upstream
            .send(request(address, "/v1", ""))
            .await
            .unwrap_err();
        assert!(
            error.detail.contains("no answer within"),
            "expected a timeout, got {}",
            error.detail
        );
    }

    #[test]
    fn the_default_settings_bound_every_wait() {
        let settings = UpstreamSettings::default();
        assert_eq!(settings.connect_timeout, Duration::from_secs(10));
        assert_eq!(settings.request_timeout, Duration::from_secs(300));
        assert_eq!(settings.pool_idle_timeout, Duration::from_secs(90));
        assert_eq!(settings.pool_max_idle_per_host, 32);
    }
}
