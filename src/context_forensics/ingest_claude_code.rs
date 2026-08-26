//! `ingest_claude_code_session` — turns a Claude Code transcript path into
//! stored [`SessionRow`]/[`TurnRow`]/[`ApiCallRow`]/native-compaction rows
//! (context-analyzer plan.md Story 1.3.2). A single, testable Transaction
//! Script (Pattern Decision: no rich `Session`/`Turn` domain objects — this
//! is an ETL-shaped problem).
//!
//! `TurnRow.cumulative_tokens` is the turn's *context size* — the largest
//! `input_tokens + cache_creation_input_tokens + cache_read_input_tokens`
//! across that turn's API calls, i.e. the total token count Claude actually
//! saw as context at that point in the conversation. This is what grows
//! (non-monotonically across compaction boundaries, but generally upward)
//! across a session and what the dashboard's growth chart / budget
//! thresholds (200K/500K/700K/1M) compare against — not a running sum of
//! per-turn *production* (`output_tokens`), which would answer a different
//! question ("how much has been generated") than the one this feature
//! exists to answer ("how full is the context window").

use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use crate::claude_code_session::native_compaction::{
    extract_native_compaction_events, NativeCompactionEvent,
};
use crate::claude_code_session::transcript::{
    build_turns, chain_coverage, parse_session_file, TranscriptRow,
};
use crate::context_forensics::composition::{
    classify_call_composition, log_dropped_leading_rows_with_usage,
};
use crate::context_forensics::store::{
    ApiCallRow, ContextForensicsStore, NativeCompactionEventRow, SessionRow, Source, TurnRow,
    UsageProvenance,
};
use crate::context_forensics::usage::extract_call_usage;

/// Result of ingesting one session file: how much was written, and how many
/// (line-level) parse failures were absorbed along the way.
///
/// `parse_failures` reflects only what this function can see —
/// `parse_session_file` already warns-and-skips malformed *lines*
/// internally without exposing a count, so this is reserved for a future
/// per-line failure count should that ever get threaded through; today it
/// is always `0`. Whole-*file* failures (this function returning `Err`) are
/// a separate, corpus-level concern handled by the caller
/// ([`crate::context_forensics::refresh`]'s per-file try/catch loop).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IngestSummary {
    pub turns_ingested: u64,
    pub calls_ingested: u64,
    pub parse_failures: u64,
}

