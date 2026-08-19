//! Compact-boundary detection and compaction planning.
//!
//! A "boundary" or "summary" marker is a flag consolette writes into a
//! turn's rows (under the `consoletteCompact` key, preserved via
//! [`crate::claude_code_session::transcript::RowFields::extra`]) once that
//! turn has already been summarized by a previous compaction run. Detecting
//! these markers lets [`create_plan`] make recompaction idempotent: turns
//! already folded into a prior summary are never re-summarized.

use crate::claude_code_session::transcript::{RowFields, TranscriptRow, Turn};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

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

/// `true` when `rows` contains at least one boundary or summary marker —
/// i.e. this transcript has already been through
/// [`crate::claude_code_session::compact_session`] at least once.
#[must_use]
pub fn is_compacted(rows: &[TranscriptRow]) -> bool {
    rows.iter().any(is_boundary_or_summary_row)
}

/// Token/cost accounting for one `compact_session` run, stamped onto every
/// summary-turn row that run writes (see
/// `crate::claude_code_session::mod::compaction_metrics_for`). One value is
/// computed per run, not per summary group — a run that folds multiple
/// summary groups shares the same aggregate figures across all of them.
///
/// `estimated_cost_usd` is always an estimate derived from
/// [`crate::cost_metrics::estimator::TiktokenEstimator`] +
/// [`crate::cost_metrics::pricing::PricingTable`], never a live-CLI-measured
/// dollar figure — `None` when the pricing model used has no entry in the
/// pricing table. `real_cost_usd`, when present, is the actual
/// `total_cost_usd` reported by the `claude` CLI for the summarization call
/// that produced this run's summaries.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CompactionMetrics {
    /// Estimated tokens across all `turns_to_summarize` content fed to the
    /// summarizer.
    pub tokens_before: u64,
    /// Estimated tokens across the summary text(s) the summarizer produced.
    pub tokens_after: u64,
    /// `tokens_before - tokens_after`. Signed because a pathological
    /// summary longer than its source is possible in principle.
    pub tokens_saved: i64,
    /// Estimated USD cost of the summarization call itself (input =
    /// `tokens_before`, output = `tokens_after`, priced via the pricing
    /// model used for the run). `None` when that model has no pricing
    /// table entry.
    pub estimated_cost_usd: Option<f64>,
    /// Real, live-measured USD cost of the summarization call, as reported
    /// by `claude -p --output-format json`'s `total_cost_usd` field. `None`
    /// when the summarizer that produced this run's summaries can't report
    /// a real cost (e.g. `FakeSummarizer`, or a transcript compacted before
    /// this field existed).
    #[serde(default)]
    pub real_cost_usd: Option<f64>,
}

/// Extract every [`CompactionMetrics`] stamped into `rows`' summary-row
/// markers, in row order. A transcript compacted more than once (i.e.
/// recompacted after new turns accrued) can carry more than one distinct
/// value; a transcript compacted exactly once but whose run produced N
/// summary groups yields N *duplicate* entries (documented run-level
/// aggregate, see [`CompactionMetrics`]'s doc comment) — callers that want
/// one figure per run should dedupe.
#[must_use]
pub fn extract_compaction_metrics(rows: &[TranscriptRow]) -> Vec<CompactionMetrics> {
    rows.iter()
        .filter_map(|row| {
            let marker = row.fields().extra.get("consoletteCompact")?.as_object()?;
            if !marker
                .get("summary")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return None;
            }
            let metrics = marker.get("metrics")?;
            serde_json::from_value(metrics.clone()).ok()
        })
        .collect()
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

