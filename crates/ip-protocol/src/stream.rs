//! A streamed answer, read into canonical events and written back out as a family's own.
//!
//! This is the second sink the whole crate was shaped around: [`Event`] has been a
//! sequence since the first line of it, so streaming is a reader and a writer here and
//! not a second translation. A whole-body answer and a streamed one produce the same
//! events; only the framing differs.
//!
//! Framing itself is not here. Splitting a byte stream into server-sent events is
//! transport, and the gateway already does it correctly — including an event split
//! across reads and a `\r\n` split across them. What arrives here is one complete
//! event's text, and what leaves is one family's complete event text.
//!
//! Two families spread one thing over several events, and holding that together is
//! most of this module: a tool call's arguments arrive as fragments of a json string,
//! and the call is only whole when the family says so.

use std::collections::BTreeMap;

use ip_core::{Dialect, ModelName, TokenCount, Tokens};
use serde_json::{Value, json};

use crate::answer::{Event, Stop};
use crate::conversation::{ToolCallId, ToolName};

/// A tool call being assembled out of the pieces a family sends it in.
#[derive(Debug, Default, Clone)]
struct Building {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl Building {
    /// The call, once there is enough of it to be one.
    fn whole(&self) -> Option<Event> {
        let id = ToolCallId::new(self.id.as_deref()?).ok()?;
        let name = ToolName::new(self.name.as_deref()?).ok()?;
        let arguments = if self.arguments.is_empty() {
            json!({})
        } else {
            serde_json::from_str(&self.arguments).unwrap_or_else(|_| json!(self.arguments))
        };
        Some(Event::ToolCall {
            id,
            name,
            arguments,
        })
    }
}

/// Reads one family's streamed events into canonical ones.
#[derive(Debug)]
pub struct Reading {
    dialect: Dialect,
    building: BTreeMap<i64, Building>,
    named: bool,
}

impl Reading {
    /// A reader for the family that will be sending.
    #[must_use]
    pub fn new(dialect: Dialect) -> Self {
        Self {
            dialect,
            building: BTreeMap::new(),
            named: false,
        }
    }

    /// What one complete server-sent event means, which is sometimes nothing: a family
    /// may send keep-alives, and the pieces of a tool call mean nothing until it ends.
    pub fn event(&mut self, block: &str) -> Vec<Event> {
        let Some(data) = payload(block) else {
            return Vec::new();
        };
        if data.trim() == "[DONE]" {
            return self.end();
        }
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            return Vec::new();
        };
        match self.dialect {
            Dialect::OpenAi => self.openai(&value),
            Dialect::Anthropic => self.anthropic(&value),
            Dialect::Gemini => self.gemini(&value),
        }
    }

    /// Whatever is still held when the stream ends: a call whose family never said it
    /// was finished is still a call the caller asked for, so it goes out rather than
    /// being dropped for want of a closing event.
    pub fn end(&mut self) -> Vec<Event> {
        let held = std::mem::take(&mut self.building);
        held.values().filter_map(Building::whole).collect()
    }

    /// Names the model once, the first time a family says it.
    fn naming(&mut self, model: Option<&str>) -> Vec<Event> {
        if self.named {
            return Vec::new();
        }
        self.named = true;
        vec![Event::Started {
            model: model.and_then(|name| ModelName::new(name).ok()),
        }]
    }

