//! The identifier an operator spells a template by.

use std::fmt;

use crate::error::AgentError;

/// The longest an identifier may be, in bytes.
const ID_MAX_BYTES: usize = 64;

/// Identifies one agent template, as the operator spells it.
///
/// Spelled by whoever writes the template rather than minted here, so applying a template twice
/// replaces it rather than making a second one, and so a recorded change names something a person
/// can place.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String")]
pub struct AgentId(Box<str>);

impl AgentId {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "agent id";

    /// Validates and wraps a raw identifier.
    pub fn new(raw: &str) -> Result<Self, AgentError> {
        validate(raw)?;
        Ok(Self(Box::from(raw)))
    }

    /// The identifier as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Refuses an identifier no template may carry.
///
/// The rule is deliberately the one `ip-core` applies to a tenant or a user id — at most 64 bytes
/// of ascii alphanumerics, `-`, `_` and `.` — so an operator meets one spelling rule across the
/// system. It is written again rather than shared because this crate depends on no other: sharing
/// that macro would make `ip-core`'s error type part of how this crate fails.
fn validate(raw: &str) -> Result<(), AgentError> {
    if raw.is_empty() {
        return Err(AgentError::Empty {
            kind: AgentId::KIND,
        });
    }
    if raw.len() > ID_MAX_BYTES {
        return Err(AgentError::TooLong {
            kind: AgentId::KIND,
            len: raw.len(),
            max: ID_MAX_BYTES,
        });
    }
    match raw
        .chars()
        .find(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')))
    {
        Some(ch) => Err(AgentError::IllegalChar {
            kind: AgentId::KIND,
            ch,
        }),
        None => Ok(()),
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for AgentId {
    type Error = AgentError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        validate(&raw)?;
        Ok(Self(raw.into_boxed_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_keeps_what_it_was_spelled_as() {
        let id = AgentId::new("support-triage.v2_1").unwrap();
        assert_eq!(id.as_str(), "support-triage.v2_1");
        assert_eq!(id.to_string(), "support-triage.v2_1");
    }

    #[test]
    fn an_empty_id_is_refused() {
        assert_eq!(
            AgentId::new(""),
            Err(AgentError::Empty {
                kind: AgentId::KIND
            })
        );
    }

    #[test]
    fn an_id_at_the_limit_is_taken_and_one_past_it_is_not() {
        let longest = "a".repeat(ID_MAX_BYTES);
        assert_eq!(AgentId::new(&longest).unwrap().as_str(), longest);

        let over = "a".repeat(ID_MAX_BYTES + 1);
        assert_eq!(
            AgentId::new(&over),
            Err(AgentError::TooLong {
                kind: AgentId::KIND,
                len: ID_MAX_BYTES + 1,
                max: ID_MAX_BYTES,
            })
        );
    }

    #[test]
    fn an_id_is_refused_the_first_character_its_kind_does_not_admit() {
        for (raw, ch) in [
            ("has a space", ' '),
            ("slash/separated", '/'),
            ("quotes\"it", '"'),
            ("agent:one", ':'),
            ("naïve", 'ï'),
        ] {
            assert_eq!(
                AgentId::new(raw),
                Err(AgentError::IllegalChar {
                    kind: AgentId::KIND,
                    ch,
                }),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn a_byte_count_is_not_a_character_count() {
        // Four characters, twelve bytes: the limit counts what a column stores.
        let raw = "三".repeat(ID_MAX_BYTES / 3 + 1);
        assert!(matches!(
            AgentId::new(&raw),
            Err(AgentError::TooLong { .. })
        ));
    }

    #[test]
    fn an_id_survives_its_own_round_trip_as_the_string_it_is_read_from() {
        let id = AgentId::new("triage").unwrap();
        let written = serde_json::to_string(&id).unwrap();
        assert_eq!(written, "\"triage\"");
        assert_eq!(serde_json::from_str::<AgentId>(&written).unwrap(), id);
    }

    #[test]
    fn a_read_refuses_an_id_the_type_would_refuse() {
        let error = serde_json::from_str::<AgentId>("\"not an id\"").unwrap_err();
        assert!(error.to_string().contains("illegal character"), "{error}");
    }
}
