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
    carry: Vec<u8>,
    streaming: bool,
    overrun: bool,
}

/// How much of an answer is held at once while the rest of it is still coming.
///
/// An answer is json only once all of it is here, so what has not parsed is kept; one longer
/// than this is past measuring rather than worth holding in memory.
const CARRY_LIMIT: usize = 1 << 20;

impl Measuring {
    /// Reads whatever passed: a json answer whole or in pieces, or a stream event by event.
    ///
    /// **An upstream sends a whole answer in as many pieces as it likes**, and no piece of a
    /// json document is one, so what has not parsed yet is held until the rest arrives. A
    /// stream instead reports its totals inside one of its events, so events are folded and
    /// dropped as they complete and nothing accumulates over the length of the answer.
    pub fn read(&mut self, bytes: &[u8]) {
        // Whatever this piece carries whole, which for a stream is where its totals are.
        for line in bytes.split(|byte| *byte == b'\n') {
            if let Some(data) = strip_data(line) {
                self.streaming = true;
                if let Ok(value) = serde_json::from_slice::<Value>(data) {
                    self.fold(&value);
                }
            }
        }
        if self.overrun {
            return;
        }
        if self.carry.len() + bytes.len() > CARRY_LIMIT {
            self.overrun = true;
            self.carry = Vec::new();
            return;
        }
        let mut carry = std::mem::take(&mut self.carry);
        carry.extend_from_slice(bytes);

        // An event the pieces split in the middle of, joined up here. Folding one twice
        // changes nothing, since a count only ever rises to the largest seen.
        let mut complete = 0;
        while let Some(at) = carry[complete..].iter().position(|byte| *byte == b'\n') {
            let line = carry[complete..complete + at].to_vec();
            complete += at + 1;
            if let Some(data) = strip_data(&line) {
                self.streaming = true;
                if let Ok(value) = serde_json::from_slice::<Value>(data) {
                    self.fold(&value);
                }
            }
        }
        if self.streaming {
            carry.drain(..complete);
            self.carry = carry;
            return;
        }
        if let Ok(value) = serde_json::from_slice::<Value>(&carry) {
            self.fold(&value);
            carry.clear();
        }
        self.carry = carry;
    }

    /// How much of an answer is being held while the rest of it comes.
    #[cfg(test)]
    fn held(&self) -> usize {
        self.carry.len()
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

    /// What every test here used to assume and no upstream obliges: that a whole answer
    /// arrives in one piece. A real provider sends it chunked, and no chunk of a json
    /// document is a json document, so measuring a piece at a time measured nothing.
    #[test]
    fn a_whole_answer_split_across_pieces_is_measured() {
        let body =
            br#"{"model":"deepseek-chat","usage":{"prompt_tokens":9887,"completion_tokens":5356}}"#;

        for at in 1..body.len() {
            let mut measuring = Measuring::default();
            let (first, rest) = body.split_at(at);
            measuring.read(first);
            measuring.read(rest);

            let measured = measuring
                .measured()
                .unwrap_or_else(|| panic!("nothing measured when split at {at}"));
            assert_eq!(measured.tokens.input.get(), 9887, "split at {at}");
            assert_eq!(measured.tokens.output.get(), 5356, "split at {at}");
        }
    }

    /// One byte at a time is the same answer, which is the strongest form of the above.
    #[test]
    fn a_whole_answer_arriving_a_byte_at_a_time_is_measured() {
        let body = br#"{"usage":{"input_tokens":7,"output_tokens":3}}"#;
        let mut measuring = Measuring::default();

        for byte in body {
            measuring.read(&[*byte]);
        }

        let measured = measuring.measured().unwrap();
        assert_eq!(
            (measured.tokens.input.get(), measured.tokens.output.get()),
            (7, 3)
        );
    }

    /// An answer whose json carries newlines is still one document, not a stream.
    #[test]
    fn a_pretty_printed_answer_is_not_mistaken_for_a_stream() {
        let body = b"{\n  \"usage\": {\n    \"prompt_tokens\": 12\n  }\n}";
        let mut measuring = Measuring::default();

        measuring.read(&body[..20]);
        measuring.read(&body[20..]);

        assert_eq!(measuring.measured().unwrap().tokens.input.get(), 12);
    }

    #[test]
    fn a_streamed_event_split_across_pieces_is_measured() {
        let event = b"data: {\"model\":\"gpt-5\",\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":4}}\n";
        let mut measuring = Measuring::default();

        measuring.read(&event[..30]);
        measuring.read(&event[30..]);

        let measured = measuring.measured().unwrap();
        assert_eq!(
            (measured.tokens.input.get(), measured.tokens.output.get()),
            (9, 4)
        );
    }

    /// A stream is folded and dropped event by event, so a long one is measured without
    /// the whole of it ever being held — which is what the carry would otherwise do.
    #[test]
    fn a_long_stream_is_measured_without_being_held() {
        let mut measuring = Measuring::default();
        let filler = "x".repeat(4096);

        for _ in 0..512 {
            let event =
                format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{filler}\"}}}}]}}\n");
            measuring.read(event.as_bytes());
        }
        measuring.read(b"data: {\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":6}}\n");

        assert!(
            measuring.held() < 2 * filler.len(),
            "a stream held {} bytes, so it is accumulating the whole answer",
            measuring.held()
        );
        assert!(!measuring.overrun, "a stream ran the carry out");
        let measured = measuring.measured().expect("the totals were read");
        assert_eq!(measured.tokens.input.get(), 5);
    }

    /// A whole answer past what is worth holding measures nothing rather than being kept.
    #[test]
    fn an_answer_too_long_to_hold_is_left_unmeasured() {
        let mut measuring = Measuring::default();

        measuring.read(&vec![b' '; CARRY_LIMIT + 1]);
        measuring.read(br#"{"usage":{"input_tokens":1}}"#);

        assert!(measuring.measured().is_none());
    }
}
