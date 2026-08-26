# Build vs. Buy: context-analyzer

**Agent**: 6 (Build vs. Buy), Phase 2 research
**Date**: 2026-08-24

## 1. Transcript ingestion: library vs. bespoke parsing

### Existing consolette code
`src/claude_code_session/transcript.rs` already hand-rolls a `TranscriptRow` parser for
`~/.claude/projects/**/*.jsonl` with `serde_json` — deliberately round-tripping every
unrecognized field via a `Map<String, Value>` `extra` flatten, because Claude Code's schema
is undocumented and evolves. `discovery.rs` walks the project directories, `session_bi.rs`
does BI aggregation, `native_compaction.rs` parses compaction/summary events. This is ~5100
lines built for a different purpose (offline compaction), but the row-shape parsing and
directory discovery are directly reusable.

### Candidate crates (checked via crates.io API, 2026-08-24)

| Crate | Downloads | Versions | Last publish | License | Notes |
|---|---|---|---|---|---|
| [`claude-code-transcripts`](https://crates.io/crates/claude-code-transcripts) | 5,559 total / 5,084 recent | 9 | 2026-05-05 | MIT OR Apache-2.0 | Typed `Entry` enum for transcript rows; single-maintainer ([alfredvc/cct](https://github.com/alfredvc/cct)), ~4 months stale relative to today, low adoption |
| [`toolpath-claude`](https://crates.io/crates/toolpath-claude) | 1,550 total / 891 recent | 16 | 2026-07-30 | Apache-2.0 | Parses Claude conversation logs into **provenance/audit** documents ([empathic/toolpath](https://github.com/empathic/toolpath)) — different problem (compliance trail, not token forensics), actively maintained |

Neither crate models Codex CLI's `~/.codex/sessions/` rollout format at all — that side needs
bespoke parsing regardless.

**Pros of adopting `claude-code-transcripts`**: saves writing the typed row enum; MIT/Apache-2.0
is compatible.
**Cons**: 5.5K lifetime downloads and one maintainer is a fragility risk for something this
feature depends on continuously (a schema-drift break stalls ingestion until upstream patches
or Tyler forks it); doesn't cover Codex; doesn't match consolette's established pattern in
`transcript.rs` of round-tripping *unknown* fields byte-faithfully (a hard requirement here
too, since Claude Code's JSONL schema isn't documented and a typed-enum-only parser silently
drops fields a future Claude Code version adds).

**Verdict: Not recommended.** Extend `src/claude_code_session/transcript.rs`'s existing
`serde_json`-based row parsing (and `discovery.rs`'s directory walk) rather than adding an
external transcript crate. It already solves the hard part (unknown-field round-tripping) that
neither external crate addresses, it's already yours to fix when the schema drifts, and pulling
in a 5.5K-download single-maintainer crate for a problem you've already solved in-repo adds a
supply-chain dependency without saving meaningful effort. Codex CLI ingestion (`~/.codex/sessions/`)
has no Rust crate candidate at all — write it as a new sibling parser module (`src/claude_code_session/codex_transcript.rs` or a new `codex_session/` module) following the same
round-tripping discipline.

## 2. Charting: hand-rolled JS vs. vendored JS lib vs. Rust-side rendering

### What consolette already does
`src/cost_metrics/dashboard.html` is a single self-contained HTML file with an inline
`<script>` — vanilla JS/DOM, no `<canvas>`, no SVG chart, no CDN script tag, no charting
library. It's currently a sortable/filterable table (`session_bi.rs` comparison view), not a
chart, but it establishes the project's convention: one static HTML file, zero external
script/CSS dependencies, served locally by `axum` (already a dependency) off `cost_metrics/server.rs`.

This feature needs actual charts context-analyzer doesn't need in table form: context-growth-
per-turn line chart with threshold bands, cache-read-churn bars, a composition donut, and a
cost-vs-context scatter for cross-session view.

### Options
- **Hand-rolled JS/Canvas/SVG** — matches the existing `dashboard.html` convention exactly, zero
  new dependencies (Rust or JS), no CDN/CSP concerns for an offline localhost tool. Cost: more
  upfront JS to write (axis scaling, band shading, tooltips) than `<script src=cdn>` would need,
  but budget-threshold bands, autocompact lines, and a scrubber/playback control (per the
  context-analyzer README's described UX) are all straightforward `<svg>` or `<canvas>` primitives
  — nothing here needs a charting library's feature set (no pan/zoom, no 3D, no animation library).
- **Vendored JS charting lib** (Chart.js, D3, etc., copied into the repo, not CDN-loaded) — saves
  implementation time for the scatter plot and donut specifically. Cost: a JS dependency with its
  own update/security lifecycle vendored into a Rust-only project, license file to track, and it's
  inconsistent with the zero-JS-dependency convention `dashboard.html` already set.
- **Rust-side rendering (`plotters` → SVG/PNG)** — would let the Rust backend own the whole chart
  as a static image response. Cost: charts here need *interactivity* (hover tooltips showing exact
  token counts, click-to-drill into a turn, budget-toggle buttons, scrubber/playback) — the
  context-analyzer README's own screenshots show hover-driven detail. `plotters` targets static
  chart generation; reproducing that interactivity would mean either re-rendering server-side on
  every hover (impractical for a local dashboard) or overlaying JS anyway, at which point `plotters`
  adds nothing hand-rolled SVG+JS doesn't already give for free client-side.

**Verdict: Recommended — hand-rolled JS/SVG, following `dashboard.html`'s existing pattern.**
No new dependency in either language, matches the file this feature is extending, and the actual
chart types needed (line with threshold bands, bar, donut, scatter) are all well within reach of
~200–400 lines of vanilla JS per view. Reserve `plotters` as a fallback only if a future
non-interactive export (e.g., a static PNG report artifact) is added — not for the live dashboard.

## 3. SaaS/managed API for Claude Code cost/context forensics

Checked: Anthropic Console's usage dashboards report organization/workspace-level API spend and
token counts, not per-call context composition, per-turn growth, or hook-level tool-call capture
inside a single Claude Code session — it has no visibility into local hook events, subagent
activity, or transcript content at all, since that data never leaves Tyler's machine in Console's
model. No third-party hosted service surfaced in search that ingests local Claude Code transcripts
for this kind of forensics; context-analyzer itself is the only tool doing this, and it's local/
self-hosted (FastAPI on localhost), not SaaS.

**Verdict: Not recommended / not applicable.** The requirement is explicitly local, personal,
no-cloud-upload (transcripts and hook events are local-only, sensitive tool I/O). No hosted
option fits that constraint even if one existed with matching functionality, and none was found
that has the functionality either.

## 4. Headroom / compression-ceiling audit: reimplement vs. shell out vs. defer

### What `headroom-ai` actually is
Checked PyPI's JSON API directly (2026-08-24): package `headroom-ai`
([chopratejas/headroom](https://github.com/chopratejas/headroom)), latest `0.36.5`
(context-analyzer's README pins `0.32.1`, i.e., it tracks a moving target), Apache-2.0 licensed,
pure-Python wheels (`py3-none-any` — no native/compiled extension), requires Python ≥3.10.
It's a general context-compression layer: field-level statistical analysis + Kneedle-algorithm
bigram-coverage subset selection for JSON, AST-aware signature-preserving compression for code,
pattern clustering for logs, plus a reversible-compression (CCR) cache-and-retrieve mechanism.
This is a materially complex, actively-changing algorithm surface (0.1x-scale version bumps,
per-content-type heuristics) — not a small formula to port.

### Options
- **Reimplement the algorithm in Rust from scratch.** High correctness risk: the Kneedle-algorithm
  JSON heuristic and AST-aware code compression are exactly the kind of "small implementation
  details compound into wrong numbers" logic the requirements' `Constraints` section is trying to
  avoid duplicating badly. Porting from the public README/docs alone (no code reuse allowed per
  Out-of-Scope) means guessing at thresholds a 36-version-deep upstream project already tuned.
  A Rust reimplementation would also immediately drift from upstream's own version churn (0.32.1
  → 0.36.5 in the time between context-analyzer's README and this research), with no way to track
  parity.
- **Shell out to the real `headroom-ai` Python package via subprocess.** Correctness: identical
  numbers to context-analyzer's own audit, by construction. Cost: introduces a Python runtime
  dependency (`pip install --no-deps headroom-ai==X && pip install tiktoken`, per the upstream
  README's own install command) into an explicitly Rust-only project — violates the stated
  constraint ("consolette is Rust-only — no existing Python runtime dependency in this project")
  outright, not just in spirit. It's also an *offline audit* feature per the requirements (run
  occasionally, not on a hot path), which weakens the case for the runtime cost of a shell-out but
  doesn't remove the dependency problem.
- **Defer this one view entirely to a follow-up.** All four other Scope items (hook capture,
  transcript ingestion, persistent store, and the four other dashboard views) stand on their own
  without headroom/compression-ceiling — it's the single most self-contained item in Scope, and
  the requirements already isolate it as a distinct bullet.

**Verdict: Recommended — defer to a follow-up project**, explicitly re-scoped as an ADR decision
rather than silently dropped. Neither remaining option is acceptable as specified: reimplementing
risks shipping wrong numbers under a "compression ceiling" claim (a number Tyler would use to
judge whether tool-output compaction is worth building elsewhere in consolette), and shelling out
directly contradicts the repo's Rust-only constraint. If Tyler wants this view sooner rather than
later, the follow-up should first re-litigate the constraint itself (a single, isolated,
occasionally-invoked Python subprocess call for one offline audit view is a much narrower ask than
"depend on Python" reads as at first) rather than defaulting to a bespoke Rust port.

## 5. Fork/adapt context-analyzer's own logic

context-analyzer ([manavgup/context-analyzer](https://github.com/manavgup/context-analyzer),
MIT) is architecturally close: Claude Code hooks → JSONL hook-event log + transcript JSONL →
SQLite (`sessions`, `api_calls`, `blocks`, `turns`, `hook_events`, `subagents`,
`subagent_api_calls`, `tool_result_offloads`) → dashboard/MCP server reading that DB. That maps
almost one-to-one onto this project's own scope (hook capture → ingestion → store → dashboard),
and consolette already has the SQLite piece proven out (`rusqlite` is an existing dependency,
used in `src/claude_code_session/omission_cache.rs` and `src/bin/cmdcrush/metrics_store.rs`).

The requirements explicitly bar depending on or porting context-analyzer's *code* or its SQLite
DB directly, but do not bar reading it — and per the Out-of-Scope note, it's MIT-licensed, so
reading-for-logic (not copying source) carries no licensing risk as long as the actual
implementation is an independent Rust rewrite. Three specific algorithms this project needs are
exactly the kind of business logic that's easy to get subtly wrong from a README description
alone:
- **Composition-breakdown categorization** (what counts as Tool I/O vs. Conversation vs. System
  prefix, and how it's attributed per API-reported token field) — the README states the *outcome*
  ("60%+ Tool I/O") but not the categorization rule.
- **Budget-threshold / danger-band logic** (200K/500K/700K/1M toggles, autocompact line placement)
  — needs to match Claude Code's actual autocompact behavior, not an invented threshold.
- **Cache-read churn calculation** (the 96–98% cache-hit-rate metric) — a specific formula over
  `cache_read` vs. `cache_creation` vs. total input tokens that's easy to get off-by-one-field on.

**Verdict: Recommended — `git clone` context-analyzer locally as a read-only reference for Phase
5 implementation**, scoped narrowly: read its Python source for these three specific algorithms
(and only these — not a general "browse the codebase" invite) at implementation time, write the
equivalent Rust logic independently, and cite the read (file + line, e.g. via a commit-pinned
GitHub link) in the implementing commit's message or a code comment, the same way this research
doc cites its own sources. This keeps the MIT provenance clean (logic, not code, ported) while
avoiding the correctness risk of guessing these three formulas from the README's prose alone. Do
not clone its SQLite DB or generated data — only source, and only for this narrow read.

## Summary table

| Decision | Verdict |
|---|---|
| Transcript ingestion library (`claude-code-transcripts` / `toolpath-claude`) | Not recommended — extend existing `transcript.rs` |
| Charting (hand-rolled vs. vendored JS vs. `plotters`) | Recommended — hand-rolled JS/SVG, matching `dashboard.html` |
| Hosted/SaaS forensics tool | Not applicable — no fit for local/no-cloud-upload constraint |
| Headroom audit: reimplement in Rust | Not recommended — correctness risk on an actively-changing algorithm |
| Headroom audit: shell out to Python `headroom-ai` | Not recommended — violates Rust-only constraint |
| Headroom audit: defer to follow-up | Recommended |
| Clone context-analyzer for reference during Phase 5 | Recommended — read-only, scoped to 3 named algorithms |
