//! `ContextForensicsStore` — the `rusqlite`-backed repository every later
//! context-analyzer epic writes into (plan.md Epic 1.2, ADR-001).
//!
//! Mirrors `claude_code_session::omission_cache::OmissionCache`'s
//! open/harden/WAL structure exactly: same parent-directory (`0700`) and
//! file (`0600`) permission hardening, since this store holds the same
//! sensitivity class of data (raw conversation/tool-I/O content derived
//! from transcripts).

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::cost_metrics::pricing::PricingTable;

// ---------------------------------------------------------------------------
// Domain sum types (Domain Glossary: `Source`, `UsageProvenance`)
// ---------------------------------------------------------------------------

/// Session-level discriminant: which CLI produced this transcript.
///
/// Stored as a `CHECK`-constrained `TEXT` column (defense-in-depth at the
/// storage boundary); this Rust-side enum, matched exhaustively, is the real
/// sum type (type-driven-design Pattern Decision).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    ClaudeCode,
    Codex,
}

impl Source {
    #[must_use]
    pub fn as_db_str(self) -> &'static str {
        match self {
            Source::ClaudeCode => "claude_code",
            Source::Codex => "codex",
        }
    }

    /// # Errors
    ///
    /// Returns an error if `value` is not one of the `CHECK`-constrained
    /// values this column ever writes (`claude_code`, `codex`) — should
    /// only happen if the sqlite file was hand-edited outside this crate.
    pub fn from_db_str(value: &str) -> Result<Self> {
        match value {
            "claude_code" => Ok(Source::ClaudeCode),
            "codex" => Ok(Source::Codex),
            other => Err(anyhow!("unknown source column value: {other}")),
        }
    }
}

/// Marks whether an [`ApiCallRow`]'s usage figures came from a transcript
/// row (exact) or a live proxy capture — enforced at write time, not query
/// time (`research/pitfalls.md` §4: "transcript is primary").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageProvenance {
    TranscriptExact,
    ProxyCaptured,
}

impl UsageProvenance {
    #[must_use]
    pub fn as_db_str(self) -> &'static str {
        match self {
            UsageProvenance::TranscriptExact => "transcript_exact",
            UsageProvenance::ProxyCaptured => "proxy_captured",
        }
    }

    /// # Errors
    ///
    /// Returns an error if `value` is not one of the `CHECK`-constrained
    /// values this column ever writes.
    pub fn from_db_str(value: &str) -> Result<Self> {
        match value {
            "transcript_exact" => Ok(UsageProvenance::TranscriptExact),
            "proxy_captured" => Ok(UsageProvenance::ProxyCaptured),
            other => Err(anyhow!("unknown usage_provenance column value: {other}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Row types (Domain Glossary: `SessionRow`, `TurnRow`, `ApiCallRow`)
// ---------------------------------------------------------------------------

/// Persisted record for one session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRow {
    pub id: String,
    pub source: Source,
    pub path: String,
    pub project: Option<String>,
    pub started_at: Option<String>,
    pub last_ingested_at: String,
    pub chain_coverage_ratio: Option<f64>,
    pub parse_failure_count: u64,
}

/// Persisted per-turn record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnRow {
    pub id: String,
    pub session_id: String,
    pub turn_index: u64,
    pub user_row_uuid: String,
    pub cumulative_tokens: u64,
}

/// Persisted per-API-call record. One assistant transcript row = one API
/// call for Claude Code.
///
/// `cache_creation_input_tokens`/`cache_read_input_tokens` are `Option<u64>`
/// (nullable in the schema) from Phase 1 onward: Claude Code ingestion
/// always writes `Some(_)` (including `Some(0)`), Codex ingestion (Phase 3)
/// writes `None` for the field it has no equivalent for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiCallRow {
    /// `"{session_id}:{row_uuid}"`.
    pub id: String,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub row_uuid: String,
    pub call_index: u64,
    pub model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub tool_io_tokens: u64,
    pub conversation_tokens: u64,
    pub system_tokens: u64,
    pub usage_provenance: UsageProvenance,
}

/// Persisted native-compaction event (Claude Code's own auto-compaction, as
/// distinct from consolette's own compaction).
///
/// `turn_index` is an additive column beyond the Migration Plan's literal
/// Phase-1 `CREATE TABLE` text (which lists only `id`/`session_id`/
/// `row_uuid`/`tokens_saved`) — needed so the growth chart (Story 1.4.3 AC2)
/// can place a compaction marker at the right turn without re-parsing the
/// transcript on every request. Nullable, additive, and harmless under the
/// plan's own `CREATE TABLE IF NOT EXISTS` migration model (no constraint
/// tightened or loosened for any existing column); disclosed here rather
/// than silently diverging from the printed schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeCompactionEventRow {
    pub session_id: String,
    pub row_uuid: String,
    pub tokens_saved: Option<i64>,
    pub turn_index: Option<u64>,
}

/// One turn's aggregated composition/usage figures — [`ContextForensicsStore::composition_for_session`]'s
/// per-element shape (Story 1.4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TurnComposition {
    pub turn_index: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub tool_io_tokens: u64,
    pub conversation_tokens: u64,
    pub system_tokens: u64,
}

/// One point on the growth chart — [`ContextForensicsStore::growth_for_session`]'s
/// per-element shape (Story 1.4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrowthPoint {
    pub turn_index: u64,
    pub cumulative_tokens: u64,
}

