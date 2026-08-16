//! `ConversationSummarizer`: deterministic, LLM-free collapse of turns older
//! than a keep-window into a single boundary marker message.
//!
//! The issue is explicit that this must not call an LLM — unlike
//! `compaction-hook`'s `ClaudeCliSummarizer` ([`crate::claude_code_session`]),
//! which can afford a subprocess round trip because it runs out-of-band, this
//! runs inline on every request once `TieredCompaction` selects `Auto`/`Full`.
//! So "summarize" here means structural reduction — `{role, first N chars of
//! first text block, tool names called, tool_result count}` per turn — not
//! prose generation, following `structural_collapse.rs`'s "keep first/last,
//! collapse the run" template even though the input shape (parsed message
//! turns) differs from its (repeated text lines).
//!
//! **Wire format decision** (open question in the design doc): dropped turns
//! are replaced with one synthetic `text` block naming how many turns were
//! collapsed and what they contained, rather than silently removed — mirrors
//! the spirit of `compaction-hook`'s ADR-011 `compact_boundary` row (an
//! explicit marker beats silent loss) but is a distinct in-band content
//! block, not a JSONL transcript row, since the data model here is a live
//! Messages API request rather than an on-disk transcript.

use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SummarizerStats {
    pub turns_summarized: usize,
}

/// One turn's structural digest, used to build the boundary marker text.
struct TurnDigest {
    role: String,
    text_excerpt: Option<String>,
    tool_names: Vec<String>,
    tool_result_count: usize,
}

const EXCERPT_CHARS: usize = 80;

fn digest_turn(message: &Value) -> TurnDigest {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();

    let mut text_excerpt = None;
    let mut tool_names = Vec::new();
    let mut tool_result_count = 0usize;

    if let Some(blocks) = message.get("content").and_then(Value::as_array) {
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") if text_excerpt.is_none() => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        let truncated: String = text.chars().take(EXCERPT_CHARS).collect();
                        text_excerpt = Some(truncated);
                    }
                }
                Some("tool_use") => {
                    if let Some(name) = block.get("name").and_then(Value::as_str) {
                        tool_names.push(name.to_string());
                    }
                }
                Some("tool_result") => tool_result_count += 1,
                _ => {}
            }
        }
    } else if let Some(text) = message.get("content").and_then(Value::as_str) {
        let truncated: String = text.chars().take(EXCERPT_CHARS).collect();
        text_excerpt = Some(truncated);
    }

    TurnDigest {
        role,
        text_excerpt,
        tool_names,
        tool_result_count,
    }
}

fn format_boundary_marker(digests: &[TurnDigest]) -> String {
    use std::fmt::Write as _;

    let mut out = format!(
        "[{} earlier turn(s) summarized by ConversationSummarizer]",
        digests.len()
    );
    for digest in digests {
        let _ = write!(out, "\n- {}", digest.role);
        if let Some(excerpt) = &digest.text_excerpt {
            let _ = write!(out, ": \"{excerpt}\"");
        }
        if !digest.tool_names.is_empty() {
            let _ = write!(out, " [tools: {}]", digest.tool_names.join(", "));
        }
        if digest.tool_result_count > 0 {
            let _ = write!(out, " [{} tool result(s)]", digest.tool_result_count);
        }
    }
    out
}

/// Summarize all but the `keep_recent` most recent messages into a single
/// synthetic `user`-role boundary message prepended before the kept slice.
///
/// A no-op (returns `messages` unchanged) if there are `keep_recent` or fewer
/// messages. The synthetic marker always takes `user` role: Anthropic's
/// strict user/assistant alternation means the correct role to splice in
/// depends on what follows it, which this pure function doesn't decide —
/// getting that right is a router-integration concern, tracked as a
/// follow-up rather than solved speculatively here.
#[must_use]
pub fn summarize_older_turns(messages: &Value, keep_recent: usize) -> (Value, SummarizerStats) {
    let Some(arr) = messages.as_array() else {
        return (messages.clone(), SummarizerStats::default());
    };

    if arr.len() <= keep_recent {
        return (messages.clone(), SummarizerStats::default());
    }

    let split_at = arr.len() - keep_recent;
    let (older, recent) = arr.split_at(split_at);
    let digests: Vec<TurnDigest> = older.iter().map(digest_turn).collect();
    let marker = format_boundary_marker(&digests);

    let mut out = Vec::with_capacity(recent.len() + 1);
    out.push(json!({
        "role": "user",
        "content": [{"type": "text", "text": marker}]
    }));
    out.extend(recent.iter().cloned());

    (
        Value::Array(out),
        SummarizerStats {
            turns_summarized: older.len(),
        },
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn text_msg(role: &str, text: &str) -> Value {
        json!({"role": role, "content": [{"type": "text", "text": text}]})
    }

    #[test]
    fn no_op_when_within_keep_window() {
        let messages = json!([text_msg("user", "hi"), text_msg("assistant", "hello")]);
        let (out, stats) = summarize_older_turns(&messages, 5);
        assert_eq!(stats.turns_summarized, 0);
        assert_eq!(out, messages);
    }

    #[test]
    fn collapses_older_turns_into_one_boundary_message() {
        let messages = json!([
            text_msg("user", "turn one"),
            text_msg("assistant", "turn two"),
            text_msg("user", "turn three"),
            text_msg("assistant", "turn four"),
        ]);
        let (out, stats) = summarize_older_turns(&messages, 1);
        assert_eq!(stats.turns_summarized, 3);
        let out_arr = out.as_array().unwrap();
        assert_eq!(out_arr.len(), 2);
        assert_eq!(out_arr[0]["role"], "user");
        let marker_text = out_arr[0]["content"][0]["text"].as_str().unwrap();
        assert!(marker_text.contains("3 earlier turn(s) summarized"));
        assert!(marker_text.contains("turn one"));
        assert!(marker_text.contains("turn two"));
        assert!(marker_text.contains("turn three"));
        assert_eq!(out_arr[1], text_msg("assistant", "turn four"));
    }

    #[test]
    fn marker_includes_tool_names_and_result_counts() {
        let messages = json!([
            {
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "t1", "name": "search_files", "input": {}}]
            },
            {
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "found"}]
            },
            text_msg("assistant", "kept"),
        ]);
        let (out, stats) = summarize_older_turns(&messages, 1);
        assert_eq!(stats.turns_summarized, 2);
        let marker_text = out[0]["content"][0]["text"].as_str().unwrap();
        assert!(marker_text.contains("search_files"));
        assert!(marker_text.contains("1 tool result(s)"));
    }

    #[test]
    fn no_op_on_non_array_messages() {
        let messages = json!("not an array");
        let (out, stats) = summarize_older_turns(&messages, 0);
        assert_eq!(out, messages);
        assert_eq!(stats, SummarizerStats::default());
    }

    #[test]
    fn truncates_long_text_excerpts() {
        let long_text = "x".repeat(200);
        let messages = json!([text_msg("user", &long_text), text_msg("assistant", "kept")]);
        let (out, _stats) = summarize_older_turns(&messages, 1);
        let marker_text = out[0]["content"][0]["text"].as_str().unwrap();
        assert!(marker_text.contains(&"x".repeat(EXCERPT_CHARS)));
        assert!(!marker_text.contains(&"x".repeat(EXCERPT_CHARS + 1)));
    }
}
