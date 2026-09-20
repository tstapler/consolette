//! Session-scoped, long-lived cache for tool I/O pruned out of a
//! compacted transcript (`src/claude_code_session/prune.rs`).
//!
//! See `project_plans/compaction-hook/decisions/ADR-009-omission-cache-and-mcp-session-scoping.md`
//! for the full rationale — in short, this is the direct structural fix for
//! magic-compact's `findSessionIdBySuffix` cross-session enumeration bug:
//! every read is scoped by `(session_id, content_id)` and there is no
//! method here that resolves a bare `content_id`.

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, TransactionBehavior};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const CREATE_TABLE_SQL: &str = "CREATE TABLE IF NOT EXISTS omitted_content (
    session_id   TEXT NOT NULL,
    content_id   TEXT NOT NULL,
    content      TEXT NOT NULL,
    tool_name    TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    PRIMARY KEY (session_id, content_id)
)";

/// A `rusqlite`-backed cache of pruned tool I/O, keyed by
/// `(session_id, content_id)`.
///
/// Holds its [`Connection`] behind a [`Mutex`] rather than requiring
/// callers to take `&mut OmissionCache`: `insert`'s per-session
/// `content_id` numbering needs a `rusqlite` transaction, which in turn
/// needs `&mut Connection`, but this cache is expected to be shared across
/// concurrent tool-row pruning during one compaction run (see ADR-009's
/// note on parallel tool-row pruning racing on the count). `&self` +
/// internal locking keeps that concurrency story out of every call site.
pub struct OmissionCache {
    conn: Mutex<Connection>,
}

