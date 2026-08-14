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
                &source_session_id,
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

    let summaries = summarizer
        .summarize(&source_session_id, &turns_to_summarize)
        .await
        .context("summarizer failed")?;

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
}
