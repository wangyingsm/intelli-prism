use std::fmt;
#[cfg(test)]
use std::ops::Not;

use zeroize::Zeroizing;

use crate::error::McpError;
use crate::id::McpId;

/// The longest a name may be, in bytes.
const NAME_MAX_BYTES: usize = 64;

/// The longest an endpoint url may be, in bytes.
const ENDPOINT_MAX_BYTES: usize = 1024;

/// The longest a token may be, in bytes.
const TOKEN_MAX_BYTES: usize = 4096;

/// What a server is called, for somebody reading a list of them.
///
/// A label rather than a key: it may carry spaces and any language a person writes in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct McpName(Box<str>);

impl McpName {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "mcp name";

    /// Validates and wraps a name.
    pub fn new(raw: &str) -> Result<Self, McpError> {
        Self::try_from(raw.to_owned())
    }

    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for McpName {
    type Error = McpError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(McpError::Empty { kind: Self::KIND });
        }
        if raw.len() > NAME_MAX_BYTES {
            return Err(McpError::TooLong {
                kind: Self::KIND,
                len: raw.len(),
                max: NAME_MAX_BYTES,
            });
        }
        // Refused rather than trimmed: a store that quietly alters what it was given hands back
        // something the operator did not write.
        if raw.trim() != raw {
            return Err(McpError::Untrimmed { kind: Self::KIND });
        }
        match raw.chars().find(|ch| ch.is_control()) {
            Some(ch) => Err(McpError::IllegalChar {
                kind: Self::KIND,
                ch,
            }),
            None => Ok(Self(raw.into_boxed_str())),
        }
    }
}

impl fmt::Display for McpName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a server answers: an `http` or `https` url with a host.
///
/// Checked for shape and for nothing else. Nothing is contacted here, the path is whatever the
/// operator wrote, and a url that parses is not a url that answers.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct McpEndpoint(Box<str>);

impl McpEndpoint {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "mcp endpoint";

    /// Validates and wraps an endpoint url.
    pub fn new(raw: &str) -> Result<Self, McpError> {
        Self::try_from(raw.to_owned())
    }

    /// The url as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether it is reached over tls.
    pub fn is_secure(&self) -> bool {
        self.0.starts_with("https://")
    }
}

impl TryFrom<String> for McpEndpoint {
    type Error = McpError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.len() > ENDPOINT_MAX_BYTES {
            return Err(McpError::TooLong {
                kind: Self::KIND,
                len: raw.len(),
                max: ENDPOINT_MAX_BYTES,
            });
        }
        let host = match raw.split_once("://") {
            Some(("http" | "https", rest)) => rest,
            _ => {
                return Err(McpError::NotAnEndpoint { value: raw });
            }
        };
        // A host, not merely a scheme and a path: `http:///tools` parses as a url in some
        // readings and names nothing this gateway can call.
        let named = host.split(['/', '?', '#']).next().unwrap_or_default();
        if named.is_empty() || named.chars().any(char::is_whitespace) {
            return Err(McpError::NotAnEndpoint { value: raw });
        }
        Ok(Self(raw.into_boxed_str()))
    }
}

impl fmt::Display for McpEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A bearer token a server requires.
///
/// **It is a credential, and this type is shaped so it cannot leak by accident.** It does not
/// implement `Serialize`, so no view and no json body can carry it without somebody writing the
/// code to do so on purpose; its `Debug` prints nothing of the value; and it is zeroized when
/// dropped. Reading it takes [`McpToken::expose`], which is deliberately awkward to type.
#[derive(Clone, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct McpToken(Zeroizing<String>);

impl McpToken {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "mcp token";

    /// Validates and wraps a token.
    pub fn new(raw: &str) -> Result<Self, McpError> {
        Self::try_from(raw.to_owned())
    }

    /// The token itself, for presenting to the server it belongs to.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for McpToken {
    type Error = McpError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(McpError::Empty { kind: Self::KIND });
        }
        if raw.len() > TOKEN_MAX_BYTES {
            return Err(McpError::TooLong {
                kind: Self::KIND,
                len: raw.len(),
                max: TOKEN_MAX_BYTES,
            });
        }
        // A header is what it will be presented in, and a header holds no control character.
        match raw.chars().find(|ch| ch.is_control()) {
            Some(ch) => Err(McpError::IllegalChar {
                kind: Self::KIND,
                ch,
            }),
            None => Ok(Self(Zeroizing::new(raw))),
        }
    }
}

impl fmt::Debug for McpToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("McpToken(…)")
    }
}

