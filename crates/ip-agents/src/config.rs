use crate::error::AgentError;

/// The longest a prompt may be, in bytes: a template is a configuration, not a blob store.
const PROMPT_MAX_BYTES: usize = 32 * 1024;
/// The most turns a template may ask for, so a runaway loop is refused before it is funded.
const MAX_TURNS_MAX: u16 = 1_000;
/// The longest a run may last, in seconds.
const TIME_LIMIT_MAX_SECONDS: u32 = 86_400;
/// The longest one upstream request may wait, in seconds.
const REQ_TIMEOUT_MAX_SECONDS: u32 = 3_600;
/// The longest a tool name may be, in bytes.
const TOOL_NAME_MAX_BYTES: usize = 64;
/// The most tools one template may name.
const TOOL_SET_MAX: usize = 64;

/// Which voice a template's prompt speaks in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum AgentRole {
    /// The instruction an agent is built with.
    System,
    /// A turn written as though the operator said it.
    User,
}

impl AgentRole {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "agent role";

    /// The name this is stored under.
    ///
    /// Apart from any rendering for a human: this one is written into a column and read back, so
    /// it may not change to read better.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
        }
    }

    /// Reads it back from that name, refusing one this code does not carry.
    pub fn named(name: &str) -> Result<Self, AgentError> {
        match name {
            "system" => Ok(Self::System),
            "user" => Ok(Self::User),
            other => Err(AgentError::Unknown {
                kind: Self::KIND,
                value: other.to_owned(),
            }),
        }
    }
}

impl TryFrom<String> for AgentRole {
    type Error = AgentError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::named(&raw)
    }
}

impl From<AgentRole> for String {
    fn from(role: AgentRole) -> Self {
        role.name().to_owned()
    }
}

/// What a template tells its agent to be.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct Prompt(Box<str>);

impl Prompt {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "prompt";

    /// Validates and wraps a prompt.
    pub fn new(raw: &str) -> Result<Self, AgentError> {
        Self::try_from(raw.to_owned())
    }

    /// The prompt as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Prompt {
    type Error = AgentError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(AgentError::Empty { kind: Self::KIND });
        }
        if raw.len() > PROMPT_MAX_BYTES {
            return Err(AgentError::TooLong {
                kind: Self::KIND,
                len: raw.len(),
                max: PROMPT_MAX_BYTES,
            });
        }
        Ok(Self(raw.into_boxed_str()))
    }
}

/// How many turns one run may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "u16")]
pub struct MaxTurns(u16);

impl MaxTurns {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "max turns";

    /// Validates and wraps a count, refusing nought and a runaway.
    pub fn new(count: u16) -> Result<Self, AgentError> {
        Self::try_from(count)
    }

    /// The count itself.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl TryFrom<u16> for MaxTurns {
    type Error = AgentError;

    fn try_from(count: u16) -> Result<Self, Self::Error> {
        if count == 0 {
            return Err(AgentError::Zero { kind: Self::KIND });
        }
        if count > MAX_TURNS_MAX {
            return Err(AgentError::TooLarge {
                kind: Self::KIND,
                value: u64::from(count),
                max: u64::from(MAX_TURNS_MAX),
            });
        }
        Ok(Self(count))
    }
}

/// How long one run may last, in seconds.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "u32")]
pub struct TimeLimit(u32);

impl TimeLimit {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "time limit";

    /// Validates and wraps a span, refusing nought and one past a day.
    pub fn new(seconds: u32) -> Result<Self, AgentError> {
        Self::try_from(seconds)
    }

