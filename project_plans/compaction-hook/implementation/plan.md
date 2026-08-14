# Implementation Plan: compaction-hook

**Feature**: Port magic-compact's Claude Code session-transcript compaction (parse → prune bulky tool I/O with a retrievable omission cache → summarize old turns via subprocess → write a resumable destination transcript) into consolette as a new `src/claude_code_session/` module, a `consolette compact-session` CLI subcommand, and a `read_omitted_content` tool on consolette's native MCP server.
**Date**: 2026-08-13
**Status**: Ready for implementation
**ADRs**: ADR-008 (module placement + binary extension), ADR-009 (omission cache backend + query-shape session scoping, with a client-supplied-`session_id` residual risk pending Task 5.1.0's topology spike), ADR-010 (subprocess-only `Summarizer` trait), ADR-011 (compact-boundary row format, flagged for empirical verification)

---

## Requirement → open-question traceability

| requirements.md open question | Resolved by |
|---|---|
| Module location: `src/compression/` vs new top-level module | ADR-008 — new `src/claude_code_session/`, self-contained; does not call into `crate::compression::*` (see Story 2.1.1's algorithm note) |
| New `[[bin]]` vs subcommand | ADR-008 — extends `consolette` binary, new `Command::CompactSession` |
| Omission-cache retrieval via MCP with session-scoping | ADR-009 — `rusqlite` table keyed `(session_id, content_id)`, every query scoped by `session_id`, which eliminates magic-compact's suffix-scan enumeration bug but does **not** independently authenticate the caller's `session_id` (a client-supplied argument) — Task 5.1.0's spike determines whether a stronger, spawn-time binding is possible before this ships |
| `claude -p --resume` direct vs pluggable via `providers` | ADR-010 — local `Summarizer` trait, subprocess-only v1 impl, not `providers::Provider` |
| Where a `UserPromptSubmit`-hook-equivalent entry point plugs in | Phase 5, Story 5.2 — `consolette compact-session` CLI subcommand is the entry point for this pass; hook registration (`hooks.json`) is an explicit documented follow-up, not built here |

---

## Dependency Visualization

```
┌─────────────────────────────────────────────┐
│ Phase 1: Transcript foundations               │
│  Epic 1.1 Row model + JSONL parsing           │
│  Epic 1.2 Turn/chain reconstruction           │
│  Epic 1.3 Compact-boundary detection (ADR-011)│
└───────────────────┬───────────────────────────┘
                     │ TranscriptRow / Turn / BoundaryState are the substrate
        ┌────────────┼─────────────────┐
        ▼                              ▼
┌───────────────────────┐   ┌───────────────────────────┐
│ Phase 2: Pruning +      │   │ Phase 3: Summarization      │
│  omission cache          │   │  Epic 3.1 Summarizer trait  │
│  Epic 2.1 Prune rules    │   │  Epic 3.2 CLI subprocess impl│
│  Epic 2.2 rusqlite cache │   │  (ADR-010)                   │
│  (ADR-009)                │   └──────────────┬───────────────┘
└────────────┬───────────────┘                  │
             │                                    │
             └───────────────┬────────────────────┘
                              ▼
                 ┌─────────────────────────────────┐
                 │ Phase 4: Destination writer        │
                 │  Epic 4.1 Atomic write + boundary  │
                 │  row (ADR-011, empirical check)     │
                 └───────────────┬─────────────────────┘
                                  ▼
                 ┌─────────────────────────────────────────┐
                 │ Phase 5: Surfaces                          │
                 │  Epic 5.1 Task 5.1.0 topology spike -> Native MCP server + read_omitted_content (ADR-009) │
                 │  Epic 5.2 `consolette compact-session` CLI  │
                 └───────────────┬─────────────────────────────┘
                                  ▼
                 ┌─────────────────────────────────┐
                 │ Phase 6: Validation                 │
                 │  Epic 6.1 Unit + integration tests  │
                 │  Epic 6.2 Docs + follow-up notes     │
                 └─────────────────────────────────────┘
```

---

## Phase 1: Transcript Foundations

### Epic 1.1: Row model + JSONL parsing
**Goal**: A tagged, round-trip-preserving representation of a Claude Code session JSONL file that keeps tool_use/tool_result/system rows intact (unlike `src/learn/transcript.rs`, which discards them) and streams line-by-line (unlike `src/learn/transcript.rs`'s whole-file read).

#### Story 1.1.1: Parse a session JSONL file into typed rows without losing unknown fields
**As a** compactor, **I want** every transcript row deserialized into a structured type that preserves fields I don't yet understand, **so that** rewriting the destination transcript doesn't silently drop data from an evolving, undocumented schema.
**Acceptance Criteria**:
- `TranscriptRow` is a `#[serde(tag = "type")]` enum (`User`, `Assistant`, `System`, `#[serde(other)] Unknown`) where every variant struct uses `#[serde(flatten)] extra: serde_json::Map<String, Value>` to retain unrecognized fields.
- `parse_session_file(path: &Path) -> anyhow::Result<Vec<TranscriptRow>>` streams via `BufReader::lines()` (not `read_to_string`), matching `cmdcrush/main.rs`'s streaming pattern, not `learn/transcript.rs`'s whole-file pattern.
- An unparseable line is logged via `tracing::warn!` with the line number and skipped, not treated as a fatal error (matches the existing defensive-parse convention in `src/learn/transcript.rs`).
- A round-trip test: parse a fixture JSONL, re-serialize every row, and byte-diff key fields (`uuid`, `parentUuid`, `type`) to confirm no data loss on unknown-field rows.
- **Explicit decision on non-transcript-row top-level metadata** (`custom-title`, `ai-title`, `tag`, `worktree-state`, etc. — `research/features.md` §6/§9 flags these as real entries some Claude Code session files carry outside the row-per-line transcript body): this plan's `parse_session_file`/`write_destination_transcript` pair **intentionally drops them**, matching magic-compact's own known-lossy behavior rather than silently reproducing or silently fixing it. This is a deliberate parity choice, not an oversight — recorded here so it's an explicit acceptance criterion rather than an implicit gap, and listed again in "Follow-ups" as a candidate for a future pass if users report losing session titles/tags across compaction.
**Files**: `src/claude_code_session/mod.rs`, `src/claude_code_session/transcript.rs`

##### Task 1.1.1a: Define `TranscriptRow` and its per-type field structs (~5 min)
- Create `src/claude_code_session/transcript.rs` with `TranscriptRow` enum (`User`, `Assistant`, `System`, `Unknown`), each variant holding `uuid: String`, `parent_uuid: Option<String>`, `is_sidechain: bool` (`#[serde(default)]`), `is_meta: bool` (`#[serde(default)]`), `message: Option<serde_json::Value>`, `extra: serde_json::Map<String, Value>` via `#[serde(flatten)]`.
- Files: `src/claude_code_session/transcript.rs`

##### Task 1.1.1b: Implement streaming `parse_session_file` (~5 min)
- Add `pub fn parse_session_file(path: &Path) -> anyhow::Result<Vec<TranscriptRow>>` using `BufReader::new(File::open(path)?).lines()`, `serde_json::from_str::<TranscriptRow>(&line)` per line, `tracing::warn!(line_no, error, "skipping unparseable transcript row")` on error.
- Files: `src/claude_code_session/transcript.rs`

##### Task 1.1.1c: Add `mod.rs` skeleton and wire into `lib.rs` (~3 min)
- Create `src/claude_code_session/mod.rs` with `pub mod transcript; pub mod boundary; pub mod prune; pub mod omission_cache; pub mod summarize; pub mod writer;` (later phases fill in the remaining files — declare them now as empty stubs with `// TODO(Phase N)` so `mod.rs` compiles incrementally).
- Add `pub mod claude_code_session;` to `src/lib.rs`'s module list.
- Files: `src/claude_code_session/mod.rs`, `src/lib.rs`

##### Task 1.1.1d: Round-trip fixture test (~5 min)
- Add a `tests` submodule in `transcript.rs` using `tempfile::NamedTempFile` (matching `learn/transcript.rs`'s test style) with a small fixture JSONL (2-3 rows including one row with an unrecognized field), asserting `parse_session_file` preserves `uuid`/`parent_uuid`/`type` and the unrecognized field survives in `extra`.
- Files: `src/claude_code_session/transcript.rs`

### Epic 1.2: Turn/chain reconstruction
**Goal**: Group parsed rows into logical turns (user message → assistant response → tool calls/results) by following `parentUuid` links, matching magic-compact's `buildActiveChain`/`buildAssistantTurns`/`recoverParallelToolRows` behavior (`research/features.md`, citing `transcript.ts:114-258`).

#### Story 1.2.1: Reconstruct the active parent-chain and group rows into `Turn`s
**As a** compactor, **I want** rows grouped into turns with cycle-detection, **so that** pruning/summarization operate on logical units instead of raw JSONL lines, and a malformed transcript with a parent-chain cycle doesn't hang the compactor.
**Acceptance Criteria**:
- `Turn { user_row: TranscriptRow, assistant_rows: Vec<TranscriptRow>, tool_rows: Vec<TranscriptRow> }` and `pub fn build_turns(rows: &[TranscriptRow]) -> anyhow::Result<Vec<Turn>>`.
- Cycle detection: if following `parent_uuid` links revisits a `uuid` already seen in the current chain walk, return `Err` with a message identifying the offending `uuid` — do not loop forever (mirrors the "cycle-detection is real, not hypothetical" finding in `research/pitfalls.md`).
- Parallel tool-call rows (multiple `tool_use`/`tool_result` rows sharing one parent assistant `uuid`) are recovered into the same `Turn.tool_rows`, not split across turns (mirrors `recoverParallelToolRows`).
- Sidechain rows (`is_sidechain: true`) are excluded from the main chain. **This is flagged as unverified against real Claude Code transcripts** (`research/features.md` §10 — neither `src/learn/transcript.rs` nor `src/bin/cmdcrush/main.rs` was built for full-fidelity turn reconstruction, so "matches existing precedent" is not by itself confirmation this is correct for compaction's purposes); Task 1.2.1c's fixture-based test is the closest available check for this pass, and a real-transcript spot-check is recorded as a follow-up (see "Follow-ups" section) rather than blocking this plan on manual transcript collection.
- If a non-sidechain row's `parent_uuid` points at a `uuid` that was excluded as a sidechain row (a dangling reference, not a cycle), `build_turns` treats that row as if it has no parent — i.e. it starts a new chain/turn from that point rather than erroring — and logs `tracing::warn!(uuid, missing_parent = parent_uuid, "parent row excluded as sidechain or otherwise missing; starting new turn")`. This is a distinct failure mode from the cycle-detection `Err` path: a missing parent is tolerated and logged, a revisited parent is a hard error.
**Files**: `src/claude_code_session/transcript.rs`

##### Task 1.2.1a: Implement `build_turns` with cycle detection and dangling-parent handling (~5 min)
- Build the `uuid -> TranscriptRow` lookup map from **only non-sidechain rows** (`is_sidechain == false`) — this is the mechanism that makes sidechain exclusion and dangling-parent tolerance the same code path: a sidechain row is never inserted into the map, so any `parent_uuid` pointing at one is indistinguishable from a `parent_uuid` pointing at any other missing `uuid`, and both are handled by the dangling-reference branch below. Then follow `parent_uuid` chains from the last row backward using a `HashSet<String>` of visited uuids per walk; return `Err(anyhow!("parent-chain cycle at {uuid}"))` on revisit. If `parent_uuid` is `Some(uuid)` but `uuid` isn't in the map (dangling reference — either an excluded sidechain row or any other missing parent), log a warning and treat the row as chain-root rather than erroring.
- Files: `src/claude_code_session/transcript.rs`

##### Task 1.2.1b: Group chain rows into `Turn`s with parallel-tool-call recovery (~5 min)
- Fold the ordered chain into `Turn`s: a new `Turn` starts at each `User` row; subsequent `Assistant`/tool rows attach to the current turn until the next `User` row.
- Files: `src/claude_code_session/transcript.rs`

##### Task 1.2.1c: Unit tests for cycle detection, parallel-tool grouping, and dangling parents (~5 min)
- Fixture JSONL with (a) a normal 2-turn conversation, (b) two tool rows sharing one assistant parent, (c) a synthetic parent-chain cycle, (d) a row whose `parent_uuid` points at a sidechain row excluded from the row map; assert (a)/(b) group correctly, (c) returns `Err` without hanging (use a test timeout via `#[tokio::test(flavor = "current_thread")]`-style bound or a plain iteration-count assertion since `build_turns` is sync), and (d) logs a warning and starts a new chain rather than erroring.
- Files: `src/claude_code_session/transcript.rs`

### Epic 1.3: Compact-boundary detection (ADR-011)
**Goal**: Detect prior `consoletteCompact.boundary`/`consoletteCompact.summary` markers so re-running `compact-session` on an already-compacted transcript doesn't re-summarize already-summarized turns.

#### Story 1.3.1: Detect existing boundary/summary markers and compute a compaction plan
**As a** compactor, **I want** to know which turns are already-summarized vs. eligible for summarization, **so that** recompaction is idempotent (mirrors `createPlan`, `compact.ts:59-83`).
**Acceptance Criteria**:
- `pub struct CompactionPlan { prefix_turns: Vec<Turn>, turns_to_summarize: Vec<Turn>, preserved_turns: Vec<Turn> }` and `pub fn create_plan(turns: Vec<Turn>) -> CompactionPlan`.
- A turn whose row carries `extra["consoletteCompact"]["summary"] == true` is classified into `prefix_turns` (already summarized, preserved verbatim, never re-summarized).
- The most recent N turns (a `preserve_last_n_turns: usize` parameter, **default `0`, matching magic-compact's actual shipped default** — `research/features.md` §3 documents `keepTurns <= 0` → `compactionEndIndex = turns.length`, i.e. preserve nothing beyond the boundary, summarize everything up to the last turn. (An earlier draft of this story proposed a default of `3` without checking it against this research; that was an unverified guess, corrected here. `preserve_last_n_turns` remains a caller-settable parameter — see Story 5.2.1's CLI flag — for callers who want more conservative behavior, but `0` is the wire default.) are `preserved_turns` (kept verbatim, not summarized) — everything strictly between the last boundary/summary marker and the preserved tail is `turns_to_summarize`.
- Unit test: given a transcript with one prior boundary marker partway through, `create_plan` puts pre-marker turns in `prefix_turns` and does not include them in `turns_to_summarize`.
**Files**: `src/claude_code_session/boundary.rs`

##### Task 1.3.1a: Define `CompactionPlan` and marker-detection helper (~4 min)
- `fn is_boundary_or_summary_row(row: &TranscriptRow) -> bool` checking `extra.get("consoletteCompact")` for `boundary`/`summary` truthy fields.
- Files: `src/claude_code_session/boundary.rs`

##### Task 1.3.1b: Implement `create_plan` (~5 min)
- Split `turns` into the three buckets per the acceptance criteria above.
- Files: `src/claude_code_session/boundary.rs`

##### Task 1.3.1c: Unit test idempotent recompaction classification (~4 min)
- Fixture: 6 turns, turn 3 already marked `consoletteCompact.summary = true`; assert turns 1-3 land in `prefix_turns`, turns 4..(n - preserve_last_n) in `turns_to_summarize`, and the tail in `preserved_turns`.
- Files: `src/claude_code_session/boundary.rs`

---

## Phase 2: Pruning + Omission Cache

### Epic 2.1: Tool-I/O pruning rules
**Goal**: Prune bulky completed tool I/O per-tool-name using a self-contained binary omit-over-threshold check on raw content (no dependency on `crate::compression`'s primitives or the stateful `CompressionEngine`/`RewindStore` — see Story 2.1.1's algorithm note), matching magic-compact's `prune.ts` thresholds where research has recorded them.

#### Story 2.1.1: Apply per-tool-name pruning thresholds to tool_result content
**As a** compactor, **I want** large tool outputs pruned with a placeholder + omission-cache reference, **so that** the destination transcript stays small while full content stays retrievable.

**Algorithm (resolves the adversarial review's threshold-order ambiguity)**: the
threshold check is measured on **raw, uncompressed** content length/word-count —
matching magic-compact's own binary model (full-field-omit over a threshold, never
partial inline compression; `prune.ts:22-291` never runs a compression pass before
deciding). This plan intentionally does **not** reuse `crate::compression`'s
fallback chain (`diff_compactor`/`text_compressor`/`line_truncate`) inside
`prune_tool_row` at all — that would be a genuine, unacknowledged semantic
divergence from magic-compact's binary model, which requirements.md's own
constraint requires be called out explicitly rather than silently substituted.
`PrunedRow` therefore stays a clean binary enum (`Unchanged` / `Pruned`), with no
third "compressed-but-inline" state needed, because there is no compression step
in this path.
**Acceptance Criteria**:
- `pub fn prune_tool_row(row: &TranscriptRow, cache: &OmissionCache, session_id: &str) -> anyhow::Result<PrunedRow>` where `PrunedRow` is either `Unchanged(TranscriptRow)` or `Pruned { row: TranscriptRow, content_id: String }`.
- Threshold is evaluated on the **raw** content text (no compression pass): over 1024 chars or 128 words is pruned (matches `research/features.md`'s recorded `DEFAULT_LIMIT`); a documented per-tool override map exists for `Bash` (1024-char flat cutoff) and an "agent output" class (512 words/4096 chars), per the thresholds `research/features.md` extracted from `prune.ts:22-291`.
- Content that is pruned is cached **verbatim, uncompressed** — `read_omitted_content` must be able to return exactly what the tool originally produced, not a lossy compressed approximation of it (this is the property that makes the omission cache trustworthy as a full-fidelity retrieval path).
- Pruned content is written to the omission cache (Epic 2.2) before the row is rewritten with a placeholder string embedding the `content_id`.
- Unit test: a >1024-char plain-text tool result is pruned, its full **unmodified** content round-trips byte-for-byte through a fake in-memory cache; a <1024-char result is left `Unchanged` with its content untouched.
**Files**: `src/claude_code_session/prune.rs`

##### Task 2.1.1a: Define pruning thresholds and `PrunedRow` (~4 min)
- Constants `DEFAULT_LIMIT_CHARS = 1024`, `DEFAULT_LIMIT_WORDS = 128`, `AGENT_OUTPUT_LIMIT_CHARS = 4096`, `AGENT_OUTPUT_LIMIT_WORDS = 512`, `BASH_LIMIT_CHARS = 1024`; `PrunedRow` enum (`Unchanged(TranscriptRow)` / `Pruned { row: TranscriptRow, content_id: String }` — no third state).
- Files: `src/claude_code_session/prune.rs`

##### Task 2.1.1b: Implement `prune_tool_row` threshold check on raw content (~5 min)
- Extract tool name + content text from a `tool_result` row's `message` JSON (mirroring `cmdcrush/main.rs::tool_result_text`'s content-block-vs-string handling); measure raw length/word-count against the per-tool threshold; **do not** call into `crate::compression::*` here (see algorithm note above — this path is binary omit-or-keep, not compress-then-decide). `crate::compression`'s primitives remain reserved for consolette's separate proxy-compaction use case; this module does not call them.
- Files: `src/claude_code_session/prune.rs`

##### Task 2.1.1c: Wire omission-cache write + placeholder rewrite (~4 min)
- On prune, call `cache.insert(session_id, tool_name, full_content) -> content_id` with the **raw, unmodified** content, then replace the row's content field with a placeholder string (e.g. `"[pruned: see read_omitted_content(session_id, \"{content_id}\")]"`).
- Files: `src/claude_code_session/prune.rs`

##### Task 2.1.1d: Unit tests for threshold boundaries (~5 min)
- Table-driven test over the four threshold constants (Bash flat cutoff, default word/char limit, agent-output limit) confirming prune/no-prune decisions at boundary values, and confirming cached content is byte-identical to the original (no compression artifact).
- Files: `src/claude_code_session/prune.rs`

### Epic 2.2: `rusqlite` omission cache (ADR-009)
**Goal**: A session-scoped, long-lived omission cache — the direct fix for the `findSessionIdBySuffix` cross-session vulnerability documented in ADR-009.

#### Story 2.2.1: Session-scoped `OmissionCache` backed by `rusqlite`
**As a** retrieval caller, **I want** every lookup bound to a session ID I don't control, **so that** one session can never read another session's pruned tool output.
**Acceptance Criteria**:
- `OmissionCache::open(path: &Path) -> anyhow::Result<Self>` runs `CREATE TABLE IF NOT EXISTS omitted_content (session_id TEXT NOT NULL, content_id TEXT NOT NULL, content TEXT NOT NULL, tool_name TEXT NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY (session_id, content_id))` (schema per ADR-009), sets `PRAGMA journal_mode=WAL` and a `busy_timeout` of 5000ms on the connection (ADR-009's concurrency hardening), and — on first creation of the file/directory — sets `0600` permissions on the sqlite file and `0700` on its parent directory via `std::os::unix::fs::PermissionsExt` (ADR-009, addressing `research/pitfalls.md` §5's file-permission gap).
- `insert(&self, session_id: &str, tool_name: &str, content: &str) -> anyhow::Result<String>` generates a per-session-incrementing `content_id` (e.g. `"omitted-{n:03}"`, n = count of existing rows for that session + 1) and inserts, with the count-then-insert sequence wrapped in a single `rusqlite` transaction so concurrent inserts for the same `session_id` (parallel tool-row pruning, or a double-fired hook) cannot race on the count or fail with `SQLITE_BUSY`.
- `get(&self, session_id: &str, content_id: &str) -> anyhow::Result<Option<String>>` — the query's `WHERE` clause includes **both** `session_id` and `content_id`; there is no method on `OmissionCache` that accepts a bare `content_id`.
- Unit test (the ADR-009-mandated one): insert content under `session_id = "A"`, then call `get("B", <A's content_id>)` and assert `Ok(None)` — proving cross-session retrieval is structurally impossible, not just policy.
**Files**: `src/claude_code_session/omission_cache.rs`

##### Task 2.2.1a: Define `OmissionCache::open`, schema creation, and connection/file hardening (~5 min)
- `rusqlite::Connection::open(path)`, run the `CREATE TABLE IF NOT EXISTS` statement, then `PRAGMA journal_mode=WAL` and `PRAGMA busy_timeout=5000`. After creating the parent directory (`create_dir_all`) and confirming the sqlite file exists, set `0700`/`0600` permissions via `std::fs::set_permissions` + `std::os::unix::fs::PermissionsExt::from_mode`.
- Files: `src/claude_code_session/omission_cache.rs`

##### Task 2.2.1b: Implement `insert` with per-session content-id numbering in a transaction (~5 min)
- Wrap in `let tx = conn.transaction()?;`: `SELECT COUNT(*) FROM omitted_content WHERE session_id = ?1` to compute the next `omitted-NNN`, then `INSERT`, then `tx.commit()`.
- Files: `src/claude_code_session/omission_cache.rs`

##### Task 2.2.1c: Implement `get` scoped by `(session_id, content_id)` (~3 min)
- `SELECT content FROM omitted_content WHERE session_id = ?1 AND content_id = ?2`.
- Files: `src/claude_code_session/omission_cache.rs`

##### Task 2.2.1d: Cross-session isolation test (ADR-009 acceptance criterion) (~4 min)
- The test described in Story 2.2.1's acceptance criteria, using `tempfile::NamedTempFile` for the sqlite path.
- Files: `src/claude_code_session/omission_cache.rs`

##### Task 2.2.1e: Default cache path resolution (~3 min)
- `pub fn default_cache_path() -> PathBuf` returning `~/.claude/consolette/omission-cache.sqlite`, creating the parent dir if missing (`std::fs::create_dir_all`), following `main.rs::config_dir()`'s `HOME`-env-var resolution convention.
- Files: `src/claude_code_session/omission_cache.rs`, `src/main.rs` (expose/reuse the `HOME` resolution helper if `config_dir()` is refactored to be shared — else duplicate the one-line `HOME` lookup, noting it in a comment rather than adding a premature abstraction)

---

## Phase 3: Summarization (ADR-010)

### Epic 3.1: `Summarizer` trait
**Goal**: A minimal, mockable seam over "summarize these turns," per ADR-010.

#### Story 3.1.1: Define `Summarizer` and its output type
**As a** compactor, **I want** summarization behind a trait, **so that** `mod.rs`'s orchestration is testable without invoking a real subprocess.
**Acceptance Criteria**:
- `pub struct TurnSummary { pub covers_turn_uuids: Vec<String>, pub summary_text: String }`.
- `#[async_trait::async_trait] pub trait Summarizer { async fn summarize(&self, session_id: &str, turns: &[Turn]) -> anyhow::Result<Vec<TurnSummary>>; }` (checks `Cargo.toml` for an existing `async-trait` dependency; if absent, use a plain `fn summarize(...) -> BoxFuture<'_, anyhow::Result<Vec<TurnSummary>>>` to avoid adding a new crate — confirm which at implementation time by checking `Cargo.toml`'s current dependency list).
**Files**: `src/claude_code_session/summarize.rs`

##### Task 3.1.1a: Define `TurnSummary` and the `Summarizer` trait (~4 min)
- As specified above; check `Cargo.toml` first for `async-trait` before deciding the trait's async signature strategy.
- Files: `src/claude_code_session/summarize.rs`, `Cargo.toml` (only if `async-trait` needs adding — prefer the `tokio`-native `impl Future` return if it avoids a new dependency)

##### Task 3.1.1b: `FakeSummarizer` test double (~3 min)
- A test-only `FakeSummarizer` returning caller-supplied canned `TurnSummary`s, used by Phase 4's orchestration tests.
- Files: `src/claude_code_session/summarize.rs`

### Epic 3.2: `ClaudeCliSummarizer` subprocess implementation
**Goal**: The v1 `Summarizer` impl, following `src/auth/exec.rs`'s subprocess pattern exactly (resolve, check permissions, spawn, timeout, uniform error handling, never log stdout/stderr).

#### Story 3.2.1: Shell out to `claude -p --resume <session_id>` and parse `<summary>` tags
**As a** compactor, **I want** a real subprocess-backed summarizer, **so that** old turns get genuinely summarized using the target session's own accumulated context.
**Acceptance Criteria**:
- `ClaudeCliSummarizer::new(command: Option<PathBuf>) -> Self` resolves `claude` via `PATH` if `command` is `None`, following `src/auth/exec.rs::resolve_command`'s exact contains-a-`/`-else-`PATH`-scan logic (reused via a shared helper if practical, else duplicated with a comment pointing at `auth/exec.rs` as the source pattern).
- Spawns via `tokio::process::Command`, `stdin`/`stdout`/`stderr` piped, wraps the wait in `tokio::time::timeout` (a `timeout: Duration` field, **default 150s — matching magic-compact's own end-to-end hook timeout budget**, `research/features.md` §7, rather than an unmeasured 60s guess from an earlier draft of this story). On timeout, `SummarizeError::Timeout` is returned and `compact_session` (Story 5.2.1) aborts the whole run without writing a partial destination transcript (the atomic writer already guarantees this) — no retry and no partial-success path in v1; if `turns_to_summarize` is large enough to routinely exceed 150s, that's a documented follow-up (chunked/incremental summarization), not solved here.
- Non-zero exit, timeout, or unparseable stdout all become one typed `SummarizeError` variant surfaced uniformly (matches `auth/exec.rs`'s "treat all of these as unavailable" convention) — stdout/stderr content is never included in logged error text (matches ADR-007 §6's redaction rule, applied here even though this isn't an auth helper).
- `<summary>...</summary>` tags are parsed out of stdout per-turn-group (mirrors `parseSummaries`, `compact.ts:518-554`) into `TurnSummary`s.
- Integration test gated behind an env var (e.g. `COMPACTION_HOOK_LIVE_CLAUDE_TEST=1`) that actually invokes `claude` if present, skipped by default in CI (no `claude` CLI in CI) — plus an unmocked unit test of the `<summary>` tag parser against a canned stdout string, which runs unconditionally.
**Files**: `src/claude_code_session/summarize.rs`

##### Task 3.2.1a: Implement command resolution + permission check (~5 min)
- Mirror `auth/exec.rs::resolve_command`/`check_permissions`; call out in a doc comment that this duplicates that logic deliberately (small, stable, not worth a shared crate-internal helper across two independent subsystems yet).
- Files: `src/claude_code_session/summarize.rs`

##### Task 3.2.1b: Implement subprocess spawn + timeout + uniform error handling (~5 min)
- `tokio::process::Command::new(resolved).arg("-p").arg("--resume").arg(session_id)...spawn()`, `tokio::time::timeout(self.timeout, child.wait_with_output())`.
- Files: `src/claude_code_session/summarize.rs`

##### Task 3.2.1c: Implement `<summary>` tag parser (~5 min)
- `fn parse_summaries(stdout: &str, turns: &[Turn]) -> anyhow::Result<Vec<TurnSummary>>`, matching each `<summary>` block to its corresponding turn group by order (per `parseSummaries`, `compact.ts:518-554`).
- Files: `src/claude_code_session/summarize.rs`

##### Task 3.2.1d: Unit test the tag parser + gated live-CLI integration test (~5 min)
- Files: `src/claude_code_session/summarize.rs`

---

## Phase 4: Destination Writer (ADR-011)

### Epic 4.1: Atomic destination-transcript write with boundary row
**Goal**: Assemble the destination JSONL (prefix turns → summarized turns → preserved turns, per magic-compact's write order) and write it atomically, with the boundary row shape from ADR-011.

#### Story 4.1.1: Assemble and atomically write the destination session file
**As a** user, **I want** a new resumable session file, **so that** I can `/resume` into a compacted session without losing tool-call structure or user messages.
**Acceptance Criteria**:
- `pub fn write_destination_transcript(plan: &CompactionPlan, summaries: &[TurnSummary], out_path: &Path) -> anyhow::Result<String>` (returns the new destination session ID) writes rows in order: boundary row first, then `prefix_turns` verbatim, then one row per `TurnSummary` (each stamped `consoletteCompact.summary = true`), then `preserved_turns` verbatim — matching `compact.ts:145-261`'s write order.
- All non-preserved rows (the boundary row and summary rows) share one flattened timestamp (matches magic-compact's behavior per `research/features.md`), generated once via `chrono::Utc::now()`.
- The boundary row is exactly the shape decided in ADR-011 (`type: "user"`, `isMeta: true`, `consoletteCompact: { boundary: true, sourceSessionId, prunedCount }`).
- Write is atomic: content is written to `out_path.with_extension("tmp")` then `std::fs::rename`d into place, following `src/bin/mcp-proxy/metrics.rs`'s existing pattern exactly (not a new idiom).
- Unit test: assemble a small plan + summaries, write to a tempdir path, re-parse the written file with `transcript::parse_session_file` and assert row order and boundary-row shape.
**Files**: `src/claude_code_session/writer.rs`, `src/claude_code_session/boundary.rs`

##### Task 4.1.1a: Implement boundary-row construction (~4 min)
- `fn build_boundary_row(source_session_id: &str, pruned_count: usize, timestamp: &str) -> TranscriptRow` per ADR-011's shape.
- Files: `src/claude_code_session/boundary.rs`

##### Task 4.1.1b: Implement destination assembly in write order (~5 min)
- Build the ordered `Vec<TranscriptRow>` per the write order above.
- Files: `src/claude_code_session/writer.rs`

##### Task 4.1.1c: Implement atomic tmp-then-rename write (~4 min)
- Serialize each row as one JSON line, write to `.tmp`, `std::fs::rename`, following `mcp-proxy/metrics.rs::write_session_start`'s pattern.
- Files: `src/claude_code_session/writer.rs`

##### Task 4.1.1d: Round-trip write/re-parse unit test (~5 min)
- Files: `src/claude_code_session/writer.rs`

##### Task 4.1.1e: Manual empirical verification against live Claude Code `/resume` (~5 min, one-time, not automated)
- Run `consolette compact-session` (once Phase 5's CLI exists) against a real `~/.claude/projects/**/*.jsonl` transcript, then run `claude --resume <printed-session-id>` and confirm Claude Code loads it without error. Record the outcome (pass/fail, and if fail, what Claude Code reported) as an update to ADR-011's Status field — this task is a checkpoint, not something to skip because it isn't a compiled test.
- Files: `project_plans/compaction-hook/decisions/ADR-011-compact-boundary-row-format.md` (Status/outcome update only)

---

## Phase 5: Surfaces

### Epic 5.1: Native MCP server + `read_omitted_content` (ADR-009)
**Goal**: Implement consolette's first-party stdio MCP server (currently `main.rs`'s `mcp()` stub) and register `read_omitted_content`, following `src/bin/mcp-proxy/server.rs`'s manual `ServerHandler` pattern (hand-written `list_tools`/`call_tool` match, not a `#[tool]`-macro style — matches both existing `ServerHandler` impls in this codebase).

##### Task 5.1.0: Spike — determine Claude Code's MCP server launch topology (~5 min, precedes Story 5.1.1)
- Required by ADR-009's blocker resolution before `read_omitted_content`'s `session_id` trust model is finalized. Check (via `research/pitfalls.md`'s citations, Claude Code's own docs/config schema, and if necessary a manual local test: configure a native MCP server in two different project directories' `.claude` configs and observe whether Claude Code spawns one long-lived shared process or one process per session/project) whether Claude Code launches a native stdio MCP server **per session** (in which case consolette can bind `session_id` at process-spawn time from the launching hook's own environment — a real authentication boundary) or as a **single shared/global process** across sessions (magic-compact's actual topology per `research/pitfalls.md`, in which case v1 ships with the client-supplied `session_id` design ADR-009 already describes as an accepted residual risk).
- Record the finding as an update to ADR-009 (either confirming the accepted-residual-risk design, or replacing it with the per-session-binding design if the topology allows it) before Story 5.1.1's implementation tasks begin.
- Files: `project_plans/compaction-hook/decisions/ADR-009-omission-cache-and-mcp-session-scoping.md` (Status/finding update only)

#### Story 5.1.1: Implement `ServerHandler` for consolette's own MCP server
**As** Claude Code (as an MCP client), **I want** a `read_omitted_content(session_id, content_id)` tool, **so that** I can retrieve pruned tool output on demand without it bloating the compacted transcript.

**Note on Task 5.1.0's outcome**: the acceptance criteria below describe the
**client-supplied-`session_id`** design (Claude Code launches one shared/global
MCP process; `session_id` is a tool-call argument, per ADR-009's accepted
residual-risk framing). **If Task 5.1.0's spike instead finds Claude Code
launches one MCP server process per session**, this story's schema changes
structurally: `read_omitted_content` takes only `content_id` (no
`session_id` argument at all), and `session_id` is bound once at process
spawn time from the launching hook's own environment/argv, then closed over
by `CompactionMcpServer`. In that case, re-scope this story's acceptance
criteria and Tasks 5.1.1a/b to the spawn-time-binding design before
implementing — do not implement the client-supplied-argument version if the
spike found per-session topology.
**Acceptance Criteria** (client-supplied-`session_id` design; see note above for the per-session-topology alternative):
- `src/claude_code_session/mcp_server.rs` defines `CompactionMcpServer { cache: OmissionCache }` implementing `rmcp::ServerHandler`, with `list_tools` returning one tool (`read_omitted_content`, JSON schema `{session_id: string, content_id: string}`) and `call_tool` dispatching to `OmissionCache::get`.
- `call_tool` returns a clear "not found" `CallToolResult` (not an error) when `(session_id, content_id)` doesn't match a row — distinguishing "wrong session" from "server error" is not exposed to the caller (no information leak about whether the `content_id` exists under a *different* session).
- `main.rs`'s `mcp()` function is implemented (no longer `anyhow::bail!`): it constructs `CompactionMcpServer`, serves it over `rmcp::transport::io::stdio()` via `ServiceExt::serve`, and awaits `service.waiting()`, following `mcp-proxy/main.rs::run_serve`'s exact shape (lines 56-88 read directly from that file).
- Unit test: construct `CompactionMcpServer` with a pre-populated `OmissionCache`, call `call_tool` directly with a valid and an invalid `session_id`, assert correct/absent results respectively (the ADR-009 cross-session guarantee, exercised at the MCP-tool layer this time, not just the cache layer).
**Files**: `src/claude_code_session/mcp_server.rs`, `src/main.rs`

##### Task 5.1.1a: Define `CompactionMcpServer` and `list_tools` (~5 min)
- Files: `src/claude_code_session/mcp_server.rs`

##### Task 5.1.1b: Implement `call_tool` dispatch for `read_omitted_content` (~5 min)
- Files: `src/claude_code_session/mcp_server.rs`

##### Task 5.1.1c: Wire `main.rs`'s `mcp()` to serve `CompactionMcpServer` over stdio (~4 min)
- Replace the `anyhow::bail!("MCP server not yet implemented")` stub with the `ServiceExt::serve(stdio())` call, resolving the omission-cache path via `omission_cache::default_cache_path()`.
- Files: `src/main.rs`

##### Task 5.1.1d: MCP-layer cross-session isolation test (~4 min)
- Files: `src/claude_code_session/mcp_server.rs`

### Epic 5.2: `consolette compact-session` CLI subcommand
**Goal**: The `UserPromptSubmit`-hook-equivalent entry point for this pass — a CLI subcommand sufficient on its own, with hook wiring documented as a follow-up (per requirements.md's explicit resolution of that open question).

#### Story 5.2.1: Add `Command::CompactSession` and orchestrate the full pipeline
**As a** user (or a future hook script), **I want** `consolette compact-session --session <path>` to run parse→plan→prune→summarize→write end to end, **so that** I get a printed new session ID to `/resume`.
**Acceptance Criteria**:
- `Command::CompactSession { session: PathBuf, preserve_last_n_turns: Option<usize> }` added to `src/main.rs`'s `Command` enum.
- A new `compact_session(session: &Path, preserve_last_n_turns: usize) -> anyhow::Result<String>` in `src/claude_code_session/mod.rs` orchestrates: `transcript::parse_session_file` → `transcript::build_turns` → `boundary::create_plan` → per-turn `prune::prune_tool_row` over tool rows → `ClaudeCliSummarizer::summarize` on `turns_to_summarize` → `writer::write_destination_transcript` → returns the new session ID.
- `main.rs`'s new `Command::CompactSession` arm calls `claude_code_session::compact_session(...)` and prints `"Resume with: claude --resume <session_id>"` on success (behavior parity with magic-compact's user-facing instruction, not string-for-string per requirements.md's explicit scope note).
- Failure at any pipeline stage (parse error, cycle detection, summarizer subprocess failure) surfaces as a non-zero exit with the underlying `anyhow::Error`'s context chain printed — no silent partial writes (the atomic writer from Phase 4 guarantees this at the file level).
- Integration test: a fixture session JSONL with 6+ turns and one oversized tool result end-to-end through `compact_session`, injecting the test-only `FakeSummarizer` (Task 3.1.1b) rather than `ClaudeCliSummarizer` — this test must not depend on the `claude` CLI being present and must run unconditionally in CI, unlike Story 3.2.1's `COMPACTION_HOOK_LIVE_CLAUDE_TEST=1`-gated real-subprocess test. Asserts the returned session ID's file exists, parses back cleanly, and the oversized tool result was replaced with a placeholder retrievable from the same `OmissionCache` instance.
**Files**: `src/main.rs`, `src/claude_code_session/mod.rs`

##### Task 5.2.1a: Add `Command::CompactSession` variant (~3 min)
- Files: `src/main.rs`

##### Task 5.2.1b: Implement `claude_code_session::compact_session` orchestration (~5 min)
- Files: `src/claude_code_session/mod.rs`

##### Task 5.2.1c: Wire the `main.rs` arm + user-facing print (~3 min)
- Files: `src/main.rs`

##### Task 5.2.1d: End-to-end fixture integration test (~5 min)
- Files: `src/claude_code_session/mod.rs`

---

## Phase 6: Validation

### Epic 6.1: Cross-cutting test coverage
**Goal**: Confirm the ADR-009 security guarantee and ADR-011's idempotency property hold at the full-pipeline level, not just per-module.

#### Story 6.1.1: Full-pipeline security and idempotency tests
**As a** maintainer, **I want** tests that would fail if the cross-session vulnerability or non-idempotent recompaction regressed, **so that** these two explicitly-flagged risks stay caught by CI, not just by manual review.
**Acceptance Criteria**:
- A test running `compact_session` twice on the same source transcript (second run using the first run's output as input) asserts the second run's `turns_to_summarize` excludes everything the first run already summarized (ADR-011's idempotency property, exercised end-to-end).
- A test running `compact_session` on two different fixture sessions against the same shared `OmissionCache` instance/path, then calling `read_omitted_content`-equivalent retrieval with session A's `content_id` under session B's `session_id`, asserts `None`/"not found" (ADR-009's guarantee, exercised end-to-end, not just at the `OmissionCache` unit level already covered in Task 2.2.1d).
- `cargo clippy --all-targets -- -D warnings` passes with no new `unwrap_used`/`expect_used`/`pedantic` allowances introduced beyond what's already in `[lints.clippy]`.
**Files**: `src/claude_code_session/mod.rs` (test module), or a new `tests/compaction_hook.rs` integration-test file if `mod.rs`'s existing unit tests make an in-module integration test unwieldy — decide at implementation time based on fixture size.

##### Task 6.1.1a: End-to-end idempotent-recompaction test (~5 min)
- Files: `src/claude_code_session/mod.rs` or `tests/compaction_hook.rs`

##### Task 6.1.1b: End-to-end cross-session isolation test (~5 min)
- Files: `src/claude_code_session/mod.rs` or `tests/compaction_hook.rs`

##### Task 6.1.1c: Run and fix clippy (~4 min)
- `cargo clippy --all-targets -- -D warnings`; address any `pedantic`/`unwrap_used`/`expect_used` findings in the new module.
- Files: any `src/claude_code_session/*.rs` file clippy flags

### Epic 6.2: Documentation of follow-ups
**Goal**: Explicitly record what's deliberately out of scope for this pass so it isn't silently forgotten (per requirements.md's Out of Scope section).

#### Story 6.2.1: Record documented follow-ups
**As a** future implementer, **I want** the deferred items written down in one place, **so that** "hook wiring is a documented follow-up" (requirements.md) actually gets documented.
**Acceptance Criteria**:
- A short "Follow-ups" section is added to `project_plans/compaction-hook/implementation/plan.md` (this file) listing: (1) `UserPromptSubmit` hook registration (`hooks.json`, plugin marketplace metadata) — out of scope per requirements.md; (2) omission-cache eviction/GC (`consolette compact-session --gc`) — noted in ADR-009 as unbuilt in v1; (3) deriving MCP `session_id` from transport-level connection identity instead of a client-supplied argument, once consolette's MCP server gains per-connection session binding — noted in ADR-009.
**Files**: `project_plans/compaction-hook/implementation/plan.md` (this section, below)

##### Task 6.2.1a: Write the Follow-ups section (~3 min)
- Files: `project_plans/compaction-hook/implementation/plan.md`

---

## Follow-ups (deliberately out of scope for this implementation pass)

1. **Hook registration.** Wiring `consolette compact-session` into a live Claude Code `UserPromptSubmit` hook (`hooks.json`, plugin marketplace metadata) is out of scope per requirements.md — the CLI subcommand is the complete, sufficient entry point for this design.
2. **Omission-cache eviction.** No GC/eviction policy ships in v1 (ADR-009) — the cache grows unboundedly, matching magic-compact's own behavior, and a failed/aborted `compact_session` run can additionally leave orphaned rows keyed to a destination `session_id` whose transcript was never written (the atomic writer prevents a partial *transcript* but doesn't roll back cache inserts already committed during pruning before a later stage fails). Both are accepted as the same unbounded-growth tradeoff for v1. A `consolette compact-session --gc` mode is a natural follow-up if cache size becomes a real problem.
3. **MCP `session_id` transport binding.** `read_omitted_content`'s `session_id` is a client-supplied argument in v1, not derived from an independently-verified per-connection identity (ADR-009), unless Task 5.1.0's spike finds Claude Code launches one MCP process per session, in which case binding at spawn time becomes the v1 design instead. Revisit either way if consolette's MCP transport later gains connection-level identity.
4. **Empirical boundary-row verification outcome.** Task 4.1.1e's live `/resume` check result should be folded back into ADR-011's Status field once run; if it fails, the fallback (`system`/`compact_boundary` per magic-compact's docs) becomes a new task, not a plan rewrite.
5. **Top-level session metadata (`custom-title`, `ai-title`, `tag`, `worktree-state`).** Dropped by `parse_session_file`/`write_destination_transcript` in this pass (Story 1.1.1) — a deliberate parity choice matching magic-compact's own known-lossy behavior, not an oversight. Revisit if users report losing session titles/tags across compaction.
6. **Sidechain-exclusion real-transcript verification.** Story 1.2.1's sidechain-row exclusion is validated only against synthetic fixtures in this pass; a spot-check against real `~/.claude/projects/**/*.jsonl` transcripts (to confirm the exclusion doesn't silently drop rows a full-fidelity reconstruction should keep) is recorded here rather than blocking the plan on manual transcript collection.
7. **Chunked/incremental summarization for oversized `turns_to_summarize` sets.** Story 3.2.1's `ClaudeCliSummarizer` has a single 150s timeout for the whole `turns_to_summarize` batch, with no retry and no partial-success path — if a session accumulates enough unsummarized turns to routinely exceed that budget, the fix is splitting the batch into multiple subprocess invocations (or summarizing incrementally as the transcript grows) rather than raising the timeout further. Not built in this pass; revisit if real usage hits the timeout.
