//! The family the gateway could not read at all, and the three-way equality that is
//! what this milestone claims.

use ip_protocol::{Block, Conversation, Role, ToolName, anthropic, gemini, openai};
use serde_json::{Value, json};

/// Reads a body, writes it back, and hands over both the shape and the second body.
fn around(body: &Value) -> (Conversation, Value) {
    let (conversation, dropped) =
        gemini::read(body.to_string().as_bytes()).expect("a conversation");
    assert!(dropped.is_empty(), "reading dropped {dropped}");
    let (written, dropped) = gemini::write(&conversation);
    assert!(dropped.is_empty(), "writing dropped {dropped}");
    (conversation, written)
}

#[test]
fn a_plain_chat_round_trips() {
    let body = json!({
        "contents": [
            {"role": "user", "parts": [{"text": "what did we spend?"}]},
            {"role": "model", "parts": [{"text": "on what?"}]},
            {"role": "user", "parts": [{"text": "in march"}]},
        ],
        "systemInstruction": {"parts": [{"text": "you audit accounts"}]},
        "generationConfig": {
            "temperature": 0.7,
            "topP": 0.9,
            "maxOutputTokens": 256,
            "stopSequences": ["END"],
        },
    });
    let (conversation, written) = around(&body);

    assert_eq!(conversation.system.as_deref(), Some("you audit accounts"));
    assert_eq!(conversation.messages.len(), 3);
    assert_eq!(
        conversation.messages[1].role,
        Role::Assistant,
        "this family calls the assistant the model"
    );
    assert_eq!(
        conversation.model, None,
        "the model is named in the path, not the body"
    );
    assert_eq!(conversation.sampling.max_tokens, Some(256));
    assert_eq!(conversation.sampling.stop, vec!["END".to_owned()]);
    assert_eq!(written, body, "the body means what it meant");
}

#[test]
fn a_conversation_with_tools_round_trips_with_its_pairing() {
    let body = json!({
        "contents": [
            {"role": "user", "parts": [{"text": "search for rust"}]},
            {"role": "model", "parts": [
                {"functionCall": {"name": "search", "args": {"q": "rust"}}},
            ]},
            {"role": "user", "parts": [
                {"functionResponse": {"name": "search", "response": {"hits": 42}}},
            ]},
        ],
        "tools": [{"functionDeclarations": [{
            "name": "search",
            "description": "look something up",
            "parameters": {"type": "object"},
        }]}],
    });
    let (conversation, written) = around(&body);

    let Block::ToolCall { id, name, .. } = &conversation.messages[1].blocks[0] else {
        panic!("the model asked for a tool")
    };
    assert_eq!(name, &ToolName::new("search").unwrap());
    let Block::ToolResult { id: answered, .. } = &conversation.messages[2].blocks[0] else {
        panic!("the user answered it")
    };
    assert_eq!(
        id, answered,
        "a family that joins by name is given an id that joins the same pairs"
    );
    assert_eq!(written, body, "the body means what it meant");
}

/// One turn calling the same function twice: this family has no ids, so the pairing is
/// by name and order, and the synthesised ids must reproduce exactly that.
#[test]
fn two_calls_to_one_function_stay_apart() {
    let body = json!({
        "contents": [
            {"role": "user", "parts": [{"text": "search twice"}]},
            {"role": "model", "parts": [
                {"functionCall": {"name": "search", "args": {"q": "rust"}}},
                {"functionCall": {"name": "search", "args": {"q": "go"}}},
            ]},
            {"role": "user", "parts": [
                {"functionResponse": {"name": "search", "response": {"hits": 1}}},
                {"functionResponse": {"name": "search", "response": {"hits": 2}}},
            ]},
        ],
    });
    let (conversation, written) = around(&body);

    let calls = &conversation.messages[1].blocks;
    let results = &conversation.messages[2].blocks;
    let ids = |blocks: &[Block]| -> Vec<String> {
        blocks
            .iter()
            .map(|block| match block {
                Block::ToolCall { id, .. } | Block::ToolResult { id, .. } => id.to_string(),
                Block::Text(_) => unreachable!("no text here"),
            })
            .collect()
    };
    assert_eq!(ids(calls), vec!["search#0", "search#1"]);
    assert_eq!(
        ids(results),
        vec!["search#0", "search#1"],
        "the nth answer pairs with the nth call, which is all this family can express"
    );
    assert_eq!(written, body);
}

/// A result whose call is not in the conversation cannot be written to a family that
/// names the function rather than quoting an id. It is dropped, and said.
#[test]
fn a_result_whose_call_is_missing_cannot_be_named_and_is_said() {
    let orphaned = json!({
        "messages": [{"role": "tool", "tool_call_id": "call_1", "content": "42"}],
    });
    let (conversation, _) = openai::read(orphaned.to_string().as_bytes()).unwrap();
    let (written, dropped) = gemini::write(&conversation);
    assert_eq!(dropped.names(), vec!["tool_results"]);
    assert_eq!(
        written["contents"][0]["parts"].as_array().unwrap().len(),
        0,
        "nothing was invented to stand in for it"
    );
}

