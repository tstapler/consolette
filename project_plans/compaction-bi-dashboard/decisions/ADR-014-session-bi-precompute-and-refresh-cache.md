# ADR-014: Precompute-at-startup + periodic background refresh for the session BI aggregation

**Status**: Accepted
**Date**: 2026-08-19
**Related**: `project_plans/compaction-bi-dashboard/requirements.md` (Constraints, Rabbit Holes), `project_plans/compaction-bi-dashboard/research/pitfalls.md`, `project_plans/compaction-bi-dashboard/research/architecture.md`

## Context

`GET /v1/dashboard/sessions` must return one comparison row per session across the operator's whole `~/.claude/projects/**/*.jsonl` tree — measured at 7,840 files on the live machine (`project_plans/compaction-bi-dashboard/research/pitfalls.md`, superseding requirements.md's stale 1,135 figure). Producing one row requires `discovery::discover_sessions_glob` (synchronous `glob`+`fs::metadata`) followed by `cost_compare::compare_compaction_cost` (synchronous JSONL parse + turn reconstruction + async per-turn `TiktokenEstimator` calls) per file. `cost_compare.rs:56-94`'s existing single-session caller already runs this inline inside an `async fn`; doing it 7,840 times inline inside an axum request handler would starve the runtime for the request's whole duration.

## Decision

Compute one `SessionBiSnapshot` (Domain Glossary) at `CostServerState::build()` time and hold it behind a `tokio::sync::watch::channel<Arc<SessionBiSnapshot>>`, mirroring `spawn_pricing_refresh_task` (`src/cost_metrics/pricing.rs:221-237`) already wired into `CostServerState` (`src/cost_metrics/server.rs:75-96`). A second background task (`session_bi::spawn_session_bi_refresh_task`) re-scans on a fixed interval and `tx.send_replace`s a new snapshot. The HTTP handler only ever calls `rx.borrow().clone()` — an `Arc` clone, never a scan.

Unlike `spawn_pricing_refresh_task` (which deliberately skips its first tick and serves the static fallback table until the first live fetch, since pricing rarely changes and a stale static table is an acceptable default), the session BI task must produce its first real snapshot synchronously during `CostServerState::build()`, before the server starts accepting connections — this feature's whole point is showing real per-session data, so there is no acceptable "empty" default to serve in the interim.

The per-file scan itself is bounded via `futures_util::stream::iter(..).map(..).buffer_unordered(N)` (a fixed concurrency, not one unbounded task per file, since fan-out over `build_turns`' full in-memory turn reconstruction could otherwise multiply peak memory across thousands of concurrent files) and a per-file `tokio::time::timeout` so one pathological transcript can't stall the whole scan.

## Alternatives Considered

1. **Synchronous per-request scan.** Simplest code, always fresh. Rejected: starves the axum runtime for the scan's full duration on every request against a corpus this pitfalls.md explicitly measured and flagged.
2. **SQLite-backed materialized cache persisted to disk, refreshed by an external cron/scheduled job.** Survives process restart; would also support a future historical-trending feature. Rejected: violates requirements.md's explicit "no new persistent datastore" constraint, and historical trending is explicitly out of scope for this release.
3. **Manual-refresh-only (button/endpoint), no periodic background task.** Simpler task lifecycle. Rejected in favor of periodic + precompute: the operator would otherwise have to know to hit a refresh endpoint before every dashboard visit; a periodic background refresh (interval chosen short enough — 15 minutes — that staleness during active use is minor, long enough that repeated 7,840-file scans aren't wasteful) needs no operator action. A manual-refresh path can be added later without touching this decision.

## Consequences

- Dashboard data can be up to one refresh interval (15 minutes) stale; the JSON response's `generated_at` timestamp lets the page make that visible rather than implying live data.
- `CostServerState` gains a second background `JoinHandle` (`_session_bi_refresh`) held only to keep the task alive for the process's lifetime, matching `_pricing_refresh`'s existing pattern.
- Startup time increases by however long the first full scan takes (unmeasured until Phase 6 verify against the real corpus) — acceptable per requirements.md's non-functional bar ("renders without hanging the browser tab", not a startup-latency SLO).
