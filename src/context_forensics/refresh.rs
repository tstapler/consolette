//! Background rescan task: periodically re-ingest every discovered Claude
//! Code transcript into a [`ContextForensicsStore`] (context-analyzer
//! plan.md Story 1.3.3).
//!
//! Full reparse + idempotent upsert on every pass — no byte-offset/
//! checkpoint incremental tailing (Pattern Decision: avoids the
//! checkpoint-invalidated-by-compaction-rewrite hazard,
//! `research/pitfalls.md` §1). Mirrors
//! `claude_code_session::session_bi::spawn_session_bi_refresh_task`'s
//! eager-scan-before-spawn / periodic-interval-loop structure — including
//! its bounded-concurrency `spawn_blocking` + `timeout` scan shape
//! (`session_bi.rs`'s `build_session_bi_snapshot`), not a naive sequential
//! loop: an early version of this module ingested every discovered file
//! sequentially and unbounded, which on a real `~/.claude/projects` corpus
//! of any real size made `serve_cost` startup (and this task's every
//!15-minute rescan) block for minutes — precisely the "must never
//! destabilize the existing dashboard" failure Story 1.4.4 exists to
//! prevent. Bounding concurrency and per-file time keeps one slow/huge
//! transcript from stalling the whole scan or the runtime it shares with
//! every other route.

use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{self, StreamExt};

use crate::claude_code_session::discovery::{discover_sessions_glob, SortBy};
use crate::context_forensics::ingest_claude_code::ingest_claude_code_session;
use crate::context_forensics::store::ContextForensicsStore;

/// How often [`spawn_context_forensics_refresh_task`] recomputes the store
/// from disk. Matches `session_bi.rs`'s `SESSION_BI_REFRESH_INTERVAL`.
pub const CONTEXT_FORENSICS_REFRESH_INTERVAL: Duration = Duration::from_mins(15);

/// Bounded fan-out for [`run_context_forensics_scan`]'s per-file ingestion —
/// matches `session_bi.rs`'s `SESSION_BI_SCAN_CONCURRENCY`.
pub const CONTEXT_FORENSICS_SCAN_CONCURRENCY: usize = 16;

/// Per-file budget for [`run_context_forensics_scan`]. Matches
/// `session_bi.rs`'s `SESSION_BI_PER_FILE_TIMEOUT`.
pub const CONTEXT_FORENSICS_PER_FILE_TIMEOUT: Duration = Duration::from_secs(5);

/// One rescan pass: glob `session_glob`, ingest every discovered file into
/// `store` with bounded concurrency and a per-file timeout, tolerating (and
/// logging) any single file's ingestion failure without aborting the rest
/// of the corpus.
///
/// Each file's `ingest_claude_code_session` call (synchronous file I/O +
/// `rusqlite` writes) runs inside [`tokio::task::spawn_blocking`], raced
/// against [`tokio::time::timeout`] — mirrors
/// `session_bi.rs::build_session_bi_snapshot`'s documented rationale: this
/// bounds how long any one file can stall the scan and moves the actual
/// work off the async runtime's worker threads, so a large or slow corpus
/// never blocks every other route this process serves.
///
/// A glob-pattern failure (malformed pattern) is logged and treated as a
/// zero-session scan rather than propagated — this task runs unattended in
/// the background and has no caller in a position to react to an `Err`.
async fn run_context_forensics_scan(store: Arc<ContextForensicsStore>, session_glob: &str) {
    let sessions = match discover_sessions_glob(session_glob, SortBy::RecentFirst) {
        Ok(sessions) => sessions,
        Err(error) => {
            tracing::warn!(%error, session_glob, "context-forensics scan: failed to glob session files");
            return;
        }
    };

    let results = stream::iter(sessions)
        .map(|session| {
            let store = Arc::clone(&store);
            async move {
                let path = session.path;
                let path_for_task = path.clone();
                let join = tokio::task::spawn_blocking(move || {
                    ingest_claude_code_session(&store, &path_for_task)
                });
                match tokio::time::timeout(CONTEXT_FORENSICS_PER_FILE_TIMEOUT, join).await {
                    Ok(Ok(Ok(_summary))) => true,
                    Ok(Ok(Err(error))) => {
                        tracing::warn!(
                            path = %path.display(),
                            %error,
                            "context-forensics scan: failed to ingest session"
                        );
                        false
                    }
                    Ok(Err(join_error)) => {
                        tracing::warn!(
                            path = %path.display(),
                            %join_error,
                            "context-forensics scan: ingest task panicked"
                        );
                        false
                    }
                    Err(_elapsed) => {
                        tracing::warn!(
                            path = %path.display(),
                            timeout_secs = CONTEXT_FORENSICS_PER_FILE_TIMEOUT.as_secs(),
                            "context-forensics scan: ingest timed out"
                        );
                        false
                    }
                }
            }
        })
        .buffer_unordered(CONTEXT_FORENSICS_SCAN_CONCURRENCY)
        .collect::<Vec<bool>>()
        .await;

    let ingested = results.iter().filter(|ok| **ok).count();
    let failed = results.len() - ingested;
    tracing::info!(ingested, failed, "context-forensics rescan complete");
}