/// Equality over the value, for comparing a token read back against the one written.
///
/// Not an authentication check: nothing here is presented to this gateway, so there is no secret
/// of ours to compare in constant time.
impl PartialEq for McpToken {
    fn eq(&self, other: &Self) -> bool {
        *self.0 == *other.0
    }
}

impl Eq for McpToken {}

/// What a server requires before it will answer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpAuth {
    /// Nothing: the endpoint is reached as it stands.
    None,
    /// A bearer token, presented as `Authorization: Bearer …`.
    Bearer {
        /// The token.
        token: McpToken,
    },
}

impl McpAuth {
    /// The name this kind is stored and reported under.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Bearer { .. } => "bearer",
        }
    }

    /// The token, where there is one.
    pub const fn token(&self) -> Option<&McpToken> {
        match self {
            Self::None => None,
            Self::Bearer { token } => Some(token),
        }
    }
}

/// One MCP server: which it is, what it is called, where it answers and what it requires.
///
/// **Nothing here is checked against the server itself.** No handshake is made, no tool is
/// listed, and an endpoint that answers nothing is as storable as one that works. What a server
/// really offers is asked of it when a run needs to know.
///
/// It does not implement `Serialize`, because [`McpToken`] does not: a server is written to
/// storage field by field, and what the api reports is built on purpose rather than derived.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    /// Which server this is.
    pub id: McpId,
    /// What it is called.
    pub name: McpName,
    /// Where it answers.
    pub endpoint: McpEndpoint,
    /// What it requires; nothing when left out.
    #[serde(default = "McpAuth::none")]
    pub auth: McpAuth,
}

impl McpAuth {
    /// Requiring nothing, which is what a server that says nothing requires.
    const fn none() -> Self {
        Self::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(auth: McpAuth) -> McpServer {
        McpServer {
            id: McpId::new("warehouse").unwrap(),
            name: McpName::new("The warehouse").unwrap(),
            endpoint: McpEndpoint::new("https://mcp.internal/warehouse").unwrap(),
            auth,
        }
    }

    #[test]
    fn a_name_takes_what_a_person_writes() {
        for raw in ["The warehouse", "仓库", "tier-2 (EU)"] {
            assert_eq!(McpName::new(raw).unwrap().as_str(), raw);
        }
        assert_eq!(McpName::new("Ware").unwrap().to_string(), "Ware");
    }

    #[test]
    fn a_name_is_refused_empty_padded_and_past_its_ceiling() {
        assert_eq!(
            McpName::new(""),
            Err(McpError::Empty {
                kind: McpName::KIND
            })
        );
        assert!(matches!(
            McpName::new(&"a".repeat(NAME_MAX_BYTES + 1)),
            Err(McpError::TooLong { .. })
        ));
        for raw in [" Ware", "Ware ", "\tWare"] {
            assert_eq!(
                McpName::new(raw),
                Err(McpError::Untrimmed {
                    kind: McpName::KIND
                }),
                "{raw:?}"
            );
        }
        assert_eq!(
            McpName::new("one\u{7}two"),
            Err(McpError::IllegalChar {
                kind: McpName::KIND,
                ch: '\u{7}'
            })
        );
    }

    #[test]
    fn an_endpoint_takes_an_http_or_https_url_with_a_host() {
        for raw in [
            "https://mcp.internal/warehouse",
            "http://127.0.0.1:8931/mcp",
            "https://mcp.internal",
            "https://mcp.internal/warehouse?v=2",
        ] {
            assert_eq!(McpEndpoint::new(raw).unwrap().as_str(), raw, "{raw:?}");
        }
        assert!(
            McpEndpoint::new("http://127.0.0.1:8931/mcp")
                .unwrap()
                .is_secure()
                .not()
        );
        assert!(
            McpEndpoint::new("https://mcp.internal")
                .unwrap()
                .is_secure()
        );
    }

    #[test]
    fn an_endpoint_refuses_what_this_gateway_will_not_call() {
        for raw in [
            "",
            "mcp.internal/warehouse",
            "ws://mcp.internal/warehouse",
            "file:///etc/passwd",
            "https://",
            "http:///warehouse",
            "https:// mcp.internal",
        ] {
            assert_eq!(
                McpEndpoint::new(raw),
                Err(McpError::NotAnEndpoint {
                    value: raw.to_owned()
                }),
                "{raw:?}"
            );
        }
        assert!(matches!(
            McpEndpoint::new(&format!("https://{}", "a".repeat(ENDPOINT_MAX_BYTES))),
            Err(McpError::TooLong { .. })
        ));
    }

    /// The point of the type: a token cannot reach a log, a response or a panic message by
    /// accident, only on purpose.
    #[test]
    fn a_token_says_nothing_about_itself() {
        let token = McpToken::new("sk-warehouse-0123456789").unwrap();
        assert_eq!(format!("{token:?}"), "McpToken(…)");
        assert!(!format!("{token:?}").contains("warehouse"));
        assert_eq!(token.expose(), "sk-warehouse-0123456789");
        let held = server(McpAuth::Bearer {
            token: token.clone(),
        });
        assert!(
            !format!("{held:?}").contains("0123456789"),
            "a server holding one says nothing either: {held:?}"
        );
    }

    #[test]
    fn a_token_is_refused_empty_oversized_and_unheaderable() {
        assert_eq!(
            McpToken::new(""),
            Err(McpError::Empty {
                kind: McpToken::KIND
            })
        );
        assert!(matches!(
            McpToken::new(&"a".repeat(TOKEN_MAX_BYTES + 1)),
            Err(McpError::TooLong { .. })
        ));
        assert_eq!(
            McpToken::new("sk-one\ntwo"),
            Err(McpError::IllegalChar {
                kind: McpToken::KIND,
                ch: '\n'
            })
        );
    }

    #[test]
    fn two_tokens_are_the_same_when_their_values_are() {
        assert_eq!(
            McpToken::new("sk-a").unwrap(),
            McpToken::new("sk-a").unwrap()
        );
        assert_ne!(
            McpToken::new("sk-a").unwrap(),
            McpToken::new("sk-b").unwrap()
        );
    }

    #[test]
    fn an_auth_names_itself_and_hands_over_its_token() {
        let none = McpAuth::None;
        assert_eq!(none.name(), "none");
        assert!(none.token().is_none());
        let bearer = McpAuth::Bearer {
            token: McpToken::new("sk-a").unwrap(),
        };
        assert_eq!(bearer.name(), "bearer");
        assert_eq!(bearer.token().unwrap().expose(), "sk-a");
    }

    #[test]
    fn a_server_is_read_from_what_an_operator_writes() {
        let written = r#"{
            "id": "warehouse",
            "name": "The warehouse",
            "endpoint": "https://mcp.internal/warehouse",
            "auth": {"kind": "bearer", "token": "sk-warehouse"}
        }"#;
        let held: McpServer = serde_json::from_str(written).unwrap();
        assert_eq!(
            held,
            server(McpAuth::Bearer {
                token: McpToken::new("sk-warehouse").unwrap()
            })
        );
    }

