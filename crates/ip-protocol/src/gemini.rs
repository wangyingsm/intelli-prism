//! The Gemini family, read into the canonical shape and written back out of it.
//!
//! This is the family the gateway could not read at all before this crate existed: it
//! calls its messages `contents` and their pieces `parts`, so every feature that looked
//! for `messages` found nothing and treated a whole provider's conversations as if they
//! were embeddings.
//!
//! It also disagrees with the other two about how a tool call is identified. They carry
//! an id on the call and quote it on the result; this family carries only the function's
//! name on both, and joins them by name and order. Reading synthesises an id from that
//! pair so the canonical shape has one, and writing resolves an id back to the name of
//! the call it belongs to.

use std::collections::HashMap;

use ip_core::{ModelName, TokenCount, Tokens};
use serde_json::{Map, Value, json};

use crate::answer::{Event, Gathered, Stop};
use crate::conversation::{
    Block, Conversation, Dropped, Message, Role, Sampling, Tool, ToolCallId, ToolName,
};
use crate::error::ProtocolError;

/// Fields this family sends that the canonical shape does not carry.
const UNCARRIED: [&str; 3] = ["safetySettings", "cachedContent", "toolConfig"];

/// The id synthesised for a call this family identifies only by name: the function's
/// name and which call to it this is, so two calls to one function in a turn stay apart
/// and the responses pair with them in the order they were made.
fn synthesised(name: &str, nth: usize) -> Result<ToolCallId, ProtocolError> {
    ToolCallId::new(&format!("{name}#{nth}"))
}

