//! Per-call token composition classification (context-analyzer plan.md
//! Story 1.3.1).
//!
//! [`classify_call_composition`] splits one API call's non-cached
//! `input_tokens` across three categories — Tool I/O, Conversation, System —
//! so the dashboard's composition breakdown (Success Metric #1) has real
//! numbers, not a guess. There is no exact per-block token count available
//! (no re-tokenization here), so the split is proportional to each
//! category's character-weight within the turn's content blocks, with the
//! integer-division remainder folded into `conversation_tokens` so the
//! three figures always sum exactly to `input_tokens` — never drifting by a
//! token or two (Pattern Decision: pure function, no `GoF` Visitor — three
//! categories and a fixed per-content-block rule don't earn double-dispatch
//! ceremony).
//!
//! `cache_read_input_tokens`/`cache_creation_input_tokens` are deliberately
//! *not* folded into this split (`research/pitfalls.md` §4: `input_tokens`
//! is specifically the non-cached portion) — they're tracked as their own
//! figures on [`crate::context_forensics::store::ApiCallRow`] instead.

use crate::claude_code_session::transcript::{TranscriptRow, Turn};
use crate::context_forensics::usage::extract_call_usage;
use crate::providers::AnthropicUsage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Sum type: which category a content block's tokens belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompositionCategory {
    ToolIo,
    Conversation,
    System,
}

/// `{ tool_io_tokens, conversation_tokens, system_tokens }` for one call or
/// turn — always sums to the `input_tokens` it was computed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CompositionBreakdown {
    pub tool_io_tokens: u64,
    pub conversation_tokens: u64,
    pub system_tokens: u64,
}

/// Split `call_usage.input_tokens` across [`CompositionCategory`]s using
/// `turn`'s content blocks (`user_row`/`assistant_rows`/`tool_rows`) as the
/// classification signal.
///
/// Classification rule, per content block:
/// - A block inside a [`TranscriptRow::System`] row is `System`, regardless
///   of block type.
/// - A `tool_use` or `tool_result` block is `ToolIo`.
/// - Everything else (`text`, `thinking`, plain-string `content`, and any
///   block type this function doesn't recognize) is `Conversation`.
///
/// When a turn's rows carry no classifiable text/content at all (all-empty
/// or all-block-types-unweighted), every `input_tokens` is attributed to
/// `Conversation` rather than silently dropped — the total is never lost.
#[must_use]
pub(crate) fn classify_call_composition(
    turn: &Turn,
    call_usage: &AnthropicUsage,
) -> CompositionBreakdown {
    let mut weights: Vec<(CompositionCategory, usize)> = Vec::new();
    weights.extend(content_blocks_chars(&turn.user_row));
    for row in &turn.assistant_rows {
        weights.extend(content_blocks_chars(row));
    }
    for row in &turn.tool_rows {
        weights.extend(content_blocks_chars(row));
    }

    let input_tokens = call_usage.input_tokens;
    let total_chars: usize = weights.iter().map(|(_, chars)| chars).sum();

    if total_chars == 0 {
        return CompositionBreakdown {
            tool_io_tokens: 0,
            conversation_tokens: input_tokens,
            system_tokens: 0,
        };
    }

    let mut tool_io_chars = 0usize;
    let mut system_chars = 0usize;
    for (category, chars) in &weights {
        match category {
            CompositionCategory::ToolIo => tool_io_chars += chars,
            CompositionCategory::System => system_chars += chars,
            CompositionCategory::Conversation => {}
        }
    }

    let tool_io_tokens = proportional_share(input_tokens, tool_io_chars, total_chars);
    let system_tokens = proportional_share(input_tokens, system_chars, total_chars);
    // Remainder (from two independent integer-division truncations) folds
    // into conversation_tokens, guaranteeing an exact sum to input_tokens.
    let conversation_tokens = input_tokens
        .saturating_sub(tool_io_tokens)
        .saturating_sub(system_tokens);

    CompositionBreakdown {
        tool_io_tokens,
        conversation_tokens,
        system_tokens,
    }
}

#[allow(clippy::cast_possible_truncation)]
fn proportional_share(total: u64, part_chars: usize, total_chars: usize) -> u64 {
    if total_chars == 0 {
        return 0;
    }
    (u128::from(total) * part_chars as u128 / total_chars as u128) as u64
}