/// Parse `path`, reconstruct turns, classify composition, and
/// upsert every derived row into `store`.
///
/// # Errors
///
/// Returns an error if the file can't be opened/read, if `build_turns`
/// detects a `parentUuid` cycle (logged via `tracing::warn!` before
/// returning — this is the per-session-file failure isolation point Story
/// 1.3.3's rescan loop relies on to keep one bad transcript from blanking
/// an entire corpus), or if any store write fails.
pub fn ingest_claude_code_session(
    store: &ContextForensicsStore,
    path: &Path,
) -> Result<IngestSummary> {
    let rows = parse_session_file(path)?;
    let turns = match build_turns(&rows) {
        Ok(turns) => turns,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "skipping session: parent-chain cycle detected");
            return Err(error);
        }
    };

    log_dropped_leading_rows_with_usage(&rows, &turns);
    let coverage = chain_coverage(&rows, &turns);
    let session_id = session_id_from_path(path);
    let project = project_from_path(path);
    let started_at = turns
        .first()
        .and_then(|turn| turn.user_row.fields().extra.get("timestamp"))
        .and_then(Value::as_str)
        .map(String::from);

    let session_row = SessionRow {
        id: session_id.clone(),
        source: Source::ClaudeCode,
        path: path.display().to_string(),
        project,
        started_at,
        last_ingested_at: chrono::Utc::now().to_rfc3339(),
        chain_coverage_ratio: Some(coverage.ratio()),
        parse_failure_count: 0,
    };

    let mut turns_ingested = 0u64;
    let mut calls_ingested = 0u64;
    let mut call_index = 0u64;
    let mut turn_rows: Vec<TurnRow> = Vec::new();
    let mut call_rows: Vec<ApiCallRow> = Vec::new();

    for (turn_index, turn) in turns.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let turn_index_u64 = turn_index as u64;
        let Some((turn_row, pending_calls)) =
            collect_turn_row(&session_id, turn_index_u64, turn, &mut call_index)
        else {
            continue;
        };
        calls_ingested += pending_calls.len() as u64;
        turns_ingested += 1;
        turn_rows.push(turn_row);
        call_rows.extend(pending_calls);
    }

    let turn_index_by_uuid = build_turn_index_lookup(&turns);
    let compaction_rows: Vec<NativeCompactionEventRow> = native_compaction_events_with_uuid(&rows)
        .into_iter()
        .map(|(row_uuid, event)| {
            let turn_index = turn_index_by_uuid.get(row_uuid.as_str()).copied();
            NativeCompactionEventRow {
                session_id: session_id.clone(),
                row_uuid,
                tokens_saved: event.tokens_saved(),
                turn_index,
            }
        })
        .collect();

    // One transaction for the whole session (Task 1.4.4a follow-up fix):
    // `upsert_session`/`upsert_turn`/`upsert_api_call` each auto-commit
    // individually, which is fine for a handful of test-fixture rows but
    // meant one WAL-commit fsync per row when Story 1.3.3's eager rescan
    // runs this over a real multi-thousand-file corpus. See
    // `ContextForensicsStore::upsert_ingested_session`'s doc comment.
    store.upsert_ingested_session(&session_row, &turn_rows, &call_rows, &compaction_rows)?;

    Ok(IngestSummary {
        turns_ingested,
        calls_ingested,
        parse_failures: 0,
    })
}

/// Builds one turn's [`TurnRow`] and its [`ApiCallRow`]s, extracted from
/// [`ingest_claude_code_session`] to keep that function under clippy's
/// line-count lint. Returns `None` when the turn has no API calls (no
/// `TurnRow` is written for a turn that never called out — mirrors the
/// original inline `if !pending_calls.is_empty()` skip) rather than an
/// empty-calls `Some`, so the caller can `continue` in one branch instead of
/// pushing then checking emptiness itself.
fn collect_turn_row(
    session_id: &str,
    turn_index_u64: u64,
    turn: &crate::claude_code_session::transcript::Turn,
    call_index: &mut u64,
) -> Option<(TurnRow, Vec<ApiCallRow>)> {
    let turn_id = format!("{session_id}:{turn_index_u64}");
    let mut turn_context_size = 0u64;
    let mut pending_calls: Vec<ApiCallRow> = Vec::new();

    for row in &turn.assistant_rows {
        let Some(call_usage) = extract_call_usage(row) else {
            continue;
        };

        let breakdown = classify_call_composition(turn, &call_usage);
        let context_size = call_usage
            .input_tokens
            .saturating_add(call_usage.cache_creation_input_tokens)
            .saturating_add(call_usage.cache_read_input_tokens);
        turn_context_size = turn_context_size.max(context_size);

        let model = row
            .fields()
            .message
            .as_ref()
            .and_then(|message| message.get("model"))
            .and_then(Value::as_str)
            .map(String::from);

        let message_json = row
            .fields()
            .message
            .as_ref()
            .and_then(|message| serde_json::to_string(message).ok());

        pending_calls.push(ApiCallRow {
            id: format!("{session_id}:{}", row.uuid()),
            session_id: session_id.to_string(),
            turn_id: Some(turn_id.clone()),
            row_uuid: row.uuid().to_string(),
            call_index: *call_index,
            model,
            input_tokens: call_usage.input_tokens,
            output_tokens: call_usage.output_tokens,
            cache_creation_input_tokens: Some(call_usage.cache_creation_input_tokens),
            cache_read_input_tokens: Some(call_usage.cache_read_input_tokens),
            tool_io_tokens: breakdown.tool_io_tokens,
            conversation_tokens: breakdown.conversation_tokens,
            system_tokens: breakdown.system_tokens,
            usage_provenance: UsageProvenance::TranscriptExact,
            message_json,
        });
        *call_index += 1;
    }

    // The turn row must exist before any api_calls row referencing it via
    // `turn_id` (a foreign key) — the caller pushes this before extending
    // `call_rows` so `upsert_ingested_session`'s single transaction still
    // writes parent-before-child.
    if pending_calls.is_empty() {
        return None;
    }
    let user_row_json = turn
        .user_row
        .fields()
        .message
        .as_ref()
        .and_then(|message| serde_json::to_string(message).ok());
    let tool_row_messages: Vec<Value> = turn
        .tool_rows
        .iter()
        .filter_map(|row| row.fields().message.clone())
        .collect();
    let tool_rows_json = serde_json::to_string(&Value::Array(tool_row_messages)).ok();

    Some((
        TurnRow {
            id: turn_id,
            session_id: session_id.to_string(),
            turn_index: turn_index_u64,
            user_row_uuid: turn.user_row.uuid().to_string(),
            cumulative_tokens: turn_context_size,
            user_row_json,
            tool_rows_json,
        },
        pending_calls,
    ))
}

