use axum::http::HeaderMap;
use axum::http::header::{COOKIE, HeaderValue};
use ip_auth::SessionToken;

/// The cookie a logged in browser carries its session in.
const NAME: &str = "ip_session";

/// The cookie a login sets: readable by no script, sent over tls only, and never on a
/// request another site started.
///
/// `SameSite=Strict` is what keeps a page elsewhere from spending this session, and
/// `HttpOnly` is what keeps a script that reaches the page from reading it.
pub fn set(token: &SessionToken, seconds: u64) -> HeaderValue {
    header(&format!("{NAME}={}; Max-Age={seconds}", token.as_str()))
}

/// The cookie a logout sets, which replaces the session with nothing and expires at once.
pub fn clear() -> HeaderValue {
    header(&format!("{NAME}=; Max-Age=0"))
}

fn header(start: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{start}; Path=/; HttpOnly; Secure; SameSite=Strict"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// The session token a request carries, if it carries one.
pub fn token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| name.trim() == NAME)
        .map(|(_, token)| token.trim())
        .filter(|token| !token.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn carrying(cookies: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for cookie in cookies {
            headers.append(COOKIE, HeaderValue::from_str(cookie).unwrap());
        }
        headers
    }

    #[test]
    fn a_login_sets_a_cookie_no_script_and_no_other_site_can_use() {
        let token = SessionToken::from_str_for_test("a.b.c");
        let cookie = set(&token, 3600).to_str().unwrap().to_owned();
        assert!(cookie.starts_with("ip_session=a.b.c;"));
        for attribute in [
            "Path=/",
            "HttpOnly",
            "Secure",
            "SameSite=Strict",
            "Max-Age=3600",
        ] {
            assert!(
                cookie.contains(attribute),
                "{attribute} is missing from {cookie}"
            );
        }
    }

    #[test]
    fn a_logout_replaces_the_session_with_nothing() {
        let cookie = clear().to_str().unwrap().to_owned();
        assert!(cookie.starts_with("ip_session=;"));
        assert!(cookie.contains("Max-Age=0"));
    }

    #[test]
    fn the_session_is_read_out_of_whatever_else_the_browser_sends() {
        let headers = carrying(&["theme=dark; ip_session=a.b.c; lang=en"]);
        assert_eq!(token(&headers), Some("a.b.c"));
    }

    #[test]
    fn a_session_in_a_second_cookie_header_is_still_found() {
        let headers = carrying(&["theme=dark", "ip_session=a.b.c"]);
        assert_eq!(token(&headers), Some("a.b.c"));
    }

    #[test]
    fn a_request_carrying_no_session_carries_none() {
        assert_eq!(token(&carrying(&["theme=dark"])), None);
        assert_eq!(token(&carrying(&[])), None);
        assert_eq!(token(&carrying(&["ip_session="])), None);
    }

    #[test]
    fn a_cookie_whose_name_only_ends_in_the_session_is_not_the_session() {
        assert_eq!(token(&carrying(&["not_ip_session=a.b.c"])), None);
    }
}
