# Implementation Plan: openai-model-resolution

**Feature**: Opt-in, per-upstream dynamic model resolution for `kind = "openai"` upstreams (auto-discovering and failing over across a live `/v1/models` catalog when a pinned model deprecates), plus the Responses API parity and `max_completion_tokens` compatibility work required to make newer candidate models reachable.
**Date**: 2026-09-18
**Status**: Ready for implementation
**ADRs**: ADR-001 (resolution lives inside `OpenaiProvider`), ADR-002 (candidate ranking heuristic), ADR-003 (error-classification seam, `map_error_status`/`ProviderError` unchanged)

---

## Domain Glossary

| Term | Definition | Notes |
|------|-----------|-------|
| `ModelFamily` | The value of `RouteUpstreamRef.model_family` — a literal prefix string identifying a set of candidate model ids on one upstream (e.g. `"gpt-5"` matching `gpt-5.1-codex-max`, `gpt-5.2-codex`, `gpt-5.3-codex`). | `Option<String>` on `RouteUpstreamRef`, sibling to the existing `model: Option<String>` field. Exactly one of `model`/`model_family` may be set. Reaches `OpenaiProvider` **per-request**, not at construction — see Epic 1.3: `build_providers` (`src/routing/router.rs:75-131`) has no `RouteUpstreamRef` in scope, so the value is threaded through the same per-dispatch channel `model` itself already uses (`Router::dispatch` body mutation, `router.rs:531-539`), via an internal-only body key `OpenaiProvider::send` strips before forwarding upstream. |
| `CandidateModel` | One model id returned by `OpenaiProvider::fetch_models()` whose id starts with the active `ModelFamily` prefix. | Plain `String` in code; the term exists for prose/test-name consistency, not a wrapper type. |
| `ResolvedModel` | The cache entry for one `ModelFamily`: the currently-winning `CandidateModel` id, its `Endpoint`, its `TokenParamStyle`, and the timestamp of last (re-)resolution. | Struct, not a newtype — holds four related fields written together (ADR-002 combines model-id and compat-flag resolution into one probe per pitfalls.md §Edge case 5). |
| `ResolutionCache` | `DashMap<String, ResolvedModel>` owned unconditionally by every `OpenaiProvider` instance, keyed by `ModelFamily` string. | Modeled on `src/routing/capability.rs`'s `CapabilityCache`, not `openrouter/cache.rs`'s single-slot `moka::sync::Cache` — per-family keying, not a singleton. Always constructed (never `Option`); a static-`model` upstream's cache simply never receives a key because no dispatched request for it ever carries the internal `model_family` body key (Epic 1.3) — this is what makes the zero-overhead constraint hold without a construction-time flag. One `OpenaiProvider` instance is always scoped to exactly one upstream (never shared/pooled across upstreams — `build_providers` constructs one per `config.upstreams` entry, `router.rs:75-131`), so a bare `ModelFamily`-string key cannot collide across upstreams even though the key omits the upstream name. |
| `OpenaiErrorClass` | Sum type: `Deprecated \| WrongEndpoint \| Transient \| Other`, produced by `classify_openai_error(status, body)` from a parsed OpenAI error envelope. | Internal to the resolution module (ADR-003) — never exposed as a `ProviderError` variant. |
| `Endpoint` | Sum type: `ChatCompletions \| Responses` — which OpenAI wire endpoint a `ResolvedModel` must be sent to. | Set once during resolution when a `WrongEndpoint` classification is observed for a candidate; otherwise defaults to `ChatCompletions`. |
| `TokenParamStyle` | Sum type: `MaxTokens \| MaxCompletionTokens` — which token-limit body field a `ResolvedModel` needs. | Determined by the same probe attempt that resolves the model id (see Story 4.1.1) — one combined probe, one combined cache write, not two independently-invalidated caches. |
| `ResolutionOutcome` | Sum type returned by the per-candidate probe/real-request classification step: `Success \| AdvanceCandidate \| RetrySameCandidateAsResponses \| Transient \| Exhausted`. | Drives the resolution loop's control flow (Story 2.3.2); `Transient` never advances the candidate list. |
| `ResolutionState` | Sum type for dashboard/observability rendering: `Newest \| Fallback \| Exhausted`. | `Newest`/`Fallback` render calm (green/amber); `Exhausted` renders loud and sticky (red), per ux.md §3. |
| `ProbeAttempt` | One record of "tried candidate X, got outcome Y, at time T" — the unit the per-resolution-attempt counter (Observability Requirements) increments on. | Not persisted beyond the counter increment; the cache only remembers the winner, not full history. |
| `SingleFlightGuard` | Per-`ModelFamily` `Arc<AtomicBool>` compare-exchange guard, wrapped by a `SingleFlightPermit` RAII type, preventing concurrent redundant candidate-list walks on the same cache miss/invalidation. | Modeled directly on `openrouter/cache.rs`'s `ModelListCache` single-flight re-fetch guard. The permit's `Drop` impl always clears the flag and notifies waiters — guarantees release on early return, `?`-propagated error, or panic-unwind (this crate does not set `panic = "abort"`; verified no such setting in `Cargo.toml`), so a stuck resolver task can never permanently starve a family (adversarial-review.md Blocker 5). |
| `ResolutionBackoff` | Per-`ModelFamily` "don't retry resolution before T" marker written after a walk ends `Exhausted` or `fetch_models()` fails, distinct from `ResolvedModel`'s positive cache. | A short (e.g. 30s) negative-cache window so sequential (non-concurrent) requests during a sustained outage fail fast instead of each paying a full candidate-walk cost (adversarial-review.md Blocker 2 — `SingleFlightGuard` alone only dedupes *concurrent* callers). |
| `ItemId` | The Responses API's string-typed output-item identifier (`response.output_item.added`'s `item.id`), addressing entries in a tree of `output[]` items. | Contrasts with Chat Completions' small-int `tool_calls[].index`; `ResponsesToAnthropicStream` tracks state keyed by `ItemId`, not a flat index. |
| `ResponsesToAnthropicStream` | New `Stream` impl (parallel to `OpenaiToAnthropicStream`) translating Responses API SSE events into Anthropic-shaped SSE events. | Lives in `src/providers/openai/responses.rs`; shares no state-machine code with `OpenaiToAnthropicStream` (per pitfalls.md §2 — the indexing models are structurally incompatible). |

---

## Pattern Decisions

| Component | Pattern Chosen | Source | Alternative Rejected | Reason |
|-----------|---------------|--------|---------------------|--------|
| Per-request `model_family` delivery (`Router` → `OpenaiProvider`) | Internal-only body-key mutation at `Router::dispatch`'s existing `body["model"]` mutation point (`router.rs:531-539`), stripped by `OpenaiProvider::send` before forwarding upstream | Mirrors this codebase's existing `model` per-dispatch channel | (a) Widen the `Provider` trait's `send()` signature with a new parameter; (b) thread `model_family` through `OpenaiProvider::new` at `build_providers` construction time | (b) is structurally impossible: `build_providers` (`router.rs:75-131`) constructs one provider per `config.upstreams` entry before any `RouteUpstreamRef`/route is in scope, and one `Upstream` can be referenced by multiple `RouteUpstreamRef`s with different families (architecture-review.md Blocker). (a) works but forces all five `Provider` impls to accept/ignore a parameter only `OpenaiProvider` uses — the body-key approach reuses a channel that already crosses this exact boundary for the exact same purpose (`model`), with zero trait-signature churn. |
| Resolution loop (`OpenaiProvider` internals) | Transaction Script (probe → classify → cache → retry, procedural) | PoEAA (Fowler) | Decorator (`ResolvingOpenaiProvider` wrapping `Provider`) | `OpenaiProvider`'s `client`/`stream_client`/`base_url` fields are private with no accessors (ADR-001); a decorator would need them exposed or duplicate a second HTTP client pair. |
| Resolution loop location | Extend `OpenaiProvider::send`, not `Router::dispatch` | ADR-001 | Router-level `UpstreamKind`-conditional branch | `Router` is explicitly kind-agnostic; model-catalog resolution has no meaning for non-OpenAI providers. |
| `ResolutionCache` | In-memory keyed cache (`DashMap<String, ResolvedModel>`) | Repository-flavored (PoEAA), modeled on `src/routing/capability.rs`'s `CapabilityCache` | `moka::sync::Cache` single-slot builder (`openrouter/cache.rs`'s `ModelListCache` shape) | Resolution is keyed per-family across potentially many upstreams/routes; a single-slot cache is the wrong granularity (same mismatch openrouter-routing research already rejected for its own per-model tracking need). |
| `OpenaiErrorClass` | Sum type / exhaustive match, no default arm on the four variants | type-driven-design | Reuse/widen `ProviderError::Validation` with a structured field | Keeps `ProviderError`'s fixed, provider-agnostic vocabulary intact (ADR-003) — ProviderError has "no room for a provider-specific variant" per existing precedent. |
| `Endpoint`, `TokenParamStyle` | Newtypes/sum types, not `bool`/raw `&str` | type-driven-design | `bool needs_responses_api`, `bool needs_max_completion_tokens` | A `bool` pair can't express "haven't determined yet" (`Unknown`) without a third out-of-band flag; sum types make the "not yet probed" state a real, exhaustively-matched case. |
| Concurrent re-resolution | Single-flight guard (`Arc<AtomicBool>` compare-exchange behind an RAII `SingleFlightPermit`), modeled on `openrouter/cache.rs`'s `ModelListCache` | Existing in-repo precedent | Let every concurrent caller independently walk the candidate list on a cache miss | Pitfalls research: N concurrent requests each independently probing on a shared-cache miss is a self-inflicted probe storm against an upstream that's already unhappy. RAII release (not a manual flag-clear at the end of the happy path) is required so an early return, `?`-propagated error, or panic during the walk can't leave the guard permanently held (adversarial-review.md Blocker 5). |
| Sequential re-resolution during an outage | Short negative-cache/backoff window (`ResolutionBackoff`, Story 2.2.3), separate from `SingleFlightGuard` | New for this project | Rely on `SingleFlightGuard` alone | The single-flight guard only dedupes *concurrent* callers; it does nothing for one request at a time each re-walking a cold cache during a sustained outage, which is worse than today's static-pin fail-fast behavior (adversarial-review.md Blocker 2). |
| `ResponsesToAnthropicStream` | New, independent `Stream` impl (Adapter-shaped: translates one wire grammar into another) | GoF (Adapter) | Extend/patch the existing `OpenaiToAnthropicStream` to branch on taxonomy | Chat Completions' flat `tool_calls[index]` and Responses API's `item_id`-tree addressing are structurally incompatible; forcing one struct to cover both produces a leaky, unmaintainable abstraction (pitfalls.md §2). |
| `src/providers/openai.rs` module layout | Split into `openai/mod.rs` + `openai/responses.rs` directory | Existing in-repo precedent (`src/providers/gemini/`'s `mod.rs`/`error.rs`/`stream.rs`/`tools.rs`/`translate.rs` split) | Keep growing the single 989-line `openai.rs` file | Two independent wire-protocol translators in one file was already flagged as a maintainability risk in architecture research; matches an established, low-risk mechanical precedent (`git mv`, not a rewrite). |

---

## Tech Debt Disposition

| Area | Existing Issue | Disposition | Justification |
|------|----------------|--------------|----------------|
| `src/providers/openai.rs:297-331` (`map_error_status`) | Collapses every non-429 4xx status (400/401/403/404/422) into one `ProviderError::Validation` bucket, discarding the parsed OpenAI error envelope — `Router::dispatch` treats `Validation` as non-failover, so this is a live blocker for resolution's "advance past a dead candidate" behavior. | Isolate via seam | ADR-003: a new resolution-only `classify_openai_error()` function re-parses the same response body independently; `map_error_status`/`ProviderError::Validation` are left byte-for-byte unchanged for every non-opted-in (static `model` pin) caller, satisfying the zero-regression requirement without touching the shared, multi-provider `ProviderError` contract. |
| `src/routing/capability.rs:204-218` (`error_verdict`) | Has the identical latent bug (collapses `Validation` regardless of the underlying 400/401/403/404 distinction) — pitfalls.md confirms this is "exactly the failure mode Rabbit Hole #3 warns about, already sitting live in this codebase." | Extend as-is | Out of this project's scope (`auto-model-family`'s cross-upstream capability-eval mechanism, not this project's within-upstream resolution — requirements.md's Out of Scope explicitly excludes touching it). Not touched by any story below; flagged here only so a reviewer doesn't assume this project silently fixes it too. |
| `src/providers/openai.rs` (989 lines, pre-split) | Single flat file already mixing HTTP transport, header-building, and one wire-protocol's translation-adjacent glue; adding two more structurally distinct concerns (dynamic resolution, and a second wire protocol — Responses API) would make this worse. | Refactor-first, done once, up front | Epic 1.4 performs the `openai.rs` → `openai/mod.rs` + `openai/resolution.rs` + `openai/responses.rs` split as the *last* task of Phase 1 (before any Phase 2 resolution code or Phase 3 Responses API code is written), so both new concerns land directly in their own module from their first line instead of being written flat and split apart later. This also closes the architecture-review.md/adversarial-review.md Concern that the original plan's Epic 3.1 split only carved out Responses API code, leaving resolution logic accreting onto `openai/mod.rs` for all of Phase 2/4. Epic 3.1 (Phase 3) is now a no-op reference back to Epic 1.4, not a second split. |

---

## Migration Plan
N/A — no database schema or persisted-data changes. The new `model_family` config field is a purely additive TOML field (see Risk Control); `ResolutionCache` is in-memory, per-process, and never persisted.

