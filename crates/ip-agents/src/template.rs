use std::fmt;

use crate::config::AgentConfig;
use crate::error::AgentError;
use crate::id::AgentId;
use crate::sandbox::Sandbox;

/// The longest a template's name may be, in bytes.
const NAME_MAX_BYTES: usize = 64;

/// What a template is called, for somebody reading a list of them.
///
/// Unlike an [`AgentId`] this is a label rather than a key: it may carry spaces and anything a
/// person writes their own language in, and two templates may share one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct AgentName(Box<str>);

impl AgentName {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "agent name";

    /// Validates and wraps a name.
    pub fn new(raw: &str) -> Result<Self, AgentError> {
        Self::try_from(raw.to_owned())
    }

    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AgentName {
    type Error = AgentError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(AgentError::Empty { kind: Self::KIND });
        }
        if raw.len() > NAME_MAX_BYTES {
            return Err(AgentError::TooLong {
                kind: Self::KIND,
                len: raw.len(),
                max: NAME_MAX_BYTES,
            });
        }
        // Refused rather than trimmed: a store that quietly alters what it was given hands back
        // something the caller did not write and cannot find again.
        if raw.trim() != raw {
            return Err(AgentError::Untrimmed { kind: Self::KIND });
        }
        match raw.chars().find(|ch| ch.is_control()) {
            Some(ch) => Err(AgentError::IllegalChar {
                kind: Self::KIND,
                ch,
            }),
            None => Ok(Self(raw.into_boxed_str())),
        }
    }
}

impl fmt::Display for AgentName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One agent template: what it is called, what it would see, and what it would run under.
///
/// **Nothing here is checked against anything else here.** In particular a tool the
/// [`AgentConfig`]'s tool set names need not appear in the [`Sandbox`]: the preset of system tools
/// is not stored in a sandbox at all, and an MCP tool is not a file. Whether a named tool can
/// really be reached is the runner's question, and there is no runner.
///
/// Whose the template is, and when it was stored, belong to whatever stores it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTemplate {
    /// Which template this is.
    pub id: AgentId,
    /// What it is called.
    pub name: AgentName,
    /// The file tree its agent would see.
    #[serde(default)]
    pub sandbox: Sandbox,
    /// What its agent would run under.
    pub config: AgentConfig,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfigParts, AgentRole, MaxTurns, Prompt, ReqTimeout, TimeLimit};
    use crate::sandbox::{Entries, ObjectLocation, PathSegment, SandboxEntry};

    fn config() -> AgentConfig {
        AgentConfig::new(AgentConfigParts {
            role: AgentRole::System,
            prompt: Prompt::new("You are a helpful assistant.").unwrap(),
            tool_set: crate::config::ToolSet::default(),
            max_turns: MaxTurns::new(10).unwrap(),
            time_limit: TimeLimit::new(60).unwrap(),
            req_timeout: ReqTimeout::new(10).unwrap(),
            use_best_effort: false,
            best_effort_strategy: None,
            model: None,
            api: None,
        })
        .unwrap()
    }

    fn template() -> AgentTemplate {
        AgentTemplate {
            id: AgentId::new("support-triage").unwrap(),
            name: AgentName::new("Support triage").unwrap(),
            sandbox: Sandbox::new(Entries::from([(
                PathSegment::new("bin").unwrap(),
                SandboxEntry::File(ObjectLocation::new("s3://mybucket/bin/python").unwrap()),
            )]))
            .unwrap(),
            config: config(),
        }
    }

    #[test]
    fn a_name_takes_what_a_person_writes() {
        for raw in ["Support triage", "客服分流", "tier-2 (EU)"] {
            assert_eq!(AgentName::new(raw).unwrap().as_str(), raw);
        }
        assert_eq!(
            AgentName::new("Support triage").unwrap().to_string(),
            "Support triage"
        );
    }

    #[test]
    fn a_name_is_refused_empty_and_past_its_ceiling() {
        assert_eq!(
            AgentName::new(""),
            Err(AgentError::Empty {
                kind: AgentName::KIND
            })
        );
        assert!(matches!(
            AgentName::new(&"a".repeat(NAME_MAX_BYTES + 1)),
            Err(AgentError::TooLong { .. })
        ));
    }

    #[test]
    fn a_name_with_whitespace_at_its_edge_is_refused_rather_than_trimmed() {
        for raw in [" triage", "triage ", "\ttriage"] {
            assert_eq!(
                AgentName::new(raw),
                Err(AgentError::Untrimmed {
                    kind: AgentName::KIND
                }),
                "{raw:?}"
            );
        }
        // Inside, a space is part of the name.
        assert_eq!(
            AgentName::new("tier two triage").unwrap().as_str(),
            "tier two triage"
        );
    }

    #[test]
    fn a_name_carrying_a_control_character_is_refused() {
        assert_eq!(
            AgentName::new("one\u{7}two"),
            Err(AgentError::IllegalChar {
                kind: AgentName::KIND,
                ch: '\u{7}',
            })
        );
    }

    #[test]
    fn a_template_survives_its_own_round_trip() {
        let template = template();
        let written = serde_json::to_string(&template).unwrap();
        assert_eq!(
            serde_json::from_str::<AgentTemplate>(&written).unwrap(),
            template
        );
    }

    #[test]
    fn a_template_may_see_no_files_at_all() {
        let written = r#"{"id":"triage","name":"Triage","config":{"role":"system","prompt":"Be helpful.","max_turns":4,"time_limit":30,"req_timeout":10}}"#;
        let template: AgentTemplate = serde_json::from_str(written).unwrap();
        assert!(template.sandbox.is_empty());
        assert!(template.config.tool_set().is_empty());
    }

    #[test]
    fn a_template_refuses_a_field_this_code_does_not_carry() {
        let written =
            serde_json::to_string(&template())
                .unwrap()
                .replacen('{', "{\"owner\":\"acme\",", 1);
        let error = serde_json::from_str::<AgentTemplate>(&written).unwrap_err();
        assert!(error.to_string().contains("owner"), "{error}");
    }

    #[test]
    fn a_template_refuses_a_part_its_own_type_refuses() {
        let written = serde_json::to_string(&template())
            .unwrap()
            .replace(r#""name":"Support triage""#, r#""name":" Support triage""#);
        let error = serde_json::from_str::<AgentTemplate>(&written).unwrap_err();
        assert!(error.to_string().contains("whitespace"), "{error}");
    }

    #[test]
    fn a_tool_need_not_appear_in_the_sandbox() {
        let mut parts = AgentConfigParts::from(config());
        parts.tool_set =
            crate::config::ToolSet::new(vec![crate::config::ToolName::new("sql").unwrap()])
                .unwrap();
        let template = AgentTemplate {
            config: AgentConfig::new(parts).unwrap(),
            sandbox: Sandbox::default(),
            ..template()
        };
        let written = serde_json::to_string(&template).unwrap();
        assert_eq!(
            serde_json::from_str::<AgentTemplate>(&written).unwrap(),
            template
        );
    }
}