    #[test]
    fn a_server_that_says_nothing_about_auth_requires_nothing() {
        let written = r#"{"id":"warehouse","name":"The warehouse","endpoint":"https://mcp.internal/warehouse"}"#;
        let held: McpServer = serde_json::from_str(written).unwrap();
        assert_eq!(held, server(McpAuth::None));
        assert!(held.auth.token().is_none());
    }

    #[test]
    fn a_server_refuses_a_field_this_code_does_not_carry() {
        let written = r#"{"id":"warehouse","name":"The warehouse","endpoint":"https://mcp.internal/warehouse","tenant":"acme"}"#;
        let error = serde_json::from_str::<McpServer>(written).unwrap_err();
        assert!(error.to_string().contains("tenant"), "{error}");
    }

    #[test]
    fn a_server_refuses_a_part_its_own_type_refuses() {
        for (written, says) in [
            (
                r#"{"id":"ware house","name":"W","endpoint":"https://m.internal"}"#,
                "may not carry",
            ),
            (
                r#"{"id":"warehouse","name":" W","endpoint":"https://m.internal"}"#,
                "whitespace",
            ),
            (
                r#"{"id":"warehouse","name":"W","endpoint":"ws://m.internal"}"#,
                "http or https",
            ),
            (
                r#"{"id":"warehouse","name":"W","endpoint":"https://m.internal","auth":{"kind":"bearer","token":""}}"#,
                "is empty",
            ),
        ] {
            let error = serde_json::from_str::<McpServer>(written).unwrap_err();
            assert!(error.to_string().contains(says), "{says} not in {error}");
        }
    }

    /// A kind this build does not carry is refused by name, rather than read as the one it
    /// happens to resemble.
    #[test]
    fn an_auth_kind_this_code_does_not_know_is_refused() {
        let written = r#"{"id":"warehouse","name":"W","endpoint":"https://m.internal","auth":{"kind":"mtls"}}"#;
        let error = serde_json::from_str::<McpServer>(written).unwrap_err();
        assert!(error.to_string().contains("mtls"), "{error}");
    }
}
