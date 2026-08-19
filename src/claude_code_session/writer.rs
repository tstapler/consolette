//! Atomic destination-transcript writer for a compaction run.
//!
//! Assembles a rewritten session JSONL file from a [`CompactionPlan`] plus
//! the [`TurnSummary`]s produced for its `turns_to_summarize`, in write
//! order: boundary row → `prefix_turns` verbatim → one synthetic turn
//! (placeholder user row + assistant row) per `TurnSummary` →
//! `preserved_turns` verbatim. See
//! `project_plans/compaction-hook/implementation/plan.md` Epic 4.1.

use crate::claude_code_session::boundary::{build_boundary_row, CompactionMetrics, CompactionPlan};
use crate::claude_code_session::summarize::TurnSummary;
use crate::claude_code_session::transcript::{RowFields, TranscriptRow, Turn};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::ffi::OsStr;
use std::fs;
use std::path::Path;

/// Rows belonging to one turn, in write order: the user row, then
/// assistant rows, then tool-result rows — see this module's own doc
/// comment on the ordering simplification (mirrors
/// `boundary.rs::is_turn_marked`'s treatment of a `Turn`'s three row
/// groups as an unordered-between-groups set).
fn turn_rows(turn: &Turn) -> Vec<&TranscriptRow> {
    let mut rows = Vec::with_capacity(1 + turn.assistant_rows.len() + turn.tool_rows.len());
    rows.push(&turn.user_row);
    rows.extend(turn.assistant_rows.iter());
    rows.extend(turn.tool_rows.iter());
    rows
}

/// Build a full synthetic turn (placeholder user row + assistant row) for
/// one [`TurnSummary`], returned in write order.
///
/// A summary cannot be written as a bare assistant row: `build_turns`
/// (`transcript.rs::push_row_into_turns`) drops any row that precedes the
/// first genuine user row it encounters while walking the parent chain —
/// an accepted gap there, but fatal here whenever a summary is the very
/// first thing written after the boundary (i.e. `prefix_turns` is empty,
/// the common case for a transcript's first-ever compaction). Wrapping
/// the summary in its own placeholder-user-then-assistant turn guarantees
/// `build_turns` recognizes it as a real `Turn`, so
/// [`crate::claude_code_session::boundary::create_plan`]'s marker scan on
/// a later, idempotent recompaction pass actually sees the
/// `consoletteCompact.summary` marker instead of silently missing it.
#[allow(clippy::expect_used)] // CompactionMetrics is plain numeric/Option<f64> fields — serialization cannot fail.
fn build_summary_turn_rows(
    summary: &TurnSummary,
    parent_uuid: Option<&str>,
    timestamp: &str,
    metrics: Option<&CompactionMetrics>,
) -> (TranscriptRow, TranscriptRow) {
    let mut user_marker = json!({
        "summary": true,
        "coversTurnUuids": summary.covers_turn_uuids,
    });
    if let Some(metrics) = metrics {
        user_marker["metrics"] =
            serde_json::to_value(metrics).expect("CompactionMetrics serializes");
    }
    let mut user_extra = serde_json::Map::new();
    user_extra.insert("type".to_string(), json!("user"));
    user_extra.insert("timestamp".to_string(), json!(timestamp));
    user_extra.insert("consoletteCompact".to_string(), user_marker);
    let user_uuid = uuid::Uuid::new_v4().to_string();
    let user_row = TranscriptRow::User(RowFields {
        uuid: user_uuid.clone(),
        parent_uuid: parent_uuid.map(str::to_string),
        is_sidechain: false,
        is_meta: false,
        message: Some(json!({
            "role": "user",
            "content": "[consolette: compacted turns summary]",
        })),
        extra: user_extra,
    });

    let mut assistant_marker = json!({
        "summary": true,
        "coversTurnUuids": summary.covers_turn_uuids,
    });
    if let Some(metrics) = metrics {
        assistant_marker["metrics"] =
            serde_json::to_value(metrics).expect("CompactionMetrics serializes");
    }
    let mut assistant_extra = serde_json::Map::new();
    assistant_extra.insert("type".to_string(), json!("assistant"));
    assistant_extra.insert("timestamp".to_string(), json!(timestamp));
    assistant_extra.insert("consoletteCompact".to_string(), assistant_marker);
    let assistant_row = TranscriptRow::Assistant(RowFields {
        uuid: uuid::Uuid::new_v4().to_string(),
        parent_uuid: Some(user_uuid),
        is_sidechain: false,
        is_meta: false,
        message: Some(json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": summary.summary_text }],
        })),
        extra: assistant_extra,
    });

    (user_row, assistant_row)
}

