//! ADR-010: subprocess-based summarization of old turns.
//!
//! [`Summarizer`] is a minimal trait seam over "summarize these turns" so
//! `mod.rs`'s orchestration (Phase 5) can be exercised with [`FakeSummarizer`]
//! without invoking a real subprocess. [`ClaudeCliSummarizer`] is the v1
//! implementation: it shells out to
//! `claude -p --resume <session_id> --output-format json`, parses the
//! resulting JSON envelope's `result` field for `<summary>` tags, and
//! surfaces the envelope's `total_cost_usd` as the real, live-measured cost
//! of the call — following the same subprocess pattern established in
//! `src/auth/exec.rs` (resolve via `PATH`, check ownership/world-writable
//! permissions, spawn, `tokio::time::timeout`-wrap the wait, treat every
//! failure mode uniformly, never log stdout/stderr content).

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;

use crate::claude_code_session::transcript::Turn;

// ---------------------------------------------------------------------------
// Epic 3.1: Summarizer trait
// ---------------------------------------------------------------------------

/// One summarized group of turns, ready to be folded into the destination
/// transcript as a single row (Phase 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSummary {
    /// UUIDs (each a turn's `user_row` UUID) of the turns this summary
    /// covers, in the order they were summarized.
    pub covers_turn_uuids: Vec<String>,
    pub summary_text: String,
}

/// Result of one [`Summarizer::summarize`] call: the produced summaries plus
/// (when the implementation can observe it) the real, live-measured USD cost
/// of producing them — as opposed to the `TiktokenEstimator`-derived
/// estimate `mod.rs`'s `compaction_metrics_for` computes independently.
#[derive(Debug, Clone, PartialEq)]
pub struct SummarizeOutcome {
    pub summaries: Vec<TurnSummary>,
    /// `claude -p --output-format json`'s `total_cost_usd` field for the
    /// subprocess call that produced `summaries`. `None` for summarizers
    /// (like [`FakeSummarizer`]) that have no real cost to report.
    pub real_cost_usd: Option<f64>,
}

/// A seam over "summarize these turns" (ADR-010), so `mod.rs`'s
/// orchestration can be tested without invoking a real subprocess.
#[async_trait]
pub trait Summarizer {
    /// Summarizes `turns` from session `session_id`.
    ///
    /// # Errors
    ///
    /// Returns an error if summarization fails for any reason — see the
    /// implementing type's documentation for its specific failure modes.
    async fn summarize(&self, session_id: &str, turns: &[Turn])
        -> anyhow::Result<SummarizeOutcome>;
}

/// Test/fixture double for [`Summarizer`] that returns caller-supplied
/// canned [`TurnSummary`]s (or a canned failure) regardless of input.
///
/// Deliberately **not** gated behind `#[cfg(test)]`: Phase 5's orchestration
/// tests in `mod.rs` construct one of these from outside this module's own
/// test module, so it needs to be a normal `pub` item. It's intended for
/// tests and fixtures only, not for production use.
pub struct FakeSummarizer {
    result: Result<SummarizeOutcome, String>,
}

impl FakeSummarizer {
    /// Returns `summaries` verbatim from every [`Summarizer::summarize`]
    /// call (with no real cost reported), ignoring the actual
    /// `session_id`/`turns` arguments.
    #[must_use]
    pub fn with_summaries(summaries: Vec<TurnSummary>) -> Self {
        Self {
            result: Ok(SummarizeOutcome {
                summaries,
                real_cost_usd: None,
            }),
        }
    }

    /// Like [`Self::with_summaries`], but also configures a canned real
    /// cost — for exercising callers that thread `real_cost_usd` through.
    #[must_use]
    pub fn with_summaries_and_cost(summaries: Vec<TurnSummary>, real_cost_usd: f64) -> Self {
        Self {
            result: Ok(SummarizeOutcome {
                summaries,
                real_cost_usd: Some(real_cost_usd),
            }),
        }
    }

