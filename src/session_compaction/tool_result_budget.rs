//! `ToolResultBudget`: age-rank `tool_result` content blocks across the full
//! `messages[]` array and elide all but the N most recent.
//!
//! Operates on the request body's `messages[]` directly rather than any
//! separately persisted history — the Anthropic Messages API is stateless
//! per request and the client resends the full conversation every turn, so
//! "age" here just means "earlier position in the array already in hand."
//! Elision is a one-way, lossy transform in this first pass (unlike
//! [`crate::compression::rewind`]'s hash-recoverable markers); wiring elided
//! content through `RewindStore` so it stays retrievable is tracked as a
//! follow-up, not implemented here.

use serde_json::{json, Value};

/// Per-call elision statistics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolResultBudgetStats {
    pub tool_result_count: usize,
    pub elided_count: usize,
}

/// Elide all but the `keep_recent` most recent `tool_result` content blocks
/// in `messages`, oldest-first. A `tool_result` counts as "recent" by its
/// position among all `tool_result` blocks in the array, not by message
/// index (a single message can hold multiple `tool_result` blocks, e.g. from
/// parallel tool calls).
///
/// Returns the (possibly modified) messages array and stats. If
/// `keep_recent` is greater than or equal to the total `tool_result` count,
/// `messages` is returned unchanged.
#[must_use]
pub fn budget_tool_results(messages: &Value, keep_recent: usize) -> (Value, ToolResultBudgetStats) {
    let Some(arr) = messages.as_array() else {
        return (messages.clone(), ToolResultBudgetStats::default());
    };

    let total = count_tool_results(arr);
    if total <= keep_recent {
        return (
            messages.clone(),
            ToolResultBudgetStats {
                tool_result_count: total,
                elided_count: 0,
            },
        );
    }

    let elide_up_to = total - keep_recent;
    let mut seen = 0usize;
    let mut elided_count = 0usize;

    let mut out = messages.clone();
    if let Some(out_arr) = out.as_array_mut() {
        for msg in out_arr.iter_mut() {
            let Some(content) = msg.get_mut("content").and_then(Value::as_array_mut) else {
                continue;
            };
            for block in content.iter_mut() {
                if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
                if seen < elide_up_to {
                    elide_tool_result_content(block);
                    elided_count += 1;
                }
                seen += 1;
            }
        }
    }

    (
        out,
        ToolResultBudgetStats {
            tool_result_count: total,
            elided_count,
        },
    )
}

fn count_tool_results(messages: &[Value]) -> usize {
    messages
        .iter()
        .filter_map(|m| m.get("content").and_then(Value::as_array))
        .flat_map(|blocks: &Vec<Value>| blocks.iter())
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        .count()
}

/// Replace a `tool_result` block's `content` with a short marker noting how
/// many bytes were elided, preserving `tool_use_id` so tool-pair validation
/// (`crate::compression::engine`'s guard) still sees a matching pair.
fn elide_tool_result_content(block: &mut Value) {
    let original_len = block
        .get("content")
        .map_or(0, |c| serde_json::to_string(c).unwrap_or_default().len());
    block["content"] = json!(format!(
        "[tool result elided by ToolResultBudget — {original_len} bytes omitted]"
    ));
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tool_result_msg(tool_use_id: &str, content: &str) -> Value {
        json!({
            "role": "user",
            "content": [
                {"type": "tool_result", "tool_use_id": tool_use_id, "content": content}
            ]
        })
    }

    #[test]
    fn elides_all_but_n_most_recent() {
        let messages = json!([
            tool_result_msg("t1", "result one"),
            tool_result_msg("t2", "result two"),
            tool_result_msg("t3", "result three"),
        ]);
        let (out, stats) = budget_tool_results(&messages, 1);
        assert_eq!(stats.tool_result_count, 3);
        assert_eq!(stats.elided_count, 2);
        assert!(out[0]["content"][0]["content"]
            .as_str()
            .unwrap()
            .contains("elided"));
        assert!(out[1]["content"][0]["content"]
            .as_str()
            .unwrap()
            .contains("elided"));
        assert_eq!(out[2]["content"][0]["content"], "result three");
    }

    #[test]
    fn keeps_tool_use_id_after_eliding() {
        let messages = json!([
            tool_result_msg("t1", "result one"),
            tool_result_msg("t2", "result two"),
        ]);
        let (out, _stats) = budget_tool_results(&messages, 1);
        assert_eq!(out[0]["content"][0]["tool_use_id"], "t1");
    }

    #[test]
    fn no_op_when_within_budget() {
        let messages = json!([
            tool_result_msg("t1", "result one"),
            tool_result_msg("t2", "result two"),
        ]);
        let (out, stats) = budget_tool_results(&messages, 5);
        assert_eq!(stats.elided_count, 0);
        assert_eq!(out, messages);
    }

    #[test]
    fn counts_multiple_tool_results_in_one_message() {
        let messages = json!([{
            "role": "user",
            "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "a"},
                {"type": "tool_result", "tool_use_id": "t2", "content": "b"},
                {"type": "tool_result", "tool_use_id": "t3", "content": "c"},
            ]
        }]);
        let (out, stats) = budget_tool_results(&messages, 1);
        assert_eq!(stats.tool_result_count, 3);
        assert_eq!(stats.elided_count, 2);
        assert!(out[0]["content"][0]["content"]
            .as_str()
            .unwrap()
            .contains("elided"));
        assert!(out[0]["content"][1]["content"]
            .as_str()
            .unwrap()
            .contains("elided"));
        assert_eq!(out[0]["content"][2]["content"], "c");
    }

    #[test]
    fn ignores_non_tool_result_blocks() {
        let messages = json!([
            {"role": "assistant", "content": [{"type": "text", "text": "thinking..."}]},
            tool_result_msg("t1", "result one"),
        ]);
        let (out, stats) = budget_tool_results(&messages, 0);
        assert_eq!(stats.tool_result_count, 1);
        assert_eq!(out[0]["content"][0]["text"], "thinking...");
    }

    #[test]
    fn no_op_on_non_array_messages() {
        let messages = json!("not an array");
        let (out, stats) = budget_tool_results(&messages, 0);
        assert_eq!(out, messages);
        assert_eq!(stats, ToolResultBudgetStats::default());
    }
}
