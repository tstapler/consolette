# Pitfalls & Risks: compaction-cost-metrics

Research for Phase 2 (SDD) — Agent 4. Scope: known failure modes for session-scoped
cost/token instrumentation bolted onto `SessionCompactionPipeline::apply` and the
OpenAI-shim response path in `src/providers/mod.rs`.

## 1. General LLM-proxy token/cost accounting pitfalls

- **Double-counting on retries/streaming.** If a request is retried (provider 5xx,
  client disconnect-and-resubmit) or a streamed response is read incrementally, naively
  incrementing session counters on every "response observed" event will count the same
  logical turn twice. `usage.input_tokens`/`output_tokens` must be attributed exactly
  once per accepted response; a retried request should replace, not add to, the prior
  attempt's contribution.
- **Race conditions on concurrent session updates.** `SessionStateStore` gives an
  `Arc<RwLock<SessionState>>` per key, but two concurrent requests on the *same*
  `SessionKey` (e.g. a client firing overlapping calls, or a retry racing the original)
  can both read pre-update state, compute their own delta, and write back — losing one
  update. This is a plain read-modify-write race on the `RwLock`'s guarded fields; the
  fix is to make the increment atomic under a single write-lock acquisition (or use
  interior atomics for the counters), not "read state, compute new state in caller,
  write state" across an await point.
- **Drift between estimated and real usage.** The counterfactual (tiktoken-estimated,
  computed at `apply()` time on the *pre-compaction* message array) and the actual
  (real `usage.input_tokens`, observed later in `src/providers/mod.rs:267-275`) are
  fundamentally different measurements — different tokenizer, different point in time,
  different message set. Reported deltas can look wrong (even negative "savings") for
  reasons that have nothing to do with compaction quality: system-prompt caching
  discounts, provider-side prompt truncation, or the tokenizer mismatch itself. The
  UI/API must never imply these two numbers are computed the same way.
- **Cache eviction losing in-flight accounting.** `SessionStateStore`'s moka cache has
  `time_to_live(Duration::from_hours(1))` (`src/session_compaction/session_state.rs:61`).
  A session that starts a request just before the 1hr TTL fires can have its
  `SessionState` evicted before the response returns and the actual-usage reconciliation
  tries to write back — silently dropping that turn's actual accounting (see §3). Moka
  eviction listeners run outside the critical path and are not synchronized with
  in-flight requests holding a `SessionKey`.
- **Global counters cross-contaminate session-scoped ones.** `ProxyMetrics`
  (`src/metrics/counters.rs`) already tracks global `tokens_before`/`tokens_after` as
  `AtomicU64`. Wiring per-session accounting alongside it invites accidental double
  bookkeeping paths (one team member "fixes" the global counters to be per-session-sum
  and breaks the global `/metrics` contract, or vice versa) if the two aren't clearly
  documented as independent. Keep them structurally separate (new struct/module), not
  a refactor of `ProxyMetrics`.

## 2. Stack-specific risks

- **tiktoken-rs correctness/maintenance.** `tiktoken-rs = "0.5"` is already a dependency
  (Cargo.toml:88) but it implements OpenAI's BPE vocabularies (cl100k_base, o200k_base,
  etc.) — it has **no Anthropic tokenizer**. Anthropic's real tokenizer is not public;
  any tiktoken-based estimate for Claude-model traffic is a heuristic approximation, not
  a measurement. Requirements already call this out as a rabbit hole/risk — the
  mitigation is exclusively a labeling/UX concern (always mark counterfactual figures as
  "estimated"), not a fixable accuracy problem. Also watch tiktoken-rs's own maintenance
  cadence (upstream vocab-file changes when OpenAI ships new encodings) since it's a
  fairly small crate ecosystem-wise.
- **Moka concurrency: read-then-write races under the existing pattern.**
  `SessionStateStore::get_or_default` (session_state.rs:67) returns a cloned `Arc` — the
  cache itself is safe, but the `RwLock<SessionState>` inside is exactly where races
  live. moka's `get_with`/`entry` API atomically initializes missing entries, but does
  **nothing** to make multi-step mutations of the *value* atomic across `.await` points.
  Any code that does `state.read().await` → compute new totals → `state.write().await`
  in two separate lock acquisitions has a TOCTOU gap. Concurrent requests to the same
  session (the realistic case: a client fires several tool-call round-trips in flight)
  will lose updates unless the whole read-modify-write happens under one lock, or the
  new counters are atomics (`AtomicU64`) inside `SessionState` rather than plain
  integers protected by the outer `RwLock`.
- **Async/lookup dependency vs. "no per-request latency" constraint.** The requirement
  explicitly forbids inline network calls for pricing in `SessionCompactionPipeline::apply`.
  The natural failure mode is a well-intentioned "just check if pricing needs refresh"
  call that looks synchronous-cheap in dev (cache hit) but becomes a blocking network
  round-trip in prod on cold start or cache-expiry storms (all sessions' first request
  after TTL expiry triggering a lookup simultaneously — thundering herd on the pricing
  endpoint). Mitigate with a background refresh task (spawned once, on a timer) that
  publishes into a `watch`/`ArcSwap`-style cell that `apply()` reads synchronously and
  never awaits. Also: a hung/slow pricing API must never be able to block compaction —
  it needs its own timeout and must fail closed to the static table, not retry inline.
- **Pricing table staleness is invisible by default.** If the optional live lookup
  degrades to the static table (network failure, API shape change, rate limiting), and
  that degradation isn't loud (the requirement's Observability section already flags
  logging/metric-on-fallback), dollar figures silently drift from reality for
  arbitrarily long. This is worse than no live lookup at all, because it looks
  authoritative.