    fn openai(&mut self, value: &Value) -> Vec<Event> {
        // A keep-alive carries none of this family's answer, and does not start one.
        if value.get("choices").is_none() && value.get("usage").is_none() {
            return Vec::new();
        }
        let mut events = self.naming(value.get("model").and_then(Value::as_str));
        let choice = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first());
        if let Some(choice) = choice {
            let delta = choice.get("delta").unwrap_or(choice);
            if let Some(text) = delta.get("content").and_then(Value::as_str)
                && !text.is_empty()
            {
                events.push(Event::Text(text.to_owned()));
            }
            for call in delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .unwrap_or(&Vec::new())
            {
                let at = call.get("index").and_then(Value::as_i64).unwrap_or(0);
                let building = self.building.entry(at).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    building.id = Some(id.to_owned());
                }
                if let Some(function) = call.get("function") {
                    if let Some(name) = function.get("name").and_then(Value::as_str) {
                        building.name = Some(name.to_owned());
                    }
                    if let Some(piece) = function.get("arguments").and_then(Value::as_str) {
                        building.arguments.push_str(piece);
                    }
                }
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                // Every call this answer was building is whole by the time it stops.
                events.extend(self.end());
                events.push(Event::Stopped(match reason {
                    "stop" => Stop::End,
                    "length" => Stop::Length,
                    "tool_calls" | "function_call" => Stop::ToolUse,
                    "content_filter" => Stop::Filtered,
                    other => Stop::Other(other.to_owned()),
                }));
            }
        }
        if let Some(usage) = value.get("usage")
            && !usage.is_null()
        {
            events.push(Event::Spent(Tokens::new(
                counted(usage.get("prompt_tokens")),
                counted(usage.get("completion_tokens")),
            )));
        }
        events
    }

    fn anthropic(&mut self, value: &Value) -> Vec<Event> {
        let mut events = Vec::new();
        match value.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                let message = value.get("message");
                events.extend(self.naming(message.and_then(|held| held.get("model")?.as_str())));
                if let Some(usage) = message.and_then(|held| held.get("usage")) {
                    events.push(Event::Spent(Tokens::new(
                        counted(usage.get("input_tokens")),
                        counted(usage.get("output_tokens")),
                    )));
                }
            }
            Some("content_block_start") => {
                let at = value.get("index").and_then(Value::as_i64).unwrap_or(0);
                let block = value.get("content_block");
                if block.and_then(|held| held.get("type")?.as_str()) == Some("tool_use") {
                    let building = self.building.entry(at).or_default();
                    building.id = block
                        .and_then(|held| held.get("id")?.as_str())
                        .map(str::to_owned);
                    building.name = block
                        .and_then(|held| held.get("name")?.as_str())
                        .map(str::to_owned);
                } else if let Some(text) = block.and_then(|held| held.get("text")?.as_str())
                    && !text.is_empty()
                {
                    events.push(Event::Text(text.to_owned()));
                }
            }
            Some("content_block_delta") => {
                let at = value.get("index").and_then(Value::as_i64).unwrap_or(0);
                let delta = value.get("delta");
                if let Some(text) = delta.and_then(|held| held.get("text")?.as_str()) {
                    events.push(Event::Text(text.to_owned()));
                } else if let Some(piece) =
                    delta.and_then(|held| held.get("partial_json")?.as_str())
                {
                    self.building
                        .entry(at)
                        .or_default()
                        .arguments
                        .push_str(piece);
                }
            }
            Some("content_block_stop") => {
                let at = value.get("index").and_then(Value::as_i64).unwrap_or(0);
                if let Some(building) = self.building.remove(&at)
                    && let Some(call) = building.whole()
                {
                    events.push(call);
                }
            }
            Some("message_delta") => {
                if let Some(reason) = value
                    .get("delta")
                    .and_then(|held| held.get("stop_reason")?.as_str())
                {
                    events.push(Event::Stopped(match reason {
                        "end_turn" => Stop::End,
                        "max_tokens" => Stop::Length,
                        "tool_use" => Stop::ToolUse,
                        "refusal" => Stop::Filtered,
                        other => Stop::Other(other.to_owned()),
                    }));
                }
                if let Some(usage) = value.get("usage") {
                    events.push(Event::Spent(Tokens::new(
                        counted(usage.get("input_tokens")),
                        counted(usage.get("output_tokens")),
                    )));
                }
            }
            _ => {}
        }
        events
    }

    fn gemini(&mut self, value: &Value) -> Vec<Event> {
        if value.get("candidates").is_none() && value.get("usageMetadata").is_none() {
            return Vec::new();
        }
        let mut events = self.naming(value.get("modelVersion").and_then(Value::as_str));
        let candidate = value
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first());
        if let Some(candidate) = candidate {
            for part in candidate
                .get("content")
                .and_then(|content| content.get("parts"))
                .and_then(Value::as_array)
                .unwrap_or(&Vec::new())
            {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    events.push(Event::Text(text.to_owned()));
                } else if let Some(call) = part.get("functionCall") {
                    let Some(name) = call.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    let at = self.building.len() as i64;
                    let building = self.building.entry(at).or_default();
                    building.id = Some(format!("{name}#{at}"));
                    building.name = Some(name.to_owned());
                    building.arguments = call
                        .get("args")
                        .map(ToString::to_string)
                        .unwrap_or_default();
                }
            }
            if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                events.extend(self.end());
                events.push(Event::Stopped(match reason {
                    "STOP" => Stop::End,
                    "MAX_TOKENS" => Stop::Length,
                    "SAFETY" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "RECITATION" => {
                        Stop::Filtered
                    }
                    other => Stop::Other(other.to_owned()),
                }));
            }
        }
        if let Some(usage) = value.get("usageMetadata") {
            events.push(Event::Spent(Tokens::new(
                counted(usage.get("promptTokenCount")),
                counted(usage.get("candidatesTokenCount")),
            )));
        }
        events
    }
}

