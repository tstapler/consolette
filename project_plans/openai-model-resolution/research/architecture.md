# Research: Architecture — openai-model-resolution

Scope: where dynamic model resolution logic lives relative to the `Provider`
trait boundary, how Responses API translation should be structured relative
to the existing chat/completions path, the probe/cache/invalidate data flow
and its interaction with `Router`/`HealthRegistry`, and where deprecation-vs-
transient error classification belongs. Read in full or by targeted range at
current `main` (`d1a0b68`): `src/providers/openai.rs` (989 lines, most read in
full), `src/providers/mod.rs` (`Provider` trait, `ProviderError`,
`translate_anthropic_request_to_openai`), `src/routing/router.rs`
(`build_providers`, `from_config`, `dispatch`, `already_tried`), `src/routing/
health.rs` (whole file, 146 lines), `src/config/schema.rs` (`RouteUpstreamRef`,
`Route`). `src/routing/family.rs` does **not exist on `main`** — it lives only
on the unmerged `origin/feat/auto-model-family` branch; read there instead
(commit `5aa3059`, `git show origin/feat/auto-model-family:src/routing/family.rs`).
Builds on and cites rather than re-derives: `project_plans/gemini-provider/
research/architecture.md` (§§1-2, 5 — `Provider` contract, `UpstreamKind`
factory, private per-provider `build_headers`), `project_plans/openrouter-
routing/research/architecture.md` (§1, §3 — provider composition/delegation,
strategy-owned per-candidate state).

## 1. Where does dynamic model resolution live?

**Recommendation: inside `OpenaiProvider`, not the router, and not a
decorator.**

