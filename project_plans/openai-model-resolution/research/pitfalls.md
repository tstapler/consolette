# Research: Pitfalls — openai-model-resolution

Scope note: this digs past the Rabbit Holes/Feasibility Risks already named in
`requirements.md` — it doesn't restate them, it adds depth and net-new risks
found by reading the actual code this feature will sit next to.

## 0. Prior art already exists in this repo — read it before designing anything

`src/routing/capability.rs` (added by the `auto-model-family` project,
`project_plans/auto-model-family/`) already solves a structurally identical
problem: periodically probing pinned models with a real, cheap, synthetic
request and caching a verdict. It is the single best reference for this
project and should be treated as the default pattern to follow or explicitly
deviate from, not reinvented from scratch:

- `EVAL_TOOL_NAME = "get_time"` — a probe uses a synthetic tool call
  identifiable in logs, "never a real client tool," so probe traffic can be
  distinguished from production traffic in metrics/dashboards/billing review.
- `EVAL_PROBES_PER_ROUND = 2` with `EVAL_PROBE_SPACING_SECS = 3` — best-of-N
  with spacing, because "a single flaky miss must not exile a model."
- `EVAL_MAX_TOKENS = 64` — a deliberately cheap probe shape.
- **Paid pins are never probed** ("probes cost real money") — only `:free`
  suffixed models are eval-eligible. This project's probing is against
  paid/production upstreams (ExampleCorp Model Gateway), so it cannot copy this
  exclusion outright, but it must decide its own cost-safety equivalent
  (e.g. probe only on cache-miss/failure, never speculatively; smallest
  possible token budget; no tool calls in the probe body).
- `error_verdict()` (`src/routing/capability.rs:198-218`) is the existing
  answer to Rabbit Hole #3 (deprecation vs. transient vs. wrong-endpoint), and
  it has a **latent bug worth knowing about before copying it**: it treats
  *any* `ProviderError::Validation` as `Fail` ("upstream rejected the model
  id"). But `map_error_status()` in `src/providers/openai.rs:297-323` collapses
  **every** 4xx status except 429 into `ProviderError::Validation(body,
  status)` — 400 (bad request / deprecated model), 401 (bad/expired API key),
  403 (permission denied), 404 (wrong endpoint — exactly the
  chat/completions-vs-responses split this project must handle), and 422 all
  arrive as the same variant. `error_verdict()` has no way to tell "the model
  id is dead" apart from "the API key just expired" or "this model needs
  `/v1/responses`, not `/v1/completions`" — all three currently exile the
  model. That's tolerable for `auto-model-family`'s `:free`-only, best-effort
  cross-upstream aliasing, but it is **exactly the failure mode Rabbit Hole
  #3 warns about**, already sitting live in this codebase. Do not port
  `error_verdict()` verbatim; the new resolution logic needs the actual HTTP
  status code (not just "4xx bucket") to distinguish 404 (try Responses API)
  from 400/410-style deprecation from 401/403 (auth misconfig, must not
  advance the candidate list) from 429 (already separated as `RateLimited`).
  This likely means either enriching `ProviderError::Validation` to carry the
  status code more legibly for callers, or having resolution logic parse the
  body's OpenAI-style `error.code`/`error.type` fields itself (per Open
  Question in requirements.md) rather than trusting the coarse variant.
- `CapabilityVerdict::Unknown` for "no probe answered" (all rate-limited or
  timed out) — the round doesn't decide anything, "punish it for the network,
  not for its tool behavior." Model resolution needs the same three-way split
  (Pass / Fail-advance-candidate / Unknown-stay-put), not a binary
  success/failure.
- Test harness: `ScriptedProvider` (`src/routing/capability.rs:322-401`) is a
  fake `Provider` that returns scripted responses/errors per call — this is
  the existing pattern for testing probe logic without a real upstream, and
  should be reused/extended rather than building a new mock HTTP server from
  scratch for every test (see §5 below for why a full HTTP mock is still
  needed for some of this).

