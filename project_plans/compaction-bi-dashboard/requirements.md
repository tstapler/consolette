# Requirements: compaction-bi-dashboard

**Date**: 2026-08-19
**Type**: feature addition
**Complexity**: 3 — system design

## Problem Statement
Consolette can already estimate the cost/token impact of its own compaction on a single session (`compare_compaction_cost` in `src/claude_code_session/cost_compare.rs`), but it has no way to answer two related questions the operator (Tyler) actually needs answered to decide whether consolette's compaction is worth running at all:

1. **Does Claude Code's own native auto-compaction already solve this?** `boundary.rs`'s `is_compacted()`/`extract_compaction_metrics()` only recognize consolette's own `consoletteCompact` markers. They are blind to Claude Code's native `"type":"system","subtype":"compact_boundary"` rows carrying a `compactMetadata` object (`trigger`, `preTokens`, `postTokens`, `durationMs`, `cumulativeDroppedTokens`, `preservedSegment`/`preservedMessages`, `preCompactDiscoveredTools`) — a real, large, already-on-disk dataset (6,201 native compaction events across 1,135 session files under `~/.claude/projects/`, confirmed by direct `rg` inspection).
2. **How does that compare across many sessions at once, not just one?** `compare_compaction_cost` and `discovery.rs`'s `list-sessions` both operate at the single-session or single-list-of-files granularity. There is no aggregate, sortable, side-by-side view of "session X: consolette saved N tokens / $Y; native saved M tokens / $Z" across the whole `~/.claude/projects/` tree.

## Baseline
Today, answering "is consolette's compaction pulling its weight compared to what Claude Code already does for free?" requires manually opening individual `.jsonl` files, `rg`-searching for `compactMetadata`, and doing arithmetic by hand per session — there is no tooling path at all, let alone a comparative one.

## Users / Consumers
The proxy/tool operator (Tyler), running consolette locally — an operational/tuning tool, not end-user-facing. Consumed via a browser hitting a new HTML page served by the existing `consolette serve-cost` process (already loopback-only, port 8787 default).

## Success Metrics
- `boundary.rs` (or a new sibling module) can parse a native `compact_boundary` row's `compactMetadata` into a typed struct and a unit test using a real captured example (shape confirmed against actual `~/.claude/projects/` data) passes.
- `consolette serve-cost` exposes a new JSON endpoint that, given the `~/.claude/projects/**/*.jsonl` tree, returns one row per session with both native-compaction and consolette-compaction metrics (counts, tokens saved, estimated cost, chain coverage) side by side.
- Opening `http://127.0.0.1:8787/` (or a dedicated dashboard path) in a browser renders a table of all discovered sessions from that endpoint, sortable by clicking any column header (tokens saved, cost, session size, etc.) and filterable by at least project/path substring and by compaction status (native only / consolette only / both / neither).
- Running the aggregation against the operator's real `~/.claude/projects/` tree (1,135+ files) completes and renders without the browser tab hanging (manual smoke check during Phase 6 verify, not an automated perf test).

## Appetite
Medium (1–2 weeks) — two additive epics (native-compaction parsing; multi-session aggregation + dashboard route/page) on top of already-existing single-session plumbing (`cost_compare.rs`, `discovery.rs`, `server.rs`), no new external dependencies expected (vanilla JS, no new datastore).

## Constraints
- No new persistent datastore — this reads `.jsonl` files directly from disk on demand (or cached in-memory within the running `serve-cost` process), matching the existing `CostServerState`/`CostTracker` in-process-only pattern. Phase 2 research must confirm whether an in-memory scan cache (with manual/periodic refresh) is needed for the 1,135-file scale, or whether a synchronous per-request scan is fast enough.
- Must reuse `discovery.rs`'s session-listing logic rather than re-implementing a glob walk.
- Must reuse `PricingTable`/`TiktokenEstimator` for cost estimation rather than inventing a second pricing path.
- Dashboard is server-rendered static HTML + vanilla JS fetching one JSON endpoint from the same process — no separate build step, no new frontend framework/dependency, per the user's explicit interface decision (local web dashboard, reusing the running `serve-cost` process; not a static export, not a TUI).
- Must not change `is_compacted()`'s existing consolette-only semantics in a way that breaks its current callers (`cost_compare.rs`, session_compaction pipeline) — native-compaction detection must be additive (new function(s)/struct(s)), not a behavior change to the existing function.

## Non-functional Requirements
- **Performance SLO**: no hard p99 target; "renders without hanging the browser tab" for ~1,135 real session files is the working bar (see Success Metrics). Phase 2 research should sanity-check parse time for that file count.
- **Scalability**: same order of magnitude as the operator's own `~/.claude/projects/` tree today; no requirement to scale beyond a single operator's local session history.
- **Security classification**: internal/operator-only tooling; `serve-cost` is already loopback-only with no auth — the new routes inherit that posture, no new external attack surface.
- **Data residency**: not applicable — purely local filesystem reads, no new data leaves the process.

