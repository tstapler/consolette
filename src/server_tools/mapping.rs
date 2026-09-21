//! Turn accumulation + server-shape mapping (Story 1.1.2).
//!
//! Pure functions. The loop keeps message history in **function shape**
//! (what upstreams accept); only the final client-facing turn is mapped to
//! the native `server_tool_use` + `web_search_tool_result` shape here.

use serde_json::{json, Value};

use crate::server_tools::detect::SERVER_TOOL_NAME;
use crate::server_tools::executor::SearchResult;

/// One upstream `tool_use(name = "web_search")` call awaiting execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamSearchCall {
    pub id: String,
    pub query: String,
}

/// Maximum snippet characters kept per result before the `…(truncated)`
/// marker applies.
pub const MAX_SNIPPET_CHARS: usize = 2000;

/// Extract upstream `web_search` calls from an Anthropic response's
/// `content[]` (`tool_use` blocks named `web_search`). Non-string or missing
/// `input.query` values yield an empty query, which the loop feeds back as
/// an error-content result rather than calling the executor.
#[must_use]
pub fn find_web_search_calls(response: &Value) -> Vec<UpstreamSearchCall> {
    response
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| {
                    b.get("type").and_then(Value::as_str) == Some("tool_use")
                        && b.get("name").and_then(Value::as_str) == Some(SERVER_TOOL_NAME)
                })
                .map(|b| UpstreamSearchCall {
                    id: b
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("call_unknown")
                        .to_string(),
                    query: b
                        .get("input")
                        .and_then(|input| input.get("query"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// All `tool_use` blocks in a response that are NOT `web_search` (user tools
/// the client must execute itself — mixed-turn passthrough).
#[must_use]
pub fn find_other_tool_calls(response: &Value) -> Vec<Value> {
    response
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| {
                    b.get("type").and_then(Value::as_str) == Some("tool_use")
                        && b.get("name").and_then(Value::as_str) != Some(SERVER_TOOL_NAME)
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Deterministic server-block id for iteration `iteration`, result `index`
/// (`srvtoolu_…` prefix matches the native shape).
#[must_use]
pub fn server_block_id(iteration: u32, index: usize) -> String {
    format!("srvtoolu_emul_{iteration:02}_{index:02}")
}

/// Build a `server_tool_use` block pairing with
/// [`to_web_search_tool_result`] via `id`.
#[must_use]
pub fn to_server_tool_use(server_id: &str, query: &str) -> Value {
    json!({
        "type": "server_tool_use",
        "id": server_id,
        "name": SERVER_TOOL_NAME,
        "input": {"query": query}
    })
}

/// Map executor results to a `web_search_tool_result` block.
///
/// Fidelity note (lossy by design): native results carry
/// `encrypted_content` blobs we cannot mint, so each hit is
/// `{type: "web_search_result", title, url, text}` with `text` truncated per
/// [`MAX_SNIPPET_CHARS`] (see the lossy-mapping table in `super`).
#[must_use]
pub fn to_web_search_tool_result(server_id: &str, results: &[SearchResult]) -> Value {
    let content: Vec<Value> = results
        .iter()
        .map(|r| {
            json!({
                "type": "web_search_result",
                "title": r.title,
                "url": r.url,
                "text": truncate_with_marker(&r.description, MAX_SNIPPET_CHARS)
            })
        })
        .collect();
    json!({
        "type": "web_search_tool_result",
        "tool_use_id": server_id,
        "content": content
    })
}

/// Error-content result for a failed search (executor error, empty query):
/// the failure is surfaced to the model as turn content, never as a
/// `ProviderError`, so backend outages degrade instead of failing requests.
#[must_use]
pub fn to_error_tool_result(server_id: &str, message: &str) -> Value {
    json!({
        "type": "web_search_tool_result",
        "tool_use_id": server_id,
        "content": [{"type": "text", "text": message}]
    })
}

/// Function-shape `tool_result` for loop-internal history (re-dispatch).
#[must_use]
pub fn to_function_tool_result(call_id: &str, text: &str) -> Value {
    json!({
        "type": "tool_result",
        "tool_use_id": call_id,
        "content": text
    })
}

/// Render executed results as the plain-text feedback the model sees on the
/// next loop iteration (function history has no server blocks).
#[must_use]
pub fn results_to_text(results: &[SearchResult]) -> String {
    results
        .iter()
        .map(|r| {
            format!(
                "{}\n{}\n{}",
                r.title,
                r.url,
                truncate_with_marker(&r.description, MAX_SNIPPET_CHARS)
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Truncate to `max_chars` characters (char-boundary safe) with a
/// `…(truncated)` marker when anything was cut.
#[must_use]
pub fn truncate_with_marker(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars).collect();
    format!("{kept}…(truncated)")
}

/// Aggregate per-iteration `(input_tokens, output_tokens)` pairs into the
/// final `usage` object, including the native
/// `server_tool_use.web_search_requests` counter.
#[must_use]
pub fn aggregate_usage(usages: &[(u64, u64)], searches_executed: u64) -> Value {
    let (input, output) = usages
        .iter()
        .fold((0_u64, 0_u64), |(acc_in, acc_out), (i, o)| {
            (acc_in.saturating_add(*i), acc_out.saturating_add(*o))
        });
    json!({
        "input_tokens": input,
        "output_tokens": output,
        "server_tool_use": {"web_search_requests": searches_executed}
    })
}

/// Read `(input_tokens, output_tokens)` from an Anthropic response's
/// `usage` object (missing → zeros).
#[must_use]
pub fn extract_usage_pair(response: &Value) -> (u64, u64) {
    let usage = response.get("usage");
    let input = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    (input, output)
}

/// Convert client-echoed server blocks in message history back to function
/// shape before re-dispatch: `server_tool_use` → `tool_use`,
/// `web_search_tool_result` → `tool_result` (ids preserved so pairing
/// holds). Used both for prior emulated turns in a multi-turn conversation
/// and for the loop's own re-dispatch hygiene.
#[must_use]
pub fn convert_history_for_upstream(messages: &Value) -> Value {
    let Value::Array(turns) = messages else {
        return messages.clone();
    };
    Value::Array(
        turns
            .iter()
            .map(|turn| {
                let mut turn = turn.clone();
                if let Some(blocks) = turn.get_mut("content").and_then(Value::as_array_mut) {
                    let mut out = Vec::with_capacity(blocks.len());
                    for block in blocks.drain(..) {
                        match block.get("type").and_then(Value::as_str) {
                            Some("server_tool_use") => {
                                let id = block
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("call_unknown");
                                let input = block.get("input").cloned().unwrap_or(Value::Null);
                                out.push(json!({
                                    "type": "tool_use",
                                    "id": id,
                                    "name": SERVER_TOOL_NAME,
                                    "input": input
                                }));
                            }
                            Some("web_search_tool_result") => {
                                let id = block
                                    .get("tool_use_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                out.push(json!({
                                    "type": "tool_result",
                                    "tool_use_id": id,
                                    "content": flatten_result_content(&block)
                                }));
                            }
                            _ => out.push(block),
                        }
                    }
                    *blocks = out;
                }
                turn
            })
            .collect(),
    )
}

/// Flatten a `web_search_tool_result`'s content to plain text for function
/// history: result blocks join as `title\nurl\ntext`, text blocks inline.
fn flatten_result_content(block: &Value) -> String {
    block
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| match item.get("type").and_then(Value::as_str) {
                    Some("web_search_result") => {
                        let title = item.get("title").and_then(Value::as_str).unwrap_or("");
                        let url = item.get("url").and_then(Value::as_str).unwrap_or("");
                        let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                        format!("{title}\n{url}\n{text}")
                    }
                    _ => item
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
}

/// One executed search paired with its client-facing blocks.
#[derive(Debug, Clone)]
pub struct ExecutedSearch {
    pub call_id: String,
    pub query: String,
    pub server_id: String,
    /// `Ok` results, or the `Err` message already rendered as error content.
    pub result: Result<Vec<SearchResult>, String>,
}

fn push_pair(content: &mut Vec<Value>, done: &ExecutedSearch) {
    content.push(to_server_tool_use(&done.server_id, &done.query));
    match &done.result {
        Ok(results) => content.push(to_web_search_tool_result(&done.server_id, results)),
        Err(message) => content.push(to_error_tool_result(&done.server_id, message)),
    }
}

/// Map the loop's last upstream response to the client-facing turn.
///
/// `executed` accumulates every executed pair across all iterations, in
/// execution order. Pairs whose call still appears in `last_response` render
/// inline at that position; pairs from earlier iterations (their calls are
/// already consumed into history) are prepended in order, so the client sees
/// the full search trail ahead of the final answer.
///
/// - Executed `web_search` calls become `server_tool_use` +
///   `web_search_tool_result` pairs (error results → error content).
/// - Unexecuted `web_search` calls (iteration cap hit) are stripped: the
///   client sees the model's text answer, never a dangling call.
/// - Non-search `tool_use` blocks pass through verbatim with
///   `stop_reason: "tool_use"` preserved (mixed turn — the client executes
///   the rest; C2: synthetic defs never appear on the client-visible
///   surface since the client holds the original server defs).
/// - Otherwise `stop_reason: "end_turn"`.
#[must_use]
pub fn build_final_turn(
    last_response: &Value,
    executed: &[ExecutedSearch],
) -> (Vec<Value>, String) {
    use std::collections::{HashMap, HashSet};

    let current_calls: HashSet<&str> = last_response
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| {
                    b.get("type").and_then(Value::as_str) == Some("tool_use")
                        && b.get("name").and_then(Value::as_str) == Some(SERVER_TOOL_NAME)
                })
                .filter_map(|b| b.get("id").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    let by_call: HashMap<&str, &ExecutedSearch> =
        executed.iter().map(|e| (e.call_id.as_str(), e)).collect();

    let mut content = Vec::new();
    // Earlier iterations' pairs first, in execution order.
    for done in executed {
        if !current_calls.contains(done.call_id.as_str()) {
            push_pair(&mut content, done);
        }
    }

    let mut saw_passthrough_tool_use = false;
    if let Some(blocks) = last_response.get("content").and_then(Value::as_array) {
        for block in blocks {
            let is_tool_use = block.get("type").and_then(Value::as_str) == Some("tool_use");
            let name = block.get("name").and_then(Value::as_str).unwrap_or("");
            if is_tool_use && name == SERVER_TOOL_NAME {
                let call_id = block.get("id").and_then(Value::as_str).unwrap_or("");
                if let Some(done) = by_call.get(call_id) {
                    push_pair(&mut content, done);
                }
                // Unexecuted (cap hit): stripped, never dangling.
            } else {
                if is_tool_use {
                    saw_passthrough_tool_use = true;
                }
                content.push(block.clone());
            }
        }
    }

    let stop_reason = if saw_passthrough_tool_use {
        "tool_use".to_string()
    } else {
        "end_turn".to_string()
    };
    (content, stop_reason)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn search_response() -> Value {
        json!({
            "id": "msg_1",
            "content": [
                {"type": "text", "text": "Let me search."},
                {"type": "tool_use", "id": "toolu_1", "name": "web_search",
                 "input": {"query": "rust async"}},
                {"type": "tool_use", "id": "toolu_9", "name": "get_time", "input": {}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })
    }

    #[test]
    fn map_should_preserve_tool_use_stop_when_other_tools_called() {
        let response = search_response();
        let executed = vec![ExecutedSearch {
            call_id: "toolu_1".to_string(),
            query: "rust async".to_string(),
            server_id: server_block_id(0, 0),
            result: Ok(vec![SearchResult {
                title: "t".to_string(),
                url: "https://example.com".to_string(),
                description: "d".to_string(),
            }]),
        }];

        let (content, stop) = build_final_turn(&response, &executed);

        assert_eq!(stop, "tool_use");
        // Text kept, search call mapped to a server pair, other tool kept.
        assert!(content.iter().any(|b| b["type"] == json!("text")));
        assert!(content
            .iter()
            .any(|b| b["type"] == json!("server_tool_use")));
        assert!(content
            .iter()
            .any(|b| b["type"] == json!("web_search_tool_result")));
        assert!(
            content
                .iter()
                .any(|b| b["type"] == json!("tool_use") && b["name"] == json!("get_time")),
            "non-search tool_use must pass through: {content:?}"
        );
        assert!(
            !content
                .iter()
                .any(|b| b["name"] == json!(SERVER_TOOL_NAME) && b["type"] == json!("tool_use")),
            "no function-shape web_search may reach the client"
        );
    }

    #[test]
    fn final_turn_should_strip_unexecuted_calls_at_cap() {
        let (content, stop) = build_final_turn(&search_response(), &[]);

        assert_eq!(stop, "tool_use"); // get_time still pending
        assert!(
            !content.iter().any(|b| b["name"] == json!(SERVER_TOOL_NAME)),
            "unexecuted search calls must be stripped: {content:?}"
        );
    }

    #[test]
    fn usage_should_sum_iterations_when_loop_runs() {
        let usage = aggregate_usage(&[(10, 5), (20, 7), (0, 0)], 4);

        assert_eq!(usage["input_tokens"], json!(30));
        assert_eq!(usage["output_tokens"], json!(12));
        assert_eq!(usage["server_tool_use"]["web_search_requests"], json!(4));
    }

    #[test]
    fn redispatch_should_send_function_history_when_server_blocks_present() {
        let messages = json!([
            {"role": "user", "content": "search?"},
            {"role": "assistant", "content": [
                {"type": "server_tool_use", "id": "srvtoolu_1",
                 "name": "web_search", "input": {"query": "q"}},
                {"type": "text", "text": "done"}
            ]},
            {"role": "user", "content": [
                {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1",
                 "content": [
                     {"type": "web_search_result", "title": "t",
                      "url": "https://example.com", "text": "snippet"}
                 ]}
            ]}
        ]);

        let converted = convert_history_for_upstream(&messages);

        let assistant_blocks = converted[1]["content"].as_array().unwrap();
        assert!(
            assistant_blocks
                .iter()
                .any(|b| b["type"] == json!("tool_use") && b["id"] == json!("srvtoolu_1")),
            "server_tool_use must redispatch as function tool_use: {converted}"
        );
        let user_blocks = converted[2]["content"].as_array().unwrap();
        let result = user_blocks
            .iter()
            .find(|b| b["type"] == json!("tool_result"))
            .unwrap();
        assert_eq!(result["tool_use_id"], json!("srvtoolu_1"));
        assert!(
            result["content"]
                .as_str()
                .unwrap()
                .contains("https://example.com"),
            "result text must survive the round-trip: {converted}"
        );
        assert!(
            !converted.to_string().contains("server_tool_use"),
            "no server shape may leak upstream: {converted}"
        );
    }

    #[test]
    fn truncate_should_mark_cut_snippets_at_char_boundary() {
        assert_eq!(truncate_with_marker("short", 10), "short");
        let long = "abcdefghij";
        let cut = truncate_with_marker(long, 4);
        assert_eq!(cut, "abcd…(truncated)");
    }

    #[test]
    fn server_block_id_should_be_deterministic_per_iteration() {
        assert_eq!(server_block_id(0, 0), server_block_id(0, 0));
        assert_ne!(server_block_id(0, 0), server_block_id(1, 0));
        assert!(server_block_id(0, 0).starts_with("srvtoolu_"));
    }
}