`src/routing/family.rs`, cited in `requirements.md`'s Out of Scope section as
existing prior art, **does not exist in the current tree** — `git log` shows
it was added in commit `13a8a07`/merged via PR #18 (`7c61443`), but by HEAD
the equivalent logic lives in `src/routing/capability.rs`,
`src/routing/router.rs`, and `src/routing/session_overrides.rs` instead (no
`family.rs` file survived, likely folded into `router.rs` during later
review/hardening commits like `7f7aab1`/`5aa3059`). **Verify current file
locations against `git log`/`grep` before citing "existing mechanism X at
path Y" in the plan** — the codebase has already moved once since the
Alternatives-Considered section was drafted, and stale path references in a
plan doc waste an implementer's first 20 minutes.

## 1. Probing side effects: cost, rate limits, thundering herd

- **Concurrent re-resolution on a shared cache.** The NFR says "cache
  invalidation triggers on the cached choice starting to fail." If N requests
  are in flight when the cached model starts 400ing, and each one
  independently notices the failure and starts probing candidates, that's N
  parallel candidate-list walks against the same upstream — worse than the
  problem being solved (a single probe storm right when the upstream is
  already unhappy). This needs either:
  - a single-flight/mutex-guarded resolution per upstream (only one in-flight
    re-resolution at a time; concurrent callers either wait for it or fall
    back to the stale-but-still-attempted cached model for their own
    request), or
  - accept that the *triggering* request itself pays the cost of walking the
    candidate list (serially, in its own request path), and only that
    request's outcome updates the shared cache — meaning some number of
    concurrent requests still fail during the walk, which must be an
    explicit, documented tradeoff, not an accident.
  `capability.rs`'s model doesn't have this problem to solve because it's a
  background poll loop (`run_eval_loop`, fixed interval) that updates a cache
  read-only by the request path — no request ever blocks on or triggers a
  probe. If this project's resolution is instead triggered synchronously by
  a failing production request (which the requirements imply — "recovers
  automatically... within N requests"), it's a materially different
  concurrency shape than the existing prior art and needs its own design,
  not a copy-paste.
- **Billing/cost**: a probe that exercises the *actual* codex-family models
  is not free, and unlike `capability.rs`'s `:free`-gated probes, there's no
  free tier to lean on here — every probe against a paid Model Gateway route
  is real spend. The cheap-probe-shape requirement in Rabbit Holes needs a
  concrete token budget decided in the plan (mirror `EVAL_MAX_TOKENS = 64`?),
  and the observability requirement (§ Observability in requirements.md)
  should probably also count/estimate probe spend, not just success/failure,
  so an operator can tell "resolution is costing us $X/day due to a dead
  upstream nobody's fixed" apart from normal traffic.
- **Idempotency / side effects of the probe request itself**: Chat
  Completions and Responses API text-generation calls are not
  side-effecting by default, but if a candidate model happens to support
  tool calling and the probe includes tools (unlikely needed here since this
  is model-*existence* probing, not capability probing — a plain
  "hi"-shaped request suffices), a probe must never risk invoking a
  real tool. This project's probe shape should be *simpler* than
  `capability.rs`'s (which deliberately includes a tool to test tool-calling
  capability) — keep the model-resolution probe to bare completion, no
  `tools` array at all, to avoid ever risking a tool_call response that some
  overeager downstream logic might act on.
