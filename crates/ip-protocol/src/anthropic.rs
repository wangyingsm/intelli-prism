//! The Anthropic family, read into the canonical shape and written back out of it.
//!
//! This family is where the canonical shape came from in two places: the standing
//! instruction sits beside the conversation rather than among the messages, and the
//! answer to a tool call is a block inside a user message rather than a message of its
//! own. Reading it is therefore mostly direct; what it needs that the others do not is
//! a ceiling on the answer, which it requires and they leave optional.

use ip_core::{ModelName, TokenCount, Tokens};
use serde_json::{Map, Value, json};

use crate::answer::{Event, Gathered, Stop};
use crate::conversation::{
    Block, Conversation, Dropped, Message, Role, Sampling, Tool, ToolCallId, ToolName,
};
use crate::error::ProtocolError;

/// The ceiling given to a conversation that arrived without one.
///
/// This family refuses a request that names no `max_tokens`, and the other two leave it
/// optional, so a conversation crossing into it must be given a number that was never
/// asked for. It is recorded as dropped when it is imposed, because a caller whose
/// answer is cut short deserves to know a ceiling it did not choose was applied.
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Fields this family sends that the canonical shape does not carry.
const UNCARRIED: [&str; 3] = ["tool_choice", "top_k", "metadata"];

/// Reads a messages request.
///
/// # Errors
///
/// Refuses a body that is not json, and one that carries no `messages`.
pub fn read(body: &[u8]) -> Result<(Conversation, Dropped), ProtocolError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| ProtocolError::NotJson(error.to_string()))?;
    let sent = value
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(ProtocolError::NotAConversation)?;
    let mut dropped = Dropped::none();
    for key in UNCARRIED {
        if value.get(key).is_some_and(|held| !held.is_null()) {
            dropped.note(key);
        }
    }

    let mut messages = Vec::with_capacity(sent.len());
    for message in sent {
        let role = match message.get("role").and_then(Value::as_str) {
            Some("assistant") => Role::Assistant,
            _ => Role::User,
        };
        messages.push(Message {
            role,
            blocks: blocks_of(message.get("content"), &mut dropped)?,
        });
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
        let Some(name) = declared.get("name").and_then(Value::as_str) else {
            dropped.note("tools");
            continue;
        };
        tools.push(Tool {
            name: ToolName::new(name)?,
            description: declared
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_owned),
            schema: declared.get("input_schema").cloned().unwrap_or(json!({})),
        });
    }

    Ok((
        Conversation {
            model: value
                .get("model")
                .and_then(Value::as_str)
                .and_then(|name| ModelName::new(name).ok()),
            system: system_of(value.get("system"), &mut dropped),
            messages,
            tools,
            sampling: Sampling {
                temperature: value.get("temperature").and_then(Value::as_f64),
                top_p: value.get("top_p").and_then(Value::as_f64),
                max_tokens: value
                    .get("max_tokens")
                    .and_then(Value::as_u64)
                    .and_then(|count| u32::try_from(count).ok()),
                stop: value
                    .get("stop_sequences")
                    .and_then(Value::as_array)
                    .map(|many| {
                        many.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
            },
            stream: value
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or_default(),
        },
        dropped,
    ))
}

/// Writes a conversation as a messages request.
#[must_use]
pub fn write(conversation: &Conversation) -> (Value, Dropped) {
    let mut dropped = Dropped::none();
    let messages: Vec<Value> = conversation
        .messages
        .iter()
        .map(|message| {
            json!({
                "role": match message.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                },
                "content": written_content(message),
            })
        })
        .collect();

    let mut body = Map::new();
    if let Some(model) = &conversation.model {
        body.insert("model".to_owned(), json!(model.as_str()));
    }
    // A ceiling this family requires and the caller never set is a ceiling imposed.
    let max_tokens = conversation.sampling.max_tokens.unwrap_or_else(|| {
        dropped.note("max_tokens");
        DEFAULT_MAX_TOKENS
    });
    body.insert("max_tokens".to_owned(), json!(max_tokens));
    if let Some(system) = &conversation.system {
        body.insert("system".to_owned(), json!(system));
    }
    body.insert("messages".to_owned(), Value::Array(messages));
    if !conversation.tools.is_empty() {
        let tools = conversation
            .tools
            .iter()
            .map(|tool| {
                let mut declared = Map::new();
                declared.insert("name".to_owned(), json!(tool.name.as_str()));
                if let Some(description) = &tool.description {
                    declared.insert("description".to_owned(), json!(description));
                }
                declared.insert("input_schema".to_owned(), tool.schema.clone());
                Value::Object(declared)
            })
            .collect();
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    let sampling = &conversation.sampling;
    if let Some(temperature) = sampling.temperature {
        body.insert("temperature".to_owned(), json!(temperature));
    }
    if let Some(top_p) = sampling.top_p {
        body.insert("top_p".to_owned(), json!(top_p));
    }
    if !sampling.stop.is_empty() {
        body.insert("stop_sequences".to_owned(), json!(sampling.stop));
    }
    if conversation.stream {
        body.insert("stream".to_owned(), json!(true));
    }
    (Value::Object(body), dropped)
}

/// Reads a whole messages answer as the events it describes.
///
/// # Errors
///
/// Refuses a body that is not json and one that carries no `content`.
pub fn read_answer(body: &[u8]) -> Result<(Vec<Event>, Dropped), ProtocolError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| ProtocolError::NotJson(error.to_string()))?;
    let content = value
        .get("content")
        .and_then(Value::as_array)
        .ok_or(ProtocolError::NotAnAnswer)?;
    let mut dropped = Dropped::none();
    let mut events = vec![Event::Started {
        model: value
            .get("model")
            .and_then(Value::as_str)
            .and_then(|name| ModelName::new(name).ok()),
    }];
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    events.push(Event::Text(text.to_owned()));
                }
            }
            Some("tool_use") => events.push(read_use(block)?),
            _ => dropped.note("content_parts"),
        }
    }
    if let Some(reason) = value.get("stop_reason").and_then(Value::as_str) {
        events.push(Event::Stopped(stop_of(reason)));
    }
    if let Some(usage) = value.get("usage") {
        events.push(Event::Spent(Tokens::new(
            counted(usage.get("input_tokens")),
            counted(usage.get("output_tokens")),
        )));
    }
    Ok((events, dropped))
}

