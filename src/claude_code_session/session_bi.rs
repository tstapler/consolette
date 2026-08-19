//! Fleet-wide session business-intelligence aggregation: one comparison row
//! per discovered session transcript, cached and periodically refreshed for
//! the `GET /dashboard` / `GET /v1/dashboard/sessions` routes
//! (see `src/cost_metrics/server.rs`).
//!
//! Aggregation is read-only: nothing in this module writes to, or mutates,
//! any session transcript. It only ever globs, parses, and estimates.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};

use crate::claude_code_session::cost_compare::compare_compaction_cost;
use crate::claude_code_session::discovery::{discover_sessions_glob, SessionFile, SortBy};

/// Bounded fan-out for [`build_session_bi_snapshot`]'s per-file scan —
/// keeps a large corpus (thousands of transcripts) from spawning unbounded
/// concurrent work.
pub const SESSION_BI_SCAN_CONCURRENCY: usize = 16;

/// Per-file budget for [`build_session_bi_snapshot`]'s scan. See that
/// function's doc comment for exactly what this does and does not bound.
pub const SESSION_BI_PER_FILE_TIMEOUT: Duration = Duration::from_secs(5);

/// How often [`spawn_session_bi_refresh_task`] recomputes the snapshot.
pub const SESSION_BI_REFRESH_INTERVAL: Duration = Duration::from_mins(15);

/// A session's compaction status, as a single sum type rather than two
/// independent booleans — `(native, consolette)` combinations are
/// meaningful states in their own right (e.g. "both" isn't just the AND of
/// two unrelated facts, it's the interesting case where either compactor
/// alone would have under-reported savings).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionStatus {
    NativeOnly,
    ConsoletteOnly,
    Both,
    Neither,
}

impl CompactionStatus {
    #[must_use]
    pub fn from_flags(native: bool, consolette: bool) -> Self {
        match (native, consolette) {
            (true, true) => Self::Both,
            (true, false) => Self::NativeOnly,
            (false, true) => Self::ConsoletteOnly,
            (false, false) => Self::Neither,
        }
    }
}

/// One row of the session BI table: one session transcript, its compaction
/// status, and its estimated cost/token figures.
///
/// This struct is serialized directly to the `/v1/dashboard/sessions` JSON
/// response consumed by `dashboard.html` — a field rename here is also a
/// wire-format change for that hand-written JS (`row.foo` field accesses),
/// with no compiler check tying the two together.
///
/// `native_tokens_saved` and `consolette_tokens_saved` are intentionally
/// independent of `status`: `status` records *which* compactor(s) touched
/// this session, while these two fields record *how much* each one saved
/// (or `None` when that compactor never ran) — the two aren't redundant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionComparisonRow {
    pub session_path: String,
    pub session_id: String,
    pub project: String,
    pub size_bytes: u64,
    pub modified_unix_secs: u64,
    pub status: CompactionStatus,
    pub native_event_count: usize,
    pub native_tokens_saved: Option<i64>,
    pub consolette_tokens_saved: Option<i64>,
    pub net_advantage_tokens: Option<i64>,
    pub no_compaction_total_tokens: u64,
    pub no_compaction_estimated_cost_usd: Option<f64>,
    pub chain_coverage_ratio: f64,
}

/// One session that failed to parse/estimate during a
/// [`build_session_bi_snapshot`] scan — a name and a human-readable reason,
/// not a typed error category (only an aggregate count is surfaced to the
/// dashboard today; see `handler_dashboard_sessions`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionComparisonError {
    pub session_path: String,
    pub reason: String,
}

/// A full BI scan's results: successfully aggregated rows, sessions that
/// failed, and when the scan ran.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionBiSnapshot {
    pub rows: Vec<SessionComparisonRow>,
    pub parse_failures: Vec<SessionComparisonError>,
    pub generated_at: chrono::DateTime<chrono::Utc>,
}

/// Extract the project directory name immediately under
/// `.claude/projects/` from a session file path, falling back to `"unknown"`
/// when the path doesn't contain that segment (e.g. a fixture path used in
/// tests).
fn project_from_path(path: &Path) -> String {
    let components: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    components
        .iter()
        .position(|&c| c == "projects")
        .and_then(|idx| components.get(idx + 1))
        .map_or_else(|| "unknown".to_string(), std::string::ToString::to_string)
}