impl OmissionCache {
    /// Open (creating if necessary) the sqlite database at `path`.
    ///
    /// Creates the parent directory if missing, creates the
    /// `omitted_content` table if missing, enables WAL mode and a 5s
    /// `busy_timeout`, then hardens permissions: `0700` on the parent
    /// directory and `0600` on the sqlite file itself
    /// (`research/pitfalls.md` §5 — this cache stores raw, unredacted tool
    /// output, potentially including secrets).
    ///
    /// # Errors
    ///
    /// Returns an error if the parent directory can't be created, the
    /// connection can't be opened, the schema/pragmas can't be applied, or
    /// permissions can't be set.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|error| {
                    anyhow!(
                        "failed to create omission cache directory {}: {error}",
                        parent.display()
                    )
                })?;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(
                    |error| {
                        anyhow!(
                            "failed to set 0700 permissions on {}: {error}",
                            parent.display()
                        )
                    },
                )?;
            }
        }

        let conn = Connection::open(path).map_err(|error| {
            anyhow!("failed to open omission cache {}: {error}", path.display())
        })?;

        conn.execute(CREATE_TABLE_SQL, [])
            .map_err(|error| anyhow!("failed to create omitted_content table: {error}"))?;

        let journal_mode: String = conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
            .map_err(|error| anyhow!("failed to set journal_mode=WAL: {error}"))?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            return Err(anyhow!(
                "expected journal_mode=WAL, sqlite reported {journal_mode}"
            ));
        }

        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| anyhow!("failed to set busy_timeout=5000: {error}"))?;

        if path.exists() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| {
                anyhow!(
                    "failed to set 0600 permissions on {}: {error}",
                    path.display()
                )
            })?;
        }

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Insert `content` for `session_id`/`tool_name`, returning the newly
    /// assigned `content_id` (`"omitted-{n:03}"`, `n` = 1 + the number of
    /// rows already cached for `session_id`).
    ///
    /// Uses [`TransactionBehavior::Immediate`] write locks alongside a monotonic
    /// suffix retry loop (`omitted-001_1`, `omitted-001_2`, ...) upon primary key
    /// `(session_id, content_id)` collision, eliminating primary key collisions
    /// across concurrent process handles in compliance with `ADR-002`.
    ///
    /// # Errors
    ///
    /// Returns an error if the cache's internal lock is poisoned or any
    /// underlying `rusqlite` call fails.
    pub fn insert(&self, session_id: &str, tool_name: &str, content: &str) -> Result<String> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("omission cache connection lock poisoned"))?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM omitted_content WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        let base_num = count + 1;
        let created_at = chrono::Utc::now().to_rfc3339();

        let mut suffix = 0usize;
        let content_id = loop {
            let candidate = if suffix == 0 {
                format!("omitted-{base_num:03}")
            } else {
                format!("omitted-{base_num:03}_{suffix}")
            };

            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM omitted_content WHERE session_id = ?1 AND content_id = ?2)",
                params![session_id, &candidate],
                |row| row.get(0),
            )?;

            if !exists {
                match tx.execute(
                    "INSERT INTO omitted_content (session_id, content_id, content, tool_name, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![session_id, &candidate, content, tool_name, created_at],
                ) {
                    Ok(_) => break candidate,
                    Err(err) => {
                        if suffix >= 1000 {
                            return Err(anyhow!("too many collisions inserting content_id: {err}"));
                        }
                    }
                }
            }
            suffix += 1;
            if suffix >= 1000 {
                return Err(anyhow!(
                    "exceeded max retry iterations for content_id collision"
                ));
            }
        };

        tx.commit()?;

        Ok(content_id)
    }

    /// Look up cached content, scoped to `(session_id, content_id)`.
    ///
    /// There is deliberately no method that accepts a bare `content_id` —
    /// see this module's doc comment and ADR-009. A `content_id` that
    /// exists but under a *different* `session_id` returns `Ok(None)`,
    /// identically to a `content_id` that doesn't exist at all.
    ///
    /// # Errors
    ///
    /// Returns an error if the cache's internal lock is poisoned or the
    /// underlying `rusqlite` query fails for a reason other than "no
    /// matching row."
    pub fn get(&self, session_id: &str, content_id: &str) -> Result<Option<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("omission cache connection lock poisoned"))?;
        match conn.query_row(
            "SELECT content FROM omitted_content WHERE session_id = ?1 AND content_id = ?2",
            params![session_id, content_id],
            |row| row.get::<_, String>(0),
        ) {
            Ok(content) => Ok(Some(content)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(anyhow!("omission cache lookup failed: {error}")),
        }
    }

    /// Returns the total count of omission cache entries, optionally filtered by `session_id`.
    ///
    /// # Errors
    ///
    /// Returns an error if the cache's internal lock is poisoned or the SQL query fails.
    pub fn count_entries(&self, session_id: Option<&str>) -> Result<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("omission cache connection lock poisoned"))?;
        let count: i64 = if let Some(sid) = session_id {
            conn.query_row(
                "SELECT COUNT(*) FROM omitted_content WHERE session_id = ?1",
                params![sid],
                |row| row.get(0),
            )?
        } else {
            conn.query_row("SELECT COUNT(*) FROM omitted_content", [], |row| row.get(0))?
        };
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// `~/.claude/consolette/omission-cache.sqlite`.
    ///
    /// Duplicates `src/main.rs::config_dir()`'s one-line `HOME`-env-var
    /// lookup rather than sharing it — this cache's directory
    /// (`~/.claude/consolette`) and the CLI's config directory
    /// (`~/.config/consolette`) are different paths for a different
    /// purpose, and plan.md's Task 2.2.1e explicitly calls for duplicating
    /// the lookup here rather than forcing a premature shared abstraction.
    #[must_use]
    pub fn default_cache_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home)
            .join(".claude")
            .join("consolette")
            .join("omission-cache.sqlite")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // test assertions on well-formed fixtures
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// `OmissionCache::open` hardens its cache file's *parent directory* to
    /// `0700` on every open (ADR-009), so the parent must be a directory the
    /// test process actually owns and may chmod — nesting under a fresh
    /// `tempfile::tempdir()` (rather than placing the file directly in the
    /// shared, sandbox-restricted OS temp root via a bare `NamedTempFile`)
    /// keeps that chmod within the test's own directory.
    fn temp_cache_path(dir: &TempDir) -> PathBuf {
        dir.path().join("omission-cache.sqlite")
    }

    #[test]
    fn omission_cache_get_should_return_some_when_session_and_content_id_match() {
        let dir = TempDir::new().unwrap();
        let cache = OmissionCache::open(&temp_cache_path(&dir)).unwrap();

        let content_id = cache.insert("A", "Bash", "some long output").unwrap();
        let result = cache.get("A", &content_id).unwrap();

        assert_eq!(result, Some("some long output".to_string()));
    }

    #[test]
    fn omission_cache_get_should_return_none_when_content_id_belongs_to_different_session() {
        let dir = TempDir::new().unwrap();
        let cache = OmissionCache::open(&temp_cache_path(&dir)).unwrap();

        let content_id = cache
            .insert("A", "Bash", "session A's secret output")
            .unwrap();

        // The core ADR-009 security property: a content_id valid for
        // session A must not resolve under a different session_id.
        let result = cache.get("B", &content_id).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn omission_cache_open_should_create_file_with_0600_permissions_and_wal_mode() {
        let dir = TempDir::new().unwrap();
        let cache_path = temp_cache_path(&dir);
        // Start from deliberately loose permissions so the assertions below
        // prove `open` actively tightens them, rather than merely observing
        // whatever default `TempDir` happened to create.
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();

        let cache = OmissionCache::open(&cache_path).unwrap();

        let file_mode = fs::metadata(&cache_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600);

        let dir_mode = fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);

        let conn = cache.conn.lock().unwrap();
        let journal_mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode.to_lowercase(), "wal");

        let busy_timeout: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(busy_timeout, 5000);
    }

    #[test]
    fn omission_cache_should_handle_primary_key_collisions_with_monotonic_suffix_retry_loop() {
        // UT-CACHE-002: Primary key collision retry loop
        let dir = TempDir::new().unwrap();
        let cache_path = temp_cache_path(&dir);
        let cache = OmissionCache::open(&cache_path).unwrap();

        // Pre-insert omitted-002 directly into SQLite (with 1 row total, so count + 1 = 2, colliding with omitted-002)
        {
            let raw = Connection::open(&cache_path).unwrap();
            raw.execute(
                "INSERT INTO omitted_content (session_id, content_id, content, tool_name, created_at) \
                 VALUES ('session-1', 'omitted-002', 'pre-existing', 'Bash', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        }

        // Call insert: count is 1, candidate base is omitted-002, which collides with pre-seeded omitted-002.
        let id1 = cache.insert("session-1", "Bash", "new content 1").unwrap();
        assert_eq!(id1, "omitted-002_1");

        // Insert again: count is 2 (rows omitted-002 and omitted-002_1). base_num = 3 -> omitted-003.
        let id2 = cache.insert("session-1", "Bash", "new content 2").unwrap();
        assert_eq!(id2, "omitted-003");
        let val1 = cache.get("session-1", &id1).unwrap();
        assert_eq!(val1, Some("new content 1".to_string()));
    }

    #[test]
    fn omission_cache_should_support_multithreaded_concurrent_insertions() {
        // IT-CACHE-004: Multi-threaded concurrent insertion stress test
        use std::sync::Arc;

        let dir = TempDir::new().unwrap();
        let cache = Arc::new(OmissionCache::open(&temp_cache_path(&dir)).unwrap());
        let mut handles = Vec::new();

        for t in 0..8 {
            let cache_clone = Arc::clone(&cache);
            handles.push(std::thread::spawn(move || {
                let mut ids = Vec::new();
                for i in 0..10 {
                    let text = format!("thread {t} item {i}");
                    let id = cache_clone
                        .insert("concurrent-session", "Bash", &text)
                        .unwrap();
                    ids.push((id, text));
                }
                ids
            }));
        }

        let mut all_ids = std::collections::HashSet::new();
        for handle in handles {
            let ids = handle.join().unwrap();
            for (id, text) in ids {
                assert!(all_ids.insert(id.clone()), "content_id {id} must be unique");
                let retrieved = cache.get("concurrent-session", &id).unwrap();
                assert_eq!(retrieved, Some(text));
            }
        }
        assert_eq!(all_ids.len(), 80);
    }
}
