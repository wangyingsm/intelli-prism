use std::sync::Arc;

use bytes::Bytes;
use http::HeaderValue;
use ip_cache::{Cache, CacheError, CacheKey, CacheLevel, Ttl};
use ip_core::{RouteKey, TenantId};
use sha2::{Digest, Sha256};

/// Bytes a cached response writes its content type's length in.
const TYPE_LEN: usize = size_of::<u16>();

/// The cache a repeated request is answered from, and how long an answer stays in it.
#[derive(Clone)]
pub struct ResponseCache {
    cache: Arc<dyn Cache>,
    ttl: Ttl,
}

impl ResponseCache {
    /// Answers repeated requests out of `cache`, keeping each answer for `ttl`.
    pub fn new(cache: Arc<dyn Cache>, ttl: Ttl) -> Self {
        Self { cache, ttl }
    }

    /// The answer held for the request `key` names, if the cache still holds one.
    pub async fn get(&self, key: &CacheKey) -> Result<Option<CachedResponse>, CacheError> {
        Ok(self
            .cache
            .get(key)
            .await?
            .and_then(|entry| CachedResponse::decode(&entry)))
    }

    /// Keeps `response` as the answer to the request `key` names.
    pub async fn put(&self, key: &CacheKey, response: &CachedResponse) -> Result<(), CacheError> {
        self.cache
            .put(key, &response.encode(), Some(self.ttl))
            .await
    }
}

/// Names one cached response, so a hit can only ever be the same tenant asking the same route
/// the same thing.
pub fn response_key(
    route: &RouteKey,
    tenant: &TenantId,
    body: &[u8],
) -> Result<CacheKey, CacheError> {
    let route = route.to_string();
    let mut hasher = Sha256::new();
    for part in [route.as_bytes(), tenant.as_str().as_bytes(), body] {
        // The lengths are hashed too, so no two different parts can concatenate to the same bytes.
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    CacheKey::new(CacheLevel::Response, &hex::encode(hasher.finalize()))
}

/// A response the cache holds: what it answered with, and what kind of body that was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedResponse {
    content_type: Option<HeaderValue>,
    body: Bytes,
}

impl CachedResponse {
    /// Keeps a body, with the content type the response carried if it carried one.
    pub fn new(content_type: Option<HeaderValue>, body: Bytes) -> Self {
        Self { content_type, body }
    }

    /// The content type to answer with again.
    pub fn content_type(&self) -> Option<&HeaderValue> {
        self.content_type.as_ref()
    }

    /// The body to answer with again.
    pub fn body(&self) -> &Bytes {
        &self.body
    }

    /// The entry as the cache stores it: the content type's length, the content type, the body.
    fn encode(&self) -> Vec<u8> {
        let content_type = self.content_type.as_ref().map_or(&[][..], |value| {
            let bytes = value.as_bytes();
            // A content type longer than the length field can hold is stored as none at all.
            if bytes.len() > u16::MAX as usize {
                &[][..]
            } else {
                bytes
            }
        });
        let mut entry = Vec::with_capacity(TYPE_LEN + content_type.len() + self.body.len());
        let len = u16::try_from(content_type.len()).unwrap_or(0);
        entry.extend_from_slice(&len.to_be_bytes());
        entry.extend_from_slice(content_type);
        entry.extend_from_slice(&self.body);
        entry
    }

    /// Reads an entry back, or nothing when the bytes are not one.
    fn decode(entry: &[u8]) -> Option<Self> {
        let (len, rest) = entry.split_at_checked(TYPE_LEN)?;
        let len = usize::from(u16::from_be_bytes(len.try_into().ok()?));
        let (content_type, body) = rest.split_at_checked(len)?;
        let content_type = match content_type.is_empty() {
            true => None,
            false => Some(HeaderValue::from_bytes(content_type).ok()?),
        };
        Some(Self {
            content_type,
            body: Bytes::copy_from_slice(body),
        })
    }
}

#[cfg(test)]
mod tests {
    use ip_core::{AbsPath, Endpoint, Host, Port, Protocol};

    use super::*;

    fn route(path: &str) -> RouteKey {
        RouteKey::new(Endpoint::new(
            Protocol::Https,
            Host::new("api.example.com").unwrap(),
            Port::new(443).unwrap(),
            AbsPath::new(path).unwrap(),
        ))
    }

    fn tenant(id: &str) -> TenantId {
        TenantId::new(id).unwrap()
    }

    fn json() -> HeaderValue {
        HeaderValue::from_static("application/json")
    }

    #[test]
    fn the_same_request_names_the_same_entry() {
        assert_eq!(
            response_key(&route("/v1"), &tenant("acme"), b"ask").unwrap(),
            response_key(&route("/v1"), &tenant("acme"), b"ask").unwrap()
        );
    }

    #[test]
    fn another_tenant_names_another_entry() {
        assert_ne!(
            response_key(&route("/v1"), &tenant("acme"), b"ask").unwrap(),
            response_key(&route("/v1"), &tenant("other"), b"ask").unwrap()
        );
    }

    #[test]
    fn another_route_names_another_entry() {
        assert_ne!(
            response_key(&route("/v1"), &tenant("acme"), b"ask").unwrap(),
            response_key(&route("/v2"), &tenant("acme"), b"ask").unwrap()
        );
    }

    #[test]
    fn another_body_names_another_entry() {
        assert_ne!(
            response_key(&route("/v1"), &tenant("acme"), b"ask").unwrap(),
            response_key(&route("/v1"), &tenant("acme"), b"ask again").unwrap()
        );
    }

    #[test]
    fn a_split_that_moves_bytes_between_parts_names_another_entry() {
        assert_ne!(
            response_key(&route("/v1"), &tenant("ac"), b"me").unwrap(),
            response_key(&route("/v1"), &tenant("acme"), b"").unwrap()
        );
    }

    #[test]
    fn an_entry_round_trips() {
        let response = CachedResponse::new(Some(json()), Bytes::from_static(b"{}"));
        assert_eq!(CachedResponse::decode(&response.encode()), Some(response));
    }

    #[test]
    fn an_entry_without_a_content_type_round_trips() {
        let response = CachedResponse::new(None, Bytes::from_static(b"body"));
        assert_eq!(CachedResponse::decode(&response.encode()), Some(response));
    }

    #[test]
    fn an_entry_with_an_empty_body_round_trips() {
        let response = CachedResponse::new(Some(json()), Bytes::new());
        assert_eq!(CachedResponse::decode(&response.encode()), Some(response));
    }

    #[test]
    fn bytes_too_short_to_hold_a_length_are_not_an_entry() {
        assert_eq!(CachedResponse::decode(&[7]), None);
    }

    #[test]
    fn a_length_longer_than_what_follows_is_not_an_entry() {
        assert_eq!(CachedResponse::decode(&[0, 9, b'a']), None);
    }
}