- **Rate-limit interaction with candidate-list walking**: if walking N
  candidates on a cache miss fires N requests in quick succession and the
  Nth one gets rate-limited, is that "candidate N is dead" (wrong — advances
  past a possibly-fine model) or "back off and retry candidate N" (right, per
  Rabbit Hole #3)? The existing `EVAL_PROBE_SPACING_SECS` gap exists
  precisely to avoid a burst of probes reading as correlated/suspicious to
  the upstream's rate limiter — a candidate-list walk with no analogous
  spacing risks self-inflicted 429s that look like "every candidate is
  dead" if 429 isn't handled per Rabbit Hole #3's explicit warning.

## 2. Responses API streaming translation pitfalls

The existing `OpenaiToAnthropicStream` (`src/providers/openai.rs:397-770`,
read in full above) is a hand-rolled state machine over Chat Completions'
flat `chat.completion.chunk` deltas. It works because Chat Completions'
streaming model is simple: one `choices[0].delta` per event, tool-call
fragments keyed by a stable-ish `index`. The Responses API breaks several of
the assumptions this code currently leans on:

- **Different unit of "block".** Chat Completions deltas are keyed by a flat
  `index` into `tool_calls[]`; Responses API deltas are keyed by
  `item_id`/`output_index`/`content_index` against a tree of `output[]`
  items (`message`, `function_call`, `reasoning`, ...), each independently
  added (`response.output_item.added`) and completed
  (`response.output_item.done`). The existing `ToolSlot`
  wire-index-to-block-index remap (`push_tool_delta`,
  `src/providers/openai.rs:518-597`) assumes one flat index space; Responses
  API needs a remap keyed by `item_id` (a string, not a small int) plus
  awareness that items can be `added` before any content arrives. Treat this
  as a new translator, not a patch to the existing one — the requirements
  doc's Rabbit Holes section already says this; the concrete pitfall is
  *reusing the `ToolSlot`/`pending: VecDeque<Bytes>` machinery unmodified*
  and discovering mid-implementation that the indexing model doesn't fit,
  after already committing to the existing struct shape.
- **Reasoning items as a first-class streamed type.** Chat Completions'
  `reasoning_content`/`reasoning` fields (already special-cased at
  `src/providers/openai.rs:694-699` as an OpenRouter-ism folded into plain
  text) are a bolt-on; Responses API's `reasoning` output item is a proper
  typed item with its own `summary`/content parts and its own
  added/delta/done lifecycle. If this project's reasoning-item "passthrough"
  requirement (Success Metrics: "reasoning-item passthrough") means mapping
  to Anthropic's `thinking` content-block type (not plain text), that's a
  new content_block type this translator has never emitted before —
  budget for it explicitly rather than assuming the existing text-block path
  can be reused with a relabeled `type` field, since Anthropic's `thinking`
  blocks carry a `signature` field with its own semantics that plain text
  blocks don't.
- **Error-mid-stream handling divergence.** The current translator's only
  error path is `Poll::Ready(Some(Err(e)))` from `eventsource_stream`
  (transport/parse errors), handled by silently closing the stream with
  `end_turn` (`src/providers/openai.rs:668-671`) — no distinction between "the
  upstream cleanly ended" and "the upstream broke mid-stream." The Responses
  API SSE taxonomy has an explicit `response.failed` / `response.error`
  event type for errors that occur *after* streaming has already started
  (e.g. content-policy trip mid-generation) — something Chat Completions
  mostly doesn't surface this way (errors there are pre-stream HTTP status
  codes). If the new translator doesn't handle `response.failed` in-band, a
  client conducting a coding-agent tool loop across a stream that fails
  midway would either see a truncated response with no visible error (if
  the failure is swallowed like the current transport-error path) or a
  Rust-panic-shaped protocol violation. This needs an explicit "mid-stream
  application-level error" event mapped to something Anthropic clients can
  recognize (an Anthropic `error` SSE event, if the client actually reads
  it, or at minimum a `message_delta` with a distinguishing stop_reason)
  rather than silently downgrading it to a clean `end_turn`.
- **Interleaved item ordering across the wire.** Chat Completions guarantees
  text/tool-call deltas arrive in a single flat delta stream per
  chunk. Responses API can interleave multiple `output_item`s (e.g. a
  `reasoning` item's deltas interleaved with a subsequent `function_call`
  item's deltas) since they're addressed by independent `item_id`s rather
  than a single active generation slot. A translator that assumes "one
  active block, then the next" (which is exactly what `text_started` +
  single-tool-in-progress state implies today) will need genuine
  multi-item concurrent tracking, not just a bigger match statement.