/// Writes canonical events as one family's own streamed events.
///
/// Each family frames its own: two of them send bare `data:` lines and the third names
/// every event on a line of its own, which its clients read.
#[derive(Debug)]
pub struct Writing {
    dialect: Dialect,
    opened: bool,
    blocks: usize,
}

impl Writing {
    /// A writer for the family that will be reading.
    #[must_use]
    pub fn new(dialect: Dialect) -> Self {
        Self {
            dialect,
            opened: false,
            blocks: 0,
        }
    }

    /// The complete server-sent events this one canonical event becomes, framing and
    /// all. Empty where a family has nothing to say about it.
    pub fn event(&mut self, event: &Event) -> String {
        match self.dialect {
            Dialect::OpenAi => self.openai(event),
            Dialect::Anthropic => self.anthropic(event),
            Dialect::Gemini => self.gemini(event),
        }
    }

    /// How this family says a stream is over.
    pub fn end(&mut self) -> String {
        match self.dialect {
            Dialect::OpenAi => "data: [DONE]\n\n".to_owned(),
            Dialect::Anthropic if self.opened => {
                named("message_stop", &json!({"type": "message_stop"}))
            }
            Dialect::Anthropic | Dialect::Gemini => String::new(),
        }
    }

    fn openai(&mut self, event: &Event) -> String {
        let chunk = match event {
            Event::Started { model } => json!({
                "object": "chat.completion.chunk",
                "model": model.as_ref().map(ModelName::as_str),
                "choices": [{"index": 0, "delta": {"role": "assistant"}}],
            }),
            Event::Text(text) => json!({
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {"content": text}}],
            }),
            Event::ToolCall {
                id,
                name,
                arguments,
            } => {
                let at = self.blocks;
                self.blocks += 1;
                json!({
                    "object": "chat.completion.chunk",
                    "choices": [{"index": 0, "delta": {"tool_calls": [{
                        "index": at,
                        "id": id.as_str(),
                        "type": "function",
                        "function": {"name": name.as_str(), "arguments": arguments.to_string()},
                    }]}}],
                })
            }
            Event::Stopped(stop) => json!({
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {}, "finish_reason": openai_stop(stop)}],
            }),
            Event::Spent(tokens) => json!({
                "object": "chat.completion.chunk",
                "choices": [],
                "usage": {
                    "prompt_tokens": tokens.input.get(),
                    "completion_tokens": tokens.output.get(),
                    "total_tokens": tokens.total(),
                },
            }),
        };
        format!("data: {chunk}\n\n")
    }

    fn anthropic(&mut self, event: &Event) -> String {
        match event {
            Event::Started { model } => {
                self.opened = true;
                named(
                    "message_start",
                    &json!({
                        "type": "message_start",
                        "message": {
                            "type": "message",
                            "role": "assistant",
                            "model": model.as_ref().map(ModelName::as_str),
                            "content": [],
                        },
                    }),
                )
            }
            Event::Text(text) => {
                let at = self.blocks;
                self.blocks += 1;
                [
                    named(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": at,
                            "content_block": {"type": "text", "text": ""},
                        }),
                    ),
                    named(
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": at,
                            "delta": {"type": "text_delta", "text": text},
                        }),
                    ),
                    named(
                        "content_block_stop",
                        &json!({"type": "content_block_stop", "index": at}),
                    ),
                ]
                .concat()
            }
            Event::ToolCall {
                id,
                name,
                arguments,
            } => {
                let at = self.blocks;
                self.blocks += 1;
                [
                    named(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": at,
                            "content_block": {
                                "type": "tool_use",
                                "id": id.as_str(),
                                "name": name.as_str(),
                                "input": {},
                            },
                        }),
                    ),
                    named(
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": at,
                            "delta": {
                                "type": "input_json_delta",
                                "partial_json": arguments.to_string(),
                            },
                        }),
                    ),
                    named(
                        "content_block_stop",
                        &json!({"type": "content_block_stop", "index": at}),
                    ),
                ]
                .concat()
            }
            Event::Stopped(stop) => named(
                "message_delta",
                &json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": anthropic_stop(stop)},
                }),
            ),
            Event::Spent(tokens) => named(
                "message_delta",
                &json!({
                    "type": "message_delta",
                    "delta": {},
                    "usage": {
                        "input_tokens": tokens.input.get(),
                        "output_tokens": tokens.output.get(),
                    },
                }),
            ),
        }
    }

    fn gemini(&mut self, event: &Event) -> String {
        let chunk = match event {
            // This family names its model on the chunks themselves and opens with none.
            Event::Started { .. } => return String::new(),
            Event::Text(text) => {
                json!({"candidates": [{"content": {"role": "model", "parts": [{"text": text}]}}]})
            }
            Event::ToolCall {
                name, arguments, ..
            } => json!({"candidates": [{"content": {"role": "model", "parts": [
                {"functionCall": {"name": name.as_str(), "args": arguments}},
            ]}}]}),
            Event::Stopped(stop) => json!({
                "candidates": [{
                    "content": {"role": "model", "parts": []},
                    "finishReason": gemini_stop(stop),
                }],
            }),
            Event::Spent(tokens) => json!({
                "candidates": [],
                "usageMetadata": {
                    "promptTokenCount": tokens.input.get(),
                    "candidatesTokenCount": tokens.output.get(),
                    "totalTokenCount": tokens.total(),
                },
            }),
        };
        format!("data: {chunk}\n\n")
    }
}

