use std::collections::HashMap;
use std::net::SocketAddr;

use ip_config::Config;
use ip_core::{AbsPath, Endpoint, Host, Port, Protocol, RouteKey, RouteRule};
use ip_storage::RouteStore;

use crate::error::RouteError;

/// The exactly matched part of a key: everything but the path.
type Authority = (Protocol, Host, Port);

/// Where a matched request is sent, and the rule that sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    rule: RouteRule,
    target: Endpoint,
}

impl Resolution {
    /// The endpoint to forward to, with the unmatched path remainder already appended.
    pub fn target(&self) -> &Endpoint {
        &self.target
    }

    /// The rule that matched.
    pub fn rule(&self) -> &RouteRule {
        &self.rule
    }
}

/// Every routing rule in force, from the configuration file and from the database.
#[derive(Debug, Clone, Default)]
pub struct RoutingTable {
    /// Rules grouped by the part of the key that matches exactly, each group ordered
    /// longest path first so the first match found is the longest one.
    by_authority: HashMap<Authority, Vec<RouteRule>>,
}

impl RoutingTable {
    /// Builds the table from configured upstreams and stored rules. Config wins:
    /// a stored rule under a key an upstream already claims is ignored.
    pub fn build(config: &Config, dynamic: Vec<RouteRule>) -> Result<Self, RouteError> {
        let listen = config.server.listen;
        let mut rules = Vec::with_capacity(config.upstreams.len() + dynamic.len());
        for upstream in &config.upstreams {
            rules.push(upstream.route_rule(listen)?);
        }
        for rule in dynamic {
            if !rules.iter().any(|configured| configured.key == rule.key) {
                rules.push(rule);
            }
        }
        Ok(Self::from_rules(rules))
    }

    /// Reads the stored rules and builds the table around them.
    pub async fn load(config: &Config, store: &dyn RouteStore) -> Result<Self, RouteError> {
        Self::build(config, store.routes().await?)
    }

    fn from_rules(rules: Vec<RouteRule>) -> Self {
        let mut by_authority: HashMap<Authority, Vec<RouteRule>> = HashMap::new();
        for rule in rules {
            by_authority
                .entry(authority(rule.key.endpoint()))
                .or_default()
                .push(rule);
        }
        for group in by_authority.values_mut() {
            group.sort_by(|left, right| {
                right
                    .key
                    .endpoint()
                    .path
                    .as_str()
                    .len()
                    .cmp(&left.key.endpoint().path.as_str().len())
            });
        }
        Self { by_authority }
    }

    /// The rule that carries this request, if any.
    ///
    /// Costs a linear scan of one authority group. Replace with a path trie before a
    /// group holds more than a few thousand rules.
    pub fn resolve(&self, key: &RouteKey) -> Option<Resolution> {
        let requested = key.endpoint();
        let group = self.by_authority.get(&authority(requested))?;
        for rule in group {
            let Some(remainder) =
                remainder_of(rule.key.endpoint().path.as_str(), requested.path.as_str())
            else {
                continue;
            };
            let target = rule.target.endpoint();
            let path = AbsPath::new(&join(target.path.as_str(), remainder)).ok()?;
            return Some(Resolution {
                rule: rule.clone(),
                target: Endpoint::new(target.protocol, target.host.clone(), target.port, path),
            });
        }
        None
    }

    /// How many rules are in force.
    pub fn len(&self) -> usize {
        self.by_authority.values().map(Vec::len).sum()
    }

    /// Whether the gateway would route nothing.
    pub fn is_empty(&self) -> bool {
        self.by_authority.is_empty()
    }

    /// Every rule in force, in no particular order.
    pub fn rules(&self) -> impl Iterator<Item = &RouteRule> {
        self.by_authority.values().flatten()
    }
}

fn authority(endpoint: &Endpoint) -> Authority {
    (endpoint.protocol, endpoint.host.clone(), endpoint.port)
}

/// The part of the request path the rule did not claim, or `None` when it does not match.
/// A rule only matches on a segment boundary, so `/anthropic` never claims `/anthropicx`.
fn remainder_of<'a>(rule: &str, requested: &'a str) -> Option<&'a str> {
    if rule == "/" {
        return Some(requested);
    }
    let rule = rule.strip_suffix('/').unwrap_or(rule);
    if requested == rule {
        return Some("");
    }
    requested
        .strip_prefix(rule)
        .filter(|rest| rest.starts_with('/'))
}