/// Build one [`SessionComparisonRow`] from a discovered session file, by
/// delegating to [`compare_compaction_cost`] for the actual parsing and
/// estimation.
///
/// # Errors
///
/// Returns [`SessionComparisonError`] (not `anyhow::Error`, since this is a
/// per-row failure meant to be collected alongside successful rows, not
/// propagated) when `compare_compaction_cost` fails — e.g. the file can't
/// be opened, or a turn's tokens can't be estimated.
pub async fn build_session_comparison_row(
    file: &SessionFile,
    pricing_model: &str,
) -> Result<SessionComparisonRow, SessionComparisonError> {
    let comparison = compare_compaction_cost(&file.path, pricing_model)
        .await
        .map_err(|e| SessionComparisonError {
            session_path: file.path.display().to_string(),
            reason: e.to_string(),
        })?;

    // `None` (unknown), not `Some(0)`, when any event's `tokens_saved()` is
    // unknown (missing preTokens/postTokens) — `filter_map` would otherwise
    // silently treat "unknown" as "contributed nothing" and understate the
    // sum instead of reporting it as unavailable.
    let native_tokens_saved = if comparison.native_events.is_empty() {
        None
    } else {
        comparison
            .native_events
            .iter()
            .map(super::native_compaction::NativeCompactionEvent::tokens_saved)
            .sum::<Option<i64>>()
    };

    let consolette_tokens_saved = if comparison.compaction_metrics.is_empty() {
        None
    } else {
        Some(
            comparison
                .compaction_metrics
                .iter()
                .map(|m| m.tokens_saved)
                .sum::<i64>(),
        )
    };

    let net_advantage_tokens = match (consolette_tokens_saved, native_tokens_saved) {
        (Some(c), Some(n)) => Some(c - n),
        _ => None,
    };

    let session_id = file
        .path
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("unknown")
        .to_string();

    Ok(SessionComparisonRow {
        session_path: file.path.display().to_string(),
        session_id,
        project: project_from_path(&file.path),
        size_bytes: file.size_bytes,
        modified_unix_secs: file
            .modified
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        status: CompactionStatus::from_flags(
            comparison.is_native_compacted,
            comparison.is_compacted,
        ),
        native_event_count: comparison.native_events.len(),
        native_tokens_saved,
        consolette_tokens_saved,
        net_advantage_tokens,
        no_compaction_total_tokens: comparison.no_compaction.total_tokens,
        no_compaction_estimated_cost_usd: comparison.no_compaction.estimated_cost_usd,
        chain_coverage_ratio: comparison.chain_coverage.ratio(),
    })
}

