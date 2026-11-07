use std::collections::HashMap;
use std::net::SocketAddr;

use ip_config::{Config, Secret};
use ip_core::{AbsPath, Endpoint, Host, Port, Protocol, RouteKey, RouteRule, Weighted};
use std::sync::Arc;

use crate::dispatch::Plan;
use ip_storage::RouteStore;

use crate::error::RouteError;

/// The exactly matched part of a key: everything but the path.
type Authority = (Protocol, Host, Port);

/// Where a matched request may be sent, and the rule that sent it.
#[derive(Debug, Clone)]
pub struct Resolution {
    rule: RouteRule,
    targets: Vec<Weighted>,
    plan: Arc<Plan>,
    credential: Option<Arc<Secret>>,
}

impl Resolution {
    /// Every endpoint the request may go to, each with the unmatched path remainder already
    /// appended and the share it takes. Which one it goes to is a dispatch decision.
    pub fn targets(&self) -> &[Weighted] {
        &self.targets
    }

    /// The first endpoint, which is where a rule with one goes.
    pub fn primary(&self) -> &Endpoint {
        &self.targets[0].endpoint
    }

    /// The rule that matched.
    pub fn rule(&self) -> &RouteRule {
        &self.rule
    }

    /// How this rule dispatches, worked out when the table was built.
    pub fn plan(&self) -> &Arc<Plan> {
        &self.plan
    }

    /// The credential the deployment holds for this rule's upstream, where it configured one.
    ///
    /// Only a configured upstream has one: a stored rule is a route somebody added over the
    /// api and names no credential, and nothing invents one for it.
    ///
    /// It is read by a request the deployment itself makes, such as an agent turn, which has no
    /// caller to borrow a key from. A proxied request presents whatever its own caller sent and
    /// this is not reached for it.
    pub fn credential(&self) -> Option<&Secret> {
        self.credential.as_deref()
    }
}

/// Every routing rule in force, from the configuration file and from the database.
#[derive(Debug, Clone, Default)]
pub struct RoutingTable {
    /// Rules grouped by the part of the key that matches exactly, each group ordered
    /// longest path first so the first match found is the longest one.
    by_authority: HashMap<Authority, Vec<Planned>>,
}

/// A rule and what a dispatcher needs to know about its endpoints.
#[derive(Debug, Clone)]
struct Planned {
    rule: RouteRule,
    plan: Arc<Plan>,
    credential: Option<Arc<Secret>>,
}

impl From<RouteRule> for Planned {
    /// A rule carrying no credential, which is every rule that did not come from the file.
    fn from(rule: RouteRule) -> Self {
        let plan = Arc::new(Plan::of(rule.target.weighted()));
        Self {
            rule,
            plan,
            credential: None,
        }
    }
}

/// What the configuration file contributes to the table: its rules, and the credential each
/// one's upstream presents.
///
/// The two travel together because a reload rebuilds the table from the configured rules it
/// kept and the stored rules it just read. Keeping the credentials anywhere else means a
/// reload that quietly drops them, and a run that worked until the first rule change.
#[derive(Debug, Clone, Default)]
pub struct Configured {
    rules: Vec<RouteRule>,
    credentials: HashMap<RouteKey, Arc<Secret>>,
}

impl Configured {
    /// Reads the upstreams a configuration declares.
    pub fn of(config: &Config) -> Result<Self, RouteError> {
        let listen = config.server.listen;
        let mut rules = Vec::with_capacity(config.upstreams.len());
        let mut credentials = HashMap::new();
        for upstream in &config.upstreams {
            let rule = upstream.route_rule(listen)?;
            if !upstream.api_key.is_empty() {
                credentials.insert(rule.key.clone(), Arc::new(upstream.api_key.clone()));
            }
            rules.push(rule);
        }
        Ok(Self { rules, credentials })
    }

    /// The rules, as a table assembles them.
    pub fn rules(&self) -> &[RouteRule] {
        &self.rules
    }