/// One row of `GET /v1/context/sessions/summary` —
/// [`ContextForensicsStore::summary_for_all_sessions`]'s per-element shape
/// (Story 2.1.1). Doubles as the "Cost/Call Over Time" trend series (Story
/// 2.1.2) since the store method orders these by `started_at` ascending.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub source: Source,
    pub started_at: Option<String>,
    pub cost_per_call_usd: f64,
    pub peak_context_tokens: u64,
    pub call_count: u64,
    pub chain_coverage_ratio: Option<f64>,
}

/// `rusqlite` has no `ToSql` impl for `u64` (sqlite's native integer type is
/// signed 64-bit) — every `u64` token/index count this store persists is
/// converted through this helper, saturating at `i64::MAX` rather than
/// wrapping, since a real token count overflowing `i64::MAX` is not a case
/// worth modeling.
fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

// ---------------------------------------------------------------------------
// Row-write SQL, factored out of the `ContextForensicsStore` methods below
// so both the single-row `upsert_*` methods and `upsert_ingested_session`'s
// one-transaction batch path share the exact same statements (see that
// method's doc comment for why the batch path exists). Each takes `&Connection`
// so a `&rusqlite::Transaction` (which derefs to `Connection`) works too.
// ---------------------------------------------------------------------------

fn exec_upsert_session(conn: &Connection, row: &SessionRow) -> Result<()> {
    let parse_failure_count = to_i64(row.parse_failure_count);
    conn.execute(
        "INSERT INTO sessions (id, source, path, project, started_at, last_ingested_at, chain_coverage_ratio, parse_failure_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT (id) DO UPDATE SET
            source = excluded.source,
            path = excluded.path,
            project = excluded.project,
            started_at = excluded.started_at,
            last_ingested_at = excluded.last_ingested_at,
            chain_coverage_ratio = excluded.chain_coverage_ratio,
            parse_failure_count = excluded.parse_failure_count",
        params![
            row.id,
            row.source.as_db_str(),
            row.path,
            row.project,
            row.started_at,
            row.last_ingested_at,
            row.chain_coverage_ratio,
            parse_failure_count,
        ],
    )?;
    Ok(())
}

fn exec_upsert_turn(conn: &Connection, row: &TurnRow) -> Result<()> {
    let turn_index = to_i64(row.turn_index);
    let cumulative_tokens = to_i64(row.cumulative_tokens);
    conn.execute(
        "INSERT INTO turns (id, session_id, turn_index, user_row_uuid, cumulative_tokens)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (session_id, turn_index) DO UPDATE SET
            id = excluded.id,
            user_row_uuid = excluded.user_row_uuid,
            cumulative_tokens = excluded.cumulative_tokens",
        params![
            row.id,
            row.session_id,
            turn_index,
            row.user_row_uuid,
            cumulative_tokens,
        ],
    )?;
    Ok(())
}

fn exec_upsert_api_call(conn: &Connection, row: &ApiCallRow) -> Result<()> {
    let call_index = to_i64(row.call_index);
    let input_tokens = to_i64(row.input_tokens);
    let output_tokens = to_i64(row.output_tokens);
    let cache_creation_input_tokens = row.cache_creation_input_tokens.map(to_i64);
    let cache_read_input_tokens = row.cache_read_input_tokens.map(to_i64);
    let tool_io_tokens = to_i64(row.tool_io_tokens);
    let conversation_tokens = to_i64(row.conversation_tokens);
    let system_tokens = to_i64(row.system_tokens);
    conn.execute(
        "INSERT INTO api_calls (
            id, session_id, turn_id, row_uuid, call_index, model,
            input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens,
            tool_io_tokens, conversation_tokens, system_tokens, usage_provenance
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT (session_id, row_uuid) DO UPDATE SET
            id = excluded.id,
            turn_id = excluded.turn_id,
            call_index = excluded.call_index,
            model = excluded.model,
            input_tokens = excluded.input_tokens,
            output_tokens = excluded.output_tokens,
            cache_creation_input_tokens = excluded.cache_creation_input_tokens,
            cache_read_input_tokens = excluded.cache_read_input_tokens,
            tool_io_tokens = excluded.tool_io_tokens,
            conversation_tokens = excluded.conversation_tokens,
            system_tokens = excluded.system_tokens,
            usage_provenance = excluded.usage_provenance",
        params![
            row.id,
            row.session_id,
            row.turn_id,
            row.row_uuid,
            call_index,
            row.model,
            input_tokens,
            output_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
            tool_io_tokens,
            conversation_tokens,
            system_tokens,
            row.usage_provenance.as_db_str(),
        ],
    )?;
    Ok(())
}

