# ADR-017: Manual `Body::from_stream` passthrough over `axum::response::sse::Sse` for Anthropic-native streaming

**Status**: Accepted
**Date**: 2026-08-19
**Related**: [requirements.md](../requirements.md), `src/providers/mod.rs` (`ProviderResponse::Stream`), `src/routing/router.rs`

## Context

`ProviderResponse::Stream` (`src/providers/mod.rs:20-27`) is `Pin<Box<dyn Stream<Item = Result<Bytes, anyhow::Error>> + Send>>` — raw bytes taken directly from the upstream provider's HTTP response body. For Anthropic upstreams these bytes are already a complete, correctly-framed SSE stream (`event: message_start\ndata: {...}\n\n`, etc.) produced by the real Anthropic API.

axum and axum-extra (both already dependencies) offer `axum::response::sse::Sse<S>`, which expects a `Stream<Item = Result<Event, E>>` of *parsed* `sse::Event` values and re-serializes each one back into `event: ...\ndata: ...\n\n` wire format on the way out.

For the `POST /v1/messages` (Anthropic-native) endpoint, the outbound wire format must be byte-for-byte identical to what a real Anthropic API server would send, because Claude Code and Anthropic's own client SDKs are the target consumers (per `ux.md`'s SSE-event-sequencing requirement). Using `Sse` here would require parsing every upstream frame into an `sse::Event` (splitting `event:`/`data:` lines, re-associating multi-line `data:`, handling `id:`/`retry:` fields if present) purely so that `Sse` can immediately re-serialize the same bytes back out — a round-trip that adds parsing risk (a subtly wrong re-serialization would silently corrupt the passthrough) for zero behavioral benefit on this endpoint.

## Decision

For the Anthropic-native `POST /v1/messages` streaming path, return the upstream SSE byte stream to the client via `axum::body::Body::from_stream(stream)` wrapped in a `Response` with `Content-Type: text/event-stream`, `Cache-Control: no-cache`, and `Connection: keep-alive` headers set explicitly — a direct byte-for-byte passthrough, with only the `CostTrackingStream` tee (ADR-016, Story 2.2.1) interposed to peek at `message_delta`/`message_stop` frames for usage extraction, never to rewrite them.

`axum::response::sse::Sse` is used only where it is the right tool: nowhere in this feature's Anthropic-native path, since there is no not-already-framed data to serialize. The OpenAI-compatible streaming path (`POST /v1/chat/completions`) is different in kind — it must *translate* Anthropic-shaped SSE events into OpenAI-shaped `chat.completion.chunk` JSON payloads, which is genuinely a parse-then-reserialize problem; the `OpenAiStreamTranslator` for that path uses the `eventsource-stream` crate to parse the incoming frames into a stream of typed events. Even there, plan Story 3.2.1 hand-emits `data: ...\n\n` byte chunks via `Body::from_stream` rather than `axum::response::sse::Sse`, for consistency with the Anthropic-native path and because the exact `data: [DONE]\n\n` sentinel framing OpenAI clients expect is simpler to emit directly as bytes than to coerce through `Sse`'s `Event` type.

## Alternatives Considered

1. **Parse upstream SSE into `sse::Event` and use `axum::response::sse::Sse` for both endpoints.** Rejected for `/v1/messages`: adds a parse/re-serialize round trip with no benefit for a byte-for-byte passthrough, and introduces a new class of bug (subtly-wrong re-serialization) that a raw passthrough cannot have.
2. **Use `Sse` for the OpenAI-compat path only, since it does need to construct new events.** Considered viable but rejected for consistency: `Body::from_stream` with hand-built `Bytes` chunks keeps both streaming handlers structurally identical (same response-building helper, same header set), and keeps the exact `data: [DONE]\n\n` sentinel (a raw byte literal OpenAI clients string-match on) unambiguous rather than relying on `Sse`'s own end-of-stream handling.

## Consequences

- The passthrough handler must set SSE response headers manually (`Content-Type`, `Cache-Control`, `Connection`) since `Body::from_stream` does not set them automatically the way `Sse` would.
- Any future bug in upstream SSE framing (e.g. a provider ever changing its event format) passes through to the client unchanged and undetected by consolette — acceptable, since correctness of the upstream's own SSE framing is out of scope for consolette to validate.
- The `CostTrackingStream` tee (Story 2.2.1) must do its own minimal SSE frame scanning (looking for `event: message_delta`/`event: message_stop` lines) without altering the bytes it passes through, since it sits between the raw upstream stream and `Body::from_stream`.