    /// Configures this summarizer to fail every [`Summarizer::summarize`]
    /// call with `message`, for exercising callers' failure paths.
    #[must_use]
    pub fn failing(message: impl Into<String>) -> Self {
        Self {
            result: Err(message.into()),
        }
    }
}

#[async_trait]
impl Summarizer for FakeSummarizer {
    async fn summarize(
        &self,
        _session_id: &str,
        _turns: &[Turn],
    ) -> anyhow::Result<SummarizeOutcome> {
        self.result
            .clone()
            .map_err(|message| anyhow::anyhow!(message))
    }
}

// ---------------------------------------------------------------------------
// Epic 3.2: ClaudeCliSummarizer subprocess implementation
// ---------------------------------------------------------------------------

/// Failure modes for [`ClaudeCliSummarizer`]. Every variant is deliberately
/// free of subprocess stdout/stderr content (ADR-007 §6's redaction rule,
/// applied here even though this isn't an auth helper) — only the fact and
/// shape of a failure is ever included in `Display` text or `tracing` logs.
#[derive(Debug, thiserror::Error)]
pub enum SummarizeError {
    /// The `claude` binary could not be resolved on `PATH`, failed the
    /// ownership/world-writable permission check, or could not be spawned.
    #[error("failed to run summarizer subprocess: {0}")]
    SpawnFailed(String),
    /// The subprocess exited with a non-zero status.
    #[error("summarizer subprocess exited with status {0:?}")]
    NonZeroExit(Option<i32>),
    /// The subprocess did not complete within the configured timeout.
    #[error("summarizer subprocess timed out after {0:?}")]
    Timeout(Duration),
    /// stdout could not be parsed into the expected `<summary>` blocks.
    #[error("summarizer subprocess produced unparseable output: {0}")]
    UnparseableOutput(String),
    /// stdout was not the JSON envelope `--output-format json` promises
    /// (missing/malformed `result` or `total_cost_usd` fields).
    #[error("summarizer subprocess produced malformed JSON envelope: {0}")]
    MalformedJsonEnvelope(String),
}

/// The shape of `claude -p --output-format json`'s stdout: the assistant's
/// final response text plus the real, live-measured USD cost of the call.
/// Only the fields this module needs are modeled — `usage` and other
/// envelope fields are ignored.
#[derive(Debug, serde::Deserialize)]
struct ClaudeCliJsonEnvelope {
    result: String,
    total_cost_usd: f64,
}

/// Resolves `command` to a path: as given if it contains a `/`, else the
/// first `PATH` entry that has it.
///
/// Deliberately duplicates `auth/exec.rs::resolve_command`'s logic exactly,
/// rather than sharing a helper across the two subsystems — small, stable,
/// and not worth a premature abstraction (plan.md, Task 3.2.1a).
fn resolve_command(command: &str) -> Result<PathBuf, SummarizeError> {
    if command.contains('/') {
        return Ok(PathBuf::from(command));
    }
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(command))
                .find(|p| p.is_file())
        })
        .ok_or_else(|| {
            SummarizeError::SpawnFailed(format!("command {command:?} not found on PATH"))
        })
}

/// ADR-007 §5's permission check, duplicated from `auth/exec.rs` for the
/// same reason as [`resolve_command`]: the summarizer binary must be owned
/// by the current user and not world-writable.
fn check_permissions(path: &Path) -> Result<(), SummarizeError> {
    let meta = std::fs::metadata(path)
        .map_err(|e| SummarizeError::SpawnFailed(format!("cannot stat {}: {e}", path.display())))?;

    // SAFETY: geteuid() takes no arguments and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if meta.uid() != euid {
        return Err(SummarizeError::SpawnFailed(format!(
            "{} is not owned by the current user",
            path.display()
        )));
    }
    if meta.permissions().mode() & 0o002 != 0 {
        return Err(SummarizeError::SpawnFailed(format!(
            "{} is world-writable",
            path.display()
        )));
    }
    Ok(())
}

/// The v1 [`Summarizer`] implementation (ADR-010): shells out to
/// `claude -p --resume <session_id>` and parses `<summary>` tags out of its
/// stdout.
pub struct ClaudeCliSummarizer {
    /// Explicit path to the `claude` binary, or `None` to resolve it via
    /// `PATH` at call time.
    command: Option<PathBuf>,
    /// Subprocess wait timeout.
    timeout: Duration,
}