/// Pair each [`NativeCompactionEvent`] with the `uuid` of the row it was
/// extracted from — `extract_native_compaction_events` itself returns bare
/// events (no uuid), since `claude_code_session`'s own callers never needed
/// the pairing; this crate does, to satisfy `native_compaction_events`'
/// `(session_id, row_uuid)` unique key.
fn native_compaction_events_with_uuid(
    rows: &[TranscriptRow],
) -> Vec<(String, NativeCompactionEvent)> {
    rows.iter()
        .filter_map(|row| {
            let events = extract_native_compaction_events(std::slice::from_ref(row));
            events
                .into_iter()
                .next()
                .map(|event| (row.uuid().to_string(), event))
        })
        .collect()
}

/// `row uuid -> turn_index` for every row folded into a [`Turn`] (its
/// `user_row` and every `assistant_rows`/`tool_rows` entry) — lets a
/// `compact_boundary` system row (which `build_turns` typically attaches to
/// the current turn's `assistant_rows`) be placed on the growth chart at
/// the right turn without re-parsing the transcript at query time. A row
/// `build_turns` dropped entirely (the "leading row" accepted gap) has no
/// entry and its compaction event is stored with `turn_index: None`.
fn build_turn_index_lookup(
    turns: &[crate::claude_code_session::transcript::Turn],
) -> std::collections::HashMap<&str, u64> {
    let mut lookup = std::collections::HashMap::new();
    for (turn_index, turn) in turns.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let turn_index_u64 = turn_index as u64;
        lookup.insert(turn.user_row.uuid(), turn_index_u64);
        for row in turn.assistant_rows.iter().chain(turn.tool_rows.iter()) {
            lookup.insert(row.uuid(), turn_index_u64);
        }
    }
    lookup
}

/// The session id: the transcript file's stem (Claude Code names session
/// files `<session-uuid>.jsonl`).
fn session_id_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("unknown")
        .to_string()
}