fn join(target: &str, remainder: &str) -> String {
    if remainder.is_empty() {
        return target.to_owned();
    }
    let base = target.strip_suffix('/').unwrap_or(target);
    format!("{base}{remainder}")
}

/// The key a request arriving at `listen` for `host` and `path` is looked up by.
pub fn request_key(
    protocol: Protocol,
    host: &str,
    listen: SocketAddr,
    path: &str,
) -> Result<RouteKey, ip_core::CoreError> {
    let (host, port) = split_authority(host, listen.port());
    Ok(RouteKey::new(Endpoint::new(
        protocol,
        Host::new(host)?,
        Port::new(port)?,
        AbsPath::new(path)?,
    )))
}

/// Splits a `Host` header, which brackets an ipv6 literal and may omit the port.
fn split_authority(raw: &str, default_port: u16) -> (&str, u16) {
    if let Some(rest) = raw.strip_prefix('[')
        && let Some((inside, after)) = rest.split_once(']')
    {
        let port = after
            .strip_prefix(':')
            .and_then(|port| port.parse().ok())
            .unwrap_or(default_port);
        return (inside, port);
    }
    match raw.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') => (name, port.parse().unwrap_or(default_port)),
        _ => (raw, default_port),
    }
}

#[cfg(test)]
mod tests {
    use ip_core::{RouteKey, RouteTarget};

    use super::*;

    const CONFIG: &str = r#"
[server]
listen = "127.0.0.1:8080"

[storage]
backend = "sqlite"
path = "./test.db"

[auth.jwt]
issuer = "intelli-prism"
secret = "0123456789abcdef0123456789abcdef"

[[upstream]]
id = "anthropic"
base_url = "https://api.anthropic.com/v1"
api_key = "sk-x"
model = "claude-opus-5"
"#;

    fn endpoint(protocol: Protocol, host: &str, port: u16, path: &str) -> Endpoint {
        Endpoint::new(
            protocol,
            Host::new(host).unwrap(),
            Port::new(port).unwrap(),
            AbsPath::new(path).unwrap(),
        )
    }

    fn rule(key_path: &str, target_host: &str, target_path: &str) -> RouteRule {
        RouteRule {
            key: RouteKey::new(endpoint(Protocol::Http, "127.0.0.1", 8080, key_path)),
            target: RouteTarget::new(endpoint(Protocol::Https, target_host, 443, target_path)),
        }
    }

    fn key(path: &str) -> RouteKey {
        RouteKey::new(endpoint(Protocol::Http, "127.0.0.1", 8080, path))
    }

    fn table(rules: Vec<RouteRule>) -> RoutingTable {
        RoutingTable::from_rules(rules)
    }

    #[test]
    fn an_exact_path_resolves_to_the_target_untouched() {
        let table = table(vec![rule("/anthropic", "api.example.com", "/v1")]);
        let resolved = table.resolve(&key("/anthropic")).unwrap();
        assert_eq!(
            resolved.target().to_string(),
            "https://api.example.com:443/v1"
        );
    }

    #[test]
    fn a_deeper_path_carries_its_remainder_to_the_target() {
        let table = table(vec![rule("/anthropic", "api.example.com", "/v1")]);
        let resolved = table.resolve(&key("/anthropic/messages")).unwrap();
        assert_eq!(
            resolved.target().to_string(),
            "https://api.example.com:443/v1/messages"
        );
    }

    #[test]
    fn the_longest_matching_rule_wins() {
        let table = table(vec![
            rule("/anthropic", "api.example.com", "/v1"),
            rule("/anthropic/beta", "beta.example.com", "/v1"),
        ]);
        let resolved = table.resolve(&key("/anthropic/beta/messages")).unwrap();
        assert_eq!(
            resolved.target().to_string(),
            "https://beta.example.com:443/v1/messages"
        );
        let resolved = table.resolve(&key("/anthropic/messages")).unwrap();
        assert_eq!(
            resolved.target().to_string(),
            "https://api.example.com:443/v1/messages"
        );
    }

    #[test]
    fn a_rule_only_claims_whole_segments() {
        let table = table(vec![rule("/anthropic", "api.example.com", "/v1")]);
        assert!(table.resolve(&key("/anthropicx")).is_none());
        assert!(table.resolve(&key("/anthropic-beta")).is_none());
    }

    #[test]
    fn a_root_rule_carries_the_whole_path() {
        let table = table(vec![rule("/", "api.example.com", "/v1")]);
        let resolved = table.resolve(&key("/messages")).unwrap();
        assert_eq!(
            resolved.target().to_string(),
            "https://api.example.com:443/v1/messages"
        );
    }