/// The claim the milestone rests on, now across all three.
#[test]
fn the_same_conversation_in_three_families_reads_to_one_shape() {
    let in_openai = json!({
        "messages": [
            {"role": "system", "content": "you audit accounts"},
            {"role": "user", "content": "search for rust"},
            {"role": "assistant", "content": null, "tool_calls": [{
                "id": "search#0",
                "type": "function",
                "function": {"name": "search", "arguments": "{\"q\":\"rust\"}"},
            }]},
            {"role": "tool", "tool_call_id": "search#0", "content": "{\"hits\":42}"},
        ],
    });
    let in_anthropic = json!({
        "system": "you audit accounts",
        "messages": [
            {"role": "user", "content": "search for rust"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "search#0", "name": "search", "input": {"q": "rust"}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "search#0", "content": "{\"hits\":42}"},
            ]},
        ],
    });
    let in_gemini = json!({
        "systemInstruction": {"parts": [{"text": "you audit accounts"}]},
        "contents": [
            {"role": "user", "parts": [{"text": "search for rust"}]},
            {"role": "model", "parts": [
                {"functionCall": {"name": "search", "args": {"q": "rust"}}},
            ]},
            {"role": "user", "parts": [
                {"functionResponse": {"name": "search", "response": {"hits": 42}}},
            ]},
        ],
    });

    let (from_openai, _) = openai::read(in_openai.to_string().as_bytes()).unwrap();
    let (from_anthropic, _) = anthropic::read(in_anthropic.to_string().as_bytes()).unwrap();
    let (from_gemini, _) = gemini::read(in_gemini.to_string().as_bytes()).unwrap();
    assert_eq!(from_openai, from_anthropic);
    assert_eq!(from_anthropic, from_gemini);
}

#[test]
fn a_conversation_crosses_into_this_family() {
    let in_openai = json!({
        "model": "gpt-4o",
        "messages": [
            {"role": "system", "content": "you audit accounts"},
            {"role": "user", "content": "hello"},
        ],
        "max_tokens": 100,
    });
    let (conversation, _) = openai::read(in_openai.to_string().as_bytes()).unwrap();
    let (crossed, dropped) = gemini::write(&conversation);
    assert!(
        dropped.is_empty(),
        "the model travelling in the path is this family's own shape, not a loss"
    );
    assert_eq!(
        crossed,
        json!({
            "contents": [{"role": "user", "parts": [{"text": "hello"}]}],
            "systemInstruction": {"parts": [{"text": "you audit accounts"}]},
            "generationConfig": {"maxOutputTokens": 100},
        })
    );
}

#[test]
fn an_answer_round_trips_through_its_events() {
    let body = json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "forty-two"}]},
            "finishReason": "STOP",
        }],
        "modelVersion": "gemini-2.5-pro",
        "usageMetadata": {
            "promptTokenCount": 10,
            "candidatesTokenCount": 2,
            "totalTokenCount": 12,
        },
    });
    let (events, dropped) = gemini::read_answer(body.to_string().as_bytes()).expect("an answer");
    assert!(dropped.is_empty());
    let (written, _) = gemini::write_answer(&events);
    assert_eq!(written, body);
}