    /// The credential the upstream behind one rule presents, where it declared one.
    fn credential(&self, key: &RouteKey) -> Option<Arc<Secret>> {
        self.credentials.get(key).map(Arc::clone)
    }
}

impl From<Vec<RouteRule>> for Configured {
    /// Rules with no credentials, which is what a test and an example contribute.
    fn from(rules: Vec<RouteRule>) -> Self {
        Self {
            rules,
            credentials: HashMap::new(),
        }
    }
}

impl RoutingTable {
    /// Builds the table from configured upstreams and stored rules. Config wins:
    /// a stored rule under a key an upstream already claims is ignored.
    pub fn build(config: &Config, dynamic: Vec<RouteRule>) -> Result<Self, RouteError> {
        Self::assemble(&Configured::of(config)?, dynamic)
    }

    /// Builds the table from rules the configuration already contributed and stored ones, as
    /// a reload does when it has both in hand. Config wins, the same way.
    pub fn assemble(configured: &Configured, dynamic: Vec<RouteRule>) -> Result<Self, RouteError> {
        let mut rules = Vec::with_capacity(configured.rules.len() + dynamic.len());
        rules.extend(configured.rules.iter().cloned());
        for rule in dynamic {
            if !rules.iter().any(|configured| configured.key == rule.key) {
                rules.push(rule);
            }
        }
        for rule in &rules {
            let path = &rule.key.endpoint().path;
            if path.is_reserved() {
                return Err(RouteError::ReservedPath { path: path.clone() });
            }
        }
        Ok(Self::from_planned(
            rules
                .into_iter()
                .map(|rule| Planned {
                    credential: configured.credential(&rule.key),
                    ..Planned::from(rule)
                })
                .collect(),
        ))
    }

    /// Reads the stored rules and builds the table around them.
    pub async fn load(config: &Config, store: &dyn RouteStore) -> Result<Self, RouteError> {
        Self::build(config, store.routes().await?)
    }

    #[cfg(test)]
    pub(crate) fn from_rules(rules: Vec<RouteRule>) -> Self {
        Self::from_planned(rules.into_iter().map(Planned::from).collect())
    }