The `Provider` trait (`src/providers/mod.rs:155-`, cited in full by gemini
architecture.md §1) is:

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    async fn send(&self, body: Value, headers: HeaderMap, stream: bool)
        -> Result<ProviderResponse, ProviderError>;
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError>;
}
```

`Router::dispatch` (`src/routing/router.rs:478-`) only ever holds
`Arc<dyn Provider>` and calls `.send(body, headers, stream)` once per
candidate — it has no notion of "this call may itself retry across several
model ids before returning." Three candidate locations, weighed:

- **Inside `OpenaiProvider::send`/`send_request`** (this doc's
  recommendation): the resolve-probe-cache-retry loop happens *underneath*
  the `Provider` trait boundary, invisible to `Router`. `Router::dispatch`
  calls `provider.send(..)` exactly once per candidate exactly as it does
  today for every other provider; `OpenaiProvider` internally may issue
  several HTTP requests (to `/v1/models`, then to the resolved model) before
  that one `send()` call returns. This requires **zero changes** to
  `Router::dispatch`, `already_tried`, or `HealthRegistry` — the entire
  resolution mechanism is invisible above the `Provider` trait, matching how
  `RouteUpstreamRef.model` overriding the request body's `model` field
  already works today (`router.rs`'s "dispatch overrides model field" test,
  `router.rs:1346`) as a pure pre-`send()` body mutation. Con: `OpenaiProvider`
  gains responsibility beyond "translate and forward one request," and a
  probe-then-real-request sequence inside one `send()` call means the
  existing per-request timeout budget (`request_timeout_secs`, passed into
  `OpenaiProvider::new`, `openai.rs:71`) has to cover both calls, or resolution
  needs its own shorter timeout carved out of it.
- **A wrapper/decorator around `OpenaiProvider`** (e.g.
  `ResolvingOpenaiProvider` implementing `Provider`, holding an inner
  `OpenaiProvider`): architecturally cleaner separation (resolution logic
  isolated from wire translation), and mirrors the `OpenrouterProvider`-
  wraps-`OpenaiProvider`-helpers precedent from openrouter-routing
  architecture.md §1 ("`OpenrouterProvider` can compose or delegate to
  `OpenaiProvider`'s existing request-building/response-parsing functions").
  But that precedent is about **reusing translation helpers**, not wrapping
  the whole `Provider` impl — and a decorator here still needs `OpenaiProvider`
  to expose enough (its `base_url`, headers, and both the chat/completions and
  Responses API send paths) that the wrapper isn't duplicating HTTP-client
  setup. Given `OpenaiProvider` already privately owns `client`/`stream_client`/
  `base_url`/`resolver`/`exec_cache` (`openai.rs:40-54`) with no public
  accessors, a decorator would need those exposed or would have to hold its
  *own* second `reqwest::Client` pair — real duplication, not composition.
  Verdict: the decorator's separation-of-concerns benefit doesn't pay for
  itself given `OpenaiProvider`'s current field privacy; revisit only if
  `OpenaiProvider` internals grow public accessors for another reason first.
- **In the router** (`build_providers`/`dispatch`): rejected outright. The
  router is explicitly kind-agnostic (openrouter-routing architecture.md §2:
  "not a field on `Router` itself... `Router` is kind-agnostic per ADR-003").
  Model-catalog resolution is an OpenAI-specific concern (calling that
  upstream's own `/v1/models`, understanding OpenAI's family-naming/version
  conventions) that has no meaning for `AnthropicProvider`/`BedrockProvider`/
  `GeminiProvider`. Putting it in the router would require either a new
  `UpstreamKind`-conditional branch in `dispatch` (the "no bespoke dispatch
  path" anti-pattern both prior research docs flag and reject for their own
  provider-specific quirks — gemini architecture.md §3, openrouter-routing
  architecture.md §3.3) or a new `Provider` trait method just for this one
  kind, which breaks the trait's kind-agnostic contract for every other
  implementor.

**Concrete shape**: `OpenaiProvider` gains a new private field, e.g.
`resolution: Option<Arc<ModelResolutionCache>>` (populated only when
`RouteUpstreamRef`'s new opt-in field is set — see §3), and `send_request`/
`send_streaming_request` resolve `body["model"]` through it before building
the URL, exactly parallel to how `Router::dispatch` already overrides
`body["model"]` from `RouteUpstreamRef.model` today — except this resolution
happens **inside** the provider (post-dispatch, pre-HTTP-call) rather than in
`Router::dispatch` (pre-`send()`), since only `OpenaiProvider` knows how to
probe its own `/v1/models` catalog and issue the Responses-vs-chat/completions
decision (§2).

## 2. Responses API translation: sibling module, not one bigger file

`src/providers/openai.rs` is already 989 lines and structurally single-
purpose: HTTP client setup, header-building, one `send_request`/
`send_streaming_request` pair scoped to `POST /v1/chat/completions`, and
`map_error_status` (`openai.rs:297-331`, a private free function — confirming
gemini architecture.md's observation that `map_error_status`-style mapping is
per-provider-private, not a shared `providers/mod.rs` function). The
chat/completions **translation** functions, though, live in `providers/mod.rs`
as shared free functions (`translate_anthropic_request_to_openai`,
`translate_openai_response_to_anthropic`, `translate_openai_to_anthropic`,
etc. — `mod.rs:208-1250`+), not in `openai.rs` itself. `openai.rs` calls into
those shared functions rather than owning translation logic directly (this
research did not trace every call site, but the module boundary is clear:
`openai.rs` = transport, `mod.rs` = Anthropic↔OpenAI-shape translation).

Given the requirements' own warning (Rabbit Holes: "Responses API wire shape
is structurally different... not just a renamed field... translating this to
consolette's existing Anthropic-shaped internal representation is materially
more work"), and the openrouter-routing precedent for module layout under
growth (`src/providers/gemini/` split into `mod.rs`/`error.rs`/`stream.rs`/
`tools.rs`/`translate.rs` once one file wasn't enough, architecture.md §1),
the recommended shape is a **new sibling submodule**, not more lines in the
existing flat files:

- **`src/providers/openai/` becomes a directory** (`openai.rs` → `openai/
  mod.rs`), following the Gemini precedent exactly. This is a mechanical,
  low-risk rename+split (`git mv`), not a rewrite.
- **`src/providers/openai/responses.rs`**: new translation functions
  parallel to (not replacing) `mod.rs`'s chat/completions ones —
  `translate_anthropic_request_to_responses`, `translate_responses_response_
  to_anthropic`, a Responses-specific streaming-event translator (chat's
  `chat.completion.chunk` SSE handling has no shared code with Responses'
  `response.output_item.added`/`response.output_text.delta` taxonomy per the
  requirements' own Rabbit Holes note — there is nothing to factor out
  *before* writing this, because the two SSE grammars don't overlap enough to
  share a helper without one arm becoming a leaky abstraction). Where the
  Anthropic-shaped *domain* concepts are identical (a `tool_use` block is a
  `tool_use` block regardless of which OpenAI-family wire shape it's going
  to/from), the **input side** (Anthropic → OpenAI-shaped) can share small
  pieces with `mod.rs`'s existing `anthropic_blocks_to_openai`/
  `translate_tool_definition`/`sanitize_schema_*` helpers by extracting the
  Anthropic-side normalization (already OpenAI-tool-JSON-Schema shaped,
  independent of which endpoint it's POSTed to) rather than duplicating it —
  but the **output side** (parsing the response back into Anthropic shape)
  genuinely cannot share code, because `choices[0].message` and
  `output[].type == "message"` are structurally different documents, not
  differently-named fields of the same document.
- **`src/providers/openai/mod.rs`** (renamed from today's `openai.rs`) keeps
  transport/header/HTTP-client code and gains one new pair of send methods
  (`send_responses_request`/`send_responses_streaming_request`, mirroring
  today's `send_request`/`send_streaming_request` but posting to `{base_url}/
  v1/responses`), and a decision point — once §1's resolution cache picks a
  model, or once a request targets a model the resolution/compat logic (§4)
  has classified as Responses-only — for whether `send()` routes to the
  chat/completions or Responses path for that request.
- **Do not** put the endpoint-choice branching in `providers/mod.rs`'s shared
  `translate_and_record` (`mod.rs:1351`, the shared entry point used by every
  provider) — that function has no OpenAI-specific knowledge today and adding
  a `UpstreamKind`-conditional there would be the same "no bespoke path
  in shared code" anti-pattern flagged in §1's rejection of router-level
  resolution. The choice belongs inside `OpenaiProvider::send`, which already
  is the OpenAI-specific transport layer.

This produces a structure where "one file growing" is avoided, the two wire
protocols never leak into each other's translation code, and the *shared*
Anthropic-domain-shaped helpers in `mod.rs` are reused for the input side
without forcing a shared abstraction onto the incompatible output side.

## 3. Data flow: probe → classify → cache → invalidate

### 3.1 Config entry point

The new opt-in field lands on `RouteUpstreamRef` (`src/config/schema.rs:152-
161`), alongside the existing `model: Option<String>`:

```rust
pub struct RouteUpstreamRef {
    pub name: String,
    pub weight: Option<f64>,
    pub model: Option<String>,       // existing: static pin
    pub model_family: Option<String>, // new: opt-in family/prefix (exact
                                       // syntax deferred to sdd:3-plan per
                                       // requirements' Open Questions)
}
```

Both fields being `Option` and mutually-informative (a config with both set,
or neither, needs a `validate.rs` rule — see below) is a `src/config/
validate.rs` addition, not a schema-level enum choice, because `RouteUpstreamRef`
is `#[serde(deny_unknown_fields)]` flat struct (schema.rs:150-151), matching
the existing precedent of `model` being a plain optional field rather than a
tagged variant.