## 3. Actual/counterfactual reconciliation pitfalls

- **Request/response pairing bugs.** Counterfactual tokens are computable at `apply()`
  time (pre-compaction message array, before the request is sent); actual tokens are
  only known once the provider responds (`src/providers/mod.rs:267-275`, currently
  discarded after being reshaped to OpenAI format). Between those two points there is no
  existing correlation id threaded through the pipeline — `apply()` returns a
  `CompactionReport` to `post_compact` hooks, but that report is built *before* the
  response exists. Any design must either (a) hold the counterfactual estimate keyed by
  a per-request id until the response arrives and reconcile then, or (b) restructure so
  actual-usage recording happens in a hook/callback that already has access to both. Get
  the correlation key wrong (e.g. keying only by `SessionKey` when a session has
  concurrent in-flight requests) and two requests' actual/counterfactual pairs can cross
  — attributing request A's actual tokens to request B's counterfactual baseline.
- **Dropped/failed requests never reconcile.** If the upstream call errors, times out,
  or the client disconnects before a response is parsed, there is no `usage.input_tokens`
  event to close out the counterfactual estimate that was already recorded at `apply()`
  time. Left unhandled, every failed request becomes a permanent "pending" entry:
  cumulative-actual understates cumulative-counterfactual forever for that session, and
  a naive implementation that stores pending entries in a `Vec`/`HashMap` keyed by
  request id **never removes them on the error path**, which leaks memory (see below).
  Need an explicit "abandon this pending reconciliation" path on every error/timeout
  branch, or a short pending-entry TTL independent of the session's own 1hr TTL.
- **Memory leaks from unreconciled entries.** Two failure surfaces: (1) per-request
  pending state (the correlation map from the previous bullet) growing unboundedly if
  requests fail without cleanup, and (2) `SessionState` itself growing unboundedly if
  cumulative accounting is stored as a growing list of per-request records instead of
  running totals. The existing `RequestDetail` ring buffer (`src/metrics/mod.rs`, "last
  100 requests") is the cautionary example of a bounded-by-design pattern already in the
  codebase — new per-session state should default to O(1) running aggregates (sums,
  counts), not O(requests) history, unless the requirement genuinely needs per-request
  drill-down.
- **Streaming responses complicate "the response returns" boundary.** If any target
  provider path streams SSE chunks, `usage` may arrive only in a final chunk (or not at
  all for some providers/error paths) — code that assumes usage is available synchronously
  after "the response" needs to handle "usage never arrived" as a first-class case, not
  an exception.

## 4. What to explicitly design against

- **Unbounded growth of per-session accounting state** — cap to running totals per
  session (actual tokens, counterfactual tokens, request count), not per-request
  history; rely on the existing 1hr TTL for session-level cleanup, and add an
  independent, shorter timeout for orphaned in-flight reconciliation entries so a
  crashed/dropped request can't hold state open past the session's own TTL.
- **Silently stale pricing data** — every dollar figure must be observable as "static
  table" vs. "live lookup," with the requirement's own observability point (log/metric
  on fallback) treated as a hard requirement, not a nice-to-have; consider surfacing the
  pricing-table's age/source in the API response itself, not just server logs.
- **Misleading precision in displayed numbers** — counterfactual tokens (tiktoken
  estimate of Claude-bound text) and the derived dollar delta are approximations
  compounded from two heuristics (wrong tokenizer + possibly-stale pricing); avoid
  displaying them with false precision (e.g. `$0.0231847`) or without an "estimated"
  qualifier next to every counterfactual-derived figure, per the requirement's own
  "exact vs estimated source label per figure" observability requirement.
- **Lock contention becoming a new hot-path cost** — even though pricing lookups must
  stay off the hot path, the accounting write itself (session state mutation) is
  necessarily inline; keep the critical section tiny (atomic increments, not
  read-full-struct-clone-modify-write-full-struct) so this feature doesn't itself
  introduce the latency regression the constraints are trying to avoid.
- **Coupling accounting correctness to hook ordering** — `CompactHookRegistry` invokes
  hooks in registration order with no error handling/isolation shown between hooks
  (`src/session_compaction/hooks.rs`); if the new accounting hook is registered
  alongside others, an unrelated hook's behavior (or panic) must not be able to corrupt
  or skip cost recording, and the accounting hook itself must not be allowed to affect
  compaction behavior (recording is observational only, never feeds back into `tier`
  selection within the same request).

## Sources consulted (VERIFIED — read directly)

- `src/session_compaction/hooks.rs:1-40` — `CompactionReport`, `CompactHooks`,
  `CompactHookRegistry` shapes; confirms no token/size fields exist yet and hooks run
  in plain registration order.
- `src/providers/mod.rs:250-289` — confirms `usage.input_tokens`/`output_tokens` are
  parsed from real Anthropic responses and only used to build the OpenAI-shape reply;
  discarded afterward.
- `src/session_compaction/session_state.rs` (grep) — confirms `SessionStateStore` is
  `Cache<SessionKey, Arc<RwLock<SessionState>>>` via `moka::future::Cache`, TTL
  `Duration::from_hours(1)`, and `get_or_default` as the access pattern — this is the
  exact concurrency shape the moka/race-condition analysis above is based on.
- `Cargo.toml:80,88` — confirms `moka = "0.12"` (features = ["future"]) and
  `tiktoken-rs = "0.5"` are already dependencies, not proposed additions.
