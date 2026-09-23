//! The header and uri surgery a proxy owes the next hop.

use http::header::{
    CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST, HeaderName, PROXY_AUTHENTICATE,
    PROXY_AUTHORIZATION, TE, TRAILER, TRANSFER_ENCODING, UPGRADE,
};
use http::uri::{Authority as UriAuthority, Scheme};
use http::{HeaderMap, HeaderValue, Request, Uri};
use ip_core::Endpoint;
use std::net::SocketAddr;

use crate::body::GatewayBody;
use crate::error::{GatewayError, GatewayErrorKind};
use crate::stage::{Forwarded, Stage};

/// Points the request at the endpoint it was routed to, carrying its query across.
pub(super) fn rewrite(
    request: Request<GatewayBody>,
    target: &Endpoint,
) -> Result<Request<GatewayBody>, GatewayError> {
    let query = request.uri().query().map(ToOwned::to_owned);
    let (mut parts, body) = request.into_parts();
    parts.uri = target_uri(target, query.as_deref()).map_err(|detail| {
        GatewayError::new(Forwarded::NAME, GatewayErrorKind::Malformed { detail })
    })?;
    let host = format!("{}:{}", target.host, target.port);
    let host = host.parse().map_err(|_| {
        GatewayError::new(
            Forwarded::NAME,
            GatewayErrorKind::Malformed {
                detail: format!("{host} is not a host header"),
            },
        )
    })?;
    strip_hop_by_hop(&mut parts.headers);
    parts.headers.insert(HOST, host);
    Ok(Request::from_parts(parts, body))
}

/// Headers that describe one connection, which a proxy must not pass on to the next.
const HOP_BY_HOP: [HeaderName; 8] = [
    CONNECTION,
    HeaderName::from_static("keep-alive"),
    HeaderName::from_static("proxy-connection"),
    PROXY_AUTHENTICATE,
    PROXY_AUTHORIZATION,
    TRAILER,
    TRANSFER_ENCODING,
    UPGRADE,
];

/// Drops every hop by hop header, including any the `Connection` header names.
/// `te: trailers` survives, because grpc needs it end to end.
pub(super) fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let listed: Vec<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in listed.iter().chain(HOP_BY_HOP.iter()) {
        headers.remove(name);
    }
    let trailers_only = headers
        .get_all(TE)
        .iter()
        .all(|value| value.as_bytes().eq_ignore_ascii_case(b"trailers"));
    if !trailers_only {
        headers.remove(TE);
    }
}

/// The authority a request names, which the `Host` header carries over http 1.1
/// and the uri carries over http 2.
pub(super) fn authority_of(request: &Request<GatewayBody>, listen: SocketAddr) -> String {
    if let Some(authority) = request.uri().authority() {
        return authority.to_string();
    }
    request
        .headers()
        .get(HOST)
        .and_then(|host| host.to_str().ok())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| listen.to_string())
}

fn target_uri(target: &Endpoint, query: Option<&str>) -> Result<Uri, String> {
    let scheme = Scheme::try_from(target.protocol.name())
        .map_err(|_| format!("{} is not a uri scheme", target.protocol))?;
    let authority = format!("{}:{}", target.host, target.port);
    let authority = UriAuthority::try_from(authority.as_str())
        .map_err(|_| format!("{authority} is not a uri authority"))?;
    let path = match query {
        Some(query) => format!("{}?{}", target.path, query),
        None => target.path.to_string(),
    };
    Uri::builder()
        .scheme(scheme)
        .authority(authority)
        .path_and_query(path)
        .build()
        .map_err(|error| error.to_string())
}

/// Whether a response is a stream of server sent events.
pub(super) fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
}

/// Makes `Content-Length` match a body a plugin rewrote, so the next hop reads all of it.
pub(super) fn set_length(headers: &mut HeaderMap, length: usize) {
    headers.insert(CONTENT_LENGTH, HeaderValue::from(length));
}

#[cfg(test)]
mod tests {
    use super::super::Gateway;
    use super::super::fixtures::*;
    use super::*;
    use crate::processor::ProcessorChain;

    #[tokio::test]
    async fn the_host_header_is_rewritten_to_the_target() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(table(), ProcessorChain::new(), upstream.clone());
        gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap();
        assert_eq!(upstream.last().host, "api.example.com:443");
    }

    #[tokio::test]
    async fn hop_by_hop_headers_never_reach_the_upstream() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(table(), ProcessorChain::new(), upstream.clone());
        let mut request = request("/anthropic", "");
        for (name, value) in [
            ("connection", "keep-alive, x-private"),
            ("keep-alive", "timeout=5"),
            ("x-private", "for this hop only"),
            ("upgrade", "websocket"),
            ("transfer-encoding", "chunked"),
            ("proxy-authorization", "Basic c2VjcmV0"),
            ("te", "gzip"),
            ("x-keep", "end to end"),
        ] {
            request.headers_mut().insert(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        gateway.handle(context(granted()), request).await.unwrap();
        let sent = upstream.last().headers;
        for name in [
            "connection",
            "keep-alive",
            "x-private",
            "upgrade",
            "transfer-encoding",
            "proxy-authorization",
            "te",
        ] {
            assert!(sent.get(name).is_none(), "{name} reached the upstream");
        }
        assert_eq!(sent.get("x-keep").unwrap(), "end to end");
    }

    #[tokio::test]
    async fn te_trailers_is_carried_for_grpc() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(table(), ProcessorChain::new(), upstream.clone());
        let mut request = request("/anthropic", "");
        request
            .headers_mut()
            .insert(TE, HeaderValue::from_static("trailers"));
        gateway.handle(context(granted()), request).await.unwrap();
        assert_eq!(upstream.last().headers.get(TE).unwrap(), "trailers");
    }

    #[tokio::test]
    async fn hop_by_hop_headers_from_the_upstream_never_reach_the_caller() {
        let upstream = FakeUpstream::answering_with_headers(
            "pong",
            vec![
                ("connection", "close"),
                ("keep-alive", "timeout=5"),
                ("x-upstream-keep", "1"),
            ],
        );
        let gateway = Gateway::new(table(), ProcessorChain::new(), upstream);
        let response = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap();
        assert!(response.headers().get("connection").is_none());
        assert!(response.headers().get("keep-alive").is_none());
        assert_eq!(response.headers().get("x-upstream-keep").unwrap(), "1");
    }
}