/// Return a copy of `row` with its `parentUuid` set to `parent_uuid`.
///
/// Used to re-parent the first row of a written block (`prefix_turns`,
/// `preserved_turns`) onto the last row of the immediately preceding
/// block, since that first row's *original* parent no longer exists in
/// the destination transcript — see the write-order comment in
/// [`write_destination_transcript`] for why this is required for
/// `build_turns` to reconstruct a connected chain on a later parse.
fn reparent(row: &TranscriptRow, parent_uuid: Option<&str>) -> TranscriptRow {
    let mut fields = row.fields().clone();
    fields.parent_uuid = parent_uuid.map(str::to_string);
    match row {
        TranscriptRow::User(_) => TranscriptRow::User(fields),
        TranscriptRow::Assistant(_) => TranscriptRow::Assistant(fields),
        TranscriptRow::System(_) => TranscriptRow::System(fields),
        TranscriptRow::Unknown(_) => TranscriptRow::Unknown(fields),
    }
}

/// Rewrite `row`'s `sessionId` field (if present) to `new_session_id`, so
/// every row in the destination transcript agrees on which session it
/// belongs to.
fn restamp_session_id(row: &TranscriptRow, new_session_id: &str) -> TranscriptRow {
    let mut fields = row.fields().clone();
    if fields.extra.contains_key("sessionId") {
        fields.extra.insert(
            "sessionId".to_string(),
            Value::String(new_session_id.to_string()),
        );
    }
    match row {
        TranscriptRow::User(_) => TranscriptRow::User(fields),
        TranscriptRow::Assistant(_) => TranscriptRow::Assistant(fields),
        TranscriptRow::System(_) => TranscriptRow::System(fields),
        TranscriptRow::Unknown(_) => TranscriptRow::Unknown(fields),
    }
}

