# Stack Research: context-analyzer

Agent 1 (Stack), SDD Phase 2. Scope: Rust crate/pattern choices for the
context-analyzer feature (persistent storage, dashboard charting, Codex CLI
log parsing, and current dependency versions).

## 1. Persistent storage: SQLite is already a dependency — use it, don't add sqlx

**Finding**: `rusqlite = { version = "0.40", features = ["bundled"] }` is
already a direct dependency in
[`Cargo.toml`](../../../Cargo.toml) (root deps block, not test-only). It is
already used for exactly this kind of "structured local persistent store"
job in three places:

- `src/bin/cmdcrush/metrics_store.rs` — `Mutex<Connection>`, opens with
  `conn.pragma_update(None, "journal_mode", "WAL")`, `CREATE TABLE IF NOT
  EXISTS otel_metrics (...)`.
- `src/claude_code_session/omission_cache.rs` — same `Mutex<Connection>` +
  `CREATE TABLE IF NOT EXISTS omitted_content (...)` shape, with a
  count-then-insert sequence wrapped in a transaction.
- `src/claude_code_session/prune.rs` — opens the same cache DB directly in
  tests (`rusqlite::Connection::open(&cache_path)`).

**Recommendation**: extend this exact pattern — a new `context_analyzer::store`
module (or extend `cost_metrics::store`, see open question below) backed by
`rusqlite` with `journal_mode=WAL`, one `Mutex<Connection>` (or a small
connection-per-call pattern if concurrent writers become a problem — none of
the three existing call sites need a pool). **Do not add `sqlx`, `sea-orm`,
or a second DB dependency.** `rusqlite`'s `bundled` feature statically links
SQLite, so there's no new system dependency either — consistent with "local
disk only, no cloud upload."

**Store-in-memory vs. persistent tension**: `cost_metrics::store::SessionCostStore`
(`src/cost_metrics/store.rs`) is explicitly in-memory — a `moka::future::Cache`
with a 1-hour TTL and a bounded per-session ring buffer (`MAX_RECORDS_PER_SESSION
= 200`), designed to be safe to lose on restart. The context-analyzer feature's
requirements explicitly need durability across restarts (cross-session trends,
a sortable session table spanning all history) — the moka/in-memory shape is
the wrong fit for that data, even though its *code style* (atomic
`get_or_init`, bounded rings) is worth mirroring for any request-scoped
in-flight state. This confirms the requirements doc's framing of "extend
cost_metrics::store vs. new store" as a real open question, not a false
dichotomy: **the durable session/call/turn/hook-event/subagent tables need
`rusqlite`; short-lived in-flight session state can keep using the existing
`moka`-backed pattern if it's still useful during ingestion.**

**Version**: `rusqlite` 0.40.2 is current on crates.io as of this research
(checked via the crates.io API) — the pinned `"0.40"` in `Cargo.toml` is
already up to date; no version bump needed.

## 2. Dashboard charting: two existing patterns, only one is offline-safe

There are **two** dashboard implementations in the repo today, with
divergent charting approaches:

- **`src/dashboard.rs`** (`DASHBOARD_HTML` served at whatever route wires it
  up, ported from `stapler-scripts/claude-proxy/main.py`) — loads
  **Chart.js 4.4.0 from a CDN**:
  `<script src="https://cdn.jsdelivr.net/npm/chart.js@4.4.0/dist/chart.umd.min.js"></script>`.
  This is a **CDN reference**, not offline-friendly, and directly violates
  this feature's "must stay local" NFR if reused as-is.
- **`src/cost_metrics/dashboard.html`** (served via `include_str!` in
  `src/cost_metrics/server.rs:45`, `Html(DASHBOARD_HTML)` at `GET
  /dashboard`) — **no charting library at all**. It's a single self-contained
  file: inline `<style>`, one `<script>` block of vanilla ES5-ish JS (no
  build step, no bundler, no external `<script src>` of any kind) that
  fetches `/v1/dashboard/sessions`, and renders a sortable/filterable
  `<table>`. Zero external dependencies, zero CDN references — this is the
  offline-safe precedent.