/// Scan every session transcript matching `session_glob`, aggregating each
/// into a [`SessionComparisonRow`] (or a [`SessionComparisonError`] on
/// failure), with bounded concurrency and a per-file timeout.
///
/// # Hardening: why the per-file work is wrapped in `spawn_blocking`
///
/// [`build_session_comparison_row`]'s call chain
/// (`parse_session_file`/`build_turns`) is synchronous CPU/IO work with no
/// `.await` inside it, and `compare_compaction_cost` itself does have a
/// genuine `.await` per turn (`estimate_turn_tokens(...).await`) — so
/// wrapping only in [`tokio::time::timeout`] cannot actually preempt a
/// pathological file: a `tokio::time::timeout` only gets a chance to fire
/// at an `.await` point inside the wrapped future, and a future that spends
/// most of its time in synchronous code never yields control back to the
/// executor for the timeout to race against.
///
/// The fix here is to run the whole per-file unit of work — sync prefix and
/// per-turn `.await`s alike — inside [`tokio::task::spawn_blocking`], via
/// `tokio::runtime::Handle::current().block_on(..)` inside the blocking
/// closure (the documented way to bridge sync and async code from a
/// blocking-pool thread — see Tokio's own guidance on `spawn_blocking`), and
/// race [`tokio::time::timeout`] against the resulting `JoinHandle`'s `.await`
/// instead of against the inner future directly. A `JoinHandle` **is** a
/// proper future the runtime can poll and abandon on timeout, so this
/// bounds how long an axum worker thread waits for one file, and it moves
/// the actual scan work off the worker threads entirely (onto Tokio's
/// separate blocking thread pool), so 16 concurrent scans no longer starve
/// the `/v1/cost/{session_key}` route on the main runtime.
///
/// This is *not* airtight: if the underlying work truly hangs (e.g. a stuck
/// syscall), `tokio::time::timeout` abandons waiting for it, but the
/// blocking thread itself is not killed — it leaks for the lifetime of the
/// process. What this achieves is bounding worker-thread stalls and giving
/// every other route a responsive server even when one file misbehaves, not
/// guaranteeing the leaked thread is ever reclaimed.
///
/// # Errors
///
/// Never returns `Err` — a glob pattern error or scan failure is reported
/// as a single [`SessionComparisonError`] inside the returned snapshot's
/// `parse_failures`, so callers (the refresh task, `build_with_session_glob`)
/// always get a usable snapshot.
pub async fn build_session_bi_snapshot(
    session_glob: &str,
    pricing_model: &str,
    concurrency: usize,
    per_file_timeout: Duration,
) -> SessionBiSnapshot {
    let files = match discover_sessions_glob(session_glob, SortBy::RecentFirst) {
        Ok(files) => files,
        Err(e) => {
            return SessionBiSnapshot {
                rows: Vec::new(),
                parse_failures: vec![SessionComparisonError {
                    session_path: "<scan>".to_string(),
                    reason: e.to_string(),
                }],
                generated_at: chrono::Utc::now(),
            };
        }
    };

    let pricing_model = pricing_model.to_string();
    let results = stream::iter(files)
        .map(|file| {
            let pricing_model = pricing_model.clone();
            async move {
                let session_path = file.path.display().to_string();
                let handle = tokio::runtime::Handle::current();
                let join = tokio::task::spawn_blocking(move || {
                    handle.block_on(build_session_comparison_row(&file, &pricing_model))
                });
                match tokio::time::timeout(per_file_timeout, join).await {
                    Ok(Ok(row_result)) => row_result,
                    Ok(Err(join_error)) => Err(SessionComparisonError {
                        session_path,
                        reason: format!("scan task panicked: {join_error}"),
                    }),
                    Err(_elapsed) => Err(SessionComparisonError {
                        session_path,
                        reason: format!("timed out after {}s", per_file_timeout.as_secs()),
                    }),
                }
            }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;

    let mut rows = Vec::new();
    let mut parse_failures = Vec::new();
    for result in results {
        match result {
            Ok(row) => rows.push(row),
            Err(error) => parse_failures.push(error),
        }
    }

    SessionBiSnapshot {
        rows,
        parse_failures,
        generated_at: chrono::Utc::now(),
    }
}

/// Spawn a background task that recomputes the session BI snapshot every
/// `interval` and publishes it via `tx`.
///
/// The first tick fires immediately and is discarded: the caller
/// (`CostServerState::build_with_session_glob`) already computes an eager
/// initial snapshot synchronously before this task is spawned (per
/// ADR-014), so this loop only ever produces the *second* snapshot onward.
#[must_use]
pub fn spawn_session_bi_refresh_task(
    tx: tokio::sync::watch::Sender<Arc<SessionBiSnapshot>>,
    session_glob: String,
    pricing_model: String,
    interval: Duration,
    concurrency: usize,
    per_file_timeout: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await; // first tick fires immediately; skip it
        loop {
            ticker.tick().await;
            let snapshot = build_session_bi_snapshot(
                &session_glob,
                &pricing_model,
                concurrency,
                per_file_timeout,
            )
            .await;
            tx.send_replace(Arc::new(snapshot));
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_fixture(dir: &TempDir, name: &str, lines: &[String]) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut f = fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        path
    }

    fn user_row(uuid: &str, parent: Option<&str>, text: &str) -> String {
        let parent_json = parent.map_or_else(|| "null".to_string(), |p| format!("\"{p}\""));
        format!(
            r#"{{"uuid":"{uuid}","parentUuid":{parent_json},"type":"user","timestamp":"2024-01-01T00:00:00Z","message":{{"role":"user","content":"{text}"}}}}"#
        )
    }

    fn assistant_row(uuid: &str, parent: Option<&str>, text: &str) -> String {
        let parent_json = parent.map_or_else(|| "null".to_string(), |p| format!("\"{p}\""));
        format!(
            r#"{{"uuid":"{uuid}","parentUuid":{parent_json},"type":"assistant","timestamp":"2024-01-01T00:00:01Z","message":{{"role":"assistant","content":"{text}"}}}}"#
        )
    }

    #[test]
    fn compaction_status_from_flags_should_cover_all_four_combinations() {
        assert_eq!(
            CompactionStatus::from_flags(true, true),
            CompactionStatus::Both
        );
        assert_eq!(
            CompactionStatus::from_flags(true, false),
            CompactionStatus::NativeOnly
        );
        assert_eq!(
            CompactionStatus::from_flags(false, true),
            CompactionStatus::ConsoletteOnly
        );
        assert_eq!(
            CompactionStatus::from_flags(false, false),
            CompactionStatus::Neither
        );
    }

    #[test]
    fn compaction_status_should_serialize_to_snake_case_wire_values() {
        assert_eq!(
            serde_json::to_string(&CompactionStatus::NativeOnly).unwrap(),
            "\"native_only\""
        );
        assert_eq!(
            serde_json::to_string(&CompactionStatus::ConsoletteOnly).unwrap(),
            "\"consolette_only\""
        );
        assert_eq!(
            serde_json::to_string(&CompactionStatus::Both).unwrap(),
            "\"both\""
        );
        assert_eq!(
            serde_json::to_string(&CompactionStatus::Neither).unwrap(),
            "\"neither\""
        );
    }

    #[tokio::test]
    async fn build_session_comparison_row_should_succeed_for_uncompacted_transcript() {
        let dir = TempDir::new().unwrap();
        let lines = vec![
            user_row("u1", None, "hello"),
            assistant_row("a1", Some("u1"), "hi"),
        ];
        let path = write_fixture(&dir, "session.jsonl", &lines);
        let metadata = fs::metadata(&path).unwrap();
        let file = SessionFile {
            path,
            modified: metadata.modified().unwrap(),
            size_bytes: metadata.len(),
        };

        let row = build_session_comparison_row(&file, "claude-sonnet-5")
            .await
            .unwrap();

        assert_eq!(row.status, CompactionStatus::Neither);
        assert_eq!(row.native_tokens_saved, None);
        assert_eq!(row.consolette_tokens_saved, None);
        assert_eq!(row.net_advantage_tokens, None);
        assert!(row.no_compaction_total_tokens > 0);
    }

    /// Regression test for a real bug: a native event with `tokens_saved()
    /// == None` (missing `preTokens`/`postTokens`) was previously counted as
    /// contributing `0` to the sum (via `filter_map`), so a session with one
    /// such event reported `native_tokens_saved: Some(0)` — indistinguishable
    /// from "confirmed zero tokens saved" — instead of `None` ("unknown").
    /// This also corrupted `net_advantage_tokens`, which is derived from it.
    #[tokio::test]
    async fn build_session_comparison_row_should_report_native_tokens_saved_as_unknown_when_any_event_is_missing_token_counts(
    ) {
        let dir = TempDir::new().unwrap();
        // One native event with full token counts, one with none — the
        // aggregate must be `None`, not `Some(<partial sum>)`.
        let lines = vec![
            r#"{"uuid":"n1","parentUuid":null,"type":"system","subtype":"compact_boundary","timestamp":"2024-01-01T00:00:00Z","message":null,"compactMetadata":{"trigger":"auto","preTokens":9000,"postTokens":1200}}"#.to_string(),
            user_row("u1", Some("n1"), "hello"),
            r#"{"uuid":"n2","parentUuid":"u1","type":"system","subtype":"compact_boundary","timestamp":"2024-01-01T00:00:02Z","message":null,"compactMetadata":{"trigger":"manual"}}"#.to_string(),
        ];
        let path = write_fixture(&dir, "session.jsonl", &lines);
        let metadata = fs::metadata(&path).unwrap();
        let file = SessionFile {
            path,
            modified: metadata.modified().unwrap(),
            size_bytes: metadata.len(),
        };

        let row = build_session_comparison_row(&file, "claude-sonnet-5")
            .await
            .unwrap();

        assert_eq!(row.native_event_count, 2);
        assert_eq!(row.native_tokens_saved, None);
        assert_eq!(row.net_advantage_tokens, None);
    }

    /// Task 2.1.2b: a nonexistent path — not a garbage-JSON-content file —
    /// is the fixture that actually produces an `Err` here, since
    /// `parse_session_file` skips-and-warns unparseable lines rather than
    /// erroring (see `parse_session_file_should_skip_and_warn_when_line_is_unparseable_json`
    /// in `transcript.rs`); only a genuine `File::open` I/O failure does.
    #[tokio::test]
    async fn build_session_comparison_row_should_error_when_file_does_not_exist() {
        let missing_path = std::path::PathBuf::from("/nonexistent/path/session.jsonl");
        let file = SessionFile {
            path: missing_path.clone(),
            modified: std::time::SystemTime::UNIX_EPOCH,
            size_bytes: 0,
        };

        let error = build_session_comparison_row(&file, "claude-sonnet-5")
            .await
            .unwrap_err();

        assert_eq!(error.session_path, missing_path.display().to_string());
        assert!(
            error.reason.contains("failed to open session file"),
            "unexpected reason: {}",
            error.reason
        );
    }

    #[tokio::test]
    async fn build_session_bi_snapshot_should_aggregate_all_fixture_files_in_glob() {
        let dir = TempDir::new().unwrap();
        write_fixture(
            &dir,
            "a.jsonl",
            &[
                user_row("u1", None, "hi"),
                assistant_row("a1", Some("u1"), "hey"),
            ],
        );
        write_fixture(
            &dir,
            "b.jsonl",
            &[
                user_row("u2", None, "hi"),
                assistant_row("a2", Some("u2"), "hey"),
            ],
        );
        let pattern = format!("{}/*.jsonl", dir.path().display());

        let snapshot = build_session_bi_snapshot(
            &pattern,
            "claude-sonnet-5",
            SESSION_BI_SCAN_CONCURRENCY,
            SESSION_BI_PER_FILE_TIMEOUT,
        )
        .await;

        assert_eq!(snapshot.rows.len(), 2);
        assert!(snapshot.parse_failures.is_empty());
    }

    #[tokio::test]
    async fn build_session_bi_snapshot_should_report_parse_failure_for_unparseable_file() {
        let dir = TempDir::new().unwrap();
        write_fixture(
            &dir,
            "good.jsonl",
            &[
                user_row("u1", None, "hi"),
                assistant_row("a1", Some("u1"), "hey"),
            ],
        );
        // A directory matching the glob but not a real session file: causes
        // `discover_sessions_glob`'s own `fs::metadata` to succeed (it's a
        // valid path) but `parse_session_file`'s `File::open` to fail when
        // treated as a file to read.
        let bogus_dir = dir.path().join("not_a_file.jsonl");
        fs::create_dir(&bogus_dir).unwrap();
        let pattern = format!("{}/*.jsonl", dir.path().display());

        let snapshot = build_session_bi_snapshot(
            &pattern,
            "claude-sonnet-5",
            SESSION_BI_SCAN_CONCURRENCY,
            SESSION_BI_PER_FILE_TIMEOUT,
        )
        .await;

        assert_eq!(snapshot.rows.len(), 1);
        assert_eq!(snapshot.parse_failures.len(), 1);
    }

    #[tokio::test]
    async fn build_session_bi_snapshot_should_report_scan_error_for_invalid_glob() {
        let snapshot = build_session_bi_snapshot(
            "[invalid-glob",
            "claude-sonnet-5",
            SESSION_BI_SCAN_CONCURRENCY,
            SESSION_BI_PER_FILE_TIMEOUT,
        )
        .await;

        assert!(snapshot.rows.is_empty());
        assert_eq!(snapshot.parse_failures.len(), 1);
        assert_eq!(snapshot.parse_failures[0].session_path, "<scan>");
    }

    #[tokio::test]
    async fn spawn_session_bi_refresh_task_should_publish_snapshot_after_interval() {
        let dir = TempDir::new().unwrap();
        write_fixture(
            &dir,
            "a.jsonl",
            &[
                user_row("u1", None, "hi"),
                assistant_row("a1", Some("u1"), "hey"),
            ],
        );
        let pattern = format!("{}/*.jsonl", dir.path().display());

        let (tx, mut rx) = tokio::sync::watch::channel(Arc::new(SessionBiSnapshot {
            rows: Vec::new(),
            parse_failures: Vec::new(),
            generated_at: chrono::Utc::now(),
        }));

        let handle = spawn_session_bi_refresh_task(
            tx,
            pattern,
            "claude-sonnet-5".to_string(),
            Duration::from_millis(20),
            SESSION_BI_SCAN_CONCURRENCY,
            SESSION_BI_PER_FILE_TIMEOUT,
        );

        tokio::time::timeout(Duration::from_secs(5), rx.changed())
            .await
            .expect("expected refresh task to publish a snapshot within 5s")
            .unwrap();

        let snapshot = rx.borrow().clone();
        assert_eq!(snapshot.rows.len(), 1);

        handle.abort();
    }
}