## Observability Plan
- **Logs**: `tracing::info!` at each resolution round start (family, upstream, candidate count) and each candidate outcome (`tracing::warn!` on `AdvanceCandidate`/`RetrySameCandidateAsResponses`, `tracing::error!` on `Exhausted`); `tracing::debug!` on cache-hit fast path (rate-limited/sampled if this proves noisy — decide during implementation).
- **Metrics** (`src/metrics/counters.rs`, extends `UpstreamCounters`):
  - `resolution_attempts_total{upstream, family, candidate, outcome}` — one increment per `ProbeAttempt` (Story 5.1.1).
  - `resolution_exhausted_total{upstream, family}` — increments once per exhaustion event, feeds the dashboard's sticky red state (Story 5.1.2).
  - `resolution_probe_tokens_total{upstream, family}` — cumulative token spend attributable to probe traffic, separated from production traffic spend (pitfalls.md §1 cost-visibility need).
  - Existing `UpstreamCounters::last_error_kind` gets no new variant — resolution failures are absorbed inside `OpenaiProvider`, never surfaced to `Router`'s error-kind attribution unless every candidate is exhausted (Story 2.3.4).
- **Alerts**: no new paging alert. `resolution_exhausted_total` incrementing is the dashboard-visible, human-must-notice signal (Story 5.2.2's sticky red `status-resolution-exhausted` state) — consistent with this feature's Constraints ("not a production outage... no rushed/unreviewed path").

## Risk Control
- **Feature flag**: `model_family` being unset on a `RouteUpstreamRef` *is* the flag — default is off (existing static `model` pin behavior, unchanged). No separate boolean flag needed.
- **Rollback procedure**: unset `model_family` in the offending route's TOML, revert to a static `model` pin; no code rollback needed since the mechanism is entirely additive and self-contained inside `OpenaiProvider`.
- **Staged rollout**: full rollout on merge — the feature is opt-in per-route, so merging it changes nothing for any config that doesn't set the new field (including `references/conf.d/00-providers.toml`'s existing examples and the ExampleCorp plugin's current pin, until that separate repo's follow-up PR adopts it).

