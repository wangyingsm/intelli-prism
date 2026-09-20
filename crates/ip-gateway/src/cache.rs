use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use http::header::{CACHE_CONTROL, EXPIRES};
use http::{HeaderMap, HeaderValue};
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

    /// How long an answer stays when the response itself says nothing.
    pub fn ttl(&self) -> Ttl {
        self.ttl
    }

    /// Keeps `response` as the answer to the request `key` names, for `ttl`.
    pub async fn put(
        &self,
        key: &CacheKey,
        response: &CachedResponse,
        ttl: Ttl,
    ) -> Result<(), CacheError> {
        self.cache.put(key, &response.encode(), Some(ttl)).await
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

/// What a response's own headers say about keeping it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Keep the answer for exactly this long.
    For(Ttl),
    /// The response says nothing, so the system default stands.
    Unsaid,
    /// Keep nothing at all.
    Never,
}

/// Reads how long a response says its answer is good for.
///
/// `Cache-Control` outranks `Expires`, as http requires: `no-store`, `no-cache` and `private`
/// keep nothing, and `max-age` names the span. `Expires` names the moment it stops being good.
pub fn freshness(headers: &HeaderMap, now: SystemTime) -> Freshness {
    if let Some(control) = headers
        .get(CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
    {
        let directives: Vec<&str> = control
            .split(',')
            .map(|directive| directive.trim())
            .collect();
        // A shared cache keeps none of these, and this proxy is a shared cache.
        if directives
            .iter()
            .any(|directive| matches!(*directive, "no-store" | "no-cache" | "private"))
        {
            return Freshness::Never;
        }
        if let Some(seconds) = directives
            .iter()
            .find_map(|directive| directive.strip_prefix("max-age="))
        {
            return match seconds.trim().parse::<u64>() {
                Ok(0) => Freshness::Never,
                Ok(seconds) => span(Duration::from_secs(seconds)),
                // A max-age nothing can read says nothing, so the system default stands.
                Err(_) => Freshness::Unsaid,
            };
        }
    }
    let Some(expires) = headers
        .get(EXPIRES)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| httpdate::parse_http_date(value).ok())
    else {
        return Freshness::Unsaid;
    };
    match expires.duration_since(now) {
        Ok(left) => span(left),
        Err(_) => Freshness::Never,
    }
}

/// A span as a ttl, keeping nothing when it rounds away to no time at all.
fn span(left: Duration) -> Freshness {
    match Ttl::new(left) {
        Ok(ttl) => Freshness::For(ttl),
        Err(_) => Freshness::Never,
    }
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

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    #[test]
    fn a_response_that_says_nothing_leaves_the_default_in_force() {
        assert_eq!(freshness(&headers(&[]), now()), Freshness::Unsaid);
    }

    #[test]
    fn a_max_age_names_the_span_an_answer_is_kept_for() {
        assert_eq!(
            freshness(&headers(&[("cache-control", "max-age=120")]), now()),
            Freshness::For(Ttl::seconds(120).unwrap())
        );
    }

    #[test]
    fn a_max_age_among_other_directives_is_still_read() {
        assert_eq!(
            freshness(
                &headers(&[("cache-control", "public, max-age=30, must-revalidate")]),
                now()
            ),
            Freshness::For(Ttl::seconds(30).unwrap())
        );
    }

    #[test]
    fn a_response_that_refuses_to_be_stored_is_kept_nowhere() {
        for directive in ["no-store", "no-cache", "private", "public, no-store"] {
            assert_eq!(
                freshness(&headers(&[("cache-control", directive)]), now()),
                Freshness::Never,
                "{directive} was not honoured"
            );
        }
    }

    #[test]
    fn a_max_age_of_zero_is_kept_nowhere() {
        assert_eq!(
            freshness(&headers(&[("cache-control", "max-age=0")]), now()),
            Freshness::Never
        );
    }

    #[test]
    fn a_max_age_nothing_can_read_leaves_the_default_in_force() {
        assert_eq!(
            freshness(&headers(&[("cache-control", "max-age=soon")]), now()),
            Freshness::Unsaid
        );
    }

    #[test]
    fn an_expires_names_the_moment_an_answer_stops_being_good() {
        let expires = httpdate::fmt_http_date(now() + Duration::from_secs(90));
        assert_eq!(
            freshness(&headers(&[("expires", &expires)]), now()),
            Freshness::For(Ttl::new(Duration::from_secs(90)).unwrap())
        );
    }

    #[test]
    fn an_expires_already_past_is_kept_nowhere() {
        let expires = httpdate::fmt_http_date(now() - Duration::from_secs(1));
        assert_eq!(
            freshness(&headers(&[("expires", &expires)]), now()),
            Freshness::Never
        );
    }

    #[test]
    fn an_expires_nothing_can_read_leaves_the_default_in_force() {
        assert_eq!(
            freshness(&headers(&[("expires", "0")]), now()),
            Freshness::Unsaid
        );
    }

    #[test]
    fn a_cache_control_that_names_no_span_leaves_the_expires_in_force() {
        let expires = httpdate::fmt_http_date(now() + Duration::from_secs(90));
        assert_eq!(
            freshness(
                &headers(&[("cache-control", "public"), ("expires", &expires)]),
                now()
            ),
            Freshness::For(Ttl::new(Duration::from_secs(90)).unwrap())
        );
    }

    #[test]
    fn a_cache_control_outranks_an_expires() {
        let expires = httpdate::fmt_http_date(now() + Duration::from_secs(900));
        assert_eq!(
            freshness(
                &headers(&[("cache-control", "max-age=10"), ("expires", &expires)]),
                now()
            ),
            Freshness::For(Ttl::seconds(10).unwrap())
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