**Recommendation**: follow the `cost_metrics/dashboard.html` pattern
(self-contained HTML+CSS+JS, `include_str!`, served via `axum::response::Html`),
**not** the `dashboard.rs`/CDN-Chart.js pattern. For the feature's actual
charting needs (context-growth-per-turn line chart with threshold lines,
cache-read churn chart, composition breakdown), two options, in order of
preference:

1. **Vendor Chart.js locally** (download the UMD build once, inline it as a
   `const CHART_JS_SRC: &str = include_str!("vendor/chart.umd.min.js");`
   injected into a `<script>` tag, or embed it as a `data:` URI) — keeps the
   full Chart.js feature set (line/bar charts, tooltips, axis scaling, the
   threshold-line annotation need) without a network fetch. This is the
   closest to what `dashboard.rs` already assumes Chart.js can do, just made
   offline-safe.
2. **Hand-rolled inline SVG**, generated either server-side (Rust builds
   `<svg>` markup directly, zero JS) or client-side (small vanilla-JS
   function that draws `<path>`/`<rect>` elements) — more code to write for
   axes/legends/tooltips, but zero vendored dependency to keep in sync and
   trivially themeable (matches the existing `prefers-color-scheme`
   light/dark CSS variables pattern already used in
   `cost_metrics/dashboard.html`).

Given the feature's chart needs are a small, fixed set (line chart with
threshold/autocompact reference lines, a churn chart, a composition
breakdown — likely a stacked bar or donut), **hand-rolled inline SVG is
probably the better fit** for a solo/personal tool: it avoids maintaining a
vendored third-party JS blob, matches the zero-external-script precedent
`cost_metrics/dashboard.html` already established, and the chart types
needed (line + stacked bar) are straightforward to hand-draw. Vendoring
Chart.js is the fallback if the SVG hand-rolling proves more work than
expected once the actual chart specs are in `plan.md`. Either way: **no new
Rust charting crate** — this is server-rendered-HTML-plus-JS/SVG territory,
not a Rust-side plotting library (e.g. `plotters`) rendering images, which
would be a worse fit for an interactive, filterable web dashboard and isn't
a dependency anywhere in this repo today.

## 3. Codex CLI rollout log parsing: bespoke JSON parsing, no usable crate