/// Reads a generate-content request.
///
/// # Errors
///
/// Refuses a body that is not json, and one that carries no `contents`.
pub fn read(body: &[u8]) -> Result<(Conversation, Dropped), ProtocolError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| ProtocolError::NotJson(error.to_string()))?;
    let contents = value
        .get("contents")
        .and_then(Value::as_array)
        .ok_or(ProtocolError::NotAConversation)?;
    let mut dropped = Dropped::none();
    for key in UNCARRIED {
        if value.get(key).is_some_and(|held| !held.is_null()) {
            dropped.note(key);
        }
    }

    // Calls and the responses that answer them are counted apart, so the nth response
    // to a function finds the nth call to it.
    let mut calls: HashMap<String, usize> = HashMap::new();
    let mut answers: HashMap<String, usize> = HashMap::new();
    let mut messages = Vec::with_capacity(contents.len());
    for content in contents {
        let role = match content.get("role").and_then(Value::as_str) {
            Some("model") => Role::Assistant,
            _ => Role::User,
        };
        let mut blocks = Vec::new();
        for part in content
            .get("parts")
            .and_then(Value::as_array)
            .unwrap_or(&Vec::new())
        {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                blocks.push(Block::Text(text.to_owned()));
            } else if let Some(call) = part.get("functionCall") {
                let Some(name) = call.get("name").and_then(Value::as_str) else {
                    dropped.note("tools");
                    continue;
                };
                let nth = calls.entry(name.to_owned()).or_default();
                blocks.push(Block::ToolCall {
                    id: synthesised(name, *nth)?,
                    name: ToolName::new(name)?,
                    arguments: call.get("args").cloned().unwrap_or(json!({})),
                });
                *nth += 1;
            } else if let Some(answer) = part.get("functionResponse") {
                let Some(name) = answer.get("name").and_then(Value::as_str) else {
                    dropped.note("tool_results");
                    continue;
                };
                let nth = answers.entry(name.to_owned()).or_default();
                blocks.push(Block::ToolResult {
                    id: synthesised(name, *nth)?,
                    content: response_text(answer.get("response")),
                });
                *nth += 1;
            } else {
                dropped.note("content_parts");
            }
        }
        messages.push(Message { role, blocks });
    }
    if messages.is_empty() {
        return Err(ProtocolError::NotAConversation);
    }

    let mut tools = Vec::new();
    for declared in value
        .get("tools")
        .and_then(Value::as_array)
        .unwrap_or(&Vec::new())
    {
        for function in declared
            .get("functionDeclarations")
            .and_then(Value::as_array)
            .unwrap_or(&Vec::new())
        {
            let Some(name) = function.get("name").and_then(Value::as_str) else {
                dropped.note("tools");
                continue;
            };
            tools.push(Tool {
                name: ToolName::new(name)?,
                description: function
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                schema: function.get("parameters").cloned().unwrap_or(json!({})),
            });
        }
    }

    let config = value.get("generationConfig");
    Ok((
        Conversation {
            // This family names the model in the request path, never in the body.
            model: None,
            system: parts_text(value.get("systemInstruction")),
            messages,
            tools,
            sampling: Sampling {
                temperature: config.and_then(|held| held.get("temperature")?.as_f64()),
                top_p: config.and_then(|held| held.get("topP")?.as_f64()),
                max_tokens: config
                    .and_then(|held| held.get("maxOutputTokens")?.as_u64())
                    .and_then(|count| u32::try_from(count).ok()),
                stop: config
                    .and_then(|held| held.get("stopSequences")?.as_array())
                    .map(|many| {
                        many.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
            },
            stream: false,
        },
        dropped,
    ))
}

/// Writes a conversation as a generate-content request.
///
/// The model is not written: this family names it in the request path, so a route that
/// sends here has already said which model in the url it built.
#[must_use]
pub fn write(conversation: &Conversation) -> (Value, Dropped) {
    let mut dropped = Dropped::none();
    // A result carries only an id, and this family wants the name of the call it
    // answers; the conversation itself is where that name is.
    let named: HashMap<&ToolCallId, &ToolName> = conversation
        .messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| match block {
            Block::ToolCall { id, name, .. } => Some((id, name)),
            _ => None,
        })
        .collect();

    let mut contents = Vec::with_capacity(conversation.messages.len());
    for message in &conversation.messages {
        let mut parts = Vec::with_capacity(message.blocks.len());
        for block in &message.blocks {
            match block {
                Block::Text(text) => parts.push(json!({"text": text})),
                Block::ToolCall {
                    name, arguments, ..
                } => parts.push(json!({
                    "functionCall": {"name": name.as_str(), "args": arguments},
                })),
                Block::ToolResult { id, content } => match named.get(id) {
                    Some(name) => parts.push(json!({
                        "functionResponse": {
                            "name": name.as_str(),
                            "response": written_response(content),
                        },
                    })),
                    // Nothing in this conversation says which function the result
                    // answers, and this family has no way to say "the call with this id".
                    None => dropped.note("tool_results"),
                },
            }
        }
        contents.push(json!({
            "role": match message.role {
                Role::User => "user",
                Role::Assistant => "model",
            },
            "parts": parts,
        }));
    }

    let mut body = Map::new();
    body.insert("contents".to_owned(), Value::Array(contents));
    if let Some(system) = &conversation.system {
        body.insert(
            "systemInstruction".to_owned(),
            json!({"parts": [{"text": system}]}),
        );
    }
    if !conversation.tools.is_empty() {
        let declarations: Vec<Value> = conversation
            .tools
            .iter()
            .map(|tool| {
                let mut declared = Map::new();
                declared.insert("name".to_owned(), json!(tool.name.as_str()));
                if let Some(description) = &tool.description {
                    declared.insert("description".to_owned(), json!(description));
                }
                declared.insert("parameters".to_owned(), tool.schema.clone());
                Value::Object(declared)
            })
            .collect();
        body.insert(
            "tools".to_owned(),
            json!([{"functionDeclarations": declarations}]),
        );
    }
    let sampling = &conversation.sampling;
    let mut config = Map::new();
    if let Some(temperature) = sampling.temperature {
        config.insert("temperature".to_owned(), json!(temperature));
    }
    if let Some(top_p) = sampling.top_p {
        config.insert("topP".to_owned(), json!(top_p));
    }
    if let Some(max_tokens) = sampling.max_tokens {
        config.insert("maxOutputTokens".to_owned(), json!(max_tokens));
    }
    if !sampling.stop.is_empty() {
        config.insert("stopSequences".to_owned(), json!(sampling.stop));
    }
    if !config.is_empty() {
        body.insert("generationConfig".to_owned(), Value::Object(config));
    }
    (Value::Object(body), dropped)
}