    /// The span in seconds.
    #[must_use]
    pub const fn seconds(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for TimeLimit {
    type Error = AgentError;

    fn try_from(seconds: u32) -> Result<Self, Self::Error> {
        bounded(Self::KIND, seconds, TIME_LIMIT_MAX_SECONDS).map(Self)
    }
}

/// How long one upstream request may wait, in seconds.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "u32")]
pub struct ReqTimeout(u32);

impl ReqTimeout {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "request timeout";

    /// Validates and wraps a span, refusing nought and one past an hour.
    pub fn new(seconds: u32) -> Result<Self, AgentError> {
        Self::try_from(seconds)
    }

    /// The span in seconds.
    #[must_use]
    pub const fn seconds(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for ReqTimeout {
    type Error = AgentError;

    fn try_from(seconds: u32) -> Result<Self, Self::Error> {
        bounded(Self::KIND, seconds, REQ_TIMEOUT_MAX_SECONDS).map(Self)
    }
}

/// Refuses a span of nought and one past what its kind allows.
fn bounded(kind: &'static str, seconds: u32, max: u32) -> Result<u32, AgentError> {
    if seconds == 0 {
        return Err(AgentError::Zero { kind });
    }
    if seconds > max {
        return Err(AgentError::TooLarge {
            kind,
            value: u64::from(seconds),
            max: u64::from(max),
        });
    }
    Ok(seconds)
}

/// One tool a template asks for.
///
/// The shape alone is checked, not the name: the preset of system tools does not exist yet, and a
/// set to check against that is empty would refuse every tool there is.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct ToolName(Box<str>);

impl ToolName {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "tool name";

    /// Validates and wraps a tool name.
    pub fn new(raw: &str) -> Result<Self, AgentError> {
        Self::try_from(raw.to_owned())
    }

    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ToolName {
    type Error = AgentError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(AgentError::Empty { kind: Self::KIND });
        }
        if raw.len() > TOOL_NAME_MAX_BYTES {
            return Err(AgentError::TooLong {
                kind: Self::KIND,
                len: raw.len(),
                max: TOOL_NAME_MAX_BYTES,
            });
        }
        // Lower case only, unlike an id: a tool name is the name of something executable, and two
        // tools differing in case alone would be one tool on a sandbox that folds it.
        match raw
            .chars()
            .find(|ch| !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_')))
        {
            Some(ch) => Err(AgentError::IllegalChar {
                kind: Self::KIND,
                ch,
            }),
            None => Ok(Self(raw.into_boxed_str())),
        }
    }
}

/// Every tool a template asks for, each named once.
///
/// Empty is allowed: an agent with no tools is a conversation, which is a legitimate thing to
/// configure.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "Vec<ToolName>")]
pub struct ToolSet(Vec<ToolName>);

impl ToolSet {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "tool set";

    /// Validates and wraps a set, refusing a tool named twice.
    pub fn new(tools: Vec<ToolName>) -> Result<Self, AgentError> {
        Self::try_from(tools)
    }

    /// The tools, in the order the template named them.
    #[must_use]
    pub fn tools(&self) -> &[ToolName] {
        &self.0
    }

    /// Whether the template asks for no tools at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl TryFrom<Vec<ToolName>> for ToolSet {
    type Error = AgentError;

    fn try_from(tools: Vec<ToolName>) -> Result<Self, Self::Error> {
        if tools.len() > TOOL_SET_MAX {
            return Err(AgentError::TooMany {
                kind: Self::KIND,
                count: tools.len(),
                max: TOOL_SET_MAX,
            });
        }
        let mut seen = tools.clone();
        seen.sort_unstable();
        if let Some(pair) = seen.windows(2).find(|pair| pair[0] == pair[1]) {
            return Err(AgentError::Duplicate {
                kind: Self::KIND,
                value: pair[0].as_str().to_owned(),
            });
        }
        Ok(Self(tools))
    }
}

/// How a best-effort run picks its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum BestEffortStrategy {
    /// Put every upstream's answer together.
    Aggregate,
    /// Take one of them.
    One,
}

impl BestEffortStrategy {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "best effort strategy";

    /// The name this is stored under.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Aggregate => "aggr",
            Self::One => "one",
        }
    }

    /// Reads it back from that name, refusing one this code does not carry.
    pub fn named(name: &str) -> Result<Self, AgentError> {
        match name {
            "aggr" => Ok(Self::Aggregate),
            "one" => Ok(Self::One),
            other => Err(AgentError::Unknown {
                kind: Self::KIND,
                value: other.to_owned(),
            }),
        }
    }
}

impl TryFrom<String> for BestEffortStrategy {
    type Error = AgentError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::named(&raw)
    }
}

impl From<BestEffortStrategy> for String {
    fn from(strategy: BestEffortStrategy) -> Self {
        strategy.name().to_owned()
    }
}

/// The parts one agent configuration is built from.
///
/// Its own shape is the stored shape, so the spellings here are what a column holds and what the
/// api reads. A field this code does not carry is refused rather than ignored, the way a
/// misspelled configuration key is.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfigParts {
    /// Which voice the prompt speaks in.
    pub role: AgentRole,
    /// What the agent is told to be.
    pub prompt: Prompt,
    /// Every tool it asks for, and none where it asks for nothing.
    #[serde(default)]
    pub tool_set: ToolSet,
    /// How many turns one run may take.
    pub max_turns: MaxTurns,
    /// How long one run may last.
    pub time_limit: TimeLimit,
    /// How long one upstream request may wait.
    pub req_timeout: ReqTimeout,
    /// Whether it should ask every upstream rather than one.
    #[serde(default)]
    pub use_best_effort: bool,
    /// How such a run picks its answer, and none where it is left to the deployment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub best_effort_strategy: Option<BestEffortStrategy>,
}

