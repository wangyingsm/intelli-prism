use std::fmt;
use std::str::FromStr;

use sha2::{Digest, Sha256};

use crate::error::CoreError;
use crate::id::{ApiId, TenantId, UserId};
use crate::key::{DIGEST_BYTES, decode_hex};

/// The sha256 of a plugin's wasm, which is how a plugin is named and cached.
#[derive(
    Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct Checksum([u8; DIGEST_BYTES]);

impl Checksum {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "plugin checksum";

    /// The checksum of some wasm.
    pub fn of(wasm: &[u8]) -> Self {
        Self(Sha256::digest(wasm).into())
    }

    /// Parses the hex form a rule or a file name carries.
    pub fn from_hex(raw: &str) -> Result<Self, CoreError> {
        decode_hex(Self::KIND, raw).map(Self)
    }

    /// The hex form.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// The raw digest bytes.
    pub fn as_bytes(&self) -> &[u8; DIGEST_BYTES] {
        &self.0
    }

    /// Whether some wasm is the wasm this names.
    pub fn matches(&self, wasm: &[u8]) -> bool {
        Self::of(wasm) == *self
    }
}

impl fmt::Display for Checksum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Checksum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Checksum").field(&self.to_hex()).finish()
    }
}

impl FromStr for Checksum {
    type Err = CoreError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::from_hex(raw)
    }
}

impl TryFrom<String> for Checksum {
    type Error = CoreError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::from_hex(&raw)
    }
}

impl From<Checksum> for String {
    fn from(checksum: Checksum) -> Self {
        checksum.to_hex()
    }
}

/// Highest order reserved for the plugins the system ships.
pub const PRIMARY_ORDER_MAX: u8 = 63;

/// Where a plugin sits in its chain. Higher runs first.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct PluginOrder(u8);

impl PluginOrder {
    /// Wraps a plugin's order.
    pub const fn new(value: u8) -> Self {
        Self(value)
    }

    /// The order as a number.
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Whether this order belongs to a plugin the system ships rather than a tenant's.
    pub const fn is_primary(self) -> bool {
        self.0 <= PRIMARY_ORDER_MAX
    }
}

impl fmt::Display for PluginOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Where in the dataflow a plugin runs.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    /// Rewrites the request headers.
    ReqHeader,
    /// Rewrites the request body.
    ReqBody,
    /// Rewrites the response headers.
    RespHeader,
    /// Rewrites the response body.
    RespBody,
    /// Rewrites one chunk of a streamed response.
    RespChunk,
}

impl PluginKind {
    /// The kind's name, as a stored row holds it.
    pub fn name(self) -> &'static str {
        match self {
            Self::ReqHeader => "req_header",
            Self::ReqBody => "req_body",
            Self::RespHeader => "resp_header",
            Self::RespBody => "resp_body",
            Self::RespChunk => "resp_chunk",
        }
    }

    /// Whether this kind acts on headers rather than on a body.
    pub fn is_header(self) -> bool {
        matches!(self, Self::ReqHeader | Self::RespHeader)
    }
}

impl fmt::Display for PluginKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for PluginKind {
    type Err = CoreError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "req_header" => Ok(Self::ReqHeader),
            "req_body" => Ok(Self::ReqBody),
            "resp_header" => Ok(Self::RespHeader),
            "resp_body" => Ok(Self::RespBody),
            "resp_chunk" => Ok(Self::RespChunk),
            other => Err(CoreError::UnknownPluginKind {
                value: other.to_owned(),
            }),
        }
    }
}

/// Who a plugin rule applies to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum PluginScope {
    /// Every flow. Only plugins the system ships run here, at a primary order.
    Global,
    /// One tenant, narrowed to a user and to an api when either is named.
    Tenant {
        /// The tenant the rule belongs to.
        tenant: TenantId,
        /// The one user it applies to, or every user in the tenant.
        user: Option<UserId>,
        /// The one api it applies to, or every api the tenant reaches.
        api: Option<ApiId>,
    },
}