**Storage location and naming** (confirms the requirements doc's assumption):
`~/.codex/sessions/YYYY/MM/DD/rollout-<TIMESTAMP>-<UUID>.jsonl` — date-hierarchical
directories, one file per session. Cold files may be Zstandard-compressed.
Source: [openai/codex discussion #3827](https://github.com/openai/codex/discussions/3827),
[DeepWiki: Rollout Persistence and Replay](https://deepwiki.com/openai/codex/3.5.2-rollout-persistence-and-replay).

**Format**: JSONL, one `RolloutLine` per line — `{timestamp, ordinal?, item:
RolloutItem}`, where `RolloutItem` is a serde-tagged enum with (at least)
eight variants: `ResponseItem` (model responses/tool calls), `EventMsg`
(protocol events — `UserMessage`, `TokenCount`, `ThreadGoalUpdated`, etc.),
`SessionMeta` (id, source, cwd, model_provider, cli_version), `TurnContext`
(model/approval-policy/sandbox-policy snapshot), `Compacted`
(history-compaction summaries), `InterAgentCommunication`, `WorldState`.
Source: [DeepWiki rollout page](https://deepwiki.com/openai/codex/3.5.2-rollout-persistence-and-replay)
citing `codex-rs/rollout/src/recorder.rs:57-66`.

**Token usage**: present, via `EventMsg::TokenCount` carrying a `TokenUsage`
struct with `input_tokens`, `cached_input_tokens`, `output_tokens`,
`reasoning_output_tokens`, `total_tokens`. Source:
[issue #14489](https://github.com/openai/codex/issues/14489),
[commit 0269096](https://github.com/openai/codex/commit/0269096229e8c8bd95185173706807dc10838c7a).
**Caveat**: [issue #32479](https://github.com/openai/codex/issues/32479) —
as of GPT-5.6, `cache_write_tokens` is silently dropped from `TokenUsage`/the
rollout JSONL because of a serde unknown-field bug; cache-write accounting
from Codex logs is presently unreliable.

**No reusable Rust crate — verified two candidates and ruled both out**:

1. `codex-rollout` — the actual in-repo crate name for
   `codex-rs/rollout` (confirmed by reading its `Cargo.toml`: `name =
   "codex-rollout"`, lib name `codex_rollout`) — **is not published to
   crates.io** (`cargo info codex-rollout` / crates.io API returns "crate
   `codex-rollout` does not exist", checked directly). It only exists as an
   internal workspace member of the ~130-crate `codex-rs` monorepo.
2. `codex-protocol` **is** published on crates.io (0.63.0, latest as of this
   research) and its description ("Protocol definitions for Codex AI agent")
   sounds like a plausible fit — **but its crates.io `repository` field
   points to `https://github.com/namastexlabs/codex`, a third-party fork, not
   `openai/codex`.** This is not an official OpenAI-published crate; treat it
   as unverified/unofficial provenance and do not add it as a dependency
   without separately vetting that fork. Its docs.rs top-level modules
   (`account, approvals, config_types, custom_prompts, items,
   message_history, models, num_format, parse_command, plan_tool, protocol,
   user_input`) also don't obviously surface `RolloutItem`/`RolloutLine`/
   `TokenUsage` — would need a source read of that fork's `protocol` module
   to confirm even if provenance were acceptable.

**Recommendation**: bespoke `serde`/`serde_json` parsing for Codex rollout
logs, mirroring the existing `claude_code_session/transcript.rs` approach
(tagged-enum-by-hand with a catch-all `extra: Map<String, Value>` for
forward-compat against schema drift, rather than a strict `#[serde(tag =
"type")]` derive that would hard-fail on new/renamed fields). No new crate
needed — `serde`, `serde_json`, and `chrono` are all already direct
dependencies. Given the documented schema drift (the wire format already
changed shape once, per DeepWiki, from bare untagged JSON to the current
tagged `RolloutLine` wrapper; pre-1.0 versioning on the only related crate),
budget for **log-and-skip on malformed/unrecognized lines** (already a
stated observability requirement in requirements.md) rather than a
crash-on-parse-error strategy, and expect to revisit the parser across Codex
CLI upgrades.

## 4. Dependency version summary

| Crate | In `Cargo.toml` today | Current on crates.io | Action |
|---|---|---|---|
| `rusqlite` | `0.40` (`bundled` feature) | 0.40.2 | none — reuse as-is |
| `tiktoken-rs` | `0.5` | 0.12.0 | **not** in scope for this feature (used today only by `cost_compare.rs`'s estimator path); flagging the drift for awareness, not proposing a bump here |
| new charting dependency | none | — | none recommended (see §2) — hand-rolled SVG or vendored Chart.js, not a new crate |
| new DB dependency | none | — | none recommended (see §1) — `rusqlite` already present |
| Codex rollout-log parser | none | — | see §3 |

### Note on exact API token usage extraction (Claude Code side)

The requirements call for "exact API token usage (input/output/cache_read/
cache_creation)" from Claude Code transcripts. Checked
`src/claude_code_session/transcript.rs`'s `RowFields`: the `message` field is
untyped (`pub message: Option<Value>`), and the existing
`src/claude_code_session/cost_compare.rs` computes token counts via
`TiktokenEstimator` (an *estimate*, tokenizing text with `tiktoken-rs`) —
**not** by reading the transcript's actual `message.usage.{input_tokens,
output_tokens,cache_creation_input_tokens,cache_read_input_tokens}` sub-object
(the Anthropic Messages API usage shape, which Claude Code transcripts do
carry on assistant rows). Extracting the real usage numbers needs new parsing
code, but **no new crate** — `serde_json::Value` (already in scope) is
sufficient to pull `row.fields().message` apart; this is a plan/architecture
task, not a dependency question, but worth flagging since it means the
existing `cost_compare.rs` estimator path is not a shortcut to "exact" usage
despite superficially looking related.
