# Implementation Plan: compaction-bi-dashboard

**Requirements**: `project_plans/compaction-bi-dashboard/requirements.md`
**Research**: `project_plans/compaction-bi-dashboard/research/{stack,features,architecture,pitfalls,ux,build-vs-buy}.md`
**Decisions**: `project_plans/compaction-bi-dashboard/decisions/ADR-014-session-bi-precompute-and-refresh-cache.md`, `ADR-015-vanilla-js-inlined-dashboard-html.md`

## Step 0.5: Creative Pass

Three candidate architectures for serving one comparison row per session across the whole `~/.claude/projects/**/*.jsonl` tree (7,840 files measured, per `research/pitfalls.md`):

| # | Approach | Strength | Weakness |
|---|----------|----------|----------|
| A (chosen) | Precompute a `SessionBiSnapshot` at server startup, hold it in a `tokio::sync::watch::channel<Arc<SessionBiSnapshot>>`, refresh on a fixed interval via a background task mirroring `spawn_pricing_refresh_task` (`src/cost_metrics/pricing.rs:221-237`). | Request handlers never block on I/O or parsing; matches an existing, tested pattern (`_pricing_refresh` in `CostServerState`) instead of inventing a new one. | Data can be up to one refresh interval stale (surfaced via a `generated_at` timestamp, not hidden). |
| B | Scan the whole tree synchronously inside the `GET /v1/dashboard/sessions` handler on every request. | Simplest code — no cache, no staleness, no background task lifecycle. | Blocks the request (and, without careful `spawn_blocking`, the whole axum runtime) for as long as a 7,840-file scan takes; `research/pitfalls.md` explicitly flags this corpus size as the reason per-request scanning is unacceptable. Rejected. |
| C | Materialize the snapshot into a small on-disk SQLite/JSON cache file, refreshed by a scheduled job outside the request path, survives process restart. | Survives restarts; opens a path to future historical trending. | Requires a new persistent datastore, which `requirements.md`'s Constraints explicitly rule out for this release; historical trending is out of scope. Rejected. |

Approach A is selected; B and C are recorded above and in the Pattern Decisions table (Step 3) as rejected alternatives for the caching component.

## Step 1: System Type

A **read-only reporting/BI aggregation layer** bolted onto an existing local HTTP server (`consolette serve-cost`). It has no write path, no user accounts, no persistence beyond an in-memory cache rebuilt on restart. It reads two kinds of existing on-disk artifacts (raw `*.jsonl` transcripts via `discover_sessions_glob`, and consolette's own compaction markers via `boundary.rs`) and one new kind (Claude Code's native `compact_boundary` markers), reduces each session's transcript to one summary row, and exposes the resulting collection as JSON + a server-rendered HTML table.

## Step 2: Domain Glossary

| Term | Definition |
|---|---|
| `NativeCompactionEvent` | A typed, best-effort deserialization of one Claude Code native `compact_boundary` row's `compactMetadata` object (`src/claude_code_session/native_compaction.rs`). Every field is `Option<T>` — Claude Code's own transcript schema is undocumented and versioned by the vendor, so no field's presence can be assumed. |
| `is_native_compacted` | `fn(&[TranscriptRow]) -> bool`. Returns `true` iff at least one row is a `TranscriptRow::System` row whose `extra["subtype"] == "compact_boundary"` — mirrors `boundary::is_compacted`'s "presence of a marker, not success of metric extraction" contract. |
| `extract_native_compaction_events` | `fn(&[TranscriptRow]) -> Vec<NativeCompactionEvent>`. Filters to `compact_boundary` rows, then best-effort-deserializes each row's `compactMetadata` value; a row with a missing or malformed `compactMetadata` is skipped (not an error), mirroring `boundary::extract_compaction_metrics`'s `serde_json::from_value(...).ok()` pattern. |
| `NativeCompactionEvent::tokens_saved` | `fn(&self) -> Option<i64>`. `pre_tokens as i64 - post_tokens as i64` when both are present, else `None` — never defaults a missing side to `0`. |
| `CompactionComparison::native_events` | New field on the existing `cost_compare::CompactionComparison` struct: `Vec<NativeCompactionEvent>`, populated unconditionally (independent of `is_compacted`) by `compare_compaction_cost`. |
| `CompactionComparison::is_native_compacted` | New `bool` field on `CompactionComparison`, computed via `native_compaction::is_native_compacted(&rows)`, symmetric with the existing `is_compacted` field. |
| `CompactionStatus` | New 4-variant enum in `session_bi.rs`: `NativeOnly`, `ConsoletteOnly`, `Both`, `Neither` — the session-level combination of `is_native_compacted` and `is_compacted`. A sum type, not two loose booleans, so the dashboard and any future consumer switch over one exhaustive value instead of re-deriving the same 2x2 logic. |
| `SessionComparisonRow` | New struct in `session_bi.rs`: one row of the dashboard/JSON response — file identity, `CompactionStatus`, native and consolette token/cost figures, the no-compaction counterfactual, and `chain_coverage_ratio`. |
| `SessionComparisonError` | New struct in `session_bi.rs` carrying `{ session_path: String, reason: String }` — one entry per file that failed to parse/compare, collected rather than aborting the whole scan (a single malformed transcript must not blank the entire dashboard). |
| `SessionBiSnapshot` | New struct in `session_bi.rs`: `{ rows: Vec<SessionComparisonRow>, parse_failures: Vec<SessionComparisonError>, generated_at: DateTime<Utc> }` — the one value held behind the new `watch::channel`. |
| `build_session_comparison_row` | `async fn(&SessionFile, &str) -> Result<SessionComparisonRow, SessionComparisonError>` — wraps one call to `cost_compare::compare_compaction_cost` plus native-event/status derivation for a single file. |
| `build_session_bi_snapshot` | `async fn(session_glob: &str, pricing_model: &str, concurrency: usize, per_file_timeout: Duration) -> SessionBiSnapshot` — takes an explicit glob pattern as a plain argument (never reads `HOME` itself) and calls `discovery::discover_sessions_glob(session_glob, SortBy::RecentFirst)`, then fans `build_session_comparison_row` out over the results using bounded concurrency (`futures_util::stream::buffer_unordered`) and a per-file timeout. Corrected during the Phase 3 repair loop: the earlier draft called the `HOME`-rooted `discovery::discover_sessions` internally, which broke test hermeticity (see Task 3.1.1b). |
| `spawn_session_bi_refresh_task` | `fn(watch::Sender<Arc<SessionBiSnapshot>>, session_glob: String, pricing_model: String, interval: Duration, concurrency: usize, per_file_timeout: Duration) -> JoinHandle<()>` — background task in `session_bi.rs` mirroring `spawn_pricing_refresh_task`'s shape (interval loop, `tx.send_replace`), but see ADR-014 for why it does *not* skip its first tick. Takes the same explicit `session_glob` as `build_session_bi_snapshot`, computed once by its caller (`CostServerState::build()`) and passed through unchanged on every refresh tick. |
| `dashboard_router` | New `fn(watch::Receiver<Arc<SessionBiSnapshot>>) -> Router` in `src/cost_metrics/server.rs`, separate from the existing `cost_router`, exposing `GET /v1/dashboard/sessions` (JSON) and `GET /dashboard` (HTML). |
| `CostServerState.session_bi_rx` / `_session_bi_refresh` | New fields on the existing `CostServerState` struct: the receiver handed to `dashboard_router`, and the background task's `JoinHandle` held for the process's lifetime (same pattern as `_pricing_refresh`). Populated by a new `CostServerState::build_with_session_glob(session_glob: &str) -> Self` associated function that does the real construction work; the existing `pub async fn build() -> Self` becomes a thin wrapper that computes `"{$HOME}/.claude/projects/**/*.jsonl"` from the real `HOME` env var and delegates to it. `build()`'s public signature is unchanged, so the one pre-existing test that calls it (`serve_cost_should_expose_apply_result_via_http_route_when_pipeline_and_route_share_same_tracker`, `src/cost_metrics/server.rs:203-206`) needs zero code changes. Tests that want a fixture corpus call `build_with_session_glob("<fixture-dir>/**/*.jsonl")` directly — no `std::env::set_var("HOME", ...)` anywhere, which is what removes the cross-test data race. |
| `confidence_legend` | A static object included once in the `/v1/dashboard/sessions` JSON response body (not per-row) mapping each native-derived field name to `"native_reported"` and each consolette-derived field name to `"estimated"` — satisfies the requirement to label which figures are exact vs. estimated without duplicating an unchanging mapping on every one of up to 7,840 rows. |
| `net_advantage_tokens` | `Option<i64>` field on `SessionComparisonRow`: `consolette_tokens_saved - native_tokens_saved`, present only when both sides are known — the single number `research/ux.md` calls out as the headline comparison figure. |
| `chain_coverage_ratio` | `f64` field on `SessionComparisonRow`, taken directly from the existing `ChainCoverage::ratio()` (`src/claude_code_session/transcript.rs:414`) — surfaced as its own sortable dashboard column per requirements.md, so a `--resume`/`--clear`-fragmented session's partial numbers are visible, not silently presented as complete. |
| `ChainCoverage` *(reused)* | Existing type (`transcript.rs:404`) — fraction of a transcript's messages reachable from its last row via parent-chain reconstruction. |
| `PricingTable` / `TiktokenEstimator` *(reused)* | Existing `cost_metrics` types (`pricing.rs:57`, `estimator.rs`) — the single pricing/estimation path for both the existing no-compaction estimate and this feature's consolette-side cost figures; never a second implementation. |
| `discover_sessions_glob` *(reused)* | Existing function (`discovery.rs:70`, doc comment: "Exposed separately so tests can point at fixture directories") — takes an arbitrary glob pattern and does no `HOME`/env reads itself. `build_session_bi_snapshot` calls this directly with a pattern passed in by its caller, not `discovery::discover_sessions` (`discovery.rs:55-61`, which reads `HOME` internally with no injectable parameter and is therefore unsuitable for a function that must also run hermetically under test). No new discovery code either way. |

