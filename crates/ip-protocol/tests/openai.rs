//! A body of this family, read into the canonical shape and written back out of it,
//! still means what it meant.
//!
//! Round-trips are compared as parsed values, never as bytes: key order and an absent
//! field against a null one differ harmlessly, and chasing that would be chasing the
//! wrong property. What must survive is the conversation.

use ip_protocol::{Block, Conversation, Role, ToolCallId, ToolName, openai};
use serde_json::{Value, json};

/// Reads a body, writes it back, and hands over both the shape and the second body.
fn around(body: &Value) -> (Conversation, Value) {
    let (conversation, dropped) =
        openai::read(body.to_string().as_bytes()).expect("a conversation");
    assert!(
        dropped.is_empty(),
        "nothing in this body should be dropped: {dropped}"
    );
    let (written, dropped) = openai::write(&conversation);
    assert!(dropped.is_empty(), "writing dropped {dropped}");
    (conversation, written)
}

#[test]
fn a_plain_chat_round_trips() {
    let body = json!({
        "model": "gpt-4o",
        "messages": [
            {"role": "system", "content": "you audit accounts"},
            {"role": "user", "content": "what did we spend?"},
            {"role": "assistant", "content": "on what?"},
            {"role": "user", "content": "in march"},
        ],
        "temperature": 0.7,
        "top_p": 0.9,
        "max_tokens": 256,
        "stop": ["END"],
        "stream": true,
    });
    let (conversation, written) = around(&body);

    assert_eq!(conversation.model.as_ref().unwrap().as_str(), "gpt-4o");
    assert_eq!(
        conversation.system.as_deref(),
        Some("you audit accounts"),
        "a system message leaves the messages and becomes the instruction"
    );
    assert_eq!(conversation.messages.len(), 3);
    assert_eq!(conversation.messages[0].role, Role::User);
    assert_eq!(conversation.messages[1].role, Role::Assistant);
    assert_eq!(conversation.sampling.temperature, Some(0.7));
    assert_eq!(conversation.sampling.max_tokens, Some(256));
    assert_eq!(conversation.sampling.stop, vec!["END".to_owned()]);
    assert!(conversation.stream);
    assert_eq!(written, body, "the body means what it meant");
}

#[test]
fn a_conversation_with_tools_round_trips_with_its_call_ids() {
    let body = json!({
        "model": "gpt-4o",
        "messages": [
            {"role": "user", "content": "search for rust"},
            {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "search", "arguments": "{\"q\":\"rust\"}"},
            }]},
            {"role": "tool", "tool_call_id": "call_1", "content": "42 results"},
            {"role": "assistant", "content": "forty-two"},
        ],
        "tools": [{
            "type": "function",
            "function": {
                "name": "search",
                "description": "look something up",
                "parameters": {"type": "object", "properties": {"q": {"type": "string"}}},
            },
        }],
    });
    let (conversation, written) = around(&body);

    // The id is the only join between a call and the result that answers it.
    let call = &conversation.messages[1].blocks[0];
    let Block::ToolCall {
        id,
        name,
        arguments,
    } = call
    else {
        panic!("the assistant asked for a tool: {call:?}")
    };
    assert_eq!(id.as_str(), "call_1");
    assert_eq!(name.as_str(), "search");
    assert_eq!(
        arguments,
        &json!({"q": "rust"}),
        "arguments arrive as a json string and are held as json"
    );

    let result = &conversation.messages[2].blocks[0];
    assert_eq!(
        result,
        &Block::ToolResult {
            id: ToolCallId::new("call_1").unwrap(),
            content: "42 results".to_owned(),
        },
        "a tool message becomes a result inside a user message"
    );
    assert_eq!(conversation.messages[2].role, Role::User);
    assert_eq!(conversation.tools.len(), 1);
    assert_eq!(conversation.tools[0].name, ToolName::new("search").unwrap());
    assert_eq!(written, body, "the body means what it meant");
}

#[test]
fn content_as_a_string_and_as_parts_read_the_same() {
    let plain = json!({"messages": [{"role": "user", "content": "hello there"}]});
    let parts = json!({"messages": [{"role": "user", "content": [
        {"type": "text", "text": "hello "},
        {"type": "text", "text": "there"},
    ]}]});
    let (from_plain, _) = openai::read(plain.to_string().as_bytes()).unwrap();
    let (from_parts, _) = openai::read(parts.to_string().as_bytes()).unwrap();
    assert_eq!(from_plain, from_parts);
}

#[test]
fn a_part_this_pass_does_not_carry_is_recorded_as_dropped() {
    let body = json!({"messages": [{"role": "user", "content": [
        {"type": "text", "text": "what is this"},
        {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}},
    ]}]});
    let (conversation, dropped) = openai::read(body.to_string().as_bytes()).unwrap();
    assert_eq!(dropped.names(), vec!["content_parts"]);
    assert_eq!(
        conversation.first_user_text().as_deref(),
        Some("what is this"),
        "the text of the message survives the part that did not"
    );
}

#[test]
fn a_field_the_canonical_shape_does_not_carry_is_recorded_by_its_own_name() {
    let body = json!({
        "messages": [{"role": "user", "content": "hi"}],
        "seed": 7,
        "response_format": {"type": "json_object"},
    });
    let (_, dropped) = openai::read(body.to_string().as_bytes()).unwrap();
    assert_eq!(
        dropped.names(),
        vec!["response_format", "seed"],
        "a caller reading the header should see the keys it sent"
    );
}

