//! The OpenAI family, read into the canonical shape and written back out of it.
//!
//! Two asymmetries with the canonical shape are this family's own, and both are handled
//! here rather than leaking outwards: a system instruction is a message with a role, and
//! the answer to a tool call is a message with a role, where the canonical shape keeps
//! the first beside the conversation and the second inside a user message.

use ip_core::{ModelName, TokenCount, Tokens};
use serde_json::{Map, Value, json};

use crate::answer::{Event, Gathered, Stop};
use crate::conversation::{
    Block, Conversation, Dropped, Message, Role, Sampling, Tool, ToolCallId, ToolName,
};
use crate::error::ProtocolError;

/// Fields this family sends that the canonical shape does not carry. Named by their own
/// keys, because what a caller reads in a header should be what it wrote in its request.
const UNCARRIED: [&str; 7] = [
    "tool_choice",
    "response_format",
    "n",
    "seed",
    "frequency_penalty",
    "presence_penalty",
    "logit_bias",
];

/// Reads a chat completions request.
///
/// # Errors
///
/// Refuses a body that is not json, and one that carries no `messages` — an embeddings
/// or moderation request reaches this and is not a conversation.
#[expect(
    clippy::too_many_lines,
    reason = "one pass over a wire format, an arm per shape this family can send"
)]
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

    let mut system: Option<String> = None;
    let mut messages = Vec::with_capacity(sent.len());
    for message in sent {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        match role {
            // `developer` is this family's newer spelling of the same standing instruction.
            "system" | "developer" => {
                let text = text_of(message.get("content"), &mut dropped);
                match &mut system {
                    Some(held) => {
                        held.push_str("\n\n");
                        held.push_str(&text);
                    }
                    None => system = Some(text),
                }
            }
            "tool" => {
                let Some(id) = message.get("tool_call_id").and_then(Value::as_str) else {
                    dropped.note("tool_results");
                    continue;
                };
                messages.push(Message {
                    role: Role::User,
                    blocks: vec![Block::ToolResult {
                        id: ToolCallId::new(id)?,
                        content: text_of(message.get("content"), &mut dropped),
                    }],
                });
            }
            "assistant" => {
                let mut blocks = Vec::new();
                let text = text_of(message.get("content"), &mut dropped);
                if !text.is_empty() {
                    blocks.push(Block::Text(text));
                }
                for call in message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .unwrap_or(&Vec::new())
                {
                    blocks.push(read_call(call)?);
                }
                messages.push(Message {
                    role: Role::Assistant,
                    blocks,
                });
            }
            _ => messages.push(Message {
                role: Role::User,
                blocks: vec![Block::Text(text_of(message.get("content"), &mut dropped))],
            }),
        }
    }
    if messages.is_empty() && system.is_none() {
        return Err(ProtocolError::NotAConversation);
    }

    let mut tools = Vec::new();
    for declared in value
        .get("tools")
        .and_then(Value::as_array)
        .unwrap_or(&Vec::new())
    {
        let Some(function) = declared.get("function") else {
            dropped.note("tools");
            continue;
        };
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

    Ok((
        Conversation {
            model: value
                .get("model")
                .and_then(Value::as_str)
                .and_then(|name| ModelName::new(name).ok()),
            system,
            messages,
            tools,
            sampling: Sampling {
                temperature: value.get("temperature").and_then(Value::as_f64),
                top_p: value.get("top_p").and_then(Value::as_f64),
                // The newer spelling wins where a caller sent both, as this family reads it.
                max_tokens: value
                    .get("max_completion_tokens")
                    .or_else(|| value.get("max_tokens"))
                    .and_then(Value::as_u64)
                    .and_then(|count| u32::try_from(count).ok()),
                stop: strings_of(value.get("stop")),
            },
            stream: value
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or_default(),
        },
        dropped,
    ))
}