- **`function_call_output` round-trip on the input side** (tool results
  going back to the model) uses a different item shape than Chat
  Completions' `tool` role messages — a translation bug here is easy to
  miss in testing because it only manifests on the *second* turn of a
  multi-turn tool-use conversation (the first turn, model → tool call,
  looks fine; it's turn two, tool result → model, where a wrong shape
  produces a silent 400 or a model that ignores the tool result). Test
  plans must include a full two-turn tool-use round trip, not just "model
  emits a tool call" in isolation.

## 3. Error misclassification: production failure mode if it goes wrong

Requirements already flag *that* this must not happen; here's *what breaks*
if it does, concretely, given this codebase's actual candidate-list shape:

- **Thrash-through-the-list on a single upstream blip.** If a transient 5xx
  or a slow-to-recover rate-limit window is misread as "model dead, advance,"
  and the candidate list has (say) 4 members, a single upstream-wide outage
  (all models on one Model Gateway instance briefly 503ing) doesn't produce
  "wait and retry" — it produces "exhaust the entire candidate list within
  one blip," landing on `ProviderError::Exhausted`
  (`src/providers/mod.rs:134`) and taking the whole route down *harder* than
  doing nothing would have, because now every subsequent request also starts
  from "no known-good candidate" instead of the previously-cached working
  model. The failure amplifies rather than degrades gracefully. This is the
  single most important thing to guard in review: a bug here doesn't just
  fail to fix deprecation, it can turn a 30-second blip into an outage that
  outlives the blip (cache now points at "exhausted," and recovery requires
  either a fixed re-probe interval or another human-noticed manual reset —
  exactly the toil this project exists to remove, reintroduced by the fix
  itself).
- **Cache poisoning across restarts is not in scope but worth naming**: since
  resolution state is explicitly per-process (Non-functional Requirements:
  "no shared/distributed cache needed"), a misclassification-driven
  exhaustion is at least bounded to one process's lifetime — a restart
  clears it. That's a mitigation worth stating explicitly in the plan (it
  bounds the blast radius of this failure mode) rather than leaving it
  implicit.