## Scope
### In Scope
- A native-compaction parser (in `boundary.rs` or a new sibling module) that recognizes `"type":"system","subtype":"compact_boundary"` rows carrying `compactMetadata` and extracts a typed struct (trigger, pre/post tokens, tokens saved, duration, cumulative dropped tokens) distinct from consolette's own `CompactionMetrics` — a session can have native compaction, consolette compaction, both, or neither, and the two must be reportable independently.
- Cost estimation for native-compaction events via the existing `PricingTable`, so native compaction's dollar impact is comparable to consolette's.
- Extending `cost_compare.rs` (or a new aggregation module) so a per-session comparison struct carries both native and consolette metrics side by side.
- A new multi-session aggregation path that walks `discovery.rs`'s session list (or a superset of it, since native-compaction data needs the whole `~/.claude/projects/` tree, not just files consolette itself has touched) and produces one comparison row per session.
- A new JSON route on the existing `serve-cost` axum server exposing that aggregated list.
- A minimal static HTML/JS page served by the same process, fetching that JSON route, rendering a sortable (click-column-header), filterable (project/path substring, compaction-status) table.

### Out of Scope
- Any change to consolette's actual compaction *decision-making* (tier thresholds, `SessionCompactionPipeline::apply` behavior) — this is read-only reporting, not a policy change.
- Any change to Claude Code's own native auto-compaction behavior — consolette only reads its markers, never triggers or influences them.
- A general-purpose charting/BI framework, live-updating dashboard (websockets/polling), authentication, or multi-user access — single operator, single static-per-request fetch is sufficient.
- Persisting aggregated results to a database or file — recomputed from the transcripts on demand (subject to Phase 2's caching-strategy finding).
- Historical trending over time (e.g. "cost saved per week") — this release is a point-in-time snapshot table, not a time series.

## Rabbit Holes
- **`compactMetadata` shape may vary across Claude Code versions.** The shape was confirmed by direct inspection of real transcripts, but Claude Code is not consolette's own project — field presence/naming could differ across CLI versions. The parser must treat every `compactMetadata` field as optional/best-effort (never panic or hard-fail a whole session's aggregation because one row's shape is unexpected) and skip-and-log rather than abort.
- **Scanning 1,135+ files per request could be slow enough to matter.** Needs a Phase 2 timing check; if slow, an in-memory cache with explicit refresh (button/endpoint) is the likely mitigation — must stay in the "no new datastore" constraint (in-memory only, rebuilt on process restart).
- **Chain-coverage / multi-root transcripts** (already a known wrinkle in `cost_compare.rs`'s `ChainCoverage`) apply per-session here too; the aggregate table must surface coverage rather than silently presenting partial-chain numbers as if they were complete.
- **Sessions outside consolette's own touch** (i.e., never run through consolette's own compaction) still need to appear in the table with "native compaction: yes/no" — the aggregation must not implicitly filter to consolette-known sessions only, since the whole point is comparing consolette vs. native across *all* sessions.

## Alternatives Considered
- A separate static HTML export/CLI report generator instead of a live dashboard route — rejected per the user's explicit interface decision (extend the existing `serve-cost` server, not a static export).
- A TUI-based session browser — rejected per the user's explicit interface decision.
- A full frontend framework (React/etc.) for the table — rejected as unnecessary weight for a sortable/filterable table; vanilla JS keeps this dependency-light per repo convention.

## Feasibility Risks
- `compactMetadata` field-shape drift across Claude Code CLI versions (see Rabbit Holes) — mitigated by best-effort/optional parsing.
- Scan performance at 1,135+ files is unverified until Phase 2 measures it against the real corpus (read-only, non-destructive).
- Native compaction's `preTokens`/`postTokens` are Claude Code's own accounting, not necessarily using the same tokenizer/estimation approach as consolette's `TiktokenEstimator` — the comparison must clearly label which numbers are "native-reported" vs. "consolette-estimated" so the operator doesn't mistake one for the other's precision (same "estimated vs. exact" labeling pattern already used in `CostReport`'s `pricing_source`/`counterfactual_source` fields).

## Observability Requirements
Standard request logging is not sufficient since this is itself an observability feature. In scope:
- A log line when a session file fails to parse or yields unexpected `compactMetadata` shape (skip-and-log, not abort — see Rabbit Holes).
- The aggregation endpoint's response should include, per session, which fields are native-reported (exact) vs. consolette-estimated so the dashboard can render a confidence indicator per column, mirroring the existing `CostReport` exact/estimated pattern.

## Risk Control
Low risk / additive only — new read-only parsing, a new read-only aggregation module, and new read-only routes/page on an already loopback-only, unauthenticated local server. No feature flag needed; rollback is reverting the new module/routes. Does not touch `SessionCompactionPipeline::apply` or any existing route's behavior.

## Open Questions
- Exact aggregation endpoint path/naming (e.g. `/v1/cost/sessions` vs. a new `/v1/dashboard/...` namespace) — Phase 3 planning to decide, consistent with the existing `/v1/cost/{session_key}` convention.
- Whether the scan result should be cached in-memory with a manual refresh affordance (dashboard button hitting a refresh endpoint) or recomputed per request — Phase 2 research to confirm via a timing check against the real `~/.claude/projects/` tree.
- Whether "session discovery" for this feature should be the full `~/.claude/projects/**/*.jsonl` glob (matching where native-compaction data actually lives) rather than `discovery.rs`'s current default, and whether `discovery.rs` needs a new function or just direct reuse of `discover_sessions`/`discover_sessions_glob`.