/// What one agent would run under, with every part of it already refused or accepted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "AgentConfigParts", into = "AgentConfigParts")]
pub struct AgentConfig(AgentConfigParts);

impl AgentConfig {
    /// Assembles a configuration, refusing the parts that contradict each other.
    pub fn new(parts: AgentConfigParts) -> Result<Self, AgentError> {
        Self::try_from(parts)
    }

    /// Which voice the prompt speaks in.
    #[must_use]
    pub const fn role(&self) -> AgentRole {
        self.0.role
    }

    /// What the agent is told to be.
    #[must_use]
    pub const fn prompt(&self) -> &Prompt {
        &self.0.prompt
    }

    /// Every tool it asks for.
    #[must_use]
    pub const fn tool_set(&self) -> &ToolSet {
        &self.0.tool_set
    }

    /// How many turns one run may take.
    #[must_use]
    pub const fn max_turns(&self) -> MaxTurns {
        self.0.max_turns
    }

    /// How long one run may last.
    #[must_use]
    pub const fn time_limit(&self) -> TimeLimit {
        self.0.time_limit
    }

    /// How long one upstream request may wait.
    #[must_use]
    pub const fn req_timeout(&self) -> ReqTimeout {
        self.0.req_timeout
    }

    /// Whether it asks every upstream rather than one.
    ///
    /// Nothing reads this yet: the best-effort endpoint is unbuilt, and a template may say so
    /// ahead of it so that a deployment needs no second write when it lands.
    #[must_use]
    pub const fn uses_best_effort(&self) -> bool {
        self.0.use_best_effort
    }

    /// How such a run picks its answer, where the template says.
    #[must_use]
    pub const fn best_effort_strategy(&self) -> Option<BestEffortStrategy> {
        self.0.best_effort_strategy
    }
}

impl TryFrom<AgentConfigParts> for AgentConfig {
    type Error = AgentError;

    fn try_from(parts: AgentConfigParts) -> Result<Self, Self::Error> {
        if parts.req_timeout.seconds() > parts.time_limit.seconds() {
            return Err(AgentError::TimeoutOutlivesRun {
                timeout: parts.req_timeout.seconds(),
                limit: parts.time_limit.seconds(),
            });
        }
        if !parts.use_best_effort && parts.best_effort_strategy.is_some() {
            return Err(AgentError::StrategyWithoutBestEffort);
        }
        Ok(Self(parts))
    }
}