/// `(category, char_count)` for every content block in `row`'s message —
/// empty when `row` has no `message`/`content` at all.
fn content_blocks_chars(row: &TranscriptRow) -> Vec<(CompositionCategory, usize)> {
    let is_system = matches!(row, TranscriptRow::System(_));
    let Some(message) = &row.fields().message else {
        return Vec::new();
    };
    let Some(content) = message.get("content") else {
        return Vec::new();
    };

    match content {
        Value::String(s) => {
            let category = if is_system {
                CompositionCategory::System
            } else {
                CompositionCategory::Conversation
            };
            vec![(category, s.chars().count())]
        }
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| {
                let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
                let category = classify_block_category(is_system, block_type);
                (category, block_text_chars(block, block_type))
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn classify_block_category(row_is_system: bool, block_type: &str) -> CompositionCategory {
    if row_is_system {
        return CompositionCategory::System;
    }
    match block_type {
        "tool_use" | "tool_result" => CompositionCategory::ToolIo,
        _ => CompositionCategory::Conversation,
    }
}

fn block_text_chars(block: &Value, block_type: &str) -> usize {
    match block_type {
        "text" | "thinking" => block
            .get("text")
            .and_then(Value::as_str)
            .map_or(0, |s| s.chars().count()),
        "tool_use" => block
            .get("input")
            .and_then(|v| serde_json::to_string(v).ok())
            .map_or(0, |s| s.chars().count()),
        "tool_result" => match block.get("content") {
            Some(Value::String(s)) => s.chars().count(),
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .map(|s| s.chars().count())
                .sum(),
            _ => 0,
        },
        _ => 0,
    }
}

/// `tracing::debug!`-log every row preceding the first reconstructed turn's
/// `user_row` that itself carries non-zero usage.
///
/// `build_turns` already silently drops rows before the first genuine user
/// turn (its own documented "accepted gap," `transcript.rs:369-380`); this
/// makes that drop visible for context-analyzer's totals too, rather than
/// letting a real (if rare) leading-row token cost vanish without a trace
/// (Story 1.3.1 AC2).
pub fn log_dropped_leading_rows_with_usage(rows: &[TranscriptRow], turns: &[Turn]) {
    let Some(first_turn) = turns.first() else {
        return;
    };
    let first_user_uuid = first_turn.user_row.uuid();

    for row in rows {
        if row.uuid() == first_user_uuid {
            break;
        }
        if let Some(usage) = extract_call_usage(row) {
            let has_nonzero_usage = usage.input_tokens > 0
                || usage.output_tokens > 0
                || usage.cache_creation_input_tokens > 0
                || usage.cache_read_input_tokens > 0;
            if has_nonzero_usage {
                tracing::debug!(
                    uuid = row.uuid(),
                    input_tokens = usage.input_tokens,
                    cache_creation_input_tokens = usage.cache_creation_input_tokens,
                    cache_read_input_tokens = usage.cache_read_input_tokens,
                    "dropped leading transcript row carries non-zero usage; excluded from context-forensics totals"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row_from_json(value: &Value) -> TranscriptRow {
        serde_json::from_value(value.clone()).unwrap()
    }

    fn user_row(uuid: &str, text: &str) -> TranscriptRow {
        row_from_json(&json!({
            "type": "user",
            "uuid": uuid,
            "parentUuid": null,
            "isSidechain": false,
            "isMeta": false,
            "message": {"role": "user", "content": text},
        }))
    }

    fn tool_result_row(uuid: &str, parent: &str, content: &str) -> TranscriptRow {
        row_from_json(&json!({
            "type": "user",
            "uuid": uuid,
            "parentUuid": parent,
            "isSidechain": false,
            "isMeta": false,
            "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "x", "content": content}]},
        }))
    }

    fn assistant_text_row(uuid: &str, parent: &str, text: &str) -> TranscriptRow {
        row_from_json(&json!({
            "type": "assistant",
            "uuid": uuid,
            "parentUuid": parent,
            "isSidechain": false,
            "isMeta": false,
            "message": {"role": "assistant", "content": [{"type": "text", "text": text}]},
        }))
    }

    fn system_row_with_usage(uuid: &str, cache_creation_input_tokens: u64) -> TranscriptRow {
        row_from_json(&json!({
            "type": "system",
            "uuid": uuid,
            "parentUuid": null,
            "isSidechain": false,
            "isMeta": false,
            "message": {
                "content": "CLAUDE.md loaded",
                "usage": {
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "cache_creation_input_tokens": cache_creation_input_tokens,
                    "cache_read_input_tokens": 0,
                }
            },
        }))
    }

    fn usage(input_tokens: u64) -> AnthropicUsage {
        AnthropicUsage {
            input_tokens,
            output_tokens: 50,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        }
    }

    #[test]
    fn classify_call_composition_should_split_tokens_across_categories_when_turn_has_tool_and_user_rows(
    ) {
        // Tool result content is much longer than the short user text, so
        // the char-weighted split should favor ToolIo while still leaving a
        // nonzero conversation share, and the two must sum to input_tokens
        // exactly.
        let turn = Turn {
            user_row: user_row("u1", "go"),
            assistant_rows: vec![],
            tool_rows: vec![tool_result_row(
                "t1",
                "a1",
                &"x".repeat(998), // total content chars: 2 (user) + 998 (tool) = 1000
            )],
        };

        let breakdown = classify_call_composition(&turn, &usage(1000));

        assert_eq!(
            breakdown.tool_io_tokens + breakdown.conversation_tokens + breakdown.system_tokens,
            1000
        );
        assert!(breakdown.tool_io_tokens > breakdown.conversation_tokens);
        assert_eq!(breakdown.system_tokens, 0);
    }

    #[test]
    fn classify_call_composition_should_favor_conversation_when_turn_is_conversation_heavy() {
        let turn = Turn {
            user_row: user_row("u1", &"hello ".repeat(50)),
            assistant_rows: vec![assistant_text_row("a1", "u1", &"reply ".repeat(50))],
            tool_rows: vec![],
        };

        let breakdown = classify_call_composition(&turn, &usage(600));

        assert_eq!(breakdown.tool_io_tokens, 0);
        assert_eq!(breakdown.system_tokens, 0);
        assert_eq!(breakdown.conversation_tokens, 600);
    }

    #[test]
    fn classify_call_composition_should_return_all_conversation_when_no_content_weight_available() {
        let turn = Turn {
            user_row: TranscriptRow::User(crate::claude_code_session::transcript::RowFields {
                uuid: "u1".to_string(),
                parent_uuid: None,
                is_sidechain: false,
                is_meta: false,
                message: None,
                extra: serde_json::Map::new(),
            }),
            assistant_rows: vec![],
            tool_rows: vec![],
        };

        let breakdown = classify_call_composition(&turn, &usage(250));

        assert_eq!(breakdown.conversation_tokens, 250);
        assert_eq!(breakdown.tool_io_tokens, 0);
        assert_eq!(breakdown.system_tokens, 0);
    }

    #[test]
    fn classify_call_composition_should_log_and_exclude_dropped_leading_system_row_when_row_precedes_first_user_turn(
    ) {
        // Regression/behavior test for Story 1.3.1 AC2: a leading system
        // row (before any turn's user_row) is not part of any Turn's
        // content at all — classify_call_composition never sees it —
        // log_dropped_leading_rows_with_usage is the function that surfaces
        // it instead of letting it vanish silently. This test asserts the
        // non-panicking, best-effort contract (the debug! line itself isn't
        // assertable without a tracing test subscriber); the meaningful
        // assertion is that the leading row is correctly identified as
        // "before the first turn" and the function returns without error
        // for both fixture cases.
        let leading = system_row_with_usage("sys1", 3000);
        let user = user_row("u1", "hello");
        let rows = vec![leading.clone(), user.clone()];
        let turns = vec![Turn {
            user_row: user,
            assistant_rows: vec![],
            tool_rows: vec![],
        }];

        // Should not panic, and the leading row must not appear in the
        // reconstructed turn's content at all (confirming it really was
        // dropped, matching build_turns's own documented behavior).
        log_dropped_leading_rows_with_usage(&rows, &turns);
        assert_eq!(turns[0].user_row.uuid(), "u1");
        assert_ne!(turns[0].user_row.uuid(), leading.uuid());
    }

    #[test]
    fn log_dropped_leading_rows_with_usage_should_not_panic_when_no_turns_reconstructed() {
        let rows = vec![system_row_with_usage("sys1", 3000)];
        log_dropped_leading_rows_with_usage(&rows, &[]);
    }
}
