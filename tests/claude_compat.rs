//! Claude-Code compatibility harness: Anthropic-protocol invariants that must
//! hold for translated traffic no matter what shape the upstream returns.
//!
//! Two layers:
//! - Mock conformance (this file, runs in CI): the shared translators in
//!   `consolette::providers` are driven with recorded/adversarial upstream
//!   shapes; every output must satisfy the message/SSE invariants below.
//! - Live probes (`tests/claude_live_probe.rs`, `#[ignore]`): the same
//!   invariants checked against real pinned models through a running daemon.
//!
//! Background: Claude Code's agentic loop stops the moment a turn carries
//! `stop_reason: "tool_use"` with zero `tool_use` blocks (or tool blocks
//! whose `input` isn't an object). Every scenario here pins one way that
//! used to break.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{json, Value};

use consolette::providers::{
    translate_anthropic_request_to_openai, translate_openai_response_to_anthropic,
};

/// Anthropic message invariants for anything handed to a Claude-Code-style
/// client: typed blocks, `tool_use` blocks with object input, stop reason
/// consistent with the blocks present.
fn assert_message_invariants(v: &Value) {
    assert_eq!(v["type"], json!("message"), "not a message: {v}");
    assert_eq!(v["role"], json!("assistant"), "role drift: {v}");
    let content = v["content"].as_array().expect("content is not an array");
    assert!(!content.is_empty(), "empty content array: {v}");

    let mut kinds = Vec::new();
    for (i, b) in content.iter().enumerate() {
        let t = b["type"].as_str().unwrap_or("<missing>");
        kinds.push(t.to_string());
        match t {
            "text" => {
                assert!(b["text"].is_string(), "block {i} text not a string: {b}");
            }
            "tool_use" => {
                assert!(b["id"].as_str().is_some_and(|s| !s.is_empty()));
                assert!(b["name"].as_str().is_some_and(|s| !s.is_empty()));
                assert!(
                    b["input"].is_object(),
                    "block {i} input is not an object: {b}"
                );
            }
            "thinking" => {
                assert!(b["thinking"].is_string(), "block {i} thinking not text");
            }
            other => panic!("unexpected block type {other:?} in: {v}"),
        }
    }

    let stop = v["stop_reason"].as_str().unwrap_or("<missing>");
    if stop == "tool_use" {
        assert!(
            kinds.iter().any(|k| k == "tool_use"),
            "stop_reason=tool_use with zero tool_use blocks: {v}"
        );
    }
    if kinds.iter().any(|k| k == "tool_use") {
        assert_eq!(
            stop, "tool_use",
            "tool_use blocks with stop_reason={stop}: {v}"
        );
    }
    if let Some(pos) = kinds.iter().position(|k| k == "text") {
        for k in kinds.iter().take(pos) {
            assert_eq!(k, "thinking", "non-thinking block before text: {kinds:?}");
        }
    }
}

#[test]
fn plain_completion_stays_wellformed() {
    let openai = json!({
        "id": "chatcmpl-1", "model": "m",
        "choices": [{
            "message": {"role": "assistant", "content": "hi"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 2}
    });

    let out = translate_openai_response_to_anthropic(&openai, None);
    assert_message_invariants(&out);
    assert_eq!(out["stop_reason"], json!("end_turn"));
}

#[test]
fn tool_calls_become_tool_use_blocks() {
    // The premature-stop repro: model wants a tool, proxy must not swallow it.
    let openai = json!({
        "id": "chatcmpl-2", "model": "m",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1", "type": "function",
                    "function": {"name": "get_time", "arguments": "{\"zone\":\"utc\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 50, "completion_tokens": 12}
    });

    let out = translate_openai_response_to_anthropic(&openai, None);
    assert_message_invariants(&out);
    assert_eq!(out["stop_reason"], json!("tool_use"));
    assert_eq!(
        out["content"],
        json!([{"type": "tool_use", "id": "call_1",
                "name": "get_time", "input": {"zone": "utc"}}])
    );
}

#[test]
fn text_and_tool_calls_coexist() {
    let openai = json!({
        "id": "chatcmpl-3", "model": "m",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "checking",
                "tool_calls": [{
                    "id": "c1", "type": "function",
                    "function": {"name": "get_time", "arguments": "{}"}
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 50, "completion_tokens": 12}
    });

    let out = translate_openai_response_to_anthropic(&openai, None);
    assert_message_invariants(&out);
    assert_eq!(out["content"][0]["type"], json!("text"));
    assert_eq!(out["content"][1]["type"], json!("tool_use"));
}

#[test]
fn malformed_tool_arguments_become_empty_object() {
    // Truncated (length-stop) or vendor-mangled arguments must stay a
    // well-formed tool_use block the client can answer with a tool error,
    // never a dropped call or a non-object input.
    let openai = json!({
        "id": "chatcmpl-4", "model": "m",
        "choices": [{
            "message": {
                "role": "assistant", "content": null,
                "tool_calls": [{
                    "id": "c9", "type": "function",
                    "function": {"name": "get_time", "arguments": "{\"zone\":"}
                }]
            },
            "finish_reason": "length"
        }],
        "usage": {"prompt_tokens": 50, "completion_tokens": 8}
    });

    let out = translate_openai_response_to_anthropic(&openai, None);
    assert_message_invariants(&out);
    assert_eq!(out["content"][0]["input"], json!({}));
}

#[tokio::test]
async fn request_forwards_tools_and_choice() {
    // Claude Code always sends tools; dropping them silently reduces the
    // model to chatter (the other half of premature stopping).
    let anthropic = json!({
        "model": "m", "max_tokens": 64,
        "system": "sys",
        "tools": [{
            "name": "get_time",
            "description": "current time",
            "input_schema": {"type": "object", "properties": {}},
            "cache_control": {"type": "ephemeral"}
        }],
        "tool_choice": {"type": "auto"},
        "messages": [{"role": "user", "content": "what time is it"}]
    });

    let out = translate_anthropic_request_to_openai(&anthropic).await;
    assert_eq!(
        out["tools"],
        json!([{"type": "function", "function": {
            "name": "get_time", "description": "current time",
            "parameters": {"type": "object", "properties": {}}
        }}])
    );
    assert_eq!(out["tool_choice"], json!("auto"));
}

#[tokio::test]
async fn request_maps_tool_result_history_to_tool_role() {
    // Multi-turn continuity: without this the model loses tool context and
    // the loop degrades after the first call.
    let anthropic = json!({
        "model": "m", "max_tokens": 64,
        "messages": [
            {"role": "user", "content": "time?"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "checking"},
                {"type": "tool_use", "id": "c1", "name": "get_time", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "c1", "content": "noon"}
            ]}
        ]
    });

    let out = translate_anthropic_request_to_openai(&anthropic).await;
    let msgs = out["messages"].as_array().expect("messages array");
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[1]["role"], json!("assistant"));
    assert_eq!(
        msgs[1]["tool_calls"],
        json!([{"id": "c1", "type": "function",
                "function": {"name": "get_time", "arguments": "{}"}}])
    );
    assert_eq!(msgs[2]["role"], json!("tool"));
    assert_eq!(msgs[2]["tool_call_id"], json!("c1"));
    assert_eq!(msgs[2]["content"], json!("noon"));
}
