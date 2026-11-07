use std::fmt;

use crate::error::McpError;

/// The longest an id may be, in bytes.
const ID_MAX_BYTES: usize = 64;

/// Which MCP server this is, as an operator spells it and a template names it.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String")]
pub struct McpId(Box<str>);

impl McpId {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "mcp id";

    /// Validates and wraps an id.
    pub fn new(raw: &str) -> Result<Self, McpError> {
        Self::try_from(raw.to_owned())
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The rule is the one `ip-core` applies to a tenant or a user id, and the one `ip-agents`
/// applies to a template: at most 64 bytes of ascii alphanumerics, `-`, `_` and `.`, so an
/// operator meets one spelling rule across the system. It is written again rather than shared
/// because this crate depends on no other: sharing it would make another crate's error type
/// part of how this one fails.
fn validate(raw: &str) -> Result<(), McpError> {
    if raw.is_empty() {
        return Err(McpError::Empty { kind: McpId::KIND });
    }
    if raw.len() > ID_MAX_BYTES {
        return Err(McpError::TooLong {
            kind: McpId::KIND,
            len: raw.len(),
            max: ID_MAX_BYTES,
        });
    }
    match raw
        .chars()
        .find(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')))
    {
        Some(ch) => Err(McpError::IllegalChar {
            kind: McpId::KIND,
            ch,
        }),
        None => Ok(()),
    }
}

impl TryFrom<String> for McpId {
    type Error = McpError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        validate(&raw)?;
        Ok(Self(raw.into_boxed_str()))
    }
}

impl fmt::Display for McpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_takes_what_an_operator_writes() {
        for raw in ["warehouse", "warehouse-2", "acme.sql", "a_b", "A1"] {
            assert_eq!(McpId::new(raw).unwrap().as_str(), raw);
        }
        assert_eq!(McpId::new("warehouse").unwrap().to_string(), "warehouse");
    }

    #[test]
    fn an_id_is_refused_empty_and_past_its_ceiling() {
        assert_eq!(McpId::new(""), Err(McpError::Empty { kind: McpId::KIND }));
        assert!(matches!(
            McpId::new(&"a".repeat(ID_MAX_BYTES + 1)),
            Err(McpError::TooLong { .. })
        ));
        assert!(McpId::new(&"a".repeat(ID_MAX_BYTES)).is_ok());
    }

    #[test]
    fn an_id_refuses_what_it_may_not_carry() {
        for (raw, ch) in [
            ("ware house", ' '),
            ("ware/house", '/'),
            ("warehouse!", '!'),
        ] {
            assert_eq!(
                McpId::new(raw),
                Err(McpError::IllegalChar {
                    kind: McpId::KIND,
                    ch
                }),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn an_id_is_read_from_json_through_its_own_rule() {
        assert_eq!(
            serde_json::from_str::<McpId>(r#""warehouse""#).unwrap(),
            McpId::new("warehouse").unwrap()
        );
        let error = serde_json::from_str::<McpId>(r#""ware house""#).unwrap_err();
        assert!(error.to_string().contains("may not carry"), "{error}");
    }
}
