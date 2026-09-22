# ADR-001: Dynamic model resolution lives inside `OpenaiProvider`, not a decorator or the router

**Status**: Accepted
**Date**: 2026-09-18

## Context

Requirements call for opt-in, per-`RouteUpstreamRef` dynamic model resolution: on a cache miss or a classified-deprecated failure, walk an upstream's own `/v1/models` catalog (filtered by a configured family prefix) trying candidates newest-first until one works, then cache the winner per family.

Three places could own this loop:
1. Inside `OpenaiProvider::send`/`send_request` (`src/providers/openai.rs`).
2. A new `ResolvingOpenaiProvider` decorator implementing `Provider`, wrapping an inner `OpenaiProvider`.
3. Inside `Router::dispatch` (`src/routing/router.rs`), as a new `UpstreamKind`-conditional branch.

## Decision

Resolution lives inside `OpenaiProvider`, entirely beneath the `Provider` trait boundary (`src/providers/mod.rs:154-`). `OpenaiProvider::send` may issue several HTTP requests (probe candidate 1, candidate 2, ..., the real request) before returning once to `Router::dispatch`, exactly as `RouteUpstreamRef.model` already overrides `body["model"]` today as a pre-HTTP-call mutation invisible to the router.

## Alternatives rejected

- **Decorator (`ResolvingOpenaiProvider`)**: architecturally cleaner in isolation, but `OpenaiProvider` currently keeps `client`, `stream_client`, `base_url`, `resolver`, `exec_cache` (`src/providers/openai.rs:40-54`) private with no accessors. A decorator would either force those fields public (a real API-surface cost for a single caller) or stand up its own duplicate `reqwest::Client` pair — real duplication, not composition. Rejected unless `OpenaiProvider` grows public accessors for an unrelated reason first.
- **Router-level branch**: `Router` is explicitly kind-agnostic (existing `openrouter-routing` ADR-003 precedent). Model-catalog resolution — calling `/v1/models`, understanding OpenAI's own naming/versioning conventions — has no meaning for `AnthropicProvider`/`BedrockProvider`/`GeminiProvider`. Adding a `UpstreamKind`-conditional to `dispatch` or a new `Provider` trait method just for this one kind breaks the trait's kind-agnostic contract for every other implementor. Rejected outright.

## Consequences

- Zero changes to `Router::dispatch`, `already_tried`, or `src/routing/health.rs` — resolution failures never trip upstream-level health/cooldown circuitry (verified by a dedicated test, Story 2.3.4).
- `OpenaiProvider`'s existing per-request timeout budget must be split (or a shorter timeout carved out) to cover a probe-then-real-request sequence within one `send()` call — tracked as Task 2.3.2c.
- `OpenaiProvider` gains responsibility beyond "translate and forward one request." Accepted as the smaller cost versus the alternatives' field-privacy or kind-agnostic-contract costs.
