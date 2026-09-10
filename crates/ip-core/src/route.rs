use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use crate::error::CoreError;
use crate::id::ApiId;

/// The path prefix the gateway keeps for its own endpoints, which no rule may claim.
pub const RESERVED_PATH_PREFIX: &str = "/_ip";

const HOST_MAX_BYTES: usize = 253;
const HOST_LABEL_MAX_BYTES: usize = 63;
const PATH_MAX_BYTES: usize = 2048;

/// The wire protocol one end of a route speaks.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// Plain http.
    Http,
    /// Http over tls.
    Https,
    /// Plain websocket.
    Ws,
    /// Websocket over tls.
    Wss,
    /// Raw tcp, which carries grpc.
    Tcp,
}

impl Protocol {
    /// The bare scheme, as a stored row or a serialized rule holds it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Ws => "ws",
            Self::Wss => "wss",
            Self::Tcp => "tcp",
        }
    }

    /// The scheme as a routing rule spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http://",
            Self::Https => "https://",
            Self::Ws => "ws://",
            Self::Wss => "wss://",
            Self::Tcp => "tcp",
        }
    }

    /// The port used when a rule names none.
    pub fn default_port(self) -> Option<Port> {
        match self {
            Self::Http | Self::Ws => Some(Port(80)),
            Self::Https | Self::Wss => Some(Port(443)),
            Self::Tcp => None,
        }
    }

    /// Whether this build carries requests of this protocol.
    pub fn is_forwarded(self) -> bool {
        matches!(self, Self::Http | Self::Https)
    }

    /// Whether the protocol runs over tls.
    pub fn is_secure(self) -> bool {
        matches!(self, Self::Https | Self::Wss)
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Protocol {
    type Err = CoreError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "http://" | "http" => Ok(Self::Http),
            "https://" | "https" => Ok(Self::Https),
            "ws://" | "ws" => Ok(Self::Ws),
            "wss://" | "wss" => Ok(Self::Wss),
            "tcp" | "tcp://" => Ok(Self::Tcp),
            other => Err(CoreError::UnknownProtocol {
                value: other.to_owned(),
            }),
        }
    }
}

/// A hostname or ip address, held lowercase so routing matches whatever case was sent.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String")]
pub struct Host(Box<str>);

impl Host {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "host";

    /// Validates a hostname or ip address and folds it to lowercase.
    pub fn new(raw: &str) -> Result<Self, CoreError> {
        let folded = raw.to_ascii_lowercase();
        validate_host(&folded)?;
        Ok(Self(folded.into_boxed_str()))
    }

    /// The host as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The host as an ip address, when it is one rather than a name.
    pub fn as_ip(&self) -> Option<IpAddr> {
        self.0.parse().ok()
    }
}

fn validate_host(raw: &str) -> Result<(), CoreError> {
    let kind = Host::KIND;
    if raw.is_empty() {
        return Err(CoreError::Empty { kind });
    }
    if raw.len() > HOST_MAX_BYTES {
        return Err(CoreError::TooLong {
            kind,
            len: raw.len(),
            max: HOST_MAX_BYTES,
        });
    }
    if raw.parse::<IpAddr>().is_ok() {
        return Ok(());
    }
    for label in raw.split('.') {
        if label.is_empty() {
            return Err(CoreError::Empty { kind: "host label" });
        }
        if label.len() > HOST_LABEL_MAX_BYTES {
            return Err(CoreError::TooLong {
                kind: "host label",
                len: label.len(),
                max: HOST_LABEL_MAX_BYTES,
            });
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(CoreError::IllegalChar {
                kind: "host label",
                ch: '-',
            });
        }
        if let Some(ch) = label
            .chars()
            .find(|ch| !(ch.is_ascii_alphanumeric() || *ch == '-'))
        {
            return Err(CoreError::IllegalChar {
                kind: "host label",
                ch,
            });
        }
    }
    Ok(())
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Host {
    type Error = CoreError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::new(&raw)
    }
}