/// Writes a conversation as a chat completions request.
#[must_use]
pub fn write(conversation: &Conversation) -> (Value, Dropped) {
    let dropped = Dropped::none();
    let mut messages = Vec::new();
    if let Some(system) = &conversation.system {
        messages.push(json!({"role": "system", "content": system}));
    }
    for message in &conversation.messages {
        match message.role {
            Role::Assistant => messages.push(write_assistant(message)),
            // A tool result has its own role here, so it leaves the user message it
            // travels inside everywhere else.
            Role::User => {
                for block in &message.blocks {
                    if let Block::ToolResult { id, content } = block {
                        messages.push(
                            json!({"role": "tool", "tool_call_id": id.as_str(), "content": content}),
                        );
                    }
                }
                let text = joined_text(message);
                if !text.is_empty() {
                    messages.push(json!({"role": "user", "content": text}));
                }
            }
        }
    }

    let mut body = Map::new();
    if let Some(model) = &conversation.model {
        body.insert("model".to_owned(), json!(model.as_str()));
    }
    body.insert("messages".to_owned(), Value::Array(messages));
    if !conversation.tools.is_empty() {
        let tools = conversation
            .tools
            .iter()
            .map(|tool| {
                let mut function = Map::new();
                function.insert("name".to_owned(), json!(tool.name.as_str()));
                if let Some(description) = &tool.description {
                    function.insert("description".to_owned(), json!(description));
                }
                function.insert("parameters".to_owned(), tool.schema.clone());
                json!({"type": "function", "function": Value::Object(function)})
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
    if let Some(max_tokens) = sampling.max_tokens {
        body.insert("max_tokens".to_owned(), json!(max_tokens));
    }
    if !sampling.stop.is_empty() {
        body.insert("stop".to_owned(), json!(sampling.stop));
    }
    if conversation.stream {
        body.insert("stream".to_owned(), json!(true));
    }
    (Value::Object(body), dropped)
}

/// Reads a whole chat completions answer as the events it describes.
///
/// # Errors
///
/// Refuses a body that is not json and one that carries no `choices`.
pub fn read_answer(body: &[u8]) -> Result<(Vec<Event>, Dropped), ProtocolError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| ProtocolError::NotJson(error.to_string()))?;
    let choices = value
        .get("choices")
        .and_then(Value::as_array)
        .ok_or(ProtocolError::NotAnAnswer)?;
    let mut dropped = Dropped::none();
    if choices.len() > 1 {
        dropped.note("choices");
    }
    let choice = choices.first().ok_or(ProtocolError::NotAnAnswer)?;

    let mut events = vec![Event::Started {
        model: value
            .get("model")
            .and_then(Value::as_str)
            .and_then(|name| ModelName::new(name).ok()),
    }];
    let message = choice.get("message").unwrap_or(choice);
    let text = text_of(message.get("content"), &mut dropped);
    if !text.is_empty() {
        events.push(Event::Text(text));
    }
    for call in message
        .get("tool_calls")
        .and_then(Value::as_array)
        .unwrap_or(&Vec::new())
    {
        let Block::ToolCall {
            id,
            name,
            arguments,
        } = read_call(call)?
        else {
            unreachable!("reading a call yields a call")
        };
        events.push(Event::ToolCall {
            id,
            name,
            arguments,
        });
    }
    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
        events.push(Event::Stopped(match reason {
            "stop" => Stop::End,
            "length" => Stop::Length,
            "tool_calls" | "function_call" => Stop::ToolUse,
            "content_filter" => Stop::Filtered,
            other => Stop::Other(other.to_owned()),
        }));
    }
    if let Some(usage) = value.get("usage") {
        events.push(Event::Spent(Tokens::new(
            counted(usage.get("prompt_tokens")),
            counted(usage.get("completion_tokens")),
        )));
    }
    Ok((events, dropped))
}