/// Build a compact-boundary row for the destination transcript.
///
/// Matches Claude Code's own native `compact_boundary` shape
/// (`type: "system"`, `subtype: "compact_boundary"`, `parentUuid: null`,
/// `logicalParentUuid` pointing at the last pre-boundary row) — see
/// `project_plans/compaction-hook/decisions/ADR-011-compact-boundary-row-format.md`'s
/// Empirical Verification section for the real-transcript evidence this is
/// based on. `compactMetadata`/`level` are deliberately not reproduced —
/// they describe Claude Code's own auto-compaction internals, which this
/// compactor has no equivalent for.
///
/// A `consoletteCompact` marker is embedded alongside the native fields so
/// [`is_boundary_or_summary_row`] recognizes this row on a later,
/// idempotent recompaction pass, without needing to special-case `type`/
/// `subtype`.
#[must_use]
pub fn build_boundary_row(
    source_session_id: &str,
    pruned_count: usize,
    logical_parent_uuid: Option<&str>,
    timestamp: &str,
) -> TranscriptRow {
    let mut extra = Map::new();
    extra.insert("type".to_string(), json!("system"));
    extra.insert("subtype".to_string(), json!("compact_boundary"));
    extra.insert("content".to_string(), json!("Conversation compacted"));
    extra.insert("timestamp".to_string(), json!(timestamp));
    if let Some(parent) = logical_parent_uuid {
        extra.insert("logicalParentUuid".to_string(), json!(parent));
    }
    extra.insert(
        "consoletteCompact".to_string(),
        json!({
            "boundary": true,
            "sourceSessionId": source_session_id,
            "prunedCount": pruned_count,
        }),
    );

    TranscriptRow::System(RowFields {
        uuid: uuid::Uuid::new_v4().to_string(),
        parent_uuid: None,
        is_sidechain: false,
        is_meta: false,
        message: None,
        extra,
    })
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

    #[test]
    fn build_boundary_row_should_carry_native_shape_and_consolette_marker() {
        let row = build_boundary_row(
            "source-session",
            3,
            Some("last-pre-boundary-uuid"),
            "2026-08-14T00:00:00Z",
        );

        assert!(matches!(row, TranscriptRow::System(_)));
        assert_eq!(row.parent_uuid(), None);
        let extra = &row.fields().extra;
        assert_eq!(extra.get("type").and_then(Value::as_str), Some("system"));
        assert_eq!(
            extra.get("subtype").and_then(Value::as_str),
            Some("compact_boundary")
        );
        assert_eq!(
            extra.get("logicalParentUuid").and_then(Value::as_str),
            Some("last-pre-boundary-uuid")
        );
        assert!(is_boundary_or_summary_row(&row));
    }

    fn marked_assistant_row_with_metrics(
        uuid: &str,
        parent: Option<&str>,
        text: &str,
        metrics: &CompactionMetrics,
    ) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"{uuid}","parentUuid":{},"isSidechain":false,"isMeta":false,"consoletteCompact":{{"summary":true,"metrics":{}}},"message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}}}}"#,
            parent_uuid_json(parent),
            serde_json::to_string(metrics).unwrap(),
        )
    }

    #[test]
    fn is_compacted_should_return_false_for_transcript_with_no_markers() {
        let turns = [make_turn("u1", "a1", false), make_turn("u2", "a2", false)];
        let rows: Vec<TranscriptRow> = turns
            .iter()
            .flat_map(|t| {
                std::iter::once(t.user_row.clone()).chain(t.assistant_rows.iter().cloned())
            })
            .collect();

        assert!(!is_compacted(&rows));
    }

    #[test]
    fn is_compacted_should_return_true_when_any_row_carries_boundary_or_summary_marker() {
        let turns = [make_turn("u1", "a1", true), make_turn("u2", "a2", false)];
        let rows: Vec<TranscriptRow> = turns
            .iter()
            .flat_map(|t| {
                std::iter::once(t.user_row.clone()).chain(t.assistant_rows.iter().cloned())
            })
            .collect();

        assert!(is_compacted(&rows));
    }

    #[test]
    fn extract_compaction_metrics_should_return_empty_when_no_summary_row_carries_metrics() {
        let turns = [make_turn("u1", "a1", true), make_turn("u2", "a2", false)];
        let rows: Vec<TranscriptRow> = turns
            .iter()
            .flat_map(|t| {
                std::iter::once(t.user_row.clone()).chain(t.assistant_rows.iter().cloned())
            })
            .collect();

        assert!(extract_compaction_metrics(&rows).is_empty());
    }

    #[test]
    fn extract_compaction_metrics_should_deserialize_metrics_stamped_on_summary_rows() {
        let metrics = CompactionMetrics {
            tokens_before: 500,
            tokens_after: 50,
            tokens_saved: 450,
            estimated_cost_usd: Some(0.001_23),
            real_cost_usd: None,
        };
        let user_line = user_row("u1", None, "[consolette: compacted turns summary]");
        let assistant_line =
            marked_assistant_row_with_metrics("a1", Some("u1"), "summary text", &metrics);

        let rows: Vec<TranscriptRow> = vec![
            serde_json::from_str(&user_line).unwrap(),
            serde_json::from_str(&assistant_line).unwrap(),
        ];

        let extracted = extract_compaction_metrics(&rows);
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0], metrics);
    }

    #[test]
    fn extract_compaction_metrics_should_default_real_cost_to_none_for_pre_field_transcripts() {
        let user_line = user_row("u1", None, "[consolette: compacted turns summary]");
        let assistant_line = r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","isSidechain":false,"isMeta":false,"consoletteCompact":{"summary":true,"metrics":{"tokens_before":500,"tokens_after":50,"tokens_saved":450,"estimated_cost_usd":0.00123}},"message":{"role":"assistant","content":[{"type":"text","text":"summary text"}]}}"#
            .to_string();

        let rows: Vec<TranscriptRow> = vec![
            serde_json::from_str(&user_line).unwrap(),
            serde_json::from_str(&assistant_line).unwrap(),
        ];

        let extracted = extract_compaction_metrics(&rows);
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].real_cost_usd, None);
    }
}
