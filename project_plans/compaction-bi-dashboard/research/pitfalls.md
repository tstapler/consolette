# Pitfalls research: compaction-bi-dashboard

Sources: `src/claude_code_session/boundary.rs`, `cost_compare.rs`, `transcript.rs`,
`discovery.rs`, `cost_metrics/server.rs`; live inspection of real `compact_boundary`
rows under `~/.claude/projects/` (this machine currently has 7,840 `.jsonl` files,
more than the 1,135 cited in requirements.md, so the parser/perf work should be
validated against whatever tree is live at test time, not a hardcoded count).

## 1. Parsing an undocumented external shape (`compactMetadata`)

Confirmed real shape (`rg -m1 -l "compact_boundary" ~/.claude/projects/`, then
extracted one row):

```json
{
  "type": "system", "subtype": "compact_boundary",
  "parentUuid": null, "logicalParentUuid": "50cf282e-...",
  "compactMetadata": {
    "trigger": "auto",
    "preTokens": 108176, "postTokens": 23394,
    "cumulativeDroppedTokens": 84782, "durationMs": 181623,
    "preCompactDiscoveredTools": ["WebFetch"],
    "preservedSegment": {"anchorUuid": "...", "headUuid": "...", "tailUuid": "..."},
    "preservedMessages": {"anchorUuid": "...", "uuids": [...], "allUuids": [...]}
  }
}
```

