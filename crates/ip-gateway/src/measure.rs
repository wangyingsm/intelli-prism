//! What an answer says it cost, read out of the answer itself.
//!
//! Every upstream family spells the same two numbers differently, and a streamed answer spreads
//! them over several events. The keys of all three families are read here, whichever arrives.

use ip_core::{ModelName, TokenCount, Tokens};
use serde_json::Value;

/// Where an answer says how many tokens it took in, by family.
const INPUT_KEYS: [&str; 3] = ["input_tokens", "prompt_tokens", "promptTokenCount"];

/// Where it says how many it answered with.
const OUTPUT_KEYS: [&str; 3] = ["output_tokens", "completion_tokens", "candidatesTokenCount"];

/// What an answer cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Measured {
    /// The model that answered, when the answer named one.
    pub model: Option<ModelName>,
    /// What it spent in each direction.
    pub tokens: Tokens,
}

/// Reads what an answer cost: a whole body at once, or a stream event by event as it passes.
///
/// Every family reports a running total rather than an increment, so a count already seen is
/// only ever replaced by a larger one and an event repeating itself changes nothing.
#[derive(Debug, Default)]
pub struct Measuring {
    model: Option<ModelName>,
    input: Option<TokenCount>,
    output: Option<TokenCount>,
}

impl Measuring {
    /// Reads whatever passed: a json answer, or one or more events of a stream.
    pub fn read(&mut self, bytes: &[u8]) {
        if let Ok(value) = serde_json::from_slice::<Value>(bytes) {
            self.fold(&value);
            return;
        }
        for line in bytes.split(|byte| *byte == b'\n') {
            let Some(data) = strip_data(line) else {
                continue;
            };
            if let Ok(value) = serde_json::from_slice::<Value>(data) {
                self.fold(&value);
            }
        }
    }

    /// What the answer said it cost, or nothing when it said neither count.
    pub fn measured(self) -> Option<Measured> {
        let (input, output) = (self.input, self.output);
        if input.is_none() && output.is_none() {
            return None;
        }
        Some(Measured {
            model: self.model,
            tokens: Tokens::new(input.unwrap_or_default(), output.unwrap_or_default()),
        })
    }

    /// Takes whatever one json object carries, keeping the largest count seen.
    fn fold(&mut self, value: &Value) {
        if self.model.is_none() {
            self.model = model_of(value);
        }
        let Some(usage) = usage_of(value) else {
            return;
        };
        self.input = self.input.max(count(usage, &INPUT_KEYS));
        self.output = self.output.max(count(usage, &OUTPUT_KEYS));
    }
}

/// Reads what a whole answer cost.
pub fn measure(body: &[u8]) -> Option<Measured> {
    let mut measuring = Measuring::default();
    measuring.read(body);
    measuring.measured()
}

/// The data an event line carries, without the prefix or the space after it.
fn strip_data(line: &[u8]) -> Option<&[u8]> {
    let data = line.strip_prefix(b"data:")?;
    Some(data.strip_prefix(b" ").unwrap_or(data))
}

/// Where a body or an event holds its counts.
fn usage_of(value: &Value) -> Option<&Value> {
    value
        .get("usage")
        .or_else(|| value.pointer("/message/usage"))
        .or_else(|| value.get("usageMetadata"))
}

/// The model an answer names, whichever key it names it under.
fn model_of(value: &Value) -> Option<ModelName> {
    let named = value
        .get("model")
        .or_else(|| value.pointer("/message/model"))
        .or_else(|| value.get("modelVersion"))?;
    ModelName::new(named.as_str()?).ok()
}