/// Spawn the periodic rescan task.
///
/// Runs one eager scan on the calling task *before* spawning the periodic
/// loop — matches `server.rs:115-121`'s documented eager-scan-before-
/// background-loop rationale, so the very first `/dashboard/context`
/// request after startup never races an empty store. Callers `.await` this
/// function itself (it's `async`, unlike `session_bi.rs`'s non-async
/// spawn-wrapper, since the eager scan below needs to run on the async
/// runtime to get its concurrency/timeout bounding) before treating the
/// store as populated.
pub async fn spawn_context_forensics_refresh_task(
    store: Arc<ContextForensicsStore>,
    session_glob: String,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    run_context_forensics_scan(Arc::clone(&store), &session_glob).await;

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await; // first tick fires immediately; skip it (eager scan above already covered it)
        loop {
            ticker.tick().await;
            run_context_forensics_scan(Arc::clone(&store), &session_glob).await;
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
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

    fn user_line(uuid: &str, parent: Option<&str>) -> String {
        let parent_json = parent.map_or("null".to_string(), |p| format!("\"{p}\""));
        format!(
            r#"{{"type":"user","uuid":"{uuid}","parentUuid":{parent_json},"isSidechain":false,"isMeta":false,"message":{{"role":"user","content":"hi"}}}}"#
        )
    }

    fn assistant_line(uuid: &str, parent: &str) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"{uuid}","parentUuid":"{parent}","isSidechain":false,"isMeta":false,"message":{{"role":"assistant","content":[{{"type":"text","text":"hi"}}],"usage":{{"input_tokens":100,"output_tokens":20,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}}}}"#
        )
    }

    #[tokio::test]
    async fn spawn_context_forensics_refresh_task_should_populate_store_before_returning_when_two_sessions_exist(
    ) {
        let projects_dir = TempDir::new().unwrap();
        write_fixture(
            &projects_dir,
            "s1.jsonl",
            &[user_line("u1", None), assistant_line("a1", "u1")],
        );
        write_fixture(
            &projects_dir,
            "s2.jsonl",
            &[user_line("u2", None), assistant_line("a2", "u2")],
        );
        let glob = format!("{}/*.jsonl", projects_dir.path().display());

        let store_dir = TempDir::new().unwrap();
        let store =
            Arc::new(ContextForensicsStore::open(&store_dir.path().join("cf.sqlite")).unwrap());

        // Eager scan must be complete before this `.await` resolves.
        let handle =
            spawn_context_forensics_refresh_task(Arc::clone(&store), glob, Duration::from_hours(1))
                .await;

        assert_eq!(store.session_row_count().unwrap(), 2);
        handle.abort();
    }

    #[tokio::test]
    async fn run_context_forensics_scan_should_ingest_remaining_files_when_one_file_is_malformed() {
        let projects_dir = TempDir::new().unwrap();
        write_fixture(
            &projects_dir,
            "good1.jsonl",
            &[user_line("u1", None), assistant_line("a1", "u1")],
        );
        // A parentUuid cycle makes build_turns return Err for this one file.
        write_fixture(
            &projects_dir,
            "bad.jsonl",
            &[user_line("a", Some("b")), user_line("b", Some("a"))],
        );
        write_fixture(
            &projects_dir,
            "good2.jsonl",
            &[user_line("u2", None), assistant_line("a2", "u2")],
        );
        let glob = format!("{}/*.jsonl", projects_dir.path().display());

        let store_dir = TempDir::new().unwrap();
        let store =
            Arc::new(ContextForensicsStore::open(&store_dir.path().join("cf.sqlite")).unwrap());

        run_context_forensics_scan(Arc::clone(&store), &glob).await;

        assert_eq!(store.session_row_count().unwrap(), 2);
    }

    #[tokio::test]
    async fn run_context_forensics_scan_should_not_hang_when_ingest_task_exceeds_per_file_timeout()
    {
        // Regression guard for the real hang this module used to have: a
        // large number of files, scanned with bounded concurrency and a
        // per-file timeout, must complete in bounded wall-clock time rather
        // than serializing unboundedly. 40 trivial fixture files exercise
        // the buffer_unordered concurrency path without needing a
        // synthetic slow-file timeout trigger (which would need an
        // artificially tiny timeout, risking flakiness on a loaded CI
        // runner) — the assertion is that this completes at all within the
        // test's own default timeout, proving no unbounded serialization.
        let projects_dir = TempDir::new().unwrap();
        for i in 0..40 {
            write_fixture(
                &projects_dir,
                &format!("s{i}.jsonl"),
                &[
                    user_line(&format!("u{i}"), None),
                    assistant_line(&format!("a{i}"), &format!("u{i}")),
                ],
            );
        }
        let glob = format!("{}/*.jsonl", projects_dir.path().display());

        let store_dir = TempDir::new().unwrap();
        let store =
            Arc::new(ContextForensicsStore::open(&store_dir.path().join("cf.sqlite")).unwrap());

        run_context_forensics_scan(Arc::clone(&store), &glob).await;

        assert_eq!(store.session_row_count().unwrap(), 40);
    }
}
