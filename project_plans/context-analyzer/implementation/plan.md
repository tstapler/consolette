# Implementation Plan: context-analyzer

**Feature**: Context-window forensics inside consolette — per-turn token composition, budget-threshold growth tracking, cross-session analytics, message inspection, and (later) Codex ingestion and Claude Code hook capture, all served from consolette's existing dashboard/MCP surface rather than a second standalone tool.
**Date**: 2026-08-24
**Status**: Ready for implementation
**ADRs**: ADR-001 (new persistent store), ADR-002 (settings.json hook install/uninstall), ADR-003 (defer headroom audit), ADR-004 (subagent token attribution)

---

## Step 0.5 — Alternatives considered

Three shapes were compared for where this feature lives and how it's reached:

**A. New top-level `src/context_forensics/` module** — its own `rusqlite` store, reusing `claude_code_session::{discovery, transcript, native_compaction}` for parsing, merged into the existing `serve-cost` process's router (one URL, one process).
- *Strength*: reuses the two hardest-won pieces of prior art (`transcript.rs`'s unknown-field-preserving parser, `omission_cache.rs`'s `rusqlite` hardening pattern) without bolting new, out-of-scope responsibilities onto modules whose doc comments explicitly scope them elsewhere (`claude_code_session/mod.rs`: "Claude Code session transcript compaction"; `cost_metrics/mod.rs`: "actual-vs-counterfactual... for `SessionCompactionPipeline`").
- *Weakness*: duplicates the "batch scan + periodic refresh" plumbing `session_bi.rs` already built once, rather than sharing it directly.

**B. Extend `cost_metrics::store::SessionCostStore` with a `rusqlite` persistence layer underneath its existing `moka` cache.**
- *Strength*: one store surface; existing `/v1/cost/*` consumers and `CostTracker` don't need to learn a second store.
- *Weakness*: `SessionCostStore`'s `max_capacity(1000)` / 1-hour-TTL semantics exist specifically so it's safe to lose on restart — durable cross-session history is the opposite requirement. Fighting a cache's own eviction invariants to make it durable is worse than building the small store this problem actually needs.

**C. Standalone sibling binary (like `cmdcrush`) with its own store, dashboard server, and CLI, only loosely linked from the existing dashboard.**
- *Strength*: total isolation — no risk of destabilizing `cost_metrics`/`claude_code_session` while iterating.
- *Weakness*: this is exactly the "second standalone tool" the Problem Statement says Tyler is trying to escape, and directly contradicts the Success Metric's "without leaving consolette" and the UX research's "one URL, not three" finding.

**Chosen: A.** B is ruled out by a real semantic conflict (already resolved as an open question in requirements.md and independently confirmed by `research/stack.md` §1 and `research/architecture.md` §1). C is ruled out by an explicit product requirement, not a technical one. Both are recorded in the Pattern Decisions table below where they recur at the component level (store pattern, router wiring).

---

## Step 1: System type

This is a local, single-user **ETL + reporting** system: batch-ingest append-only log files (Claude Code JSONL transcripts, Codex rollout logs, and event-driven hook payloads) into a small relational store, then serve read-side aggregation queries to a dashboard and an MCP server. It is **not** a rich-behavior domain model — there are no multi-step business transactions, no aggregates enforcing invariants across writes beyond simple idempotent upserts. Per PoEAA's own guidance (match pattern to complexity — don't reach for Domain Model on a CRUD/reporting-shaped problem), the ingestion layer is written as Transaction Scripts over an explicit schema, with a thin Repository (`ContextForensicsStore`) as the sole write/read boundary. The one place with genuine multi-step state is hook install/uninstall (a two-state idempotent machine), scoped narrowly in Phase 4.

---

## Domain Glossary

