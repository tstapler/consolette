# Architecture Research: compaction-bi-dashboard

Scope: where native-compaction parsing and multi-session aggregation should
live, and how the dashboard route/page wires into the existing
`serve-cost` process. This is CRUD/reporting over existing read paths, not
multi-actor business logic — no Event-Command-Policy table.

All citations are `path:line` against the working tree at commit
`de68db4` (`src/claude_code_session/boundary.rs`'s last touching commit;
`git log -1 --format=%H -- src/claude_code_session/boundary.rs`).

## 1. Native-compaction parser: new sibling module, not a `boundary.rs` addition

`boundary.rs` is entirely about consolette's own `consoletteCompact` marker
convention — its own module doc says so explicitly
(`src/claude_code_session/boundary.rs:1-8`), and `CompactionMetrics`
(`src/claude_code_session/boundary.rs:73-94`) is documented as "stamped onto
every summary-turn row" by *consolette's own* `compact_session` — a
consolette-run-scoped struct, not a generic one. Requirements explicitly
forbid a name collision with this struct (Scope: "distinct from
consolette's own `CompactionMetrics`") and forbid changing `is_compacted()`
semantics (Constraints).

Recommendation: add `src/claude_code_session/native_compaction.rs`, a
sibling to `boundary.rs`, `cost_compare.rs`, and `discovery.rs` in the same
`claude_code_session` module (see `mod.rs` for how siblings are declared —
`boundary`, `cost_compare`, `discovery` are already `pub mod` entries there
alongside `transcript`). Rationale:
- Keeps `boundary.rs` semantically pure (consolette-marker detection only),
  matching its existing doc comment and avoiding a misleading name if a
  function like `is_compacted` grew a native-aware overload in the same
  file.
- Naming: `NativeCompactionEvent` (struct) and
  `extract_native_compaction_events(rows: &[TranscriptRow]) ->
  Vec<NativeCompactionEvent>` (function) — mirrors
  `extract_compaction_metrics`'s shape (`src/claude_code_session/boundary.rs:104`)
  for a reader already familiar with that function, while `Event` (vs.
  `Metrics`) signals this is Claude-Code-observed telemetry per boundary
  row, not a consolette-run aggregate. Avoids "CompactionMetrics" collision
  entirely by construction.
- A `is_native_compacted(rows: &[TranscriptRow]) -> bool` convenience
  function belongs in the new module too, not as a second code path bolted
  onto `boundary::is_compacted` — requirements explicitly say
  `is_compacted()`'s existing semantics must not change and its callers
  (`cost_compare.rs`, session_compaction pipeline) must be unaffected.

### Row shape to parse (confirmed against real data)

A native boundary row, verified via `rg` against a real transcript under
`~/.claude/projects/`:

```json
{
  "type": "system",
  "subtype": "compact_boundary",
  "compactMetadata": {
    "trigger": "auto",
    "preTokens": 111534,
    "postTokens": 13834,
    "durationMs": 110854,
    "cumulativeDroppedTokens": 97700,
    "preCompactDiscoveredTools": ["TaskGet", "TaskList", "TaskOutput"],
    "preservedSegment": {"headUuid": "...", "anchorUuid": "...", "tailUuid": "..."},
    "preservedMessages": {"anchorUuid": "...", "uuids": [...], "allUuids": [...]}
  }
}
```

Parsing plan, using the existing infrastructure:
- Detection: `row.fields().extra.get("subtype").and_then(Value::as_str) ==
  Some("compact_boundary")` — `subtype` lands in `RowFields::extra` today
  because it isn't a named field (`src/claude_code_session/transcript.rs:33-45`),
  exactly the same access pattern `is_boundary_or_summary_row` already uses
  for `consoletteCompact` (`src/claude_code_session/boundary.rs:31-42`).
  `type == "system"` is redundant to check separately since
  `TranscriptRow::System` already carries this, but the row could
  theoretically arrive as `Unknown` if this module's manual
  `Deserialize` ever fails to tag it — check `row.fields().extra`
  regardless of variant, matching existing practice.
- Extraction: `row.fields().extra.get("compactMetadata")`, then
  best-effort field-by-field extraction (not a single
  `serde_json::from_value::<NativeCompactionEvent>` deserialize) — the
  Rabbit Holes section requires every field optional/best-effort and
  "skip-and-log rather than abort" on unexpected shape. A struct with
  `#[serde(default)]` on every field plus `#[serde(deny_unknown_fields)]`
  absent achieves "never panic," but the row-level extraction function
  should still catch a `serde_json::from_value` `Err` and `tracing::warn!`
  + skip that one row rather than fail the whole file, per Observability
  Requirements ("A log line when a session file fails to parse or yields
  unexpected `compactMetadata` shape").
- Suggested struct (all fields `Option`/optional-with-default, deliberately
  not mirroring `CompactionMetrics`'s all-required-`u64` shape from
  `src/claude_code_session/boundary.rs:73-94`):

```rust
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NativeCompactionEvent {
    pub trigger: Option<String>,          // "auto" | "manual" | ...
    pub pre_tokens: Option<u64>,
    pub post_tokens: Option<u64>,
    pub duration_ms: Option<u64>,
    pub cumulative_dropped_tokens: Option<u64>,
    // preservedSegment/preservedMessages/preCompactDiscoveredTools:
    // capture presence, not full UUID lists, unless the dashboard needs them.
}
```
  `tokens_saved` should be a derived method
  (`pre_tokens.zip(post_tokens).map(|(pre, post)| pre.saturating_sub(post))`)
  rather than a stored field, since it's fully determined by the other two
  and storing it invites drift if a row has only one of the two fields.

- Unit test (Success Metrics requires one against "a real captured
  example"): inline the JSON blob above (trimmed) as a fixture, matching
  the existing test style of building `TranscriptRow` JSON lines by hand
  (see `cost_compare.rs:103-130`'s `user_row`/`assistant_row` helpers and
  `boundary.rs`'s own tests further down the file).

## 2. Multi-session aggregation: new module, not an extension of `cost_compare.rs`

`cost_compare.rs` is single-session and async-per-turn-token-estimate
(`compare_compaction_cost`, `src/claude_code_session/cost_compare.rs:56-94`,
calls `super::estimate_turn_tokens` per turn). Requirements ask for a
*per-session comparison struct carrying both native and consolette metrics
side by side* (Scope) plus a *separate* multi-session walk producing one row
per session (Scope, 4th bullet) — two distinct new responsibilities.

Recommendation: two additions, not one:
1. Extend `cost_compare.rs` with a new `NativeCompactionComparison`-bearing
   variant of the existing single-session flow — either add a sibling
   function `compare_native_compaction(session_path, pricing_model) ->
   NativeCompactionComparison` next to `compare_compaction_cost`, or fold
   native events into the *existing* `CompactionComparison` struct
   (`src/claude_code_session/cost_compare.rs:26-43`) as an additional
   field (e.g. `pub native_events: Vec<NativeCompactionEvent>`, populated
   unconditionally alongside `compaction_metrics`). The latter is
   preferable: it's one call per session, matches the requirement that
   "a session can have native compaction, consolette compaction, both, or
   neither" (Scope) — a single struct expressing all four states beats two
   structs a caller must remember to call in tandem — and costs nothing
   extra per session since `parse_session_file`/`build_turns` are already
   being run for the consolette side.
2. A **new module** — `src/claude_code_session/session_bi.rs` (matching the
   `session_key`/`session_compaction` naming already used elsewhere in the
   crate, e.g. `SessionKey` in `src/cost_metrics/server.rs:35`) — that:
   - calls `discovery::discover_sessions` (or `discover_sessions_glob`) to
     get the file list,
   - runs `cost_compare::compare_compaction_cost` (extended per above) per
     file,
   - collects results into `Vec<SessionComparisonRow>` (one row = one
     session: path, native summary, consolette summary, chain coverage,
     size/mtime from `SessionFile`).
   This is a *reporting/aggregation* concern layered over `cost_compare`,
   not a natural extension of `cost_compare.rs` itself, which is
   documented and tested as single-session
   (`src/claude_code_session/cost_compare.rs:45-54`'s doc comment: "Compares
   ... `session_path`"). Putting the fan-out loop there would mix the two
   granularities in one file's public API and one file's test suite.
   `dashboard.rs` was considered as the module name but rejected — HTTP/
   presentation naming leaking into `claude_code_session` (a
   transport-agnostic domain module per this repo's `CLAUDE.md`
   Architecture Notes: "Keep transport code ... thin; put real logic where
   it's independently testable").

## 3. Data flow / concurrency: precompute at startup + background refresh, following the existing pricing-refresh precedent

1,135 files is not free to parse per-request (each parse does file I/O +
`build_turns` + per-turn tiktoken estimation), and the requirements/rabbit
holes explicitly anticipate this ("Scanning 1,135+ files per request could
be slow enough to matter... if slow, an in-memory cache with explicit
refresh"). There's already a direct structural precedent in this same
server for "background task refreshes a `watch` channel, handlers read the
latest snapshot synchronously":

- `spawn_pricing_refresh_task` (`src/cost_metrics/pricing.rs:221`, imported
  at `src/cost_metrics/server.rs:32`) is spawned once in
  `CostServerState::build()` (`src/cost_metrics/server.rs:75-96`), writes
  into a `tokio::sync::watch::channel` (`pricing_tx`/`pricing_rx`,
  `src/cost_metrics/server.rs:76`), and the `_pricing_refresh:
  tokio::task::JoinHandle<()>` field (`src/cost_metrics/server.rs:65`) is
  held only to keep the task alive for the server's lifetime — the exact
  comment ("Held only to keep it alive... dropped, and thus aborted, when
  the server shuts down") is the pattern to reuse verbatim.

Recommendation: add a second `watch::channel<Arc<Vec<SessionComparisonRow>>>`
(or a wrapper struct also carrying "last refreshed at" for the UI), a
`spawn_session_bi_refresh_task` that does one full scan on startup and then
re-scans on an interval (or exposes a manual refresh trigger — the Open
Questions leave "manual vs. periodic" undecided; either satisfies "no new
datastore" and "in-memory only, rebuilt on process restart"). `CostServerState`
gains a `session_bi_rx: watch::Receiver<Arc<Vec<SessionComparisonRow>>>`
field (or the row type wrapped) alongside `_pricing_refresh`, and the JSON
route reads the latest value out of the receiver synchronously — no
per-request scan, no request ever blocks on file I/O.

Given a dashboard-refresh cadence in the minutes-to-manual range (not the
pricing table's 24h — session data changes as Tyler actually uses Claude
Code), a manual refresh endpoint (`POST /v1/dashboard/refresh` or similar)
that triggers an immediate re-scan is worth pairing with a periodic
fallback (e.g. every 5–10 minutes) — this satisfies the dashboard UX
("renders without hanging") without inventing a websocket/live-update
mechanism, which Out of Scope explicitly excludes.

**Parallelism**: no `rayon` dependency exists in `Cargo.toml` (checked
directly — only `tokio`, `tokio-util`, `tokio-stream` are present, no
`rayon` entry). Two options, in order of preference:
- `tokio::task::spawn_blocking` per file (or chunked), since each
  session's parse is currently `async fn compare_compaction_cost` but its
  actual heavy work — `parse_session_file` (sync, does `BufReader` line
  reads, `src/claude_code_session/transcript.rs:14-16`) and
  `TiktokenEstimator` calls — is likely CPU/IO-blocking rather than
  naturally async. Fanning 1,135 `spawn_blocking` tasks out via
  `futures::future::join_all` (or manual `tokio::spawn` + `JoinSet`) inside
  the background refresh task is the path of least new dependency: `tokio`
  is already `features = ["full"]` (`Cargo.toml`), which includes
  `rt-multi-thread` and `JoinSet`.
- Adding `rayon` is unnecessary weight for a background task that runs
  every few minutes, not per-request — the Appetite section explicitly
  rules out "no new external dependencies expected." Skip it.

Either way this work happens *inside the refresh task*, never on the
request path, so even an unoptimized sequential-but-`spawn_blocking`
implementation is acceptable for a correctness-first pass — 1,135 files
sequentially at (from Success Metrics' framing) sub-second-per-file parse
time is very likely single-digit seconds total, well inside "doesn't hang
the browser tab" once it's off the request path. A Phase 2/6 timing check
against the real tree should confirm this number before deciding whether
parallelizing the refresh task is worth the complexity at all.

## 4. Route/page wiring: additive routes on the existing router, no change to `/v1/cost/{session_key}`

`cost_router` (`src/cost_metrics/server.rs:102-106`) is a small,
easily-extended `Router::new().route(...).with_state(tracker)` builder.
Two structural options:
- Add `.route("/v1/dashboard/sessions", get(handler_dashboard_sessions))`
  and `.route("/", get(handler_dashboard_page))` (or `/dashboard`) directly
  onto the same `Router`, but this requires `with_state` to carry *both*
  `Arc<CostTracker>` and the new `watch::Receiver<...>` (or a wrapper
  struct combining them) — axum only supports one state type per router
  without `.with_state` calls at different route groups joined by
  `.merge()`.
- Cleaner: build a second small `Router` (e.g. `dashboard_router(rx:
  watch::Receiver<Arc<Vec<SessionComparisonRow>>>) -> Router`) with its own
  `.with_state(rx)`, then `.merge()` it onto `cost_router`'s result in
  `serve_cost` (`src/cost_metrics/server.rs:136-143`, where `router` is
  currently just `cost_router(...)`). This keeps `cost_router`'s existing
  signature, its existing tests (`serve_cost_should_respond_404_...`,
  `serve_cost_should_expose_apply_result_via_http_route_...`,
  `src/cost_metrics/server.rs:176-197` and `203+`) completely untouched,
  and matches the requirement "without disrupting the existing
  `/v1/cost/{session_key}` route or its tests."

Route naming: Open Questions in requirements.md flag
`/v1/cost/sessions` vs. a new `/v1/dashboard/...` namespace as undecided.
Given `/v1/cost/{session_key}` already claims the `/v1/cost/*` prefix for
single-session reports and `serve_cost`'s doc comment
(`src/cost_metrics/server.rs:1-16`) frames this whole process as cost
tooling, `/v1/dashboard/sessions` (JSON) + `/` or `/dashboard` (HTML) reads
as the cleaner split — "cost" stays single-session-report-shaped,
"dashboard" is explicitly the new multi-session aggregate concern. This is
a naming call for Phase 3 planning to confirm, not something this research
should force.

Static HTML/JS: since no template engine is a dependency here, the
handler can `Html(include_str!("dashboard.html"))` (a static asset baked
into the binary via `include_str!`, zero runtime file I/O, matching the
"no new dependency" constraint) with vanilla `<script>` doing `fetch('/v1/dashboard/sessions')`
client-side, sortable via click handlers on `<th>` elements and filterable
via a plain JS `.filter()` over the fetched array — no build step, matching
Alternatives Considered's explicit rejection of a frontend framework.

## 5. Session discovery: `discovery.rs`'s existing default glob already covers this — no divergence needed

Requirements' Open Questions asks whether dashboard discovery needs to
differ from `discovery.rs`'s current default, "given native-compaction
data can exist in files consolette has never touched." Checked directly:
`discover_sessions` (`src/claude_code_session/discovery.rs:55-61`) already
globs the *entire* `~/.claude/projects/**/*.jsonl` tree unconditionally —
`format!("{}/.claude/projects/**/*.jsonl", home.display())` — with no
filter for "sessions consolette has touched." It is not scoped to
consolette-run sessions today; it's a plain filesystem walk. The
aggregation module should call `discover_sessions(SortBy::RecentFirst)`
(or whichever default the dashboard UI wants as its initial sort) directly
— no new discovery function needed, and Constraints' "must reuse
`discovery.rs`'s session-listing logic rather than re-implementing a glob
walk" is satisfied trivially. `discover_sessions_glob` remains available
if a future need arises to scope to a subdirectory, but nothing in this
feature requires it.

## Summary of new files/changes

| Concern | File | Change |
|---|---|---|
| Native compaction row parsing | `src/claude_code_session/native_compaction.rs` (new) | `NativeCompactionEvent` struct + `extract_native_compaction_events`/`is_native_compacted` |
| Per-session comparison (native + consolette) | `src/claude_code_session/cost_compare.rs` | Add `native_events: Vec<NativeCompactionEvent>` field to existing `CompactionComparison`, populated in `compare_compaction_cost` |
| Multi-session aggregation | `src/claude_code_session/session_bi.rs` (new) | `SessionComparisonRow` struct + a function that walks `discovery::discover_sessions` and calls `compare_compaction_cost` per file |
| Background scan/cache | `src/cost_metrics/server.rs` (or a new `src/cost_metrics/session_bi_refresh.rs`) | `spawn_session_bi_refresh_task`, mirroring `spawn_pricing_refresh_task`; new `watch::channel` held in `CostServerState` |
| JSON + HTML routes | `src/cost_metrics/server.rs` | New `dashboard_router(...)` merged into `serve_cost`'s router; `cost_router` and its tests untouched |
| Module registration | `src/claude_code_session/mod.rs` | `pub mod native_compaction;` and `pub mod session_bi;` |

No new `Cargo.toml` dependencies required (`glob`, `tokio` full features,
`axum`, `serde`/`serde_json` all already present and sufficient).