- **Interaction with existing per-upstream health/circuit-breaking**
  (`src/routing/health.rs`) needs an explicit answer: does a model-resolution
  failure count toward the *upstream's* health score (affecting routing
  decisions elsewhere, e.g. cross-upstream fallback in `router.rs`), or is it
  scoped strictly to the within-upstream candidate list? If unscoped, a model
  resolution false-positive could trip upstream-level health circuitry too,
  compounding the outage across a dimension this project isn't supposed to
  touch (Out of Scope explicitly excludes touching `family.rs`'s — now
  `capability.rs`'s — cross-upstream mechanism). Needs explicit non-crossing
  boundaries in the design, verified by a test that a model-resolution
  failure does *not* move any `src/routing/health.rs` counter.

## 4. Config/schema pitfalls

- **`deny_unknown_fields` is already on `RouteUpstreamRef`**
  (`src/config/schema.rs:150-160`) and it's a flat, non-enum struct (`name`,
  `weight`, `model`, all `Option`), so adding a new optional field (e.g.
  `resolve` or `model_family`) is mechanically safe for backward
  compatibility in the simple case — existing TOML with only `model` set
  continues to deserialize with the new field defaulting to `None`/absent.
  The real pitfall is **mutual exclusivity, not additive presence**: nothing
  today stops a config from setting *both* `model` and the new field
  simultaneously, and `deny_unknown_fields` won't catch that (it only
  rejects fields it's never heard of, not invalid *combinations* of known
  fields). Needs an explicit `validate.rs` rule (a struct-level custom
  validation, since serde's `deny_unknown_fields` can't express "at most one
  of X/Y") for "exactly one of `model` / `<new field>` may be set," with a
  clear error message — otherwise a config that sets both silently picks
  one (whichever the code happens to check first) and an operator has no
  signal they made a mistake.
- **Enum-tagged config sections don't compose with `deny_unknown_fields`
  today** — the existing comment at `src/config/schema.rs:126-128` notes
  serde doesn't support combining `#[serde(flatten)]`-style tagged enums
  with `deny_unknown_fields` on the outer struct, and the codebase already
  worked around this once for `UpstreamKind`. If the new resolution field's
  shape ends up being an enum (e.g. `Static(String)` vs. `Family { prefix,
  ...}` rather than a flat `Option<String>` sibling field), expect to hit
  the exact same workaround-shaped problem again — worth deciding the field
  shape with this constraint in mind up front (a flat `Option<String>` for
  family/prefix, sibling to `model`, sidesteps it entirely; a nested enum
  reintroduces it).
- **Cross-repo compatibility with the ExampleCorp plugin (`ndotfiles`)**: the
  plugin's `50-model-gateway.toml` is a *separate repo*, not covered by this
  repo's CI. A schema change that's additive here can still break that
  config in practice if, e.g., a new *required* (non-`Option`) field is
  accidentally introduced instead of a properly-defaulted optional one, or
  if `references/conf.d/`'s own example TOMLs aren't updated in lockstep and
  drift from what the schema actually accepts (silent doc rot — an operator
  copies the stale example, gets a deploy-time deserialization error with no
  connection back to "the docs were wrong"). The plan should include a
  concrete task: add/update a `references/conf.d/` example exercising the
  new field, and treat it as tested (there's likely already a test that
  loads every file in `references/conf.d/` — verify and extend it rather
  than assuming schema unit tests alone cover config-file compatibility).
- **Schema version skew during rollout**: because the ExampleCorp plugin change
  is explicitly a *separate*, later PR in a different repo (Scope item 6),
  there's a window where this repo's `main` has the new field but the
  plugin config hasn't adopted it yet — fine, since it's additive — but also
  a *reverse* skew risk: if a consolette binary built before this feature
  lands is still running against a config file that's been hand-edited
  ahead of the code update (e.g. by someone testing the ndotfiles side
  first), `deny_unknown_fields` will make the *old* binary hard-fail on the
  *new* field name, rather than ignoring it. That's arguably correct
  behavior (fail loud, don't silently ignore a typo) but should be a
  deliberate, stated tradeoff, not a surprise during rollout sequencing.

## 5. Testing pitfalls: "live catalog that changes over time"

- **The core trap: a mock that encodes today's catalog becomes a test that
  verifies nothing once the real catalog moves on.** If a test hardcodes
  `["gpt-5.1-codex-max", "gpt-5.2-codex", "gpt-5.3-codex"]` as the fake
  `/v1/models` response and asserts resolution picks `gpt-5.3-codex`, that
  test is validating today's snapshot, not the *resolution algorithm*. It
  will keep passing forever regardless of whether the ordering/selection
  logic is actually correct, because the fixture and the assertion were
  derived from the same one-time observation. Tests need to exercise the
  *algorithm's properties* against synthetic, clearly-fake model ids
  (`"family-v1"`, `"family-v2"`, `"family-v3-preview"`) with deliberately
  adversarial orderings/metadata (e.g. `/v1/models` returns them in
  arbitrary order, some with `shutdown_date` set and some without, mirroring
  the real observed bug that `shutdown_date: null` is not a reliable
  deprecation signal) — not against real-looking model names, so nobody is
  tempted to treat "does this match today's real catalog" as the pass
  condition.
- **Feasibility Risk already flags** that integration testing against the
  real Model Gateway requires VPN/SBN-Dev-Agent context and must not be a
  required CI gate — the corollary pitfall is **silent test skip masquerading
  as coverage**: if the real-gateway integration test is `#[ignore]`d or
  gated behind an env var that's never set in CI, it can bit-rot into "code
  that compiles but has never actually run against a real Responses API
  response" without anyone noticing, since the mock-based unit tests will
  keep passing. Mitigate by capturing real request/response fixtures
  *once* (redacted of auth) from an actual manual probe against the gateway
  — recorded JSON fixtures for a real `response.output_item.added` /
  `response.output_text.delta` sequence — and replaying those as the mock
  server's canned responses in the always-run unit/integration tests,
  rather than hand-writing synthetic SSE frames from reading the OpenAI
  docs. Hand-written fixtures reliably miss real-world quirks (field
  ordering, null vs. absent, extra vendor-specific fields) that only a
  captured real response exposes — exactly the kind of gap that produced
  this project's own trigger bug (`shutdown_date: null` being an unreliable
  signal was discovered by hitting the real thing, not by reading the API
  reference).
- **`ScriptedProvider`-style mocks (see `capability.rs:322-401`) can't
  exercise the actual `reqwest`/`eventsource_stream` wire path** — they
  mock at the `Provider` trait boundary, above HTTP. For the Responses API
  SSE translator specifically, testing needs to go one level lower (a real
  local HTTP server, e.g. `wiremock` or a bare `tokio::net::TcpListener`
  handler emitting real `text/event-stream` bytes) so that chunk-boundary
  edge cases (an SSE event split across two TCP reads, a `data:` line with
  no trailing blank line yet) are exercised — bugs that a trait-level mock
  structurally cannot produce, because it hands back whole parsed `Value`s,
  never raw bytes. Confirm whether the existing streaming tests
  (`src/providers/openai.rs`'s own `#[tokio::test]`s, e.g.
  `empty_stream_still_produces_well_formed_bracketing_events`) already do
  this for Chat Completions — if so, that's the pattern to replicate for
  Responses API; if they instead feed pre-chunked `Bytes` directly into the
  stream struct (bypassing real socket/TCP chunking), that's a pre-existing
  gap this project would inherit rather than fix, and worth flagging back to
  the plan as a known limitation rather than assuming coverage exists that
  doesn't.
- **Flakiness from real time in cache-TTL/backoff tests**: `capability.rs`'s
  constants (`EVAL_PROBE_SPACING_SECS`, `EVAL_TTL_SECS`, etc.) are real
  `tokio::time::sleep` calls; tests for them would be intolerably slow or
  flaky under real wall-clock time. Check whether existing tests for that
  module use `tokio::time::pause()`/`advance()` (Tokio's test-time
  facilities) rather than real sleeps — if this project's cache/backoff
  logic follows the same pattern (likely, given the shared TTL-based
  invalidation concept), budget for wiring up paused/advanced virtual time
  in tests from the start, not as an afterthought once tests are already
  slow.

## Summary of what to explicitly design against

1. Single-flight resolution per upstream — never let concurrent in-flight
   requests each independently walk the candidate list on the same cache
   miss.
2. A real HTTP-status-aware error classifier for "advance candidate" vs.
   "transient, stay put" vs. "auth/config problem, definitely don't advance"
   — richer than the existing `error_verdict()`/`ProviderError::Validation`
   split, which currently cannot tell 400 from 401 from 404.
3. A hard, low, explicit token/cost budget for probe requests, with a metric
   that makes probe spend visible separately from production traffic spend.
4. Treat the Responses API streaming translator as new code with its own
   state model (multi-item, `item_id`-keyed, in-band error events) rather
   than a patch to `OpenaiToAnthropicStream`.
5. A schema validation rule for mutual exclusivity between `model` and the
   new field, plus an updated `references/conf.d/` example kept in sync.
6. Recorded real-response fixtures (redacted) as the backbone of streaming
   tests, with synthetic (obviously-fake) model ids for resolution-algorithm
   tests — never real current model names as the thing being asserted on.