| Term | Definition | Notes |
|------|-----------|-------|
| `Session` | Reused, unchanged: consolette's existing unit of one Claude Code or Codex transcript. | Not renamed to "conversation"/"run" — matches `research/ux.md` §2. |
| `Turn` | Reused, unchanged, from `claude_code_session::transcript::Turn` — one user row plus the assistant/tool rows before the next genuine user row. | Not redefined; the store persists a `TurnRow` derived from it, see below. |
| `SessionRow` | Persisted record in `context_forensics::store` for one session: id, `Source`, path, project, timestamps, `chain_coverage_ratio`, `parse_failure_count`. | New. `Row`-suffixed to avoid colliding with `claude_code_session`'s own in-memory `TranscriptRow`/`Turn` types. |
| `TurnRow` | Persisted per-turn record: session id, turn index, user row uuid, cumulative token count at that turn. | New. |
| `ApiCallRow` | Persisted per-API-call record: session id, turn id, source row uuid, `AnthropicUsage` fields, `CompositionBreakdown` fields, `UsageProvenance`. | New. One assistant transcript row = one API call for Claude Code. `cache_creation_input_tokens`/`cache_read_input_tokens` are nullable `INTEGER` from the Phase 1 schema onward (not tightened-then-loosened) — Claude Code always writes a value, Codex (Phase 3) writes `NULL` for the field it has no equivalent for, since `CREATE TABLE IF NOT EXISTS` can never loosen a constraint later. |
| `AnthropicUsage` | The Anthropic Messages API `usage` object shape: `input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`. | Defined once in `src/providers/mod.rs`, reused (not reimplemented) by transcript-side extraction — same JSON shape, two sources. |
| `CompositionCategory` | Sum type: `ToolIo \| Conversation \| System`. | New vocabulary, adopted verbatim from the reference tool per `research/ux.md` §2 (deliberately not folded into consolette's existing compaction-tier vocabulary — orthogonal axis). |
| `CompositionBreakdown` | `{ tool_io_tokens, conversation_tokens, system_tokens }` for one call or turn. | New. |
| `Source` | Sum type: `ClaudeCode \| Codex`. Session-level discriminant. | New. Stored as a `CHECK`-constrained `TEXT` column, matched exhaustively in Rust. |
| `UsageProvenance` | Sum type: `TranscriptExact \| ProxyCaptured`. Marks whether a call's `AnthropicUsage` came from a transcript row or the proxy's own capture. | New. Enforces "transcript is primary" at write time, not query time (`research/pitfalls.md` §4). |
| `BudgetThreshold` | Newtype wrapping a `u64` token count, one of the four presets (200K/500K/700K/1M) or custom. | New. `crossed(peak_tokens) -> bool`. |
| `NativeCompactionEvent` | Reused, unchanged, from `claude_code_session::native_compaction`. | Not reparsed a second time. |
| `ChainCoverage` | Reused, unchanged, from `claude_code_session::transcript`. Threaded through to every aggregate that sums "total tokens" so a `<1.0` ratio is never silently dropped (`research/pitfalls.md` §2). | |
| `HookEventKind` | Sum type of the ten Claude Code hook names this feature captures: `PostToolUse, PostToolUseFailure, PreCompact, PostCompact, SessionStart, SessionEnd, UserPromptSubmit, SubagentStart, SubagentStop, InstructionsLoaded`. | New, Phase 4. |
| `HookEventRow` | Persisted hook-event record: session id (nullable — some events precede session-id assignment), `HookEventKind`, raw JSON payload, received timestamp. | New, Phase 4. |
| `SubagentRow` | Persisted subagent (Task-tool) record: parent session/turn id, start/end timestamps. | New, Phase 4. See ADR-004 for rollup decision. **v1 scope**: populated from `SubagentStart`/`SubagentStop` hook-event timestamps only — invocation count/duration. `AnthropicUsage` totals (parsed from `isSidechain: true` transcript rows) are deferred to a future pass; not implemented in Phase 4, and no dashboard "Subagent spend" line exists until that pass lands. |
| `ContextForensicsStore` | The `rusqlite`-backed repository — open/create schema, upsert, and query methods. Mirrors `claude_code_session::omission_cache::OmissionCache`. | New. |
| `HookInstallState` | Sum type: `NotInstalled \| Installed`. Derived by reading `~/.claude/settings.json` on demand — **not** persisted as its own DB row (see Pattern Decisions). | New, Phase 4. |
| `SettingsJsonGateway` | Component wrapping read/backup/atomic-write access to `~/.claude/settings.json`. | New, Phase 4. |
| `HookMarker` | The stable, recognizable string consolette stamps into its own installed hook command entries (e.g. a `consolette context-hook <event>` command literal) so `down` can find and remove exactly its own entries. | New, Phase 4. |
| `CrossCheckStatus` | Sum type: `TranscriptOnly \| Corroborated \| Diverged`. | New, Phase 5. |
| `PeakContext` | A session's maximum `TurnRow.cumulative_tokens` — computed at query time via `MAX(cumulative_tokens)`, not stored redundantly. | New. |
| `RescanTask` | The periodic background re-ingestion task, mirroring `claude_code_session::session_bi::spawn_session_bi_refresh_task`. | New. |
| `ContextForensicsMcpServer` | The hand-written `ServerHandler` impl exposing this feature's query tools over MCP. | New, Phase 5. Mirrors `claude_code_session::mcp_server::CompactionMcpServer`. |

---

## Pattern Decisions

| Component | Pattern Chosen | Source | Alternative Rejected | Reason |
|-----------|---------------|--------|---------------------|--------|
| `ContextForensicsStore` | Repository | PoEAA (Fowler) | `sqlx`/`sea-orm` | `rusqlite` already proven twice in-repo (`omission_cache.rs`, `cmdcrush/metrics_store.rs`); Build-vs-Buy research explicitly rules out a new DB dependency. |
| `ingest_claude_code_session` / `ingest_codex_session` | Transaction Script | PoEAA (Fowler) | Domain Model (rich `Turn`/`Session` objects with behavior) | ETL-shaped problem (parse → transform → upsert); no recurring business rule complex enough to earn an object graph. |
| Source dispatch (`ClaudeCode` vs `Codex` ingestion) | Sum type + exhaustive match | type-driven-design | GoF Strategy (`dyn IngestSource` trait object) | Closed, unlikely-to-grow 2-variant set; a match arm is simpler and compiler-checked exhaustive, no vtable indirection needed. |
| `HookInstallState` | Sum type + pure functions (`up`, `down`) | type-driven-design | GoF State (polymorphic state objects) | 2-state machine; a Rust enum match is idiomatic and needs no OO ceremony. |
| `SettingsJsonGateway` | Gateway | PoEAA (Fowler) | Repository | Wraps one external resource with a foreign format (a hand-edited JSON file), not a collection of domain objects — Gateway is PoEAA's own distinction for "ad hoc access to an external/legacy resource." |
| `BudgetThreshold` | Newtype | type-driven-design | raw `u64` | Prevents confusing a raw token count with a threshold; gives `crossed()` a clear receiver instead of comparison logic scattered across dashboard/query code. |
| `Source`, `CompositionCategory`, `UsageProvenance`, `CrossCheckStatus`, `HookEventKind` | Sum types / sealed enums | type-driven-design | String constants (as context-analyzer's own Python/SQLite does) | Compiler-enforced exhaustive `match` in Rust; SQLite `CHECK` constraint kept too, as defense-in-depth at the storage boundary, but the Rust-side type is the real sum type. |
| Composition classifier (`classify_call_composition`) | Pure function, no GoF pattern | — | GoF Visitor over `TranscriptRow` variants | Only 3 categories with a fixed per-content-block classification rule; Visitor's double-dispatch buys nothing over one match/if-chain function. |
| `AnthropicUsage` extraction (proxy path and transcript path) | Shared value type, two extraction sites | type-driven-design | Two independent structs (`ProxyUsage`, `TranscriptUsage`) | Identical JSON shape (Anthropic Messages API `usage` object); one type read from two sources avoids two structurally-identical definitions drifting apart. |
| Dashboard router wiring | `Router::merge` onto the existing `serve-cost` router (existing precedent) | — | Separate `axum::serve` process/port for context-forensics | Success Metric requires "one consolette-served dashboard... without leaving consolette"; a second port reproduces the "second standalone tool" problem `research/ux.md` flags. |
| MCP tool exposure | Hand-written `ServerHandler`, match-dispatched by tool name | — (mirrors `CompactionMcpServer`) | `#[tool]`-macro-generated dispatch | Consistency with the one existing MCP server in this codebase; tool count (4–5) stays small enough for manual dispatch to remain more readable than adopting the macro for a one-off. |
| Dashboard charting | Hand-rolled inline SVG, vanilla client-side JS, no library | — | Vendored Chart.js | Matches `cost_metrics/dashboard.html`'s zero-dependency convention (ADR-015); the needed chart set (line+threshold bands, bar, donut, scatter) is within reach of hand-rolled SVG per `research/stack.md` §2 and `research/build-vs-buy.md` §2. |
| Rescan consistency model | Full reparse + idempotent upsert keyed by `row_uuid`, periodic background task | — (mirrors `session_bi.rs`) | Byte-offset/checkpoint incremental tailing | Avoids the checkpoint-invalidated-by-compaction-rewrite hazard (`research/pitfalls.md` §1); matches context-analyzer's own "batch ingestion on-demand" design; no freshness SLA to justify the added complexity. |
| Subagent token attribution | Separate `subagents` table, **not** summed into parent totals by default (ADR-004) | — | Fold subagent usage directly into the parent turn's `tool_io_tokens` | Avoids silently double-counting or silently hiding subagent-heavy sessions' real cost; matches context-analyzer's own separate-table design. **v1 scope** (ADR-004): Phase 4 populates only invocation count/duration from hook-event timestamps; token totals and the dashboard's "Subagent spend" line are deferred to a future pass that parses sidechain transcript content. |

---

## Migration Plan

**Store file**: `~/.claude/consolette/context-forensics.sqlite` (same parent directory convention as `omission_cache.rs`'s `~/.claude/consolette/omission-cache.sqlite`), `0700`/`0600` permission hardening applied identically — this store holds raw conversation/tool-I/O content, the same sensitivity class `omission_cache.rs`'s own doc comment already documents the hardening for.

**Migration mechanism**: no migration framework — schema is created via repeated `CREATE TABLE IF NOT EXISTS` in `ContextForensicsStore::open()`, added to incrementally as each phase's epic lands (mirrors `omission_cache.rs` and `cmdcrush/metrics_store.rs`, neither of which uses a migration tool either). Phase 1 creates `sessions`, `turns`, `api_calls`, `native_compaction_events`. Phase 4 adds `hook_events`, `subagents`. Phase 5 adds `proxy_cross_check`.

```sql
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
    id                           TEXT PRIMARY KEY,   -- "{session_id}:{row_uuid}"
    session_id                   TEXT NOT NULL REFERENCES sessions(id),
    turn_id                      TEXT REFERENCES turns(id),
    row_uuid                     TEXT NOT NULL,
    call_index                   INTEGER NOT NULL,
    model                        TEXT,
    input_tokens                 INTEGER NOT NULL,
    output_tokens                INTEGER NOT NULL,
    cache_creation_input_tokens  INTEGER,          -- nullable from Phase 1: Codex (Phase 3) has no
    cache_read_input_tokens      INTEGER,          -- cache-creation equivalent and writes NULL, not 0
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
    UNIQUE (session_id, row_uuid)
);

-- Phase 4:
CREATE TABLE IF NOT EXISTS hook_events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT,
    event_kind  TEXT NOT NULL,
    payload     TEXT NOT NULL,
    received_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS subagents (
    id                           TEXT PRIMARY KEY,
    parent_session_id            TEXT NOT NULL REFERENCES sessions(id),
    parent_turn_id                TEXT REFERENCES turns(id),
    subagent_session_id          TEXT,
    input_tokens                 INTEGER NOT NULL,
    output_tokens                INTEGER NOT NULL,
    cache_creation_input_tokens  INTEGER NOT NULL,
    cache_read_input_tokens      INTEGER NOT NULL,
    started_at                   TEXT,
    ended_at                     TEXT
);

-- Phase 5:
CREATE TABLE IF NOT EXISTS proxy_cross_check (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id       TEXT NOT NULL REFERENCES sessions(id),
    call_id          TEXT NOT NULL REFERENCES api_calls(id),
    proxy_request_id TEXT,
    status           TEXT NOT NULL CHECK (status IN ('transcript_only','corroborated','diverged')),
    variance_tokens  INTEGER
);
```

**Reversibility**: irreversible in the traditional up/down-script sense, but harmlessly so — every table here is a **derived cache**, fully rebuildable by deleting `context-forensics.sqlite` and letting the next rescan re-ingest every transcript from scratch (`hook_events`/`subagents` rows recorded only via live hooks are the one exception: those are lost on delete, same as context-analyzer's own SQLite store would be). Rollback from a bad schema change during development is: delete the file, restart.

**Zero-downtime strategy**: not applicable — single-user local process, no concurrent readers/writers beyond one consolette process (WAL mode + `busy_timeout` per the `omission_cache.rs` precedent handles the one real concurrency case: a hook-event write racing a dashboard read).

**Rollback procedure**: delete `~/.claude/consolette/context-forensics.sqlite`; next `serve-cost` startup or manual "refresh now" recreates the schema and re-ingests.

---

## Observability Plan

- **Logs**: `tracing::warn!` on every skipped/malformed transcript line (already the house style, `transcript.rs:176-186`) and every failed session-file parse, both carrying `session_id`/`path`; `tracing::info!` at rescan start/end with counts ingested/skipped; `tracing::error!` on any `SettingsJsonGateway` write failure and on `ContextForensicsStore::open()` failure at `serve_cost` startup (Story 1.4.4) — both are failure modes with real blast radius (a corrupted settings file, or `serve_cost`'s own existing `cost_metrics` dashboard going down with it).
- **Metrics**: none new required — this is a personal tool with no SLA per requirements.md's Non-functional Requirements; the dashboard's own `parse_failure_count`/ingestion-warning banners (Phase 5.3) are the user-facing signal, not a metrics pipeline.
- **Alerts**: no new alerts required (personal tool, no oncall).

## Risk Control

- **Feature flag**: not gated — personal, single-user tool (matches requirements.md's Risk Control section). Each phase ships as a self-contained epic; an unfinished later phase (e.g. Codex ingestion) simply doesn't appear in the UI until its route/table exists.
- **Rollback procedure**: standard revert via PR close + revert commit for code; `context-forensics.sqlite` deletion for store-schema issues (see Migration Plan); `consolette context-tracker down` for hook-install issues (Phase 4, its own explicit uninstall path).
- **Staged rollout**: full rollout on merge, phase by phase — Phase 1 (composition + growth chart, Claude Code transcripts only, no hooks) is usable and satisfies both Success Metrics on its own; every subsequent phase is additive.

## Unresolved Questions

- [ ] Codex rollout-log row shape is unverified against real data (no local `~/.codex/sessions/` fixture exists on this machine) — blocks Story 3.1.1 — owner: Tyler, needs to run Codex CLI locally (or supply a fixture file) before that story starts.
- [ ] Exact hook-timeout budget Claude Code enforces on `PostToolUse` (pitfalls.md: "on the order of tens of seconds," not independently verified against current Claude Code docs) — blocks Story 4.2.1's latency-budget acceptance criterion — owner: verify against current Claude Code hook documentation immediately before implementing that story, not at plan time.
- [ ] Whether `stapler-scripts/llm-sync` touches `~/.claude/settings.json` on any schedule (only confirmed to sync the MCP server list per the repo's own CLAUDE.md; not independently verified here) — blocks Story 4.1.1's "identify all concurrent writers" acceptance criterion — owner: read `stapler-scripts/llm-sync`'s source directly before implementing `SettingsJsonGateway`.

## Dependency Visualization

```
Phase 1: Foundations + MVP slice (composition + growth, Claude Code, no hooks)
  Epic 1.1 (usage/pricing fixes) -> Epic 1.2 (store) -> Epic 1.3 (CC ingestion) -> Epic 1.4 (dashboard MVP)
   |
   |-- satisfies both Success Metrics on its own; everything below is additive --
   |
   +--> Phase 2: Cross-session analytics + message inspector + cache churn  (Claude Code only)
   |
   +--> Phase 3: Codex CLI ingestion                                        (independent of Phase 2)
   |       Epic 3.1 (rollout-log parsing spike) -> Epic 3.2 (wired into store + dashboard)
   |
   +--> Phase 4: Hook install/uninstall + hook-event ingestion              (independent of Phase 2/3)
   |       Epic 4.1 (SettingsJsonGateway) -> Epic 4.2 (hook-event ingestion)
   |
   +--> Phase 5: MCP exposure + proxy cross-check + polish                  (depends on 1-4's store/routes existing)
           Epic 5.1 (MCP tools) | Epic 5.2 (cross-check) | Epic 5.3 (error/empty states, a11y)
```

---

## Phase 1: Foundations + MVP slice (composition + growth, Claude Code transcripts only, no hooks)

### Epic 1.1: Exact API usage extraction and cache-aware pricing (prerequisite)
**Goal**: Close the "exact API token usage" gap flagged by three separate research agents — today nothing in this codebase reads `usage.{cache_creation_input_tokens,cache_read_input_tokens}` from either a live proxy response or a transcript row, and `PricingTable` has no cache-tier rate fields at all. Every downstream composition/cost number depends on this being fixed first.

#### Story 1.1.1: `providers::extract_usage` returns full cache-aware usage
**As a** consolette maintainer, **I want** the proxy's own usage extraction to carry `cache_creation_input_tokens`/`cache_read_input_tokens`, **so that** the existing proxy-captured cost path (and this feature's cross-check in Phase 5) isn't silently missing 60%+ of a cache-heavy call's real cost breakdown.
**Acceptance Criteria**:
- `extract_usage` (renamed target: a new `AnthropicUsage` struct, replacing the old `Option<(u64,u64)>` tuple return) reads all four usage fields, defaulting any missing one to `0`.
  - *Given* an Anthropic response JSON `{"usage": {"input_tokens": 100, "output_tokens": 20, "cache_creation_input_tokens": 500, "cache_read_input_tokens": 8000}}`, *When* `extract_usage` is called on it, *Then* it returns `Some(AnthropicUsage { input_tokens: 100, output_tokens: 20, cache_creation_input_tokens: 500, cache_read_input_tokens: 8000 })`.
- Existing callers of the old tuple-returning `extract_usage` (`record_actual_usage_from_anthropic_response` and its call sites) compile against the new struct without behavior change to the two fields they already consumed.
  - *Given* the same response as above, *When* `record_actual_usage_from_anthropic_response` runs, *Then* `input_tokens`/`output_tokens` continue to flow into `CostTracker` exactly as before, with no regression in the existing `cost_metrics` test suite.
**Files**: `src/providers/mod.rs`, `src/cost_metrics/mod.rs` (where `record_actual_usage_from_anthropic_response` lives), `src/cost_metrics/types.rs`, `src/entrypoint/cost_tee.rs` (a third `extract_usage` call site, added 2026-08-21 — verified via `grep -rln "extract_usage(" src/`)

##### Task 1.1.1a: Add `AnthropicUsage` struct and rewrite `extract_usage` (~4 min)
- In `src/providers/mod.rs`, define `pub(crate) struct AnthropicUsage { pub input_tokens: u64, pub output_tokens: u64, pub cache_creation_input_tokens: u64, pub cache_read_input_tokens: u64 }` above `extract_usage` (currently `mod.rs:457`).
- Rewrite `extract_usage` to parse all four fields from `usage`, each defaulting to `0` via `.and_then(Value::as_u64).unwrap_or(0)` (same pattern already used for the two existing fields), returning `Option<AnthropicUsage>`.
- Files: `src/providers/mod.rs`

##### Task 1.1.1b: Fix call sites consuming the old tuple shape (~3 min)
- `grep -rn "extract_usage(" src/` to find every call site; update each to destructure the new `AnthropicUsage` struct's `input_tokens`/`output_tokens` fields (unchanged names) instead of tuple `.0`/`.1`.
- Files: whichever file(s) the grep surfaces (expected: `src/providers/mod.rs`'s own `record_actual_usage_from_anthropic_response`, possibly `src/cost_metrics/hook.rs`)

##### Task 1.1.1c: Extend `extract_usage`'s unit tests for cache fields (~3 min)
- Add a test asserting all four fields populate from a full usage object, and a test asserting missing cache fields default to `0` (extending the existing `extract_usage_should_default_missing_individual_field_to_zero`-style tests at `providers/mod.rs:588-595`).
- Files: `src/providers/mod.rs`

#### Story 1.1.2: `PricingTable`/`ModelPrice` gain cache-tier rates
**As a** context-analyzer dashboard, **I want** per-model cache-read and cache-creation USD rates, **so that** any dollar figure computed for a cache-heavy call isn't wrong by construction (cache read ≈10% of input rate; cache write ≈125%).
**Acceptance Criteria**:
- `ModelPrice` carries `cache_read_usd_per_token` and `cache_creation_usd_per_token`, sourced from `pricing_default.json`'s existing (already-vendored, currently-unread) `cache_read_input_token_cost`/`cache_creation_input_token_cost` fields.
  - *Given* `pricing_default.json` contains a model entry with `"cache_read_input_token_cost": 0.00000003`, *When* `PricingTable::from_default_snapshot()` parses it, *Then* `price_for("<model>").cache_read_usd_per_token == 0.00000003`.
- A model entry missing the cache-cost fields in the snapshot parses successfully with `cache_read_usd_per_token`/`cache_creation_usd_per_token` defaulting to `0.0`, not a parse error.
  - *Given* a model JSON entry with only `input_cost_per_token`/`output_cost_per_token` set, *When* it's deserialized, *Then* `ModelPrice::cache_read_usd_per_token == 0.0` and parsing succeeds.
**Files**: `src/cost_metrics/pricing.rs`, `src/cost_metrics/pricing_default.json` (verify fields present, re-sync if a checked model is missing them)

##### Task 1.1.2a: Add cache-rate fields to `ModelPrice` (~3 min)
- Add `#[serde(rename = "cache_read_input_token_cost", default)] pub cache_read_usd_per_token: f64` and the equivalent `cache_creation_...` field to `ModelPrice` (`pricing.rs:46-51`).
- Files: `src/cost_metrics/pricing.rs`

##### Task 1.1.2b: Verify/re-sync `pricing_default.json` carries the new fields for in-use models (~4 min)
- Check 2-3 models `src/providers/*.rs` actually routes to for presence of `cache_read_input_token_cost`/`cache_creation_input_token_cost`; if absent, re-fetch and re-filter from the upstream LiteLLM URL already documented at `pricing.rs:31-32`, following the existing re-sync convention noted in that file's doc comment.
- Files: `src/cost_metrics/pricing_default.json`

##### Task 1.1.2c: Unit test cache-tier cost calc (~3 min)
- Add a test parsing a fixture snapshot with all four rate fields set and asserting `price_for` returns them correctly; add a test for the missing-field-defaults-to-zero case from the AC above.
- Files: `src/cost_metrics/pricing.rs`

#### Story 1.1.3: Transcript-side `extract_call_usage`
**As a** Claude Code ingestion pipeline, **I want** to read a transcript assistant row's real `message.usage` object, **so that** composition/cost figures use exact API-reported counts, not `TiktokenEstimator` guesses.
**Acceptance Criteria**:
- `extract_call_usage(row: &TranscriptRow) -> Option<AnthropicUsage>` reads `row.fields().message.get("usage")` (the whole `message` is already an opaque `Value` per `RowFields::message`, `transcript.rs:42`) and parses it via the same field logic as `providers::AnthropicUsage`.
  - *Given* a `TranscriptRow::Assistant` row whose `message` field is `{"usage": {"input_tokens": 1200, "output_tokens": 340, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 15000}, ...}`, *When* `extract_call_usage(&row)` is called, *Then* it returns `Some(AnthropicUsage { input_tokens: 1200, output_tokens: 340, cache_creation_input_tokens: 0, cache_read_input_tokens: 15000 })`.
- A row with no `message` field, or a `message` with no `usage` sub-object, returns `None` rather than panicking or defaulting to zeroed usage that would look like a real (if empty) call.
  - *Given* a `TranscriptRow::User` row whose `message` is `None`, *When* `extract_call_usage(&row)` is called, *Then* it returns `None`.
**Files**: `src/context_forensics/usage.rs` (new), `src/context_forensics/mod.rs` (new)

##### Task 1.1.3a: Scaffold `context_forensics` module and `usage.rs` (~3 min)
- Create `src/context_forensics/mod.rs` with a module doc comment scoping it per `research/architecture.md` §1 ("depends on, does not extend, `claude_code_session`/`cost_metrics`"); wire it into `src/lib.rs`'s module list.
- Create `src/context_forensics/usage.rs` with `extract_call_usage`, reusing `providers::AnthropicUsage` (make it `pub(crate)` visible to this module, or `pub` if module boundaries require).
- Files: `src/context_forensics/mod.rs`, `src/context_forensics/usage.rs`, `src/lib.rs`

##### Task 1.1.3b: Unit tests for `extract_call_usage` against real-shaped fixtures (~4 min)
- Add tests for: full usage object populates all four fields; missing `usage` key returns `None`; `message: None` returns `None`; a `user` row (no assistant usage ever expected) returns `None`.
- Files: `src/context_forensics/usage.rs`

---

### Epic 1.2: Persistent store foundation
**Goal**: Stand up `ContextForensicsStore`, the `rusqlite`-backed repository every later epic writes into, following the `omission_cache.rs` hardening pattern exactly (per ADR-001).

#### Story 1.2.1: `ContextForensicsStore::open` with Phase-1 schema
**As a** context-analyzer ingestion pipeline, **I want** a durable, permission-hardened SQLite store, **so that** session/turn/call data survives process restarts (the requirement `SessionCostStore` can't meet).
**Acceptance Criteria**:
- `ContextForensicsStore::open(path: &Path) -> Result<Self>` creates the parent directory (`0700`), the database file, sets `journal_mode=WAL`, and creates the `sessions`/`turns`/`api_calls`/`native_compaction_events` tables from the Migration Plan schema — all mirroring `OmissionCache::open`'s structure (`omission_cache.rs:55-90`). `api_calls.cache_creation_input_tokens`/`cache_read_input_tokens` are nullable `INTEGER` from this first schema, not `NOT NULL` — Claude Code ingestion (Phase 1) always writes an explicit value (including `0`); Codex ingestion (Phase 3) will write `NULL` for the field it has no equivalent for, and since this project's only migration mechanism is `CREATE TABLE IF NOT EXISTS` (a no-op against an already-existing table), the constraint can never be loosened later — it must be correct in this first schema.
  - *Given* a fresh path `~/.claude/consolette/context-forensics.sqlite` that does not yet exist, *When* `ContextForensicsStore::open(path)` is called, *Then* the file exists afterward with mode `0600`, its parent directory has mode `0700`, `sqlite3 <path> ".tables"` lists `sessions, turns, api_calls, native_compaction_events`, and `sqlite3 <path> ".schema api_calls"` shows `cache_creation_input_tokens`/`cache_read_input_tokens` without a `NOT NULL` constraint.
- Calling `open` a second time against an already-initialized file is a no-op on schema (idempotent `CREATE TABLE IF NOT EXISTS`) and does not error.
  - *Given* a store already opened and closed once, *When* `ContextForensicsStore::open(path)` is called again, *Then* it returns `Ok` and no data is lost.
**Files**: `src/context_forensics/store.rs` (new)

##### Task 1.2.1a: Implement `ContextForensicsStore::open` (~5 min)
- Port `OmissionCache::open`'s directory-creation/permission-hardening/WAL-pragma structure (`omission_cache.rs:55-90`) into a new `ContextForensicsStore` struct wrapping `Mutex<Connection>`.
- Execute the four Phase-1 `CREATE TABLE IF NOT EXISTS` statements from the Migration Plan.
- Files: `src/context_forensics/store.rs`

##### Task 1.2.1b: Tests for open/idempotency/permissions (~4 min)
- Test: fresh open creates all four tables (query `sqlite_master`). Test: re-open is idempotent. Test (unix-only, `#[cfg(unix)]`, mirroring `omission_cache.rs`'s own test pattern): file mode is `0600`, parent dir mode is `0700`.
- Files: `src/context_forensics/store.rs`

#### Story 1.2.2: Upsert API for sessions/turns/api_calls
**As a** Claude Code ingestion pipeline, **I want** idempotent upsert methods keyed by natural ids, **so that** re-ingesting a growing transcript file on every rescan cycle never double-inserts or double-counts (per the "full reparse + idempotent upsert" Pattern Decision).
**Acceptance Criteria**:
- `upsert_session(&self, row: &SessionRow) -> Result<()>` and `upsert_turn`/`upsert_api_call` use `INSERT ... ON CONFLICT (...) DO UPDATE SET ...` keyed on each table's natural unique key (`sessions.id`; `(session_id, turn_index)`; `(session_id, row_uuid)`).
  - *Given* an `ApiCallRow` for `row_uuid = "abc-123"` already stored with `input_tokens = 100`, *When* `upsert_api_call` is called again with the same `row_uuid` but `input_tokens = 100` and an unchanged `output_tokens`, *Then* the table still has exactly one row for that `row_uuid` (no duplicate), reflecting the latest values.
- `session_row_count(&self) -> Result<u64>` (or equivalent query helper) is available for tests to assert against without hand-writing raw SQL in every test.
  - *Given* three distinct sessions upserted, *When* `session_row_count()` is called, *Then* it returns `3`.
**Files**: `src/context_forensics/store.rs`

##### Task 1.2.2a: Implement `upsert_session`/`upsert_turn`/`upsert_api_call` (~5 min)
- One method per table, each an `INSERT ... ON CONFLICT DO UPDATE`, wrapped in the existing `Mutex<Connection>` lock.
- Files: `src/context_forensics/store.rs`

##### Task 1.2.2b: Test idempotent re-upsert and row-count helpers (~4 min)
- Test double-upsert of the same `ApiCallRow` produces one row. Test upserting a changed `SessionRow` (e.g. bumped `last_ingested_at`) updates in place.
- Files: `src/context_forensics/store.rs`

---

### Epic 1.3: Claude Code ingestion (composition + growth slice)
**Goal**: Turn parsed transcripts into `SessionRow`/`TurnRow`/`ApiCallRow` data — the minimum needed for both Success Metrics.

#### Story 1.3.1: Composition classifier
**As a** context-analyzer ingestion pipeline, **I want** to classify each API call's tokens into Tool I/O / Conversation / System, **so that** the dashboard's composition breakdown (Success Metric #1) has real numbers, not a guess.
**Acceptance Criteria**:
- `classify_call_composition(turn: &Turn, call_usage: &AnthropicUsage) -> CompositionBreakdown` splits `input_tokens` (the non-cached portion, per `research/pitfalls.md` §4's "input_tokens is specifically non-cached") across the three categories using `Turn.tool_rows`/`Turn.user_row`/`Turn.assistant_rows`' content-block types as the classification signal, and treats `cache_read_input_tokens`/`cache_creation_input_tokens` as their own tracked figures rather than folded silently into one category (per the pitfalls research's cost-overstatement warning).
  - *Given* a `Turn` whose `tool_rows` contain one `tool_result` block and whose `user_row`'s `message.content` is plain text, *When* `classify_call_composition` runs on that turn's call, *Then* `tool_io_tokens` reflects the tool_result block's share of `input_tokens` and `conversation_tokens` reflects the user text's share, summing to `input_tokens` (not `input_tokens + cache_read_input_tokens`).
- A leading `system` row (e.g. `CLAUDE.md` load) that would previously be silently dropped by `build_turns`' "rows preceding the first genuine user turn" gap (`transcript.rs:369-380`) is explicitly surfaced as a known accepted gap for this feature too, logged via `tracing::debug!` when non-zero usage is detected on a dropped leading row — not silently absorbed into the totals.
  - *Given* a transcript whose first row is a `system` row carrying `usage.cache_creation_input_tokens = 3000` and preceding any user row, *When* `ingest_claude_code_session` runs, *Then* a `tracing::debug!` line names the dropped row's uuid and token count, and the session's totals do not silently include it.
**Files**: `src/context_forensics/composition.rs` (new)

##### Task 1.3.1a: Implement `classify_call_composition` (~5 min)
- Walk `turn.tool_rows`/`turn.assistant_rows`/`turn.user_row`'s content blocks (via the already-captured `message: Option<Value>`) to build the three-way split; treat `TranscriptRow::System` rows' content as `system_tokens`.
- Files: `src/context_forensics/composition.rs`

##### Task 1.3.1b: Tests against fixture turns for each category boundary (~5 min)
- Test tool-result-heavy turn, conversation-heavy turn, and a turn including a `system` row, asserting the three counts sum correctly to `input_tokens`.
- Files: `src/context_forensics/composition.rs`

#### Story 1.3.2: `ingest_claude_code_session` pipeline
**As a** context-analyzer background task, **I want** one function that turns a transcript path into stored rows, **so that** ingestion is a single, testable Transaction Script.
**Acceptance Criteria**:
- `ingest_claude_code_session(store: &ContextForensicsStore, path: &Path) -> Result<IngestSummary>` calls `parse_session_file` → `build_turns` → per-turn `extract_call_usage` + `classify_call_composition` + `extract_native_compaction_events`, upserting `SessionRow`/`TurnRow`/`ApiCallRow`/native-compaction rows, and returns a summary (`turns_ingested`, `calls_ingested`, `parse_failures`).
  - *Given* a fixture `.jsonl` with 3 turns, each with one assistant row carrying a `usage` object, *When* `ingest_claude_code_session` runs, *Then* the store contains 1 `SessionRow`, 3 `TurnRow`s, and 3 `ApiCallRow`s, and the returned `IngestSummary.calls_ingested == 3`.
- `build_turns`' cycle-detection `Err` is caught per-session-file (not propagated to abort a whole-corpus rescan), matching `research/pitfalls.md` §2's "one bad session must not blank the entire cross-session view."
  - *Given* a fixture transcript engineered to produce a `parentUuid` cycle, *When* `ingest_claude_code_session` runs on it as part of a multi-session rescan, *Then* it returns `Err` (or an `IngestSummary` flagging the failure) for that one session without panicking, and the caller (Story 1.3.3) continues to the next file.
- `TurnRow.cumulative_tokens` accounts for `ChainCoverage.ratio() < 1.0` by recording the ratio on `SessionRow` (already in the Phase-1 schema) rather than silently omitting it, so a multi-root session's undercounted total is visible to the dashboard later (Phase 5.3), not hidden.
  - *Given* a session whose active chain covers 60% of its rows (`chain_coverage.ratio() == 0.6`), *When* ingestion runs, *Then* `SessionRow.chain_coverage_ratio == 0.6` is stored.
**Files**: `src/context_forensics/ingest_claude_code.rs` (new)

##### Task 1.3.2a: Implement `ingest_claude_code_session` happy path (~5 min)
- Wire `discovery`/`parse_session_file`/`build_turns` (imported from `claude_code_session`) through `extract_call_usage`/`classify_call_composition`, computing `cumulative_tokens` as a running sum across turns, and upsert into the store.
- Files: `src/context_forensics/ingest_claude_code.rs`

##### Task 1.3.2b: Catch `build_turns` cycle errors per-file (~3 min)
- Wrap the `build_turns` call so its `Err` becomes an `IngestSummary`-level failure logged via `tracing::warn!`, not a propagated panic/abort.
- Files: `src/context_forensics/ingest_claude_code.rs`

##### Task 1.3.2c: Thread `chain_coverage` and native-compaction events into storage (~4 min)
- Compute `ChainCoverage` alongside `build_turns`' output, store its ratio on `SessionRow`; call `extract_native_compaction_events` and upsert each into `native_compaction_events`.
- Files: `src/context_forensics/ingest_claude_code.rs`

##### Task 1.3.2d: Integration test against a fixture `.jsonl` (~5 min)
- Write a `NamedTempFile`-based fixture (mirroring `transcript.rs:473`'s pattern) with 3 turns and known usage figures; assert `IngestSummary` and store contents match.
- Files: `src/context_forensics/ingest_claude_code.rs`

#### Story 1.3.3: Background rescan task
**As a** consolette user, **I want** transcripts re-ingested periodically without a manual step, **so that** the dashboard reflects recent sessions without requiring a live-tailing daemon.
**Acceptance Criteria**:
- `spawn_context_forensics_refresh_task(store: Arc<ContextForensicsStore>) -> JoinHandle<()>` performs an eager initial full scan (via `discover_sessions_glob`) before returning, then re-scans every 15 minutes (`Duration::from_secs(15 * 60)`, matching `session_bi.rs:29`'s `SESSION_BI_REFRESH_INTERVAL`), calling `ingest_claude_code_session` per discovered file.
  - *Given* two `.jsonl` files under `~/.claude/projects/`, *When* the task is spawned, *Then* before the spawning call returns, both sessions are already queryable in the store (eager-initial-scan-before-background-loop, matching `server.rs:115-121`'s documented rationale).
- One session file failing to parse does not stop the rest of the corpus from being ingested in the same rescan pass.
  - *Given* three session files where the second is malformed, *When* a rescan runs, *Then* sessions 1 and 3 are ingested and a `tracing::warn!` names the second file's failure.
**Files**: `src/context_forensics/refresh.rs` (new)

##### Task 1.3.3a: Implement eager-scan-then-interval-loop task (~5 min)
- Port the structure of `session_bi.rs`'s `spawn_session_bi_refresh_task` (eager scan before spawn returns, then `tokio::time::interval` loop), swapping in `ingest_claude_code_session` per file.
- Files: `src/context_forensics/refresh.rs`

##### Task 1.3.3b: Test eager-scan and per-file failure isolation (~4 min)
- Test the eager scan completes before the function returns (assert store is populated immediately). Test a malformed file among valid ones doesn't abort the batch.
- Files: `src/context_forensics/refresh.rs`

---

### Epic 1.4: Dashboard MVP (composition breakdown + budget-threshold growth chart)
**Goal**: Ship the smallest slice that satisfies both Success Metrics verbatim, reachable from consolette's one dashboard entry point.

#### Story 1.4.1: JSON query routes
**As a** dashboard frontend, **I want** JSON endpoints for per-session composition and growth data, **so that** the client-side chart code has something to fetch.
**Acceptance Criteria**:
- `GET /v1/context/sessions` returns `[{id, source, project, started_at, peak_context_tokens, chain_coverage_ratio}, ...]` for every stored session.
  - *Given* two ingested sessions, one with `PeakContext = 850_000`, *When* `GET /v1/context/sessions` is called, *Then* the response JSON array has 2 entries and one has `"peak_context_tokens": 850000`.
- `GET /v1/context/sessions/{id}/composition` returns per-turn `CompositionBreakdown` plus `AnthropicUsage` totals; `GET /v1/context/sessions/{id}/growth` returns `[{turn_index, cumulative_tokens}, ...]` plus the session's `native_compaction_events` for the autocompact line.
  - *Given* a session with 3 turns, cumulative tokens `[10000, 45000, 210000]`, *When* `GET /v1/context/sessions/{id}/growth` is called, *Then* the response's third element has `"cumulative_tokens": 210000`.
- An unknown `{id}` returns `404` with a JSON error body, not a `500`.
  - *Given* a session id not present in the store, *When* `GET /v1/context/sessions/does-not-exist/composition` is called, *Then* the response is `404`.
**Files**: `src/context_forensics/server.rs` (new)

##### Task 1.4.1a: Implement `GET /v1/context/sessions` and `.../composition` (~5 min)
- Add axum handlers backed by two new `ContextForensicsStore` query methods (`list_sessions`, `composition_for_session`).
- Files: `src/context_forensics/server.rs`, `src/context_forensics/store.rs`

##### Task 1.4.1b: Implement `GET /v1/context/sessions/{id}/growth` with 404 handling (~4 min)
- Add the handler + `growth_for_session` store query; return `StatusCode::NOT_FOUND` when the session id doesn't exist.
- Files: `src/context_forensics/server.rs`, `src/context_forensics/store.rs`

##### Task 1.4.1c: Route-level tests (~4 min)
- `axum::body` request tests for all three routes, including the 404 case, mirroring existing test patterns in `cost_metrics/server.rs`'s test module.
- Files: `src/context_forensics/server.rs`

#### Story 1.4.2: Composition breakdown UI
**As a** Tyler-the-dashboard-user, **I want** a donut plus an exact numeric table for Tool I/O / Conversation / System, **so that** I can answer "top token-cost contributor per turn" at a glance and precisely (Success Metric #1).
**Acceptance Criteria**:
- The composition view renders a per-turn stacked/donut breakdown alongside a numeric table (never chart-only, per `research/ux.md` §1's APM-tool precedent), and each category is distinguished by pattern/label, not color alone (`research/ux.md` §3).
  - *Given* a turn with `tool_io_tokens = 12000, conversation_tokens = 3000, system_tokens = 500`, *When* the composition view renders that turn, *Then* the numeric table shows all three exact figures and the donut/bar segment for Tool I/O is visually the largest, each segment carrying a distinct hatch/label independent of color.
- Selecting a turn via the scrubber (native `<input type="range">`, keyboard-operable per `research/ux.md` §3) updates the composition panel to that turn without a full page reload.
  - *Given* the scrubber is at turn 1, *When* the user presses the right-arrow key, *Then* the composition panel updates to show turn 2's breakdown.
**Files**: `src/context_forensics/dashboard.html` (new)

##### Task 1.4.2a: Scaffold `dashboard.html` shell with theme-aware CSS (~5 min)
- Single self-contained file, `include_str!`'d — port `cost_metrics/dashboard.html`'s `prefers-color-scheme` CSS-variable pattern (`dashboard.html:8-36`) and `aria-live` banner state machine (`dashboard.html:129,187-220`) as the starting shell.
- Files: `src/context_forensics/dashboard.html`

##### Task 1.4.2b: Composition donut/bar + numeric table, fetch from `/v1/context/sessions/{id}/composition` (~5 min)
- Vanilla JS `fetch` + inline-SVG rendering for the donut/stacked-bar; render the numeric table alongside.
- Files: `src/context_forensics/dashboard.html`

##### Task 1.4.2c: Turn scrubber wired to a native `<input type="range">` (~4 min)
- Range input's `oninput` re-renders the composition panel for the selected turn index; verify arrow-key operability.
- Files: `src/context_forensics/dashboard.html`

#### Story 1.4.3: Context-growth-per-turn chart with budget thresholds
**As a** Tyler-the-dashboard-user, **I want** a growth chart with 200K/500K/700K/1M threshold toggle buttons, a dashed budget line, and an autocompact marker, **so that** I can identify the turn/session where context crossed a chosen budget (Success Metric #2).
**Acceptance Criteria**:
- The chart renders `cumulative_tokens` per turn as a line, with toggle buttons (not a dropdown) for each `BudgetThreshold` preset that draw a dashed red line at that value plus a red-shaded danger band above it, per `research/ux.md` §1's reference-tool pattern.
  - *Given* a session whose cumulative tokens cross 500,000 at turn 40, *When* the user clicks the "500K" threshold toggle, *Then* a dashed line renders at the 500K y-position and turn 40's point on the line falls inside the shaded danger band above it.
- `NativeCompactionEvent`s render as a distinct dotted "autocompact" marker on the same chart, reusing consolette's own "native" compaction vocabulary (not a new term), per `research/ux.md` §2.
  - *Given* a `NativeCompactionEvent` recorded at turn 55, *When* the growth chart renders, *Then* a dotted marker labeled with the existing "native" compaction term appears at turn 55.
- A session with zero calls yet (freshly discovered, not yet ingested) shows an inline "No calls recorded for this session yet" empty state scoped to the chart region, not a blank canvas or a full-page error, per `research/ux.md` §4.
  - *Given* a session with a `SessionRow` but zero `ApiCallRow`s, *When* its growth chart is requested, *Then* the chart region shows the named empty-state message instead of an empty SVG.
**Files**: `src/context_forensics/dashboard.html`

##### Task 1.4.3a: Inline-SVG line chart with axis scaling (~5 min)
- Hand-rolled SVG `<path>` generation from `[{turn_index, cumulative_tokens}]`, with axis labels.
- Files: `src/context_forensics/dashboard.html`

##### Task 1.4.3b: Threshold toggle buttons + dashed line + danger band (~5 min)
- Four `<button>` elements (real buttons, `:focus-visible` styled per the existing convention at `dashboard.html:103-106`) toggling a dashed `<line>` + shaded `<rect>` overlay.
- Files: `src/context_forensics/dashboard.html`

##### Task 1.4.3c: Autocompact marker + empty-chart-region state (~4 min)
- Render dotted markers from the `native_compaction_events` the growth endpoint already returns (Story 1.4.1); render the scoped empty state when `calls_ingested == 0`.
- Files: `src/context_forensics/dashboard.html`

##### Task 1.4.3d: Text/table fallback for the growth chart (~3 min)
- A collapsed `<details>` below the SVG (`ux.md` Surface 1 wireframe) computed from the same `[{turn_index, cumulative_tokens}]` payload as Task 1.4.3a — peak value + turn, each toggled threshold's crossing turn, and the autocompact turn if present — so the chart is never the sole source of that data (`ux.md`'s "never chart-only" convention, UX acceptance criterion 27).
- Files: `src/context_forensics/dashboard.html`

#### Story 1.4.4: Route wiring into `serve-cost`
**As a** consolette user, **I want** the new dashboard reachable from the process I already run, **so that** I never leave consolette (Success Metric, verbatim).
**Acceptance Criteria**:
- `context_forensics::server::context_router(store)` is merged onto the existing `cost_router(...).merge(dashboard_router(...))` chain in `serve_cost` (`cost_metrics/server.rs:221-237`), mounted at `/dashboard/context` (distinct from the pre-existing, already-contested `/dashboard` route per `research/features.md`'s collision note) with its JSON API under `/v1/context/*`.
  - *Given* `consolette serve-cost` is running, *When* a `GET /dashboard/context` request is made, *Then* it returns the new dashboard's HTML, and `GET /dashboard` continues to return the existing session-BI dashboard unchanged.
- The existing `/dashboard` page gains a link/nav element to `/dashboard/context` (and vice versa), satisfying `research/ux.md`'s "one entry point" finding rather than shipping a third disconnected page.
  - *Given* `/dashboard` is loaded, *When* the user looks for the new view, *Then* a visible link to `/dashboard/context` is present in the page chrome.
- `ContextForensicsStore::open()` failing (corrupted DB file, disk full, permissions) fails open, not closed: `serve_cost` logs the error and starts normally without the context-forensics routes mounted, rather than propagating the error via `?` and aborting the whole process — `cost_metrics`'s existing, working dashboard must never go down because of this new feature. Today, nothing analogous to this store is on `serve_cost`'s startup path at all (`OmissionCache::open` is only called from the separate `prune`/`mcp` subcommands, `src/main.rs:164,182` — never from `serve_cost`), so this is new failure-handling, not an existing pattern to copy.
  - *Given* `ContextForensicsStore::open()` returns `Err` (e.g. the store file is corrupted), *When* `serve_cost` starts, *Then* it logs a `tracing::error!` naming the failure, `GET /dashboard` still returns `200` with the existing session-BI dashboard working normally, and any `GET /dashboard/context` or `/v1/context/*` request returns `503 Service Unavailable` (or is absent from the router) instead of the process failing to start or panicking.
**Files**: `src/cost_metrics/server.rs`, `src/context_forensics/dashboard.html`, `src/cost_metrics/dashboard.html`

##### Task 1.4.4a: Merge `context_router` into `serve_cost` with fail-open error handling (~4 min)
- Call `ContextForensicsStore::open(...)` at `serve_cost` startup and match on the result rather than propagating with `?`: on `Ok(store)`, `.merge(context_forensics::server::context_router(store))` onto the existing chain and spawn `spawn_context_forensics_refresh_task` alongside the existing `session_bi` refresh spawn; on `Err(e)`, `tracing::error!(error = %e, "context forensics store failed to open; context-forensics routes disabled")` and continue building the router *without* the `context_router` merge (or mount a small fallback sub-router that returns `503` for `/dashboard/context` and `/v1/context/*`), so `cost_router`/`dashboard_router` still start.
- Files: `src/cost_metrics/server.rs`

##### Task 1.4.4b: Cross-link the two dashboards (~3 min)
- Add a nav link in each dashboard's HTML shell pointing at the other.
- Files: `src/context_forensics/dashboard.html`, `src/cost_metrics/dashboard.html`

##### Task 1.4.4c: End-to-end route test (~3 min)
- Assert `GET /dashboard/context` returns `200` and the existing `GET /dashboard` route is unaffected (regression guard against the collision risk `research/features.md` flagged).
- Files: `src/cost_metrics/server.rs`

##### Task 1.4.4d: Test that `serve_cost` starts and `/dashboard` still works when the context store fails to open (~4 min)
- Point `ContextForensicsStore::open` at a path that can't be opened (e.g. a file where a directory is expected, mirroring how `omission_cache.rs`'s own tests simulate open failure) and assert `GET /dashboard` still returns `200` and `GET /dashboard/context` returns `503` (or `404`) rather than the test harness failing to start the server at all.
- Files: `src/cost_metrics/server.rs`

---

## Phase 2: Cross-session analytics, message inspector, cache-read churn (Claude Code only)

### Epic 2.1: Cross-session analytics
**Goal**: A sortable session table plus a cost/call-vs-peak-context scatter, matching context-analyzer's `/sessions` view.

#### Story 2.1.1: Cross-session query + route
**As a** dashboard frontend, **I want** one query returning cost/call, peak context, and trend data across every session, **so that** the scatter/table has data to render.
**Acceptance Criteria**:
- `GET /v1/context/sessions/summary` returns, per session, `{id, source, started_at, cost_per_call_usd, peak_context_tokens, call_count, chain_coverage_ratio}`, computed with the Story 1.1.2 cache-aware `PricingTable` rates (not the plain input/output rate alone).
  - *Given* a session with 10 calls totaling `$0.42` computed via cache-aware rates, *When* `GET /v1/context/sessions/summary` is called, *Then* that session's entry has `"cost_per_call_usd": 0.042`.
- The response array is ordered by `started_at` ascending, so the same payload doubles as the `cost_per_call_usd` trend series requirements.md's In-Scope "trends" bullet calls for — no second endpoint needed (Story 2.1.2 renders it as a line chart).
  - *Given* three sessions with `started_at` values `2026-08-01`, `2026-08-03`, `2026-08-02` and `cost_per_call_usd` `0.02`, `0.05`, `0.03` respectively, *When* `GET /v1/context/sessions/summary` is called, *Then* the response array is ordered `[2026-08-01 (0.02), 2026-08-02 (0.03), 2026-08-03 (0.05)]`, chronological by `started_at` rather than insertion order.
**Files**: `src/context_forensics/server.rs`, `src/context_forensics/store.rs`

##### Task 2.1.1a: Implement `summary_for_all_sessions` store query (~5 min)
- `GROUP BY session_id` aggregation over `api_calls`, joined against `PricingTable` cost calculation; `ORDER BY sessions.started_at ASC` so callers get a chronological trend series for free.
- Files: `src/context_forensics/store.rs`

##### Task 2.1.1b: Route + test (~3 min)
- Handler + test asserting the shape/values above, including the `started_at`-ordering case.
- Files: `src/context_forensics/server.rs`

#### Story 2.1.2: Scatter + sortable table, click-through
**As a** Tyler-the-dashboard-user, **I want** to click a session in the cross-session view and land on its single-session dashboard, **so that** I can go from "which session is expensive" to "why" in one click (`research/ux.md` §1's Honeycomb/Datadog precedent).
**Acceptance Criteria**:
- The `/dashboard/context/sessions` view (client-side route within the same single-page dashboard, not a new HTML file, per the "one entry point" UX finding) renders a sortable `<table>` (reusing `dashboard.html`'s existing sort-button convention) plus a cost-vs-peak-context scatter plot.
  - *Given* 5 sessions with varying `cost_per_call_usd`, *When* the user clicks the "cost/call" column header, *Then* the table re-sorts ascending/descending on that column.
- Clicking a scatter point or table row navigates to that session's single-session view (`/dashboard/context?session={id}`).
  - *Given* the scatter renders a point for session `abc`, *When* the user clicks it, *Then* the page shows session `abc`'s composition/growth view.
- A "Cost/Call Over Time" line chart renders below the scatter, plotting Story 2.1.1's `started_at`-ordered `cost_per_call_usd` series with no extra fetch (same `/v1/context/sessions/summary` payload); fewer than 2 sessions shows "Not enough sessions yet for a trend" instead of a single-point/empty line.
  - *Given* the summary payload has 6 sessions ordered by `started_at`, *When* the cross-session view renders, *Then* the trend chart's line has 6 points in that chronological order, x-position increasing monotonically with `started_at`.
**Files**: `src/context_forensics/dashboard.html`

##### Task 2.1.2a: Sortable cross-session table (~5 min)
- Files: `src/context_forensics/dashboard.html`

##### Task 2.1.2b: Scatter plot + click-through navigation (~5 min)
- Files: `src/context_forensics/dashboard.html`

##### Task 2.1.2c: Cost/Call Over Time line chart (~4 min)
- Hand-rolled inline-SVG line chart (matching the growth chart's existing pattern from Task 1.4.3a) plotting the same `/v1/context/sessions/summary` payload already fetched for the scatter/table, ordered by `started_at`; render the "Not enough sessions yet for a trend" empty state when fewer than 2 sessions are present.
- Files: `src/context_forensics/dashboard.html`

##### Task 2.1.2d: Text summary alongside the trend chart (~3 min)
- Compute a first-vs-last (or min/max) `cost_per_call_usd` summary string (e.g. "up 3.1x over 12 sessions") from the same `/v1/context/sessions/summary` payload as Task 2.1.2c, rendered next to the chart so the trend is never chart-only (`ux.md` UX acceptance criterion 25).
- Files: `src/context_forensics/dashboard.html`

### Epic 2.2: Message inspector
**Goal**: Full turn-content viewing, reusing the request-body-inspection-modal pattern already proven in `dashboard.rs`.

#### Story 2.2.1: Turn-content route
**As a** dashboard frontend, **I want** a route returning one turn's full row content, **so that** the inspector modal has something to render.
**Acceptance Criteria**:
- `GET /v1/context/sessions/{id}/turns/{turn_index}` returns the turn's `user_row`/`assistant_rows`/`tool_rows` content (from the already-captured `message: Option<Value>`, no re-parsing needed).
  - *Given* turn 5 of session `abc` has a `user_row` with `message.content = "explain X"`, *When* `GET /v1/context/sessions/abc/turns/5` is called, *Then* the response includes that text verbatim.
**Files**: `src/context_forensics/server.rs`, `src/context_forensics/store.rs`

##### Task 2.2.1a: Store the raw row content needed for inspection (~4 min)
- **Schema correction** (the AC needs `user_row`/`tool_rows` content, which nothing before this task stores — `TurnRow` only ever held a `user_row_uuid` reference): add nullable `TEXT` columns `message_json` on `api_calls` (the assistant row's serialized `message`, one per call — covers `assistant_rows`) AND `user_row_json` + `tool_rows_json` on `turns` (the turn's user row's serialized `message`, and a JSON array of its tool rows' `message` values in order). Denormalized onto `TurnRow` rather than a new per-row table: tool rows carry no token usage and the inspector only ever renders a whole turn at once, never queries individual tool rows, so a join-free blob is proportionate.
- Files: `src/context_forensics/store.rs`

##### Task 2.2.1b: Route + test (~4 min)
- Files: `src/context_forensics/server.rs`

#### Story 2.2.2: Inspector UI with collapsible content blocks
**As a** Tyler-the-dashboard-user, **I want** to expand a turn's full content inline, **so that** I don't have to `cat` a JSONL file to see what a turn actually contained.
**Acceptance Criteria**:
- Clicking a turn on the growth chart or scrubber opens a collapsible panel showing `user_row`/`assistant_rows`/`tool_rows` content, reusing `dashboard.rs`'s modal pattern (`dashboard.rs:277-289`) adapted to this dashboard's theme-aware CSS.
  - *Given* turn 5's tool row contains a large JSON blob, *When* the user expands the tool-row section, *Then* the full content renders, collapsible back to a summary line.
**Files**: `src/context_forensics/dashboard.html`

##### Task 2.2.2a: Collapsible inspector panel (~5 min)
- Files: `src/context_forensics/dashboard.html`

### Epic 2.3: Cache-read churn chart
**Goal**: Visualize `cache_read`/`cache_creation` behavior across a session, labeled with the literal API field names per `research/ux.md` §2.

#### Story 2.3.1: Churn aggregation query
**As a** dashboard frontend, **I want** per-turn cache-read/cache-creation totals, **so that** the churn chart has data.
**Acceptance Criteria**:
- `GET /v1/context/sessions/{id}/cache-churn` returns `[{turn_index, cache_read_input_tokens, cache_creation_input_tokens}, ...]`.
  - *Given* turn 10 has `cache_read_input_tokens = 18000, cache_creation_input_tokens = 200`, *When* the route is called, *Then* turn 10's entry reflects those exact figures.
**Files**: `src/context_forensics/server.rs`, `src/context_forensics/store.rs`

##### Task 2.3.1a: Query + route + test (~4 min)
- Files: `src/context_forensics/store.rs`, `src/context_forensics/server.rs`

#### Story 2.3.2: Churn chart UI
**As a** Tyler-the-dashboard-user, **I want** a bar chart labeled `cache_read`/`cache_creation` (not an abstracted "churn score"), **so that** the numbers cross-check directly against raw API responses I already read.
**Acceptance Criteria**:
- The chart's legend and axis labels use the literal field names.
  - *Given* the churn chart renders for a session, *When* the legend is inspected, *Then* it reads "cache_read_input_tokens" / "cache_creation_input_tokens", not "churn score" or similar.
**Files**: `src/context_forensics/dashboard.html`

##### Task 2.3.2a: Bar chart implementation (~4 min)
- Files: `src/context_forensics/dashboard.html`

---

## Phase 3: Codex CLI ingestion

### Epic 3.1: Codex rollout-log parsing spike
**Goal**: De-risk the one part of this feature with zero local verification (`research/architecture.md` §2: "no local `~/.codex/sessions/` fixture exists... needs a real fixture before implementation").

#### Story 3.1.1: Fixture acquisition and `RolloutLine`/`RolloutItem` types
**As a** context-analyzer maintainer, **I want** at least one real Codex rollout-log fixture and a hand-rolled parser for it, **so that** Codex ingestion isn't guessed from documentation alone.
**Acceptance Criteria**:
- A real (or Tyler-supplied) `rollout-<TIMESTAMP>-<UUID>.jsonl` fixture exists under a test-fixtures path, and `RolloutLine { timestamp, ordinal: Option<_>, item: RolloutItem }` with `RolloutItem` as a hand-rolled tagged enum (mirroring `TranscriptRow`'s unknown-field-preserving pattern, not a strict derive) parses it without silently dropping fields.
  - *Given* the fixture file, *When* it's parsed line-by-line, *Then* every line either parses into a `RolloutLine` or is logged-and-skipped, and zero lines panic the parser.
- `TokenUsage`'s known `cache_write_tokens` serde-drop bug (`openai/codex` issue #32479, `research/stack.md` §3) is explicitly *not* assumed present — the parser tolerates its absence rather than treating a missing field as `0` with false confidence.
  - *Given* a `TokenCount` event with no `cache_write_tokens` field at all, *When* parsed, *Then* the corresponding Rust field is `Option<u64>` (`None`), never silently coerced to `Some(0)`.
**Files**: `src/context_forensics/codex_rollout.rs` (new), `tests/fixtures/codex/` (new, or wherever this repo's other transcript fixtures live — confirm during Task a)

##### Task 3.1.1a: Locate this repo's fixture convention and add a Codex fixture (~4 min)
- `grep -rn "write_temp_jsonl\|NamedTempFile" src/claude_code_session/transcript.rs` to confirm the existing in-file fixture convention; add the Codex fixture the same way (inline in test module) unless a shared `tests/fixtures/` dir already exists.
- Files: `src/context_forensics/codex_rollout.rs`

##### Task 3.1.1b: Implement `RolloutLine`/`RolloutItem` hand-rolled parsing (~5 min)
- Mirror `TranscriptRow`'s manual `Deserialize` dispatch-by-tag approach (`transcript.rs:104-119`) for `RolloutItem`'s 8 variants.
- Files: `src/context_forensics/codex_rollout.rs`

##### Task 3.1.1c: Parse-and-skip test against the fixture (~4 min)
- Files: `src/context_forensics/codex_rollout.rs`

#### Story 3.1.2: `extract_codex_usage` and turn/call boundary reconstruction
**As a** Codex ingestion pipeline, **I want** Codex's `TokenCount` events mapped to the same `AnthropicUsage`-shaped composition/cost pipeline, **so that** cross-session analytics can span both CLIs.
**Acceptance Criteria**:
- `extract_codex_usage(item: &RolloutItem) -> Option<CodexUsage>` (a distinct type from `AnthropicUsage` — Codex's `TokenUsage` has `reasoning_output_tokens` with no Anthropic equivalent, and folds cache accounting differently per `research/pitfalls.md` §2) extracts `input_tokens`/`cached_input_tokens`/`output_tokens`/`reasoning_output_tokens`.
  - *Given* an `EventMsg::TokenCount` item with `input_tokens: 500, cached_input_tokens: 100, output_tokens: 50, reasoning_output_tokens: 20`, *When* `extract_codex_usage` runs, *Then* all four values populate.
**Files**: `src/context_forensics/codex_rollout.rs`

##### Task 3.1.2a: Implement `CodexUsage` type and extraction (~4 min)
- Files: `src/context_forensics/codex_rollout.rs`

##### Task 3.1.2b: Test extraction against the fixture (~4 min)
- Files: `src/context_forensics/codex_rollout.rs`

### Epic 3.2: Codex ingestion wired into store + dashboard

#### Story 3.2.1: `ingest_codex_session` + discovery
**As a** context-analyzer background task, **I want** Codex sessions discovered and ingested the same way Claude Code sessions are, **so that** they appear in the same store with a `source = 'codex'` discriminant.
**Acceptance Criteria**:
- `discover_codex_sessions() -> Result<Vec<PathBuf>>` walks `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`.
  - *Given* two rollout files under different date directories, *When* `discover_codex_sessions()` is called, *Then* both paths are returned.
- `ingest_codex_session` upserts into the same `sessions`/`turns`/`api_calls` tables with `source = 'codex'`, mapping `CodexUsage` into `ApiCallRow`'s `AnthropicUsage`-shaped columns and writing `NULL` (not `0`) into `cache_creation_input_tokens` where Codex has no equivalent — an explicit doesn't-distinguish representation, not a silent zero, so the dashboard can gray it out rather than imply "no caching happened". These columns are already nullable as of the Phase 1 schema (Story 1.2.1), so this is a plain `INSERT`, not a schema change.
  - *Given* a Codex session with 2 turns, *When* ingested, *Then* the store has a `SessionRow` with `source = 'codex'` and its `ApiCallRow`s' `cache_creation_input_tokens` column is `NULL`, distinguishable from a Claude Code call's real `0`.
**Files**: `src/context_forensics/ingest_codex.rs` (new), `src/context_forensics/store.rs`

##### Task 3.2.1a: `discover_codex_sessions` (~4 min)
- Files: `src/context_forensics/ingest_codex.rs`

##### Task 3.2.1b: Implement `ingest_codex_session` writing `NULL` cache-creation figures (~5 min)
- `cache_creation_input_tokens`/`cache_read_input_tokens` are already nullable as of the Phase 1 `api_calls` schema (Story 1.2.1) — no schema/constraint change needed here. Map `CodexUsage` into an `ApiCallRow`, passing `None` for `cache_creation_input_tokens` (Codex has no equivalent field) and `Some(CodexUsage.cached_input_tokens)` for `cache_read_input_tokens`, then upsert.
- Files: `src/context_forensics/ingest_codex.rs`

##### Task 3.2.1c: Integration test against the Story 3.1.1 fixture (~4 min)
- Files: `src/context_forensics/ingest_codex.rs`

#### Story 3.2.2: Source badges + graying compaction-specific columns
**As a** Tyler-the-dashboard-user, **I want** Codex sessions visually distinguished, **so that** a Codex row's "not tracked" compaction column never reads as "no compaction happened."
**Acceptance Criteria**:
- The cross-session table shows a "CC"/"Codex" badge per row (reusing `dashboard.rs`'s existing badge-class convention, `dashboard.rs:454,475-477`), and compaction-specific columns are grayed/marked "n/a" for Codex rows rather than showing `0`.
  - *Given* a Codex-source session row, *When* the cross-session table renders, *Then* its compaction column shows "n/a" (styled distinctly), not "0".
**Files**: `src/context_forensics/dashboard.html`

##### Task 3.2.2a: Badge + n/a-column rendering (~4 min)
- Files: `src/context_forensics/dashboard.html`

---

## Phase 4: Hook install/uninstall + hook-event ingestion

### Epic 4.1: `SettingsJsonGateway`
**Goal**: Idempotent, backup-first, reversible `~/.claude/settings.json` hook installation — the one part of this feature with blast radius beyond itself.

#### Story 4.1.1: Read/backup/atomic-write primitives
**As a** hook installer, **I want** to read `settings.json` as an untyped `serde_json::Value`, back it up, and write changes atomically, **so that** unrelated sections (`permissions`, `env`, other plugins' hooks) are never lost and a crash mid-write never corrupts the live file.
**Acceptance Criteria**:
- `SettingsJsonGateway::read(path) -> Result<Value>` preserves every top-level key byte-for-byte (parsed as `Value`, never a strict typed struct).
  - *Given* a `settings.json` with `{"permissions": {...}, "hooks": {...}, "unrelated_future_key": 42}`, *When* read and re-serialized without modification, *Then* `unrelated_future_key` is still present with value `42`.
- `SettingsJsonGateway::write(path, value)` writes to `settings.json.tmp` in the same directory, `fsync`s, then `rename()`s over the original — never truncates-and-rewrites the live file in place.
  - *Given* a write in progress, *When* the process is killed between the tmp-file write and the rename, *Then* the original `settings.json` is untouched (verified by asserting the temp-write-then-rename sequence via a test that inspects file operations, not a real kill-signal test).
- `SettingsJsonGateway::backup(path)` writes `settings.json.bak.<unix-timestamp>` before any mutating call, only if a backup for this install run doesn't already exist.
  - *Given* `up` is called twice in the same run, *When* the second call's backup step runs, *Then* only one new backup file is created, not two.

*Precondition, not an AC of this story's code*: llm-sync's actual write behavior toward `settings.json` must be confirmed (not assumed) before Task 4.1.1a's finding is relied on elsewhere in this epic — tracked in Unresolved Questions, not restated here as a testable criterion.
**Files**: `src/context_forensics/hooks_install.rs` (new)

##### Task 4.1.1a: Confirm llm-sync's settings.json contact surface (~3 min)
- Read `stapler-scripts/llm-sync`'s source (path relative to the dotfiles repo, not this one — cross-repo read) for any `settings.json` write, not just its documented MCP-server-list sync.
- Files: none in this repo — a research read; record the finding as a comment in `hooks_install.rs`'s module doc.

##### Task 4.1.1b: Implement `read`/`write` (atomic tmp+rename) (~5 min)
- Files: `src/context_forensics/hooks_install.rs`

##### Task 4.1.1c: Implement `backup` with once-per-run guard (~4 min)
- Files: `src/context_forensics/hooks_install.rs`

##### Task 4.1.1d: Tests for byte-faithful round-trip, atomic write sequencing, backup idempotency (~5 min)
- Files: `src/context_forensics/hooks_install.rs`

#### Story 4.1.2: `up` — idempotent install
**As a** Tyler, **I want** `consolette context-tracker up` to add consolette's hooks without touching my existing ones, **so that** installing this feature never breaks my current Claude Code setup.
**Acceptance Criteria**:
- `up` appends consolette's `HookMarker`-carrying entries to each relevant event's array (`PostToolUse`, `SessionStart`, etc.) rather than overwriting the array, per `research/pitfalls.md` §3's "multiple hooks on the same event, one array."
  - *Given* `settings.json` already has an existing `PostToolUse` hook (e.g. an RTK hook), *When* `up` runs, *Then* the existing entry is still present and consolette's new entry is appended alongside it.
- Running `up` twice does not produce two copies of consolette's own entry (idempotency check: "is this exact `HookMarker`-carrying entry already present").
  - *Given* `up` has already run once, *When* `up` runs again, *Then* the `PostToolUse` array still contains exactly one consolette entry.
- `up` never reorders or inserts before existing entries — consolette's entry is always positioned *after* every pre-existing entry in that event's array, so an existing hook's execution order (and any order-dependent side effect, e.g. a command-rewrite hook that must run before a later hook sees its output) is unaffected (pre-mortem P1: array-append was tested for entry presence but never for execution-order/interaction with existing hooks).
  - *Given* `settings.json`'s `PostToolUse` array already has one real-shaped existing entry (a hook command that writes a distinct marker line to stdout and exits `0`) at index `0`, *When* `up` runs, *Then* that entry is still at index `0` afterward, consolette's entry is appended at the end of the array (never prepended, never inserted between existing entries), and invoking the pre-existing entry's command directly still produces the same stdout/exit code it did before `up` ran.
**Files**: `src/context_forensics/hooks_install.rs`, `src/main.rs`

##### Task 4.1.2a: Implement `up` (~5 min)
- Files: `src/context_forensics/hooks_install.rs`

##### Task 4.1.2b: Add `ContextTrackerUp` to `main.rs`'s `Command` enum + dispatch (~3 min)
- Files: `src/main.rs`

##### Task 4.1.2c: Tests: additive append, idempotent re-run (~4 min)
- Files: `src/context_forensics/hooks_install.rs`

##### Task 4.1.2d: Test order-preservation against a real-shaped pre-existing hook (~4 min)
- Seed a fixture `settings.json` with a `PostToolUse` entry shaped like a real hook with an observable side effect (a small command that writes distinct stdout and exits non-zero, mirroring the RTK/`fewer-permission-prompts`-style hooks pre-mortem.md #2 names); run `up`; assert (a) that entry's array index and command string are byte-identical before/after, (b) consolette's `HookMarker` entry is appended after it, never before or between, and (c) invoking the pre-existing entry's command directly still produces the same stdout/exit code as before `up` ran. Name the test `up_should_append_after_existing_entries_never_before_when_settings_json_has_pre_existing_hook`, following `validation.md`'s `<fn>_should_<expected>_when_<condition>` naming convention, and add it to `validation.md`'s Requirement → Test Mapping under REQ-1 alongside the existing `up_should_append_consolette_entry_when_existing_hook_present` row.
- Files: `src/context_forensics/hooks_install.rs`

#### Story 4.1.3: `down` — targeted uninstall
**As a** Tyler, **I want** `consolette context-tracker down` to remove exactly the entries `up` added, **so that** hooks I configured after `up` ran are never clobbered by a blind restore-from-backup.
**Acceptance Criteria**:
- `down` removes entries carrying consolette's `HookMarker` and leaves every other entry (including ones added after `up` ran) untouched — it does **not** restore from the Story 4.1.1 backup file.
  - *Given* `up` ran, then Tyler manually added a second, unrelated `PostToolUse` hook, *When* `down` runs, *Then* consolette's entry is gone and the manually-added hook is still present.
- Running `down` when nothing is installed is a no-op, not an error.
  - *Given* `settings.json` has no consolette-marked entries, *When* `down` runs, *Then* it returns `Ok` and the file is unchanged.
**Files**: `src/context_forensics/hooks_install.rs`, `src/main.rs`

##### Task 4.1.3a: Implement `down` (~4 min)
- Files: `src/context_forensics/hooks_install.rs`

##### Task 4.1.3b: Add `ContextTrackerDown` to `main.rs` (~3 min)
- Files: `src/main.rs`

##### Task 4.1.3c: Tests: targeted removal preserves post-install additions; no-op when absent (~4 min)
- Files: `src/context_forensics/hooks_install.rs`

### Epic 4.2: Hook-event ingestion

#### Story 4.2.1: `consolette context-hook <event>` subcommand
**As a** Claude Code hook, **I want** a fast, fire-and-forget subcommand that writes one `hook_events` row and exits, **so that** every tool call in every session doesn't get perceptibly slower.
**Acceptance Criteria**:
- `consolette context-hook <event-name>` reads the hook's JSON payload from stdin (per Claude Code's hook-JSON-over-stdin convention), inserts one `HookEventRow`, and exits `0` within [the verified timeout budget from Unresolved Questions] with no lock contention against a concurrent dashboard reader (WAL mode already provides this per the Story 1.2.1 pragma).
  - *Given* a `PostToolUse` payload piped to `consolette context-hook PostToolUse`, *When* the command runs, *Then* it inserts exactly one `hook_events` row with `event_kind = 'PostToolUse'` and exits `0`.
- A malformed or empty stdin payload logs a warning and exits `0` (never blocks the tool call on a hook-ingestion failure) — matches the observability requirement that ingestion failures degrade gracefully.
  - *Given* empty stdin, *When* the command runs, *Then* it exits `0` and no row is inserted, with a `tracing::warn!` logged.
**Files**: `src/context_forensics/hook_event.rs` (new), `src/main.rs`

##### Task 4.2.1a: Implement stdin-JSON → `hook_events` insert (~5 min)
- Files: `src/context_forensics/hook_event.rs`, `src/context_forensics/store.rs`

##### Task 4.2.1b: Add `ContextHook { event: String }` to `main.rs`'s `Command` enum (~3 min)
- Files: `src/main.rs`

##### Task 4.2.1c: Malformed-payload and timing tests (~4 min)
- Files: `src/context_forensics/hook_event.rs`

#### Story 4.2.2: Subagent tracking (`SubagentStart`/`SubagentStop` → `subagents` table)
**As a** context-analyzer store, **I want** subagent invocations recorded as their own ledger, **so that** subagent-heavy sessions' invocation count and duration are visible without corrupting parent-session totals (ADR-004).

**v1 scope note**: per ADR-004, this story implements *only* start/stop-time correlation — invocation count and duration. `SubagentStart`/`SubagentStop` hook events carry no token-usage data; populating `SubagentRow` with real `AnthropicUsage` totals requires parsing the subagent's own transcript content (`isSidechain: true` rows, currently excluded by `build_turns`, `transcript.rs:249-254`), which is out of scope for this story and deferred to a future pass. No dashboard "Subagent spend" ($/token) line is implemented until that pass lands.

**Acceptance Criteria**:
- A `SubagentStart`/`SubagentStop` hook-event pair for the same subagent session id is correlated into one `SubagentRow` with start/end timestamps. `SubagentRow`'s `AnthropicUsage` fields (`input_tokens`/`output_tokens`/`cache_creation_input_tokens`/`cache_read_input_tokens`, all `NOT NULL` in the `subagents` schema) are written as `0` placeholders in this story, not real figures — populating them from real subagent transcript content is explicitly out of scope here (see v1 scope note above and ADR-004). A future pass `UPDATE`s these columns in place once sidechain-transcript parsing lands; no schema/constraint change is needed for that follow-up since `subagents` isn't created until Phase 4 and every row already carries a valid (if placeholder) value.
  - *Given* a `SubagentStart` event at `T0` and a `SubagentStop` event at `T1` sharing a subagent session id, *When* both are ingested, *Then* one `SubagentRow` exists with `started_at = T0, ended_at = T1`.
- `PeakContext`/composition totals computed for the *parent* session do **not** include subagent totals (per ADR-004), now or once a future pass adds real subagent usage totals.
  - *Given* a parent session with `peak_context_tokens = 400_000` and a subagent invocation recorded via `SubagentStart`/`SubagentStop`, *When* the parent session's summary is queried, *Then* `peak_context_tokens` is still `400_000`, unaffected by the subagent's existence.
**Files**: `src/context_forensics/ingest_claude_code.rs`, `src/context_forensics/store.rs`

##### Task 4.2.2a: Correlate `SubagentStart`/`Stop` hook events into `SubagentRow` (~5 min)
- Files: `src/context_forensics/store.rs`

##### Task 4.2.2b: Verify parent-session queries exclude `subagents` (~3 min)
- Add a regression test asserting `summary_for_all_sessions`/`growth_for_session` queries never join against `subagents`.
- Files: `src/context_forensics/store.rs`

---

## Phase 5: MCP exposure, transcript-vs-proxy cross-check, polish

### Epic 5.1: MCP tool exposure

#### Story 5.1.1: `ContextForensicsMcpServer`
**As a** Claude Code session using consolette's MCP server, **I want** to query composition/growth/session data as MCP tools, **so that** I can ask Claude itself "where did my tokens go" without opening the dashboard (resolved Open Question: MCP exposure is in scope).
**Acceptance Criteria**:
- The server exposes tools mirroring the JSON API: `get_session_composition`, `get_session_growth`, `list_sessions_summary`, `get_turn_content` — hand-written `ServerHandler` dispatch by tool name (Pattern Decisions), each delegating to the same `ContextForensicsStore` query methods the HTTP routes use (no duplicated query logic).
  - *Given* a Claude Code session calls the `get_session_composition` tool with a valid `session_id`, *When* dispatched, *Then* the result matches `GET /v1/context/sessions/{id}/composition`'s JSON byte-for-byte (same underlying store call).
- An unknown tool name returns a structured MCP error result, not a panic.
  - *Given* a call to a tool name not in the dispatch table, *When* `dispatch` runs, *Then* it returns an `is_error: true` result.
**Files**: `src/context_forensics/mcp_server.rs` (new), `src/main.rs`

##### Task 5.1.1a: Implement `ContextForensicsMcpServer` with 4-tool dispatch (~5 min)
- Mirror `CompactionMcpServer`'s structure (`mcp_server.rs:28-77`) — same hand-written `ServerHandler`, `dispatch` fn, and `#[cfg(test)]` test-only entry point pattern.
- Files: `src/context_forensics/mcp_server.rs`

##### Task 5.1.1b: Wire into `consolette mcp`'s existing server composition (~4 min)
- Files: `src/main.rs`

##### Task 5.1.1c: Tests for each tool + unknown-tool-name error path (~5 min)
- Files: `src/context_forensics/mcp_server.rs`

### Epic 5.2: Transcript-vs-proxy cross-check

#### Story 5.2.1: `cross_check.rs` reconciliation
**As a** context-analyzer store, **I want** to compare transcript-derived usage against proxy-captured usage for the same call when both exist, **so that** the "transcripts primary" requirement is enforced at write time, not query time (`research/pitfalls.md` §4).
**Acceptance Criteria**:
- A call observed by both the proxy (keyed by Anthropic's `request_id`/message `id` — the only safe join key, not timestamp) and the transcript writes exactly one `ApiCallRow` (`usage_provenance = 'transcript_exact'`) plus one `proxy_cross_check` row with `status = 'corroborated'` if the two sources agree within a small tolerance, or `'diverged'` if not — never two competing `ApiCallRow`s that could be double-summed.
  - *Given* the proxy captured `input_tokens = 1200` for message id `msg_abc` and the transcript's row for the same `msg_abc` also reports `1200`, *When* cross-check runs, *Then* the store has one `ApiCallRow` and one `proxy_cross_check` row with `status = 'corroborated'`.
- A session with no proxy-side data at all (never routed through consolette) degrades to `status = 'transcript_only'` — never an error, never a missing row.
  - *Given* a session run directly against Anthropic with no proxy record, *When* cross-check runs for its calls, *Then* every `proxy_cross_check` row for that session has `status = 'transcript_only'`.
**Files**: `src/context_forensics/cross_check.rs` (new), `src/context_forensics/store.rs`

##### Task 5.2.1a: Implement join-by-message-id reconciliation (~5 min)
- Files: `src/context_forensics/cross_check.rs`

##### Task 5.2.1b: Graceful degrade to `transcript_only` when no proxy record exists (~3 min)
- Files: `src/context_forensics/cross_check.rs`

##### Task 5.2.1c: Tests: corroborated, diverged, transcript-only cases (~5 min)
- Files: `src/context_forensics/cross_check.rs`

#### Story 5.2.2: Surface cross-check status on the single-session dashboard
**As a** Tyler-the-dashboard-user, **I want** to see whether a session's usage figures are corroborated against proxy data or transcript-only, **so that** requirements.md's cross-check is visible, not just computed internally (`ux.md` Surface 1, UX acceptance criterion 26).
**Acceptance Criteria**:
- `GET /v1/context/sessions/{id}/composition` (or a small addition to it) includes the session's aggregate `proxy_cross_check` status (`corroborated` / `diverged` / `transcript_only`) alongside the existing composition payload — no second fetch.
  - *Given* a session whose calls all have `status = 'corroborated'`, *When* `GET /v1/context/sessions/{id}/composition` is called, *Then* the response includes `"cross_check_status": "corroborated"`.
- The composition panel renders a `[Corroborated ⓘ]` / `[Diverged ⓘ]` / `[Transcript only ⓘ]` badge; clicking/focusing the ⓘ expands a one-line tooltip naming the specific discrepancy when `diverged`.
  - *Given* `cross_check_status = "diverged"`, *When* the composition panel renders, *Then* a `[Diverged ⓘ]` badge is visible and expands a tooltip on click/focus.
**Files**: `src/context_forensics/server.rs`, `src/context_forensics/store.rs`, `src/context_forensics/dashboard.html`

##### Task 5.2.2a: Add `cross_check_status` (session-level aggregate) to the composition query/route (~4 min)
- Files: `src/context_forensics/store.rs`, `src/context_forensics/server.rs`

##### Task 5.2.2b: Render the provenance badge + tooltip in the composition panel (~4 min)
- Files: `src/context_forensics/dashboard.html`

### Epic 5.3: Error/empty states and accessibility polish

#### Story 5.3.1: Partial-ingestion and empty-corpus banners
**As a** Tyler-the-dashboard-user, **I want** a visible warning whenever a session's data is incomplete, **so that** a partially-ingested session never looks identical to a fully-ingested one (`research/ux.md` §4's "the whole point of this tool is to trust the numbers").
**Acceptance Criteria**:
- A session with `parse_failure_count > 0` shows a per-session banner ("N of M transcript lines could not be parsed — data on this page may be incomplete"), reusing `dashboard.html`'s existing `parseFailureCount`/banner convention (`dashboard.html:358-366`) one level down from the whole-corpus case it currently covers.
  - *Given* a session with `parse_failure_count = 3`, *When* its single-session dashboard loads, *Then* the banner text names "3" failed lines.
**Files**: `src/context_forensics/dashboard.html`

##### Task 5.3.1a: Per-session ingestion-warning banner (~4 min)
- Files: `src/context_forensics/dashboard.html`

#### Story 5.3.2: Accessibility touches
**As a** Tyler-the-dashboard-user, **I want** the cheap accessibility wins already established elsewhere in consolette applied here too, **so that** the new dashboard doesn't regress below the existing bar.
**Acceptance Criteria**:
- Every chart region has an `aria-live="polite"` status element for loading/error/empty states, reusing the `setBannerState` pattern (`dashboard.html:187-220`); the composition legend never encodes category by color alone (already covered in Story 1.4.2, verified here as a regression check across all chart types added in Phases 2–3).
  - *Given* the growth chart is loading, *When* a screen reader is active, *Then* the `aria-live` region announces "Loading growth data" (or equivalent) without requiring focus to move.
**Files**: `src/context_forensics/dashboard.html`

##### Task 5.3.2a: Apply `aria-live` banner pattern to every chart region (~4 min)
- Files: `src/context_forensics/dashboard.html`

##### Task 5.3.2b: Audit all chart legends (composition, churn, cross-session badges) for color-independence (~3 min)
- Files: `src/context_forensics/dashboard.html`

##### Task 5.3.2c: Keyboard-activatable cross-session table rows (~3 min)
- Wrap each row's content in a real `<button>`/`<a>`, or — if a real interactive element can't wrap a `<tr>` — add `tabindex="0"` + `role="button"` plus Enter/Space handling, matching the `:focus-visible` treatment already applied to sort headers (Task 2.1.2a) and the threshold toggles (Task 1.4.3b), per `ux.md` UX acceptance criterion 18b.
- Files: `src/context_forensics/dashboard.html`