## Unresolved Questions
- [ ] Exact deprecation/wrong-endpoint message substrings beyond the two already captured in requirements.md ("has been deprecated", "Use the v1/responses endpoint instead") — needs a small corpus of real captured error bodies from the ExampleCorp Model Gateway (via the SBN Dev Agent) to harden `classify_openai_error`'s match table beyond the two known cases. Blocks full test coverage of Story 1.1.2 (specifically Task 1.1.2c, added per pre-mortem.md P1 #1), not its structure. This is an environment/access dependency, not a design gap — the plan's structure (data-driven table, `Other`-classification counter as a fallback safety net) does not change if the fixture can't literally be captured during planning; it only means Task 1.1.2c's real-fixture entry ships during `sdd:5-implement` instead of being pre-populated now. — owner: implementer, during `sdd:5-implement`, with SBN Dev Agent/VPN access.
- [ ] Exact secondary TTL safety-net duration for `ResolvedModel` (a good-choice re-check independent of failure-triggered invalidation) — `family.rs`'s cited "1h" is a precedent value, not a validated one for this project's traffic pattern. Proposed default: 1 hour, exposed as an internal constant (not new config surface) so it's a one-line change if wrong. — owner: implementer, Story 2.3.3, confirm no `sdd:2-research` re-derivation needed before shipping.
- [ ] Whether operators need a config-level per-candidate denylist (features.md's "operator wants to exclude one known-bad candidate without losing auto-recovery for the rest") — explicitly deferred, not built in this project. Flagging as a likely fast-follow, not blocking any story here.

**Resolved during this planning pass** (were open in requirements.md, now settled by architecture research + this plan):
- "Does 'recovers within N requests' mean one client request internally walks the whole candidate list, or N separate client requests each absorbing one candidate's failure?" → **Resolved: one request walks the whole list.** Per ADR-001, resolution happens synchronously inside a single `OpenaiProvider::send()` call on a cache miss/invalidation — the triggering request pays the full candidate-walk cost itself (§3.3 in architecture research); subsequent requests hit the warm cache. This is a deliberate, stated tradeoff (see Story 2.2.2's single-flight guard), not an accident.
- "How does `model_family` actually reach `OpenaiProvider` per-request, given `build_providers` constructs providers before any route is in scope?" → **Resolved: internal-only body-key mutation at the same point `Router::dispatch` already mutates `body["model"]`** (see Epic 1.3, Pattern Decisions table). Chosen over widening the `Provider` trait's `send()` signature because it needs zero changes to the other four `Provider` implementations. See architecture-review.md's Blocker for the full analysis this closes.
- "Should concurrent callers on a single-flight cache miss block-and-wait for the winner, or fall back to a stale/no-cache attempt?" (previously deferred to "Task 2.2.2b's decision") → **Resolved: bounded wait, then stale-or-propagate.** Concurrent callers await the winner's `Notify` for `min(remaining request budget, a 5s ceiling)`; on notify or timeout they re-check the cache — a populated entry (the winner just wrote one, or a still-valid stale entry) is used directly, otherwise the caller propagates a transient error rather than starting a second independent walk. See Story 2.2.2's revised acceptance criteria (adversarial-review.md Concern).

## Dependency Visualization
```
Phase 1: Foundation (no dependency on Responses API work)
  Epic 1.1 Error Classification
  Epic 1.2 Config Schema
  Epic 1.3 Per-Request model_family Threading (Router → OpenaiProvider)
    -- REQUIRED before Epic 2.2/2.3 can start: without Epic 1.3's body-key
       channel, OpenaiProvider has no per-request way to learn which family
       to resolve (architecture-review.md Blocker) --
  Epic 1.4 Module Restructure (openai/mod.rs + resolution.rs + responses.rs)
    -- done last in Phase 1, before Phase 2/3 write any new code into the
       module they each own --
  All of Phase 1 ──► Phase 2: Resolution Core
                        Epic 2.1 Candidate Discovery/Ranking (incl. fetch_models
                                  failure handling, Story 2.1.3)
                        Epic 2.2 Resolution Cache (incl. single-flight guard
                                  w/ RAII release, Story 2.2.2; negative-cache
                                  backoff, Story 2.2.3)
                              ──► Epic 2.3 Probe/Walk Loop (incl. 429 handling,
                                  Story 2.3.2; inter-candidate spacing, Story 2.3.5)
                                        │
                                        ▼
                        Phase 4: max_completion_tokens (Epic 4.1) ◄── shares the
                                  combined-probe design from Epic 2.3

Phase 3: Responses API (independent of Phase 2/4 until Epic 3.6)
  Epic 3.1 Module split ──► Epic 3.2 Non-streaming ──► Epic 3.3 Streaming ──► Epic 3.4 Tool round-trip
                                                                          └─► Epic 3.5 Reasoning passthrough
  Epic 3.6 Endpoint routing decision ◄── requires Epic 2.3 (WrongEndpoint classification) AND Epic 3.2 (Responses send path)

Phase 5: Observability (depends on Epic 2.3's ProbeAttempt/ResolutionState existing)
  Epic 5.1 Metrics ──► Epic 5.2 Dashboard

Phase 6: Hardening/Docs (cuts across all phases, sequenced last)
  Epic 6.1 Test infra ──► Epic 6.2 Docs
```

---

## Phase 1: Foundation — Error Classification & Config Schema

### Epic 1.1: OpenAI Error Classification
**Goal**: Give the resolution loop (Phase 2) a way to tell "deprecated model" from "wrong endpoint" from "transient" from "auth/malformed," without touching the existing `map_error_status`/`ProviderError` contract (ADR-003).

#### Story 1.1.1: Classify a parsed OpenAI error body into `OpenaiErrorClass`
**As a** resolution loop, **I want** to classify an HTTP status + response body into `Deprecated`/`WrongEndpoint`/`Transient`/`Other`, **so that** I can decide whether to advance the candidate list, retry against `/v1/responses`, or leave the cache alone.
**Acceptance Criteria**:
- A 400 response with body `{"error":{"type":"invalid_request_error","message":"The model `gpt-5.1-codex-max` has been deprecated"}}` classifies as `Deprecated`.
  - *Given* `classify_openai_error(400, r#"{"error":{"type":"invalid_request_error","message":"...has been deprecated"}}"#)`, *When* called, *Then* it returns `OpenaiErrorClass::Deprecated`.
- A 404 response with body containing `"Use the v1/responses endpoint instead"` classifies as `WrongEndpoint`.
  - *Given* `classify_openai_error(404, r#"{"error":{"type":"invalid_request_error","message":"...not supported in the v1/chat/completions endpoint. Use the v1/responses endpoint instead"}}"#)`, *When* called, *Then* it returns `OpenaiErrorClass::WrongEndpoint`.
- A 500 or malformed/unparseable body classifies as `Transient`, never advancing the candidate list.
  - *Given* `classify_openai_error(503, "Service Unavailable")` (non-JSON body), *When* called, *Then* it returns `OpenaiErrorClass::Transient`.
- A 401/403 (auth) or a 400 that doesn't match the deprecated/wrong-endpoint patterns classifies as `Other`, which the resolution loop must never advance on.
  - *Given* `classify_openai_error(401, r#"{"error":{"type":"invalid_request_error","message":"Incorrect API key provided"}}"#)`, *When* called, *Then* it returns `OpenaiErrorClass::Other`.
**Files**: `src/providers/openai/mod.rs` (new function, colocated near `map_error_status`)

##### Task 1.1.1a: Define `OpenaiErrorClass` enum (~3 min)
- Add `enum OpenaiErrorClass { Deprecated, WrongEndpoint, Transient, Other }`, `#[derive(Debug, Clone, Copy, PartialEq, Eq)]`.
- Files: `src/providers/openai.rs`

##### Task 1.1.1b: Implement `classify_openai_error(status: u16, body: &str) -> OpenaiErrorClass` (~5 min)
- Parse `body` as `serde_json::Value`; on parse failure or missing `error.message`, return `Transient` for 5xx/0 and `Other` for 4xx.
- Match `(status, error.type, message substring)` per the table in ADR-003 / architecture.md §4.2: 400 + "has been deprecated"/"decommissioned" → `Deprecated`; 404 + "v1/responses" → `WrongEndpoint`; 429 is not passed here (already `RateLimited` upstream of this function); 401/403 → `Other`; anything else 4xx → `Other`; 5xx/network → `Transient`.
- Files: `src/providers/openai.rs`

##### Task 1.1.1c: Unit tests for each classification branch (~5 min)
- One `#[test]` per Acceptance Criterion above, plus a malformed-JSON-body case and a 400-that-doesn't-match-any-known-substring case (→ `Other`, not `Deprecated`).
- Files: `src/providers/openai.rs` (`#[cfg(test)] mod tests`)

#### Story 1.1.2: Guard against string-matching fragility with a data-driven table
**As a** future maintainer, **I want** the deprecated/wrong-endpoint substring matches expressed as an explicit, auditable table (not inline `if` chains), **so that** adding a newly-observed error phrasing is a one-line diff, not a logic change.
**Acceptance Criteria**:
- The match rules are expressed as a `const` slice of `(u16, &str, OpenaiErrorClass)` (status, message-substring, class) tuples, iterated in `classify_openai_error`.
  - *Given* the const table contains `(400, "has been deprecated", OpenaiErrorClass::Deprecated)`, *When* a new entry `(400, "no longer available", OpenaiErrorClass::Deprecated)` is appended, *Then* no other code changes are needed for that new phrasing to classify correctly.
- At least one entry in the table, and one test fixture exercising it, is sourced from a real 400/404 response body captured from the ExampleCorp Model Gateway (via SBN Dev Agent), not solely from the two strings already quoted in requirements.md — per pre-mortem.md P1 #1, the real gateway's wording may not match the two hardcoded substrings, which would make a genuinely deprecated model classify as `Other` and never advance, silently reproducing the exact bug this project fixes.
  - *Given* a captured (redacted) real ExampleCorp Model Gateway 400/404 error body added as fixture `tests/fixtures/model_gateway_deprecated_error.json` (or equivalent), *When* `classify_openai_error` is called with it, *Then* it classifies correctly against the real wording, not just the requirements.md-quoted strings.
**Files**: `src/providers/openai.rs`

##### Task 1.1.2a: Refactor Task 1.1.1b's match arms into a `const` table + lookup (~4 min)
- Files: `src/providers/openai.rs`

##### Task 1.1.2b: Test that an unmatched but structurally-4xx body still returns `Other`, not a panic or default `Deprecated` (~3 min)
- Files: `src/providers/openai.rs`

##### Task 1.1.2c: Capture at least one real 400/404 response body from the ExampleCorp Model Gateway (via SBN Dev Agent/VPN access) for a genuinely deprecated or wrong-endpoint model, add it as a redacted fixture file, and add a table entry + test asserting it classifies correctly (~10 min, environment-dependent — see Unresolved Questions)
- If SBN Dev Agent/VPN access is unavailable during implementation, this task cannot be completed as scoped; do not substitute a synthetic string that merely restates requirements.md's existing two examples — flag the gap explicitly in the PR instead of silently marking this done.
- Files: `src/providers/openai.rs`, new fixture file under `tests/fixtures/` or `src/providers/openai/testdata/` (confirm convention per Story 6.1.2's note)

---

### Epic 1.2: Config Schema for Opt-in Resolution
**Goal**: Add the `model_family` field additively, enforce mutual exclusivity with `model`, and keep `references/conf.d/` examples in sync.

#### Story 1.2.1: Add `model_family: Option<String>` to `RouteUpstreamRef`
**As an** operator, **I want** to set `model_family = "gpt-5"` instead of a static `model` pin, **so that** consolette can auto-discover and fail over across that upstream's live catalog.
**Acceptance Criteria**:
- A TOML route upstream entry with only `model_family` set deserializes successfully; one with only `model` set (today's existing configs) is unaffected.
  - *Given* TOML `[[routes.upstreams]]\nname = "model-gateway-openai"\nmodel_family = "gpt-5"`, *When* `Config` is deserialized via `figment`, *Then* `RouteUpstreamRef.model_family == Some("gpt-5".to_string())` and `RouteUpstreamRef.model == None`.
**Files**: `src/config/schema.rs`

##### Task 1.2.1a: Add the field with a doc comment matching the existing `model` field's style (~3 min)
- `#[serde(default)] pub model_family: Option<String>,` immediately after `model` in `RouteUpstreamRef` (`src/config/schema.rs:152-161`).
- Files: `src/config/schema.rs`

##### Task 1.2.1b: Deserialization round-trip test for `model_family`-only and `model`-only configs (~4 min)
- Files: `src/config/schema.rs` (or its existing `#[cfg(test)]` module)

#### Story 1.2.2: Reject configs that set both `model` and `model_family` (and neither), and reject `model_family` on non-OpenAI upstreams
**As an** operator, **I want** an explicit config error if I accidentally set both `model` and `model_family` on the same upstream ref, set neither on a `kind = "openai"` upstream that needs one, or set `model_family` on a non-`openai`-kind upstream — **so that** I don't silently get whichever the code happens to check first, a confusing downstream failure from having selected no model at all, or (per architecture-review.md's second-pass finding) a misconfiguration that leaks the internal `__consolette_model_family` dispatch key straight into a real Anthropic/Bedrock/Gemini/OpenRouter request body, since only `OpenaiProvider::send` knows to strip it.

*Deliberate tradeoff, stated explicitly (architecture-review.md Concern):* `model: Option<String>` and `model_family: Option<String>` as two independently-optional fields is a textbook illegal-states-representable gap — the type system alone permits `{None, None}` and `{Some, Some}`; this story's runtime validator is what actually enforces "exactly one, and only on `kind = "openai"`." Accepted here as consistent with `RouteUpstreamRef`'s existing flat, `#[serde(deny_unknown_fields)]` struct shape (matching `model`'s own precedent) rather than introducing a custom `Deserialize` producing an internal `enum ModelSelector { Pin(String), Family(String) }` — a reasonable TOML-ergonomics tradeoff for a two-field struct, revisit if a third mutually-exclusive selector is ever added.
**Acceptance Criteria**:
- Loading a config where one `RouteUpstreamRef` sets both fields fails with a new `ConfigError` variant naming the route and upstream.
  - *Given* a `RouteUpstreamRef { name: "model-gateway-openai".into(), model: Some("gpt-5.1".into()), model_family: Some("gpt-5".into()), .. }` inside route `"coding"`, *When* `validate_references`-equivalent validation runs, *Then* it returns `ConfigError::ConflictingModelSelector { route: "coding".into(), upstream: "model-gateway-openai".into() }`.
- A `kind = "openai"` `RouteUpstreamRef` setting *neither* `model` nor `model_family` is explicitly checked against whatever validation already governs an unset `model` today (adversarial-review.md Minor) — if that's already an existing validation error, this story adds a test cross-checking it stays an error after this change; if it's currently silently accepted, this story extends `validate_model_selectors` to reject it for `kind = "openai"` upstreams specifically.
  - *Given* a `kind = "openai"` `RouteUpstreamRef` with `model: None, model_family: None`, *When* validation runs, *Then* it is rejected (either by pre-existing validation, confirmed by a new test, or by this story's new check if no such validation exists today).
- A `RouteUpstreamRef` with `kind != Openai` (i.e. `Anthropic`, `Bedrock`, `Gemini`, or `Openrouter`) setting `model_family: Some(_)` fails with a new `ConfigError` variant naming the route, upstream, and its actual kind — this is the leak-prevention check, since `Router::dispatch`'s `model_family`-to-body-key mutation (Story 1.3.2) is keyed only on `UpstreamRef.model_family` being `Some`, not on upstream kind, and only `OpenaiProvider::send` (Story 1.3.3) strips the resulting internal key back out.
  - *Given* a `RouteUpstreamRef { name: "anthropic".into(), model_family: Some("gpt-5".into()), .. }` on a `kind = "anthropic"` upstream, *When* validation runs, *Then* it returns `ConfigError::ModelFamilyOnNonOpenaiUpstream { route: ..., upstream: "anthropic".into(), kind: "anthropic".into() }` rather than reaching `Router::dispatch`.
**Files**: `src/config/validate.rs`, `src/config/mod.rs` (or wherever `ConfigError` is declared)

##### Task 1.2.2a: Add `ConfigError::ConflictingModelSelector { route, upstream }` and `ConfigError::ModelFamilyOnNonOpenaiUpstream { route, upstream, kind }` variants (~3 min)
- Files: `src/config/mod.rs` (colocated with existing `ConfigError::UnknownUpstreamReference`)

##### Task 1.2.2b: Add `validate_model_selectors(config: &Config) -> Result<(), ConfigError>`, called alongside `validate_references` (~6 min)
- Iterate every route's `upstreams`; error if both `model` and `model_family` are `Some` on the same `kind = "openai"` entry. Check first (via `grep -n "model.*None"` or reading existing `validate_references`) whether an unset `model` on a `kind = "openai"` upstream is already rejected today; if not, extend this same function to reject the neither-set case too, rather than leaving it as a separate, easy-to-forget check. Additionally: for every `RouteUpstreamRef` with `model_family: Some(_)`, look up its upstream's `kind` in `config.upstreams` and reject if `kind != Openai` — this is the leak-prevention check (architecture-review.md's second-pass blocker), not optional.
- Files: `src/config/validate.rs`, call-site wiring wherever `validate_references` is invoked (find via `grep -n validate_references`)

##### Task 1.2.2c: Test the conflict-rejected, neither-set-rejected, exactly-one-set-accepted, and `model_family`-on-non-openai-rejected cases (~5 min)
- Files: `src/config/validate.rs`

#### Story 1.2.3: Document the new field with a working example
**As an** operator reading `references/conf.d/`, **I want** a working example of `model_family`, **so that** I don't have to reverse-engineer the syntax from source.
**Acceptance Criteria**:
- `references/conf.d/00-providers.toml` (or a new sibling example file) contains a commented, non-active example route upstream using `model_family`, and existing conf.d-loading tests still pass with it present.
  - *Given* the updated `references/conf.d/00-providers.toml`, *When* the repo's existing "load every file in `references/conf.d/`" test runs, *Then* it passes with no deserialization error.
**Files**: `references/conf.d/00-providers.toml`

##### Task 1.2.3a: Add the documented example (commented-out, consistent with the file's existing `kind = "openai"` commented block at line 12) (~3 min)
- Files: `references/conf.d/00-providers.toml`

##### Task 1.2.3b: Verify (don't just assume) the existing conf.d-loading test still passes — locate it via `grep -rn "conf.d" src/config` and run it (~3 min)
- Files: none (verification task)

---

### Epic 1.3: Per-Request `model_family` Threading (Router → `OpenaiProvider`)
**Goal**: Give the resolution loop (Phase 2) a real, per-request channel for `model_family`. `build_providers` (`src/routing/router.rs:75-131`) constructs exactly one `Arc<dyn Provider>` per `config.upstreams` entry, before any route or `RouteUpstreamRef` is in scope — there is no `model_family` value to bake into `OpenaiProvider::new`, and one `Upstream` can be referenced by multiple `RouteUpstreamRef`s with different families anyway (architecture-review.md Blocker). `model` itself already solves this exact problem via a per-dispatch body mutation (`Router::dispatch`, `router.rs:531-539`, from the router-internal `UpstreamRef`, `src/routing/strategy.rs:17-22`) — this epic threads `model_family` through the same channel instead of inventing a new one. This epic must land before any Epic 2.2/2.3 story that assumes the resolution cache is reachable per-request.

#### Story 1.3.1: Widen `UpstreamRef` with `model_family: Option<String>`
**As** `Router::dispatch`, **I want** the active route upstream's `model_family` available on the same per-dispatch struct that already carries `model`, **so that** I have something to hand to the provider at send time.
**Acceptance Criteria**:
- `UpstreamRef` gains `model_family: Option<String>`, populated from `route_upstream.model_family.clone()` at the same place `model` is populated today (`router.rs:373-378`).
  - *Given* a route upstream entry with `model_family = Some("gpt-5")` and no `model`, *When* `Router::from_config` builds its `candidates: Vec<UpstreamRef>`, *Then* the corresponding `UpstreamRef.model_family == Some("gpt-5".to_string())` and `.model == None`.
**Files**: `src/routing/strategy.rs`, `src/routing/router.rs`

##### Task 1.3.1a: Add `pub model_family: Option<String>` to `UpstreamRef` (~2 min)
- Files: `src/routing/strategy.rs`

##### Task 1.3.1b: Populate it in `Router::from_config`'s candidate-building loop, alongside the existing `model` assignment (~3 min)
- Files: `src/routing/router.rs:373-378`

##### Task 1.3.1c: Test that a `model_family`-configured route upstream produces a `UpstreamRef` with the field set, and a `model`-configured one leaves it `None` (~3 min)
- Files: `src/routing/router.rs`

#### Story 1.3.2: `Router::dispatch` threads the active family into the request via an internal-only body key
**As** `Router::dispatch`, **I want** to hand the chosen candidate's `model_family` to the provider the same way I already hand it `model`, **so that** `OpenaiProvider` can tell which family (if any) this request should resolve against, without widening the `Provider` trait.
**Acceptance Criteria**:
- When `chosen.model_family` is `Some`, the request body sent to `provider.send()` gains an internal-only key (`MODEL_FAMILY_BODY_KEY = "__consolette_model_family"`) carrying the family string; when `chosen.model_family` is `None` (every existing config today), the request body construction path (`router.rs:531-539`) is byte-for-byte unchanged from before this story.
  - *Given* `chosen.model_family == Some("gpt-5")`, *When* `Router::dispatch` builds `request_body`, *Then* `request_body["__consolette_model_family"] == "gpt-5"`.
  - *Given* `chosen.model_family == None`, *When* `Router::dispatch` builds `request_body`, *Then* the resulting body is identical to today's output (no new key added, `model` handling unaffected).
**Files**: `src/routing/router.rs`

##### Task 1.3.2a: Extend the `request_body` match block at `router.rs:531-539` to also set the internal key from `chosen.model_family` (independent of, and composable with, the existing `model` branch) (~4 min)
- Files: `src/routing/router.rs`

##### Task 1.3.2b: Test both branches (family set / family unset) (~4 min)
- Files: `src/routing/router.rs`

#### Story 1.3.3: `OpenaiProvider::send` extracts and strips the internal key before translating/forwarding
**As** `OpenaiProvider`, **I want** to read the internal `model_family` key off the incoming body and remove it before any translation or forwarding step, **so that** it's never leaked to the real upstream and becomes the key I use to consult/populate the resolution cache for this call.
**Acceptance Criteria**:
- The internal key never appears in the outgoing HTTP request body sent to the real upstream (chat/completions or, once Epic 3.2 lands, responses); its value becomes the per-call `family` used by Epic 2.2/2.3's resolution logic.
  - *Given* an incoming body containing `"__consolette_model_family": "gpt-5"`, *When* `OpenaiProvider::send` runs, *Then* the key is removed from `body` before any existing request-building/translation logic executes, and its value is passed into the resolution entry point as `family`.
- A body with no such key (every static-`model` upstream, the common case) skips all resolution logic entirely — no `ResolutionCache`/`SingleFlightGuard` access, no clock read — satisfying the zero-overhead constraint Story 2.2.1 depends on.
  - *Given* an incoming body with no internal key, *When* `OpenaiProvider::send` runs, *Then* it proceeds exactly as it does today, with no new branch executed beyond a single cheap presence check.
**Files**: `src/providers/openai.rs`

##### Task 1.3.3a: Define `const MODEL_FAMILY_BODY_KEY: &str = "__consolette_model_family";` colocated with `OpenaiProvider` (~2 min)
- Files: `src/providers/openai.rs`

##### Task 1.3.3b: Add the extract-and-strip step (`body.as_object_mut().and_then(|o| o.remove(MODEL_FAMILY_BODY_KEY))`) at the top of `send()`, before any existing request-building logic (~4 min)
- Files: `src/providers/openai.rs`

##### Task 1.3.3c: Test that the key is stripped from the outgoing HTTP body (capture via a test double / `MockServer`, see Epic 2.1's note on reusing `src/cost_metrics/test_support.rs::MockServer`), and confirm a static-pin request's outgoing body is byte-identical to before this story landed (~5 min)
- Files: `src/providers/openai.rs`

---

### Epic 1.4: Module Restructure (moved earlier — see Tech Debt Disposition)
**Goal**: Split `openai.rs` into `openai/mod.rs` + `openai/resolution.rs` + `openai/responses.rs` *before* Phase 2's resolution code or Phase 3's Responses API code exists, so both new concerns land directly in their own module instead of accreting onto one flat/growing file that gets split apart later (architecture-review.md and adversarial-review.md's shared Concern: the original plan only split out Responses API code in Epic 3.1, leaving resolution logic mixed into `openai/mod.rs` for all of Phase 2/4).

#### Story 1.4.1: Split `openai.rs` into `openai/mod.rs` + `openai/resolution.rs` + `openai/responses.rs`
**As a** maintainer, **I want** dynamic resolution and Responses API translation each in their own sibling module from the start, **so that** three structurally distinct concerns (transport, resolution, Responses translation) never mix in one file, matching the `src/providers/gemini/` precedent.
**Acceptance Criteria**:
- `src/providers/openai.rs` becomes `src/providers/openai/mod.rs` (mechanical `git mv`); `src/providers/openai/resolution.rs` and `src/providers/openai/responses.rs` exist as empty (doc-comment-only) stubs; all existing tests pass unchanged; `src/providers/mod.rs`'s `pub mod openai;` line requires no change (a directory module with `mod.rs` is transparent to callers).
  - *Given* the rename and new stub modules, *When* `cargo test providers::openai` runs, *Then* every previously-passing test still passes with zero behavior change.
**Files**: `src/providers/openai.rs` → `src/providers/openai/mod.rs`, `src/providers/openai/resolution.rs` (new, empty stub), `src/providers/openai/responses.rs` (new, empty stub)

##### Task 1.4.1a: `git mv src/providers/openai.rs src/providers/openai/mod.rs` (~2 min)
- Files: `src/providers/openai.rs`, `src/providers/openai/mod.rs`

##### Task 1.4.1b: Create empty `src/providers/openai/resolution.rs` and `src/providers/openai/responses.rs`, each with a module doc comment stating its scope; add `mod resolution;` and `mod responses;` to `openai/mod.rs` (~3 min)
- Files: `src/providers/openai/mod.rs`, `src/providers/openai/resolution.rs`, `src/providers/openai/responses.rs`

##### Task 1.4.1c: Run full test suite, confirm zero regressions from the move alone (~3 min)
- Files: none (verification)

---

## Phase 2: Dynamic Model Resolution Core

**Module placement**: Epics 2.1-2.3 and 4.1's resolution-specific code (`ResolutionCache`, `SingleFlightGuard`, `filter_candidates`/`rank_candidates`/`classify_openai_error`/probe-and-walk loop) lands in `src/providers/openai/resolution.rs`, created by Epic 1.4 (moved to the end of Phase 1, before Phase 2 starts — see the Tech Debt Disposition table's addendum) — not accreted onto `openai/mod.rs`.

**Test infrastructure**: all of Phase 2's tests that need a fake OpenAI HTTP server reuse the existing `src/cost_metrics/test_support.rs::MockServer` harness (a bare `TcpListener`+`axum` local server, already reused by `src/cost_metrics/estimator.rs`) rather than inventing a third bespoke mock-server pattern (`src/providers/openrouter/models.rs:127-215` is already a second, independent reinvention of the same thing — architecture-review.md Concern). `OpenaiProvider::new` already takes `base_url: String` as a plain runtime parameter, so pointing it at a local `MockServer` instance requires no production-code changes.

### Epic 2.1: Candidate Discovery & Ranking

#### Story 2.1.1: Fetch and filter `/v1/models` candidates by family prefix
**As** `OpenaiProvider`, **I want** to fetch `/v1/models` and keep only ids starting with the active `ModelFamily` prefix, **so that** resolution only considers relevant candidates.
**Acceptance Criteria**:
- Given a `/v1/models` response listing `["gpt-5.1-codex-max", "gpt-5.2-codex", "gpt-5.3-codex", "text-embedding-3-small"]` and family `"gpt-5"`, filtering keeps exactly the three `gpt-5.*` ids.
  - *Given* `fetch_models()` returns the above list and `ModelFamily = "gpt-5"`, *When* `filter_candidates` runs, *Then* it returns `["gpt-5.1-codex-max", "gpt-5.2-codex", "gpt-5.3-codex"]` in the order `/v1/models` returned them (ranking happens separately, Story 2.1.2).
**Files**: `src/providers/openai/resolution.rs`

##### Task 2.1.1a: Implement `filter_candidates(models: &[ModelInfo], family: &str) -> Vec<String>` (~3 min)
- `id.starts_with(family)` string filter, reusing the already-fetched `fetch_models()` (`src/providers/openai.rs:188-213`, pre-split line numbers — relocate to `openai/mod.rs` post-Epic-1.4) result.
- Files: `src/providers/openai/resolution.rs`

##### Task 2.1.1b: Unit test with the four-model fixture above, using synthetic (not real) model ids per pitfalls.md testing guidance (~4 min)
- Files: `src/providers/openai/resolution.rs`

#### Story 2.1.2: Rank filtered candidates newest-first (ADR-002 heuristic)
**As** the resolution loop, **I want** filtered candidates ordered newest-first, **so that** the walk tries the most-capable model before older ones.
**Acceptance Criteria**:
- Given ids `["family-v2", "family-v10", "family-v3-preview"]` (deliberately adversarial per pitfalls.md, avoiding real model names), ranking produces `["family-v10", "family-v3-preview", "family-v2"]` — the two-digit `v10` correctly outranks `v2` and `v3-preview` (a plain lexicographic sort would put `v10` first only by accident; this test specifically catches the case where it wouldn't).
  - *Given* `rank_candidates("family-", ["family-v2", "family-v10", "family-v3-preview"])`, *When* called, *Then* it returns `["family-v10", "family-v3-preview", "family-v2"]`.
- An id with no extractable numeric token after the prefix sorts after all numerically-tokenized ids.
  - *Given* `rank_candidates("family-", ["family-v2", "family-experimental"])`, *When* called, *Then* it returns `["family-v2", "family-experimental"]`.
**Files**: `src/providers/openai/resolution.rs`

##### Task 2.1.2a: Implement `extract_version_tokens(id: &str, prefix: &str) -> Option<Vec<u64>>` (~5 min)
- Strip `prefix`, split remaining on `.`/`-`, parse each segment as `u64`, stop at the first non-numeric segment; `None` if zero tokens extracted.
- Files: `src/providers/openai/resolution.rs`

##### Task 2.1.2b: Implement `rank_candidates(prefix: &str, ids: &[String]) -> Vec<String>` using `extract_version_tokens`, descending tuple comparison, non-numeric ids sorted last by reverse-lexicographic order among themselves (~5 min)
- Files: `src/providers/openai/resolution.rs`

##### Task 2.1.2c: Unit tests for both Acceptance Criteria plus a tie-break case (two ids with identical numeric tokens) (~5 min)
- Files: `src/providers/openai/resolution.rs`

#### Story 2.1.3: Handle `fetch_models()` failure during resolution
**As** the resolution loop, **I want** a `fetch_models()` failure (network error, non-200, or malformed/unparseable JSON) to abort cleanly rather than panic, silently proceed with an empty candidate list, or corrupt the cache, **so that** the upstream's `/v1/models` endpoint being unavailable is handled as a first-class failure mode of the exact dependency this feature exists to be resilient against (adversarial-review.md Blocker 1).
**Acceptance Criteria**:
- A `fetch_models()` network error, non-200 response, or malformed-JSON body aborts the resolution attempt immediately, writes nothing to `ResolutionCache` (an existing stale entry, if any, is left untouched — not invalidated), records the failure for Story 2.2.3's backoff, and surfaces to the caller as a transient error — treated identically to a `Transient` classification during the candidate walk (Story 2.3.2), never as "all candidates exhausted."
  - *Given* a scripted upstream where `GET /v1/models` returns a connection error, *When* resolution runs, *Then* no candidate probe is attempted, `ResolutionCache` is unchanged, and the caller receives a transient-classified error (not a panic, not `Exhausted`).
  - *Given* `GET /v1/models` returns `200` with a malformed body (non-JSON, or JSON missing the expected `data` array), *When* resolution runs, *Then* the same abort-without-cache-mutation behavior applies.
**Files**: `src/providers/openai/resolution.rs`

##### Task 2.1.3a: Wrap `fetch_models()`'s existing error paths (network/non-200/parse failure) at the resolution entry point, routing them into Story 2.3.2's existing "`Transient` aborts without advancing, without writing cache" control flow rather than a new, separately-tested branch (~4 min)
- Files: `src/providers/openai/resolution.rs`

##### Task 2.1.3b: Test all three failure modes (network error, non-200, malformed JSON), asserting no cache mutation and a transient (not exhausted) error surfaces (~5 min)
- Files: `src/providers/openai/resolution.rs`

---

### Epic 2.2: Resolution Cache

#### Story 2.2.1: `ResolutionCache` owned by `OpenaiProvider`, keyed by family
**As** `OpenaiProvider`, **I want** a per-family cache of the last-known-working model/endpoint/token-style, **so that** steady-state requests skip resolution entirely.
**Acceptance Criteria**:
- Every `OpenaiProvider` instance unconditionally owns `resolution: Arc<DashMap<String, ResolvedModel>>` (never `Option`, never a constructor parameter) — per Epic 1.3, whether it's ever populated for a given upstream depends solely on whether any dispatched request for that upstream carries the internal `model_family` body key, which is a per-request fact, not something knowable at construction time.
  - *Given* an `OpenaiProvider` whose every dispatched request lacks the internal `model_family` body key (a static-`model` upstream), *When* `send()` runs for any number of requests, *Then* `resolution` is never read or written, and behavior is byte-identical to before this story landed (zero-overhead constraint).
- A cache hit for a known family returns the cached `ResolvedModel` without any HTTP call.
  - *Given* `ResolutionCache` already contains `"gpt-5" -> ResolvedModel { model_id: "gpt-5.3-codex", endpoint: Responses, token_param: MaxCompletionTokens, .. }`, *When* a request carrying the internal key `"gpt-5"` is dispatched, *Then* `OpenaiProvider` reads the cache entry synchronously and proceeds directly to building the real request — no `/v1/models` or probe HTTP call is made.
**Files**: `src/providers/openai/resolution.rs`, `src/providers/openai.rs` (field addition + `send()` call-in point, per Epic 1.3's Story 1.3.3)

##### Task 2.2.1a: Define `ResolvedModel` struct (`model_id: String, endpoint: Endpoint, token_param: TokenParamStyle, resolved_at: Instant`) and `Endpoint`/`TokenParamStyle` enums (~4 min)
- Files: `src/providers/openai/resolution.rs`

##### Task 2.2.1b: Add `resolution: Arc<DashMap<String, ResolvedModel>>` field to `OpenaiProvider`, initialized unconditionally (`Arc::new(DashMap::new())`) in `OpenaiProvider::new` — no new constructor parameter, since Epic 1.3 delivers the family value per-request via the stripped body key, not at construction (~4 min)
- Files: `src/providers/openai.rs`

##### Task 2.2.1c: Test that a request without the internal `model_family` key never touches `resolution`, and one with it does (cache miss still triggers the Epic 2.3 walk, but the field itself is always present and inert until keyed) (~4 min)
- Files: `src/providers/openai.rs`

#### Story 2.2.2: Single-flight guard prevents concurrent redundant candidate walks, with guaranteed release
**As** `OpenaiProvider`, **I want** only one in-flight resolution walk per family at a time, with the guard always released even if the winning walk errors, returns early, or panics, **so that** N concurrent requests hitting a cache miss don't each independently probe the whole candidate list, and a single stuck/failed walk can never permanently starve that family's resolution (adversarial-review.md Blocker 5).
**Acceptance Criteria**:
- Two concurrent requests for the same family, both hitting a cache miss simultaneously, result in exactly one `/v1/models` fetch and one candidate-walk.
  - *Given* two concurrent `send()` calls for family `"gpt-5"` with an empty cache, *When* both are dispatched at the same instant, *Then* `fetch_models()` is called exactly once (verified via a `ScriptedProvider`-style call-count assertion in the test double), not twice.
- The second, non-winning caller awaits the winner's completion signal for `min(remaining request budget, a 5s ceiling)`; on notification or timeout it re-checks the cache — a populated entry (the winner's fresh write, or a still-valid stale entry) is used directly; if the cache is still empty, the caller propagates a transient error rather than starting a second independent walk.
  - *Given* the winning caller's walk is still in flight when the ceiling elapses, *When* the waiting caller's wait times out, *Then* it re-reads the cache once and, finding it still empty, returns a transient error without calling `fetch_models()` itself.
- The guard is released via RAII (a `SingleFlightPermit` whose `Drop` impl clears the compare-exchanged flag and notifies waiters), not a manual clear at the end of the happy path, so an early return, `?`-propagated error, or panic-unwind during the winning walk still releases it.
  - *Given* the winning resolver task panics mid-walk (simulated via a scripted upstream handler that panics, run inside a `tokio::spawn`ed task so the panic doesn't tear down the test), *When* the panic unwinds (this crate does not set `panic = "abort"` in `Cargo.toml` — verified, so `Drop` runs), *Then* a subsequent call for the same family can still acquire the permit and successfully resolve — the guard is not permanently stuck.
**Files**: `src/providers/openai/resolution.rs`

##### Task 2.2.2a: Add `SingleFlightGuard` (`DashMap<String, Arc<AtomicBool>>` paired with `Arc<tokio::sync::Notify>`) and a `SingleFlightPermit<'_>` RAII wrapper returned by a successful compare-exchange, modeled on `openrouter/cache.rs`'s single-flight compare-exchange pattern (~5 min)
- Files: `src/providers/openai/resolution.rs`

##### Task 2.2.2b: Implement `SingleFlightPermit`'s `Drop` impl: always clear the flag and call `Notify::notify_waiters()`, regardless of how the holder's scope exits (~4 min)
- Files: `src/providers/openai/resolution.rs`

##### Task 2.2.2c: Wire the guard into the resolution entry point: first caller wins the compare-exchange, holds the permit for the walk's duration; concurrent callers `tokio::select!` between `Notify::notified()` and a bounded sleep, then re-check the cache per the acceptance criteria above (~5 min)
- Files: `src/providers/openai/resolution.rs`

##### Task 2.2.2d: Concurrency test using `tokio::join!` on two simulated concurrent resolutions against a counting fake HTTP client, asserting exactly one `/v1/models` call (~5 min)
- Files: `src/providers/openai/resolution.rs`

##### Task 2.2.2e: Panic/early-return release test: the winning walk panics or returns early via `?`; assert a subsequent call for the same family still acquires the permit and resolves (not permanently stuck) (~5 min)
- Files: `src/providers/openai/resolution.rs`

#### Story 2.2.3: Negative-cache/backoff for sequential resolution attempts on a cold cache
**As** `OpenaiProvider`, **I want** a short backoff window after a resolution walk ends in `Exhausted` or a `fetch_models()` failure (Story 2.1.3), **so that** sequential (not just concurrent) requests during a sustained outage don't each re-pay the full multi-candidate probe cost — `SingleFlightGuard` alone only dedupes concurrent callers, doing nothing for one-at-a-time requests (adversarial-review.md Blocker 2).
**Acceptance Criteria**:
- After a resolution walk ends `Exhausted` or with a `fetch_models()` failure at time T, a request for that family arriving within a backoff window (internal constant, default 30s) fails fast with the recorded error, making no `/v1/models` call and no candidate probes; a request arriving after the window re-attempts resolution normally.
  - *Given* a resolution walk for family `"gpt-5"` ends `Exhausted` at time T, *When* a second, sequential request for `"gpt-5"` arrives at T+5s (within the 30s backoff), *Then* it fails immediately with the recorded error and zero HTTP calls are made (verified via call-count assertion).
  - *Given* the same scenario but the second request arrives at T+31s, *When* it is dispatched, *Then* a fresh resolution walk is attempted.
- This is strictly better than today's static-pin behavior (fail-fast, no probe cost) under a sustained outage, not worse.
- Tests use `tokio::time::pause()`/`advance()`, not real sleeps.
**Files**: `src/providers/openai/resolution.rs`

##### Task 2.2.3a: Add a per-family negative-cache entry (e.g. `resolution_backoff: Arc<DashMap<String, (Instant, ProviderError)>>`, or fold into a `CacheEntry::BackedOff` variant alongside `ResolvedModel`) recording the last-failure timestamp and error (~4 min)
- Files: `src/providers/openai/resolution.rs`

##### Task 2.2.3b: Check-and-short-circuit at the top of the resolution entry point: if a still-within-window backoff entry exists, return its recorded error immediately instead of starting a walk (~4 min)
- Files: `src/providers/openai/resolution.rs`

##### Task 2.2.3c: Tests for both criteria using `tokio::time::pause()`/`advance()` (~5 min)
- Files: `src/providers/openai/resolution.rs`

---

### Epic 2.3: Probe-and-Walk Resolution Loop

#### Story 2.3.1: Cheap, side-effect-free probe request shape
**As** `OpenaiProvider`, **I want** a minimal-token, no-tools probe request, **so that** resolution's real-request attempts are as cheap and safe as possible.
**Acceptance Criteria**:
- The probe request body has no `tools` array and a small `max_tokens`/`max_completion_tokens` value (e.g. 16), distinct from `capability.rs`'s `EVAL_MAX_TOKENS = 64` tool-probing shape (this feature doesn't need tool-calling capability, only existence).
  - *Given* `build_probe_body("gpt-5.3-codex")`, *When* inspected, *Then* the resulting `Value` has no `"tools"` key and `max_tokens` (or the resolved `TokenParamStyle`'s key) `<= 16`.
**Files**: `src/providers/openai/resolution.rs`

##### Task 2.3.1a: Implement `build_probe_body(model_id: &str) -> Value` — a minimal single-user-message body, no tools (~4 min)
- Files: `src/providers/openai/resolution.rs`

##### Task 2.3.1b: Unit test asserting the shape (no `tools`, small token budget) (~3 min)
- Files: `src/providers/openai/resolution.rs`

#### Story 2.3.2: Walk candidates, classify each outcome, decide advance/retry/stay/abort
**As** `OpenaiProvider`, **I want** to try ranked candidates in order, using `classify_openai_error` (Story 1.1.1) to decide whether to advance, retry the same id against Responses, or abort, **so that** a transient blip — including a rate limit — doesn't burn through the whole candidate list (pitfalls.md §3's "thrash on a single upstream blip"; requirements.md Rabbit Hole #3; adversarial-review.md Blocker 3).
**Acceptance Criteria**:
- A `Deprecated`-classified failure on candidate 1 advances to candidate 2; a `Transient`-classified failure on candidate 1 aborts the walk immediately (does not try candidate 2) and surfaces the transient error, leaving the cache untouched.
  - *Given* ranked candidates `["v3", "v2", "v1"]` and a scripted upstream returning `Deprecated` for `"v3"` then success for `"v2"`, *When* resolution runs, *Then* the cache ends with `"v2"` as the winner and exactly two probe attempts were made.
  - *Given* the same ranked candidates and a scripted upstream returning a 503 (`Transient`) for `"v3"`, *When* resolution runs, *Then* the walk stops after one attempt, no cache entry is written, and the caller receives a transient error (not "candidate v3 is dead").
- A `WrongEndpoint`-classified failure on a candidate retries the **same** candidate id against `/v1/responses` rather than advancing to the next candidate (this criterion's real behavior lands with Epic 3.6 once the Responses send path exists; until then, this story's scope is limited to correctly *not* advancing the candidate list on `WrongEndpoint` — advancing to a next candidate on this classification is the bug this story must not introduce).
  - *Given* candidate `"v3"` returns `WrongEndpoint`, *When* Epic 3.6 is not yet implemented, *Then* the walk aborts without advancing past `"v3"` and surfaces a clear "needs Responses API support" error, rather than silently trying `"v2"`.
- A `429`/`ProviderError::RateLimited` result from any candidate probe during the walk aborts immediately, exactly like `Transient` — it never advances the candidate list and never writes the cache. `classify_openai_error` never sees a 429 (it's already `RateLimited` upstream of that function, per Task 1.1.1b); the walk loop must check for `RateLimited` *before* calling `classify_openai_error` at all, so a rate limit can never be misrouted into `Other`/`Deprecated` handling.
  - *Given* ranked candidates `["v3", "v2", "v1"]` and a scripted upstream returning a 429 (`RateLimited`) for `"v3"`, *When* resolution runs, *Then* the walk stops after one attempt, no cache entry is written, `"v2"`/`"v1"` are never tried, and the caller receives a rate-limited error.
- An `Other`-classified result aborts the walk exactly like `Transient` (no advance, no cache write) but is recorded and logged under a distinct label from `Transient`, not merged with it — per pre-mortem.md P1 #1, `Other` means "resolution didn't recognize this error at all" (a likely sign `classify_openai_error`'s table is missing the real gateway's wording), which is a different operator action than "this was a genuine transient blip," and today's outcome vocabulary (`success`/`advance`/`retry_responses`/`transient`/`exhausted`) has no way to tell them apart.
  - *Given* ranked candidates `["v3", "v2", "v1"]` and a scripted upstream returning a 401 (`Other`, per Task 1.1.1b's classification table) for `"v3"`, *When* resolution runs, *Then* the walk stops after one attempt, no cache entry is written, and the recorded outcome is distinguishable in code/logs from a `Transient` (e.g. 503) abort on the same candidate.
- The walk's worst-case wall-clock cost — `(candidates_tried × per-probe timeout) + (candidates_tried − 1) × EVAL_PROBE_SPACING_SECS` — stays comfortably under a documented real client-side timeout, so the very request resolution exists to save can't itself time out client-side before the walk finishes (pre-mortem.md P1 #2). The documented client-timeout assumption is Claude Code's own hardcoded internal request timeout, ~300s/5 minutes (`anthropics/claude-code` issue #39906's `requestTimeout` default of 300000ms — looked up, not guessed; the Anthropic Python SDK's own default non-streaming `httpx` timeout is a separately-looser 600s, so 300s is the tighter, more conservative bound to design against). With Task 2.3.2c's formula and `default_request_timeout() == 60` (`src/config/schema.rs:306-308`) and requirements.md's documented 2-5-candidate range, the worst case (5 candidates, `min(60/5, 10) == 10s` per probe) is `5×10 + 4×3 = 62s` — under a third of the 300s budget, with margin to spare. No shrinking of candidate count/spacing/timeout is required to fit; this criterion exists so a future change to any of those constants (or to `default_request_timeout`) can't silently blow the budget unnoticed.
  - *Given* the constants above (`request_timeout_secs = 60`, up to 5 candidates, `EVAL_PROBE_SPACING_SECS = 3`, probe-timeout floor 10s), *When* a unit test computes the worst-case walk duration using Task 2.3.2c's formula, *Then* the result is asserted to be less than 300s (with an explicit comment citing the 300s client-timeout source), failing the build if a future constant change pushes it over budget.
**Files**: `src/providers/openai/resolution.rs`

##### Task 2.3.2a: Implement the ordered-walk loop: for each ranked candidate, send the probe body; check for `ProviderError::RateLimited` first (abort, do not advance, do not write cache — see Task 2.3.2e), otherwise classify the result via `map_error_status`'s body + `classify_openai_error` (~5 min)
- On success: write `ResolvedModel` to cache, return.
- On `Deprecated`: continue to next candidate.
- On `WrongEndpoint`: (pre-Epic-3.6) abort with a distinct, clearly-labeled error; do not advance.
- On `Transient`: abort immediately, do not advance, do not write cache; record/log outcome `"transient"`.
- On `Other`: abort immediately, do not advance, do not write cache; record/log outcome `"other"`, kept distinct from `"transient"` so Story 5.1.1's counter (extended by this pre-mortem fix) can surface "resolution never recognized this error" separately from "genuinely exhausted"/"auth is broken" (pre-mortem.md P1 #1).
- Files: `src/providers/openai/resolution.rs`

##### Task 2.3.2b: Exhaustion path — all candidates return `Deprecated`/`WrongEndpoint` with none succeeding (~4 min)
- Return `ProviderError::Upstream { status: 0, body: "all candidates in family <F> exhausted" }` (reuses existing variant per architecture.md §3.4 — no new `ProviderError` variant needed) so `Router::dispatch`'s existing failover-without-cooldown path fires. Also writes Story 2.2.3's backoff entry so a sequential follow-up request fails fast instead of re-walking.
- Files: `src/providers/openai/resolution.rs`

##### Task 2.3.2c: Split/scope the per-request timeout so a multi-candidate walk doesn't silently exceed the configured `request_timeout_secs` on the first real request after a cold cache (~5 min)
- Carve out a shorter per-probe timeout via `reqwest::RequestBuilder::timeout(..)` (a per-request override — `OpenaiProvider`'s `reqwest::Client` already bakes in a `read_timeout` at construction, `src/providers/openai.rs:73-78`, so this needs the builder-level override, not a second `Client`). Probe timeout = `min(request_timeout_secs / candidate_count.clamp(1, 5), a floor like 10s)` — the `clamp(1, 5)` bounds the divisor against unrealistically long candidate lists (requirements.md's own examples show 2-5 real candidates per family), so a pathological candidate count can't drive the per-probe timeout arbitrarily low on an otherwise-healthy, merely-slow upstream during the highest-stakes (first cold-cache) resolution.
- Files: `src/providers/openai/resolution.rs`

##### Task 2.3.2f: Add the wall-clock budget test from Story 2.3.2's new acceptance criterion (pre-mortem.md P1 #2): compute the worst-case walk duration from Task 2.3.2c's formula for the max documented candidate count (5) and default `request_timeout_secs` (60), assert it's under the documented 300s client-timeout budget with margin, with a code comment citing the `anthropics/claude-code#39906` source for the 300s figure so the assumption doesn't silently go stale (~4 min)
- Files: `src/providers/openai/resolution.rs`

##### Task 2.3.2d: `ScriptedProvider`-style test double covering the Acceptance Criteria scenarios above, built on `src/cost_metrics/test_support.rs::MockServer` (see Epic 2.1's note) rather than a new bespoke fake — `OpenaiProvider::new` already takes `base_url: String` as a plain runtime parameter, so pointing it at a local `MockServer` needs no production-code change (~5 min)
- Files: `src/providers/openai/resolution.rs` (test module)

##### Task 2.3.2e: Test the 429-mid-walk scenario (last Acceptance Criterion above): confirm zero advancement, zero cache write, and that `classify_openai_error` is never invoked for that candidate (~4 min)
- Files: `src/providers/openai/resolution.rs`

#### Story 2.3.3: Failure-triggered cache invalidation + secondary TTL safety net
**As** `OpenaiProvider`, **I want** the cache to invalidate when a real (non-probe) request against the cached model fails with `Deprecated`, and to have a secondary long TTL as a safety net, **so that** recovery is reactive (not a blind poll) but a silently-un-deprecated or newly-available model still eventually gets re-checked.
**Acceptance Criteria**:
- A real production request against the cached `"gpt-5.2-codex"` that fails `Deprecated` invalidates the cache entry for that family; the *next* request for that family triggers a fresh resolution walk.
  - *Given* `ResolutionCache["gpt-5"] = ResolvedModel { model_id: "gpt-5.2-codex", .. }` and a real request against it returns a `Deprecated`-classified 400, *When* that response is processed, *Then* `ResolutionCache.remove("gpt-5")` is called before returning the error, and the subsequent request for `"gpt-5"` re-runs Story 2.3.2's walk.
- A cache entry older than the secondary TTL (default 1 hour, per Unresolved Questions) is treated as stale on next access and triggers re-resolution even without an observed failure. This TTL is also the backstop for a `WrongEndpoint` misclassification surviving into steady state (e.g. a model that becomes Responses-only after being cached as chat/completions) — there is no separate real-request re-check for that case beyond this TTL (architecture-review.md nitpick, noted explicitly here rather than left implicit).
  - *Given* `ResolutionCache["gpt-5"].resolved_at` is 2 hours in the past and the TTL constant is 1 hour, *When* the next request for `"gpt-5"` reads the cache, *Then* it treats the entry as a miss and re-runs resolution (using `tokio::time::pause()`/`advance()` in the test, per pitfalls.md §5's flakiness warning — no real sleeps).
**Files**: `src/providers/openai/resolution.rs`

##### Task 2.3.3a: Invalidate-on-`Deprecated`-failure for real (non-probe) requests against the cached model (~4 min)
- Files: `src/providers/openai/resolution.rs`

##### Task 2.3.3b: TTL-on-read check (`resolved_at.elapsed() > RESOLUTION_TTL`) before using a cached entry (~3 min)
- Files: `src/providers/openai/resolution.rs`

##### Task 2.3.3c: Tests for both criteria using `tokio::time::pause()`/`advance()`, not real sleeps (~5 min)
- Files: `src/providers/openai/resolution.rs`

#### Story 2.3.4: Resolution failures never trip `Router`/`HealthRegistry` state
**As** the router's existing health/cooldown machinery, **I want** model-resolution failures scoped strictly to `OpenaiProvider`'s internal cache, **so that** a resolution false-positive doesn't compound into cross-upstream health degradation (pitfalls.md §3's explicit warning — pitfalls.md calls this "the single most important thing to guard").
**Acceptance Criteria**:
- A resolution exhaustion event (Story 2.3.2b) does not change any `src/routing/health.rs` counter for the upstream.
  - *Given* a route with a `model_family`-configured upstream whose entire candidate list is exhausted, *When* `Router::dispatch` handles the resulting `ProviderError::Upstream`, *Then* `HealthRegistry`'s per-upstream state (verified by reading its counters before/after in the test) is unchanged from before the exhaustion — the error is treated exactly as any other `Upstream{..}` error already is today, with no new special-casing added to `health.rs`.
- **This test failing blocks Phase 2 completion.** It is not a "file a finding and move on" outcome (adversarial-review.md Concern: given pitfalls.md names this the top risk, an escape hatch here would ship the exact regression the project is trying to avoid).
**Files**: `src/routing/health.rs` (test only — no production code change expected; this story is a verification gate, not a feature)

##### Task 2.3.4a: Write the before/after `HealthRegistry` state-unchanged test described above (~5 min)
- Files: `src/routing/router.rs` (or `health.rs`, wherever existing `dispatch_*` integration tests for `Upstream{..}` errors already live — follow the existing `dispatch_should_attribute_exhausted_kind_to_dashboard_counters` test's structure, `src/routing/router.rs:2773`)

##### Task 2.3.4b: If the test fails (i.e. `Upstream{..}` errors do trip something unexpected today), this blocks Phase 2 sign-off — fix the leak (in `health.rs`/`router.rs`'s dispatch handling) before proceeding to Phase 3/4/5, do not defer it as a documented-but-unfixed finding (~2 min to detect; fix time depends on what's found)
- Files: TBD based on what the test finds

#### Story 2.3.5: Inter-candidate probe spacing
**As** the resolution loop, **I want** a fixed delay between consecutive candidate probes within one walk, mirroring `src/routing/capability.rs`'s `EVAL_PROBE_SPACING_SECS` precedent (already `pub`, value `3`), **so that** a cold-cache walk against a multi-candidate family doesn't fire a correlated burst an upstream's rate limiter reads as abuse — self-inflicting the exact "every candidate looks dead" failure mode requirements.md's Rabbit Hole #3 warns against (adversarial-review.md Blocker 4; the original plan cited `capability.rs`'s precedent by name without actually implementing the spacing it names).
**Acceptance Criteria**:
- Consecutive candidate probes within a single walk are separated by at least `capability::EVAL_PROBE_SPACING_SECS` of (simulated) time.
  - *Given* a walk over 3 candidates where the first two both fail `Deprecated`, *When* timed with `tokio::time::pause()`/`advance()`, *Then* at least `EVAL_PROBE_SPACING_SECS` elapses between the first and second probe, and between the second and third.
**Files**: `src/providers/openai/resolution.rs`

##### Task 2.3.5a: Add a `tokio::time::sleep(Duration::from_secs(capability::EVAL_PROBE_SPACING_SECS))` between candidates in Task 2.3.2a's walk loop, reusing the existing `pub` constant directly rather than duplicating its value (~4 min)
- Files: `src/providers/openai/resolution.rs`

##### Task 2.3.5b: Test using `tokio::time::pause()`/`advance()` confirming the spacing, not a real-time-based flaky assertion (~4 min)
- Files: `src/providers/openai/resolution.rs`

---

## Phase 3: Responses API Support

### Epic 3.1: Module Restructure — superseded, done in Epic 1.4

The `openai.rs` → `openai/mod.rs` + `openai/resolution.rs` + `openai/responses.rs` split (originally scoped here as Story 3.1.1) was moved to Epic 1.4 and performed at the *start* of Phase 1, before Phase 2's resolution code exists — see the Tech Debt Disposition table and Epic 1.4 for the full rationale. No work remains in this epic; `src/providers/openai/responses.rs` already exists as an empty stub by the time Phase 3 begins, ready for Epic 3.2's translation code.

---

### Epic 3.2: Non-streaming Responses Translation

#### Story 3.2.1: Anthropic request → Responses API `input` translation
**As** `OpenaiProvider`, **I want** to translate an Anthropic-shaped request body into a Responses API `input` array, **so that** non-streaming text requests can reach a Responses-only model.
**Acceptance Criteria**:
- A simple single-turn Anthropic request (`messages: [{role: "user", content: "hi"}]`) translates to a Responses API body with `input` as a string or single-item array (per OpenAI's flat-string shorthand for simple cases) and `model` set correctly.
  - *Given* `translate_anthropic_request_to_responses(json!({"model": "gpt-5.3-codex", "messages": [{"role": "user", "content": "hi"}], "max_tokens": 100}))`, *When* called, *Then* the result has `result["model"] == "gpt-5.3-codex"` and `result["input"]` represents the single user turn (exact shape — flat string vs. typed array — decided during implementation against captured real Responses API request examples).
- Shared Anthropic-side normalization helpers (`anthropic_blocks_to_openai`-equivalent) are reused, not duplicated, for the input side (per architecture.md §2).
  - *Given* a multi-block Anthropic message containing both text and a prior `tool_result` block, *When* translated, *Then* the tool-result block maps to a `function_call_output` item using the same block-walking logic already used for chat/completions' tool-message translation, not a hand-rolled duplicate.
**Files**: `src/providers/openai/responses.rs`, `src/providers/mod.rs` (extraction of shared input-side helpers, if any are pulled out)

##### Task 3.2.1a: Implement `translate_anthropic_request_to_responses(anthropic: Value) -> Value` for the single-turn, no-tools case (~5 min)
- Files: `src/providers/openai/responses.rs`

##### Task 3.2.1b: Extend to multi-turn conversations (prior assistant/tool turns in `messages`) (~5 min)
- Files: `src/providers/openai/responses.rs`

##### Task 3.2.1c: Extend to tool definitions (`tools` → Responses API's tool-definition shape) (~5 min)
- Files: `src/providers/openai/responses.rs`

##### Task 3.2.1d: Unit tests for single-turn, multi-turn, and tool-definition translation, using the existing `translate_anthropic_request_to_openai` test suite's style as a template (~5 min)
- Files: `src/providers/openai/responses.rs`

#### Story 3.2.2: Responses API response → Anthropic translation
**As** `OpenaiProvider`, **I want** to translate a non-streaming Responses API response (`output[]` items) into Anthropic's `content` block array, **so that** callers see a normal Anthropic-shaped response regardless of which OpenAI endpoint served it.
**Acceptance Criteria**:
- A Responses API response with `output: [{"type": "message", "content": [{"type": "output_text", "text": "hello"}]}]` translates to an Anthropic response with `content: [{"type": "text", "text": "hello"}]`.
  - *Given* `translate_responses_response_to_anthropic(json!({"output": [{"type": "message", "content": [{"type": "output_text", "text": "hello"}]}], "usage": {...}}))`, *When* called, *Then* `result["content"] == [{"type": "text", "text": "hello"}]`.
- A `function_call` output item translates to an Anthropic `tool_use` content block.
  - *Given* an `output` array containing `{"type": "function_call", "call_id": "call_1", "name": "get_time", "arguments": "{}"}`, *When* translated, *Then* the result contains a `{"type": "tool_use", "id": "call_1", "name": "get_time", "input": {}}` block.
**Files**: `src/providers/openai/responses.rs`

##### Task 3.2.2a: Implement `translate_responses_response_to_anthropic` for the plain-text `message` item case (~5 min)
- Files: `src/providers/openai/responses.rs`

##### Task 3.2.2b: Extend for `function_call` items → `tool_use` blocks (~5 min)
- Files: `src/providers/openai/responses.rs`

##### Task 3.2.2c: Extend for `reasoning` items → Anthropic `thinking` blocks (see Epic 3.5 for streaming; this task is the non-streaming case) (~5 min)
- Files: `src/providers/openai/responses.rs`

##### Task 3.2.2d: Unit tests mirroring `src/providers/mod.rs`'s existing `translate_openai_response_to_anthropic` test naming style (~5 min)
- Files: `src/providers/openai/responses.rs`

#### Story 3.2.3: Wire the non-streaming Responses send path into `OpenaiProvider::send`
**As** `OpenaiProvider`, **I want** a `send_responses_request` method and a decision point in `send()` for when to use it, **so that** a resolved-as-Responses-only model actually gets routed to `/v1/responses`.
**Acceptance Criteria**:
- Given a `ResolvedModel` with `endpoint: Endpoint::Responses`, a non-streaming `send()` call posts to `{base_url}/v1/responses`, not `/v1/chat/completions`.
  - *Given* `ResolutionCache["gpt-5"] = ResolvedModel { model_id: "gpt-5.3-codex", endpoint: Endpoint::Responses, .. }`, *When* a non-streaming request for family `"gpt-5"` is sent, *Then* the HTTP request URL is `{base_url}/v1/responses`.
**Files**: `src/providers/openai/mod.rs`

##### Task 3.2.3a: Implement `send_responses_request(&self, body: Value) -> Result<Value, ProviderError>`, mirroring `send_request` (`openai/mod.rs:129-`) but posting to `/v1/responses`, reusing `build_headers`/`map_error_status` (~5 min)
- Files: `src/providers/openai/mod.rs`

##### Task 3.2.3b: Add the endpoint-choice branch inside `send()`, based on the resolved/cached `Endpoint` (~4 min)
- Files: `src/providers/openai/mod.rs`

##### Task 3.2.3c: Integration-style test: full round trip from Anthropic request → Responses-shaped HTTP call (via a mock HTTP server) → Anthropic response (~5 min)
- Files: `src/providers/openai/mod.rs` (test module) or a new integration test file

---

### Epic 3.3: Streaming Responses Translation

#### Story 3.3.1: `ResponsesToAnthropicStream` — `item_id`-keyed state machine
**As a** streaming client, **I want** Responses API SSE events translated into Anthropic SSE events, **so that** streaming works identically to the chat/completions path from the client's perspective.
**Acceptance Criteria**:
- A `response.output_text.delta` event for a `message` item produces an Anthropic `content_block_delta` (text) event; a `response.output_item.added` for a `function_call` item followed by `response.function_call_arguments.delta` events produces Anthropic `content_block_start`/`content_block_delta` (tool_use) events keyed by that item's `ItemId`, independent of any concurrently-streaming `reasoning` item's events.
  - *Given* a captured (redacted) real SSE sequence: `response.created` → `response.output_item.added` (message) → `response.output_text.delta` ×3 → `response.output_item.done` → `response.completed`, *When* streamed through `ResponsesToAnthropicStream`, *Then* the emitted Anthropic events are `message_start` → `content_block_start` → `content_block_delta` ×3 → `content_block_stop` → `message_delta` (`stop_reason: end_turn`) → `message_stop`.
  - *Given* a captured sequence with an interleaved `reasoning` item's deltas arriving between two `function_call` item's deltas (both items independently `added` before either completes), *When* streamed, *Then* each item's Anthropic content block is correctly attributed to its own `ItemId`-derived block index — no cross-item content mixing.
**Files**: `src/providers/openai/responses.rs`

##### Task 3.3.1a: Define the `ResponsesToAnthropicStream<S>` struct with `item_id -> block_index` map (`HashMap<String, usize>`, replacing `OpenaiToAnthropicStream`'s flat `ToolSlot` int-index remap) (~5 min)
- Files: `src/providers/openai/responses.rs`

##### Task 3.3.1b: Implement the `response.output_item.added`/`response.output_text.delta`/`response.output_item.done` match arms for the plain-text `message` case (~5 min)
- Files: `src/providers/openai/responses.rs`

##### Task 3.3.1c: Implement the `function_call` item's `response.function_call_arguments.delta`/`.done` arms → Anthropic `tool_use` block deltas (~5 min)
- Files: `src/providers/openai/responses.rs`

##### Task 3.3.1d: Implement `response.completed` → Anthropic `message_delta`/`message_stop`, mapping Responses' finish/stop signal (~4 min)
- Files: `src/providers/openai/responses.rs`

##### Task 3.3.1e: Tests using a captured (redacted) real SSE fixture for the plain-text case (per Epic 6.1's fixture-capture story) — write against the fixture placeholder now, wire the real fixture once Epic 6.1 lands it (~5 min)
- Files: `src/providers/openai/responses.rs`

##### Task 3.3.1f: Test the interleaved multi-item ordering scenario with synthetic (hand-constructed, clearly-fake) SSE frames (~5 min)
- Files: `src/providers/openai/responses.rs`

#### Story 3.3.2: Mid-stream application-level error handling
**As a** streaming client, **I want** a `response.failed`/`response.error` event mid-stream to surface as a recognizable error, **so that** a coding-agent tool loop doesn't silently see a truncated response with no error indication (pitfalls.md §2).
**Acceptance Criteria**:
- A `response.failed` event arriving after some content has already streamed produces an Anthropic-recognizable error signal (an `error` SSE event, or at minimum a `message_delta` with a distinguishing `stop_reason` such as `"error"` — exact choice made during implementation against what Anthropic-compatible clients actually branch on), not a silent `end_turn`.
  - *Given* a captured/synthetic SSE sequence ending in `response.failed` (with an error payload) instead of `response.completed`, *When* streamed through `ResponsesToAnthropicStream`, *Then* the terminal Anthropic event is distinguishable from the normal `end_turn` completion path (assert the `stop_reason` or event type differs from Story 3.3.1's normal-completion assertion).
**Files**: `src/providers/openai/responses.rs`

##### Task 3.3.2a: Add the `response.failed`/`response.error` match arm, distinct from the existing transport-error (`Poll::Ready(Some(Err(e)))`) path (~5 min)
- Files: `src/providers/openai/responses.rs`

##### Task 3.3.2b: Test both the new mid-stream-application-error path and confirm it's distinguishable from Story 3.3.1's normal completion in the test assertions (~4 min)
- Files: `src/providers/openai/responses.rs`

---

### Epic 3.4: Tool Use Round-trip (both directions)

#### Story 3.4.1: Outbound — Anthropic `tool_use`/`tool_result` → Responses `function_call`/`function_call_output`
**As** `OpenaiProvider`, **I want** a full two-turn tool-use conversation to translate correctly in both directions, **so that** coding-agent tool loops work end-to-end against Responses-only models.
**Acceptance Criteria**:
- Turn 1 (model emits a tool call): a Responses API response with a `function_call` item translates to an Anthropic `tool_use` block (already covered by Story 3.2.2's second criterion — cross-referenced here for the full-loop test).
- Turn 2 (client sends the tool result back): an Anthropic request whose `messages` includes a `tool_result` block for that `tool_use` id translates to a Responses API `input` array containing a `function_call_output` item with the matching `call_id`.
  - *Given* an Anthropic request with a prior assistant `tool_use` block (`id: "call_1"`) followed by a user message containing `{"type": "tool_result", "tool_use_id": "call_1", "content": "12:00 UTC"}`, *When* `translate_anthropic_request_to_responses` runs, *Then* the resulting `input` array contains `{"type": "function_call_output", "call_id": "call_1", "output": "12:00 UTC"}`.
**Files**: `src/providers/openai/responses.rs`

##### Task 3.4.1a: Implement the `tool_result` block → `function_call_output` item translation in `translate_anthropic_request_to_responses` (~5 min)
- Files: `src/providers/openai/responses.rs`

##### Task 3.4.1b: Full two-turn round-trip test: turn 1 response parsed → turn 2 request built from it → assert `call_id` matches end-to-end (pitfalls.md's explicit warning that this bug class only manifests on turn two) (~5 min)
- Files: `src/providers/openai/responses.rs`

#### Story 3.4.2: Streaming tool_use round trip
**As a** streaming coding-agent client, **I want** the same two-turn tool-use round trip to work when the first turn is streamed, **so that** streaming and non-streaming tool_use parity holds (Success Metrics).
**Acceptance Criteria**:
- A streamed `function_call` item (via `ResponsesToAnthropicStream`, Story 3.3.1c) produces a `tool_use` block whose `id` matches what Story 3.4.1's second-turn translation expects as `call_id`.
  - *Given* a streamed Responses API tool-call sequence completing with `call_id: "call_2"`, *When* the resulting Anthropic stream's final assembled message is fed back into `translate_anthropic_request_to_responses` as a prior turn, *Then* the `tool_use_id`/`call_id` round-trips unchanged.
**Files**: `src/providers/openai/responses.rs`

##### Task 3.4.2a: Integration test chaining Story 3.3.1's streaming output into Story 3.4.1's request-building input (~5 min)
- Files: `src/providers/openai/responses.rs`

---

### Epic 3.5: Reasoning-item Passthrough

#### Story 3.5.1: Reasoning output item → Anthropic `thinking` block (non-streaming and streaming)
**As a** client relying on extended-thinking display, **I want** Responses API `reasoning` items to pass through as Anthropic `thinking` blocks, **so that** reasoning-item parity holds per the Success Metrics.
**Acceptance Criteria**:
- A non-streaming Responses response containing a `reasoning` output item with a `summary` translates to an Anthropic `thinking` content block (with a `signature` field populated per Anthropic's `thinking` block contract — exact signature-generation strategy, e.g. a placeholder/opaque passthrough token, decided during implementation since Responses API reasoning items don't carry an Anthropic-compatible signature natively).
  - *Given* `output: [{"type": "reasoning", "summary": [{"type": "summary_text", "text": "Let me think..."}]}]`, *When* translated, *Then* the result contains a content block `{"type": "thinking", "thinking": "Let me think...", "signature": <non-empty>}`.
- The streaming case (Story 3.3.1's `ResponsesToAnthropicStream`) emits the equivalent `thinking`-typed `content_block_start`/`delta`/`stop` sequence for a streamed `reasoning` item.
  - *Given* a streamed `reasoning` item's added/delta/done events, *When* processed, *Then* the emitted Anthropic content block has `type: "thinking"`, not `type: "text"`.
**Files**: `src/providers/openai/responses.rs`

##### Task 3.5.1a: Implement the non-streaming `reasoning` → `thinking` block translation (builds on Task 3.2.2c) (~4 min)
- Files: `src/providers/openai/responses.rs`

##### Task 3.5.1b: Implement the streaming `reasoning` item arms in `ResponsesToAnthropicStream` (~5 min)
- Files: `src/providers/openai/responses.rs`

##### Task 3.5.1c: Tests for both, confirming `thinking`-typed blocks are emitted (not mislabeled as `text`, per pitfalls.md §2's explicit warning) (~5 min)
- Files: `src/providers/openai/responses.rs`

---

### Epic 3.6: Endpoint Routing Decision

#### Story 3.6.1: `WrongEndpoint` classification retries the same candidate against `/v1/responses`
**As** the resolution loop, **I want** a `WrongEndpoint`-classified failure to retry the *same* model id against `/v1/responses` instead of advancing to the next candidate, **so that** newer Responses-only models resolve correctly instead of being wrongly treated as "dead."
**Acceptance Criteria**:
- Given candidate `"gpt-5.3-codex"` returns a 404 classified `WrongEndpoint` on `/v1/chat/completions`, resolution retries the same id against `/v1/responses`; if that succeeds, the cache entry is written with `endpoint: Endpoint::Responses` and the candidate list is *not* advanced past `"gpt-5.3-codex"`.
  - *Given* a scripted upstream returning `WrongEndpoint` for `POST /v1/chat/completions` with `model: "gpt-5.3-codex"` and success for `POST /v1/responses` with the same model, *When* resolution runs, *Then* `ResolutionCache["gpt-5"] == ResolvedModel { model_id: "gpt-5.3-codex", endpoint: Endpoint::Responses, .. }`.
**Files**: `src/providers/openai/resolution.rs` (walk loop), `src/providers/openai/mod.rs` (`send_responses_request` call site)

##### Task 3.6.1a: Update Story 2.3.2's walk loop (`resolution.rs`): on `WrongEndpoint`, retry the same candidate via `send_responses_request` (Story 3.2.3a, in `openai/mod.rs`) before deciding success/failure for that candidate (~5 min)
- Files: `src/providers/openai/resolution.rs`, `src/providers/openai/mod.rs`

##### Task 3.6.1b: Update the Story 2.3.2's Acceptance Criterion #3 placeholder test to assert the real end-to-end behavior now that Epic 3.2's Responses send path exists (~4 min)
- Files: `src/providers/openai/resolution.rs`

---

## Phase 4: `max_tokens` → `max_completion_tokens` Compatibility

### Epic 4.1: Combined Model+Compat Probe

#### Story 4.1.1: One resolution probe determines both "model alive" and "needs `max_completion_tokens`"
**As** the resolution loop, **I want** the same probe attempt that validates a candidate's liveness to also determine its token-param style, **so that** the two facts stay in sync in one cache entry rather than drifting via two independently-invalidated caches (features.md's Edge Case 5).
**Acceptance Criteria**:
- A probe against a candidate that rejects `max_tokens` with a 400 whose message names `max_completion_tokens` retries the same probe once with `max_completion_tokens` substituted; success on retry sets `TokenParamStyle::MaxCompletionTokens` on the resulting `ResolvedModel`, all within the same resolution round (no separate cache write).
  - *Given* a scripted upstream returning 400 `{"error":{"message":"Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead."}}` for the first probe attempt on `"gpt-5.1"`, and success on a retried probe with `max_completion_tokens` substituted, *When* resolution runs, *Then* `ResolutionCache["gpt-5"].token_param == TokenParamStyle::MaxCompletionTokens` and exactly one cache write occurred for that resolution round.
**Files**: `src/providers/openai/resolution.rs`

##### Task 4.1.1a: Add a `max_tokens`-vs-`max_completion_tokens` classification branch to `classify_openai_error` or a sibling function (message substring: `"max_completion_tokens"`) (~4 min)
- Files: `src/providers/openai/resolution.rs`

##### Task 4.1.1b: Wire the retry-with-substituted-param logic into Story 2.3.2's per-candidate probe step (~5 min)
- Files: `src/providers/openai/resolution.rs`

##### Task 4.1.1c: Test the retry-and-cache-together behavior (~5 min)
- Files: `src/providers/openai/resolution.rs`

#### Story 4.1.2: Apply `TokenParamStyle` as a post-processing step inside `OpenaiProvider::send`, not by widening the shared translator
**As** `OpenaiProvider`, **I want** the resolved `TokenParamStyle` applied to the body `translate_anthropic_request_to_openai` already produces, **so that** real (non-probe) requests use the correct param key — **without** changing that function's signature, since it is not `OpenaiProvider`-private: it is also called from `OpenrouterProvider::send` (`src/providers/openrouter/mod.rs:605`), with its own existing test coverage (`src/providers/mod.rs:1703,1720,2379,2387`) that must stay untouched (architecture-review.md Concern — the original plan's Files list for this story omitted `openrouter/mod.rs` entirely, understating the change's real blast radius).
**Acceptance Criteria**:
- For a `model_family`-resolved upstream whose cached `TokenParamStyle` is `MaxCompletionTokens`, the outgoing request body has `max_completion_tokens` set and no `max_tokens` key; for a static-`model` (non-resolving) upstream, and for every `OpenrouterProvider` call, behavior is completely unchanged (`max_tokens` as today, `translate_anthropic_request_to_openai`'s signature and every existing call site untouched).
  - *Given* `ResolutionCache["gpt-5"].token_param == TokenParamStyle::MaxCompletionTokens` and an Anthropic request with `max_tokens: 500`, *When* `OpenaiProvider::send` builds the outgoing body for that upstream, *Then* the resulting body has `max_completion_tokens: 500` and no `max_tokens` key — achieved by renaming the key on the `Value` the shared translator already returned, inside `OpenaiProvider::send`, not inside `translate_anthropic_request_to_openai` itself.
  - *Given* a static-`model` upstream (no `model_family`, no `TokenParamStyle` known) or any `OpenrouterProvider` call, *When* the request is built, *Then* the resulting body and the shared function's signature/behavior are byte-identical to today's output — confirmed by the existing `max_tokens`-focused tests (`mod.rs:1695-1722`, and the `openrouter/mod.rs:605` call site's own tests) passing unmodified.
**Files**: `src/providers/openai.rs` (post-processing step only — `src/providers/mod.rs` and `src/providers/openrouter/mod.rs` are explicitly *not* touched by this story)

##### Task 4.1.2a: In `OpenaiProvider::send`, after calling `translate_anthropic_request_to_openai` (unchanged), apply a `if let Some(TokenParamStyle::MaxCompletionTokens) = resolved_token_param { rename "max_tokens" -> "max_completion_tokens" on the resulting Value }` step before sending (~5 min)
- Files: `src/providers/openai.rs`

##### Task 4.1.2b: Tests for both the resolved-`MaxCompletionTokens` post-processing case and the unresolved/default-`MaxTokens` no-op case, plus a regression check that `src/providers/mod.rs`'s and `src/providers/openrouter/mod.rs`'s existing `max_tokens` tests are unmodified and still pass (~4 min)
- Files: `src/providers/openai.rs`

---

## Phase 5: Observability

### Epic 5.1: Metrics

#### Story 5.1.1: Per-resolution-attempt counters
**As an** operator, **I want** a counter incremented per `ProbeAttempt` with candidate/family/outcome labels, **so that** I can see resolution happening and diagnose which candidates are failing and why.
**Acceptance Criteria**:
- Each candidate tried during a resolution walk (Story 2.3.2) increments `resolution_attempts_total` with labels for upstream, family, candidate model id, and outcome (`success`/`advance`/`retry_responses`/`transient`/`other`/`exhausted`) — `other` is a distinct label from `transient`, not folded into it, per pre-mortem.md P1 #1: an `Other`-classified 4xx on a resolving upstream means "resolution's classification table doesn't recognize this error at all," which is operationally different from a genuine transient blip, and today's label set had no way to distinguish the two, making an unrecognized real-gateway error wording invisible next to the `Exhausted` alert.
  - *Given* a resolution walk tries `"v3"` (fails, `Deprecated`) then `"v2"` (succeeds), *When* `/metrics` is queried afterward, *Then* it shows two `resolution_attempts_total` increments: one for `("model-gateway-openai", "gpt-5", "v3", "advance")` and one for `("model-gateway-openai", "gpt-5", "v2", "success")`.
  - *Given* a resolution walk aborts on `"v3"` with an `Other`-classified 401, *When* `/metrics` is queried, *Then* it shows `resolution_attempts_total{upstream="model-gateway-openai", family="gpt-5", candidate="v3", outcome="other"}` incrementing — a separate series from `outcome="transient"`, so an operator/dashboard can alert on or investigate "resolution never recognized this error" independently of "genuinely exhausted" or "auth is broken."
**Files**: `src/metrics/counters.rs`, `src/providers/openai/resolution.rs`

##### Task 5.1.1a: Add `resolution_attempts: DashMap<(String, String, String, String), AtomicU64>` (or a flatter keyed structure matching the repo's existing counter style) to `ProxyMetrics` (~5 min)
- Files: `src/metrics/counters.rs`

##### Task 5.1.1b: Call the increment from Story 2.3.2's walk loop at each candidate outcome, including the new `"other"` outcome (Task 2.3.2a) (~4 min)
- Files: `src/providers/openai/resolution.rs`

##### Task 5.1.1c: Expose the counter in `ProxyMetrics::to_json` (`src/metrics/counters.rs:336-`) (~4 min)
- Files: `src/metrics/counters.rs`

##### Task 5.1.1d: Test the counter increments and JSON exposure, including a dedicated assertion that an `Other`-classified abort increments `outcome="other"` and does *not* increment `outcome="transient"` (~4 min)
- Files: `src/metrics/counters.rs`

#### Story 5.1.2: Exhaustion signal (log + counter), self-healing not self-clearing on a timer
**As an** operator, **I want** a distinct, sticky signal when a family exhausts all candidates, **so that** I notice before assuming the feature "just works" (Observability Requirements).
**Acceptance Criteria**:
- Exhaustion increments `resolution_exhausted_total{upstream, family}` and logs at `error` level; the signal only clears (a subsequent successful resolution) — never on a timer.
  - *Given* family `"gpt-5"` on upstream `"model-gateway-openai"` exhausts all candidates, *When* `/metrics` is queried, *Then* `resolution_exhausted_total{upstream="model-gateway-openai", family="gpt-5"} >= 1` and a subsequent successful resolution for that family does not decrement or reset the counter (it's a cumulative counter — the *dashboard's* current-state flag, not this counter, is what self-clears; see Story 5.2.2).
**Files**: `src/metrics/counters.rs`, `src/providers/openai/resolution.rs`

##### Task 5.1.2a: Add `resolution_exhausted_total` counter + `tracing::error!` call at Story 2.3.2b's exhaustion path (~4 min)
- Files: `src/metrics/counters.rs`, `src/providers/openai/resolution.rs`

##### Task 5.1.2b: Add a separate, self-healing `resolution_state: DashMap<String, ResolutionState>` (per family) that *does* reset to `Newest`/`Fallback` on the next success — distinct from the cumulative counter above (~4 min)
- Files: `src/metrics/counters.rs`

##### Task 5.1.2c: Tests for the cumulative-counter-never-resets vs. state-flag-self-heals distinction (~5 min)
- Files: `src/metrics/counters.rs`

---

### Epic 5.2: Dashboard

#### Story 5.2.1: Extend `/metrics` JSON with `resolved_model`/`resolution_state`/`resolution_family`
**As the** dashboard frontend, **I want** each opted-in upstream's `/metrics` JSON entry to carry its current resolved model, state, and family, **so that** the status bar can render it and a script can identify which family an exhausted/fallback upstream is resolving without a separate config lookup.
**Acceptance Criteria**:
- `/metrics`'s `providers["model-gateway-openai"]` object gains flat, top-level `resolved_model: "gpt-5.3-codex"`, `resolution_state: "newest"` (or `"fallback"`/`"exhausted"`), and `resolution_family: "gpt-5-codex"` fields — flat siblings on the provider object, not nested under a `"resolution"` sub-object (this is the single committed JSON shape; see ux.md Surface 3, reconciled to match this story rather than the other way around) — present only for upstreams with `model_family` configured; absent (not `null`) for static-pin upstreams.
  - *Given* a resolved family with the newest candidate active, *When* `to_json()` is called, *Then* `providers["model-gateway-openai"]["resolution_state"] == "newest"` and `providers["model-gateway-openai"]["resolution_family"] == "gpt-5-codex"`.
  - *Given* an exhausted family, *When* `to_json()` is called, *Then* `resolution_family` is still populated (even though `resolved_model` is absent/stale) so a consumer can identify which family to check — this is the JSON-side half of ux.md's "no dead end" cross-surface rule.
- No `attempts[]`/per-candidate-history array is added to this per-upstream object — per-attempt classification history is already exposed via `resolution_attempts_total{upstream, family, candidate, outcome}` (Story 5.1.1), and the Domain Glossary's `ProbeAttempt` entry already commits to not persisting attempt history beyond that counter; embedding a duplicate array here would be a second, drifting source of the same data.
**Files**: `src/metrics/counters.rs`

##### Task 5.2.1a: Add `resolved_model`/`resolution_state`/`resolution_family` to `to_json()`'s per-upstream object, sourced from Story 5.1.2b's state map (`resolution_family` is the map's own key, already available at the call site) (~4 min)
- Files: `src/metrics/counters.rs`

##### Task 5.2.1b: Test the fields' presence (including `resolution_family` in the exhausted case) for resolving upstreams and absence for static-pin upstreams (~4 min)
- Files: `src/metrics/counters.rs`

#### Story 5.2.2: Dashboard status-bar states for fallback (calm) and exhausted (sticky red)
**As an** operator glancing at the dashboard, **I want** "resolved to a fallback" shown calmly and "exhausted" shown loudly and stickily, **so that** I don't get alert fatigue on normal fallback but never miss a real outage (ux.md §3).
**Acceptance Criteria**:
- A `resolution_state: "fallback"` upstream renders with a low-noise, non-red suffix (e.g. `" (using gpt-5.2, newest unavailable)"`), matching the existing `status-cooldown` suffix pattern (`src/dashboard.rs:369-372`); a `resolution_state: "exhausted"` upstream renders with a new `status-resolution-exhausted` red CSS class that does not disappear on the next poll unless resolution actually succeeds again.
  - *Given* `data.providers["model-gateway-openai"].resolution_state === "fallback"`, *When* `loadMetrics()` runs, *Then* the rendered `cls` is unaffected (stays `status-active`/`status-cooldown` per existing precedence) but the label suffix includes the fallback note.
  - *Given* `resolution_state === "exhausted"`, *When* `loadMetrics()` runs, *Then* `cls === "status-resolution-exhausted"`, checked **before** `cooling` in the ternary (per the existing `last_error_kind`-before-`cooling` precedence established at `src/dashboard.rs:365-368` and its accompanying regression test at `src/dashboard.rs:699`).
- The exhausted label includes the family name (e.g. "Model-gateway (all candidates failed — check gpt-5-codex family)", per ux.md Surface 2's example), sourced from Story 5.2.1's `resolution_family` field — not a generic "exhausted"/"error" label with no next action named.
  - *Given* `data.providers["model-gateway-openai"] === { resolution_state: "exhausted", resolution_family: "gpt-5-codex", ... }`, *When* `loadMetrics()` runs, *Then* the rendered label text contains `"gpt-5-codex"`.
**Files**: `src/dashboard.rs`

##### Task 5.2.2a: Add `.status-resolution-exhausted { background: #ef4444; }` CSS rule (reuse the existing red, or a distinguishable red variant) (~2 min)
- Files: `src/dashboard.rs`

##### Task 5.2.2b: Extend the `cls` ternary at `src/dashboard.rs:365-368` with a new branch checked before `cooling`, and extend the suffix logic at `369-372` for both `fallback` and `exhausted` — the exhausted suffix interpolates `resolution_family` (~5 min)
- Files: `src/dashboard.rs`

##### Task 5.2.2b-1: Test that the exhausted label's suffix contains the `resolution_family` value (~3 min)
- Files: `src/dashboard.rs`

##### Task 5.2.2c: Extend the existing precedence-order regression test (`dashboard_js_status_logic_should_check_last_error_kind_before_cooling`, `src/dashboard.rs:699`) to also cover the new `resolution_state` branch's precedence (~4 min)
- Files: `src/dashboard.rs`

##### Task 5.2.2d: Extend the cold-start non-regression test (`src/dashboard.rs:715`-area) to confirm a static-pin upstream (no `resolution_state` key at all) renders identically to today (~3 min)
- Files: `src/dashboard.rs`

---

## Phase 6: Hardening, Testing, Docs

### Epic 6.1: Test Infrastructure

#### Story 6.1.1: Synthetic-model-id fixtures for all resolution-algorithm tests
**As a** future maintainer, **I want** every resolution-ranking/candidate-walk test to use obviously-fake model ids, **so that** no test accidentally starts validating "today's real catalog snapshot" instead of the algorithm (pitfalls.md §5).
**Acceptance Criteria**:
- A repo-wide check (manual review checklist item, not automated tooling) confirms no test in Epics 2.1-2.3 asserts against a real current OpenAI model name as its pass condition.
  - *Given* the full diff for Phase 2's tests, *When* grepped for `"gpt-"` outside of comments/docs, *Then* zero matches appear in test *assertions* (fixture setup for the Deprecated/WrongEndpoint classification tests, which necessarily quote requirements.md's real captured strings, is the one accepted exception — flag those explicitly with a comment noting they're from the real motivating incident, not the algorithm's pass condition).
**Files**: (verification task, cuts across Phase 2's test files)

##### Task 6.1.1a: Review Phase 2's tests (Stories 2.1.1, 2.1.2, 2.3.2, 2.3.3) for real-model-name leakage into assertions; fix any found (~5 min)
- Files: `src/providers/openai/resolution.rs`

#### Story 6.1.2: Captured real Responses API fixtures for streaming tests
**As a** test author, **I want** at least one real (redacted) captured Responses API SSE sequence — including a tool-call and a reasoning-item stream — replayed as canned mock-server responses, **so that** streaming tests catch real-world wire-format quirks hand-written fixtures would miss (pitfalls.md §5).
**Acceptance Criteria**:
- At least one redacted, real captured SSE transcript (non-streaming and streaming) exists as a test fixture file and is used by at least one of Story 3.3.1's tests instead of a fully hand-written synthetic stream.
  - *Given* a captured transcript file `tests/fixtures/responses_api_text_stream.txt` (or equivalent), *When* Story 3.3.1's plain-text test runs, *Then* it feeds this file's bytes through `ResponsesToAnthropicStream` rather than constructing SSE frames by hand.
**Files**: new fixture file(s) under `tests/fixtures/` or `src/providers/openai/testdata/` (confirm the repo's existing fixture convention before choosing the path — `grep -rn "testdata\|fixtures" src/` first)

##### Task 6.1.2a: Capture a real (or ExampleCorp-Model-Gateway-real, redacted of auth) Responses API text-only streaming transcript — requires SBN Dev Agent/VPN access per requirements.md's Feasibility Risks; if unavailable during implementation, use OpenAI's own publicly documented example transcripts as the interim source and flag the gap explicitly (~5 min)
- Files: new fixture file

##### Task 6.1.2b: Wire the fixture into Story 3.3.1's test via a bare local HTTP server/TCP listener (not a pre-chunked `Bytes` feed) so real chunk-boundary edge cases are exercised, matching whatever pattern the existing chat/completions streaming tests use — verify that pattern first (~5 min)
- Files: `src/providers/openai/responses.rs` (test module)

##### Task 6.1.2c: Repeat for a tool-call transcript and a reasoning-item transcript (~5 min each, two tasks)
- Files: new fixture files, `src/providers/openai/responses.rs`

#### Story 6.1.3: Real-gateway integration test is documented as manual/gated, not silently skipped
**As a** future contributor, **I want** the real-ExampleCorp-Model-Gateway integration test clearly marked as requiring VPN/SBN-Dev-Agent context, **so that** it doesn't bit-rot into "compiles but never runs" without anyone noticing (pitfalls.md §5).
**Acceptance Criteria**:
- The integration test (if written) is `#[ignore]`d with a doc comment explaining exactly how to run it manually, and this plan's own docs (Story 6.2.1) mention its existence and manual-run requirement.
  - *Given* the test file, *When* `cargo test` runs without extra flags, *Then* the gateway-dependent test is skipped (not failed), and running `cargo test -- --ignored` with VPN/SBN Dev Agent active runs it.
**Files**: wherever this test is added (likely `src/providers/openai/mod.rs` or a new `tests/` integration file)

##### Task 6.1.3a: Add the `#[ignore]`d test with a clear doc comment (~4 min)
- Files: TBD per above

##### Task 6.1.3b: Cross-reference it from Story 6.2.1's docs update (~2 min)
- Files: docs file from Story 6.2.1

---

### Epic 6.2: Docs

#### Story 6.2.1: Document `model_family` and Responses API support in operator-facing docs
**As an** operator, **I want** the new field and its behavior documented, **so that** I can adopt it without reading source code.
**Acceptance Criteria**:
- The repo's config reference docs (wherever `model`'s existing doc comment / README section lives — confirm exact location, e.g. `README.md`'s config section or `references/conf.d/00-providers.toml`'s inline comments) describe `model_family`'s prefix-match semantics, the opt-in nature, and the mutual-exclusivity rule with `model`.
  - *Given* the updated docs, *When* an operator reads them, *Then* they can correctly predict that `model_family = "gpt-5"` matches `gpt-5.1-codex-max`/`gpt-5.2-codex`/`gpt-5.3-codex` but not `gpt-4-turbo`, without reading `src/providers/openai.rs`.
**Files**: `README.md` (or wherever the existing config docs live — verify), `references/conf.d/00-providers.toml` (already updated in Story 1.2.3, cross-check consistency here)

##### Task 6.2.1a: Locate the existing `model` field's operator-facing documentation (`grep -rn "RouteUpstreamRef\|model =" README.md docs/ 2>/dev/null`) and add the `model_family` section alongside it (~4 min)
- Files: TBD per grep result

##### Task 6.2.1b: Cross-check Story 1.2.3's `references/conf.d/` example and this doc section describe the same syntax (no drift) (~3 min)
- Files: same as above

##### Task 6.2.1c: Note the ExampleCorp plugin follow-up (Scope item 6) as explicitly out-of-repo-scope, pointing to the `ndotfiles` repo, consistent with requirements.md's own framing (~2 min)
- Files: same as above

##### Task 6.2.1d: Document the first-request latency cost of a cold-cache resolution walk (pre-mortem.md P1 #2): up to ~62s worst-case (5 candidates, default 60s `request_timeout`) on the single request that triggers resolution after a model deprecates or on first use of a new `model_family` route; every subsequent request for that family hits the warm cache and pays no extra latency. Cross-reference Story 2.3.2's wall-clock budget acceptance criterion so this number doesn't drift from the code if the underlying constants change (~3 min)
- Files: same as above
