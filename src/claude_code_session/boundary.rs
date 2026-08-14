//! Compact-boundary detection and compaction planning.
//!
//! A "boundary" or "summary" marker is a flag consolette writes into a
//! turn's rows (under the `consoletteCompact` key, preserved via
//! [`crate::claude_code_session::transcript::RowFields::extra`]) once that
//! turn has already been summarized by a previous compaction run. Detecting
//! these markers lets [`create_plan`] make recompaction idempotent: turns
//! already folded into a prior summary are never re-summarized.

use crate::claude_code_session::transcript::{TranscriptRow, Turn};
use serde_json::Value;

/// The result of planning a compaction: which turns are already-summarized
/// prefix, which turns should be summarized now, and which most-recent
/// turns must be preserved verbatim.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionPlan {
    /// Turns up to and including the most recent already-marked
    /// (boundary/summary) turn. Never re-summarized.
    pub prefix_turns: Vec<Turn>,
    /// Turns that should be summarized by this compaction run.
    pub turns_to_summarize: Vec<Turn>,
    /// The most recent `preserve_last_n_turns` turns, kept verbatim and
    /// never summarized.
    pub preserved_turns: Vec<Turn>,
}

/// `true` when `row`'s `extra["consoletteCompact"]` marks it as a
/// compaction boundary or an already-produced summary.
fn is_boundary_or_summary_row(row: &TranscriptRow) -> bool {
    row.fields()
        .extra
        .get("consoletteCompact")
        .and_then(Value::as_object)
        .is_some_and(|obj| {
            obj.get("boundary")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || obj.get("summary").and_then(Value::as_bool).unwrap_or(false)
        })
}

/// `true` when any row in `turn` carries a boundary/summary marker.
fn is_turn_marked(turn: &Turn) -> bool {
    is_boundary_or_summary_row(&turn.user_row)
        || turn.assistant_rows.iter().any(is_boundary_or_summary_row)
        || turn.tool_rows.iter().any(is_boundary_or_summary_row)
}