### 3.2 Cache ownership and keying

Per this project's Non-Functional Requirements ("resolution state is
per-process, per-upstream; no shared/distributed cache needed") and per §1's
recommendation that resolution lives inside `OpenaiProvider`, the cache is:

- **Owned by `OpenaiProvider`** (constructed once in `OpenaiProvider::new`,
  alongside `client`/`exec_cache` — same lifetime as the provider, which
  itself lives exactly as long as `Router` does, since `build_providers`
  constructs one `Arc<dyn Provider>` per configured upstream and never
  rebuilds it except on a full config reload).
- **Keyed by family string** (`RouteUpstreamRef.model_family`'s value), not
  by route or by upstream index — a `DashMap<String, ResolvedModel>` inside
  the provider, where `ResolvedModel` holds the currently-winning model id
  plus enough state to know when to re-probe. One `OpenaiProvider` instance
  can serve multiple routes that all reference the same upstream with
  different (or the same) family strings, so keying by family string (not
  by `(route, upstream)`) naturally dedupes: two routes both resolving
  `"gpt-5-codex"` on the same upstream share one cached winner and one probe
  history, avoiding redundant `/v1/models` calls and redundant "try candidate
  N" HTTP round-trips for what is, from the upstream's point of view, the
  exact same question asked twice.
- This is a **narrower, purpose-built structure**, not a generalization of
  `HealthRegistry` (index-keyed, whole-upstream granularity — wrong
  granularity here, same mismatch openrouter-routing architecture.md §3.1
  already identified and rejected for its own per-model tracking need) and
  not a reuse of the family.rs `FamilyTable`/`FamilyRuntime` machinery (that
  system resolves *declared* named aliases across upstreams the operator
  configured by hand; this project resolves *undeclared* candidates
  discovered live from one upstream's own `/v1/models` response — the
  candidate list itself isn't known until a network call happens, unlike
  `FamilyEntry.members`, which is populated straight from config at
  `FamilyTable::from_config` time with zero I/O).

### 3.3 The probe/classify/cache/invalidate loop, and its structural template

`family.rs`'s denylist mechanism (only on `origin/feat/auto-model-family`,
not `main` — `git show origin/feat/auto-model-family:src/routing/family.rs`)
is the closest existing precedent for "a failure classification result feeds
a cache that gates future candidate selection, with TTL-based recovery," even
though it operates one layer up (across upstreams, not within one upstream's
catalog — Out of Scope §51 of requirements.md already rules out unifying with
it). Its doc comment (`family.rs:486`, per this research's `git show`) states
the pattern this project should mirror at the OpenaiProvider level:

> "`is_validation` with no failover... that failure feeds the denylist, so
> requests N+1.. skip the dead ID until the 1h TTL expires."

Concretely, this project's own loop:

1. **First request (or first after cache invalidation) for a given family**:
   `OpenaiProvider` calls its own `fetch_models()` (already exists,
   `openai.rs:188-213`, hits `GET /v1/models`), filters/orders candidates
   matching the family (newest-first, per requirements §41), and tries them
   in order via real requests until one returns success or the list is
   exhausted. Each attempt's response is run through the classification
   logic in §4; only a "deprecated/wrong-endpoint"-classified failure
   advances to the next candidate — a transient failure on the *first probe
   attempt* should not burn through the whole candidate list in one request
   (this needs its own bounded retry-vs-abort rule, flagged for `sdd:3-plan`,
   not resolved here).
2. **Cache write**: on the first success, the winning model id (and, if
   §4's endpoint classification determined chat/completions vs. Responses,
   that decision too) is written into the `DashMap<String, ResolvedModel>`
   entry for that family — this is the only state write, and it happens
   after all `.await`s for that resolution round have completed, matching
   `health.rs`'s "never hold a guard across `.await`" discipline (a `DashMap`
   read for the cache-hit fast path, and a separate write once resolution
   finishes, never both under one held reference across a network call).