/// An event this family names on a line of its own, as its clients read it.
fn named(name: &str, body: &Value) -> String {
    format!("event: {name}\ndata: {body}\n\n")
}

/// The `data:` of one event, joined where a family spread it over several lines.
fn payload(block: &str) -> Option<String> {
    let mut data = String::new();
    for line in block.lines() {
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        if !data.is_empty() {
            data.push('\n');
        }
        data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
    }
    (!data.is_empty()).then_some(data)
}

/// How the OpenAI family spells a stop, its set being closed.
fn openai_stop(stop: &Stop) -> &'static str {
    match stop {
        Stop::End | Stop::Other(_) => "stop",
        Stop::Length => "length",
        Stop::ToolUse => "tool_calls",
        Stop::Filtered => "content_filter",
    }
}

/// How the Anthropic family spells a stop.
fn anthropic_stop(stop: &Stop) -> &str {
    match stop {
        Stop::End => "end_turn",
        Stop::Length => "max_tokens",
        Stop::ToolUse => "tool_use",
        Stop::Filtered => "refusal",
        Stop::Other(reason) => reason.as_str(),
    }
}

/// How the Gemini family spells a stop, which has no word for wanting a tool.
fn gemini_stop(stop: &Stop) -> &str {
    match stop {
        Stop::End | Stop::ToolUse => "STOP",
        Stop::Length => "MAX_TOKENS",
        Stop::Filtered => "SAFETY",
        Stop::Other(reason) => reason.as_str(),
    }
}