/// Writes a sequence of answer events as a whole messages answer.
#[must_use]
pub fn write_answer(events: &[Event]) -> (Value, Dropped) {
    let gathered = Gathered::of(events);
    let mut content = Vec::new();
    if !gathered.text.is_empty() {
        content.push(json!({"type": "text", "text": gathered.text}));
    }
    for call in &gathered.calls {
        if let Event::ToolCall {
            id,
            name,
            arguments,
        } = call
        {
            content.push(json!({
                "type": "tool_use",
                "id": id.as_str(),
                "name": name.as_str(),
                "input": arguments,
            }));
        }
    }

    let mut body = Map::new();
    body.insert("type".to_owned(), json!("message"));
    body.insert("role".to_owned(), json!("assistant"));
    if let Some(model) = &gathered.model {
        body.insert("model".to_owned(), json!(model.as_str()));
    }
    body.insert("content".to_owned(), Value::Array(content));
    if let Some(stop) = &gathered.stop {
        body.insert(
            "stop_reason".to_owned(),
            json!(match stop {
                Stop::End => "end_turn",
                Stop::Length => "max_tokens",
                Stop::ToolUse => "tool_use",
                // This family has no refusal reason of its own; naming the canonical one
                // is truer than claiming it ended normally.
                Stop::Filtered => "refusal",
                Stop::Other(reason) => reason.as_str(),
            }),
        );
    }
    if gathered.spent.is_some() {
        let spent = gathered.spent_or_zero();
        body.insert(
            "usage".to_owned(),
            json!({"input_tokens": spent.input.get(), "output_tokens": spent.output.get()}),
        );
    }
    (Value::Object(body), Dropped::none())
}