impl ClaudeCliSummarizer {
    /// Default subprocess timeout — matches magic-compact's own end-to-end
    /// hook timeout budget (`research/features.md` §7), not an arbitrary
    /// guess.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(150);

    /// Constructs a summarizer that resolves `claude` via `PATH` at call
    /// time when `command` is `None`.
    #[must_use]
    pub fn new(command: Option<PathBuf>) -> Self {
        Self {
            command,
            timeout: Self::DEFAULT_TIMEOUT,
        }
    }

    /// Overrides the default subprocess timeout — primarily for tests that
    /// need a deterministic, fast timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[async_trait]
impl Summarizer for ClaudeCliSummarizer {
    async fn summarize(
        &self,
        session_id: &str,
        turns: &[Turn],
    ) -> anyhow::Result<SummarizeOutcome> {
        let resolved = match &self.command {
            Some(path) => path.clone(),
            None => resolve_command("claude")?,
        };
        check_permissions(&resolved)?;

        let mut child = tokio::process::Command::new(&resolved)
            .arg("-p")
            .arg("--resume")
            .arg(session_id)
            .arg("--output-format")
            .arg("json")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| {
                SummarizeError::SpawnFailed(format!("failed to spawn {}: {e}", resolved.display()))
            })?;

        // `claude -p --resume` doesn't read a request from stdin (unlike
        // the ADR-007 exec-helper protocol) — drop it immediately so the
        // subprocess never blocks waiting for input that will never come.
        drop(child.stdin.take());

        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_elapsed| SummarizeError::Timeout(self.timeout))?
            .map_err(|e| {
                SummarizeError::SpawnFailed(format!(
                    "failed to wait on {}: {e}",
                    resolved.display()
                ))
            })?;

        if !output.status.success() {
            tracing::warn!(
                command = %resolved.display(),
                exit_code = output.status.code(),
                "claude summarizer subprocess exited non-zero (output redacted)"
            );
            return Err(SummarizeError::NonZeroExit(output.status.code()).into());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let envelope: ClaudeCliJsonEnvelope = serde_json::from_str(&stdout)
            .map_err(|e| SummarizeError::MalformedJsonEnvelope(e.to_string()))?;

        let summaries = parse_summaries(&envelope.result, turns)
            .map_err(|e| SummarizeError::UnparseableOutput(e.to_string()))?;

        Ok(SummarizeOutcome {
            summaries,
            real_cost_usd: Some(envelope.total_cost_usd),
        })
    }
}