    fn from_planned(planned: Vec<Planned>) -> Self {
        let mut by_authority: HashMap<Authority, Vec<Planned>> = HashMap::new();
        for rule in planned {
            by_authority
                .entry(authority(rule.rule.key.endpoint()))
                .or_default()
                .push(rule);
        }
        for group in by_authority.values_mut() {
            group.sort_by(|left, right| {
                right
                    .rule
                    .key
                    .endpoint()
                    .path
                    .as_str()
                    .len()
                    .cmp(&left.rule.key.endpoint().path.as_str().len())
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
        for planned in group {
            let rule = &planned.rule;
            let Some(remainder) =
                remainder_of(rule.key.endpoint().path.as_str(), requested.path.as_str())
            else {
                continue;
            };
            let mut targets = Vec::with_capacity(rule.target.weighted().len());
            for target in rule.target.weighted() {
                let endpoint = &target.endpoint;
                let path = AbsPath::new(&join(endpoint.path.as_str(), remainder)).ok()?;
                targets.push(Weighted::serving(
                    Endpoint::new(
                        endpoint.protocol,
                        endpoint.host.clone(),
                        endpoint.port,
                        path,
                    ),
                    target.weight,
                    target.serves,
                ));
            }
            return Some(Resolution {
                rule: rule.clone(),
                targets,
                plan: Arc::clone(&planned.plan),
                credential: planned.credential.clone(),
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
        self.by_authority
            .values()
            .flatten()
            .map(|planned| &planned.rule)
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

[cache]
backend = "sled"
path = "./test-cache"

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
            api: ip_core::ApiId::new("anthropic").unwrap(),
            key: RouteKey::new(endpoint(Protocol::Http, "127.0.0.1", 8080, key_path)),
            target: RouteTarget::new(endpoint(Protocol::Https, target_host, 443, target_path)),
            strategy: ip_core::Strategy::default(),
            hedge: ip_core::Hedge::default(),
            hedge_delay: ip_core::HedgeDelay::default(),
            caller_speaks: None,
            upstream_speaks: None,
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
            resolved.primary().to_string(),
            "https://api.example.com:443/v1"
        );
    }

    #[test]
    fn a_deeper_path_carries_its_remainder_to_the_target() {
        let table = table(vec![rule("/anthropic", "api.example.com", "/v1")]);
        let resolved = table.resolve(&key("/anthropic/messages")).unwrap();
        assert_eq!(
            resolved.primary().to_string(),
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
            resolved.primary().to_string(),
            "https://beta.example.com:443/v1/messages"
        );
        let resolved = table.resolve(&key("/anthropic/messages")).unwrap();
        assert_eq!(
            resolved.primary().to_string(),
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
            resolved.primary().to_string(),
            "https://api.example.com:443/v1/messages"
        );
    }

    #[test]
    fn a_target_at_the_root_gains_no_double_slash() {
        let table = table(vec![rule("/anthropic", "api.example.com", "/")]);
        let resolved = table.resolve(&key("/anthropic/messages")).unwrap();
        assert_eq!(
            resolved.primary().to_string(),
            "https://api.example.com:443/messages"
        );
    }

    #[test]
    fn a_trailing_slash_on_a_rule_changes_nothing() {
        let table = table(vec![rule("/anthropic/", "api.example.com", "/v1")]);
        let resolved = table.resolve(&key("/anthropic/messages")).unwrap();
        assert_eq!(
            resolved.primary().to_string(),
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
            resolved.primary().to_string(),
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
            resolved.primary().host.as_str(),
            "api.anthropic.com",
            "the configured target must survive a stored rule under the same key"
        );
    }

    #[test]
    fn every_endpoint_of_a_replicated_target_carries_the_remainder() {
        let mut replicated = rule("/anthropic", "one.example.com", "/v1");
        replicated.target = ip_core::RouteTarget::from_endpoints(vec![
            endpoint(Protocol::Https, "one.example.com", 443, "/v1"),
            endpoint(Protocol::Https, "two.example.com", 443, "/v1"),
        ])
        .unwrap();
        let table = table(vec![replicated]);
        let resolved = table.resolve(&key("/anthropic/messages")).unwrap();
        let rendered: Vec<String> = resolved
            .targets()
            .iter()
            .map(|target| target.endpoint.to_string())
            .collect();
        assert_eq!(
            rendered,
            vec![
                "https://one.example.com:443/v1/messages".to_owned(),
                "https://two.example.com:443/v1/messages".to_owned(),
            ]
        );
    }

    #[test]
    fn a_rule_may_not_claim_the_reserved_prefix() {
        let config = Config::parse(CONFIG).unwrap();
        let intruder = rule("/_ip/whoami", "attacker.example", "/v1");
        assert!(matches!(
            RoutingTable::build(&config, vec![intruder]),
            Err(RouteError::ReservedPath { .. })
        ));
    }

    #[test]
    fn a_path_merely_starting_with_the_prefix_is_allowed() {
        let config = Config::parse(CONFIG).unwrap();
        let ordinary = rule("/_iproute", "api.example.com", "/v1");
        assert!(RoutingTable::build(&config, vec![ordinary]).is_ok());
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

    /// A configuration naming two providers, each with its own credential, which is the shape
    /// the whole of this exists for.
    const TWO: &str = r#"
[server]
listen = "127.0.0.1:8080"

[storage]
backend = "sqlite"
path = "./test.db"

[cache]
backend = "sled"
path = "./test-cache"

[auth.jwt]
issuer = "intelli-prism"
secret = "0123456789abcdef0123456789abcdef"

[[upstream]]
id = "anthropic"
base_url = "https://api.anthropic.com/v1"
api_key = "sk-ant-x"
model = "claude-opus-5"

[[upstream]]
id = "openai"
base_url = "https://api.openai.com/v1"
api_key = "sk-oai-y"
model = "gpt-4o"
"#;

    fn exposed(table: &RoutingTable, path: &str) -> Option<String> {
        table
            .resolve(&key(path))
            .unwrap()
            .credential()
            .map(|secret| secret.expose().to_owned())
    }

    #[test]
    fn a_configured_upstreams_credential_reaches_the_rule_it_contributed() {
        let config = Config::parse(CONFIG).unwrap();
        let table = RoutingTable::build(&config, Vec::new()).unwrap();

        assert_eq!(exposed(&table, "/anthropic"), Some("sk-x".to_owned()));
    }

    /// The path remainder is joined onto the target after the rule matched, so a deeper request
    /// must still arrive with the credential of the rule that carried it.
    #[test]
    fn a_deeper_request_under_a_configured_rule_carries_the_credential_too() {
        let config = Config::parse(CONFIG).unwrap();
        let table = RoutingTable::build(&config, Vec::new()).unwrap();

        assert_eq!(
            exposed(&table, "/anthropic/messages"),
            Some("sk-x".to_owned())
        );
    }

    /// The property the whole commit exists for: a key belongs to one upstream and reaches no
    /// other, so a turn dispatched to one provider can never present another's credential.
    #[test]
    fn each_upstream_keeps_its_own_credential_and_no_other() {
        let config = Config::parse(TWO).unwrap();
        let table = RoutingTable::build(&config, Vec::new()).unwrap();

        assert_eq!(exposed(&table, "/anthropic"), Some("sk-ant-x".to_owned()));
        assert_eq!(exposed(&table, "/openai"), Some("sk-oai-y".to_owned()));
    }

    /// A route somebody added over the api names no credential, and nothing invents one.
    #[test]
    fn a_stored_rule_carries_no_credential() {
        let config = Config::parse(CONFIG).unwrap();
        let table =
            RoutingTable::build(&config, vec![rule("/internal", "llm.corp", "/v1")]).unwrap();

        assert_eq!(exposed(&table, "/internal/chat"), None);
        assert_eq!(exposed(&table, "/anthropic"), Some("sk-x".to_owned()));
    }

    /// Nothing is presented rather than an empty header, so the refusal is "this upstream holds
    /// no credential" rather than a provider answering 401 to a blank one.
    #[test]
    fn an_upstream_declaring_an_empty_credential_carries_none() {
        let config =
            Config::parse(&CONFIG.replace(r#"api_key = "sk-x""#, r#"api_key = """#)).unwrap();
        let table = RoutingTable::build(&config, Vec::new()).unwrap();

        assert_eq!(exposed(&table, "/anthropic"), None);
    }

    /// A reload rebuilds the table from the configured rules it kept and the stored rules it
    /// just read. The credentials travel with the first, so a rule change must not drop them.
    #[test]
    fn a_reload_keeps_what_the_configuration_contributed() {
        let config = Config::parse(CONFIG).unwrap();
        let configured = Configured::of(&config).unwrap();

        let table = RoutingTable::assemble(&configured, vec![rule("/internal", "llm.corp", "/v1")])
            .unwrap();

        assert_eq!(exposed(&table, "/anthropic"), Some("sk-x".to_owned()));
        assert_eq!(exposed(&table, "/internal"), None);
    }

    /// A stored rule cannot claim a configured key, and must not reach its credential by
    /// claiming one either.
    #[test]
    fn a_stored_rule_claiming_a_configured_key_gets_neither_target_nor_credential() {
        let config = Config::parse(CONFIG).unwrap();
        let table =
            RoutingTable::build(&config, vec![rule("/anthropic", "attacker.example", "/v1")])
                .unwrap();
        let resolved = table.resolve(&key("/anthropic")).unwrap();

        assert_eq!(resolved.primary().host.as_str(), "api.anthropic.com");
        assert_eq!(
            resolved
                .credential()
                .map(|secret| secret.expose().to_owned()),
            Some("sk-x".to_owned())
        );
    }

    /// Rules a test or an example contributes have no configuration behind them.
    #[test]
    fn rules_with_no_configuration_behind_them_carry_no_credential() {
        let table = table(vec![rule("/anthropic", "api.example.com", "/v1")]);

        assert_eq!(exposed(&table, "/anthropic"), None);
    }
}