/// A stored count, or none of them.
fn counted(value: Option<&Value>) -> TokenCount {
    value
        .and_then(Value::as_u64)
        .and_then(|count| u32::try_from(count).ok())
        .map_or(TokenCount::ZERO, TokenCount::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::answer::Gathered;

    /// Everything one family would send for these events, as its own stream, read back
    /// into canonical events again. Framing is the gateway's job in production and the
    /// splitting here stands in for it.
    fn around(dialect: Dialect, events: &[Event]) -> Vec<Event> {
        let mut writing = Writing::new(dialect);
        let mut sent = String::new();
        for event in events {
            sent.push_str(&writing.event(event));
        }
        sent.push_str(&writing.end());

        let mut reading = Reading::new(dialect);
        let mut read = Vec::new();
        for block in sent.split("\n\n").filter(|block| !block.trim().is_empty()) {
            read.extend(reading.event(block));
        }
        read.extend(reading.end());
        read
    }

    fn spoken() -> Vec<Event> {
        vec![
            Event::Started {
                model: Some(ModelName::new("shared").unwrap()),
            },
            Event::Text("forty-".to_owned()),
            Event::Text("two".to_owned()),
            Event::Stopped(Stop::End),
            Event::Spent(Tokens::new(TokenCount::new(10), TokenCount::new(2))),
        ]
    }

    #[test]
    fn every_family_carries_what_was_said_through_its_own_stream() {
        for dialect in Dialect::ALL {
            let gathered = Gathered::of(&around(dialect, &spoken()));
            assert_eq!(gathered.text, "forty-two", "{dialect} lost the words");
            assert_eq!(gathered.stop, Some(Stop::End), "{dialect} lost the ending");
            assert_eq!(
                gathered.spent.map(|spent| spent.total()),
                Some(12),
                "{dialect} lost what it cost"
            );
        }
    }

    #[test]
    fn two_families_name_the_model_and_the_third_names_it_on_its_chunks() {
        for dialect in [Dialect::OpenAi, Dialect::Anthropic] {
            let gathered = Gathered::of(&around(dialect, &spoken()));
            assert_eq!(
                gathered.model.map(|model| model.as_str().to_owned()),
                Some("shared".to_owned()),
                "{dialect} lost the model"
            );
        }
        // Gemini opens with no event of its own: it names the model on the chunks that
        // carry content, and a stream this gateway writes carries none to name it on.
        assert_eq!(
            Gathered::of(&around(Dialect::Gemini, &spoken())).model,
            None
        );
    }

    fn calling() -> Vec<Event> {
        vec![
            Event::ToolCall {
                id: ToolCallId::new("call_1").unwrap(),
                name: ToolName::new("search").unwrap(),
                arguments: json!({"q": "rust"}),
            },
            Event::Stopped(Stop::ToolUse),
        ]
    }

    #[test]
    fn every_family_carries_a_tool_call_through_its_own_stream() {
        for dialect in Dialect::ALL {
            let read = around(dialect, &calling());
            let call = read
                .iter()
                .find(|event| matches!(event, Event::ToolCall { .. }))
                .unwrap_or_else(|| panic!("{dialect} lost the call: {read:?}"));
            let Event::ToolCall {
                id,
                name,
                arguments,
            } = call
            else {
                unreachable!("a call is a call")
            };
            assert_eq!(name.as_str(), "search", "{dialect} lost the name");
            assert_eq!(
                arguments,
                &json!({"q": "rust"}),
                "{dialect} lost the arguments"
            );
            match dialect {
                // Gemini joins a call to its result by the function's name, so an id
                // put into it does not come back out; one is synthesised instead.
                Dialect::Gemini => assert_eq!(id.as_str(), "search#0"),
                _ => assert_eq!(id.as_str(), "call_1", "{dialect} lost the id"),
            }
        }
    }

    #[test]
    fn a_family_that_wants_a_tool_says_so_in_its_own_words() {
        for (dialect, expected) in [
            (Dialect::OpenAi, Stop::ToolUse),
            (Dialect::Anthropic, Stop::ToolUse),
            // This family has no reason for wanting a tool: it stops normally and
            // leaves a call behind, so the crossing back reads an ordinary ending.
            (Dialect::Gemini, Stop::End),
        ] {
            let read = around(dialect, &calling());
            assert_eq!(
                Gathered::of(&read).stop,
                Some(expected.clone()),
                "{dialect} said the wrong thing about why it stopped"
            );
        }
    }

    #[test]
    fn every_way_a_family_withholds_an_answer_reads_as_withheld() {
        for dialect in Dialect::ALL {
            let withheld = [Event::Stopped(Stop::Filtered)];
            assert_eq!(
                Gathered::of(&around(dialect, &withheld)).stop,
                Some(Stop::Filtered),
                "{dialect} did not carry a withheld answer"
            );
        }
        for (dialect, length) in
            Dialect::ALL.map(|dialect| (dialect, [Event::Stopped(Stop::Length)]))
        {
            assert_eq!(
                Gathered::of(&around(dialect, &length)).stop,
                Some(Stop::Length),
                "{dialect} did not carry a ceiling it hit"
            );
        }
    }

    #[test]
    fn an_event_that_says_nothing_this_layer_reads_means_nothing() {
        for dialect in Dialect::ALL {
            let mut reading = Reading::new(dialect);
            // A comment, a keep-alive, a shape no family sends, and plain noise.
            assert!(reading.event(": keeping the connection open").is_empty());
            assert!(reading.event("event: ping\ndata: {}").is_empty());
            assert!(reading.event("data: not json at all").is_empty());
            assert!(reading.event("").is_empty());
        }
    }

    /// A family that never said its call was finished still asked for one, and the
    /// caller is owed it when the stream ends.
    #[test]
    fn a_call_left_unfinished_still_reaches_the_caller() {
        let mut reading = Reading::new(Dialect::Anthropic);
        let started = reading.event(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"search\"}}",
        );
        assert!(started.is_empty(), "a call is not whole when it begins");
        let ended = reading.end();
        assert_eq!(ended.len(), 1, "the call was dropped: {ended:?}");
    }

    /// The one line of a stream that says it is over, which only one family sends.
    #[test]
    fn only_the_family_that_ends_its_streams_says_so() {
        assert_eq!(Writing::new(Dialect::OpenAi).end(), "data: [DONE]\n\n");
        assert_eq!(Writing::new(Dialect::Gemini).end(), "");
        // This family closes a message it opened, and closes nothing it did not.
        assert_eq!(Writing::new(Dialect::Anthropic).end(), "");
        let mut opened = Writing::new(Dialect::Anthropic);
        opened.event(&Event::Started { model: None });
        assert!(opened.end().contains("message_stop"));
    }

    #[test]
    fn a_data_line_spread_over_several_is_read_as_one() {
        let mut reading = Reading::new(Dialect::OpenAi);
        let events =
            reading.event("data: {\"choices\":[{\"delta\":\ndata: {\"content\":\"split\"}}]}");
        assert_eq!(
            events,
            vec![
                Event::Started { model: None },
                Event::Text("split".to_owned())
            ]
        );
    }
}
