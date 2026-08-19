# Stack Research: compaction-bi-dashboard

## 1. Relevant existing `Cargo.toml` dependencies

All already present as direct dependencies — no new crate is needed for this feature:

| Concern | Crate (version) | Notes |
|---|---|---|
| HTTP server / routing | `axum = "0.8"` (features: `macros`) | Already used by `src/cost_metrics/server.rs`'s `serve_cost`. |
| Extra axum helpers | `axum-extra = "0.10"` (feature: `typed-header`) | Not needed for this feature (no typed headers involved). |
| Tower middleware | `tower = "0.5"`, `tower-http = "0.7"` (features: `trace`, `timeout`) | **No `fs` feature enabled** — `tower_http::services::ServeDir/ServeFile` is unavailable as-is, but not needed (see §2). |
| JSON (de)serialization | `serde = "1"` (derive), `serde_json = "1"` | Used throughout `claude_code_session` for `TranscriptRow` / metadata. |
| Filesystem globbing | `glob = "0.3"` | Already used by `discovery.rs::discover_sessions_glob`. |
| Async runtime | `tokio = "1"` (full) | Server + async file scanning. |
| HTTP client (unrelated to this feature) | `reqwest = "0.12"` | Used for the LiteLLM pricing endpoint; not needed here since this feature is same-process/local only. |
| Cost estimation | `tiktoken-rs = "0.5"` via `cost_metrics::estimator::TiktokenEstimator`, plus `cost_metrics::pricing::PricingTable` | Reuse directly per the requirements' constraint ("must reuse `PricingTable`/`TiktokenEstimator`"). |
| Logging | `tracing = "0.1"` | Skip-and-log parse failures per the requirements' observability section. |

**Conclusion**: the requirements' assumption ("no new external dependencies expected") is confirmed — everything needed (routing, JSON, globbing, async, logging) is already a direct dependency.

## 2. Serving static HTML + inline vanilla JS from an axum route

Confirmed idiomatic and already done in-repo, no new crate needed:

- `src/dashboard.rs` is a **direct, working precedent** for exactly this pattern: a `const DASHBOARD_HTML: &str = r#"<!DOCTYPE html>...</html>"#;` (lines 14–613), served by:
  ```rust
  use axum::response::{Html, IntoResponse};
  pub async fn handle_dashboard() -> impl IntoResponse {
      Html(DASHBOARD_HTML)
  }
  ```
  (`src/dashboard.rs:615-621`). `axum::response::Html` sets `Content-Type: text/html` automatically and wraps any `Into<String>`/`&'static str`. This requires no additional crate — `Html` ships in axum core, already a dependency.
- One caveat found: `src/dashboard.rs`'s existing HTML loads Chart.js from a CDN (`<script src="https://cdn.jsdelivr.net/...">`, line 20) — that's an *external network* dependency at render time, not a Rust crate dependency, but it conflicts with the "vanilla JS, no new dependency" spirit of this feature's requirements. The new compaction dashboard page should **not** copy that CDN pattern — inline `<script>...</script>` JS only (sort/filter logic is simple enough not to need Chart.js or any library), keeping the page fully self-contained.
- `include_str!` is an equally idiomatic alternative to an inline raw-string literal if the HTML is large enough to warrant its own `.html` file (e.g. `src/cost_metrics/dashboard.html`) — `pricing.rs:27` already uses `include_str!` for a JSON asset (`PRICING_DEFAULT_JSON`), establishing that pattern is acceptable in this repo too. Either approach (inline `r#"..."#` const like `dashboard.rs`, or `include_str!("dashboard.html")` like `pricing.rs`) is consistent with existing conventions; `include_str!` is preferable here since the combined HTML+CSS+JS for a sortable/filterable table will likely be larger and more awkward to review as an inline Rust string literal.
- No `tower-http` `ServeDir`/`ServeFile` (which would need the currently-absent `fs` feature) is required, since there's exactly one static page, not a directory of assets.

## 3. Parsing/streaming ~1,135 JSONL files

The repo already has one canonical JSONL-streaming function to reuse as-is — no new pattern or crate needed:

- **`transcript.rs::parse_session_file`** (`src/claude_code_session/transcript.rs:158-186`): opens the file, wraps in `BufReader`, iterates `reader.lines().enumerate()`, trims/skips blank lines, and does `serde_json::from_str::<TranscriptRow>(trimmed)` per line — a parse failure on one line is `tracing::warn!`-logged with the 1-indexed line number and **skipped**, not fatal to the whole file. This exactly matches the requirements' "skip-and-log rather than abort" rabbit-hole guidance (requirements.md line 58) and should be reused (or the native-`compact_boundary` parser should follow the identical per-line skip/log discipline) rather than re-implemented.
- **`discovery.rs::discover_sessions_glob`** (`src/claude_code_session/discovery.rs:70-93`) already walks `glob(pattern)` and builds `SessionFile` entries (path, mtime, size), matching the requirements' "must reuse `discovery.rs`'s session-listing logic" constraint. `discover_sessions` (line 55) hardcodes the default `~/.claude/projects/**/*.jsonl` pattern via `discover_sessions_glob`, confirming that this feature's "whole tree" scan is exactly `discover_sessions_glob`'s intended use, just possibly with `SortBy` irrelevant for aggregation purposes (any variant works — the aggregation route would re-sort/filter client-side in JS per the requirements).
- No additional streaming/JSONL crate (e.g. `serde_jsonlines`) is warranted: the existing hand-rolled `BufReader::lines()` + `serde_json::from_str` loop is simple, already proven at this repo's scale, and adding a crate for it would violate the "no new dependency" constraint for no benefit.
- **Performance**: 1,135 files was not independently timed in this research pass (that's explicitly called out in requirements.md as a Phase 2/6 verification task, not a stack-selection question) — but the existing per-line-streaming approach bounds memory to one line at a time per file, and `tokio::task::spawn_blocking` (or scanning inside a blocking-safe async handler) is the standard axum idiom if synchronous multi-file I/O inside a request handler risks blocking the async runtime's worker threads; `tokio = { features = ["full"] }` already provides `spawn_blocking`, no new dependency required for that either.

## 4. Community-recommended current versions (validating "no new dependency" assumption)

No new dependency is needed, so no new version needs pinning. For completeness, the versions already pinned in `Cargo.toml` are current/non-stale as of this research:

- `axum = "0.8"` — current major version line (0.8.x is axum's latest stable major as of this research; no breaking-change motivation to bump mid-feature).
- `tower-http = "0.7"` — current; the unused `fs` feature (for `ServeDir`) is available in this same version if ever needed later, so no version bump would even be required if the team later wants a directory of static assets instead of one inline page.
- `serde_json = "1"`, `glob = "0.3"`, `tracing = "0.1"` — all stable, unversioned-breaking crates already widely current across the Rust ecosystem; no action needed.

**Overall confirmation**: the requirements' assumption of "no new external dependencies expected" holds. This feature is implementable entirely with `axum::response::Html` (or `include_str!` + `Html`), `serde`/`serde_json`, `glob` (via `discovery.rs`), and existing `cost_metrics::{pricing::PricingTable, estimator::TiktokenEstimator}` — all already direct dependencies.

## Key files referenced
- `/Users/tstapler/code/github.com/tstapler/consolette/Cargo.toml`
- `/Users/tstapler/code/github.com/tstapler/consolette/src/dashboard.rs` (lines 1-20, 600-621) — static-HTML-route precedent
- `/Users/tstapler/code/github.com/tstapler/consolette/src/claude_code_session/transcript.rs` (lines 135-186) — JSONL streaming/skip-log precedent
- `/Users/tstapler/code/github.com/tstapler/consolette/src/claude_code_session/discovery.rs` (lines 55-93) — session glob discovery to reuse
- `/Users/tstapler/code/github.com/tstapler/consolette/src/claude_code_session/boundary.rs` (lines 1-60, 104-190) — existing consolette-only compaction marker parsing, to be extended additively
- `/Users/tstapler/code/github.com/tstapler/consolette/src/cost_metrics/server.rs` (lines 1-136) — existing `serve_cost`/`cost_router`/`CostServerState` to extend with a new route
- `/Users/tstapler/code/github.com/tstapler/consolette/src/cost_metrics/pricing.rs:27` — `include_str!` precedent for embedding a static asset