impl PluginScope {
    /// The scope's name, as it appears in errors.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Tenant { .. } => "tenant",
        }
    }

    /// The tenant the rule belongs to, when it belongs to one.
    pub fn tenant(&self) -> Option<&TenantId> {
        match self {
            Self::Global => None,
            Self::Tenant { tenant, .. } => Some(tenant),
        }
    }

    /// Whether a request by this user in this tenant, routed to this api, falls inside.
    pub fn contains(&self, tenant: &TenantId, user: &UserId, api: &ApiId) -> bool {
        match self {
            Self::Global => true,
            Self::Tenant {
                tenant: owner,
                user: only_user,
                api: only_api,
            } => {
                owner == tenant
                    && only_user.as_ref().is_none_or(|only| only == user)
                    && only_api.as_ref().is_none_or(|only| only == api)
            }
        }
    }
}

/// Refuses an order the scope may not use: primary orders are global, the rest are a tenant's.
fn check_order(order: PluginOrder, scope: &PluginScope) -> Result<(), CoreError> {
    let global = matches!(scope, PluginScope::Global);
    if order.is_primary() == global {
        return Ok(());
    }
    Err(CoreError::OrderOutOfScope {
        order: order.get(),
        scope: scope.name(),
    })
}

/// A plugin about to be placed in a chain. Its kind is read from the stored plugin, so a
/// rule can never claim a kind its wasm was not stored as.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NewPluginRule {
    checksum: Checksum,
    order: PluginOrder,
    scope: PluginScope,
}

impl NewPluginRule {
    /// Places a plugin, refusing an order its scope may not use.
    pub fn new(
        checksum: Checksum,
        order: PluginOrder,
        scope: PluginScope,
    ) -> Result<Self, CoreError> {
        check_order(order, &scope)?;
        Ok(Self {
            checksum,
            order,
            scope,
        })
    }

    /// The plugin to run.
    pub fn checksum(&self) -> &Checksum {
        &self.checksum
    }

    /// Where it sits in its chain.
    pub fn order(&self) -> PluginOrder {
        self.order
    }

    /// Who it applies to.
    pub fn scope(&self) -> &PluginScope {
        &self.scope
    }

    /// Completes the rule with the kind the stored plugin was written as.
    pub fn with_kind(self, kind: PluginKind) -> PluginRule {
        PluginRule {
            checksum: self.checksum,
            kind,
            order: self.order,
            scope: self.scope,
        }
    }
}

/// A plugin placed in a chain: which one, where, and for whom.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginRule {
    checksum: Checksum,
    kind: PluginKind,
    order: PluginOrder,
    scope: PluginScope,
}

impl PluginRule {
    /// Rebuilds a rule from its parts, refusing an order its scope may not use.
    pub fn new(
        checksum: Checksum,
        kind: PluginKind,
        order: PluginOrder,
        scope: PluginScope,
    ) -> Result<Self, CoreError> {
        check_order(order, &scope)?;
        Ok(Self {
            checksum,
            kind,
            order,
            scope,
        })
    }

    /// The plugin to run.
    pub fn checksum(&self) -> &Checksum {
        &self.checksum
    }

    /// Which chain it joins.
    pub fn kind(&self) -> PluginKind {
        self.kind
    }

    /// Where it sits in that chain.
    pub fn order(&self) -> PluginOrder {
        self.order
    }

    /// Who it applies to.
    pub fn scope(&self) -> &PluginScope {
        &self.scope
    }