/// Reads a whole generate-content answer as the events it describes.
///
/// # Errors
///
/// Refuses a body that is not json and one that carries no `candidates`.
pub fn read_answer(body: &[u8]) -> Result<(Vec<Event>, Dropped), ProtocolError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| ProtocolError::NotJson(error.to_string()))?;
    let candidates = value
        .get("candidates")
        .and_then(Value::as_array)
        .ok_or(ProtocolError::NotAnAnswer)?;
    let mut dropped = Dropped::none();
    if candidates.len() > 1 {
        dropped.note("candidates");
    }
    let candidate = candidates.first().ok_or(ProtocolError::NotAnAnswer)?;

    let mut events = vec![Event::Started {
        model: value
            .get("modelVersion")
            .and_then(Value::as_str)
            .and_then(|name| ModelName::new(name).ok()),
    }];
    let mut calls: HashMap<String, usize> = HashMap::new();
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
                dropped.note("tools");
                continue;
            };
            let nth = calls.entry(name.to_owned()).or_default();
            events.push(Event::ToolCall {
                id: synthesised(name, *nth)?,
                name: ToolName::new(name)?,
                arguments: call.get("args").cloned().unwrap_or(json!({})),
            });
            *nth += 1;
        } else {
            dropped.note("content_parts");
        }
    }
    if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
        events.push(Event::Stopped(stop_of(reason)));
    }
    if let Some(usage) = value.get("usageMetadata") {
        events.push(Event::Spent(Tokens::new(
            counted(usage.get("promptTokenCount")),
            counted(usage.get("candidatesTokenCount")),
        )));
    }
    Ok((events, dropped))
}

/// Writes a sequence of answer events as a whole generate-content answer.
#[must_use]
pub fn write_answer(events: &[Event]) -> (Value, Dropped) {
    let gathered = Gathered::of(events);
    let mut parts = Vec::new();
    if !gathered.text.is_empty() {
        parts.push(json!({"text": gathered.text}));
    }
    for call in &gathered.calls {
        if let Event::ToolCall {
            name, arguments, ..
        } = call
        {
            parts.push(json!({"functionCall": {"name": name.as_str(), "args": arguments}}));
        }
    }

    let mut candidate = Map::new();
    candidate.insert(
        "content".to_owned(),
        json!({"role": "model", "parts": parts}),
    );
    if let Some(stop) = &gathered.stop {
        candidate.insert(
            "finishReason".to_owned(),
            json!(match stop {
                // This family has no reason of its own for wanting a tool run: it stops
                // normally and leaves a functionCall part behind.
                Stop::End | Stop::ToolUse => "STOP",
                Stop::Length => "MAX_TOKENS",
                Stop::Filtered => "SAFETY",
                Stop::Other(reason) => reason.as_str(),
            }),
        );
    }

    let mut body = Map::new();
    body.insert("candidates".to_owned(), json!([Value::Object(candidate)]));
    if let Some(model) = &gathered.model {
        body.insert("modelVersion".to_owned(), json!(model.as_str()));
    }
    if gathered.spent.is_some() {
        let spent = gathered.spent_or_zero();
        body.insert(
            "usageMetadata".to_owned(),
            json!({
                "promptTokenCount": spent.input.get(),
                "candidatesTokenCount": spent.output.get(),
                "totalTokenCount": spent.total(),
            }),
        );
    }
    (Value::Object(body), Dropped::none())
}

/// This family's finish reasons, the several ways it withholds an answer gathered into
/// the one the canonical set has.
fn stop_of(reason: &str) -> Stop {
    match reason {
        "STOP" => Stop::End,
        "MAX_TOKENS" => Stop::Length,
        "SAFETY" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "RECITATION" => Stop::Filtered,
        other => Stop::Other(other.to_owned()),
    }
}

/// A `parts` holder — the system instruction is one — as plain text.
fn parts_text(held: Option<&Value>) -> Option<String> {
    let parts = held?.get("parts")?.as_array()?;
    Some(
        parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
    )
}

/// A function response, which this family carries as json rather than as text.
fn response_text(response: Option<&Value>) -> String {
    match response {
        Some(Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

/// The same, back: json that was json again, and anything else under a plain key, since
/// this family will only take an object here.
fn written_response(content: &str) -> Value {
    match serde_json::from_str::<Value>(content) {
        Ok(value @ Value::Object(_)) => value,
        _ => json!({"result": content}),
    }
}

/// A stored count, or none of them.
fn counted(value: Option<&Value>) -> TokenCount {
    value
        .and_then(Value::as_u64)
        .and_then(|count| u32::try_from(count).ok())
        .map_or(TokenCount::ZERO, TokenCount::new)
}
