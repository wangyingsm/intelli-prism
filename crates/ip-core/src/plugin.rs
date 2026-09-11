use std::fmt;
use std::str::FromStr;

use sha2::{Digest, Sha256};

use crate::error::CoreError;
use crate::key::{DIGEST_BYTES, decode_hex};

/// The sha256 of a plugin's wasm, which is how a plugin is named and cached.
#[derive(
    Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String")]
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn only_the_header_kinds_act_on_headers() {
        assert!(PluginKind::ReqHeader.is_header());
        assert!(PluginKind::RespHeader.is_header());
        assert!(!PluginKind::ReqBody.is_header());
        assert!(!PluginKind::RespBody.is_header());
        assert!(!PluginKind::RespChunk.is_header());
    }
}
