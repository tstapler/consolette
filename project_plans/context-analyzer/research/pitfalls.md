# Pitfalls & Risks: context-analyzer

Research for Phase 2 (Agent 4). Scope: known failure modes for (a) parsing
live-appended agentic-CLI transcripts, (b) this repo's existing Rust
parsing/serde patterns, (c) mutating `~/.claude/settings.json`, (d)
token/cost accounting semantics, (e) scope control for a solo Large-appetite
effort.

## 1. Live JSONL transcript ingestion: partial writes, rotation, races

Claude Code appends to `~/.claude/projects/<project>/<session_id>.jsonl`
while a session is active; Codex CLI does the same under
`~/.codex/sessions/`. Both are being read *while* another process (the live
CLI) may still be writing.

- **Partial last line.** A reader that opens the file mid-append can see a
  truncated final line (fwrite flushed a partial buffer, or the reader's
  `read()` raced the writer's `write()`). `parse_session_file`
  ([`src/claude_code_session/transcript.rs:158`](../../../src/claude_code_session/transcript.rs#L158))
  already treats any unparseable line as log-and-skip via `tracing::warn!`
  rather than erroring the whole file — this is the right posture and
  should carry over unchanged. But today it also **permanently drops** that
  line: for an offline one-shot compaction tool that's fine (the file is
  static by the time it's read); for a *live* ingester re-reading the same
  growing file, a truncated line that was skipped once must be re-attempted
  on the next poll once the rest of the line lands, not skipped forever.
  This is a genuinely new requirement `parse_session_file` was not designed
  for — the existing "skip and warn" contract silently discards partial
  lines with no signal to retry.
- **Byte-offset resumption vs. re-parsing whole file.** For a live tail,
  ingesting the whole file on every poll is wasteful and risks re-counting
  cost/tokens for rows already ingested (see §4 double-counting). A
  byte-offset or line-count checkpoint per session file is needed, but the
  checkpoint must tolerate: the offset landing mid-line (back up to the
  last `\n`), and truncation making the previous offset invalid (rotation,
  see below) — recorded offset > current file size means "start over," not
  a panic/error.
- **File rotation / compaction rewrite.** `writer.rs`
  ([`src/claude_code_session/writer.rs`](../../../src/claude_code_session/writer.rs))
  already establishes that consolette itself *rewrites* a session file
  during compaction (new file with pruned/summarized content, same session
  id). Claude Code's own native auto-compact does something similar to the
  live transcript. An ingester holding a byte offset into "the file for
  session X" can find that file now shorter, or with different content at
  the same offset, after a compaction event — offset-based resume must
  detect this (e.g. via a monotonic row/uuid high-water-mark instead of raw
  byte offset, or by validating that the first N bytes at commit time still
  match) rather than blindly seeking.
- **Concurrent-write torn reads.** POSIX doesn't guarantee append() is
  atomic across arbitrary write sizes for pipes/regular files under
  concurrent readers past a page boundary in every case; in practice
  transcript lines are typically written as a single syscall so this is
  low-probability, but a reader must never assume the file it's streaming
  is closed/immutable — it should treat EOF as "no more data *right now*,"
  not "file complete," and requires a way to distinguish "no new session
  activity" from "actively catching up."
- **Compaction-in-progress transcripts.** A `PreCompact`/`PostCompact` hook
  event landing in the hook-events stream can race the transcript file
  itself being rewritten — the requirements explicitly call out capturing
  compaction events, so the ingester needs to correlate hook-stream events
  with transcript-file state rather than assume they arrive in a fixed
  order. `native_compaction.rs` already models `compact_boundary` rows
  found *after the fact* in a settled file; it has never been exercised
  against a file still being written to mid-compaction.
- **No test coverage for any of this today.** All three transcript-parsing
  modules' tests use fully-materialized fixture files
  (`NamedTempFile`/`write_temp_jsonl`, e.g.
  [`transcript.rs:473`](../../../src/claude_code_session/transcript.rs#L473)).
  There is zero existing coverage for "read while append is in progress" —
  this needs new test infrastructure (a background thread/task appending on
  a delay while the ingester polls) that doesn't exist anywhere in the repo
  yet.

## 2. Rust/serde stack risks (this repo's existing patterns)

- **`build_turns` returns `Err` on parent-chain cycles**
  ([`transcript.rs:266`](../../../src/claude_code_session/transcript.rs#L266))
  rather than tolerating them. A malformed or adversarially-edited
  transcript (or a bug in Claude Code itself) that produces a
  `parentUuid` cycle currently aborts turn reconstruction for the *whole
  file*, not just the affected rows. For an offline compaction tool that
  's an acceptable fail-closed default; for a dashboard whose whole
  premise is "always show me something," one bad session must not blank
  the entire cross-session view — the ingestion layer needs a
  per-session-file catch boundary, not a per-request one, or a single
  corrupt session will 500 the `/sessions` cross-session page.
- **Rows preceding the first genuine user turn are silently dropped**
  ([`push_row_into_turns`, transcript.rs:369-380](../../../src/claude_code_session/transcript.rs#L369-L380)),
  by explicit design ("rare in real transcripts and flagged in plan.md as
  an accepted gap"). For a compaction tool, dropping a stray leading system
  row is harmless. For a token-accounting dashboard, *any* dropped row with
  a non-zero `usage` block (a leading `system` row can carry cache-creation
  tokens from loading `CLAUDE.md`/system prompt) means undercounted totals
  — this accepted gap needs re-litigating for this feature, since the
  correctness bar is different ("account for every token," not "reconstruct
  readable turns").
- **`chain_coverage`'s ratio can silently be `<1.0` and nothing surfaces
  it** to a caller by default — `compare_compaction_cost` computes it but a
  UI consumer must remember to check and display it. Cross-session
  analytics/trend charts that sum "total tokens" per session without
  surfacing `chain_coverage.ratio()` will under-report multi-root sessions
  (common after `--resume`/`--clear`) with no visible warning — an easy
  silent-data-loss bug to reintroduce in a new store/aggregation layer that
  doesn't thread this field through.
- **No `usage`/token-count parsing exists in `claude_code_session` at
  all.** `transcript.rs`'s `RowFields` captures `message` as an opaque
  `Value` and never looks inside it for `usage`. `summarize.rs` documents
  ("Only the fields this module needs are modeled — `usage` and other...")
  that it deliberately does *not* model usage either. Every token figure
  produced by this module tree today (`cost_compare.rs`'s
  `NoCompactionEstimate`) comes from **re-estimating tokens with
  `TiktokenEstimator`**, not from reading the API-reported `usage` block
  Claude Code already wrote into the transcript. This directly contradicts
  the requirement's "exact API token usage (input/output/cache_read/
  cache_creation)" — none of that exact-usage extraction exists yet
  anywhere in the codebase; it must be built from scratch, and the
  estimator-based code path is not reusable for it (different data source
  entirely, tiktoken counts will disagree with API-reported counts by a
  non-trivial margin, especially for tool-result JSON).
- **`PricingTable`/`ModelPrice` only has two rate fields** — `input_usd_per_token`
  and `output_usd_per_token`
  ([`pricing.rs:45-51`](../../../src/cost_metrics/pricing.rs#L45-L51)) — no
  `cache_read` or `cache_creation` rate fields at all, even though
  Anthropic prices those at materially different rates (cache read ≈10% of
  input; cache write ≈125% of input for 5-minute TTL). The vendored LiteLLM
  snapshot (`pricing_default.json`) *does* carry
  `cache_read_input_token_cost`/`cache_creation_input_token_cost` upstream —
  this repo's filter just never pulled them in. Any cost-per-call number
  this feature computes using the existing `PricingTable` as-is will be
  wrong for cache-heavy calls (which the requirements themselves say are
  ~60%+ of context) unless `ModelPrice` is extended first.
- **`extract_usage` in `src/providers/mod.rs:457` only reads
  `input_tokens`/`output_tokens`** off the Anthropic response, silently
  discarding `cache_creation_input_tokens`/`cache_read_input_tokens` if
  present. This is the function `record_actual_usage_from_anthropic_response`
  calls to feed the *existing* proxy-based cost tracker — meaning
  consolette's current proxy-captured cost figures are already missing the
  cache breakdown, a preexisting gap this feature will either need to fix
  (if reusing this path) or must not accidentally treat as ground truth
  when cross-checking against transcript-derived numbers (§4).
- **Heavy reliance on hand-rolled `Deserialize`/`Serialize` for
  `TranscriptRow`** because serde's `#[serde(other)]` can't carry data on
  an internally-tagged enum's catch-all variant (documented at
  [`transcript.rs:50-62`](../../../src/claude_code_session/transcript.rs#L50-L62)).
  Any new row-shape variant this feature needs (e.g. modeling `usage`
  fields, or Codex's differently-shaped rollout-log rows) must follow the
  same manual-impl pattern or risk silently falling into `Unknown` and
  losing the very `usage` data being extracted — it is easy to add a
  `#[serde(rename = "usage")]` field to `RowFields` naively and have it
  compile but never populate, because `message` is `Option<Value>` and
  `usage` actually lives nested inside `message.usage`, not at the row's
  top level. (Confirmed by reading `RowFields`: it only captures top-level
  keys via `#[serde(flatten)] extra`; anything inside the opaque
  `message: Option<Value>` needs a second parse pass, not a struct field.)
- **Codex CLI has no prior art in this repo at all.** Every existing
  module in `claude_code_session/` is Claude-Code-specific; `~/.codex/sessions/`
  rollout-log parsing is 100% new code with its own undocumented,
  version-drifting schema (already flagged as a rabbit hole) — but
  concretely, expect Codex's usage-accounting shape (does it even expose
  cache_read/cache_creation separately, or fold them into a single
  `cached_tokens` the way OpenAI's Chat Completions/Responses API does?)
  to differ enough from Anthropic's that a single `UsageBreakdown` type
  shared across both ingesters will need an explicit "this CLI doesn't
  distinguish X" representation, not an `Option` that silently means
  "zero."

## 3. `~/.claude/settings.json` hook-install pitfalls

No hook-install code exists anywhere in this repo today (`grep` for
`settings.json`/`install_hook`/`HookConfig` across `src/` returns nothing) —
this is 100% new surface, and it's the one part of this feature that
mutates a file Tyler's *other* tooling and Claude Code itself depend on
live.

- **Concurrent read/write with a running Claude Code process.** Claude Code
  reads `settings.json` at session start (and possibly on hook-list
  changes, per its hot-reload behavior in newer versions) — installing/
  uninstalling hooks while a Claude Code session is active risks: (a) Claude
  Code reading a half-written file if the installer doesn't write-then-
  atomically-rename (write to `settings.json.tmp`, `fsync`, then `rename()`
  over the original — never edit in place with a truncate+rewrite), and (b)
  the *running* session not picking up the new hook until its own reload
  point, giving a false impression that install "didn't work" when it's
  actually just not yet live for that process.
- **JSON formatting/comment loss on round-trip.** `settings.json` is
  hand-edited by users (Tyler's own global `~/.claude/CLAUDE.md`-adjacent
  config included) — a naive `serde_json::from_str` → mutate → `to_string`
  round-trip will: reorder keys (Rust's default `HashMap`/`serde_json::Map`
  is insertion-order-preserving *if* the `preserve_order` feature is
  enabled on `serde_json`, but this repo's `Cargo.toml` should be checked
  for that feature — if not enabled, keys silently reorder), drop trailing
  commas/whitespace formatting the user had, and JSON has no comment
  syntax at all so any `// ...` a user hand-added (invalid JSON, but some
  editors/linters tolerate it) will hard-fail parsing rather than round-
  trip. The installer must open with `serde_json::Value` (not a strict
  typed struct) so unknown top-level keys (`permissions`, `env`, any other
  section Tyler has configured) are preserved byte-for-byte in fields it
  doesn't touch, and should diff-preview or dry-run before writing.
- **Backup-first is necessary but not sufficient for "reversible."** A
  timestamped backup (`settings.json.bak.<timestamp>`) satisfies the
  literal requirement, but true idempotent uninstall means the installer
  must be able to recognize *its own* previously-installed hook entries
  (e.g. by a stable marker/comment-equivalent — since JSON has no comments,
  this likely means a recognizable command-string prefix or a dedicated
  `"_consolette"` metadata key) and remove exactly those, not "restore from
  the last backup" (which would clobber any hooks the user added *after*
  install but before uninstall). Restoring from backup is the wrong
  uninstall semantics whenever time has passed between install and
  uninstall.
- **Hook exit-code/latency contract.** Claude Code enforces a timeout on
  hook scripts (documented default is short — on the order of tens of
  seconds) and treats non-zero exit specially per hook type (e.g. some
  hook types can block the action on exit code 2). A `PostToolUse` hook
  that shells out to write to a SQLite store or append to a JSONL sidecar
  file must be fire-and-forget-fast (append-only write, no lock contention
  with a concurrently-running dashboard reader) or it will add
  perceptible latency to *every tool call* in every Claude Code session —
  this is the single biggest way this feature could make Tyler's actual
  daily driver (Claude Code itself) feel slower, and it's easy to
  underestimate hook overhead when testing dashboard code in isolation
  from a live session.
- **Multiple hooks on the same event, one array.** Claude Code's
  `settings.json` hook schema allows multiple entries per event
  (`PostToolUse` can already have Tyler's existing hooks, e.g. an RTK
  proxy-rewrite hook per `RTK.md`, or the fewer-permission-prompts style
  hooks). The installer must *append* to the existing array for that event
  rather than overwrite it, and must handle the case where the array
  already contains a stale/duplicate consolette entry from a previous
  crashed install (re-running `up` shouldn't produce two copies of the
  same hook — idempotency needs an explicit "is this exact entry already
  present" check, not just "does the file exist").
- **Cross-tool contention isn't limited to Claude Code.** Tyler's
  `stapler-scripts/llm-sync` mirrors Claude config to other tools
  (Gemini/OpenCode/Antigravity) — per the repo's own CLAUDE.md, MCP servers
  in particular get synced from `.config/mcp/mcp-servers.json`. If llm-sync
  or any other automation also touches `~/.claude/settings.json` on a
  schedule, there's a second writer to coordinate with beyond "just Claude
  Code itself" — worth explicitly checking whether llm-sync touches
  `settings.json` (not just the MCP server list) before assuming
  consolette's installer is the only non-interactive writer.

## 4. Cost/token-accounting domain pitfalls

- **`cache_read` vs `cache_creation` are not interchangeable with
  `input_tokens`.** Anthropic's usage object reports up to four figures
  per call (`input_tokens`, `output_tokens`, `cache_creation_input_tokens`,
  `cache_read_input_tokens`), and `input_tokens` in that object is
  specifically the *non-cached* portion — the "effective" input size for
  a call is the sum of all three input-side figures, not `input_tokens`
  alone. Any composition-breakdown chart ("Tool I/O vs Conversation vs
  System") that classifies tokens by content-block type but sums their
  costs using only `input_usd_per_token` will overstate cost for the
  cache-read portion (which is far cheaper) and understate cost for the
  cache-creation portion (which is more expensive) — this is exactly the
  gap identified in §2 in `PricingTable`.
- **Pricing model drift.** `pricing_default.json` is a vendored, manually-
  synced snapshot ("Re-sync by re-fetching... from github.com/BerriAI/litellm
  and re-filtering") with no automated freshness check — a stale snapshot
  silently produces wrong dollar figures with no error, only a wrong
  number on the dashboard. Given this feature's core value proposition is
  "trust these dollar figures," staleness needs either a visible
  "pricing snapshot as of DATE" indicator or the live-refresh path
  (`spawn_pricing_refresh_task`, already present per `pricing.rs`'s module
  doc) enabled by default for this feature specifically, even if it isn't
  for the base proxy cost tracker.
- **Double-counting between proxy-captured and transcript-derived usage.**
  The requirements explicitly call for cross-checking these two sources
  with transcript as primary — but *both* sources can observe the same
  underlying API call: the proxy captures it when consolette is in the
  request path; the transcript captures it because Claude Code always
  writes `usage` into the transcript regardless of whether the request
  went through consolette's proxy. If both ingestion paths write to the
  same store keyed loosely (e.g. by session_id + timestamp) without a
  precise dedup key (the Anthropic API's `request_id`/message `id` is the
  only safe join key — timestamps can collide or drift), a call that both
  paths saw will be double-counted in any aggregate that sums across
  sources instead of treating one as authoritative and the other as
  reconciliation-only. The requirement says "transcript primary" — the
  store schema must enforce that at write time (proxy-captured rows never
  contribute to totals directly, only to a comparison/variance field)
  rather than relying on query-time deduplication to get it right every
  time.
- **Subagent token attribution is genuinely ambiguous and needs an
  explicit design decision, not just plumbing.** Claude Code subagent
  (Task tool) invocations run their own nested conversation with their own
  `usage` figures, written into the transcript as `isSidechain: true`
  rows — which `transcript.rs`'s `build_turns` explicitly *excludes* from
  the main chain (`by_uuid` is built from "non-sidechain rows only,"
  [`transcript.rs:249-254`](../../../src/claude_code_session/transcript.rs#L249-L254)).
  That means today's turn/chain reconstruction has no representation of
  subagent token spend at all — it's invisible to `chain_coverage` and to
  every downstream consumer. For this feature: does a subagent's token
  spend count against (a) the parent turn that spawned it, (b) its own
  independent "session" in the store, (c) both (double-counted on
  purpose, clearly labeled), or (d) a separate `subagents` budget the
  requirements' Store schema section already lists as a table? The
  requirements list `subagents` as a store table but don't say how its
  totals roll up into session-level totals — this needs a decision before
  schema design, not an implicit default that falls out of "however the
  join happens to work," because getting it wrong either double-counts
  subagent-heavy sessions or makes them look artificially cheap.

## 5. Scope-control risks for a solo Large-appetite effort

- **4 view groups × 2 CLIs × 2 data sources is not one feature, it's at
  least eight delivery slices**, and the ones with real payoff are not
  evenly distributed. Concretely: composition breakdown and context-
  growth-per-turn only need Claude Code transcript parsing (already 80%
  there via `transcript.rs`/`build_turns`) plus the net-new exact-`usage`
  extraction from §2 — that's the cheapest, highest-signal slice and
  should ship and be validated against real sessions *before* Codex
  ingestion or cross-session analytics are touched at all. Committing to
  "all 4 views × both CLIs" as one deliverable risks a Large appetite
  (3-6 weeks solo) blowing past its circuit breaker with nothing usable
  shipped.
- **The headroom/compression-ceiling audit is the correct thing to cut or
  defer, not shrink.** It depends on a Python package
  (`headroom-ai==0.32.1`) with no Rust equivalent per the requirements'
  own rabbit-holes list — the *feature* (offline audit against Tyler's own
  transcripts) is legitimately valuable but architecturally foreign to a
  Rust CLI/MCP tool. Attempting a partial Rust port risks spending a large
  fraction of the appetite reimplementing a compression-ceiling estimator
  instead of shelling out to the existing Python tool (`pip install
  headroom-ai`) the way context-analyzer itself does — reusing it as a
  subprocess (mirroring `summarize.rs`'s existing pattern of shelling out
  to an external CLI and parsing its stdout, per
  [`summarize.rs`](../../../src/claude_code_session/summarize.rs)) is a much
  cheaper way to get this view than a native reimplementation, and should
  be the explicit design decision rather than something punted on
  implicitly.
- **"Persistent store" is a bigger decision than the requirements'
  phrasing suggests.** `cost_metrics::store` today
  ([`store.rs:1-10`](../../../src/cost_metrics/store.rs#L1-L10)) is
  explicitly in-memory (`moka::future::Cache`) with a bounded per-session
  ring buffer (`MAX_RECORDS_PER_SESSION = 200`) — it is not persistent at
  all today, despite the module living under a package called
  `cost_metrics`. This feature's "persistent store" requirement (sessions,
  per-call breakdowns, turns, hook events, subagents — five+ tables per
  the requirements) is a materially different animal from that ring
  buffer, and "extend cost_metrics::store vs. separate store" (already a
  named rabbit hole) should resolve toward **separate**: reusing
  `rusqlite` (already a bundled dependency via `omission_cache.rs`'s
  existing SQLite-backed cache — a working precedent for schema/migration
  patterns in this repo) as its own dedicated store, rather than trying to
  retrofit persistence and a five-table relational schema onto a
  bounded-ring in-memory cache designed for a completely different
  workload (proxy-request cost reconciliation, not forensic history).
- **Dashboard rendering risk is underweighted in the requirements.**
  Checked directly: `cost_metrics/dashboard.html`
  ([405 lines](../../../src/cost_metrics/dashboard.html)) is a plain HTML
  `<table>` populated via `fetch("/v1/dashboard/sessions")` and manual DOM
  `appendChild` — no canvas, no SVG, no charting library of any kind. The
  requirements ask for genuinely more complex visualizations (a context-
  growth time series with budget-threshold overlays and an autocompact
  line, a scrubber with playback, a cache-read-churn chart, a cost-vs-
  context scatter plot) than anything that exists in this repo today.
  `server.rs`'s serving pattern (static HTML + JSON endpoints) is reusable;
  the charting itself is 100% new — either a small vendored/inlined JS
  charting lib or hand-rolled SVG/canvas drawing, both of which are
  meaningfully more work than "reuse the existing dashboard." Budget this
  explicitly rather than assuming the new views bolt onto the existing
  pattern cheaply — this is exactly the kind of thing that silently
  balloons a "view group" from a day of work into a week.
