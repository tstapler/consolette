# ADR-003: New error-classification function is a seam, `map_error_status`/`ProviderError` stay unchanged

**Status**: Accepted
**Date**: 2026-09-18

## Context

`map_error_status` (`src/providers/openai.rs:297-331`) collapses every non-429 client error into `ProviderError::Validation(body_str, status_u16)`, discarding the parsed OpenAI JSON error envelope (`{"error": {"type", "code", "message"}}`). `Router::dispatch` treats `Validation` as non-failover (`e.is_validation() || e.is_auth() => return Err(e)`, `src/routing/router.rs:562`). Dynamic resolution needs to distinguish "deprecated model, advance candidate" from "wrong endpoint, retry same id against `/v1/responses`" from "transient, don't advance" from "auth/malformed request, definitely don't advance" — none of which `Validation`'s single bucket can express.

`ProviderError` is deliberately a fixed, provider-agnostic vocabulary (no per-provider variants) shared by `Anthropic`/`Bedrock`/`Gemini`/`Openrouter`/`Openai` providers and consumed generically by `Router::dispatch`.

## Decision

Add a new function, private to `src/providers/openai/mod.rs`, e.g. `classify_openai_error(status: u16, body: &str) -> OpenaiErrorClass`, that independently re-parses the same body text `map_error_status` already reads (`response.text()`), producing `OpenaiErrorClass::{Deprecated, WrongEndpoint, Transient, Other}`. This function is called **only** from the new resolution loop, before it decides "advance candidate" / "retry as Responses" / "abort, surface error." `map_error_status` itself, `ProviderError::Validation`'s signature, and every other caller of `map_error_status` are **not modified** — non-opted-in (static `model` pin) requests see byte-for-byte the same error path and `ProviderError` they see today.

## Alternatives rejected

- **Widen `ProviderError::Validation` to carry a structured classification field**: rejected — this is a shared enum across every provider; adding an OpenAI-specific classification to it (even as an `Option`) leaks OpenAI's error taxonomy into the provider-agnostic contract every other provider's match arms have to account for, and risks behavior drift for `Anthropic`/`Bedrock`/`Gemini` callers that pattern-match on `Validation`'s current two-field shape.
- **Add a new `ProviderError` variant** (e.g. `ModelDeprecated`): rejected for the same reason — `gemini-provider`'s prior research already established "there's no room for a Gemini-specific error variant" as the working precedent for this enum; an OpenAI-specific variant would be the first crack in that contract.
- **Refactor `map_error_status` itself to parse and classify inline**: rejected for this project's scope — `map_error_status` is exercised by every existing OpenAI request path today (streaming and non-streaming, resolving and non-resolving); changing its behavior is a strictly larger blast radius than adding a new, additive, resolution-only function, for no benefit this project's Success Metrics require.

## Consequences

- This is the **Tech Debt Disposition**'s "Isolate via seam" choice for `map_error_status`'s known coarseness (see plan.md) — the underlying collapse-every-4xx-into-Validation issue is not fixed generally, only worked around for the resolution code path. A future project generalizing OpenAI error classification for all callers (not just resolution) would need to revisit `map_error_status` itself.
- `classify_openai_error` duplicates the "read status + body" work `map_error_status` already does — acceptable given both are single HTTP-response reads, not a hot loop; the resolution loop only calls this on a cache-miss/failure path, not on the zero-overhead cache-hit fast path (see Constraints: no per-request cost for static pins).