/// Build a [`CompactionPlan`] from reconstructed turns.
///
/// `preserve_last_n_turns` (default `0`) keeps the most recent N turns
/// (after any already-marked prefix) out of `turns_to_summarize`
/// regardless of marker state, so a caller can guarantee the tail of a
/// conversation is never folded into a summary.
///
/// Marker semantics: **every** turn up to and including the last marked
/// turn becomes `prefix_turns`, not just the individually marked ones —
/// a marker on turn N means turns 1..=N were already captured by a prior
/// summary, even if only the boundary row itself is marked.
#[must_use]
pub fn create_plan(turns: Vec<Turn>, preserve_last_n_turns: usize) -> CompactionPlan {
    let marker_end = turns.iter().rposition(is_turn_marked);

    let (prefix_turns, remaining) = match marker_end {
        Some(idx) => {
            let mut turns = turns;
            let remaining = turns.split_off(idx + 1);
            (turns, remaining)
        }
        None => (Vec::new(), turns),
    };

    let preserve_n = preserve_last_n_turns.min(remaining.len());
    let split_at = remaining.len() - preserve_n;
    let mut remaining = remaining;
    let preserved_turns = remaining.split_off(split_at);

    CompactionPlan {
        prefix_turns,
        turns_to_summarize: remaining,
        preserved_turns,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::claude_code_session::transcript::{build_turns, parse_session_file};
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_temp_jsonl(lines: &[String]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        f
    }

    fn parent_uuid_json(parent: Option<&str>) -> String {
        parent.map_or_else(|| "null".to_string(), |p| format!("\"{p}\""))
    }

    fn user_row(uuid: &str, parent: Option<&str>, text: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"{uuid}","parentUuid":{},"isSidechain":false,"isMeta":false,"message":{{"role":"user","content":"{text}"}}}}"#,
            parent_uuid_json(parent),
        )
    }

    fn assistant_row(uuid: &str, parent: Option<&str>, text: &str) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"{uuid}","parentUuid":{},"isSidechain":false,"isMeta":false,"message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}}}}"#,
            parent_uuid_json(parent),
        )
    }

    fn marked_assistant_row(uuid: &str, parent: Option<&str>, text: &str) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"{uuid}","parentUuid":{},"isSidechain":false,"isMeta":false,"consoletteCompact":{{"summary":true}},"message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}}}}"#,
            parent_uuid_json(parent),
        )
    }

    fn make_turn(user_uuid: &str, assistant_uuid: &str, marked: bool) -> Turn {
        let user_line = user_row(user_uuid, None, "hi");
        let assistant_line = if marked {
            marked_assistant_row(assistant_uuid, Some(user_uuid), "response")
        } else {
            assistant_row(assistant_uuid, Some(user_uuid), "response")
        };
        Turn {
            user_row: serde_json::from_str(&user_line).unwrap(),
            assistant_rows: vec![serde_json::from_str(&assistant_line).unwrap()],
            tool_rows: Vec::new(),
        }
    }

    #[test]
    fn create_plan_should_classify_marked_turn_as_prefix_when_summary_flag_is_true() {
        let turns = vec![make_turn("u1", "a1", true), make_turn("u2", "a2", false)];

        let plan = create_plan(turns, 0);

        assert_eq!(plan.prefix_turns.len(), 1);
        assert_eq!(plan.prefix_turns[0].user_row.uuid(), "u1");
        assert_eq!(plan.turns_to_summarize.len(), 1);
        assert_eq!(plan.turns_to_summarize[0].user_row.uuid(), "u2");
        assert!(plan.preserved_turns.is_empty());
    }

    #[test]
    fn create_plan_should_return_empty_turns_to_summarize_when_preserve_last_n_covers_all_turns() {
        let turns = vec![make_turn("u1", "a1", false), make_turn("u2", "a2", false)];

        let plan = create_plan(turns, 5);

        assert!(plan.prefix_turns.is_empty());
        assert!(plan.turns_to_summarize.is_empty());
        assert_eq!(plan.preserved_turns.len(), 2);
    }

    #[test]
    fn create_plan_should_exclude_prefix_turns_from_turns_to_summarize_when_recompacting_already_compacted_transcript(
    ) {
        // A 6-turn fixture, parsed end-to-end via parse_session_file +
        // build_turns, where turn 3's assistant row carries a summary
        // marker from a prior compaction run. Turns 1-3 must land in
        // prefix_turns (not just turn 3), and turns 4-6 must be candidates
        // for (re-)summarization.
        let mut lines = Vec::new();
        let mut prev: Option<String> = None;
        for i in 1..=6 {
            let u = format!("u{i}");
            let a = format!("a{i}");
            lines.push(user_row(&u, prev.as_deref(), "hi"));
            let assistant_line = if i == 3 {
                marked_assistant_row(&a, Some(&u), "summary of turns 1-3")
            } else {
                assistant_row(&a, Some(&u), "response")
            };
            lines.push(assistant_line);
            prev = Some(a);
        }

        let f = write_temp_jsonl(&lines);
        let rows = parse_session_file(f.path()).unwrap();
        let turns = build_turns(&rows).unwrap();
        assert_eq!(turns.len(), 6);

        let plan = create_plan(turns, 0);

        assert_eq!(plan.prefix_turns.len(), 3);
        let prefix_uuids: Vec<&str> = plan
            .prefix_turns
            .iter()
            .map(|t| t.user_row.uuid())
            .collect();
        assert_eq!(prefix_uuids, vec!["u1", "u2", "u3"]);

        assert_eq!(plan.turns_to_summarize.len(), 3);
        let summarize_uuids: Vec<&str> = plan
            .turns_to_summarize
            .iter()
            .map(|t| t.user_row.uuid())
            .collect();
        assert_eq!(summarize_uuids, vec!["u4", "u5", "u6"]);

        assert!(plan.preserved_turns.is_empty());
    }
}
