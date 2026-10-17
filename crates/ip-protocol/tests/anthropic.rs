//! This family round-trips, and — the claim the milestone rests on — a conversation
//! written in it reads to the same shape as the same conversation written in another.

use ip_protocol::{Block, Conversation, Role, ToolCallId, ToolName, anthropic, openai};
use serde_json::{Value, json};

/// Reads a body, writes it back, and hands over both the shape and the second body.
fn around(body: &Value) -> (Conversation, Value) {
    let (conversation, dropped) =
        anthropic::read(body.to_string().as_bytes()).expect("a conversation");
    assert!(dropped.is_empty(), "reading dropped {dropped}");
    let (written, dropped) = anthropic::write(&conversation);
    assert!(dropped.is_empty(), "writing dropped {dropped}");
    (conversation, written)
}

#[test]
fn a_plain_chat_round_trips() {
    let body = json!({
        "model": "claude-opus-5",
        "max_tokens": 256,
        "system": "you audit accounts",
        "messages": [
            {"role": "user", "content": "what did we spend?"},
            {"role": "assistant", "content": "on what?"},
            {"role": "user", "content": "in march"},
        ],
        "temperature": 0.7,
        "top_p": 0.9,
        "stop_sequences": ["END"],
        "stream": true,
    });
    let (conversation, written) = around(&body);

    assert_eq!(conversation.system.as_deref(), Some("you audit accounts"));
    assert_eq!(conversation.messages.len(), 3);
    assert_eq!(conversation.sampling.max_tokens, Some(256));
    assert_eq!(conversation.sampling.stop, vec!["END".to_owned()]);
    assert!(conversation.stream);
    assert_eq!(written, body, "the body means what it meant");
}

#[test]
fn a_conversation_with_tools_round_trips_with_its_call_ids() {
    let body = json!({
        "model": "claude-opus-5",
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "search for rust"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "search", "input": {"q": "rust"}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "42 results"},
            ]},
            {"role": "assistant", "content": "forty-two"},
        ],
        "tools": [{
            "name": "search",
            "description": "look something up",
            "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}},
        }],
    });
    let (conversation, written) = around(&body);

    let Block::ToolCall { id, name, .. } = &conversation.messages[1].blocks[0] else {
        panic!("the assistant asked for a tool")
    };
    assert_eq!(id.as_str(), "toolu_1");
    assert_eq!(name.as_str(), "search");
    assert_eq!(
        conversation.messages[2].blocks[0],
        Block::ToolResult {
            id: ToolCallId::new("toolu_1").unwrap(),
            content: "42 results".to_owned(),
        }
    );
    assert_eq!(conversation.messages[2].role, Role::User);
    assert_eq!(conversation.tools[0].name, ToolName::new("search").unwrap());
    assert_eq!(written, body, "the body means what it meant");
}

/// The whole claim of this milestone, first provable here: two families spelling one
/// conversation read to one shape. The two bodies disagree about where the instruction
/// lives and about how a tool result is carried, and neither disagreement survives.
#[test]
fn the_same_conversation_in_two_families_reads_to_one_shape() {
    let in_openai = json!({
        "model": "shared-model",
        "max_tokens": 1024,
        "messages": [
            {"role": "system", "content": "you audit accounts"},
            {"role": "user", "content": "search for rust"},
            {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "search", "arguments": "{\"q\":\"rust\"}"},
            }]},
            {"role": "tool", "tool_call_id": "call_1", "content": "42 results"},
        ],
        "tools": [{"type": "function", "function": {
            "name": "search",
            "description": "look something up",
            "parameters": {"type": "object"},
        }}],
    });
    let in_anthropic = json!({
        "model": "shared-model",
        "max_tokens": 1024,
        "system": "you audit accounts",
        "messages": [
            {"role": "user", "content": "search for rust"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "call_1", "name": "search", "input": {"q": "rust"}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_1", "content": "42 results"},
            ]},
        ],
        "tools": [{
            "name": "search",
            "description": "look something up",
            "input_schema": {"type": "object"},
        }],
    });

    let (from_openai, _) = openai::read(in_openai.to_string().as_bytes()).unwrap();
    let (from_anthropic, _) = anthropic::read(in_anthropic.to_string().as_bytes()).unwrap();
    assert_eq!(from_openai, from_anthropic);
}