/// Writes a sequence of answer events as a whole chat completions answer.
#[must_use]
pub fn write_answer(events: &[Event]) -> (Value, Dropped) {
    let gathered = Gathered::of(events);
    let mut dropped = Dropped::none();
    let mut message = Map::new();
    message.insert("role".to_owned(), json!("assistant"));
    message.insert(
        "content".to_owned(),
        if gathered.text.is_empty() {
            Value::Null
        } else {
            json!(gathered.text)
        },
    );
    if !gathered.calls.is_empty() {
        let calls = gathered
            .calls
            .iter()
            .filter_map(|call| match call {
                Event::ToolCall {
                    id,
                    name,
                    arguments,
                } => Some(json!({
                    "id": id.as_str(),
                    "type": "function",
                    "function": {"name": name.as_str(), "arguments": arguments.to_string()},
                })),
                _ => None,
            })
            .collect();
        message.insert("tool_calls".to_owned(), Value::Array(calls));
    }

    let mut choice = Map::new();
    choice.insert("index".to_owned(), json!(0));
    choice.insert("message".to_owned(), Value::Object(message));
    if let Some(stop) = &gathered.stop {
        choice.insert(
            "finish_reason".to_owned(),
            json!(match stop {
                Stop::End => "stop",
                Stop::Length => "length",
                Stop::ToolUse => "tool_calls",
                Stop::Filtered => "content_filter",
                // This family's reasons are a closed set a client switches on, so one it
                // never emits is flattened to the ordinary ending rather than passed on.
                Stop::Other(_) => {
                    dropped.note("stop_reason");
                    "stop"
                }
            }),
        );
    }

    let mut body = Map::new();
    body.insert("object".to_owned(), json!("chat.completion"));
    if let Some(model) = &gathered.model {
        body.insert("model".to_owned(), json!(model.as_str()));
    }
    body.insert("choices".to_owned(), json!([Value::Object(choice)]));
    if gathered.spent.is_some() {
        let spent = gathered.spent_or_zero();
        body.insert(
            "usage".to_owned(),
            json!({
                "prompt_tokens": spent.input.get(),
                "completion_tokens": spent.output.get(),
                "total_tokens": spent.total(),
            }),
        );
    }
    (Value::Object(body), dropped)
}

/// One tool call, as this family nests it under a `function` with its arguments as a
/// json string rather than as json.
fn read_call(call: &Value) -> Result<Block, ProtocolError> {
    let function = call.get("function").unwrap_or(call);
    let id = call
        .get("id")
        .and_then(Value::as_str)
        .ok_or(ProtocolError::EmptyToolCallId)?;
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .ok_or(ProtocolError::EmptyToolName)?;
    let arguments = match function.get("arguments") {
        Some(Value::String(text)) => serde_json::from_str(text).unwrap_or(json!(text)),
        Some(other) => other.clone(),
        None => json!({}),
    };
    Ok(Block::ToolCall {
        id: ToolCallId::new(id)?,
        name: ToolName::new(name)?,
        arguments,
    })
}

/// An assistant message, which carries its calls beside its text rather than among it.
fn write_assistant(message: &Message) -> Value {
    let text = joined_text(message);
    let mut written = Map::new();
    written.insert("role".to_owned(), json!("assistant"));
    written.insert(
        "content".to_owned(),
        if text.is_empty() {
            Value::Null
        } else {
            json!(text)
        },
    );
    let calls: Vec<Value> = message
        .blocks
        .iter()
        .filter_map(|block| match block {
            Block::ToolCall {
                id,
                name,
                arguments,
            } => Some(json!({
                "id": id.as_str(),
                "type": "function",
                "function": {"name": name.as_str(), "arguments": arguments.to_string()},
            })),
            _ => None,
        })
        .collect();
    if !calls.is_empty() {
        written.insert("tool_calls".to_owned(), Value::Array(calls));
    }
    Value::Object(written)
}

/// Every text block of a message, joined as one string.
fn joined_text(message: &Message) -> String {
    message
        .blocks
        .iter()
        .filter_map(|block| match block {
            Block::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Content given as a string, or as the parts this family also allows. A part that is
/// not text is content this pass does not carry, and says so.
fn text_of(content: Option<&Value>, dropped: &mut Dropped) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => {
            let mut text = String::new();
            for part in parts {
                match part.get("text").and_then(Value::as_str) {
                    Some(piece) => text.push_str(piece),
                    None => dropped.note("content_parts"),
                }
            }
            text
        }
        _ => String::new(),
    }
}

/// A stop sequence given as one string or as several.
fn strings_of(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(one)) => vec![one.clone()],
        Some(Value::Array(many)) => many
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// A stored count, or none of them.
fn counted(value: Option<&Value>) -> TokenCount {
    value
        .and_then(Value::as_u64)
        .and_then(|count| u32::try_from(count).ok())
        .map_or(TokenCount::ZERO, TokenCount::new)
}
