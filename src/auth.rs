use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::StatusCode;
use axum::http::request::Parts;
use ip_auth::{Authority, SignedRequest};
use ip_core::{Nonce, Signature, TenantId, UserId};

use crate::state::AppState;

const HEADER_TENANT: &str = "x-ip-tnid";
const HEADER_USER: &str = "x-ip-userid";
const HEADER_SIGNATURE: &str = "x-ip-signature";
const HEADER_NONCE: &str = "x-ip-nonce";

/// A handler argument that only exists once the request signature checked out,
/// carrying everything the caller was granted.
#[derive(Debug, Clone)]
pub struct Authenticated(pub Authority);

impl FromRequestParts<AppState> for Authenticated {
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let request = signed_request(parts).ok_or_else(|| {
            tracing::warn!("rejected a request whose signing headers are absent or malformed");
            StatusCode::UNAUTHORIZED
        })?;
        let origin = origin(parts);
        let verifier = state.verifier();
        // Every reason collapses to one status: telling them apart enumerates tenants and users.
        let identity = verifier.verify(&request, origin).await.map_err(|error| {
            tracing::warn!(%origin, %error, "rejected a signed request");
            StatusCode::UNAUTHORIZED
        })?;
        let authority = verifier.authority(identity).await.map_err(|error| {
            tracing::error!(%error, "could not read what the caller was granted");
            StatusCode::UNAUTHORIZED
        })?;
        Ok(Self(authority))
    }
}

fn signed_request(parts: &Parts) -> Option<SignedRequest> {
    Some(SignedRequest {
        tenant: TenantId::new(header(parts, HEADER_TENANT)?).ok()?,
        user: UserId::new(header(parts, HEADER_USER)?).ok()?,
        nonce: Nonce::new(header(parts, HEADER_NONCE)?).ok()?,
        signature: Signature::from_hex(header(parts, HEADER_SIGNATURE)?).ok()?,
    })
}

fn header<'a>(parts: &'a Parts, name: &str) -> Option<&'a str> {
    parts.headers.get(name)?.to_str().ok()
}

/// An absent peer address is treated as remote, so the admin rule can never be skipped.
fn origin(parts: &Parts) -> IpAddr {
    parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(address)| address.ip())
        .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
}