/// And the crossing the gateway will actually make: read one family, write the other,
/// and the result is the body that family would have been sent.
#[test]
fn a_conversation_crosses_from_one_family_to_the_other() {
    let in_openai = json!({
        "model": "shared-model",
        "max_tokens": 1024,
        "messages": [
            {"role": "system", "content": "you audit accounts"},
            {"role": "user", "content": "hello"},
        ],
    });
    let (conversation, _) = openai::read(in_openai.to_string().as_bytes()).unwrap();
    let (crossed, dropped) = anthropic::write(&conversation);
    assert!(dropped.is_empty(), "nothing needed dropping: {dropped}");
    assert_eq!(
        crossed,
        json!({
            "model": "shared-model",
            "max_tokens": 1024,
            "system": "you audit accounts",
            "messages": [{"role": "user", "content": "hello"}],
        })
    );
}

#[test]
fn a_ceiling_this_family_requires_and_nobody_asked_for_is_imposed_and_said() {
    let without = json!({"messages": [{"role": "user", "content": "hello"}]});
    let (conversation, _) = openai::read(without.to_string().as_bytes()).unwrap();
    assert_eq!(conversation.sampling.max_tokens, None);

    let (written, dropped) = anthropic::write(&conversation);
    assert_eq!(
        written["max_tokens"],
        json!(anthropic::DEFAULT_MAX_TOKENS),
        "this family refuses a request without one"
    );
    assert_eq!(
        dropped.names(),
        vec!["max_tokens"],
        "a ceiling the caller never chose is a thing the caller should hear about"
    );
}

#[test]
fn a_standing_instruction_is_never_written_as_a_message() {
    let body = json!({
        "max_tokens": 10,
        "system": [{"type": "text", "text": "you audit accounts"}],
        "messages": [{"role": "user", "content": "hi"}],
    });
    let (conversation, dropped) = anthropic::read(body.to_string().as_bytes()).unwrap();
    assert!(dropped.is_empty(), "a text system block is carried whole");
    assert_eq!(conversation.system.as_deref(), Some("you audit accounts"));

    let (written, _) = anthropic::write(&conversation);
    assert_eq!(written["system"], json!("you audit accounts"));
    let messages = written["messages"].as_array().unwrap();
    assert_eq!(
        messages.len(),
        1,
        "the instruction did not become a message"
    );
    assert_eq!(messages[0]["role"], json!("user"));
}