#[test]
fn a_body_that_is_not_a_conversation_is_refused_rather_than_read_as_an_empty_one() {
    let embeddings = json!({"model": "text-embedding-3-small", "input": "some text"});
    assert!(openai::read(embeddings.to_string().as_bytes()).is_err());
    assert!(openai::read(b"not json at all").is_err());
    assert!(
        openai::read(json!({"messages": []}).to_string().as_bytes()).is_err(),
        "no messages and no instruction is no conversation"
    );
}

#[test]
fn an_answer_round_trips_through_its_events() {
    let body = json!({
        "object": "chat.completion",
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "forty-two"},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12},
    });
    let (events, dropped) = openai::read_answer(body.to_string().as_bytes()).expect("an answer");
    assert!(dropped.is_empty());
    let (written, dropped) = openai::write_answer(&events);
    assert!(dropped.is_empty());
    assert_eq!(written, body);
}

#[test]
fn an_answer_that_asks_for_a_tool_round_trips() {
    let body = json!({
        "object": "chat.completion",
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_9",
                "type": "function",
                "function": {"name": "search", "arguments": "{\"q\":\"rust\"}"},
            }]},
            "finish_reason": "tool_calls",
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 7, "total_tokens": 12},
    });
    let (events, _) = openai::read_answer(body.to_string().as_bytes()).expect("an answer");
    let (written, _) = openai::write_answer(&events);
    assert_eq!(written, body);
}

#[test]
fn an_answer_of_several_choices_keeps_the_first_and_says_it_dropped_the_rest() {
    let body = json!({
        "model": "gpt-4o",
        "choices": [
            {"index": 0, "message": {"role": "assistant", "content": "one"}, "finish_reason": "stop"},
            {"index": 1, "message": {"role": "assistant", "content": "two"}, "finish_reason": "stop"},
        ],
    });
    let (events, dropped) = openai::read_answer(body.to_string().as_bytes()).unwrap();
    assert_eq!(dropped.names(), vec!["choices"]);
    let (written, _) = openai::write_answer(&events);
    assert_eq!(written["choices"][0]["message"]["content"], json!("one"));
    assert_eq!(written["choices"].as_array().unwrap().len(), 1);
}

#[test]
fn a_body_that_is_not_an_answer_is_refused() {
    assert!(openai::read_answer(json!({"model": "gpt-4o"}).to_string().as_bytes()).is_err());
    assert!(openai::read_answer(b"{").is_err());
}

#[test]
fn a_body_this_family_would_not_have_sent_is_read_for_what_it_holds() {
    let bent = json!({
        "messages": [
            {"role": "developer", "content": "a newer word for the instruction"},
            {"role": "wizard", "content": "a role no family has"},
            {"role": "tool", "content": "an answer to nothing"},
            {"role": "assistant", "content": "and a word of its own"},
        ],
        "tools": [
            {"type": "function"},
            {"type": "function", "function": {"description": "nameless"}},
        ],
        "stop": "one sequence, not a list",
        "max_completion_tokens": 64,
        "max_tokens": 32,
    });
    let (conversation, dropped) =
        openai::read(bent.to_string().as_bytes()).expect("a conversation");
    assert_eq!(
        dropped.names(),
        vec!["tool_results", "tools"],
        "what could not be carried was named"
    );
    assert_eq!(
        conversation.system.as_deref(),
        Some("a newer word for the instruction")
    );
    assert_eq!(
        conversation.sampling.stop,
        vec!["one sequence, not a list".to_owned()],
        "one stop sequence is a list of one"
    );
    assert_eq!(
        conversation.sampling.max_tokens,
        Some(64),
        "the newer spelling wins where a caller sent both"
    );
    assert!(conversation.tools.is_empty(), "neither tool could be named");
    // The unrecognised role is read as the caller's, which is what it most likely was.
    assert_eq!(conversation.messages[0].role, Role::User);
}

#[test]
fn a_call_this_family_spells_incompletely_is_refused() {
    let nameless = json!({"messages": [
        {"role": "assistant", "tool_calls": [{"id": "call_1", "function": {}}]},
    ]});
    assert!(openai::read(nameless.to_string().as_bytes()).is_err());
    let anonymous = json!({"messages": [
        {"role": "assistant", "tool_calls": [{"function": {"name": "search"}}]},
    ]});
    assert!(openai::read(anonymous.to_string().as_bytes()).is_err());
}

#[test]
fn every_reason_this_family_stops_for_is_carried() {
    for (said, back) in [
        ("stop", "stop"),
        ("length", "length"),
        ("tool_calls", "tool_calls"),
        ("content_filter", "content_filter"),
        ("function_call", "tool_calls"),
    ] {
        let body = json!({
            "choices": [{"message": {"content": "x"}, "finish_reason": said}],
        });
        let (events, _) = openai::read_answer(body.to_string().as_bytes()).unwrap();
        let (written, _) = openai::write_answer(&events);
        assert_eq!(
            written["choices"][0]["finish_reason"],
            json!(back),
            "{said} did not survive"
        );
    }
}

#[test]
fn a_conversation_carrying_only_an_instruction_is_written_back_whole() {
    let instruction = json!({"messages": [{"role": "system", "content": "stand by"}]});
    let (conversation, _) = openai::read(instruction.to_string().as_bytes()).expect("a system");
    assert!(conversation.messages.is_empty());
    let (written, _) = openai::write(&conversation);
    assert_eq!(written["messages"][0]["role"], json!("system"));
    assert!(written.get("model").is_none(), "no model was named");
}