/// A tcp port, which is never zero.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "u16")]
pub struct Port(u16);

impl Port {
    /// Wraps a port after refusing zero.
    pub fn new(value: u16) -> Result<Self, CoreError> {
        if value == 0 {
            return Err(CoreError::ZeroPort);
        }
        Ok(Self(value))
    }

    /// The port as a number.
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl fmt::Display for Port {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl TryFrom<u16> for Port {
    type Error = CoreError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// Narrows the wider integer a database column or wire format hands back.
impl TryFrom<i64> for Port {
    type Error = CoreError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        let narrowed = u16::try_from(value).map_err(|_| CoreError::PortOutOfRange { value })?;
        Self::new(narrowed)
    }
}

/// An absolute path, which is the method name when the protocol is grpc.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String")]
pub struct AbsPath(Box<str>);

impl AbsPath {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "path";

    /// Validates a path that must be absolute and carry no query or fragment.
    pub fn new(raw: &str) -> Result<Self, CoreError> {
        validate_path(raw)?;
        Ok(Self(Box::from(raw)))
    }

    /// The path as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this path lies inside the prefix the gateway keeps for itself.
    pub fn is_reserved(&self) -> bool {
        match self.0.strip_prefix(RESERVED_PATH_PREFIX) {
            Some(rest) => rest.is_empty() || rest.starts_with('/'),
            None => false,
        }
    }
}

fn validate_path(raw: &str) -> Result<(), CoreError> {
    let kind = AbsPath::KIND;
    if raw.is_empty() {
        return Err(CoreError::Empty { kind });
    }
    if !raw.starts_with('/') {
        return Err(CoreError::MustStartWith {
            kind,
            expected: '/',
        });
    }
    if raw.len() > PATH_MAX_BYTES {
        return Err(CoreError::TooLong {
            kind,
            len: raw.len(),
            max: PATH_MAX_BYTES,
        });
    }
    match raw
        .chars()
        .find(|ch| !ch.is_ascii_graphic() || matches!(ch, '?' | '#'))
    {
        Some(ch) => Err(CoreError::IllegalChar { kind, ch }),
        None => Ok(()),
    }
}

impl fmt::Display for AbsPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for AbsPath {
    type Error = CoreError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        validate_path(&raw)?;
        Ok(Self(raw.into_boxed_str()))
    }
}

/// One end of a route: what to speak, where, and at what path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Endpoint {
    /// What the endpoint speaks.
    pub protocol: Protocol,
    /// Where it lives.
    pub host: Host,
    /// Which port it answers on.
    pub port: Port,
    /// The absolute path, or the grpc method name.
    pub path: AbsPath,
}

impl Endpoint {
    /// Assembles an endpoint from parts that are already validated.
    pub fn new(protocol: Protocol, host: Host, port: Port, path: AbsPath) -> Self {
        Self {
            protocol,
            host,
            port,
            path,
        }
    }

    /// Rebuilds an endpoint from the text and numbers a stored row holds.
    pub fn from_parts(
        protocol: &str,
        host: &str,
        port: i64,
        path: &str,
    ) -> Result<Self, CoreError> {
        Ok(Self {
            protocol: protocol.parse()?,
            host: Host::new(host)?,
            port: Port::try_from(port)?,
            path: AbsPath::new(path)?,
        })
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}{}:{}{}",
            self.protocol, self.host, self.port, self.path
        )
    }
}

/// The endpoint a request arrives at, which a rule is looked up by.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct RouteKey(Endpoint);

impl RouteKey {
    /// Wraps the endpoint a request arrived at.
    pub fn new(endpoint: Endpoint) -> Self {
        Self(endpoint)
    }

    /// The endpoint itself.
    pub fn endpoint(&self) -> &Endpoint {
        &self.0
    }
}

