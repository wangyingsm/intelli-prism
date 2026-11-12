//! Which model answers a run, and which family its turns are written in.

use ip_agents::{AgentConfig, ChosenApi, ChosenModel};
use ip_core::{Dialect, ModelName};

use crate::error::RunError;

/// What the request that started a run asked for, where it asked for anything.
///
/// It is spelled in the template's own types rather than in types of its own, so that `auto`
/// means one thing in a stored template and in a request, and is refused the same way in both.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Requested {
    /// Which model, where the request names one.
    #[serde(default)]
    pub model: Option<ChosenModel>,
    /// Which family, where the request names one.
    #[serde(default)]
    pub api: Option<ChosenApi>,
}

/// What a run's turns are pinned to, once the request and the template have both had their say.
///
/// Absent in either half means the deployment decides — the classified route, or the arbiter —
/// which is what the gateway already does for a request that names no endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pinned {
    /// The model every turn asks for, and none where nothing is pinned.
    pub model: Option<ModelName>,
    /// The family every turn is written in, and none where nothing is pinned.
    pub api: Option<Dialect>,
}

impl Pinned {
    /// Settles what a run will use.
    ///
    /// The request wins over the template wherever it says anything, each half on its own: a
    /// request naming only a model leaves the template's family alone. A request may therefore
    /// name `auto` to *unpin* what a template pinned, which is the only way to say that and the
    /// reason the literal is a value rather than an absence.
    pub fn resolve(requested: &Requested, config: &AgentConfig) -> Result<Self, RunError> {
        let model = match requested.model.as_ref().or_else(|| config.model()) {
            Some(ChosenModel::Named(name)) => Some(named(name)?),
            Some(ChosenModel::Auto) | None => None,
        };
        Ok(Self {
            model,
            api: requested.api.or_else(|| config.api()).and_then(spoken_as),
        })
    }

    /// Whether the deployment's own routing is left to choose everything.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.model.is_none() && self.api.is_none()
    }
}

/// The gateway's name for a model a template or a request spelled.
fn named(name: &str) -> Result<ModelName, RunError> {
    ModelName::new(name).map_err(|error| RunError::NotAModel {
        name: name.to_owned(),
        detail: error.to_string(),
    })
}

/// The family the gateway speaks for a family a template or a request chose.
const fn spoken_as(api: ChosenApi) -> Option<Dialect> {
    match api {
        ChosenApi::Auto => None,
        ChosenApi::OpenAi => Some(Dialect::OpenAi),
        ChosenApi::Anthropic => Some(Dialect::Anthropic),
        ChosenApi::Gemini => Some(Dialect::Gemini),
    }
}

#[cfg(test)]
mod tests {
    use ip_agents::{
        AgentConfigParts, AgentRole, MaxTurns, Prompt, ReqTimeout, TimeLimit, ToolSet,
    };

    use super::*;

    fn config(model: Option<ChosenModel>, api: Option<ChosenApi>) -> AgentConfig {
        AgentConfig::new(AgentConfigParts {
            role: AgentRole::System,
            prompt: Prompt::new("Be helpful.").unwrap(),
            tool_set: ToolSet::default(),
            max_turns: MaxTurns::new(4).unwrap(),
            time_limit: TimeLimit::new(60).unwrap(),
            req_timeout: ReqTimeout::new(10).unwrap(),
            use_best_effort: false,
            best_effort_strategy: None,
            model,
            api,
        })
        .unwrap()
    }

    fn asking(model: Option<&str>, api: Option<ChosenApi>) -> Requested {
        Requested {
            model: model.map(|raw| ChosenModel::new(raw).unwrap()),
            api,
        }
    }

    fn resolved(requested: &Requested, config: &AgentConfig) -> Pinned {
        Pinned::resolve(requested, config).unwrap()
    }

    #[test]
    fn a_template_that_pins_nothing_leaves_the_deployment_to_choose() {
        let pinned = resolved(&Requested::default(), &config(None, None));

        assert_eq!(pinned, Pinned::default());
        assert!(pinned.is_open());
    }

    #[test]
    fn what_a_template_names_is_what_a_run_uses_when_the_request_is_silent() {
        let pinned = resolved(
            &Requested::default(),
            &config(
                Some(ChosenModel::new("claude-sonnet-4-5").unwrap()),
                Some(ChosenApi::Anthropic),
            ),
        );

        assert!(!pinned.is_open());
        assert_eq!(pinned.model.unwrap().as_str(), "claude-sonnet-4-5");
        assert_eq!(pinned.api, Some(Dialect::Anthropic));
    }

