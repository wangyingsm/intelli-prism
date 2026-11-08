//! Where a plane answers, and what it is called.

use std::fmt;

use crate::error::DrsgError;

/// The most characters a plane's name may be, which is generous for a configured identifier.
const NAME_MOST: usize = 128;

/// Where a dr-strange server answers JSON-RPC.
///
/// Validated once, at construction, so every call after it is a string that is already a url.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrsgEndpoint(String);

impl DrsgEndpoint {
    /// The endpoint an operator configured, if it is one this client can post to.
    ///
    /// Only `http` and `https` are taken: a scheme this client cannot speak is a configuration
    /// mistake worth reporting at startup rather than on the first graph run.
    pub fn new(raw: &str) -> Result<Self, DrsgError> {
        let uri = raw
            .parse::<http::Uri>()
            .map_err(|error| DrsgError::Malformed {
                detail: format!("{raw:?} is not a url: {error}"),
            })?;
        match uri.scheme_str() {
            Some("http" | "https") => {}
            other => {
                return Err(DrsgError::Malformed {
                    detail: match other {
                        Some(scheme) => format!("{scheme:?} is not a scheme this speaks"),
                        None => format!("{raw:?} names no scheme"),
                    },
                });
            }
        }
        if uri.host().is_none() {
            return Err(DrsgError::Malformed {
                detail: format!("{raw:?} names no host"),
            });
        }
        Ok(Self(raw.to_owned()))
    }

    /// The endpoint as a string, for the request line.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DrsgEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The name of the one plane a client is bound to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PlaneName(String);

impl PlaneName {
    /// The plane name an operator configured, if it is one a store will take.
    ///
    /// Whitespace is refused rather than trimmed: a name that has to be repaired before it can be
    /// sent is one two deployments will spell differently and address different planes by.
    pub fn new(raw: &str) -> Result<Self, DrsgError> {
        if raw.is_empty() {
            return Err(DrsgError::Malformed {
                detail: "a plane name is empty".to_owned(),
            });
        }
        if raw.len() > NAME_MOST {
            return Err(DrsgError::Malformed {
                detail: format!("a plane name is {} bytes, over the {NAME_MOST}", raw.len()),
            });
        }
        if raw.chars().any(char::is_whitespace) {
            return Err(DrsgError::Malformed {
                detail: format!("the plane name {raw:?} carries whitespace"),
            });
        }
        Ok(Self(raw.to_owned()))
    }

    /// The name as a string, for a request body.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PlaneName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_endpoint_this_client_can_post_to_is_taken() {
        for raw in [
            "http://127.0.0.1:7777/rpc",
            "https://graphs.internal/rpc",
            "http://localhost:7777/rpc",
        ] {
            assert!(DrsgEndpoint::new(raw).is_ok(), "{raw} was refused");
        }
    }

    #[test]
    fn an_endpoint_this_client_cannot_post_to_is_refused() {
        for raw in [
            "",
            "ws://127.0.0.1/rpc",
            "/rpc",
            "127.0.0.1:7777",
            "http:///rpc",
        ] {
            assert!(
                matches!(DrsgEndpoint::new(raw), Err(DrsgError::Malformed { .. })),
                "{raw:?} was taken"
            );
        }
    }

    #[test]
    fn an_endpoint_is_kept_exactly_as_it_was_configured() {
        let endpoint = DrsgEndpoint::new("http://127.0.0.1:7777/rpc").unwrap();
        assert_eq!(endpoint.as_str(), "http://127.0.0.1:7777/rpc");
        assert_eq!(endpoint.to_string(), "http://127.0.0.1:7777/rpc");
    }

    #[test]
    fn a_plane_name_a_store_will_take_is_taken() {
        for raw in ["graphs", "intelli-prism_graphs", "g"] {
            assert!(PlaneName::new(raw).is_ok(), "{raw} was refused");
        }
    }

    #[test]
    fn a_plane_name_is_refused_rather_than_repaired() {
        for raw in [
            "",
            " graphs",
            "graphs ",
            "two words",
            "a\tb",
            &"g".repeat(129),
        ] {
            assert!(
                matches!(PlaneName::new(raw), Err(DrsgError::Malformed { .. })),
                "{raw:?} was taken"
            );
        }
        assert!(
            PlaneName::new(&"g".repeat(128)).is_ok(),
            "the limit itself is allowed"
        );
    }
}