## Step 3: Technology Validation & Pattern Decisions

**Technology validation**: No new Cargo dependency is required. `futures-util = "0.3"` (already a dependency) supplies `stream::iter(..).buffer_unordered(n)` for bounded concurrency; `tokio` (already `features = ["full"]`) supplies `watch::channel`, `time::interval`, and `time::timeout`; `chrono` (already present, `features = ["serde"]`) supplies `DateTime<Utc>` for `generated_at`. No license/security/stability concerns — everything reuses vetted, already-vendored crates.

| Component | Pattern chosen | Source | Alternative rejected | Reason |
|---|---|---|---|---|
| Native compaction parsing (`native_compaction.rs`) | Transaction Script — free functions over `&[TranscriptRow]`, no new domain object graph | PoEAA | A `NativeCompactionParser` trait/struct with internal state | `boundary.rs`'s existing `is_compacted`/`extract_compaction_metrics` free-function style is the direct precedent in this exact module family; a stateful parser object would be unjustified ceremony for a pure, stateless transform. |
| Cross-session aggregation (`session_bi.rs`) | Service Layer — one coordinating async function (`build_session_bi_snapshot`) composing `discovery`, `cost_compare`, and `native_compaction` | PoEAA | Repository pattern wrapping the file tree | There is no persistence layer to abstract over — `discovery::discover_sessions_glob` already *is* the read path; a Repository would just rename it. |
| Result caching for the HTTP layer | Precompute-at-startup + periodic background refresh via `watch::channel` (Observer/pub-sub) | GoF (Observer), existing `spawn_pricing_refresh_task` precedent | (B) per-request synchronous scan; (C) persisted on-disk cache | See ADR-014 and Step 0.5's table above. |
| Four-state compaction status | `CompactionStatus` sum type (Rust `enum`, 4 variants) | Type-driven design | Two independent `bool` fields (`is_native_compacted`, `is_compacted`) exposed raw to callers | An enum makes all four states explicit and exhaustively matchable (`match status { ... }` catches a missing arm at compile time); two booleans push the same 2x2 derivation onto every consumer, including the dashboard's JS, repeatedly. |
| Native/estimated field labeling | Static `confidence_legend` object, once per response | Mirrors `CostReport`'s `pricing_source`/`counterfactual_source` top-level-field convention | Per-row `TokenSource`-wrapped value struct (`{value, confidence}`) on every numeric field | The confidence of a given field is column-invariant (native fields are always native-reported-when-present; consolette fields are always estimator-derived) — wrapping every cell adds JSON size and client-side unwrapping for information that doesn't vary row to row. |
| Dashboard HTML delivery | `include_str!` compile-time constant, sibling `.html` file | Existing precedent, `pricing.rs:27`'s `include_str!("pricing_default.json")` | `tower-http::ServeDir` static file serving | Adds a new Cargo dependency for one file; `include_str!` needs none. See ADR-015. |
| Dashboard frontend | Vanilla JS/HTML/CSS, one file, client-side sort/filter over the fetched JSON array | Existing precedent, `src/dashboard.rs`'s `Html(const)` route shape (structure only — not its CDN dependency) | A bundled table library; a full frontend framework | No CDN calls are acceptable for this loopback/offline tool; a framework's build step is unjustified for one table. See ADR-015. |

## Step 4: Implementation Plan

### Phase 1 — Native Compaction Detection

#### Epic 1.1: `native_compaction.rs` — parse and detect Claude Code's own compaction markers

**Story 1.1.1: `NativeCompactionEvent` type and deserialization**

