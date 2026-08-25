//! Background rescan task: periodically re-ingest every discovered Claude
//! Code transcript into a [`ContextForensicsStore`] (context-analyzer
//! plan.md Story 1.3.3).
//!
//! Full reparse + idempotent upsert on every pass — no byte-offset/
//! checkpoint incremental tailing (Pattern Decision: avoids the
//! checkpoint-invalidated-by-compaction-rewrite hazard,
//! `research/pitfalls.md` §1). Mirrors
//! `claude_code_session::session_bi::spawn_session_bi_refresh_task`'s
//! eager-scan-before-spawn / periodic-interval-loop structure.

use std::sync::Arc;
use std::time::Duration;

use crate::claude_code_session::discovery::{discover_sessions_glob, SortBy};
use crate::context_forensics::ingest_claude_code::ingest_claude_code_session;
use crate::context_forensics::store::ContextForensicsStore;

/// How often [`spawn_context_forensics_refresh_task`] recomputes the store
/// from disk. Matches `session_bi.rs`'s `SESSION_BI_REFRESH_INTERVAL`.
pub const CONTEXT_FORENSICS_REFRESH_INTERVAL: Duration = Duration::from_mins(15);

/// One rescan pass: glob `session_glob`, ingest every discovered file into
/// `store`, tolerating (and logging) any single file's ingestion failure
/// without aborting the rest of the corpus.
///
/// A glob-pattern failure (malformed pattern) is logged and treated as a
/// zero-session scan rather than propagated — this task runs unattended in
/// the background and has no caller in a position to react to an `Err`.
fn run_context_forensics_scan(store: &ContextForensicsStore, session_glob: &str) {
    let sessions = match discover_sessions_glob(session_glob, SortBy::RecentFirst) {
        Ok(sessions) => sessions,
        Err(error) => {
            tracing::warn!(%error, session_glob, "context-forensics scan: failed to glob session files");
            return;
        }
    };

    let mut ingested = 0u64;
    let mut failed = 0u64;
    for session in sessions {
        match ingest_claude_code_session(store, &session.path) {
            Ok(_summary) => ingested += 1,
            Err(error) => {
                failed += 1;
                tracing::warn!(
                    path = %session.path.display(),
                    %error,
                    "context-forensics scan: failed to ingest session"
                );
            }
        }
    }
    tracing::info!(ingested, failed, "context-forensics rescan complete");
}

/// Spawn the periodic rescan task.
///
/// Runs one eager scan synchronously on the calling thread *before*
/// spawning the periodic loop — matches `server.rs:115-121`'s documented
/// eager-scan-before-background-loop rationale, so the very first
/// `/dashboard/context` request after startup never races an empty store.
/// `ContextForensicsStore`'s I/O is local `rusqlite`/file-read work, not
/// network-bound, so running it synchronously here (rather than spawning
/// it as its own task) keeps this function's contract simple: by the time
/// it returns, the store already reflects the current corpus.
#[must_use]
pub fn spawn_context_forensics_refresh_task(
    store: Arc<ContextForensicsStore>,
    session_glob: String,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    run_context_forensics_scan(&store, &session_glob);

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await; // first tick fires immediately; skip it (eager scan above already covered it)
        loop {
            ticker.tick().await;
            run_context_forensics_scan(&store, &session_glob);
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

        // Eager scan must be complete before this call returns — no
        // .await/sleep here, just an assertion immediately after.
        let handle =
            spawn_context_forensics_refresh_task(Arc::clone(&store), glob, Duration::from_hours(1));

        assert_eq!(store.session_row_count().unwrap(), 2);
        handle.abort();
    }

    #[test]
    fn run_context_forensics_scan_should_ingest_remaining_files_when_one_file_is_malformed() {
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
        let store = ContextForensicsStore::open(&store_dir.path().join("cf.sqlite")).unwrap();

        run_context_forensics_scan(&store, &glob);

        assert_eq!(store.session_row_count().unwrap(), 2);
    }
}