    /// Whether this rule joins the chain of a request by this user, routed to this api.
    pub fn applies_to(&self, tenant: &TenantId, user: &UserId, api: &ApiId) -> bool {
        self.scope.contains(tenant, user, api)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_checksum_survives_its_own_round_trip_as_the_hex_it_is_read_from() {
        let checksum = Checksum::of(b"module");
        let written = serde_json::to_string(&checksum).unwrap();
        assert_eq!(written, format!("\"{}\"", checksum.to_hex()));
        assert_eq!(
            serde_json::from_str::<Checksum>(&written).unwrap(),
            checksum
        );
    }

    #[test]
    fn a_checksum_is_the_sha256_of_the_wasm() {
        let checksum = Checksum::of(b"\0asm\x01\0\0\0");
        assert_eq!(checksum.to_hex().len(), DIGEST_BYTES * 2);
        assert!(checksum.matches(b"\0asm\x01\0\0\0"));
        assert!(!checksum.matches(b"tampered"));
    }

    #[test]
    fn a_checksum_round_trips_through_hex() {
        let checksum = Checksum::of(b"plugin");
        assert_eq!(Checksum::from_hex(&checksum.to_hex()).unwrap(), checksum);
        assert_eq!(checksum.to_hex().parse::<Checksum>().unwrap(), checksum);
    }

    #[test]
    fn a_checksum_of_the_wrong_length_is_refused() {
        assert_eq!(
            Checksum::from_hex("00ff"),
            Err(CoreError::HexLength {
                kind: "plugin checksum",
                expected: DIGEST_BYTES * 2,
            })
        );
    }

    #[test]
    fn a_checksum_that_is_not_hex_is_refused() {
        let raw = "z".repeat(DIGEST_BYTES * 2);
        assert_eq!(
            Checksum::from_hex(&raw),
            Err(CoreError::HexDigit {
                kind: "plugin checksum"
            })
        );
    }

    #[test]
    fn every_kind_reads_back_from_its_stored_name() {
        for kind in [
            PluginKind::ReqHeader,
            PluginKind::ReqBody,
            PluginKind::RespHeader,
            PluginKind::RespBody,
            PluginKind::RespChunk,
        ] {
            assert_eq!(kind.name().parse::<PluginKind>().unwrap(), kind);
            assert_eq!(kind.to_string(), kind.name());
        }
    }

    #[test]
    fn a_kind_the_code_does_not_know_is_refused() {
        assert!(matches!(
            "req_trailer".parse::<PluginKind>(),
            Err(CoreError::UnknownPluginKind { .. })
        ));
    }

    #[test]
    fn the_reserved_order_range_is_the_first_sixty_four() {
        assert!(PluginOrder::new(0).is_primary());
        assert!(PluginOrder::new(PRIMARY_ORDER_MAX).is_primary());
        assert!(!PluginOrder::new(PRIMARY_ORDER_MAX + 1).is_primary());
        assert!(!PluginOrder::new(u8::MAX).is_primary());
    }

    #[test]
    fn the_orders_left_for_tenants_number_one_hundred_and_ninety_two() {
        let tenant_orders = (0..=u8::MAX)
            .filter(|order| !PluginOrder::new(*order).is_primary())
            .count();
        assert_eq!(tenant_orders, 192);
    }

    #[test]
    fn an_order_reads_as_its_number() {
        assert_eq!(PluginOrder::new(200).get(), 200);
        assert_eq!(PluginOrder::new(200).to_string(), "200");
    }

    fn acme() -> TenantId {
        TenantId::new("acme").unwrap()
    }

    fn alice() -> UserId {
        UserId::new("alice").unwrap()
    }

    fn anthropic() -> ApiId {
        ApiId::new("anthropic").unwrap()
    }

    fn tenant_scope(user: Option<&str>, api: Option<&str>) -> PluginScope {
        PluginScope::Tenant {
            tenant: acme(),
            user: user.map(|user| UserId::new(user).unwrap()),
            api: api.map(|api| ApiId::new(api).unwrap()),
        }
    }

    #[test]
    fn a_global_rule_takes_a_primary_order() {
        assert!(
            NewPluginRule::new(
                Checksum::of(b"p"),
                PluginOrder::new(10),
                PluginScope::Global
            )
            .is_ok()
        );
    }

    #[test]
    fn a_global_rule_may_not_take_a_tenant_order() {
        assert_eq!(
            NewPluginRule::new(
                Checksum::of(b"p"),
                PluginOrder::new(100),
                PluginScope::Global
            ),
            Err(CoreError::OrderOutOfScope {
                order: 100,
                scope: "global",
            })
        );
    }

    #[test]
    fn a_tenant_rule_may_not_take_a_primary_order() {
        assert_eq!(
            PluginRule::new(
                Checksum::of(b"p"),
                PluginKind::ReqBody,
                PluginOrder::new(10),
                tenant_scope(None, None),
            ),
            Err(CoreError::OrderOutOfScope {
                order: 10,
                scope: "tenant",
            })
        );
    }

    #[test]
    fn a_new_rule_takes_its_kind_from_the_stored_plugin() {
        let rule = NewPluginRule::new(
            Checksum::of(b"p"),
            PluginOrder::new(100),
            tenant_scope(None, None),
        )
        .unwrap()
        .with_kind(PluginKind::RespBody);
        assert_eq!(rule.kind(), PluginKind::RespBody);
        assert_eq!(rule.order(), PluginOrder::new(100));
        assert_eq!(rule.checksum(), &Checksum::of(b"p"));
    }

    #[test]
    fn a_global_scope_contains_every_request() {
        let other = TenantId::new("globex").unwrap();
        assert!(PluginScope::Global.contains(&acme(), &alice(), &anthropic()));
        assert!(PluginScope::Global.contains(&other, &alice(), &anthropic()));
        assert_eq!(PluginScope::Global.tenant(), None);
    }

    #[test]
    fn a_tenant_wide_scope_contains_every_user_and_api_in_the_tenant() {
        let scope = tenant_scope(None, None);
        let bob = UserId::new("bob").unwrap();
        let internal = ApiId::new("internal").unwrap();
        assert!(scope.contains(&acme(), &alice(), &anthropic()));
        assert!(scope.contains(&acme(), &bob, &internal));
        assert_eq!(scope.tenant(), Some(&acme()));
    }

    #[test]
    fn a_tenant_scope_never_reaches_another_tenant() {
        let other = TenantId::new("globex").unwrap();
        assert!(!tenant_scope(None, None).contains(&other, &alice(), &anthropic()));
    }

    #[test]
    fn a_user_scope_contains_only_that_user() {
        let scope = tenant_scope(Some("alice"), None);
        assert!(scope.contains(&acme(), &alice(), &anthropic()));
        assert!(!scope.contains(&acme(), &UserId::new("bob").unwrap(), &anthropic()));
    }

    #[test]
    fn an_api_scope_contains_only_that_api() {
        let scope = tenant_scope(None, Some("anthropic"));
        assert!(scope.contains(&acme(), &alice(), &anthropic()));
        assert!(!scope.contains(&acme(), &alice(), &ApiId::new("internal").unwrap()));
    }

    #[test]
    fn a_user_and_api_scope_needs_both() {
        let rule = PluginRule::new(
            Checksum::of(b"p"),
            PluginKind::ReqBody,
            PluginOrder::new(100),
            tenant_scope(Some("alice"), Some("anthropic")),
        )
        .unwrap();
        assert!(rule.applies_to(&acme(), &alice(), &anthropic()));
        assert!(!rule.applies_to(&acme(), &UserId::new("bob").unwrap(), &anthropic()));
        assert!(!rule.applies_to(&acme(), &alice(), &ApiId::new("internal").unwrap()));
    }

    #[test]
    fn only_the_header_kinds_act_on_headers() {
        assert!(PluginKind::ReqHeader.is_header());
        assert!(PluginKind::RespHeader.is_header());
        assert!(!PluginKind::ReqBody.is_header());
        assert!(!PluginKind::RespBody.is_header());
        assert!(!PluginKind::RespChunk.is_header());
    }
}