fn exec_upsert_native_compaction_event(
    conn: &Connection,
    row: &NativeCompactionEventRow,
) -> Result<()> {
    let turn_index = row.turn_index.map(to_i64);
    conn.execute(
        "INSERT INTO native_compaction_events (session_id, row_uuid, tokens_saved, turn_index)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (session_id, row_uuid) DO UPDATE SET
            tokens_saved = excluded.tokens_saved,
            turn_index = excluded.turn_index",
        params![row.session_id, row.row_uuid, row.tokens_saved, turn_index],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// ContextForensicsStore
// ---------------------------------------------------------------------------

const CREATE_TABLES_SQL: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id                   TEXT PRIMARY KEY,
    source               TEXT NOT NULL CHECK (source IN ('claude_code','codex')),
    path                 TEXT NOT NULL,
    project              TEXT,
    started_at           TEXT,
    last_ingested_at     TEXT NOT NULL,
    chain_coverage_ratio REAL,
    parse_failure_count  INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS turns (
    id                TEXT PRIMARY KEY,
    session_id        TEXT NOT NULL REFERENCES sessions(id),
    turn_index        INTEGER NOT NULL,
    user_row_uuid     TEXT NOT NULL,
    cumulative_tokens INTEGER NOT NULL,
    UNIQUE (session_id, turn_index)
);

CREATE TABLE IF NOT EXISTS api_calls (
    id                           TEXT PRIMARY KEY,
    session_id                   TEXT NOT NULL REFERENCES sessions(id),
    turn_id                      TEXT REFERENCES turns(id),
    row_uuid                     TEXT NOT NULL,
    call_index                   INTEGER NOT NULL,
    model                        TEXT,
    input_tokens                 INTEGER NOT NULL,
    output_tokens                INTEGER NOT NULL,
    cache_creation_input_tokens  INTEGER,
    cache_read_input_tokens      INTEGER,
    tool_io_tokens                INTEGER NOT NULL,
    conversation_tokens           INTEGER NOT NULL,
    system_tokens                  INTEGER NOT NULL,
    usage_provenance              TEXT NOT NULL CHECK (usage_provenance IN ('transcript_exact','proxy_captured')),
    UNIQUE (session_id, row_uuid)
);

CREATE TABLE IF NOT EXISTS native_compaction_events (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id   TEXT NOT NULL REFERENCES sessions(id),
    row_uuid     TEXT NOT NULL,
    tokens_saved INTEGER,
    turn_index   INTEGER,
    UNIQUE (session_id, row_uuid)
);
";

/// `rusqlite`-backed repository for context-forensics data. Holds its
/// [`Connection`] behind a [`Mutex`] (mirrors `OmissionCache`) so callers
/// never need `&mut ContextForensicsStore`.
pub struct ContextForensicsStore {
    conn: Mutex<Connection>,
}

impl ContextForensicsStore {
    /// Open (creating if necessary) the sqlite database at `path`.
    ///
    /// Creates the parent directory (`0700`), the database file, sets
    /// `journal_mode=WAL` and a 5s `busy_timeout`, hardens the file to
    /// `0600`, and creates the Phase-1 schema
    /// (`sessions`/`turns`/`api_calls`/`native_compaction_events`) via
    /// idempotent `CREATE TABLE IF NOT EXISTS` statements.
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
                        "failed to create context-forensics store directory {}: {error}",
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
            anyhow!(
                "failed to open context-forensics store {}: {error}",
                path.display()
            )
        })?;

        conn.execute_batch(CREATE_TABLES_SQL)
            .map_err(|error| anyhow!("failed to create context-forensics schema: {error}"))?;

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

        // The standard WAL-mode pairing: NORMAL only fsyncs at checkpoints
        // (not every commit) while WAL itself still protects against
        // corruption on a crash — full FULL-durability is unnecessary for a
        // rebuildable derived cache (Migration Plan: "every table here is a
        // derived cache, fully rebuildable... by deleting the file"). Matters
        // once `upsert_ingested_session` commits once per session file
        // across a multi-thousand-file real corpus (see that method's doc
        // comment).
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|error| anyhow!("failed to set synchronous=NORMAL: {error}"))?;

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

    /// `~/.claude/consolette/context-forensics.sqlite` (Migration Plan).
    #[must_use]
    pub fn default_store_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home)
            .join(".claude")
            .join("consolette")
            .join("context-forensics.sqlite")
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| anyhow!("context-forensics store connection lock poisoned"))
    }

    /// Insert or update a [`SessionRow`], keyed on `id`.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned or the
    /// underlying `rusqlite` call fails.
    pub fn upsert_session(&self, row: &SessionRow) -> Result<()> {
        let conn = self.lock()?;
        exec_upsert_session(&conn, row)
    }

    /// Insert or update a [`TurnRow`], keyed on `(session_id, turn_index)`.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned or the
    /// underlying `rusqlite` call fails.
    pub fn upsert_turn(&self, row: &TurnRow) -> Result<()> {
        let conn = self.lock()?;
        exec_upsert_turn(&conn, row)
    }

    /// Insert or update an [`ApiCallRow`], keyed on `(session_id, row_uuid)`.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned or the
    /// underlying `rusqlite` call fails.
    pub fn upsert_api_call(&self, row: &ApiCallRow) -> Result<()> {
        let conn = self.lock()?;
        exec_upsert_api_call(&conn, row)
    }

    /// Insert or update a [`NativeCompactionEventRow`], keyed on
    /// `(session_id, row_uuid)`.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned or the
    /// underlying `rusqlite` call fails.
    pub fn upsert_native_compaction_event(&self, row: &NativeCompactionEventRow) -> Result<()> {
        let conn = self.lock()?;
        exec_upsert_native_compaction_event(&conn, row)
    }

    /// Upsert one whole ingested session (session + turns + calls +
    /// native-compaction events) inside a single `SQLite` transaction.
    ///
    /// [`ingest_claude_code_session`](crate::context_forensics::ingest_claude_code::ingest_claude_code_session)
    /// uses this instead of the single-row `upsert_*` methods above: each of
    /// those auto-commits (`SQLite`'s default outside an explicit
    /// transaction), and Task 1.4.4a wires ingestion to run eagerly, once
    /// per corpus rescan, over every discovered transcript — on a real
    /// multi-thousand-file `~/.claude/projects` corpus that meant one
    /// WAL-commit fsync per row (potentially tens of thousands per rescan)
    /// instead of one per session file, slow enough to blow well past a
    /// minute in practice. Batching to one commit per session file (not one
    /// per corpus, preserving Story 1.3.3's per-file failure isolation)
    /// fixes the root cause rather than papering over the timeout.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned, the transaction
    /// can't be started/committed, or any row write fails (the transaction
    /// is rolled back on drop if not committed, so a mid-batch failure never
    /// leaves a partially-written session).
    pub fn upsert_ingested_session(
        &self,
        session: &SessionRow,
        turns: &[TurnRow],
        calls: &[ApiCallRow],
        native_compaction_events: &[NativeCompactionEventRow],
    ) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        exec_upsert_session(&tx, session)?;
        for turn in turns {
            exec_upsert_turn(&tx, turn)?;
        }
        for call in calls {
            exec_upsert_api_call(&tx, call)?;
        }
        for event in native_compaction_events {
            exec_upsert_native_compaction_event(&tx, event)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Total number of stored [`SessionRow`]s. Test/verification helper so
    /// callers don't hand-write raw SQL in every test.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned or the query
    /// fails.
    pub fn session_row_count(&self) -> Result<u64> {
        let conn = self.lock()?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    /// Total number of stored [`ApiCallRow`]s for one session.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned or the query
    /// fails.
    pub fn api_call_row_count(&self, session_id: &str) -> Result<u64> {
        let conn = self.lock()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM api_calls WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    /// Fetch one [`SessionRow`] by id, or `None` if not present.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned, the query
    /// fails, or a stored `source` value doesn't match a known [`Source`]
    /// variant.
    pub fn get_session(&self, session_id: &str) -> Result<Option<SessionRow>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT id, source, path, project, started_at, last_ingested_at, chain_coverage_ratio, parse_failure_count
                 FROM sessions WHERE id = ?1",
                params![session_id],
                |row| {
                    let source: String = row.get(1)?;
                    let parse_failure_count: i64 = row.get(7)?;
                    Ok((
                        row.get::<_, String>(0)?,
                        source,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<f64>>(6)?,
                        parse_failure_count,
                    ))
                },
            )
            .optional()?;

        row.map(
            |(
                id,
                source,
                path,
                project,
                started_at,
                last_ingested_at,
                chain_coverage_ratio,
                parse_failure_count,
            )| {
                Ok(SessionRow {
                    id,
                    source: Source::from_db_str(&source)?,
                    path,
                    project,
                    started_at,
                    last_ingested_at,
                    chain_coverage_ratio,
                    parse_failure_count: u64::try_from(parse_failure_count).unwrap_or(0),
                })
            },
        )
        .transpose()
    }

    /// List every stored [`SessionRow`].
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned, the query
    /// fails, or a stored `source` value doesn't match a known [`Source`]
    /// variant.
    pub fn list_sessions(&self) -> Result<Vec<SessionRow>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, source, path, project, started_at, last_ingested_at, chain_coverage_ratio, parse_failure_count
             FROM sessions ORDER BY started_at ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let source: String = row.get(1)?;
            let parse_failure_count: i64 = row.get(7)?;
            Ok((
                row.get::<_, String>(0)?,
                source,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<f64>>(6)?,
                parse_failure_count,
            ))
        })?;

        let mut result = Vec::new();
        for row in rows {
            let (
                id,
                source,
                path,
                project,
                started_at,
                last_ingested_at,
                chain_coverage_ratio,
                parse_failure_count,
            ) = row?;
            result.push(SessionRow {
                id,
                source: Source::from_db_str(&source)?,
                path,
                project,
                started_at,
                last_ingested_at,
                chain_coverage_ratio,
                parse_failure_count: u64::try_from(parse_failure_count).unwrap_or(0),
            });
        }
        Ok(result)
    }

    /// Every [`TurnRow`] for one session, ordered by `turn_index`.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned or the query
    /// fails.
    pub fn turns_for_session(&self, session_id: &str) -> Result<Vec<TurnRow>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, turn_index, user_row_uuid, cumulative_tokens
             FROM turns WHERE session_id = ?1 ORDER BY turn_index ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            let turn_index: i64 = row.get(2)?;
            let cumulative_tokens: i64 = row.get(4)?;
            Ok(TurnRow {
                id: row.get(0)?,
                session_id: row.get(1)?,
                turn_index: u64::try_from(turn_index).unwrap_or(0),
                user_row_uuid: row.get(3)?,
                cumulative_tokens: u64::try_from(cumulative_tokens).unwrap_or(0),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every [`ApiCallRow`] for one session, ordered by `call_index`.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned, the query
    /// fails, or a stored `usage_provenance` value doesn't match a known
    /// [`UsageProvenance`] variant.
    pub fn api_calls_for_session(&self, session_id: &str) -> Result<Vec<ApiCallRow>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, turn_id, row_uuid, call_index, model,
                    input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens,
                    tool_io_tokens, conversation_tokens, system_tokens, usage_provenance
             FROM api_calls WHERE session_id = ?1 ORDER BY call_index ASC",
        )?;
        #[allow(clippy::type_complexity)]
        let rows = stmt.query_map(params![session_id], |row| {
            let call_index: i64 = row.get(4)?;
            let input_tokens: i64 = row.get(6)?;
            let output_tokens: i64 = row.get(7)?;
            let cache_creation_input_tokens: Option<i64> = row.get(8)?;
            let cache_read_input_tokens: Option<i64> = row.get(9)?;
            let tool_io_tokens: i64 = row.get(10)?;
            let conversation_tokens: i64 = row.get(11)?;
            let system_tokens: i64 = row.get(12)?;
            let usage_provenance: String = row.get(13)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                call_index,
                row.get::<_, Option<String>>(5)?,
                input_tokens,
                output_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
                tool_io_tokens,
                conversation_tokens,
                system_tokens,
                usage_provenance,
            ))
        })?;

        let mut result = Vec::new();
        for row in rows {
            let (
                id,
                session_id,
                turn_id,
                row_uuid,
                call_index,
                model,
                input_tokens,
                output_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
                tool_io_tokens,
                conversation_tokens,
                system_tokens,
                usage_provenance,
            ) = row?;
            result.push(ApiCallRow {
                id,
                session_id,
                turn_id,
                row_uuid,
                call_index: u64::try_from(call_index).unwrap_or(0),
                model,
                input_tokens: u64::try_from(input_tokens).unwrap_or(0),
                output_tokens: u64::try_from(output_tokens).unwrap_or(0),
                cache_creation_input_tokens: cache_creation_input_tokens
                    .map(|v| u64::try_from(v).unwrap_or(0)),
                cache_read_input_tokens: cache_read_input_tokens
                    .map(|v| u64::try_from(v).unwrap_or(0)),
                tool_io_tokens: u64::try_from(tool_io_tokens).unwrap_or(0),
                conversation_tokens: u64::try_from(conversation_tokens).unwrap_or(0),
                system_tokens: u64::try_from(system_tokens).unwrap_or(0),
                usage_provenance: UsageProvenance::from_db_str(&usage_provenance)?,
            });
        }
        Ok(result)
    }

    /// Every [`NativeCompactionEventRow`] for one session.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned or the query
    /// fails.
    pub fn native_compaction_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<NativeCompactionEventRow>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT session_id, row_uuid, tokens_saved, turn_index FROM native_compaction_events WHERE session_id = ?1",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            let turn_index: Option<i64> = row.get(3)?;
            Ok(NativeCompactionEventRow {
                session_id: row.get(0)?,
                row_uuid: row.get(1)?,
                tokens_saved: row.get(2)?,
                turn_index: turn_index.map(|v| u64::try_from(v).unwrap_or(0)),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// A session's peak `cumulative_tokens` across all its turns (`MAX`,
    /// computed at query time — [`crate::context_forensics`]'s `PeakContext`
    /// concept, never stored redundantly). `0` when the session has no
    /// turns yet.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned or the query
    /// fails.
    pub fn peak_context_tokens(&self, session_id: &str) -> Result<u64> {
        let conn = self.lock()?;
        let peak: Option<i64> = conn.query_row(
            "SELECT MAX(cumulative_tokens) FROM turns WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(peak.and_then(|v| u64::try_from(v).ok()).unwrap_or(0))
    }

    /// Per-turn [`TurnComposition`] for one session — every [`ApiCallRow`]
    /// grouped by its `turn_id`, aggregated with the containing turn's
    /// `turn_index`. A call with a `turn_id` that doesn't resolve to any
    /// stored [`TurnRow`] (shouldn't happen given ingestion always writes
    /// the turn first, but defensively skipped rather than panicking) is
    /// excluded rather than crashing the whole response.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned or the
    /// underlying queries fail.
    pub fn composition_for_session(&self, session_id: &str) -> Result<Vec<TurnComposition>> {
        let turns = self.turns_for_session(session_id)?;
        let calls = self.api_calls_for_session(session_id)?;

        let turn_index_by_id: HashMap<&str, u64> = turns
            .iter()
            .map(|t| (t.id.as_str(), t.turn_index))
            .collect();

        let mut acc: BTreeMap<u64, TurnComposition> = BTreeMap::new();
        for call in &calls {
            let Some(turn_id) = &call.turn_id else {
                continue;
            };
            let Some(&turn_index) = turn_index_by_id.get(turn_id.as_str()) else {
                continue;
            };
            let entry = acc.entry(turn_index).or_insert(TurnComposition {
                turn_index,
                ..TurnComposition::default()
            });
            entry.input_tokens += call.input_tokens;
            entry.output_tokens += call.output_tokens;
            entry.cache_creation_input_tokens += call.cache_creation_input_tokens.unwrap_or(0);
            entry.cache_read_input_tokens += call.cache_read_input_tokens.unwrap_or(0);
            entry.tool_io_tokens += call.tool_io_tokens;
            entry.conversation_tokens += call.conversation_tokens;
            entry.system_tokens += call.system_tokens;
        }

        Ok(acc.into_values().collect())
    }

    /// Per-turn `(turn_index, cumulative_tokens)` growth series for one
    /// session, ordered by `turn_index`.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned or the query
    /// fails.
    pub fn growth_for_session(&self, session_id: &str) -> Result<Vec<GrowthPoint>> {
        let turns = self.turns_for_session(session_id)?;
        Ok(turns
            .into_iter()
            .map(|turn| GrowthPoint {
                turn_index: turn.turn_index,
                cumulative_tokens: turn.cumulative_tokens,
            })
            .collect())
    }

    /// Cross-session cost/call, peak-context, and coverage summary (Story
    /// 2.1.1) — every stored session, `cost_per_call_usd` computed via
    /// `pricing`'s cache-aware rates (Story 1.1.2), not input/output alone.
    /// A call whose `model` is unset or absent from `pricing` contributes
    /// `$0.0` to that session's total rather than failing the whole
    /// response — `PricingTable::price_for` returning `None` is documented
    /// as the normal "unpriced model" case, not an error.
    ///
    /// Ordered by `started_at` ascending (a `None` `started_at` sorts last,
    /// tie-broken by `id` for determinism) so this same payload doubles as
    /// the chronological "Cost/Call Over Time" trend series (Story 2.1.2)
    /// without a second query.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned or any
    /// underlying query fails.
    pub fn summary_for_all_sessions(&self, pricing: &PricingTable) -> Result<Vec<SessionSummary>> {
        let sessions = self.list_sessions()?;
        let mut summaries = Vec::with_capacity(sessions.len());
        for session in &sessions {
            let calls = self.api_calls_for_session(&session.id)?;
            let call_count = calls.len();
            let total_cost_usd: f64 = calls.iter().map(|call| call_cost_usd(call, pricing)).sum();
            #[allow(clippy::cast_precision_loss)]
            let cost_per_call_usd = if call_count == 0 {
                0.0
            } else {
                total_cost_usd / call_count as f64
            };
            let peak_context_tokens = self.peak_context_tokens(&session.id)?;
            summaries.push(SessionSummary {
                id: session.id.clone(),
                source: session.source,
                started_at: session.started_at.clone(),
                cost_per_call_usd,
                peak_context_tokens,
                call_count: to_u64_saturating(call_count),
                chain_coverage_ratio: session.chain_coverage_ratio,
            });
        }
        summaries.sort_by(|a, b| match (&a.started_at, &b.started_at) {
            (Some(x), Some(y)) => x.cmp(y).then_with(|| a.id.cmp(&b.id)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.id.cmp(&b.id),
        });
        Ok(summaries)
    }
}

/// One call's total USD cost across all four cache-aware rate tiers (Story
/// 1.1.2/2.1.1). `0.0` when the call's `model` is unset or unpriced —
/// `PricingTable::price_for` returning `None` is a documented, expected
/// case (an unrecognized/new model), not an error this aggregation should
/// fail on.
#[allow(clippy::cast_precision_loss)]
fn call_cost_usd(call: &ApiCallRow, pricing: &PricingTable) -> f64 {
    let Some(model) = call.model.as_deref() else {
        return 0.0;
    };
    let Some(price) = pricing.price_for(model) else {
        return 0.0;
    };
    let cache_creation_input_tokens = call.cache_creation_input_tokens.unwrap_or(0);
    let cache_read_input_tokens = call.cache_read_input_tokens.unwrap_or(0);
    call.input_tokens as f64 * price.input_usd_per_token
        + call.output_tokens as f64 * price.output_usd_per_token
        + cache_creation_input_tokens as f64 * price.cache_creation_usd_per_token
        + cache_read_input_tokens as f64 * price.cache_read_usd_per_token
}

/// `usize` (a row count within one process's memory, never near
/// `u64::MAX`) into the `u64` [`SessionSummary::call_count`] uses for
/// JSON-response consistency with this module's other token/count fields.
fn to_u64_saturating(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_store_path(dir: &TempDir) -> PathBuf {
        dir.path().join("context-forensics.sqlite")
    }

    fn sample_session(id: &str) -> SessionRow {
        SessionRow {
            id: id.to_string(),
            source: Source::ClaudeCode,
            path: format!("/tmp/{id}.jsonl"),
            project: Some("consolette".to_string()),
            started_at: Some("2026-01-01T00:00:00Z".to_string()),
            last_ingested_at: "2026-01-01T00:05:00Z".to_string(),
            chain_coverage_ratio: Some(1.0),
            parse_failure_count: 0,
        }
    }

    fn sample_api_call(session_id: &str, row_uuid: &str, input_tokens: u64) -> ApiCallRow {
        ApiCallRow {
            id: format!("{session_id}:{row_uuid}"),
            session_id: session_id.to_string(),
            turn_id: None,
            row_uuid: row_uuid.to_string(),
            call_index: 0,
            model: Some("claude-sonnet-5".to_string()),
            input_tokens,
            output_tokens: 20,
            cache_creation_input_tokens: Some(0),
            cache_read_input_tokens: Some(0),
            tool_io_tokens: 0,
            conversation_tokens: input_tokens,
            system_tokens: 0,
            usage_provenance: UsageProvenance::TranscriptExact,
        }
    }

    #[test]
    fn open_should_create_all_phase_one_tables_when_called_against_fresh_path() {
        let dir = TempDir::new().unwrap();
        let path = temp_store_path(&dir);

        let store = ContextForensicsStore::open(&path).unwrap();

        let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600);
        let dir_mode = fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);

        let conn = store.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        for expected in ["sessions", "turns", "api_calls", "native_compaction_events"] {
            assert!(
                names.contains(&expected.to_string()),
                "expected table {expected} in {names:?}"
            );
        }
    }

    #[test]
    fn open_should_allow_nullable_cache_columns_when_codex_style_null_written() {
        let dir = TempDir::new().unwrap();
        let store = ContextForensicsStore::open(&temp_store_path(&dir)).unwrap();
        store.upsert_session(&sample_session("s1")).unwrap();

        let mut call = sample_api_call("s1", "u1", 100);
        call.cache_creation_input_tokens = None;
        call.cache_read_input_tokens = None;
        store.upsert_api_call(&call).unwrap();

        let stored = store.api_calls_for_session("s1").unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].cache_creation_input_tokens, None);
        assert_eq!(stored[0].cache_read_input_tokens, None);
    }

    #[test]
    fn open_should_be_idempotent_when_called_twice_against_same_path() {
        let dir = TempDir::new().unwrap();
        let path = temp_store_path(&dir);

        let store = ContextForensicsStore::open(&path).unwrap();
        store.upsert_session(&sample_session("s1")).unwrap();
        drop(store);

        let store = ContextForensicsStore::open(&path).unwrap();
        assert_eq!(store.session_row_count().unwrap(), 1);
    }

    #[test]
    fn open_should_return_err_when_path_points_to_a_file_where_directory_expected() {
        let dir = TempDir::new().unwrap();
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, b"not a directory").unwrap();
        let path = blocker.join("context-forensics.sqlite");

        let result = ContextForensicsStore::open(&path);
        assert!(result.is_err());
    }

    #[test]
    fn upsert_api_call_should_update_in_place_when_row_uuid_already_exists() {
        let dir = TempDir::new().unwrap();
        let store = ContextForensicsStore::open(&temp_store_path(&dir)).unwrap();
        store.upsert_session(&sample_session("s1")).unwrap();

        store
            .upsert_api_call(&sample_api_call("s1", "abc-123", 100))
            .unwrap();
        store
            .upsert_api_call(&sample_api_call("s1", "abc-123", 100))
            .unwrap();

        assert_eq!(store.api_call_row_count("s1").unwrap(), 1);
    }

    #[test]
    fn upsert_session_should_update_in_place_when_last_ingested_at_changes() {
        let dir = TempDir::new().unwrap();
        let store = ContextForensicsStore::open(&temp_store_path(&dir)).unwrap();

        store.upsert_session(&sample_session("s1")).unwrap();
        let mut updated = sample_session("s1");
        updated.last_ingested_at = "2026-01-02T00:00:00Z".to_string();
        store.upsert_session(&updated).unwrap();

        assert_eq!(store.session_row_count().unwrap(), 1);
        let fetched = store.get_session("s1").unwrap().unwrap();
        assert_eq!(fetched.last_ingested_at, "2026-01-02T00:00:00Z");
    }

    #[test]
    fn session_row_count_should_return_three_when_three_sessions_upserted() {
        let dir = TempDir::new().unwrap();
        let store = ContextForensicsStore::open(&temp_store_path(&dir)).unwrap();

        store.upsert_session(&sample_session("s1")).unwrap();
        store.upsert_session(&sample_session("s2")).unwrap();
        store.upsert_session(&sample_session("s3")).unwrap();

        assert_eq!(store.session_row_count().unwrap(), 3);
    }

    #[test]
    fn peak_context_tokens_should_return_max_cumulative_tokens_when_turns_exist() {
        let dir = TempDir::new().unwrap();
        let store = ContextForensicsStore::open(&temp_store_path(&dir)).unwrap();
        store.upsert_session(&sample_session("s1")).unwrap();
        store
            .upsert_turn(&TurnRow {
                id: "t1".to_string(),
                session_id: "s1".to_string(),
                turn_index: 0,
                user_row_uuid: "u1".to_string(),
                cumulative_tokens: 1000,
            })
            .unwrap();
        store
            .upsert_turn(&TurnRow {
                id: "t2".to_string(),
                session_id: "s1".to_string(),
                turn_index: 1,
                user_row_uuid: "u2".to_string(),
                cumulative_tokens: 850_000,
            })
            .unwrap();

        assert_eq!(store.peak_context_tokens("s1").unwrap(), 850_000);
    }

    #[test]
    fn peak_context_tokens_should_return_zero_when_session_has_no_turns() {
        let dir = TempDir::new().unwrap();
        let store = ContextForensicsStore::open(&temp_store_path(&dir)).unwrap();
        store.upsert_session(&sample_session("s1")).unwrap();

        assert_eq!(store.peak_context_tokens("s1").unwrap(), 0);
    }

    #[test]
    fn list_sessions_should_order_by_started_at_ascending() {
        let dir = TempDir::new().unwrap();
        let store = ContextForensicsStore::open(&temp_store_path(&dir)).unwrap();

        let mut later = sample_session("s-later");
        later.started_at = Some("2026-02-01T00:00:00Z".to_string());
        let mut earlier = sample_session("s-earlier");
        earlier.started_at = Some("2026-01-01T00:00:00Z".to_string());

        store.upsert_session(&later).unwrap();
        store.upsert_session(&earlier).unwrap();

        let sessions = store.list_sessions().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "s-earlier");
        assert_eq!(sessions[1].id, "s-later");
    }

    #[test]
    fn migration_should_be_reversible() {
        // See validation.md's dedicated Migration Test section: this store
        // is a derived, rebuildable cache. Up -> simulate a live-hook-only
        // row (stood in for here since hook_events lands in Phase 4; the
        // property under test is "transcript-derived tables round-trip
        // through drop-and-reingest") -> down (delete file) -> re-up
        // (re-open + re-upsert from the same "fixture") -> assert
        // transcript-derived rows match.
        let dir = TempDir::new().unwrap();
        let path = temp_store_path(&dir);

        let store = ContextForensicsStore::open(&path).unwrap();
        store.upsert_session(&sample_session("s1")).unwrap();
        store
            .upsert_turn(&TurnRow {
                id: "t1".to_string(),
                session_id: "s1".to_string(),
                turn_index: 0,
                user_row_uuid: "u1".to_string(),
                cumulative_tokens: 1200,
            })
            .unwrap();
        store
            .upsert_api_call(&sample_api_call("s1", "u1", 1200))
            .unwrap();
        drop(store);

        // Down: delete the file entirely (the plan's documented rollback
        // procedure).
        fs::remove_file(&path).unwrap();
        assert!(!path.exists());

        // Re-up: re-open (recreates schema) and re-ingest the same fixture.
        let store = ContextForensicsStore::open(&path).unwrap();
        store.upsert_session(&sample_session("s1")).unwrap();
        store
            .upsert_turn(&TurnRow {
                id: "t1".to_string(),
                session_id: "s1".to_string(),
                turn_index: 0,
                user_row_uuid: "u1".to_string(),
                cumulative_tokens: 1200,
            })
            .unwrap();
        store
            .upsert_api_call(&sample_api_call("s1", "u1", 1200))
            .unwrap();

        assert_eq!(store.session_row_count().unwrap(), 1);
        let turns = store.turns_for_session("s1").unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].cumulative_tokens, 1200);
        let calls = store.api_calls_for_session("s1").unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input_tokens, 1200);
    }

    fn cache_aware_pricing_table() -> PricingTable {
        let mut pricing = PricingTable::new();
        pricing.insert(
            "claude-sonnet-5",
            crate::cost_metrics::pricing::ModelPrice {
                input_usd_per_token: 0.000_003,
                output_usd_per_token: 0.000_015,
                cache_read_usd_per_token: 0.000_000_3,
                cache_creation_usd_per_token: 0.000_003_75,
            },
        );
        pricing
    }

    #[test]
    fn summary_for_all_sessions_should_compute_cost_per_call_using_cache_aware_rates() {
        let dir = TempDir::new().unwrap();
        let store = ContextForensicsStore::open(&temp_store_path(&dir)).unwrap();
        store.upsert_session(&sample_session("s1")).unwrap();

        let mut call = sample_api_call("s1", "u1", 1_000);
        call.output_tokens = 100;
        call.cache_creation_input_tokens = Some(2_000);
        call.cache_read_input_tokens = Some(10_000);
        store.upsert_api_call(&call).unwrap();

        let pricing = cache_aware_pricing_table();
        let summaries = store.summary_for_all_sessions(&pricing).unwrap();

        assert_eq!(summaries.len(), 1);
        // (1000 * 0.000003) + (100 * 0.000015) + (2000 * 0.00000375) + (10000 * 0.0000003)
        // = 0.003 + 0.0015 + 0.0075 + 0.003 = 0.015, over 1 call.
        let expected = 0.003 + 0.0015 + 0.0075 + 0.003;
        assert!(
            (summaries[0].cost_per_call_usd - expected).abs() < 1e-9,
            "expected {expected}, got {}",
            summaries[0].cost_per_call_usd
        );
        assert_eq!(summaries[0].call_count, 1);
    }

    #[test]
    fn summary_for_all_sessions_should_return_zero_cost_per_call_when_session_has_zero_calls() {
        let dir = TempDir::new().unwrap();
        let store = ContextForensicsStore::open(&temp_store_path(&dir)).unwrap();
        store.upsert_session(&sample_session("s1")).unwrap();

        let pricing = cache_aware_pricing_table();
        let summaries = store.summary_for_all_sessions(&pricing).unwrap();

        assert_eq!(summaries.len(), 1);
        // The zero-calls short-circuit in `summary_for_all_sessions`
        // returns the exact literal `0.0`, never an accumulated float, so
        // an exact comparison is correct here (not the usual float_cmp
        // footgun of comparing two independently-computed values).
        #[allow(clippy::float_cmp)]
        {
            assert_eq!(summaries[0].cost_per_call_usd, 0.0);
        }
        assert_eq!(summaries[0].call_count, 0);
    }

    #[test]
    fn summary_for_all_sessions_should_order_by_started_at_ascending_when_multiple_sessions_stored()
    {
        let dir = TempDir::new().unwrap();
        let store = ContextForensicsStore::open(&temp_store_path(&dir)).unwrap();

        let mut later = sample_session("s-later");
        later.started_at = Some("2026-08-03T00:00:00Z".to_string());
        let mut earliest = sample_session("s-earliest");
        earliest.started_at = Some("2026-08-01T00:00:00Z".to_string());
        let mut middle = sample_session("s-middle");
        middle.started_at = Some("2026-08-02T00:00:00Z".to_string());

        // Upsert out of chronological order to prove the response is
        // sorted by `started_at`, not insertion order.
        store.upsert_session(&later).unwrap();
        store.upsert_session(&earliest).unwrap();
        store.upsert_session(&middle).unwrap();

        let pricing = cache_aware_pricing_table();
        let summaries = store.summary_for_all_sessions(&pricing).unwrap();

        assert_eq!(
            summaries.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["s-earliest", "s-middle", "s-later"]
        );
    }
}
