//! Buffered-SSE synthesis for streaming emulation (Story 1.1.3).
//!
//! V1 buffers: the loop runs internally with `stream: false`, then the final
//! assembled message is re-serialized here as a well-formed Anthropic SSE
//! sequence (`message_start`, per-block start/delta/stop, `message_delta`
//! with `stop_reason` + `usage`, `message_stop`).
//!
//! Trade-off (pre-mortem failure 3): time-to-first-byte degrades to
//! time-to-final-answer for search turns only. Mid-stream incremental
//! execution is explicitly deferred, not attempted.
//!
//! The `message_delta` frame carries the aggregate `usage` so
//! `CostTrackingStream` records the exact loop total (single-count — see
//! `stream_cost_should_count_once_when_loop_recorded`). Server blocks travel
//! as a single `input_json_delta` chunk each; text blocks as `text_delta`.
//! Function-shape `tool_use` is never emitted on the wire.

use serde_json::{json, Value};

/// Serialize one assembled Anthropic message into SSE frames, each
/// `\n\n`-terminated (`event: …\ndata: …\n\n`).
#[must_use]
pub fn synthesize_sse(message: &Value) -> Vec<String> {
    let mut frames = Vec::new();
    let empty_blocks: Vec<Value> = Vec::new();
    let blocks = message
        .get("content")
        .and_then(Value::as_array)
        .unwrap_or(&empty_blocks);

    let usage = message.get("usage").cloned().unwrap_or(json!({
        "input_tokens": 0, "output_tokens": 0
    }));
    let stop_reason = message
        .get("stop_reason")
        .and_then(Value::as_str)
        .unwrap_or("end_turn");
    let id = message
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("msg_emul");
    let model = message
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown");

    frames.push(frame(
        "message_start",
        &json!({"type": "message_start",
            "message": {"id": id, "type": "message", "role": "assistant",
                        "model": model, "content": [], "stop_reason": null,
                        "usage": {"input_tokens": 0, "output_tokens": 0}}}),
    ));

    for (index, block) in blocks.iter().enumerate() {
        frames.push(frame(
            "content_block_start",
            &json!({"type": "content_block_start", "index": index,
                    "content_block": start_shape(block)}),
        ));
        frames.push(frame(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": index,
                    "delta": delta_shape(block)}),
        ));
        frames.push(frame(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": index}),
        ));
    }

    frames.push(frame(
        "message_delta",
        &json!({"type": "message_delta",
                "delta": {"stop_reason": stop_reason},
                "usage": usage}),
    ));
    frames.push(frame("message_stop", &json!({"type": "message_stop"})));
    frames
}

fn frame(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// The `content_block` preview in `content_block_start`: always an
/// empty-text block — the real payload arrives in the delta (text as
/// `text_delta`, everything else as one `input_json_delta` chunk).
fn start_shape(_block: &Value) -> Value {
    json!({"type": "text", "text": ""})
}

/// Delta payload per block kind. Server blocks have no native delta type, so
/// the full block JSON goes out as one `input_json_delta` chunk.
fn delta_shape(block: &Value) -> Value {
    if block.get("type").and_then(Value::as_str) == Some("text") {
        let text = block.get("text").and_then(Value::as_str).unwrap_or("");
        json!({"type": "text_delta", "text": text})
    } else {
        json!({"type": "input_json_delta", "partial_json": block.to_string()})
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_message() -> Value {
        json!({
            "id": "msg_emul_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [
                {"type": "server_tool_use", "id": "srvtoolu_emul_00_00",
                 "name": "web_search", "input": {"query": "q"}},
                {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_emul_00_00",
                 "content": [{"type": "web_search_result", "title": "t",
                              "url": "https://example.com", "text": "s"}]},
                {"type": "text", "text": "Grounded answer."}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 30, "output_tokens": 12,
                      "server_tool_use": {"web_search_requests": 1}}
        })
    }

    #[test]
    fn sse_should_never_emit_function_tool_use_on_wire() {
        let frames = synthesize_sse(&sample_message());
        let wire = frames.concat();

        assert!(
            !wire.contains("\"tool_use\""),
            "function-shape tool_use must never appear on the wire: {wire}"
        );
        assert!(
            wire.contains("server_tool_use"),
            "server blocks must be present"
        );
    }

    /// Golden frame-by-frame test: the exact event order and the
    /// usage-carrying `message_delta` (drives single-cost-counting).
    #[test]
    fn synthesize_should_emit_wellformed_sequence() {
        let frames = synthesize_sse(&sample_message());

        // 1 message_start + 3 blocks × 3 + message_delta + message_stop.
        assert_eq!(frames.len(), 1 + 3 * 3 + 2);

        assert!(frames[0].starts_with("event: message_start\n"));
        assert!(frames[frames.len() - 2].starts_with("event: message_delta\n"));
        assert!(frames[frames.len() - 1].starts_with("event: message_stop\n"));

        // Per-block triples in order: start, delta, stop.
        for (block_index, chunk) in frames[1..frames.len() - 2].chunks(3).enumerate() {
            assert_eq!(chunk.len(), 3, "block {block_index} triple: {chunk:?}");
            assert!(
                chunk[0].starts_with("event: content_block_start\n"),
                "{chunk:?}"
            );
            assert!(
                chunk[1].starts_with("event: content_block_delta\n"),
                "{chunk:?}"
            );
            assert!(
                chunk[2].starts_with("event: content_block_stop\n"),
                "{chunk:?}"
            );
        }

        let delta_data = frames[frames.len() - 2]
            .split_once("data: ")
            .map(|(_, data)| data)
            .unwrap();
        let delta: Value = serde_json::from_str(delta_data.trim()).unwrap();
        assert_eq!(delta["delta"]["stop_reason"], json!("end_turn"));
        assert_eq!(delta["usage"]["input_tokens"], json!(30));
        assert_eq!(
            delta["usage"]["server_tool_use"]["web_search_requests"],
            json!(1)
        );
    }
}
