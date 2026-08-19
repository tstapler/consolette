# ADR-015: Vanilla JS, `include_str!`-inlined HTML for the session BI dashboard

**Status**: Accepted
**Date**: 2026-08-19
**Related**: `project_plans/compaction-bi-dashboard/research/stack.md`, `project_plans/compaction-bi-dashboard/research/build-vs-buy.md`, `project_plans/compaction-bi-dashboard/research/ux.md`

## Context

The dashboard needs one sortable, filterable table rendered from a single JSON array (7,840-row worst case), served by the existing loopback-only, unauthenticated `consolette serve-cost` process. `src/dashboard.rs` (the pre-existing `GET /dashboard` proxy dashboard) is architectural precedent for `Html(const_str)` routes in this codebase but loads Chart.js from a CDN (`src/dashboard.rs:20`) — a network dependency this feature must not repeat, since it would be the first outbound request this offline-capable, loopback-only tool ever makes.

## Decision

Write the dashboard as one static HTML file with inlined `<style>`/`<script>` (vanilla JS, no framework, no build step), stored at `src/cost_metrics/dashboard.html` and compiled in via `include_str!("dashboard.html")` — the same pattern `src/cost_metrics/pricing.rs:27` already uses for `pricing_default.json`. Client-side sort/filter operates on the one fetched JSON array in memory (per `research/ux.md`); no server-side pagination, no websockets, no polling.

## Alternatives Considered

1. **A third-party table library (e.g. a minified DataTables/Grid.js bundle).** Less hand-written JS. Rejected: no CDN loading is acceptable (see Context), and vendoring a minified third-party blob into this single-crate, dependency-light repo has no update/audit story (`research/build-vs-buy.md`).
2. **A full frontend framework (React/Vue) with its own build step.** More structure for a large table. Rejected: unnecessary weight for one table; introduces a build pipeline this repo has never needed, and the explicit user interface decision (requirements.md's Alternatives Considered) already ruled this out.
3. **Serve the HTML/JS/CSS as separate static files via `tower-http`'s `ServeDir`.** Cleaner separation of concerns for a larger app. Rejected: pulls in a new dependency (`tower-http`) for what is, at this size, a single file — `include_str!` needs nothing new in `Cargo.toml`.

## Consequences

- The dashboard HTML/CSS/JS all live in one file (`src/cost_metrics/dashboard.html`), which will grow long as sort/filter/state-handling logic accretes; this is an accepted tradeoff for zero build-step and zero new dependencies, matching this repo's existing `src/dashboard.rs` precedent.
- Any future enhancement requiring real interactivity beyond client-side sort/filter (e.g. live updates) would need this ADR revisited alongside the "no websockets/polling" scope boundary in requirements.md.