3. **Steady state (cache hit)**: every subsequent request for that family
   reads the cached model id synchronously (or via one uncontended `DashMap`
   lookup) and skips straight to building the real request — **zero
   resolution overhead**, satisfying the Constraints section's "must not
   introduce real per-request cost/latency overhead."
4. **Invalidation on failure**: when a real (non-probe) request using the
   cached model id fails with a §4 "deprecated" classification, the cache
   entry is invalidated (or replaced with the next candidate directly, if
   the provider still has a live ordered candidate list from the last full
   resolution — cheaper than re-fetching `/v1/models` from scratch) and
   resolution re-runs on the **next** request for that family. This is
   "invalidate on failure, not fixed TTL" per the Non-Functional
   Requirements — no background timer, purely reactive, matching `family.rs`'s
   denylist being populated by dispatch-time classification rather than a
   scheduled sweep.
5. A secondary, longer TTL (family.rs's "1h" is the cited precedent value,
   not necessarily this project's chosen number) as a safety net so a family
   whose winning model gets silently un-deprecated, or whose catalog changes
   upstream in a way that isn't observable via failure (e.g. a genuinely
   *better* newer model becomes available and nothing is currently failing),
   eventually gets re-checked — deferred to `sdd:3-plan` as an explicit
   choice, not assumed.

### 3.4 Interaction with `Router`'s cooldown/health tracking

Because §1 places resolution entirely inside `OpenaiProvider::send()`
(invisible above the `Provider` trait boundary), **`Router::dispatch`,
`already_tried`, and `HealthRegistry` need no changes and do not interact
with this mechanism at all** — a key structural difference from both prior
providers' research docs, which each found a real gap requiring
`Router`/`HealthRegistry` changes (gemini architecture.md §3's cooldown-on-
schema-drift gap; openrouter-routing architecture.md §3.3-3.4's per-model
hard-exclusion and `already_tried` widening). Those gaps existed because
each of those features needed the **router's** selection process to be aware
of sub-upstream granularity (many models sharing one upstream index,
requiring `Router` itself to pick among them via `RoutingStrategy`). This
project's resolution never surfaces a choice to `Router` — from `Router`'s
perspective, `OpenaiProvider::send()` either succeeds or returns one
`ProviderError`, exactly as it does today; the fact that `send()` internally
tried three model ids before returning is not `Router`'s concern.