/// Assemble and atomically write a destination transcript for one
/// compaction run.
///
/// `out_path`'s file stem (without extension) becomes the new destination
/// session ID, returned on success — Claude Code session files are named
/// `<session-id>.jsonl`, so the caller is expected to have already chosen
/// `out_path` accordingly (e.g. a freshly generated UUID in the same
/// project directory as the source transcript).
///
/// Write order: a boundary row (see [`build_boundary_row`]), then
/// `plan.prefix_turns` verbatim, then one row per `summaries` entry, then
/// `plan.preserved_turns` verbatim. All rows share one flattened
/// `chrono::Utc::now()` timestamp generated once at the start of this call
/// (the boundary and summary rows use it directly; verbatim rows keep
/// their own original timestamps).
///
/// The file itself is written atomically: serialized to `<out_path>.tmp`
/// first, then renamed into place, matching
/// `src/bin/mcp-proxy/metrics.rs::write_session_start`'s tmp-then-rename
/// pattern, so a reader never observes a partially written transcript.
///
/// # Errors
///
/// Returns an error if `out_path` has no file stem, if serializing any row
/// fails, or if the tmp-write or rename fails.
pub fn write_destination_transcript(
    plan: &CompactionPlan,
    summaries: &[TurnSummary],
    source_session_id: &str,
    pruned_count: usize,
    out_path: &Path,
    metrics: Option<&CompactionMetrics>,
) -> Result<String> {
    let new_session_id = out_path
        .file_stem()
        .and_then(OsStr::to_str)
        .ok_or_else(|| {
            anyhow!(
                "out_path {} has no file stem to use as a session id",
                out_path.display()
            )
        })?
        .to_string();

    let timestamp = chrono::Utc::now().to_rfc3339();

    // The boundary row's `logicalParentUuid` points at the last row before
    // it — the last row of the most recent prefix turn, or (if there is no
    // prefix, i.e. this is the first compaction of this transcript) the
    // last row of the last turn being summarized, matching the real
    // Claude Code shape ADR-011 documents (`logicalParentUuid` always
    // points at *some* prior row when one exists).
    let logical_parent_uuid = plan
        .prefix_turns
        .last()
        .or(plan.turns_to_summarize.last())
        .or(plan.preserved_turns.last())
        .and_then(|turn| turn_rows(turn).last().map(|row| row.uuid().to_string()));

    let boundary_row = build_boundary_row(
        source_session_id,
        pruned_count,
        logical_parent_uuid.as_deref(),
        &timestamp,
    );

    let mut out_rows: Vec<TranscriptRow> = Vec::new();
    out_rows.push(boundary_row);
    let mut last_uuid = out_rows.last().map(|r| r.uuid().to_string());

    // `build_turns` reconstructs turns by walking the parent chain
    // *backward* from the transcript's last row; a row not reachable via
    // `parentUuid` from that walk is not merely treated as a new chain
    // root — it, and everything before it, is dropped from the returned
    // turns entirely (see `transcript.rs::build_turns`'s "starting new
    // turn" branch, which simply stops walking rather than starting a
    // second chain). `prefix_turns`/`preserved_turns` keep their rows'
    // *internal* `parentUuid` links verbatim (those still point at real
    // rows in this same block), but each block's first row originally
    // pointed at a row that is no longer written (it was folded into a
    // summary, or is the tail of a differently-ordered turn group), so it
    // must be re-parented onto the immediately preceding row actually
    // written here. Without this, the boundary/summary rows silently fall
    // out of every future replan — exactly the idempotent-recompaction
    // property this writer exists to guarantee.
    let mut prefix_rows: Vec<TranscriptRow> = plan
        .prefix_turns
        .iter()
        .flat_map(|turn| turn_rows(turn).into_iter().cloned())
        .collect();
    if let Some(first) = prefix_rows.first_mut() {
        *first = reparent(first, last_uuid.as_deref());
    }
    if let Some(last) = prefix_rows.last() {
        last_uuid = Some(last.uuid().to_string());
    }
    out_rows.extend(prefix_rows);

    for summary in summaries {
        let (user_row, assistant_row) =
            build_summary_turn_rows(summary, last_uuid.as_deref(), &timestamp, metrics);
        last_uuid = Some(assistant_row.uuid().to_string());
        out_rows.push(user_row);
        out_rows.push(assistant_row);
    }

    let mut preserved_rows: Vec<TranscriptRow> = plan
        .preserved_turns
        .iter()
        .flat_map(|turn| turn_rows(turn).into_iter().cloned())
        .collect();
    if let Some(first) = preserved_rows.first_mut() {
        *first = reparent(first, last_uuid.as_deref());
    }
    out_rows.extend(preserved_rows);

    let restamped: Vec<TranscriptRow> = out_rows
        .iter()
        .map(|row| restamp_session_id(row, &new_session_id))
        .collect();

    let mut buf = String::new();
    for row in &restamped {
        let line =
            serde_json::to_string(row).context("failed to serialize destination transcript row")?;
        buf.push_str(&line);
        buf.push('\n');
    }

    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
    }

    let tmp = out_path.with_extension("tmp");
    fs::write(&tmp, buf).with_context(|| format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, out_path).with_context(|| {
        format!(
            "failed to rename {} to {}",
            tmp.display(),
            out_path.display()
        )
    })?;

    Ok(new_session_id)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::claude_code_session::boundary::create_plan;
    use crate::claude_code_session::transcript::{build_turns, parse_session_file};
    use std::io::Write;
    use tempfile::TempDir;

    fn user_row(uuid: &str, parent: Option<&str>, session_id: &str) -> String {
        let parent_json = parent.map_or_else(|| "null".to_string(), |p| format!("\"{p}\""));
        format!(
            r#"{{"type":"user","uuid":"{uuid}","parentUuid":{parent_json},"isSidechain":false,"isMeta":false,"sessionId":"{session_id}","message":{{"role":"user","content":"hi"}}}}"#
        )
    }

    fn assistant_row(uuid: &str, parent: Option<&str>, session_id: &str) -> String {
        let parent_json = parent.map_or_else(|| "null".to_string(), |p| format!("\"{p}\""));
        format!(
            r#"{{"type":"assistant","uuid":"{uuid}","parentUuid":{parent_json},"isSidechain":false,"isMeta":false,"sessionId":"{session_id}","message":{{"role":"assistant","content":[{{"type":"text","text":"response"}}]}}}}"#
        )
    }

    #[test]
    fn write_destination_transcript_should_round_trip_boundary_prefix_summary_and_preserved_rows() {
        let dir = TempDir::new().unwrap();
        let source_path = dir.path().join("source.jsonl");
        let mut lines = Vec::new();
        let mut prev: Option<String> = None;
        for i in 1..=4 {
            let u = format!("u{i}");
            let a = format!("a{i}");
            lines.push(user_row(&u, prev.as_deref(), "source-session"));
            lines.push(assistant_row(&a, Some(&u), "source-session"));
            prev = Some(a);
        }
        {
            let mut f = fs::File::create(&source_path).unwrap();
            for line in &lines {
                writeln!(f, "{line}").unwrap();
            }
        }

        let rows = parse_session_file(&source_path).unwrap();
        let turns = build_turns(&rows).unwrap();
        assert_eq!(turns.len(), 4);

        // No prior marker: preserve the last turn, summarize the rest.
        let plan = create_plan(turns, 1);
        assert_eq!(plan.turns_to_summarize.len(), 3);
        assert_eq!(plan.preserved_turns.len(), 1);

        let summaries = vec![TurnSummary {
            covers_turn_uuids: vec!["u1".to_string(), "u2".to_string(), "u3".to_string()],
            summary_text: "summary of turns 1-3".to_string(),
        }];

        let out_dir = TempDir::new().unwrap();
        let new_session_id = uuid::Uuid::new_v4().to_string();
        let out_path = out_dir.path().join(format!("{new_session_id}.jsonl"));

        let returned_id =
            write_destination_transcript(&plan, &summaries, "source-session", 0, &out_path, None)
                .unwrap();
        assert_eq!(returned_id, new_session_id);

        let written_rows = parse_session_file(&out_path).unwrap();
        // boundary + 0 prefix turns' rows + 1 summary turn (user + assistant)
        // + 1 preserved turn's 2 rows.
        assert_eq!(written_rows.len(), 5);

        assert_eq!(
            written_rows[0]
                .fields()
                .extra
                .get("subtype")
                .and_then(Value::as_str),
            Some("compact_boundary")
        );
        assert_eq!(written_rows[0].parent_uuid(), None);

        let summary_assistant_row = &written_rows[2];
        assert_eq!(
            summary_assistant_row
                .fields()
                .extra
                .get("consoletteCompact"),
            Some(&json!({ "summary": true, "coversTurnUuids": ["u1", "u2", "u3"] }))
        );

        let preserved_user = &written_rows[3];
        assert_eq!(preserved_user.uuid(), "u4");
        assert_eq!(
            preserved_user
                .fields()
                .extra
                .get("sessionId")
                .and_then(Value::as_str),
            Some(new_session_id.as_str()),
            "preserved rows must be restamped with the new session id"
        );

        // Re-parsing and re-planning the destination transcript must treat
        // the summary row as already-marked, so a second compaction run
        // does not re-summarize turns 1-3 — the idempotency property this
        // writer exists to support. A real caller re-plans with the same
        // preserve_last_n_turns it used originally (here, 1), so the
        // preserved u4/a4 turn is protected the same way on both passes;
        // it is only "prefix" (marker-protected) or "preserved"
        // (preserve_last_n-protected), never re-summarized either way.
        let reparsed_turns = build_turns(&written_rows).unwrap();
        let replan = create_plan(reparsed_turns, 1);
        assert!(replan.turns_to_summarize.is_empty());
        assert_eq!(replan.prefix_turns.len(), 1);
        assert_eq!(replan.preserved_turns.len(), 1);
    }

    #[test]
    fn write_destination_transcript_should_stamp_metrics_onto_summary_marker_when_provided() {
        use crate::claude_code_session::boundary::CompactionMetrics;

        let dir = TempDir::new().unwrap();
        let source_path = dir.path().join("source.jsonl");
        let mut lines = Vec::new();
        let mut prev: Option<String> = None;
        for i in 1..=2 {
            let u = format!("u{i}");
            let a = format!("a{i}");
            lines.push(user_row(&u, prev.as_deref(), "source-session"));
            lines.push(assistant_row(&a, Some(&u), "source-session"));
            prev = Some(a);
        }
        {
            let mut f = fs::File::create(&source_path).unwrap();
            for line in &lines {
                writeln!(f, "{line}").unwrap();
            }
        }

        let rows = parse_session_file(&source_path).unwrap();
        let turns = build_turns(&rows).unwrap();
        let plan = create_plan(turns, 0);

        let summaries = vec![TurnSummary {
            covers_turn_uuids: vec!["u1".to_string(), "u2".to_string()],
            summary_text: "summary of turns 1-2".to_string(),
        }];

        let metrics = CompactionMetrics {
            tokens_before: 100,
            tokens_after: 20,
            tokens_saved: 80,
            estimated_cost_usd: Some(0.0042),
            real_cost_usd: Some(0.0039),
        };

        let out_dir = TempDir::new().unwrap();
        let new_session_id = uuid::Uuid::new_v4().to_string();
        let out_path = out_dir.path().join(format!("{new_session_id}.jsonl"));

        write_destination_transcript(
            &plan,
            &summaries,
            "source-session",
            0,
            &out_path,
            Some(&metrics),
        )
        .unwrap();

        let written_rows = parse_session_file(&out_path).unwrap();
        let summary_assistant_row = &written_rows[2];
        assert_eq!(
            summary_assistant_row
                .fields()
                .extra
                .get("consoletteCompact"),
            Some(&json!({
                "summary": true,
                "coversTurnUuids": ["u1", "u2"],
                "metrics": {
                    "tokens_before": 100,
                    "tokens_after": 20,
                    "tokens_saved": 80,
                    "estimated_cost_usd": 0.0042,
                    "real_cost_usd": 0.0039,
                },
            }))
        );
    }
}