#[test]
fn an_answer_round_trips_through_its_events() {
    let body = json!({
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-5",
        "content": [{"type": "text", "text": "forty-two"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 2},
    });
    let (events, dropped) = anthropic::read_answer(body.to_string().as_bytes()).expect("an answer");
    assert!(dropped.is_empty());
    let (written, _) = anthropic::write_answer(&events);
    assert_eq!(written, body);
}

#[test]
fn an_answer_that_asks_for_a_tool_round_trips() {
    let body = json!({
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-5",
        "content": [
            {"type": "text", "text": "looking"},
            {"type": "tool_use", "id": "toolu_9", "name": "search", "input": {"q": "rust"}},
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 5, "output_tokens": 7},
    });
    let (events, _) = anthropic::read_answer(body.to_string().as_bytes()).expect("an answer");
    let (written, _) = anthropic::write_answer(&events);
    assert_eq!(written, body);
}

/// A reason one family has and the other does not must not be handed to a client that
/// switches on a closed set of them.
#[test]
fn a_stop_reason_the_other_family_never_emits_is_flattened_and_said() {
    let body = json!({
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": "cut short"}],
        "stop_reason": "stop_sequence",
    });
    let (events, _) = anthropic::read_answer(body.to_string().as_bytes()).unwrap();

    // Back to its own family, it is unchanged.
    let (home, _) = anthropic::write_answer(&events);
    assert_eq!(home["stop_reason"], json!("stop_sequence"));

    // Across, it becomes the ordinary ending, and the flattening is recorded.
    let (crossed, dropped) = openai::write_answer(&events);
    assert_eq!(crossed["choices"][0]["finish_reason"], json!("stop"));
    assert_eq!(dropped.names(), vec!["stop_reason"]);
}

#[test]
fn a_body_that_is_not_a_conversation_or_an_answer_is_refused() {
    assert!(anthropic::read(json!({"input": "text"}).to_string().as_bytes()).is_err());
    assert!(anthropic::read(json!({"messages": []}).to_string().as_bytes()).is_err());
    assert!(anthropic::read(b"not json").is_err());
    assert!(anthropic::read_answer(json!({"type": "message"}).to_string().as_bytes()).is_err());
}

#[test]
fn a_body_this_family_would_not_have_sent_is_read_for_what_it_holds() {
    let bent = json!({
        "max_tokens": 10,
        "system": [{"type": "text", "text": "carried"}, {"type": "image", "source": {}}],
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "and this"},
            {"type": "tool_result", "content": "an answer quoting no call"},
            {"type": "thinking", "thinking": "a block this pass does not carry"},
        ]}],
        "tools": [{"description": "nameless"}],
        "top_k": 40,
    });
    let (conversation, dropped) =
        anthropic::read(bent.to_string().as_bytes()).expect("a conversation");
    assert_eq!(
        dropped.names(),
        vec![
            "content_parts",
            "system_parts",
            "tool_results",
            "tools",
            "top_k"
        ]
    );
    assert_eq!(conversation.system.as_deref(), Some("carried"));
    assert_eq!(conversation.first_user_text().as_deref(), Some("and this"));
    assert!(conversation.tools.is_empty());
}

#[test]
fn a_tool_result_given_as_blocks_reads_as_its_words() {
    let body = json!({
        "max_tokens": 10,
        "messages": [
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "search", "input": {}},
            ]},
            {"role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_1",
                "content": [{"type": "text", "text": "42 "}, {"type": "text", "text": "results"}],
            }]},
        ],
    });
    let (conversation, dropped) = anthropic::read(body.to_string().as_bytes()).unwrap();
    assert!(dropped.is_empty(), "{dropped}");
    assert_eq!(
        conversation.messages[1].blocks[0],
        Block::ToolResult {
            id: ToolCallId::new("toolu_1").unwrap(),
            content: "42 results".to_owned(),
        }
    );
}

#[test]
fn a_use_this_family_spells_incompletely_is_refused() {
    for bent in [
        json!({"max_tokens": 1, "messages": [{"role": "assistant", "content": [
            {"type": "tool_use", "name": "search"},
        ]}]}),
        json!({"max_tokens": 1, "messages": [{"role": "assistant", "content": [
            {"type": "tool_use", "id": "toolu_1"},
        ]}]}),
    ] {
        assert!(
            anthropic::read(bent.to_string().as_bytes()).is_err(),
            "{bent}"
        );
    }
}

#[test]
fn every_reason_this_family_stops_for_is_carried() {
    for said in [
        "end_turn",
        "max_tokens",
        "tool_use",
        "refusal",
        "stop_sequence",
    ] {
        let body = json!({
            "content": [{"type": "text", "text": "x"}],
            "stop_reason": said,
        });
        let (events, _) = anthropic::read_answer(body.to_string().as_bytes()).unwrap();
        let (written, _) = anthropic::write_answer(&events);
        assert_eq!(
            written["stop_reason"],
            json!(said),
            "{said} did not survive"
        );
    }
}

#[test]
fn an_answer_block_this_pass_does_not_carry_is_named() {
    let body = json!({
        "content": [{"type": "text", "text": "said"}, {"type": "thinking", "thinking": "not"}],
    });
    let (events, dropped) = anthropic::read_answer(body.to_string().as_bytes()).unwrap();
    assert_eq!(dropped.names(), vec!["content_parts"]);
    let (written, _) = anthropic::write_answer(&events);
    assert_eq!(written["content"][0]["text"], json!("said"));
}