One consequence worth flagging explicitly for `sdd:3-plan`: if *every*
candidate in a family is exhausted (§ observability requirement "resolution
exhausts all candidates... no working model"), `OpenaiProvider::send()`
still needs to return **some** `ProviderError` to `Router::dispatch`, which
will then apply its normal error-class handling (failover to the next
`RouteUpstreamRef` in the route, or fail the request if none remain) —
resolution exhaustion inside one upstream should probably classify as
`ProviderError::Upstream{..}` (or a new, explicit "exhausted" variant, see
§4) so it correctly triggers `Router`'s existing failover-without-cooldown
path rather than being silently swallowed inside the provider.

## 4. Error classification: deprecated vs wrong-endpoint vs transient

### 4.1 What's actually observable today, and why today's mapping is a blocker

`OpenaiProvider`'s current `map_error_status` (`openai.rs:297-331`) collapses
**every** non-429, non-2xx client error into one bucket:

```rust
if status.is_client_error() {
    let status_u16 = status.as_u16();
    let body_str = response.text().await.unwrap_or_default();
    return Err(ProviderError::Validation(body_str, status_u16));
}
```

`ProviderError::Validation` is one of the two error classes `Router::dispatch`
treats as **non-failover, return immediately** (per gemini architecture.md
§3's citation of the dispatch match: `Err(e) if e.is_validation() || e.is_auth()
=> return Err(e)`). This is a direct blocker for this project as currently
written: the deprecated-model 400 from requirements.md's own motivating
incident ("`gpt-5.1-codex-max`... 400 invalid_request_error... has been
deprecated") and the wrong-endpoint 404 both arrive as generic client errors
today, both get boxed into `Validation`, and both would **short-circuit
`Router::dispatch` with no failover** under current behavior — exactly
backwards from what resolution needs (advance to the next candidate). The
error body (already captured as `body_str` before being discarded into
`ProviderError::Validation(body_str, status_u16)`) **does** carry the
information needed to distinguish these cases — OpenAI-shaped error bodies
are `{"error": {"message": "...", "type": "...", "code": "..."}}`, and this
incident's actual observed messages ("has been deprecated", "Use the
v1/responses endpoint instead") are exact-string-matchable today; the problem
is that this string is currently thrown away rather than classified.

### 4.2 Where classification logic should live

**Recommendation: a new function private to `openai.rs`/`openai/mod.rs`,
not a shared function in `providers/mod.rs`.** This mirrors the confirmed
precedent that `map_error_status` itself is already OpenAI-private (not a
shared `mod.rs` function reused by `AnthropicProvider`/`GeminiProvider`) —
the error *taxonomy* being classified (`type: "invalid_request_error"`,
specific message substrings) is OpenAI-API-shaped and has no meaning for
other providers' error bodies. Concretely:

- A new private enum or set of match arms inside `openai.rs`/`openai/mod.rs`,
  e.g. `enum OpenaiErrorClass { Deprecated, WrongEndpoint, Transient, Other }`,
  populated by parsing the JSON error body already being read in
  `map_error_status` (today discarded into a raw string — needs to become a
  parsed `serde_json::Value` first, string match second).
- Classification signal, from what's actually knowable in a real OpenAI-
  shaped error response (per this project's own Rabbit Holes and Open
  Questions, this needs confirming against real captured responses in
  `sdd:2-research`'s protocol-research agent, not assumed here from this
  architecture pass alone — but structurally):
  - **Deprecated**: `error.type == "invalid_request_error"` combined with a
    message substring like "has been deprecated" or "decommissioned" — status
    400, not 404, and not a shape the current code distinguishes from any
    other 400.
  - **Wrong-endpoint**: `error.type == "invalid_request_error"`, status 404
    (not 400), and a message substring like "not supported in the
    v1/chat/completions endpoint" / "Use the v1/responses endpoint instead"
    — this is the signal that should cause `OpenaiProvider` to **retry the
    same model id against `/v1/responses` instead of advancing to the next
    candidate**, a materially different reaction than "deprecated" (which
    should advance the candidate list, not switch endpoints on the same id).
  - **Transient**: rate limiting is already correctly isolated (429 →
    `ProviderError::RateLimited`/`RateLimitedWithRetry`, unchanged by this
    project) and 5xx already falls into `ProviderError::Upstream{..}`
    (`is_transient()` → true, per `mod.rs`'s classification, cited in gemini
    architecture.md §1) — both already correctly excluded from the
    "advance the candidate list" trigger *if* resolution logic explicitly
    only reacts to the new Deprecated/WrongEndpoint classes and treats
    everything else (including today's generic `Validation` bucket for
    truly-malformed-request 400s that aren't deprecation) as "do not
    advance, surface the error" — this is the requirements' Rabbit Holes
    concern ("must not treat a rate-limit or transient 5xx as 'this model is
    dead'") already satisfied by construction as long as the new
    classification is additive (a new, narrower carve-out) rather than a
    reinterpretation of the existing 429/5xx paths.
- **A hardcoded list of message substrings is the realistic v1 approach**,
  per this project's own Open Questions ("hardcoded list of OpenAI error
  code/type values or something more general — defer to Phase 2 research").
  This architecture research cannot resolve which specific strings/codes are
  stable across OpenAI-compatible gateways (that's the protocol-research
  agent's job) — but structurally, wherever that list ends up, it belongs as
  a `const`/match table colocated with `map_error_status` in `openai.rs`, not
  scattered across the resolution-cache code, so one place owns "how do we
  recognize a deprecated-model response from this API family."
- `ProviderError` itself likely needs **no new variant** for this — the
  classification can happen *before* constructing the `ProviderError` (i.e.
  `map_error_status` or a caller of it inspects the parsed body and decides
  "advance candidate" vs. "return `Validation` upward" internally, only
  ever handing `Router::dispatch` the same fixed `ProviderError` vocabulary
  it already knows how to branch on). This keeps `ProviderError`'s "fixed
  vocabulary, no provider-specific variants" contract (gemini architecture.md
  §1: "there's no room for a Gemini-specific error variant") intact for this
  project too — the new classification is consumed entirely inside
  `OpenaiProvider`'s resolution loop, never exposed as a new cross-provider
  error type.

## 5. Summary of open architectural questions carried into `sdd:3-plan`

- **§1**: resolution lives inside `OpenaiProvider::send()`, decorator
  rejected given current field-privacy, router rejected as kind-agnostic-
  breaking. Confirm at plan time whether the per-request timeout budget
  needs splitting between probe and real-request phases.
- **§2**: `openai.rs` → `openai/mod.rs` + `openai/responses.rs` split;
  confirm exact extraction boundary for input-side (Anthropic→OpenAI-shaped)
  helpers that can be shared between chat/completions and Responses paths
  vs. the output-side parsers that cannot.
- **§3.3**: bounded-retry rule for a transient failure during the *initial*
  resolution probe sequence (must not burn the whole candidate list on one
  flaky request) — not resolved here, flagged for plan.
- **§3.3**: secondary TTL safety-net value and whether it's needed for v1 or
  deferred — `family.rs`'s "1h" is a precedent value, not a decision.
- **§3.4**: exhaustion-signaling `ProviderError` variant choice (reuse
  `Upstream{..}` vs. a new explicit variant) so `Router::dispatch`'s existing
  failover-without-cooldown path fires correctly when a family's candidates
  are all exhausted.
- **§4**: exact error `type`/`code`/message-substring signals for
  Deprecated/WrongEndpoint classification — needs real captured OpenAI (and
  ExampleCorp Model Gateway) error response bodies from `sdd:2-research`'s
  protocol-research agent; this document only establishes *where* the logic
  goes and *that* the current `Validation`-catch-all is a real blocker to fix
  first.
- **`max_completion_tokens` compatibility (Scope item 3)**: not covered in
  depth by this architecture pass (assigned to a different research
  dimension per the requirements' Open Questions), but structurally the
  `cohere_subset` model-id-substring-matching precedent already in
  `translate_anthropic_request_to_openai` (`mod.rs:~715`,
  `model.to_lowercase().contains("cohere")`) is a directly reusable pattern
  for gating `max_tokens` vs. `max_completion_tokens` off the resolved model
  id/family string once §1's resolution has picked one — worth flagging as
  precedent for whichever research/plan phase owns that decision.