/// The project directory name immediately under `.claude/projects/` —
/// duplicated from `claude_code_session::session_bi`'s private helper of
/// the same shape rather than exporting it, matching this codebase's own
/// "duplicate a small lookup rather than force a premature shared
/// abstraction" convention (`omission_cache.rs::default_cache_path`'s doc
/// comment).
fn project_from_path(path: &Path) -> Option<String> {
    let components: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    components
        .iter()
        .position(|&c| c == "projects")
        .and_then(|idx| components.get(idx + 1))
        .map(std::string::ToString::to_string)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};

    fn store() -> (TempDir, ContextForensicsStore) {
        let dir = TempDir::new().unwrap();
        let store = ContextForensicsStore::open(&dir.path().join("cf.sqlite")).unwrap();
        (dir, store)
    }

    fn write_fixture(lines: &[String]) -> NamedTempFile {
        let mut f = NamedTempFile::with_suffix(".jsonl").unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        f
    }

    fn user_line(uuid: &str, parent: Option<&str>, text: &str) -> String {
        let parent_json = parent.map_or("null".to_string(), |p| format!("\"{p}\""));
        format!(
            r#"{{"type":"user","uuid":"{uuid}","parentUuid":{parent_json},"isSidechain":false,"isMeta":false,"timestamp":"2026-01-01T00:00:00Z","message":{{"role":"user","content":"{text}"}}}}"#
        )
    }

    fn assistant_line_with_usage(uuid: &str, parent: &str, input_tokens: u64) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"{uuid}","parentUuid":"{parent}","isSidechain":false,"isMeta":false,"message":{{"role":"assistant","model":"claude-sonnet-5","content":[{{"type":"text","text":"reply"}}],"usage":{{"input_tokens":{input_tokens},"output_tokens":20,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}}}}"#
        )
    }

    #[test]
    fn ingest_claude_code_session_should_persist_turns_and_calls_when_fixture_has_three_turns() {
        let (_dir, store) = store();
        let lines = vec![
            user_line("u1", None, "hi"),
            assistant_line_with_usage("a1", "u1", 1000),
            user_line("u2", Some("a1"), "again"),
            assistant_line_with_usage("a2", "u2", 2000),
            user_line("u3", Some("a2"), "once more"),
            assistant_line_with_usage("a3", "u3", 3000),
        ];
        let fixture = write_fixture(&lines);

        let summary = ingest_claude_code_session(&store, fixture.path()).unwrap();

        assert_eq!(summary.turns_ingested, 3);
        assert_eq!(summary.calls_ingested, 3);
        assert_eq!(store.session_row_count().unwrap(), 1);

        let session_id = session_id_from_path(fixture.path());
        assert_eq!(store.turns_for_session(&session_id).unwrap().len(), 3);
        assert_eq!(store.api_calls_for_session(&session_id).unwrap().len(), 3);
    }

    #[test]
    fn ingest_claude_code_session_should_store_chain_coverage_ratio_when_multi_root_transcript() {
        let (_dir, store) = store();
        let lines = vec![
            user_line("u1", None, "first conversation"),
            assistant_line_with_usage("a1", "u1", 500),
            user_line("u2", None, "second conversation"),
            assistant_line_with_usage("a2", "u2", 700),
        ];
        let fixture = write_fixture(&lines);

        ingest_claude_code_session(&store, fixture.path()).unwrap();

        let session_id = session_id_from_path(fixture.path());
        let session = store.get_session(&session_id).unwrap().unwrap();
        assert!((session.chain_coverage_ratio.unwrap() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn ingest_claude_code_session_should_return_err_when_parent_chain_cycle_detected() {
        let (_dir, store) = store();
        let lines = vec![
            user_line("a", Some("b"), "x"),
            user_line("b", Some("a"), "y"),
        ];
        let fixture = write_fixture(&lines);

        let result = ingest_claude_code_session(&store, fixture.path());
        assert!(result.is_err());
    }

    #[test]
    fn ingest_claude_code_session_should_compute_cumulative_tokens_as_context_size_when_cache_tokens_present(
    ) {
        let (_dir, store) = store();
        let lines = vec![
            user_line("u1", None, "hi"),
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","isSidechain":false,"isMeta":false,"message":{"role":"assistant","content":[{"type":"text","text":"reply"}],"usage":{"input_tokens":1000,"output_tokens":20,"cache_creation_input_tokens":500,"cache_read_input_tokens":8000}}}"#
                .to_string(),
        ];
        let fixture = write_fixture(&lines);

        ingest_claude_code_session(&store, fixture.path()).unwrap();

        let session_id = session_id_from_path(fixture.path());
        let turns = store.turns_for_session(&session_id).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].cumulative_tokens, 1000 + 500 + 8000);
    }
}