    #[test]
    fn a_target_at_the_root_gains_no_double_slash() {
        let table = table(vec![rule("/anthropic", "api.example.com", "/")]);
        let resolved = table.resolve(&key("/anthropic/messages")).unwrap();
        assert_eq!(
            resolved.target().to_string(),
            "https://api.example.com:443/messages"
        );
    }

    #[test]
    fn a_trailing_slash_on_a_rule_changes_nothing() {
        let table = table(vec![rule("/anthropic/", "api.example.com", "/v1")]);
        let resolved = table.resolve(&key("/anthropic/messages")).unwrap();
        assert_eq!(
            resolved.target().to_string(),
            "https://api.example.com:443/v1/messages"
        );
    }

    #[test]
    fn another_authority_does_not_match() {
        let table = table(vec![rule("/anthropic", "api.example.com", "/v1")]);
        let elsewhere = RouteKey::new(endpoint(Protocol::Http, "other.local", 8080, "/anthropic"));
        let wrong_port = RouteKey::new(endpoint(Protocol::Http, "127.0.0.1", 9090, "/anthropic"));
        let wrong_scheme =
            RouteKey::new(endpoint(Protocol::Https, "127.0.0.1", 8080, "/anthropic"));
        assert!(table.resolve(&elsewhere).is_none());
        assert!(table.resolve(&wrong_port).is_none());
        assert!(table.resolve(&wrong_scheme).is_none());
    }

    #[test]
    fn an_empty_table_resolves_nothing() {
        let table = RoutingTable::default();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        assert!(table.resolve(&key("/anthropic")).is_none());
    }

    #[test]
    fn a_configured_upstream_becomes_a_rule() {
        let config = Config::parse(CONFIG).unwrap();
        let table = RoutingTable::build(&config, Vec::new()).unwrap();
        assert_eq!(table.len(), 1);
        let resolved = table.resolve(&key("/anthropic/messages")).unwrap();
        assert_eq!(
            resolved.target().to_string(),
            "https://api.anthropic.com:443/v1/messages"
        );
    }

    #[test]
    fn a_stored_rule_joins_the_configured_ones() {
        let config = Config::parse(CONFIG).unwrap();
        let stored = rule("/internal", "llm.corp", "/v1");
        let table = RoutingTable::build(&config, vec![stored]).unwrap();
        assert_eq!(table.len(), 2);
        assert!(table.resolve(&key("/internal/chat")).is_some());
        assert!(table.resolve(&key("/anthropic/messages")).is_some());
    }

    #[test]
    fn config_wins_where_both_sources_claim_a_key() {
        let config = Config::parse(CONFIG).unwrap();
        let hijack = rule("/anthropic", "attacker.example", "/v1");
        let table = RoutingTable::build(&config, vec![hijack]).unwrap();
        assert_eq!(table.len(), 1);
        let resolved = table.resolve(&key("/anthropic")).unwrap();
        assert_eq!(
            resolved.target().host.as_str(),
            "api.anthropic.com",
            "the configured target must survive a stored rule under the same key"
        );
    }

    #[test]
    fn the_matching_rule_is_reported_alongside_the_target() {
        let table = table(vec![rule("/anthropic", "api.example.com", "/v1")]);
        let resolved = table.resolve(&key("/anthropic/messages")).unwrap();
        assert_eq!(resolved.rule().key.endpoint().path.as_str(), "/anthropic");
    }

    #[test]
    fn a_host_header_may_carry_a_port_or_not() {
        let listen: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        assert_eq!(
            request_key(Protocol::Http, "gateway.local:9090", listen, "/v1")
                .unwrap()
                .to_string(),
            "http://gateway.local:9090/v1"
        );
        assert_eq!(
            request_key(Protocol::Http, "gateway.local", listen, "/v1")
                .unwrap()
                .to_string(),
            "http://gateway.local:8080/v1"
        );
    }

    #[test]
    fn a_host_header_may_bracket_an_ipv6_literal() {
        let listen: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        assert_eq!(
            request_key(Protocol::Http, "[2001:db8::1]:9090", listen, "/v1")
                .unwrap()
                .to_string(),
            "http://2001:db8::1:9090/v1"
        );
        assert_eq!(
            request_key(Protocol::Http, "[2001:db8::1]", listen, "/v1")
                .unwrap()
                .endpoint()
                .port
                .get(),
            8080
        );
    }
}