Sampled 8 real occurrences: `trigger` was always `"auto"` in this corpus (a
`"manual"` trigger is plausible per Claude Code's `/compact` command but wasn't
observed — don't assume the field is an exhaustive enum in Rust); `preCompactDiscoveredTools`
was present in roughly half the rows and absent in the rest, confirming it's
genuinely optional, not just occasionally empty.

**Recommended serde pattern**, matching the repo's existing convention
(`boundary.rs`'s `CompactionMetrics` already uses `#[serde(default)] pub real_cost_usd: Option<f64>`):

- Every field on the new `NativeCompactMetadata` struct should be `Option<T>`
  (or have `#[serde(default)]` for non-Option types like counters), even fields
  that are "always present" in today's sample — a future CLI version dropping
  or renaming one field must degrade to `None` on that field, not fail the
  whole row.
- Deserialize the row's `compactMetadata` value with `serde_json::from_value`
  the same way `extract_compaction_metrics` already does (`boundary.rs:104-119`)
  — `.ok()` on a per-row basis, not `?`, so one malformed row yields `None`/is
  skipped rather than aborting the file.
- Do **not** add a custom `Deserialize` impl with manual fallback logic unless
  a specific field needs cross-validation; `#[serde(default)]` + `Option<T>`
  on every field is simpler, matches existing style, and is sufficient here
  since there's no need to reject a whole row for one bad field — the row is
  either "has a recognizable `compact_boundary` subtype" (structural, cheap to
  check) or not.
- Never call `.unwrap()`/`.expect()` on any field of this struct in aggregation
  code — every consumer must treat every field as absent-capable, since
  "checked once at parse time" doesn't survive the struct being cloned/passed
  around later (this matches the repo-wide `clippy::unwrap_used` denial visible
  in `#[allow(clippy::unwrap_used)]` annotations scoped only to test modules).
- Skip-and-log: use `tracing::debug!`/`warn!` (the pattern `discovery.rs:77`
  already uses for a bad glob entry) when a `subtype: "compact_boundary"` row's
  `compactMetadata` fails to deserialize at all (e.g. it's not an object) —
  don't propagate that as an `anyhow::Error` that aborts the containing
  session's aggregation row.

## 2. Blocking the async runtime across ~1,135+ files

**Confirmed, not hypothetical**: `discover_sessions_glob` (`discovery.rs:70-92`)
does synchronous `glob()` + `fs::metadata()` per file, and `parse_session_file`
(`transcript.rs:159-161`) does a synchronous `File::open` + `BufReader::lines()`.
`compare_compaction_cost` (`cost_compare.rs:56-94`) is an `async fn` that calls
`parse_session_file` directly with **no `spawn_blocking`** — today this is fine
because it's one file per call (`/v1/cost/{session_key}`), but the new
aggregation route will call this (or equivalent) N=1,135+ times in one request.

Risks and mitigations:
- **Runtime starvation**: doing all N synchronous file reads on the async
  worker thread that's also serving other axum requests will stall the whole
  server for the duration of the scan (likely seconds, given file count).
  Wrap the whole aggregation loop (or each file's parse) in
  `tokio::task::spawn_blocking`, or use `tokio::task::block_in_place` if only
  available on a multi-threaded runtime — check `serve_cost`'s runtime setup
  (`cost_metrics/server.rs:136-143`) to confirm it's multi-threaded before
  relying on `block_in_place`.
- **One bad/huge file stalling aggregation**: `parse_session_file` already
  streams line-by-line (good — no full-file `read_to_string`), but a single
  pathological file (multi-GB, or one with millions of tiny lines) can still
  dominate wall-clock time for the whole request. Consider a per-file
  soft budget (e.g. skip/flag a file that takes >N seconds or has >N lines)
  so one outlier doesn't make the whole dashboard hang — the requirements'
  Non-Functional section only requires "doesn't hang the browser tab," so a
  per-file timeout/limit is in scope even though not explicitly named.
- **Unbounded memory**: `build_turns` (used by `cost_compare.rs:61`) holds the
  *entire* reconstructed turn list in memory per file; doing that for 1,135+
  files concurrently (if parallelized via `tokio::spawn` per file rather than
  processed serially inside one `spawn_blocking`) could multiply peak memory.
  Prefer bounding concurrency (e.g. `futures::stream::iter(...).buffer_unordered(N)`
  with a modest N, or simply processing files serially inside one
  `spawn_blocking` closure) over spawning 1,135 unbounded concurrent tasks.
- **Caching**: given the above cost, the in-memory-cache-with-manual-refresh
  option flagged as an open question in requirements.md is likely worth doing
  from the start rather than deferred — a per-request full rescan of 1,135+
  files, even off the async thread, is a multi-second HTTP response on every
  page load/refresh.

## 3. Hand-rolled HTML/JS dashboard page

- **XSS**: session data (project paths, session IDs, any free-text like
  `trigger` values) must never be interpolated into HTML via string
  concatenation/template literals + `innerHTML`. Since the page fetches JSON
  via `fetch()` and renders client-side, use `textContent`/`createElement`
  (or a minimal escaping helper) for every value that originates from a
  session file — project directory names and session UUIDs come from the
  filesystem path, which is attacker-adjacent only in the sense that any tool
  or hook could write arbitrary content into a `.jsonl` under
  `~/.claude/projects/`; treat all of it as untrusted rather than
  "it's my own history, so it's safe."
- **JSON injection into embedded `<script>`**: if the initial page ever embeds
  a JSON blob directly into an inline `<script>` tag (rather than only via a
  separate `fetch()` to the JSON route), the standard `</script>`-breakout and
  `<!--` mitigations apply — safest is to avoid embedding data in the HTML at
  all and only ever `fetch()` the JSON endpoint, which this feature's own
  design (separate JSON route + static page) already does by construction.
- **Client-side sort/filter performance at ~1,135 rows**: plain DOM
  sort/re-render of a ~1,135-row `<table>` on every keystroke of a filter box
  is likely fine (well under the range where naive re-rendering becomes
  janky, which is usually 10k+ rows for simple tables), but debounce the
  filter input (e.g. 150-250ms) to avoid re-rendering on every keystroke, and
  prefer re-using existing `<tr>` elements (toggle a `hidden`/`display:none`
  class) over destroying and rebuilding the whole table body on every filter
  change, which is the more common source of jank than raw row count.

## 4. Loopback-only security assumption

`serve_cost` binds to a configurable port with no auth (per requirements.md's
existing posture, confirmed by `cost_metrics/server.rs`'s `serve_cost(port)`
taking no auth/bind-address parameter — worth double-checking at implementation
time whether it explicitly binds `127.0.0.1` vs `0.0.0.0`, since the latter
would be a real, if pre-existing, exposure). The new dashboard route and JSON
endpoint must not change that binding, and should not be designed in a way
that assumes any request-level trust (e.g. no route should accept a
filesystem path parameter from the client and read arbitrary files — the
aggregation route should only ever walk `discover_sessions`'s own glob output,
never a client-supplied path) since if the loopback assumption is ever
violated (e.g. a future refactor accidentally binds `0.0.0.0`, or a
reverse-proxy misconfiguration), an arbitrary-path-read endpoint would turn a
scoped info leak into an arbitrary local file disclosure across
`~/.claude/projects/` and beyond.

## 5. Double-counting: can a row satisfy both `consoletteCompact` and native `compactMetadata`?

Checked `build_boundary_row` (`boundary.rs:172-204`): consolette's own
boundary rows carry `type: "system"`, `subtype: "compact_boundary"`, and a
`consoletteCompact` marker, but **deliberately do not** set `compactMetadata`
(the doc comment at `boundary.rs:164-166` states this explicitly: "compactMetadata`/`level`
are deliberately not reproduced — they describe Claude Code's own auto-compaction
internals, which this compactor has no equivalent for"). So by construction,
a consolette-authored boundary row will never carry `compactMetadata`, and a
real native `compact_boundary` row (written by the `claude` CLI itself) will
never carry `consoletteCompact` (consolette never mutates existing rows in
place — `writer.rs` only appends new rows to a new destination transcript,
confirmed by the `write_destination_transcript` test at
`writer.rs:353-389` building an entirely new output file rather than editing
the source).

**However**, one *session* (not one row) can absolutely have both: a
transcript can be auto-compacted natively by Claude Code mid-conversation,
then later run through consolette's own `compact_session`, leaving one native
`compact_boundary` row (with `compactMetadata`, no `consoletteCompact`) and
one consolette boundary row (with `consoletteCompact`, no `compactMetadata`)
in the same file. The parser/aggregator must:
- Detect the two independently per row (two separate predicate functions,
  matching the requirement that `is_compacted()`'s consolette-only semantics
  must not change) — never assume "has `compact_boundary` subtype" implies
  "is consolette's."
- Report at the *session* level as one of four states — native only,
  consolette only, both, neither — matching requirements.md's explicit
  success metric ("filterable... by compaction status (native only /
  consolette only / both / neither)"). Do not collapse "both" into whichever
  one is checked first, and do not sum native + consolette tokens-saved into
  a single number without labeling which portion came from which source
  (this is also the Feasibility Risk in requirements.md about native vs.
  consolette-estimated tokens using different accounting — don't let a
  "total saved" field silently blend an exact native count with a
  `TiktokenEstimator` estimate).

## 6. Chain-coverage / multi-root transcripts at scale

`ChainCoverage` (`transcript.rs:404-421`, computed by `chain_coverage` at
`transcript.rs:430-459`) already documents that `build_turns` only
reconstructs the *active chain ending at the file's last row* — a transcript
with disconnected roots (repeated `--resume`/`--clear` cycles) has real
message history the reconstructed `turns` never sees, so `chain_messages /
total_messages` can be well below 1.0 today even for a single session
(`cost_compare.rs`'s own test `compare_compaction_cost_should_report_partial_chain_coverage_for_multi_root_transcript`
demonstrates a 0.5 ratio case).

At 1,135+ files, this compounds two ways the aggregate view must not
obscure:
- **Per-row native-compaction counting is chain-agnostic, per-session
  no-compaction estimation is not.** Native `compact_boundary` rows can be
  found and parsed regardless of which chain they're on (a simple per-row
  scan, not a `build_turns` walk) — so native-compaction counts/metrics for a
  session are complete even when `chain_coverage` for that same session is
  low. If the dashboard reuses `no_compaction`/`total_tokens` (which *is*
  chain-scoped, per `cost_compare.rs:56-94`) as a baseline for computing
  consolette's percentage savings, a low-coverage session will show an
  artificially small "estimated tokens" denominator, inflating the apparent
  percentage saved. The `chain_coverage.ratio()` must be surfaced as its own
  column (not just used internally to compute a percentage that then hides
  it), exactly as requirements.md's Rabbit Holes section already anticipates
  ("the aggregate table must surface coverage rather than silently
  presenting partial-chain numbers as if they were complete").
- **Aggregating across 1,135+ files means a non-trivial fraction will have
  partial coverage** (any session that saw `--resume`/`--clear` at least
  once) — this isn't a rare edge case worth a single asterisk, it's a
  systemic property of the corpus. The dashboard's filter/sort UI should let
  the operator sort or flag by coverage ratio (e.g. sort ascending on
  coverage to find the sessions whose "no-compaction" cost estimate is least
  trustworthy), not just display it as an inert extra column nobody notices
  among 1,135 rows.