/// This family's stop reasons, with the one the canonical set has no name for kept by
/// its own name rather than flattened into ending normally.
fn stop_of(reason: &str) -> Stop {
    match reason {
        "end_turn" => Stop::End,
        "max_tokens" => Stop::Length,
        "tool_use" => Stop::ToolUse,
        "refusal" => Stop::Filtered,
        other => Stop::Other(other.to_owned()),
    }
}

/// A standing instruction, given as a string or as the blocks this family also allows.
fn system_of(system: Option<&Value>, dropped: &mut Dropped) -> Option<String> {
    match system {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Array(blocks)) => {
            let mut text = String::new();
            for block in blocks {
                match block.get("text").and_then(Value::as_str) {
                    Some(piece) => text.push_str(piece),
                    None => dropped.note("system_parts"),
                }
            }
            Some(text)
        }
        _ => None,
    }
}

/// Message content, given as a string or as blocks.
fn blocks_of(content: Option<&Value>, dropped: &mut Dropped) -> Result<Vec<Block>, ProtocolError> {
    match content {
        Some(Value::String(text)) => Ok(vec![Block::Text(text.clone())]),
        Some(Value::Array(written)) => {
            let mut blocks = Vec::with_capacity(written.len());
            for block in written {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => blocks.push(Block::Text(
                        block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    )),
                    Some("tool_use") => blocks.push(read_use(block).map(|event| match event {
                        Event::ToolCall {
                            id,
                            name,
                            arguments,
                        } => Block::ToolCall {
                            id,
                            name,
                            arguments,
                        },
                        _ => unreachable!("reading a use yields a call"),
                    })?),
                    Some("tool_result") => {
                        let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                            dropped.note("tool_results");
                            continue;
                        };
                        blocks.push(Block::ToolResult {
                            id: ToolCallId::new(id)?,
                            content: result_text(block.get("content")),
                        });
                    }
                    _ => dropped.note("content_parts"),
                }
            }
            Ok(blocks)
        }
        _ => Ok(Vec::new()),
    }
}

/// A tool result's content, which this family allows as a string or as blocks.
fn result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// A `tool_use` block, which carries its arguments as json rather than as a string.
fn read_use(block: &Value) -> Result<Event, ProtocolError> {
    let id = block
        .get("id")
        .and_then(Value::as_str)
        .ok_or(ProtocolError::EmptyToolCallId)?;
    let name = block
        .get("name")
        .and_then(Value::as_str)
        .ok_or(ProtocolError::EmptyToolName)?;
    Ok(Event::ToolCall {
        id: ToolCallId::new(id)?,
        name: ToolName::new(name)?,
        arguments: block.get("input").cloned().unwrap_or(json!({})),
    })
}

/// A message's blocks as this family writes them: a lone text block as the plain string
/// every client sends, and anything else as the block array.
fn written_content(message: &Message) -> Value {
    if let [Block::Text(text)] = message.blocks.as_slice() {
        return json!(text);
    }
    let blocks: Vec<Value> = message
        .blocks
        .iter()
        .map(|block| match block {
            Block::Text(text) => json!({"type": "text", "text": text}),
            Block::ToolCall {
                id,
                name,
                arguments,
            } => json!({
                "type": "tool_use",
                "id": id.as_str(),
                "name": name.as_str(),
                "input": arguments,
            }),
            Block::ToolResult { id, content } => json!({
                "type": "tool_result",
                "tool_use_id": id.as_str(),
                "content": content,
            }),
        })
        .collect();
    Value::Array(blocks)
}

/// A stored count, or none of them.
fn counted(value: Option<&Value>) -> TokenCount {
    value
        .and_then(Value::as_u64)
        .and_then(|count| u32::try_from(count).ok())
        .map_or(TokenCount::ZERO, TokenCount::new)
}