/// One count, under whichever of the keys the family spells it.
fn count(usage: &Value, keys: &[&str]) -> Option<TokenCount> {
    let counted = keys.iter().find_map(|key| usage.get(key)?.as_u64())?;
    Some(TokenCount::new(u32::try_from(counted).unwrap_or(u32::MAX)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(name: &str) -> Option<ModelName> {
        Some(ModelName::new(name).unwrap())
    }

    fn tokens(input: u32, output: u32) -> Tokens {
        Tokens::new(TokenCount::new(input), TokenCount::new(output))
    }

    #[test]
    fn an_anthropic_answer_names_its_model_and_both_counts() {
        let body = br#"{
            "id": "msg_1",
            "model": "claude-opus-5",
            "content": [{"type": "text", "text": "hi"}],
            "usage": {"input_tokens": 120, "output_tokens": 30}
        }"#;
        assert_eq!(
            measure(body).unwrap(),
            Measured {
                model: model("claude-opus-5"),
                tokens: tokens(120, 30),
            }
        );
    }

    #[test]
    fn an_openai_answer_spells_the_same_counts_its_own_way() {
        let body = br#"{
            "id": "chatcmpl-1",
            "model": "gpt-5",
            "usage": {"prompt_tokens": 9, "completion_tokens": 4, "total_tokens": 13}
        }"#;
        assert_eq!(
            measure(body).unwrap(),
            Measured {
                model: model("gpt-5"),
                tokens: tokens(9, 4),
            }
        );
    }

    #[test]
    fn a_gemini_answer_spells_them_a_third_way() {
        let body = br#"{
            "modelVersion": "gemini-3-pro",
            "usageMetadata": {
                "promptTokenCount": 11,
                "candidatesTokenCount": 5,
                "totalTokenCount": 16
            }
        }"#;
        assert_eq!(
            measure(body).unwrap(),
            Measured {
                model: model("gemini-3-pro"),
                tokens: tokens(11, 5),
            }
        );
    }

    #[test]
    fn an_anthropic_stream_carries_its_counts_across_two_events() {
        let stream = b"event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":120,\"output_tokens\":1}}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hi\"}}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":30}}\n\
\n";
        let mut measuring = Measuring::default();
        for event in stream.split(|byte| *byte == b'\n') {
            measuring.read(event);
        }
        assert_eq!(
            measuring.measured().unwrap(),
            Measured {
                model: model("claude-opus-5"),
                tokens: tokens(120, 30),
            }
        );
    }

    #[test]
    fn an_openai_stream_says_what_it_cost_in_its_last_chunk() {
        let mut measuring = Measuring::default();
        measuring.read(b"data: {\"model\":\"gpt-5\",\"choices\":[{\"delta\":{\"content\":\"h\"}}],\"usage\":null}\n");
        measuring.read(b"data: {\"model\":\"gpt-5\",\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":4}}\n");
        measuring.read(b"data: [DONE]\n");
        assert_eq!(
            measuring.measured().unwrap(),
            Measured {
                model: model("gpt-5"),
                tokens: tokens(9, 4),
            }
        );
    }

    #[test]
    fn a_running_total_never_goes_backwards() {
        let mut measuring = Measuring::default();
        measuring.read(br#"{"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":40}}"#);
        measuring.read(br#"{"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":12}}"#);
        assert_eq!(measuring.measured().unwrap().tokens, tokens(11, 40));
    }

    #[test]
    fn the_first_model_named_is_the_one_that_answered() {
        let mut measuring = Measuring::default();
        measuring.read(br#"{"model":"gpt-5","usage":{"prompt_tokens":1}}"#);
        measuring.read(br#"{"model":"gpt-5-mini"}"#);
        assert_eq!(measuring.measured().unwrap().model, model("gpt-5"));
    }

    #[test]
    fn an_answer_that_says_nothing_about_what_it_cost_is_not_measured() {
        assert_eq!(measure(br#"{"id":"msg_1","content":[]}"#), None);
        assert_eq!(measure(b"not json at all"), None);
        assert_eq!(measure(b""), None);
        assert_eq!(measure(b"data: [DONE]\n"), None);
    }

    #[test]
    fn an_answer_naming_only_a_model_is_not_measured() {
        assert_eq!(measure(br#"{"model":"claude-opus-5"}"#), None);
    }

    #[test]
    fn one_count_without_the_other_is_still_a_measurement() {
        assert_eq!(
            measure(br#"{"usage":{"output_tokens":7}}"#).unwrap().tokens,
            tokens(0, 7)
        );
    }

    #[test]
    fn a_count_no_count_can_hold_is_the_largest_one_that_can() {
        let body = br#"{"usage":{"input_tokens":99999999999}}"#;
        assert_eq!(
            measure(body).unwrap().tokens.input,
            TokenCount::new(u32::MAX)
        );
    }

    #[test]
    fn a_model_name_the_answer_spoils_is_left_unnamed() {
        let body = br#"{"model":"a name with spaces","usage":{"input_tokens":1}}"#;
        assert_eq!(measure(body).unwrap().model, None);
    }
}