#[test]
fn an_answer_that_asks_for_a_tool_crosses_to_a_family_that_says_so() {
    let body = json!({
        "candidates": [{
            "content": {"role": "model", "parts": [
                {"functionCall": {"name": "search", "args": {"q": "rust"}}},
            ]},
            "finishReason": "STOP",
        }],
    });
    let (events, _) = gemini::read_answer(body.to_string().as_bytes()).unwrap();
    let (crossed, _) = openai::write_answer(&events);
    let call = &crossed["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["id"], json!("search#0"));
    assert_eq!(call["function"]["name"], json!("search"));
    assert_eq!(
        crossed["choices"][0]["finish_reason"],
        json!("stop"),
        "this family stops normally and leaves a call behind, and says so that way"
    );
}

#[test]
fn an_answer_withheld_is_read_as_withheld_whatever_this_family_called_it() {
    for reason in ["SAFETY", "RECITATION", "PROHIBITED_CONTENT"] {
        let body = json!({
            "candidates": [{"content": {"role": "model", "parts": []}, "finishReason": reason}],
        });
        let (events, _) = gemini::read_answer(body.to_string().as_bytes()).unwrap();
        let (crossed, dropped) = openai::write_answer(&events);
        assert_eq!(
            crossed["choices"][0]["finish_reason"],
            json!("content_filter"),
            "{reason} is a withheld answer"
        );
        assert!(dropped.is_empty(), "nothing was flattened away: {dropped}");
    }
}

#[test]
fn a_body_that_is_not_a_conversation_or_an_answer_is_refused() {
    assert!(gemini::read(json!({"messages": []}).to_string().as_bytes()).is_err());
    assert!(gemini::read(json!({"contents": []}).to_string().as_bytes()).is_err());
    assert!(gemini::read(b"not json").is_err());
    assert!(gemini::read_answer(json!({"modelVersion": "x"}).to_string().as_bytes()).is_err());
}

#[test]
fn a_body_this_family_would_not_have_sent_is_read_for_what_it_holds() {
    let bent = json!({
        "contents": [{"role": "user", "parts": [
            {"text": "carried"},
            {"functionCall": {"args": {"q": "x"}}},
            {"functionResponse": {"response": {"r": 1}}},
            {"inlineData": {"mimeType": "image/png", "data": "..."}},
        ]}],
        "tools": [{"functionDeclarations": [{"description": "nameless"}]}],
        "safetySettings": [{"category": "HARM_CATEGORY_HARASSMENT"}],
    });
    let (conversation, dropped) =
        gemini::read(bent.to_string().as_bytes()).expect("a conversation");
    assert_eq!(
        dropped.names(),
        vec!["content_parts", "safetySettings", "tool_results", "tools"]
    );
    assert_eq!(conversation.first_user_text().as_deref(), Some("carried"));
    assert!(conversation.tools.is_empty());
}

#[test]
fn a_function_response_crosses_as_json_where_it_was_json_and_as_words_where_it_was_not() {
    // What this family sends as an object comes back as that object.
    let object = json!({"contents": [
        {"role": "model", "parts": [{"functionCall": {"name": "search", "args": {}}}]},
        {"role": "user", "parts": [{"functionResponse": {"name": "search", "response": {"hits": 2}}}]},
    ]});
    let (conversation, _) = gemini::read(object.to_string().as_bytes()).unwrap();
    let (written, _) = gemini::write(&conversation);
    assert_eq!(
        written["contents"][1]["parts"][0]["functionResponse"]["response"],
        json!({"hits": 2})
    );

    // And a result another family gave as plain words is given a place to sit.
    let worded = json!({"messages": [
        {"role": "assistant", "tool_calls": [
            {"id": "c1", "type": "function", "function": {"name": "search", "arguments": "{}"}},
        ]},
        {"role": "tool", "tool_call_id": "c1", "content": "forty-two"},
    ]});
    let (conversation, _) = openai::read(worded.to_string().as_bytes()).unwrap();
    let (written, dropped) = gemini::write(&conversation);
    assert!(dropped.is_empty(), "{dropped}");
    assert_eq!(
        written["contents"][1]["parts"][0]["functionResponse"]["response"],
        json!({"result": "forty-two"})
    );
}

#[test]
fn every_reason_this_family_stops_for_is_carried() {
    for said in ["STOP", "MAX_TOKENS", "SAFETY"] {
        let body = json!({"candidates": [{
            "content": {"role": "model", "parts": [{"text": "x"}]},
            "finishReason": said,
        }]});
        let (events, _) = gemini::read_answer(body.to_string().as_bytes()).unwrap();
        let (written, _) = gemini::write_answer(&events);
        assert_eq!(
            written["candidates"][0]["finishReason"],
            json!(said),
            "{said} did not survive"
        );
    }
    // A reason only this family gives is kept by its own name rather than guessed at.
    let other = json!({"candidates": [{"content": {"parts": []}, "finishReason": "OTHER"}]});
    let (events, _) = gemini::read_answer(other.to_string().as_bytes()).unwrap();
    let (written, _) = gemini::write_answer(&events);
    assert_eq!(written["candidates"][0]["finishReason"], json!("OTHER"));
}

#[test]
fn an_answer_part_this_pass_does_not_carry_is_named() {
    let body = json!({"candidates": [{"content": {"parts": [
        {"text": "said"},
        {"inlineData": {"mimeType": "image/png", "data": "..."}},
    ]}}]});
    let (events, dropped) = gemini::read_answer(body.to_string().as_bytes()).unwrap();
    assert_eq!(dropped.names(), vec!["content_parts"]);
    let (written, _) = gemini::write_answer(&events);
    assert_eq!(
        written["candidates"][0]["content"]["parts"][0]["text"],
        json!("said")
    );
}

#[test]
fn an_answer_of_several_candidates_keeps_the_first_and_says_it_dropped_the_rest() {
    let body = json!({"candidates": [
        {"content": {"parts": [{"text": "one"}]}},
        {"content": {"parts": [{"text": "two"}]}},
    ]});
    let (events, dropped) = gemini::read_answer(body.to_string().as_bytes()).unwrap();
    assert_eq!(dropped.names(), vec!["candidates"]);
    let (written, _) = gemini::write_answer(&events);
    assert_eq!(written["candidates"].as_array().unwrap().len(), 1);
    assert_eq!(
        written["candidates"][0]["content"]["parts"][0]["text"],
        json!("one")
    );
}