- Task 1.1.1a — Create `src/claude_code_session/native_compaction.rs` with the module doc comment (parallel to `boundary.rs`'s), imports (`serde::Deserialize`, `crate::claude_code_session::transcript::TranscriptRow`), and the `NativeCompactionEvent` struct:
  ```rust
  #[derive(Debug, Clone, PartialEq, Deserialize)]
  pub struct NativeCompactionEvent {
      pub trigger: Option<String>,
      #[serde(rename = "preTokens")]
      pub pre_tokens: Option<u64>,
      #[serde(rename = "postTokens")]
      pub post_tokens: Option<u64>,
      #[serde(rename = "durationMs")]
      pub duration_ms: Option<u64>,
      #[serde(rename = "cumulativeDroppedTokens")]
      pub cumulative_dropped_tokens: Option<u64>,
      #[serde(rename = "preservedSegment")]
      pub preserved_segment_tokens: Option<u64>,
      #[serde(rename = "preservedMessages")]
      pub preserved_messages: Option<u64>,
      #[serde(rename = "preCompactDiscoveredTools")]
      pub discovered_tools: Option<Vec<String>>,
  }
  ```
  Files: `src/claude_code_session/native_compaction.rs` (new).
- Task 1.1.1b — Add `impl NativeCompactionEvent { pub fn tokens_saved(&self) -> Option<i64> { ... } }` per the Domain Glossary definition above. Files: `src/claude_code_session/native_compaction.rs`.
- Task 1.1.1c — Add a private `fn is_compact_boundary_row(row: &TranscriptRow) -> bool` (`matches!(row, TranscriptRow::System(_)) && row.fields().extra.get("subtype").and_then(serde_json::Value::as_str) == Some("compact_boundary")`). Files: `src/claude_code_session/native_compaction.rs`.
- Task 1.1.1d — Add `pub fn is_native_compacted(rows: &[TranscriptRow]) -> bool` and `pub fn extract_native_compaction_events(rows: &[TranscriptRow]) -> Vec<NativeCompactionEvent>` per the Domain Glossary. Files: `src/claude_code_session/native_compaction.rs`.

  **Acceptance criterion**: "A session containing a native `compact_boundary` row with a full `compactMetadata` object is detected as natively compacted and its metadata is extracted."
  **Given-When-Then**: Given a transcript row `{"uuid":"b1","parentUuid":null,"type":"system","subtype":"compact_boundary","timestamp":"2024-01-01T00:00:00Z","message":null,"compactMetadata":{"trigger":"auto","preTokens":9000,"postTokens":1200,"durationMs":842,"cumulativeDroppedTokens":7800}}`, when `is_native_compacted(&rows)` and `extract_native_compaction_events(&rows)` are called on the parsed rows, then `is_native_compacted` returns `true` and `extract_native_compaction_events` returns `vec![NativeCompactionEvent { trigger: Some("auto".into()), pre_tokens: Some(9000), post_tokens: Some(1200), duration_ms: Some(842), cumulative_dropped_tokens: Some(7800), preserved_segment_tokens: None, preserved_messages: None, discovered_tools: None }]`, and `.tokens_saved()` on that event returns `Some(7800)`.

**Story 1.1.2: Best-effort tolerance for malformed/partial native metadata**

- Task 1.1.2a — Add a unit test asserting a `compact_boundary` row with `compactMetadata` missing entirely still makes `is_native_compacted` return `true` while `extract_native_compaction_events` returns an empty `Vec` (mirrors `boundary.rs`'s existing "compacted with no metrics" test at `cost_compare.rs:150-162`). Files: `src/claude_code_session/native_compaction.rs`.

  **Acceptance criterion**: "Parsing a session with unrecognized/partial `compactMetadata` fields never panics and skips only that row's metrics."
  **Given-When-Then**: Given a `compact_boundary` row whose `compactMetadata` is `{"trigger":"auto"}` (all numeric fields absent) alongside another `compact_boundary` row whose `compactMetadata` is the string `"not an object"` (malformed), when `extract_native_compaction_events(&rows)` runs, then it returns exactly one `NativeCompactionEvent { trigger: Some("auto".into()), pre_tokens: None, ..: None }` (the malformed row's `compactMetadata` fails `serde_json::from_value` and is silently skipped via `.ok()`, per the module's "skip-and-log, never panic" contract) and the call does not panic.
- Task 1.1.2b — Add a unit test asserting a transcript with zero `compact_boundary` rows returns `is_native_compacted() == false` and `extract_native_compaction_events() == vec![]`. Files: `src/claude_code_session/native_compaction.rs`.

  **Acceptance criterion**: "A session with no native compaction marker is correctly classified as not natively compacted."
  **Given-When-Then**: Given a transcript with only `user`/`assistant` rows and no `system`/`compact_boundary` row, when `is_native_compacted(&rows)` runs, then it returns `false`.

#### Epic 1.2: Extend `cost_compare.rs` with native metrics

**Story 1.2.1: Wire `native_compaction` into `CompactionComparison`**

- Task 1.2.1a — Add `use crate::claude_code_session::native_compaction::{extract_native_compaction_events, is_native_compacted, NativeCompactionEvent};` and add two fields to `CompactionComparison` (after `is_compacted`, before `compaction_metrics`, to keep the "consolette vs. native" fields visually adjacent to their existing `is_compacted` counterpart):
  ```rust
  pub is_native_compacted: bool,
  pub native_events: Vec<NativeCompactionEvent>,
  ```
  Files: `src/claude_code_session/cost_compare.rs` (lines ~26-43 struct definition).
- Task 1.2.1b — In `compare_compaction_cost` (lines 56-94), after the existing `let compacted = is_compacted(&rows);` block, add `let native_compacted = is_native_compacted(&rows); let native_events = extract_native_compaction_events(&rows);` (computed unconditionally, independent of `compacted`, since a session can be natively compacted without ever being touched by consolette's own compactor) and add both new fields to the `Ok(CompactionComparison { ... })` struct literal. Files: `src/claude_code_session/cost_compare.rs`.

  **Acceptance criterion**: "`CompactionComparison` carries native compaction data independent of consolette's own compaction state."
  **Given-When-Then**: Given a fixture transcript containing one native `compact_boundary` row with `compactMetadata: {"preTokens": 5000, "postTokens": 800}` and no consolette `consoletteCompact` marker anywhere, when `compare_compaction_cost(path, "claude-sonnet-5").await` runs, then the returned `CompactionComparison` has `is_compacted == false`, `compaction_metrics == vec![]`, `is_native_compacted == true`, and `native_events == vec![NativeCompactionEvent { pre_tokens: Some(5000), post_tokens: Some(800), trigger: None, duration_ms: None, cumulative_dropped_tokens: None, preserved_segment_tokens: None, preserved_messages: None, discovered_tools: None }]`.

**Story 1.2.2: Regression-proof the four-state combinations**

- Task 1.2.2a — Add a test asserting the existing 4 tests in `cost_compare.rs` (lines 132-210) still compile and pass unmodified after the struct gains two new fields (no code change needed here beyond running `cargo test`; this task is the explicit verification step, not a code edit). Files: `src/claude_code_session/cost_compare.rs` (verification only).
- Task 1.2.2b — Add one new test `compare_compaction_cost_should_report_both_native_and_consolette_compaction` covering a transcript with both a native `compact_boundary` row and a consolette `consoletteCompact` summary marker present, asserting `is_compacted == true && is_native_compacted == true`. Files: `src/claude_code_session/cost_compare.rs`.

  **Acceptance criterion**: "A session compacted by both Claude Code natively and by consolette is reported as both, never as only one."
  **Given-When-Then**: Given a fixture with a native `compact_boundary` row (`compactMetadata: {"preTokens": 3000, "postTokens": 900}`) followed by a consolette `consoletteCompact` summary row carrying `CompactionMetrics { tokens_before: 2000, tokens_after: 300, tokens_saved: 1700, estimated_cost_usd: Some(0.0009), real_cost_usd: None }`, when `compare_compaction_cost` runs, then `is_compacted == true`, `compaction_metrics == vec![that CompactionMetrics]`, `is_native_compacted == true`, and `native_events.len() == 1`.

### Phase 2 — Multi-Session Aggregation

#### Epic 2.1: `session_bi.rs` — per-session and cross-session types

**Story 2.1.1: `CompactionStatus` and `SessionComparisonRow`**

- Task 2.1.1a — Create `src/claude_code_session/session_bi.rs` with module doc comment, imports, and:
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
  #[serde(rename_all = "snake_case")]
  pub enum CompactionStatus {
      NativeOnly,
      ConsoletteOnly,
      Both,
      Neither,
  }
  impl CompactionStatus {
      pub fn from_flags(native: bool, consolette: bool) -> Self {
          match (native, consolette) {
              (true, true) => Self::Both,
              (true, false) => Self::NativeOnly,
              (false, true) => Self::ConsoletteOnly,
              (false, false) => Self::Neither,
          }
      }
  }
  ```
  Files: `src/claude_code_session/session_bi.rs` (new).

  **Acceptance criterion**: "Every session is classified into exactly one of four compaction states."
  **Given-When-Then**: Given `native = true, consolette = false`, when `CompactionStatus::from_flags(true, false)` is called, then it returns `CompactionStatus::NativeOnly` (and, exhaustively, `from_flags(false,false) == Neither`, `from_flags(true,true) == Both`, `from_flags(false,true) == ConsoletteOnly`).
- Task 2.1.1b — Add `SessionComparisonRow` struct (`#[derive(Debug, Clone, serde::Serialize)]`) with fields: `session_path: String`, `session_id: String`, `project: String`, `size_bytes: u64`, `modified_unix_secs: u64`, `status: CompactionStatus`, `native_event_count: usize`, `native_tokens_saved: Option<i64>`, `consolette_tokens_saved: Option<i64>`, `net_advantage_tokens: Option<i64>`, `no_compaction_total_tokens: u64`, `no_compaction_estimated_cost_usd: Option<f64>`, `chain_coverage_ratio: f64`. Files: `src/claude_code_session/session_bi.rs`.
- Task 2.1.1c — Add `SessionComparisonError { session_path: String, reason: String }` (`#[derive(Debug, Clone, serde::Serialize)]`). Files: `src/claude_code_session/session_bi.rs`.
- Task 2.1.1d — Add `SessionBiSnapshot { rows: Vec<SessionComparisonRow>, parse_failures: Vec<SessionComparisonError>, generated_at: chrono::DateTime<chrono::Utc> }` (`#[derive(Debug, Clone, serde::Serialize)]`). Files: `src/claude_code_session/session_bi.rs`.

**Story 2.1.2: Build one row per session file**

- Task 2.1.2a — Add `pub async fn build_session_comparison_row(file: &crate::claude_code_session::discovery::SessionFile, pricing_model: &str) -> Result<SessionComparisonRow, SessionComparisonError>`: call `compare_compaction_cost(&file.path, pricing_model).await`, mapping an `Err` to `SessionComparisonError { session_path: file.path.display().to_string(), reason: e.to_string() }`; on `Ok(comparison)`, sum `comparison.native_events.iter().filter_map(NativeCompactionEvent::tokens_saved).sum::<i64>()` into `native_tokens_saved` (`None` if `native_events` is empty, `Some(sum)` otherwise — an empty sum must not be confused with "unknown"), sum `comparison.compaction_metrics.iter().map(|m| m.tokens_saved).sum::<i64>()` into `consolette_tokens_saved` similarly, compute `net_advantage_tokens` as `Some(c - n)` only when both are `Some`, derive `status` via `CompactionStatus::from_flags(comparison.is_native_compacted, comparison.is_compacted)`, and derive `session_id`/`project` from `file.path` (session_id = file stem; project = the path segment immediately under `.claude/projects/`). Files: `src/claude_code_session/session_bi.rs`.

  **Acceptance criterion**: "Each session file is reduced to exactly one comparison row, or one recorded parse failure, never silently dropped."
  **Given-When-Then**: Given a `SessionFile { path: "/home/t/.claude/projects/my-repo/abc123.jsonl", size_bytes: 4096, modified: <t> }` whose contents parse successfully into a `CompactionComparison` with `is_native_compacted: true, native_events: [event with tokens_saved() == Some(7800)], is_compacted: false, compaction_metrics: []`, when `build_session_comparison_row(&file, "claude-sonnet-5").await` runs, then it returns `Ok(SessionComparisonRow { session_id: "abc123", project: "my-repo", status: CompactionStatus::NativeOnly, native_event_count: 1, native_tokens_saved: Some(7800), consolette_tokens_saved: None, net_advantage_tokens: None, .. })`.
- Task 2.1.2b — Add a unit test asserting `build_session_comparison_row` returns `Err(SessionComparisonError { .. })` rather than panicking when the underlying transcript genuinely fails to load/compare. **Fixture choice: a nonexistent file path** (option (a) from the Phase 3 repair loop's three candidates — a `File::open` I/O error). This was chosen over (b) invalid UTF-8 bytes and (c) a parent-chain cycle because it requires no byte-level fixture construction or multi-row cycle setup — a single `SessionFile { path: <a path that is never created>, .. }` is sufficient, and it exercises the same `Err` propagation path (`parse_session_file`'s `File::open(...).map_err(...)`, `transcript.rs:159-160`, surfacing through `compare_compaction_cost` into `build_session_comparison_row`'s `Err` mapping) as the other two options would. A file containing only unparseable JSON does **not** work here: `parse_session_file` (`transcript.rs:158-173`) skips unparseable lines and warns rather than erroring (proven by the existing test `parse_session_file_should_skip_and_warn_when_line_is_unparseable_json`, `transcript.rs:532-537`), so a file with only a garbage line parses to `Ok(vec![])`, and `build_turns(&[])` returns `Ok(Vec::new())` via the empty-rows fast path (`transcript.rs:256-258`) — that fixture produces `Ok(CompactionComparison{..all zero..})`, not `Err`, and was the (incorrect) fixture in the pre-repair draft of this task. Files: `src/claude_code_session/session_bi.rs`.

  **Acceptance criterion**: "A single unreadable/uncompareable transcript's failure is recorded and never crashes the aggregation."
  **Given-When-Then**: Given a `SessionFile { path: PathBuf::from("<tempdir>/does-not-exist.jsonl"), size_bytes: 0, modified: SystemTime::UNIX_EPOCH }` — i.e. a path that is never created on disk — when `build_session_comparison_row(&file, "claude-sonnet-5").await` is called, then it returns `Err(SessionComparisonError { session_path: <that path's display string>, reason: <contains "failed to open session file" or the underlying I/O error text, per `parse_session_file`'s error message at `transcript.rs:160`> })`, and the call does not panic.

**Story 2.1.3: Bounded-concurrency full-tree scan**

- Task 2.1.3a — Add `pub const SESSION_BI_SCAN_CONCURRENCY: usize = 16;` and `pub const SESSION_BI_PER_FILE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);` as named constants (not magic numbers) near the top of the module. Files: `src/claude_code_session/session_bi.rs`.
- Task 2.1.3b — Add `pub async fn build_session_bi_snapshot(session_glob: &str, pricing_model: &str, concurrency: usize, per_file_timeout: Duration) -> SessionBiSnapshot`: call `discovery::discover_sessions_glob(session_glob, SortBy::RecentFirst)?` — **not** `discovery::discover_sessions`, which reads the process-global `HOME` env var internally (`discovery.rs:55-61`) and has no injectable parameter, making it unsuitable for a function that must also run hermetically under test; `discover_sessions_glob` (`discovery.rs:70`) takes the glob as a plain argument instead — (on `Err`, return an empty `SessionBiSnapshot` with one `SessionComparisonError { session_path: "<scan>".into(), reason: e.to_string() }`), then `futures_util::stream::iter(files).map(|f| async move { tokio::time::timeout(per_file_timeout, build_session_comparison_row(&f, pricing_model)).await }).buffer_unordered(concurrency)`, collecting `Ok(Ok(row))` into `rows` and both `Ok(Err(e))` and `Err(_timeout)` into `parse_failures` (a timeout becomes a `SessionComparisonError { reason: "timed out after Ns" }`), then set `generated_at: chrono::Utc::now()`. The caller (`CostServerState::build()`/`build_with_session_glob`, see Task 3.1.1b) is solely responsible for computing the real `$HOME`-rooted glob; this function itself never reads `HOME` or any other env var. Files: `src/claude_code_session/session_bi.rs`.

  **Acceptance criterion**: "Scanning the whole session tree bounds concurrency and tolerates individual file failures/timeouts without losing other rows."
  **Given-When-Then**: Given a fixture directory containing 3 files — one valid uncompacted transcript, one valid natively-compacted transcript, and one corrupt-JSON file — when `build_session_bi_snapshot("<fixture-dir>/**/*.jsonl", "claude-sonnet-5", 2, Duration::from_secs(5)).await` is called (a plain string argument, no `HOME` mutation of any kind), then the resulting `rows.len() == 2` and `parse_failures.len() == 1`, and the corrupt file's path appears in `parse_failures`, not in `rows`.

#### Epic 2.2: Background refresh + `CostServerState` wiring

**Story 2.2.1: `spawn_session_bi_refresh_task`**

- Task 2.2.1a — Add `pub const SESSION_BI_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15 * 60);` and `pub fn spawn_session_bi_refresh_task(tx: tokio::sync::watch::Sender<std::sync::Arc<SessionBiSnapshot>>, session_glob: String, pricing_model: String, interval: Duration, concurrency: usize, per_file_timeout: Duration) -> tokio::task::JoinHandle<()>`: `tokio::spawn(async move { let mut ticker = tokio::time::interval(interval); ticker.tick().await; loop { ticker.tick().await; let snapshot = build_session_bi_snapshot(&session_glob, &pricing_model, concurrency, per_file_timeout).await; let _ = tx.send(std::sync::Arc::new(snapshot)); } })` — note the first `ticker.tick().await` fires immediately and is *consumed and discarded* here (not skipped as `spawn_pricing_refresh_task` does), because the eager first snapshot is computed separately and synchronously in `CostServerState::build()` before this task is even spawned (ADR-014); this loop only ever produces the *second* snapshot onward. `session_glob` is the same explicit glob string computed once by the caller (`CostServerState::build()`/`build_with_session_glob`) and moved into this task's closure — it is read from `$HOME` exactly once, never re-read from the environment on each tick. Files: `src/claude_code_session/session_bi.rs`.

  **Acceptance criterion**: "The background refresh task periodically replaces the snapshot without ever leaving the channel briefly empty or blocking startup."
  **Given-When-Then**: Given a `watch::channel` already seeded with an initial `Arc<SessionBiSnapshot>` (per Task 2.3.1a below) and `spawn_session_bi_refresh_task` called with `interval = Duration::from_millis(50)` in a test, when at least 60ms elapse, then `rx.borrow().generated_at` is later than the initial snapshot's `generated_at` (i.e., at least one refresh occurred), and at no point does `rx.borrow()` observe a "channel closed" error.

### Phase 3 — Dashboard HTTP Routes & UI

#### Epic 3.1: Wire the snapshot into `CostServerState` and add the JSON route

**Story 3.1.1: `CostServerState` gains the session BI channel**

- Task 3.1.1a — Add `session_bi_rx: tokio::sync::watch::Receiver<std::sync::Arc<SessionBiSnapshot>>` and `_session_bi_refresh: tokio::task::JoinHandle<()>` fields to `CostServerState` (`src/cost_metrics/server.rs:58-66`), with a doc comment on `_session_bi_refresh` matching `_pricing_refresh`'s existing one. Files: `src/cost_metrics/server.rs`.
- Task 3.1.1b — **Repair-loop correction**: the original draft of this task made `CostServerState::build()` call `build_session_bi_snapshot` directly using the `HOME`-rooted `discovery::discover_sessions`. That breaks test hermeticity for the one pre-existing test that calls `build()` (`serve_cost_should_expose_apply_result_via_http_route_when_pipeline_and_route_share_same_tracker`, `src/cost_metrics/server.rs:203-206`, which sets no `HOME` override today) — after the change it would silently perform a full scan of the real developer's `~/.claude/projects/**/*.jsonl` tree (7,840+ files) on every `cargo test` run — and creates a data race if a later test instead tried to fixture `HOME` via `std::env::set_var`, since Rust's default test harness runs tests concurrently within one process. **Chosen fix: a two-constructor split**, not an injectable-override-inside-`build()` design, because a plain second associated function requires no new field/parameter on `CostServerState` itself and keeps `build()`'s public signature — and therefore every existing call site, including the one pre-existing test above — completely unchanged. Concretely:
  - Add `pub async fn build_with_session_glob(session_glob: &str) -> Self` that does the real construction work: after the existing `pipeline.register_hook(hook);` line, compute the eager initial snapshot via `let initial_snapshot = crate::claude_code_session::session_bi::build_session_bi_snapshot(session_glob, crate::claude_code_session::DEFAULT_PRICING_MODEL, SESSION_BI_SCAN_CONCURRENCY, SESSION_BI_PER_FILE_TIMEOUT).await;`, seed the channel via `let (session_bi_tx, session_bi_rx) = watch::channel(Arc::new(initial_snapshot));`, spawn the refresh task via `spawn_session_bi_refresh_task(session_bi_tx, session_glob.to_string(), DEFAULT_PRICING_MODEL.to_string(), SESSION_BI_REFRESH_INTERVAL, SESSION_BI_SCAN_CONCURRENCY, SESSION_BI_PER_FILE_TIMEOUT)`, and add both new fields to the `CostServerState { .. }` struct literal.
  - Change the existing `pub async fn build() -> Self` into a thin wrapper: compute `let session_glob = format!("{}/.claude/projects/**/*.jsonl", std::env::var("HOME").unwrap_or_default());` (the same `HOME`-reading behavior `discover_sessions` used to perform internally, now done exactly once, as a plain string computation with no ongoing env coupling) and `return Self::build_with_session_glob(&session_glob).await;`. `build()`'s signature (`pub async fn build() -> Self`) does not change.
  - Tests that need a fixture corpus call `build_with_session_glob("<fixture-dir>/**/*.jsonl")` directly. No test anywhere calls `std::env::set_var("HOME", ...)`, which is what eliminates the cross-test data race — there is no shared mutable global left to race on.
  Files: `src/cost_metrics/server.rs`.

  **Acceptance criterion**: "Building `CostServerState` via a fixture glob produces a session BI snapshot synchronously, before the server can accept any request, with no environment-variable mutation and no effect on the pre-existing `build()` test."
  **Given-When-Then**: Given a fixture directory with 2 session files and its glob `"<fixture-dir>/**/*.jsonl"`, when `CostServerState::build_with_session_glob(&fixture_glob).await` completes, then `state.session_bi_rx.borrow().rows.len() == 2` immediately (no `.await`ing an interval tick required, and no `HOME` env var read or mutated by the test) — i.e., the eager-first-compute in ADR-014 is real, not aspirational. Separately: given the existing test `serve_cost_should_expose_apply_result_via_http_route_when_pipeline_and_route_share_same_tracker` (`server.rs:203-206`), which calls `CostServerState::build().await` unmodified, when it runs after this change, then it still passes with zero code changes to that test (it now transitively performs one real `$HOME`-rooted scan via the thin `build()` wrapper, exactly as `serve_cost`'s own production call site already would in normal operation — this is expected and matches pre-existing behavior for that one call site, not a new hazard).

**Story 3.1.2: `GET /v1/dashboard/sessions`**

- Task 3.1.2a — Add `pub fn dashboard_router(rx: watch::Receiver<Arc<SessionBiSnapshot>>) -> Router` to `src/cost_metrics/server.rs`, with `.route("/v1/dashboard/sessions", get(handler_dashboard_sessions)).with_state(rx)`. Files: `src/cost_metrics/server.rs`.
- Task 3.1.2b — Add `async fn handler_dashboard_sessions(State(rx): State<watch::Receiver<Arc<SessionBiSnapshot>>>) -> impl IntoResponse`: clone `rx.borrow().clone()`, build the JSON body `{ "generated_at": snapshot.generated_at, "rows": snapshot.rows, "parse_failure_count": snapshot.parse_failures.len(), "confidence_legend": { "native_tokens_saved": "native_reported", "consolette_tokens_saved": "estimated", "no_compaction_total_tokens": "estimated", "no_compaction_estimated_cost_usd": "estimated" } }`, return `(StatusCode::OK, Json(body))`. Files: `src/cost_metrics/server.rs`.

  **Acceptance criterion**: "The JSON endpoint reports one row per discoverable session plus a static confidence legend, never per-row confidence duplication."
  **Given-When-Then**: Given a seeded snapshot with 2 rows (one `NativeOnly`, one `Neither`), when `GET /v1/dashboard/sessions` is requested, then the response is `200` with a JSON body whose `rows` array has length 2, `parse_failure_count == 0`, and `confidence_legend.native_tokens_saved == "native_reported"`.

#### Epic 3.2: HTML dashboard page

**Story 3.2.1: Static HTML/CSS skeleton**

- Task 3.2.1a — Create `src/cost_metrics/dashboard.html` with a `<!doctype html>` skeleton, a `<table id="sessions">` with a `<thead>` row for columns (`Session`, `Project`, `Status`, `Native tokens saved`, `Consolette tokens saved`, `Net advantage`, `No-compaction cost`, `Chain coverage`), each `<th>` containing a `<button>` (not a bare clickable `<th>`, for keyboard/AT accessibility per `research/ux.md`) with an `aria-sort` attribute, a sticky `<thead>` via CSS `position: sticky; top: 0`, and a `<tbody id="sessions-body">` left empty for JS to populate. Include a `<div id="status-banner">` for loading/error/empty states. No `<script src>` to any external host — CSS uses only inline `<style>`. Files: `src/cost_metrics/dashboard.html` (new).
- Task 3.2.1b — Add `use std::sync::Arc;` (if not already imported) and `const SESSION_BI_DASHBOARD_HTML: &str = include_str!("dashboard.html");` near the top of `src/cost_metrics/server.rs`. Files: `src/cost_metrics/server.rs`.
- Task 3.2.1c — **(Blocker 2 fix)** Add a filter row to `dashboard.html`, positioned below the header row and above the sticky `<thead>` per `design/ux.md` Step 2's wireframe (a filter row separate from the header row, not inline in a `<th>`, per `research/ux.md` §1): a `<label for="filter-text">Filter path/project</label>` paired with `<input type="text" id="filter-text">` for a project/path substring match, and a `<label for="filter-status">Status</label>` paired with `<select id="filter-status"><option value="">All</option><option value="native_only">Native only</option><option value="consolette_only">Consolette only</option><option value="both">Both</option><option value="neither">Neither</option></select>` — option values match `CompactionStatus`'s `#[serde(rename_all = "snake_case")]` wire format exactly, with `""` meaning "any status." Also add a `<div id="row-count-readout">` for the "Showing N of M" text. Files: `src/cost_metrics/dashboard.html`.

  **Acceptance criterion**: "The filter row's inputs are keyboard-operable and screen-reader-nameable, matching `design/ux.md`'s wireframe."
  **Given-When-Then**: Given `dashboard.html` has loaded, when the DOM is inspected, then `#filter-text` and `#filter-status` each have an associated `<label>` (via `for`/`id`), both controls are reachable via Tab, and `#filter-status`'s options are exactly `["", "native_only", "consolette_only", "both", "neither"]` in value.

**Story 3.2.2: Vanilla JS fetch/render/sort**

- Task 3.2.2a — In `dashboard.html`'s `<script>`, add a `fetch('/v1/dashboard/sessions')` call; on success, store the parsed `rows` array in a module-scoped variable and call `applyFiltersAndSort()` (Task 3.2.3a) rather than `render(rows)` directly, so the initial paint already reflects the (empty, i.e. no-op) filter/sort state; on network/HTTP error, set `#status-banner`'s `textContent` (never `innerHTML`) to an error message and disable `#filter-text`/`#filter-status` (there is nothing meaningful to filter when the fetch itself failed); if `rows.length === 0` (the corpus is genuinely empty, not filtered-empty), set the banner to an empty-state message distinct from the fetch-error message, per `design/ux.md`'s distinct Loading/Fetch-error/Empty-zero states, instead of rendering an empty table. Files: `src/cost_metrics/dashboard.html`.
- Task 3.2.2b — Add `render(rows)`: clear `#sessions-body` via `replaceChildren()`, then for each row build a `<tr>` via `document.createElement` and each cell via `createElement('td')` + `.textContent = String(value)` (never `.innerHTML`, since `session_path`/`project` are transcript-derived strings that must be treated as untrusted per requirements.md's XSS constraint), append to the body. `render` takes the already-filtered-and-sorted array as its argument and performs no filtering/sorting itself — that is `applyFiltersAndSort()`'s job (Task 3.2.3a). Files: `src/cost_metrics/dashboard.html`.
- Task 3.2.2c — Add click handlers on each column-header `<button>` that re-sort the in-memory `rows` array (numeric compare for numeric columns, `Intl.Collator`/string compare for text columns, `null`/`undefined` values sorted last regardless of direction) and toggle that header's `aria-sort` between `ascending`/`descending`/`none`, resetting any other header's `aria-sort` to `none`, then re-call `applyFiltersAndSort()` (not `render(rows)` directly) so an active text/status filter stays applied across a sort change. The default sort on initial load, before any header is clicked, is `net_advantage_tokens` descending, per `design/ux.md` Step 3. Files: `src/cost_metrics/dashboard.html`.

  **Acceptance criterion**: "Clicking a column header sorts the visible table by that column without a page reload or server round-trip, and never interprets transcript-derived text as HTML."
  **Given-When-Then**: Given the dashboard has rendered 2 rows with `net_advantage_tokens` values `500` and `-100`, and one row's `project` field is the literal string `<img src=x onerror=alert(1)>` (a hostile project directory name), when the operator clicks the "Net advantage" column header once, then the table body re-orders to show the `-100` row first (ascending) with `aria-sort="ascending"` on that header, and the hostile `project` value is rendered as literal visible text (`<img src=x onerror=alert(1)>` appearing as text in the cell), not executed as markup — verified by asserting no additional `<img>` element exists in the DOM and no `alert` fires.

**Story 3.2.3: Combined filter + sort pipeline and row-count readout** *(Blocker 2 fix — `requirements.md`'s Success Metrics require the table be "filterable by at least project/path substring and by compaction status," and `design/ux.md` Step 5's UX acceptance criteria 1-6 specify the exact sort/filter mechanics this story implements; no prior task in this plan implemented filtering.)*

- Task 3.2.3a — Add `applyFiltersAndSort()`: reads `#filter-text`'s current value (trimmed, case-insensitive) and `#filter-status`'s current `value`, filters the module-scoped full `rows` array via `row => (filterText === '' || (row.session_path + row.project).toLowerCase().includes(filterText)) && (filterStatus === '' || row.status === filterStatus)`, applies the currently-active sort column/direction (tracked in module-scoped state set by Task 3.2.2c's click handlers, defaulting to `net_advantage_tokens` descending per `design/ux.md` Step 3) to the filtered array, calls `render(filteredSortedRows)`, and calls `updateRowCountReadout(filteredSortedRows.length, rows.length)` (Task 3.2.3b). Wire this function to `#filter-text`'s `input` event and `#filter-status`'s `change` event (both re-run the combined pipeline on every keystroke/selection with no network call, per `design/ux.md` Step 3 — filtering is purely client-side over the already-fetched array). Files: `src/cost_metrics/dashboard.html`.

  **Acceptance criterion**: "Typing a path/project substring and selecting a compaction status filter the visible table by both criteria combined (AND), instantly and without a network request, and the active sort is preserved across a filter change."
  **Given-When-Then**: Given the dashboard has fetched 3 rows — `{project: "my-repo", status: "native_only", net_advantage_tokens: 500}`, `{project: "other-repo", status: "both", net_advantage_tokens: -100}`, `{project: "my-repo", status: "both", net_advantage_tokens: 10}` — sorted by the default `net_advantage_tokens` descending, when the operator types `"my-repo"` into `#filter-text` and selects `"both"` in `#filter-status`, then exactly one row (`{project: "my-repo", status: "both", net_advantage_tokens: 10}`) is rendered in `#sessions-body`, no `fetch` call is made as a result of typing/selecting, and the sort order among the surviving rows still reflects `net_advantage_tokens` descending (the currently-active sort, unchanged by the filter).
- Task 3.2.3b — Add `updateRowCountReadout(shown, total)`: sets `#row-count-readout`'s `textContent` to `` `Showing ${shown} of ${total}` `` (matching `design/ux.md` Step 2's exact "Showing N of M" wording) and, when the parent snapshot's `parse_failure_count` (captured from the fetch response in Task 3.2.2a) is greater than zero, appends a note such as `` ` (${parseFailureCount} session(s) failed to parse — see server log)` `` per `design/ux.md` Step 2's parse-failure-count note and `research/ux.md` §4's "don't mistake absence for a clean zero" principle. When `shown === 0` and `total > 0` (filters exclude everything), set `#status-banner`'s `textContent` to a distinct "no sessions match your filters" message (per `design/ux.md`'s Empty-after-filter state, separate from the Empty-zero/Fetch-error states already handled in Task 3.2.2a) rather than leaving the table silently blank with no explanation. Files: `src/cost_metrics/dashboard.html`.

  **Acceptance criterion**: "The operator always sees how many rows are showing versus the total, and an all-filtered-out result is visually distinct from a genuinely empty corpus or a fetch error."
  **Given-When-Then**: Given 10 fetched rows with `parse_failure_count == 2`, when no filter is applied, then `#row-count-readout` reads `"Showing 10 of 10 (2 session(s) failed to parse — see server log)"`; when the operator then enters a filter substring matching zero rows, then `#row-count-readout` reads `"Showing 0 of 10 ..."` and `#status-banner` displays a "no sessions match your filters" message distinct from both the Task 3.2.2a fetch-error message and the empty-corpus message.

#### Epic 3.3: Route wiring

**Story 3.3.1: Merge `dashboard_router` into `serve_cost`**

- Task 3.3.1a — Add a `GET /dashboard` route to `dashboard_router` returning `Html(SESSION_BI_DASHBOARD_HTML)`. Files: `src/cost_metrics/server.rs`.
- Task 3.3.1b — In `serve_cost` (`src/cost_metrics/server.rs:136-151`), change `let router = cost_router(Arc::clone(&state.tracker));` to also merge the dashboard router: `let router = cost_router(Arc::clone(&state.tracker)).merge(dashboard_router(state.session_bi_rx.clone()));` — `cost_router`'s own signature and the existing `/v1/cost/{session_key}` route are untouched. Files: `src/cost_metrics/server.rs`.

  **Acceptance criterion**: "The existing `/v1/cost/{session_key}` route and its tests are unaffected by adding the dashboard routes."
  **Given-When-Then**: Given the full existing `cost_metrics::server` test module (`src/cost_metrics/server.rs:153-442`), when `cargo test --lib cost_metrics::server` is run after this change, then all pre-existing tests pass unmodified, and a new test hitting `GET /v1/cost/{session_key}` for an unknown key still returns `404` with `{"error":"session_not_found",...}` exactly as before.
- Task 3.3.1c — Add a new integration test in `tests/cost_metrics_end_to_end.rs` that starts a `serve_cost`-equivalent router via `CostServerState::build_with_session_glob(<fixture-dir-glob>)` (NOT `CostServerState::build()` — using the fixture-glob constructor keeps this test hermetic and avoids re-scanning the operator's real `~/.claude/projects/` tree on every `cargo test` run, matching the same fixture pattern used by Task 3.1.1b's `build_with_session_glob` test) + the merged router, using `axum::body::Body`/`tower::ServiceExt::oneshot` rather than binding a real port, and asserts `GET /dashboard` returns `200` with `Content-Type: text/html` and `GET /v1/dashboard/sessions` returns `200` with a JSON body containing a `rows` key. Files: `tests/cost_metrics_end_to_end.rs`.

### Phase 4 — Module Registration & Verification

#### Epic 4.1: Register new modules

- Task 4.1.1a — Add `pub mod native_compaction;` and `pub mod session_bi;` to the module-declaration block in `src/claude_code_session/mod.rs` (alongside the existing `pub mod boundary;` etc. at lines 7-15), keeping alphabetical order (`native_compaction` after `mcp_server`, `session_bi` after `prune`/before `summarize` — matching the existing alphabetized list). Files: `src/claude_code_session/mod.rs`.

  **Acceptance criterion**: "The new modules are part of the crate's public module tree and compile cleanly with the rest of the workspace."
  **Given-When-Then**: Given `native_compaction.rs` and `session_bi.rs` exist with the code from Phases 1-2, when `pub mod native_compaction; pub mod session_bi;` are added to `mod.rs` and `cargo build` is run, then the build succeeds with zero new warnings from `#[warn(missing_docs)]` or clippy's default lint set (any new `pub` item lacking a doc comment must be given one before this task is considered done, matching this module family's existing doc-comment density).

#### Epic 4.2: End-to-end verification against the real corpus

- Task 4.2.1a — Run `cargo test` for the whole workspace and confirm all tests (pre-existing and new) pass. Files: none (verification only).
- Task 4.2.1b — Run `cargo clippy --all-targets` and resolve any new warnings introduced by `native_compaction.rs`/`session_bi.rs`/the `server.rs`/`cost_compare.rs` edits. Files: as needed based on clippy output.
- Task 4.2.1c — Manually run `consolette serve-cost` against the operator's real `~/.claude/projects/` tree (7,840 files), open `http://127.0.0.1:<port>/dashboard` in a browser, and confirm: the page renders within a reasonable time without hanging the tab, sort/filter works on at least the `Status` and `Net advantage` columns, and at least one real `NativeOnly`, one real `ConsoletteOnly` (if any exist in the corpus), and one real `Neither` row appear — satisfying the Success Metrics requirement that the native-parser's shape be confirmed against real data. If no real `compactMetadata` sample is found in the live corpus during this manual check, capture one real `compact_boundary` row's raw JSON (redacting any session content) and add it as a fixture-based regression test in `native_compaction.rs` per Task 1.1.1's acceptance criterion. Files: `src/claude_code_session/native_compaction.rs` (only if a new real-data fixture test is added as a result of this manual check).

  **Acceptance criterion**: "The dashboard is verified end-to-end against the operator's real, current transcript corpus, not just synthetic fixtures."
  **Given-When-Then**: Given `consolette serve-cost --port 8787` is running against the real `~/.claude/projects/` tree, when the operator navigates to `http://127.0.0.1:8787/dashboard`, then the page loads a populated table (row count roughly matching the operator's known session count) without a browser "page unresponsive" warning, and the `/v1/dashboard/sessions` JSON response's `generated_at` timestamp is within the last `SESSION_BI_REFRESH_INTERVAL` window of the request time.

## Step 5: Architecture Decision Records

Written to `project_plans/compaction-bi-dashboard/decisions/`:

- **ADR-014** — Precompute-at-startup + periodic background refresh for the session BI aggregation (vs. per-request scan, vs. a persisted on-disk cache).
- **ADR-015** — Vanilla JS, `include_str!`-inlined HTML for the session BI dashboard (vs. a bundled table library, a frontend framework, or `tower-http` static file serving).

Both are non-default architectural choices for this codebase (the first new multi-file background-refresh cache since `pricing.rs`'s; the first new HTML-serving route since `dashboard.rs`, deliberately diverging from that file's CDN-dependent precedent) and warranted stubs per Step 5's instruction.

## Step 6: Summary

- **Epics**: 9 (1.1, 1.2, 2.1, 2.2, 3.1, 3.2, 3.3, 4.1, 4.2).
- **Stories**: 14 (1.1.1, 1.1.2, 1.2.1, 1.2.2, 2.1.1, 2.1.2, 2.1.3, 2.2.1, 3.1.1, 3.1.2, 3.2.1, 3.2.2, 3.2.3, 3.3.1 — Epics 4.1/4.2 hold verification tasks directly, with no story-level grouping).
- **Tasks**: 38 (counted across all stories/epics above; +3 versus the pre-repair-loop draft: Task 3.2.1c and Tasks 3.2.3a/3.2.3b added for filtering. The `build_with_session_glob` split (Blocker 1) is spelled out inline in Task 3.1.1b rather than as a new task number — it is a change to how that existing task is implemented, not new plan scope).
- **Domain Glossary terms**: 18.
- **ADRs written**: 2 (ADR-014, ADR-015).
- **Flagged choices requiring no new Cargo dependency**: confirmed — `futures-util`, `tokio`, `chrono` are all already present and sufficient; `dashboard.rs`'s Chart.js CDN pattern was explicitly rejected as precedent (see ADR-015).
- **No existing signature/behavior changes**: `boundary::is_compacted`, `cost_router`'s signature, and all pre-existing tests in `boundary.rs`, `cost_compare.rs`, and `cost_metrics/server.rs` are preserved; only additive fields/routes/modules were introduced. `CostServerState::build()`'s public signature is likewise unchanged (see Task 3.1.1b) — the real construction work moved to a new `build_with_session_glob(session_glob: &str)` associated function.
- **Phase 3 repair-loop corrections** (post architecture-review.md/adversarial-review.md, both BLOCKED): (1) `build_session_bi_snapshot` and `spawn_session_bi_refresh_task` now take an explicit `session_glob` parameter and call `discovery::discover_sessions_glob` instead of the `HOME`-reading `discovery::discover_sessions`, and `CostServerState::build()` was split into a thin `build()` wrapper plus a new `build_with_session_glob()` that tests call directly with a fixture glob — eliminating both the test-hermeticity break and the `std::env::set_var("HOME", ...)` data race identified independently by both reviewers (Task 2.1.3b, Task 2.2.1a, Task 3.1.1b, Domain Glossary). (2) Added Task 3.2.1c and Story 3.2.3 (Tasks 3.2.3a/3.2.3b) to implement the path/project-substring and compaction-status filtering that `requirements.md` requires and the original plan omitted entirely (adversarial-review.md Blocker 2). (3) Rewrote Task 2.1.2b's fixture and Given-When-Then to use a nonexistent file path instead of a garbage-JSON-line fixture, which does not actually produce an `Err` from `build_session_comparison_row` (adversarial-review.md Blocker 3).