    #[test]
    fn what_the_request_names_beats_what_the_template_names() {
        let pinned = resolved(
            &asking(Some("gpt-4o"), Some(ChosenApi::OpenAi)),
            &config(
                Some(ChosenModel::new("claude-sonnet-4-5").unwrap()),
                Some(ChosenApi::Anthropic),
            ),
        );

        assert_eq!(pinned.model.unwrap().as_str(), "gpt-4o");
        assert_eq!(pinned.api, Some(Dialect::OpenAi));
    }

    /// Each half on its own, so naming a model does not silently unpin the family beside it.
    #[test]
    fn a_request_that_names_one_half_leaves_the_other_where_it_was() {
        let template = config(
            Some(ChosenModel::new("claude-sonnet-4-5").unwrap()),
            Some(ChosenApi::Anthropic),
        );

        let pinned = resolved(&asking(Some("gpt-4o"), None), &template);
        assert_eq!(pinned.model.unwrap().as_str(), "gpt-4o");
        assert_eq!(pinned.api, Some(Dialect::Anthropic));

        let pinned = resolved(&asking(None, Some(ChosenApi::Gemini)), &template);
        assert_eq!(pinned.model.unwrap().as_str(), "claude-sonnet-4-5");
        assert_eq!(pinned.api, Some(Dialect::Gemini));
    }

    /// The only way to say "whatever the deployment picks" over a template that pinned one.
    #[test]
    fn a_request_may_unpin_what_a_template_pinned() {
        let pinned = resolved(
            &Requested {
                model: Some(ChosenModel::Auto),
                api: Some(ChosenApi::Auto),
            },
            &config(
                Some(ChosenModel::new("claude-sonnet-4-5").unwrap()),
                Some(ChosenApi::Anthropic),
            ),
        );

        assert!(pinned.is_open(), "{pinned:?}");
    }

    #[test]
    fn a_template_that_pins_nothing_on_purpose_reads_as_pinning_nothing() {
        let pinned = resolved(
            &Requested::default(),
            &config(Some(ChosenModel::Auto), Some(ChosenApi::Auto)),
        );

        assert!(pinned.is_open());
    }

    #[test]
    fn every_family_a_template_may_choose_is_one_the_gateway_speaks() {
        for (chosen, dialect) in [
            (ChosenApi::OpenAi, Dialect::OpenAi),
            (ChosenApi::Anthropic, Dialect::Anthropic),
            (ChosenApi::Gemini, Dialect::Gemini),
        ] {
            let pinned = resolved(&asking(None, Some(chosen)), &config(None, None));
            assert_eq!(pinned.api, Some(dialect), "{chosen}");
        }
    }

    /// A template checks the shape of a name, not the name, so this is where the stricter rule
    /// is met — and a run is refused before its first turn rather than at its first upstream.
    #[test]
    fn a_name_no_upstream_could_be_asked_for_refuses_the_run() {
        let refused = Pinned::resolve(
            &Requested::default(),
            &config(Some(ChosenModel::new("模型-v2").unwrap()), None),
        );

        assert!(
            matches!(refused, Err(RunError::NotAModel { ref name, .. }) if name == "模型-v2"),
            "{refused:?}"
        );
    }

    #[test]
    fn a_request_naming_a_model_the_gateway_will_not_spell_refuses_the_run() {
        let refused = Pinned::resolve(&asking(Some("模型-v2"), None), &config(None, None));

        assert!(
            matches!(refused, Err(RunError::NotAModel { .. })),
            "{refused:?}"
        );
    }

    #[test]
    fn what_a_request_asks_for_is_read_the_way_a_template_is() {
        let asked: Requested = serde_json::from_str(r#"{"model":"auto","api":"gemini"}"#).unwrap();

        assert_eq!(asked.model, Some(ChosenModel::Auto));
        assert_eq!(asked.api, Some(ChosenApi::Gemini));
        assert_eq!(
            serde_json::from_str::<Requested>("{}").unwrap(),
            Requested::default()
        );
    }

    /// Written down the way it is read, so what one machine was asked is what another is asked.
    #[test]
    fn what_a_request_asks_for_is_written_down_the_way_it_is_read() {
        for asked in [
            Requested::default(),
            Requested {
                model: Some(ChosenModel::Auto),
                api: Some(ChosenApi::Gemini),
            },
            Requested {
                model: Some(ChosenModel::try_from("gpt-4o".to_owned()).unwrap()),
                api: None,
            },
        ] {
            let written = serde_json::to_string(&asked).unwrap();
            assert_eq!(
                serde_json::from_str::<Requested>(&written).unwrap(),
                asked,
                "{written} did not come back as what it went out as"
            );
        }
    }
}