impl fmt::Display for RouteKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The endpoints a matched request may be sent to, of which there is always at least one.
///
/// Several endpoints stand behind one target when an upstream is replicated. Which one
/// a request goes to is a dispatch decision the gateway makes, not a property of the rule.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "Vec<Endpoint>")]
pub struct RouteTarget(Vec<Endpoint>);

impl RouteTarget {
    /// Wraps the single endpoint a request is forwarded to.
    pub fn new(endpoint: Endpoint) -> Self {
        Self(vec![endpoint])
    }

    /// Wraps every endpoint standing behind one target, refusing an empty list.
    pub fn from_endpoints(endpoints: Vec<Endpoint>) -> Result<Self, CoreError> {
        if endpoints.is_empty() {
            return Err(CoreError::NoEndpoint);
        }
        Ok(Self(endpoints))
    }

    /// Every endpoint a request may be sent to.
    pub fn endpoints(&self) -> &[Endpoint] {
        &self.0
    }

    /// The endpoint used until a dispatch algorithm chooses between them.
    pub fn primary(&self) -> &Endpoint {
        &self.0[0]
    }
}

impl TryFrom<Vec<Endpoint>> for RouteTarget {
    type Error = CoreError;

    fn try_from(endpoints: Vec<Endpoint>) -> Result<Self, Self::Error> {
        Self::from_endpoints(endpoints)
    }
}

impl fmt::Display for RouteTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rendered: Vec<String> = self.0.iter().map(ToString::to_string).collect();
        f.write_str(&rendered.join(", "))
    }
}