impl From<AgentConfig> for AgentConfigParts {
    fn from(config: AgentConfig) -> Self {
        config.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> ToolName {
        ToolName::new(name).unwrap()
    }

    fn parts() -> AgentConfigParts {
        AgentConfigParts {
            role: AgentRole::System,
            prompt: Prompt::new("You are a helpful assistant.").unwrap(),
            tool_set: ToolSet::new(vec![tool("python"), tool("bash")]).unwrap(),
            max_turns: MaxTurns::new(10).unwrap(),
            time_limit: TimeLimit::new(60).unwrap(),
            req_timeout: ReqTimeout::new(10).unwrap(),
            use_best_effort: false,
            best_effort_strategy: None,
        }
    }

    #[test]
    fn a_role_survives_the_name_it_is_stored_under() {
        for role in [AgentRole::System, AgentRole::User] {
            assert_eq!(AgentRole::named(role.name()).unwrap(), role);
        }
        assert_eq!(AgentRole::System.name(), "system");
        assert_eq!(AgentRole::User.name(), "user");
    }

    #[test]
    fn a_role_this_code_does_not_carry_is_refused() {
        assert_eq!(
            AgentRole::named("assistant"),
            Err(AgentError::Unknown {
                kind: AgentRole::KIND,
                value: "assistant".to_owned(),
            })
        );
    }

    #[test]
    fn a_role_reads_and_writes_as_the_string_the_column_holds() {
        let written = serde_json::to_string(&AgentRole::User).unwrap();
        assert_eq!(written, "\"user\"");
        assert_eq!(
            serde_json::from_str::<AgentRole>(&written).unwrap(),
            AgentRole::User
        );
        assert!(serde_json::from_str::<AgentRole>("\"nobody\"").is_err());
    }

    #[test]
    fn a_prompt_is_refused_empty_and_at_one_byte_past_the_ceiling() {
        assert_eq!(
            Prompt::new(""),
            Err(AgentError::Empty { kind: Prompt::KIND })
        );

        let longest = "a".repeat(PROMPT_MAX_BYTES);
        assert_eq!(Prompt::new(&longest).unwrap().as_str(), longest);

        let over = "a".repeat(PROMPT_MAX_BYTES + 1);
        assert_eq!(
            Prompt::new(&over),
            Err(AgentError::TooLong {
                kind: Prompt::KIND,
                len: PROMPT_MAX_BYTES + 1,
                max: PROMPT_MAX_BYTES,
            })
        );
    }

    #[test]
    fn a_count_of_turns_is_refused_at_nought_and_past_the_ceiling() {
        assert_eq!(
            MaxTurns::new(0),
            Err(AgentError::Zero {
                kind: MaxTurns::KIND
            })
        );
        assert_eq!(MaxTurns::new(MAX_TURNS_MAX).unwrap().get(), MAX_TURNS_MAX);
        assert_eq!(
            MaxTurns::new(MAX_TURNS_MAX + 1),
            Err(AgentError::TooLarge {
                kind: MaxTurns::KIND,
                value: u64::from(MAX_TURNS_MAX) + 1,
                max: u64::from(MAX_TURNS_MAX),
            })
        );
    }

    #[test]
    fn a_time_limit_is_refused_at_nought_and_past_a_day() {
        assert_eq!(
            TimeLimit::new(0),
            Err(AgentError::Zero {
                kind: TimeLimit::KIND
            })
        );
        assert_eq!(
            TimeLimit::new(TIME_LIMIT_MAX_SECONDS).unwrap().seconds(),
            TIME_LIMIT_MAX_SECONDS
        );
        assert!(matches!(
            TimeLimit::new(TIME_LIMIT_MAX_SECONDS + 1),
            Err(AgentError::TooLarge { .. })
        ));
    }

    #[test]
    fn a_request_timeout_is_refused_at_nought_and_past_an_hour() {
        assert_eq!(
            ReqTimeout::new(0),
            Err(AgentError::Zero {
                kind: ReqTimeout::KIND
            })
        );
        assert_eq!(
            ReqTimeout::new(REQ_TIMEOUT_MAX_SECONDS).unwrap().seconds(),
            REQ_TIMEOUT_MAX_SECONDS
        );
        assert!(matches!(
            ReqTimeout::new(REQ_TIMEOUT_MAX_SECONDS + 1),
            Err(AgentError::TooLarge { .. })
        ));
    }

    #[test]
    fn a_tool_name_takes_what_an_executable_is_called_and_nothing_else() {
        assert_eq!(tool("ripgrep-13").as_str(), "ripgrep-13");
        assert_eq!(tool("my_tool").as_str(), "my_tool");
        assert_eq!(
            ToolName::new(""),
            Err(AgentError::Empty {
                kind: ToolName::KIND
            })
        );
        for (raw, ch) in [("Python", 'P'), ("my tool", ' '), ("bin/bash", '/')] {
            assert_eq!(
                ToolName::new(raw),
                Err(AgentError::IllegalChar {
                    kind: ToolName::KIND,
                    ch,
                }),
                "{raw:?}"
            );
        }
        assert!(matches!(
            ToolName::new(&"a".repeat(TOOL_NAME_MAX_BYTES + 1)),
            Err(AgentError::TooLong { .. })
        ));
    }

    #[test]
    fn a_tool_set_keeps_the_order_it_was_named_in() {
        let set = ToolSet::new(vec![tool("python"), tool("bash"), tool("awk")]).unwrap();
        let named: Vec<&str> = set.tools().iter().map(ToolName::as_str).collect();
        assert_eq!(named, ["python", "bash", "awk"]);
        assert!(!set.is_empty());
    }

    #[test]
    fn a_tool_set_of_nothing_is_a_conversation_and_is_allowed() {
        let set = ToolSet::new(Vec::new()).unwrap();
        assert!(set.is_empty());
        assert_eq!(set, ToolSet::default());
    }

    #[test]
    fn a_tool_named_twice_is_refused_however_far_apart_the_two_sit() {
        assert_eq!(
            ToolSet::new(vec![tool("python"), tool("bash"), tool("python")]),
            Err(AgentError::Duplicate {
                kind: ToolSet::KIND,
                value: "python".to_owned(),
            })
        );
    }

    #[test]
    fn a_tool_set_past_what_it_holds_is_refused() {
        let many: Vec<ToolName> = (0..=TOOL_SET_MAX).map(|n| tool(&format!("t{n}"))).collect();
        assert_eq!(
            ToolSet::new(many),
            Err(AgentError::TooMany {
                kind: ToolSet::KIND,
                count: TOOL_SET_MAX + 1,
                max: TOOL_SET_MAX,
            })
        );
    }

    #[test]
    fn a_strategy_survives_the_name_it_is_stored_under() {
        for strategy in [BestEffortStrategy::Aggregate, BestEffortStrategy::One] {
            assert_eq!(
                BestEffortStrategy::named(strategy.name()).unwrap(),
                strategy
            );
        }
        assert_eq!(BestEffortStrategy::Aggregate.name(), "aggr");
        assert_eq!(BestEffortStrategy::One.name(), "one");
        assert_eq!(
            BestEffortStrategy::named("aggregate"),
            Err(AgentError::Unknown {
                kind: BestEffortStrategy::KIND,
                value: "aggregate".to_owned(),
            })
        );
    }

    #[test]
    fn a_configuration_answers_with_every_part_it_was_built_from() {
        let config = AgentConfig::new(parts()).unwrap();
        assert_eq!(config.role(), AgentRole::System);
        assert_eq!(config.prompt().as_str(), "You are a helpful assistant.");
        assert_eq!(config.tool_set().tools().len(), 2);
        assert_eq!(config.max_turns().get(), 10);
        assert_eq!(config.time_limit().seconds(), 60);
        assert_eq!(config.req_timeout().seconds(), 10);
        assert!(!config.uses_best_effort());
        assert_eq!(config.best_effort_strategy(), None);
    }

    #[test]
    fn a_request_may_not_be_allowed_to_outlive_its_own_run() {
        let mut parts = parts();
        parts.req_timeout = ReqTimeout::new(61).unwrap();
        assert_eq!(
            AgentConfig::new(parts),
            Err(AgentError::TimeoutOutlivesRun {
                timeout: 61,
                limit: 60,
            })
        );
    }

    #[test]
    fn a_request_timeout_equal_to_the_run_is_allowed() {
        let mut parts = parts();
        parts.req_timeout = ReqTimeout::new(60).unwrap();
        assert!(AgentConfig::new(parts).is_ok());
    }

    #[test]
    fn a_strategy_for_a_run_nobody_asked_for_is_refused() {
        let mut parts = parts();
        parts.best_effort_strategy = Some(BestEffortStrategy::One);
        assert_eq!(
            AgentConfig::new(parts),
            Err(AgentError::StrategyWithoutBestEffort)
        );
    }

    #[test]
    fn best_effort_is_carried_whether_or_not_a_strategy_names_how() {
        let mut asked = parts();
        asked.use_best_effort = true;
        let config = AgentConfig::new(asked.clone()).unwrap();
        assert!(config.uses_best_effort());
        assert_eq!(config.best_effort_strategy(), None);

        asked.best_effort_strategy = Some(BestEffortStrategy::Aggregate);
        let config = AgentConfig::new(asked).unwrap();
        assert_eq!(
            config.best_effort_strategy(),
            Some(BestEffortStrategy::Aggregate)
        );
    }

    #[test]
    fn a_configuration_survives_its_own_round_trip_through_the_column() {
        let config = AgentConfig::new(parts()).unwrap();
        let written = serde_json::to_string(&config).unwrap();
        assert_eq!(
            serde_json::from_str::<AgentConfig>(&written).unwrap(),
            config
        );
        assert!(
            !written.contains("best_effort_strategy"),
            "nothing is written where nothing was set: {written}"
        );
    }

    #[test]
    fn a_stored_configuration_is_refused_the_values_its_own_types_refuse() {
        let written = serde_json::to_string(&parts()).unwrap();
        let zeroed = written.replace("\"max_turns\":10", "\"max_turns\":0");
        let error = serde_json::from_str::<AgentConfig>(&zeroed).unwrap_err();
        assert!(error.to_string().contains("must not be zero"), "{error}");
    }

    #[test]
    fn a_stored_configuration_refuses_a_field_this_code_does_not_carry() {
        let written = serde_json::to_string(&parts()).unwrap();
        let extra = written.replace("{", "{\"temperature\":0.7,");
        let error = serde_json::from_str::<AgentConfig>(&extra).unwrap_err();
        assert!(error.to_string().contains("temperature"), "{error}");
    }
}