/// Parses `<summary>...</summary>` tags out of `stdout`, matching each
/// block to its corresponding turn by order — one `<summary>` tag per turn,
/// in the order they appear in stdout, matched positionally to `turns`
/// (mirrors magic-compact's `parseSummaries`, `compact.ts:518-554`).
///
/// # Errors
///
/// Returns an error if the number of well-formed `<summary>...</summary>`
/// blocks found in `stdout` doesn't match `turns.len()`.
pub fn parse_summaries(stdout: &str, turns: &[Turn]) -> anyhow::Result<Vec<TurnSummary>> {
    const OPEN: &str = "<summary>";
    const CLOSE: &str = "</summary>";

    let mut blocks = Vec::new();
    let mut rest = stdout;
    while let Some(start) = rest.find(OPEN) {
        let after_open = &rest[start + OPEN.len()..];
        let Some(end) = after_open.find(CLOSE) else {
            break;
        };
        blocks.push(after_open[..end].trim().to_string());
        rest = &after_open[end + CLOSE.len()..];
    }

    if blocks.len() != turns.len() {
        return Err(anyhow::anyhow!(
            "expected {} <summary> block(s) matching {} turn(s), found {}",
            turns.len(),
            turns.len(),
            blocks.len()
        ));
    }

    Ok(blocks
        .into_iter()
        .zip(turns)
        .map(|(summary_text, turn)| TurnSummary {
            covers_turn_uuids: vec![turn.user_row.uuid().to_string()],
            summary_text,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // test assertions on well-formed fixtures
mod tests {
    use super::*;
    use crate::claude_code_session::transcript::{RowFields, TranscriptRow};
    use serde_json::Map;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn make_turn(user_uuid: &str) -> Turn {
        let fields = RowFields {
            uuid: user_uuid.to_string(),
            parent_uuid: None,
            is_sidechain: false,
            is_meta: false,
            message: None,
            extra: Map::new(),
        };
        Turn {
            user_row: TranscriptRow::User(fields),
            assistant_rows: Vec::new(),
            tool_rows: Vec::new(),
        }
    }

    // -- FakeSummarizer --

    #[tokio::test]
    async fn fake_summarizer_should_return_canned_summaries_when_configured_with_summaries() {
        let canned = vec![TurnSummary {
            covers_turn_uuids: vec!["u1".to_string()],
            summary_text: "canned".to_string(),
        }];
        let summarizer = FakeSummarizer::with_summaries(canned.clone());

        let result = summarizer.summarize("any-session", &[]).await.unwrap();

        assert_eq!(result.summaries, canned);
        assert_eq!(result.real_cost_usd, None);
    }

    #[tokio::test]
    async fn fake_summarizer_should_return_canned_cost_when_configured_with_summaries_and_cost() {
        let canned = vec![TurnSummary {
            covers_turn_uuids: vec!["u1".to_string()],
            summary_text: "canned".to_string(),
        }];
        let summarizer = FakeSummarizer::with_summaries_and_cost(canned.clone(), 0.0042);

        let result = summarizer.summarize("any-session", &[]).await.unwrap();

        assert_eq!(result.summaries, canned);
        assert_eq!(result.real_cost_usd, Some(0.0042));
    }

    #[tokio::test]
    async fn fake_summarizer_should_return_err_when_configured_to_fail() {
        let summarizer = FakeSummarizer::failing("boom");

        let result = summarizer.summarize("any-session", &[]).await;

        assert!(result.is_err());
    }

    // -- parse_summaries --

    #[test]
    fn parse_summaries_should_extract_summary_per_turn_group_when_stdout_has_multiple_summary_tags()
    {
        let turns = vec![make_turn("u1"), make_turn("u2")];
        let stdout = "preamble chatter\n<summary>first summary</summary>\nsome filler text\n<summary>second summary</summary>\ntrailing chatter";

        let summaries = parse_summaries(stdout, &turns).unwrap();

        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].summary_text, "first summary");
        assert_eq!(summaries[0].covers_turn_uuids, vec!["u1".to_string()]);
        assert_eq!(summaries[1].summary_text, "second summary");
        assert_eq!(summaries[1].covers_turn_uuids, vec!["u2".to_string()]);
    }

    #[test]
    fn parse_summaries_should_return_err_when_summary_count_does_not_match_turn_count() {
        let turns = vec![make_turn("u1"), make_turn("u2")];
        let stdout = "<summary>only one</summary>";

        let result = parse_summaries(stdout, &turns);

        assert!(result.is_err());
    }

    // -- ClaudeCliSummarizer --

    /// Writes an executable shell script (owned by the current user, not
    /// world-writable) that sleeps regardless of the args it's invoked
    /// with — standing in for a `claude` binary that hangs, to trigger
    /// `SummarizeError::Timeout` deterministically without depending on the
    /// real CLI.
    fn write_slow_fake_claude() -> NamedTempFile {
        let mut script = NamedTempFile::new().unwrap();
        writeln!(script, "#!/bin/sh\nsleep 5\n").unwrap();
        let mut perms = std::fs::metadata(script.path()).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(script.path(), perms).unwrap();
        script
    }

    /// Writes an executable shell script standing in for `claude -p
    /// --output-format json`: it ignores its args and prints a canned JSON
    /// envelope to stdout, letting us exercise
    /// [`ClaudeCliSummarizer::summarize`]'s envelope-parsing path without
    /// depending on the real CLI.
    fn write_fake_claude_json(result: &str, total_cost_usd: f64) -> NamedTempFile {
        let envelope = serde_json::json!({ "result": result, "total_cost_usd": total_cost_usd });
        let mut script = NamedTempFile::new().unwrap();
        writeln!(
            script,
            "#!/bin/sh\ncat <<'CONSOLETTE_EOF'\n{}\nCONSOLETTE_EOF\n",
            serde_json::to_string(&envelope).unwrap()
        )
        .unwrap();
        let mut perms = std::fs::metadata(script.path()).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(script.path(), perms).unwrap();
        script
    }

    #[tokio::test]
    async fn claude_cli_summarizer_should_return_real_cost_and_summaries_from_json_envelope() {
        let script = write_fake_claude_json("<summary>fake summary</summary>", 0.0123);
        let summarizer = ClaudeCliSummarizer::new(Some(script.path().to_path_buf()));
        let turns = vec![make_turn("u1")];

        let outcome = summarizer.summarize("fake-session", &turns).await.unwrap();

        assert_eq!(outcome.real_cost_usd, Some(0.0123));
        assert_eq!(outcome.summaries.len(), 1);
        assert_eq!(outcome.summaries[0].summary_text, "fake summary");
    }

    #[tokio::test]
    async fn claude_cli_summarizer_should_error_when_stdout_is_not_json() {
        let mut script = NamedTempFile::new().unwrap();
        writeln!(script, "#!/bin/sh\necho 'not json'\n").unwrap();
        let mut perms = std::fs::metadata(script.path()).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(script.path(), perms).unwrap();
        let summarizer = ClaudeCliSummarizer::new(Some(script.path().to_path_buf()));

        let result = summarizer.summarize("fake-session", &[]).await;

        let error = result.expect_err("expected a malformed-envelope error");
        let summarize_error = error
            .downcast_ref::<SummarizeError>()
            .expect("expected a SummarizeError");
        assert!(matches!(
            summarize_error,
            SummarizeError::MalformedJsonEnvelope(_)
        ));
    }

    #[tokio::test]
    async fn claude_cli_summarizer_should_return_timeout_error_when_subprocess_exceeds_timeout() {
        let script = write_slow_fake_claude();
        let summarizer = ClaudeCliSummarizer::new(Some(script.path().to_path_buf()))
            .with_timeout(Duration::from_millis(50));

        let result = summarizer.summarize("fake-session", &[]).await;

        let error = result.expect_err("expected a timeout error");
        let summarize_error = error
            .downcast_ref::<SummarizeError>()
            .expect("expected a SummarizeError");
        assert!(matches!(summarize_error, SummarizeError::Timeout(_)));

        let display = format!("{summarize_error}");
        assert!(!display.contains("stdout"));
        assert!(!display.contains("stderr"));
    }

    #[tokio::test]
    async fn claude_cli_summarizer_should_invoke_real_claude_cli_when_live_test_env_var_set() {
        if std::env::var("COMPACTION_HOOK_LIVE_CLAUDE_TEST").as_deref() != Ok("1") {
            eprintln!(
                "skipping live claude CLI test (set COMPACTION_HOOK_LIVE_CLAUDE_TEST=1 to run it)"
            );
            return;
        }

        let summarizer = ClaudeCliSummarizer::new(None);
        let turns = vec![make_turn("live-turn-1")];

        // This exercises the real subprocess path end-to-end when opted
        // in. A fake session ID will most likely make `claude` fail (no
        // such session to resume), so this doesn't assert success — only
        // that invoking it doesn't panic and produces a typed result
        // either way.
        match summarizer
            .summarize("consolette-live-test-nonexistent-session", &turns)
            .await
        {
            Ok(outcome) => {
                eprintln!(
                    "live claude CLI test produced {} summaries, real_cost_usd={:?}",
                    outcome.summaries.len(),
                    outcome.real_cost_usd
                );
            }
            Err(error) => {
                eprintln!(
                    "live claude CLI test observed error (expected for a fake session id): {error}"
                );
            }
        }
    }
}