/// A routing rule: what arrives, and where it goes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RouteRule {
    /// The api this rule serves, which api capabilities are granted against.
    pub api: ApiId,
    /// What the rule matches.
    pub key: RouteKey,
    /// Where a match is sent.
    pub target: RouteTarget,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(protocol: Protocol, host: &str, port: u16, path: &str) -> Endpoint {
        Endpoint::new(
            protocol,
            Host::new(host).unwrap(),
            Port::new(port).unwrap(),
            AbsPath::new(path).unwrap(),
        )
    }

    #[test]
    fn a_protocol_reads_back_from_how_a_rule_spells_it() {
        for protocol in [
            Protocol::Http,
            Protocol::Https,
            Protocol::Ws,
            Protocol::Wss,
            Protocol::Tcp,
        ] {
            assert_eq!(protocol.as_str().parse::<Protocol>().unwrap(), protocol);
        }
    }

    #[test]
    fn a_protocol_also_reads_without_the_separator() {
        assert_eq!("https".parse::<Protocol>().unwrap(), Protocol::Https);
    }

    #[test]
    fn an_unknown_protocol_is_refused() {
        assert!(matches!(
            "gopher://".parse::<Protocol>(),
            Err(CoreError::UnknownProtocol { .. })
        ));
    }

    #[test]
    fn only_http_is_forwarded_by_this_build() {
        assert!(Protocol::Http.is_forwarded());
        assert!(Protocol::Https.is_forwarded());
        assert!(!Protocol::Ws.is_forwarded());
        assert!(!Protocol::Wss.is_forwarded());
        assert!(!Protocol::Tcp.is_forwarded());
    }

    #[test]
    fn default_ports_follow_the_scheme() {
        assert_eq!(Protocol::Http.default_port(), Some(Port::new(80).unwrap()));
        assert_eq!(
            Protocol::Https.default_port(),
            Some(Port::new(443).unwrap())
        );
        assert_eq!(Protocol::Wss.default_port(), Some(Port::new(443).unwrap()));
        assert_eq!(Protocol::Tcp.default_port(), None);
        assert!(Protocol::Https.is_secure());
        assert!(!Protocol::Http.is_secure());
    }

    #[test]
    fn a_host_is_folded_so_routing_ignores_case() {
        assert_eq!(
            Host::new("API.Example.COM").unwrap().as_str(),
            "api.example.com"
        );
        assert_eq!(
            Host::new("API.example.com").unwrap(),
            Host::new("api.EXAMPLE.com").unwrap()
        );
    }

    #[test]
    fn a_host_may_be_an_ip_address() {
        assert_eq!(
            Host::new("192.0.2.1").unwrap().as_ip(),
            Some("192.0.2.1".parse().unwrap())
        );
        assert_eq!(
            Host::new("2001:DB8::1").unwrap().as_ip(),
            Some("2001:db8::1".parse().unwrap())
        );
        assert_eq!(Host::new("api.example.com").unwrap().as_ip(), None);
    }

    #[test]
    fn a_host_with_an_empty_label_is_refused() {
        assert_eq!(
            Host::new("api..com"),
            Err(CoreError::Empty { kind: "host label" })
        );
    }

    #[test]
    fn a_host_label_may_not_be_bounded_by_a_hyphen() {
        assert_eq!(
            Host::new("-api.example.com"),
            Err(CoreError::IllegalChar {
                kind: "host label",
                ch: '-',
            })
        );
    }

    #[test]
    fn a_host_with_an_illegal_character_is_refused() {
        assert_eq!(
            Host::new("api_example.com"),
            Err(CoreError::IllegalChar {
                kind: "host label",
                ch: '_',
            })
        );
    }

    #[test]
    fn an_empty_or_over_long_host_is_refused() {
        assert_eq!(Host::new(""), Err(CoreError::Empty { kind: "host" }));
        let raw = "a".repeat(HOST_MAX_BYTES + 1);
        assert!(matches!(
            Host::new(&raw),
            Err(CoreError::TooLong { kind: "host", .. })
        ));
    }

    #[test]
    fn port_zero_is_refused() {
        assert_eq!(Port::new(0), Err(CoreError::ZeroPort));
        assert_eq!(Port::new(8080).unwrap().get(), 8080);
    }

    #[test]
    fn a_path_must_be_absolute() {
        assert_eq!(
            AbsPath::new("v1/messages"),
            Err(CoreError::MustStartWith {
                kind: "path",
                expected: '/',
            })
        );
        assert_eq!(
            AbsPath::new("/v1/messages").unwrap().as_str(),
            "/v1/messages"
        );
    }

    #[test]
    fn the_reserved_prefix_is_recognised_on_a_segment_boundary() {
        assert!(AbsPath::new("/_ip").unwrap().is_reserved());
        assert!(AbsPath::new("/_ip/healthz").unwrap().is_reserved());
        assert!(!AbsPath::new("/_iproute").unwrap().is_reserved());
        assert!(!AbsPath::new("/anthropic").unwrap().is_reserved());
        assert!(!AbsPath::new("/").unwrap().is_reserved());
    }

    #[test]
    fn a_grpc_method_name_is_a_path() {
        assert!(AbsPath::new("/anthropic.Messages/Create").is_ok());
    }

    #[test]
    fn a_path_carries_no_query_or_fragment() {
        for raw in ["/v1?model=x", "/v1#top"] {
            assert!(matches!(
                AbsPath::new(raw),
                Err(CoreError::IllegalChar { kind: "path", .. })
            ));
        }
    }

    #[test]
    fn a_scheme_has_a_bare_name_and_a_written_form() {
        assert_eq!(Protocol::Https.name(), "https");
        assert_eq!(Protocol::Https.as_str(), "https://");
        for protocol in [
            Protocol::Http,
            Protocol::Https,
            Protocol::Ws,
            Protocol::Wss,
            Protocol::Tcp,
        ] {
            assert_eq!(protocol.name().parse::<Protocol>().unwrap(), protocol);
        }
    }

    #[test]
    fn a_port_narrows_from_the_wider_integer_a_row_holds() {
        assert_eq!(Port::try_from(8080_i64).unwrap().get(), 8080);
        assert_eq!(Port::try_from(0_i64), Err(CoreError::ZeroPort));
        assert_eq!(
            Port::try_from(70_000_i64),
            Err(CoreError::PortOutOfRange { value: 70_000 })
        );
        assert_eq!(
            Port::try_from(-1_i64),
            Err(CoreError::PortOutOfRange { value: -1 })
        );
    }

    #[test]
    fn an_endpoint_rebuilds_from_stored_parts() {
        let rebuilt =
            Endpoint::from_parts("https", "API.example.com", 443, "/v1/messages").unwrap();
        assert_eq!(
            rebuilt,
            endpoint(Protocol::Https, "api.example.com", 443, "/v1/messages")
        );
    }

    #[test]
    fn stored_parts_that_are_not_an_endpoint_are_refused() {
        assert!(matches!(
            Endpoint::from_parts("gopher", "api.example.com", 443, "/v1"),
            Err(CoreError::UnknownProtocol { .. })
        ));
        assert!(matches!(
            Endpoint::from_parts("https", "api example.com", 443, "/v1"),
            Err(CoreError::IllegalChar { .. })
        ));
        assert_eq!(
            Endpoint::from_parts("https", "api.example.com", 0, "/v1"),
            Err(CoreError::ZeroPort)
        );
        assert!(matches!(
            Endpoint::from_parts("https", "api.example.com", 443, "v1"),
            Err(CoreError::MustStartWith { .. })
        ));
    }

    #[test]
    fn an_endpoint_renders_as_a_url() {
        let endpoint = endpoint(Protocol::Https, "api.example.com", 443, "/v1/messages");
        assert_eq!(
            endpoint.to_string(),
            "https://api.example.com:443/v1/messages"
        );
    }

    #[test]
    fn a_key_and_a_target_are_not_the_same_type() {
        let key = RouteKey::new(endpoint(Protocol::Https, "gateway.local", 443, "/v1"));
        let target = RouteTarget::new(endpoint(Protocol::Https, "api.example.com", 443, "/v1"));
        let rule = RouteRule {
            api: ApiId::new("gateway").unwrap(),
            key: key.clone(),
            target: target.clone(),
        };
        assert_eq!(rule.key.endpoint().host.as_str(), "gateway.local");
        assert_eq!(rule.target.primary().host.as_str(), "api.example.com");
        assert_eq!(key.to_string(), "https://gateway.local:443/v1");
    }

    #[test]
    fn a_target_may_stand_for_several_endpoints() {
        let target = RouteTarget::from_endpoints(vec![
            endpoint(Protocol::Https, "one.example.com", 443, "/v1"),
            endpoint(Protocol::Https, "two.example.com", 443, "/v1"),
        ])
        .unwrap();
        assert_eq!(target.endpoints().len(), 2);
        assert_eq!(target.primary().host.as_str(), "one.example.com");
        assert_eq!(
            target.to_string(),
            "https://one.example.com:443/v1, https://two.example.com:443/v1"
        );
    }

    #[test]
    fn a_target_that_names_nowhere_is_refused() {
        assert_eq!(
            RouteTarget::from_endpoints(Vec::new()),
            Err(CoreError::NoEndpoint)
        );
        assert!(serde_json::from_str::<RouteTarget>("[]").is_err());
    }

    #[test]
    fn a_single_endpoint_target_still_reads_as_one() {
        let target = RouteTarget::new(endpoint(Protocol::Https, "api.example.com", 443, "/v1"));
        assert_eq!(target.endpoints().len(), 1);
        assert_eq!(target.to_string(), "https://api.example.com:443/v1");
    }

    #[test]
    fn a_key_deserializes_through_validation() {
        let json = r#"{"protocol":"https","host":"API.example.com","port":443,"path":"/v1"}"#;
        let endpoint: Endpoint = serde_json::from_str(json).unwrap();
        assert_eq!(endpoint.host.as_str(), "api.example.com");
        let bad = r#"{"protocol":"https","host":"api.example.com","port":0,"path":"/v1"}"#;
        assert!(serde_json::from_str::<Endpoint>(bad).is_err());
    }
}
