//! Claude Code session transcript compaction.
//!
//! See `project_plans/compaction-hook/decisions/ADR-008-claude-code-session-module-placement.md`
//! for why this lives as a self-contained top-level module rather than
//! under `crate::compression`.

pub mod boundary;
pub mod mcp_server;
pub mod omission_cache;
pub mod prune;
pub mod summarize;
pub mod transcript;
pub mod writer;

use std::path::Path;

use anyhow::{Context, Result};

use crate::claude_code_session::boundary::create_plan;
use crate::claude_code_session::omission_cache::OmissionCache;
use crate::claude_code_session::prune::{prune_tool_row, PrunedRow};
use crate::claude_code_session::summarize::Summarizer;
use crate::claude_code_session::transcript::{build_turns, parse_session_file, Turn};
use crate::claude_code_session::writer::write_destination_transcript;

/// End-to-end compaction pipeline for one source session transcript (Epic
/// 5.2): parse -> reconstruct turns -> plan -> prune oversized tool output
/// -> summarize old turns -> write the destination transcript.
///
/// `summarizer` is generic over [`Summarizer`] so tests and the fixture
/// integration test can substitute
/// [`crate::claude_code_session::summarize::FakeSummarizer`] for the real
/// subprocess-backed
/// [`crate::claude_code_session::summarize::ClaudeCliSummarizer`], without
/// this function's logic differing between the two.
///
/// Returns the new destination session ID (`out_path`'s file stem) on
/// success.
///
/// # Errors
///
/// Returns an error, with full context identifying which pipeline stage
/// failed, if parsing, pruning, summarization, or writing fails.
pub async fn compact_session<S: Summarizer>(
    session_path: &Path,
    out_path: &Path,
    cache: &OmissionCache,
    summarizer: &S,
    preserve_last_n_turns: usize,
) -> Result<String> {
    let source_session_id = session_path
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("unknown-session")
        .to_string();

    // The session_id newly-pruned content must be cached under: after
    // `write_destination_transcript` restamps every row's `sessionId` field
    // to `out_path`'s file stem (see writer.rs's `restamp_session_id`),
    // `claude --resume <that-id>` is what the user actually runs — so the
    // MCP client's own runtime session context at `read_omitted_content`
    // call time is this destination id, not `source_session_id`. Caching
    // fresh insertions under `source_session_id` instead would make every
    // pruned placeholder permanently unretrievable post-resume. Rows already
    // pruned by an earlier compaction pass (present in `prefix_turns`) are
    // unaffected: they were already cached under what was, at the time,
    // *that* pass's destination id — which is `session_path`'s own current
    // file stem, i.e. `source_session_id` — so re-pruning them here (a
    // no-op, since their placeholder text no longer exceeds any threshold)
    // never needs to look them up under the new id.
    let destination_session_id = out_path
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("unknown-session")
        .to_string();

    let rows = parse_session_file(session_path)
        .with_context(|| format!("failed to parse session file {}", session_path.display()))?;

    let turns = build_turns(&rows).with_context(|| {
        format!(
            "failed to reconstruct turns from {}",
            session_path.display()
        )
    })?;

    let plan = create_plan(turns, preserve_last_n_turns);

    // Prune oversized tool output out of every row group across all three
    // plan buckets — a turn that ends up prefix/preserved (written
    // verbatim) still benefits from pruning, and a turn about to be
    // summarized is pruned first so the summarizer never sees (or has to
    // pay token cost for) content that's about to be discarded anyway.
    let prune_turn = |turn: &Turn| -> Result<Turn> {
        let prune_row =
            |row: &crate::claude_code_session::transcript::TranscriptRow| match prune_tool_row(
                row,
                cache,
                &destination_session_id,
            )
            .context("failed to prune tool row into omission cache")?
            {
                PrunedRow::Unchanged(row) | PrunedRow::Pruned { row, .. } => Ok(row),
            };

        Ok(Turn {
            user_row: prune_row(&turn.user_row)?,
            assistant_rows: turn
                .assistant_rows
                .iter()
                .map(prune_row)
                .collect::<Result<Vec<_>>>()?,
            tool_rows: turn
                .tool_rows
                .iter()
                .map(prune_row)
                .collect::<Result<Vec<_>>>()?,
        })
    };

    let prefix_turns = plan
        .prefix_turns
        .iter()
        .map(prune_turn)
        .collect::<Result<Vec<_>>>()
        .context("failed to prune prefix turns")?;
    let turns_to_summarize = plan
        .turns_to_summarize
        .iter()
        .map(prune_turn)
        .collect::<Result<Vec<_>>>()
        .context("failed to prune turns scheduled for summarization")?;
    let preserved_turns = plan
        .preserved_turns
        .iter()
        .map(prune_turn)
        .collect::<Result<Vec<_>>>()
        .context("failed to prune preserved turns")?;
    let pruned_count = turns_to_summarize.len();

    // Skip the summarizer call entirely when there's nothing to summarize —
    // critical for idempotent recompaction: an already-compacted transcript
    // replanned with the same preserve_last_n_turns produces an empty
    // turns_to_summarize (everything is either marked prefix or within the
    // preserved tail), and invoking a real ClaudeCliSummarizer here would
    // needlessly shell out to `claude -p --resume` on every no-op
    // recompaction pass.
    let summaries = if turns_to_summarize.is_empty() {
        Vec::new()
    } else {
        summarizer
            .summarize(&source_session_id, &turns_to_summarize)
            .await
            .context("summarizer failed")?
    };

    let pruned_plan = crate::claude_code_session::boundary::CompactionPlan {
        prefix_turns,
        turns_to_summarize,
        preserved_turns,
    };

    write_destination_transcript(
        &pruned_plan,
        &summaries,
        &source_session_id,
        pruned_count,
        out_path,
    )
    .with_context(|| {
        format!(
            "failed to write destination transcript {}",
            out_path.display()
        )
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::claude_code_session::summarize::{FakeSummarizer, TurnSummary};
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    fn user_row(uuid: &str, parent: Option<&str>, text: &str) -> String {
        let parent_json = parent.map_or_else(|| "null".to_string(), |p| format!("\"{p}\""));
        format!(
            r#"{{"type":"user","uuid":"{uuid}","parentUuid":{parent_json},"isSidechain":false,"isMeta":false,"message":{{"role":"user","content":"{text}"}}}}"#
        )
    }

    fn assistant_row(uuid: &str, parent: Option<&str>, text: &str) -> String {
        let parent_json = parent.map_or_else(|| "null".to_string(), |p| format!("\"{p}\""));
        format!(
            r#"{{"type":"assistant","uuid":"{uuid}","parentUuid":{parent_json},"isSidechain":false,"isMeta":false,"message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}}}}"#
        )
    }

    /// Task 5.2.1d: end-to-end fixture integration test using
    /// [`FakeSummarizer`] — deliberately never depends on the real `claude`
    /// CLI, so it runs unconditionally in CI.
    #[tokio::test]
    async fn compact_session_should_produce_resumable_transcript_with_fake_summarizer() {
        let dir = TempDir::new().unwrap();
        let source_path = dir.path().join("source-session.jsonl");
        let mut lines = Vec::new();
        let mut prev: Option<String> = None;
        for i in 1..=4 {
            let u = format!("u{i}");
            let a = format!("a{i}");
            lines.push(user_row(&u, prev.as_deref(), "hi"));
            lines.push(assistant_row(&a, Some(&u), "response"));
            prev = Some(a);
        }
        {
            let mut f = fs::File::create(&source_path).unwrap();
            for line in &lines {
                writeln!(f, "{line}").unwrap();
            }
        }

        let cache = OmissionCache::open(&dir.path().join("cache.sqlite")).unwrap();
        let summarizer = FakeSummarizer::with_summaries(vec![TurnSummary {
            covers_turn_uuids: vec!["u1".to_string(), "u2".to_string(), "u3".to_string()],
            summary_text: "summary of turns 1-3".to_string(),
        }]);

        let new_session_id = uuid::Uuid::new_v4().to_string();
        let out_path = dir.path().join(format!("{new_session_id}.jsonl"));

        let returned_id = compact_session(&source_path, &out_path, &cache, &summarizer, 1)
            .await
            .unwrap();
        assert_eq!(returned_id, new_session_id);

        let written_rows = parse_session_file(&out_path).unwrap();
        assert!(!written_rows.is_empty());
        assert_eq!(
            written_rows[0]
                .fields()
                .extra
                .get("subtype")
                .and_then(serde_json::Value::as_str),
            Some("compact_boundary")
        );

        // Idempotent recompaction: replanning the destination transcript
        // with the same preserve_last_n_turns must not re-summarize.
        let reparsed_turns = build_turns(&written_rows).unwrap();
        let replan = create_plan(reparsed_turns, 1);
        assert!(replan.turns_to_summarize.is_empty());
    }

    #[tokio::test]
    async fn compact_session_should_propagate_summarizer_failure_with_context() {
        let dir = TempDir::new().unwrap();
        let source_path = dir.path().join("source-session.jsonl");
        {
            let mut f = fs::File::create(&source_path).unwrap();
            writeln!(f, "{}", user_row("u1", None, "hi")).unwrap();
            writeln!(f, "{}", assistant_row("a1", Some("u1"), "response")).unwrap();
        }

        let cache = OmissionCache::open(&dir.path().join("cache.sqlite")).unwrap();
        let summarizer = FakeSummarizer::failing("boom");
        let out_path = dir.path().join(format!("{}.jsonl", uuid::Uuid::new_v4()));

        let result = compact_session(&source_path, &out_path, &cache, &summarizer, 0).await;

        let error = result.expect_err("expected summarizer failure to propagate");
        assert!(format!("{error:#}").contains("summarizer failed"));
        assert!(
            !out_path.exists(),
            "no partial file should be written on failure"
        );
    }

    /// A `tool_result`-carrying row for turn `i`, with `content` oversized
    /// enough to trip [`crate::claude_code_session::prune::DEFAULT_LIMIT_CHARS`]
    /// so at least one row in the synthetic fixture actually exercises
    /// pruning (Task 6.1.1a/b's fixture requirement). `marker` is embedded
    /// in the oversized content so two fixtures built with different
    /// markers are distinguishable even though `OmissionCache` assigns
    /// `content_id`s independently per session (i.e. two sessions' first
    /// insert both land on `"omitted-001"` — see
    /// [`crate::claude_code_session::omission_cache::OmissionCache::insert`] —
    /// so a cross-session isolation test can't tell sessions apart by
    /// `content_id` alone; it must compare the retrieved *content*).
    fn oversized_tool_row(uuid: &str, parent: &str, marker: &str) -> String {
        let big_content = format!("{marker}-{}", "x".repeat(2000));
        format!(
            r#"{{"type":"user","uuid":"{uuid}","parentUuid":"{parent}","isSidechain":false,"isMeta":false,"message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"tu1","content":"{big_content}","tool_name":"Bash"}}]}}}}"#
        )
    }

    /// Builds a synthetic 6-turn session JSONL (Task 6.1.1's fixture
    /// requirement): turn 2 carries an oversized `Bash` tool result so
    /// pruning is actually exercised, not just turn folding.
    fn six_turn_fixture_lines(marker: &str) -> Vec<String> {
        let mut lines = Vec::new();
        let mut prev: Option<String> = None;
        for i in 1..=6 {
            let u = format!("u{i}");
            let a = format!("a{i}");
            lines.push(user_row(&u, prev.as_deref(), "hi"));
            lines.push(assistant_row(&a, Some(&u), "response"));
            if i == 2 {
                lines.push(oversized_tool_row("t2", &a, marker));
            }
            prev = Some(a);
        }
        lines
    }

    fn write_jsonl(path: &std::path::Path, lines: &[String]) {
        let mut f = fs::File::create(path).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
    }

    /// Task 6.1.1a: end-to-end idempotent-recompaction test on a synthetic
    /// 6-turn fixture with an oversized tool result — running `compact_session`
    /// a second time, using the first run's output as input, must not
    /// re-summarize anything the first run already folded into a summary
    /// (ADR-011's idempotency property, exercised end-to-end).
    #[tokio::test]
    async fn compact_session_should_be_idempotent_when_recompacting_its_own_output() {
        let dir = TempDir::new().unwrap();
        let source_path = dir.path().join("source-session.jsonl");
        write_jsonl(&source_path, &six_turn_fixture_lines("marker"));

        let cache = OmissionCache::open(&dir.path().join("cache.sqlite")).unwrap();
        let summarizer = FakeSummarizer::with_summaries(vec![TurnSummary {
            covers_turn_uuids: vec![
                "u1".to_string(),
                "u2".to_string(),
                "u3".to_string(),
                "u4".to_string(),
            ],
            summary_text: "summary of turns 1-4".to_string(),
        }]);

        let first_out = dir.path().join(format!("{}.jsonl", uuid::Uuid::new_v4()));
        compact_session(&source_path, &first_out, &cache, &summarizer, 2)
            .await
            .unwrap();

        // Second pass: recompact the first run's own output. Nothing should
        // land in turns_to_summarize, since the first pass's summary turn is
        // now marked and everything after it was already within
        // preserve_last_n_turns.
        let second_out = dir.path().join(format!("{}.jsonl", uuid::Uuid::new_v4()));
        let second_summarizer = FakeSummarizer::failing(
            "should not be invoked: nothing should need summarizing on the second pass",
        );
        compact_session(&first_out, &second_out, &cache, &second_summarizer, 2)
            .await
            .unwrap();

        let written_rows = parse_session_file(&second_out).unwrap();
        let reparsed_turns = build_turns(&written_rows).unwrap();
        let replan = create_plan(reparsed_turns, 2);
        assert!(
            replan.turns_to_summarize.is_empty(),
            "recompacting already-compacted output must not find new turns to summarize"
        );
    }

    /// Extracts the `content_id` embedded in a compacted transcript's
    /// pruned-placeholder text (see `prune.rs`'s
    /// `"[pruned: see read_omitted_content(session_id, \"{content_id}\")]"`
    /// format), asserting exactly one such placeholder exists.
    fn placeholder_content_id(
        written_rows: &[crate::claude_code_session::transcript::TranscriptRow],
    ) -> String {
        let placeholder = written_rows
            .iter()
            .find_map(|row| {
                let message = row.fields().message.as_ref()?;
                let content = message.get("content")?.as_array()?;
                content
                    .iter()
                    .find_map(|block| block.get("content").and_then(serde_json::Value::as_str))
            })
            .expect("fixture should have produced at least one pruned placeholder");
        assert!(
            placeholder.starts_with("[pruned: see read_omitted_content"),
            "expected a pruned placeholder, got: {placeholder}"
        );
        placeholder
            .split('"')
            .nth(1)
            .expect("placeholder should embed a quoted content_id")
            .to_string()
    }

    /// Task 6.1.1b: end-to-end cross-session isolation test — compacting two
    /// distinct fixture sessions against the same shared `OmissionCache`
    /// must not let session B's lookup resolve session A's pruned content
    /// (ADR-009's guarantee, exercised through the full `compact_session`
    /// pipeline, not just at the `OmissionCache` unit level).
    ///
    /// Both fixtures' oversized tool content is pruned as each session's
    /// *first* insertion, so both independently land on `content_id`
    /// `"omitted-001"` (`OmissionCache::insert` numbers per-session, not
    /// globally) — a same-`content_id`-under-different-`session_id` lookup
    /// is exactly the case ADR-009's `(session_id, content_id)` composite
    /// key exists to isolate, so the fixtures deliberately use *different*
    /// marker text to make a leak observable: if isolation broke, session
    /// B's lookup would return session A's marker instead of its own.
    #[tokio::test]
    async fn compact_session_should_isolate_pruned_content_across_sessions_end_to_end() {
        let dir = TempDir::new().unwrap();
        let cache = OmissionCache::open(&dir.path().join("cache.sqlite")).unwrap();
        let summarizer = FakeSummarizer::with_summaries(vec![]);

        let session_a_path = dir.path().join("session-a.jsonl");
        write_jsonl(&session_a_path, &six_turn_fixture_lines("session-a-secret"));
        let out_a = dir.path().join(format!("{}.jsonl", uuid::Uuid::new_v4()));
        compact_session(&session_a_path, &out_a, &cache, &summarizer, 6)
            .await
            .unwrap();

        let session_b_path = dir.path().join("session-b.jsonl");
        write_jsonl(&session_b_path, &six_turn_fixture_lines("session-b-secret"));
        let out_b = dir.path().join(format!("{}.jsonl", uuid::Uuid::new_v4()));
        let session_b_id = compact_session(&session_b_path, &out_b, &cache, &summarizer, 6)
            .await
            .unwrap();

        let written_rows_a = parse_session_file(&out_a).unwrap();
        let content_id_a = placeholder_content_id(&written_rows_a);
        let out_a_session_id = out_a.file_stem().unwrap().to_str().unwrap();

        // Session A can read its own pruned content back, and it is
        // genuinely session A's content.
        let content_for_a = cache.get(out_a_session_id, &content_id_a).unwrap();
        assert!(content_for_a
            .as_deref()
            .is_some_and(|c| c.contains("session-a-secret")));

        // Session B, looking up under its *own* session id with the same
        // content_id (both sessions' first insert is "omitted-001"), must
        // get back its own content — never session A's — even though the
        // content_id string collides.
        let content_for_b = cache.get(&session_b_id, &content_id_a).unwrap();
        assert!(
            content_for_b
                .as_deref()
                .is_none_or(|c| !c.contains("session-a-secret")),
            "session B must never be able to read session A's pruned content, \
             even under a colliding content_id"
        );
        assert!(content_for_b
            .as_deref()
            .is_some_and(|c| c.contains("session-b-secret")));
    }
}
