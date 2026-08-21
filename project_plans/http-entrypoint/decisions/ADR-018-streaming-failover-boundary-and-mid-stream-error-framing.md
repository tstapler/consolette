# ADR-018: Streaming failover point-of-no-return and in-band mid-stream error framing

**Status**: Accepted
**Date**: 2026-08-19
**Related**: [requirements.md](../requirements.md) (Rabbit Holes: streaming-failover boundary correctness), `src/routing/router.rs` (`Router::dispatch`), [ADR-017](ADR-017-manual-body-from-stream-over-axum-sse.md)

## Context

`Router::dispatch` (`src/routing/router.rs:62-117`) contains all cross-upstream retry/failover logic internally: if the selected upstream is unhealthy, rate-limited, or returns a transient `ProviderError`, `dispatch` tries the next candidate per the configured `RoutingStrategy` before ever returning to its caller. This happens entirely before the `await` on `dispatch` resolves.

Once `dispatch` returns `Ok(ProviderResponse::Stream(s))`, the entrypoint has already committed to a single upstream's response and, for a streaming client, has typically already started writing SSE bytes onto the open HTTP connection. If that upstream's stream then errors or closes unexpectedly partway through (a "mid-stream cut"), there is no correct way to fail over to a different upstream and restart the response — the client has already received a partial, in-progress response body under Anthropic's or OpenAI's wire format, and silently switching providers mid-stream would either duplicate content or produce a malformed transcript indistinguishable from provider misbehavior.

The requirements doc explicitly flags this as the hardest correctness rabbit hole in the feature, and `pitfalls.md`'s three-way cost-tracking coverage requirement (clean success / pre-first-byte failure / mid-stream cut) depends on this boundary being drawn in exactly one place.

## Decision

The **point of no return** for streaming failover is the return of the single `Router::dispatch(...).await` call itself:

- Before `dispatch` returns, any failover between upstreams is `Router::dispatch`'s own internal responsibility (already implemented, out of scope for this feature) — the entrypoint handler never sees or is aware of a pre-dispatch retry.
- After `dispatch` returns `Ok(ProviderResponse::Stream(s))`, the entrypoint commits to that single stream for the remainder of the response. No code in the entrypoint may call `dispatch` a second time for the same client request once a `Stream` variant has been returned.
- If the stream `s` yields an `Err(_)` item partway through (a mid-stream cut), the entrypoint must **not** attempt failover and must **not** silently truncate the response. Instead it synthesizes and emits one in-band SSE `event: error` frame (Anthropic-native path) — `event: error\ndata: {"type":"error","error":{"type":"api_error","message":"upstream stream interrupted"}}\n\n` — or, for the OpenAI-compat path, a final `chat.completion.chunk` with a non-null `finish_reason` of `"stop"` preceded by a code comment noting the interruption, followed by `data: [DONE]\n\n`, so the client's stream loop terminates cleanly instead of hanging or timing out.
- The response's HTTP status code and headers were already sent (200 OK, `text/event-stream`) before the cut was known, so the error cannot be surfaced as a different HTTP status — it must be in-band, inside the already-open SSE stream. This is a direct consequence of streaming responses: the status line is the first thing written, before any body bytes exist to reveal a later failure.

## Alternatives Considered

1. **Buffer the entire provider stream before writing anything to the client, so a failure can still trigger a normal error response.** Rejected: defeats the purpose of streaming (time-to-first-byte), and `ProviderResponse::Stream` exists specifically because responses can be arbitrarily long-lived; buffering also would not match real Anthropic/OpenAI API behavior, which clients expect to stream incrementally.
2. **On mid-stream cut, attempt to call `Router::dispatch` again with a fresh request and splice the new response in.** Rejected: produces a client-visible transcript that looks like a single coherent stream but actually mixes two upstreams' outputs, which is both semantically wrong (duplicated or dropped content, mismatched `message_id`) and impossible to make byte-compatible with the real Anthropic/OpenAI wire formats those clients expect.
3. **Simply close the TCP connection / end the stream abruptly on a mid-stream error, relying on the client's own timeout/retry logic.** Rejected: `ux.md`'s findings on client SDK behavior (e.g. Claude Code's own stream-tolerance assumptions) indicate this looks indistinguishable from a network failure to some clients, which may retry the entire request against a proxy that is already struggling, rather than surfacing the interruption to the end user immediately.

## Consequences

- The entrypoint's streaming handlers (Stories 2.2.2, 2.2.3, 3.2.2) must wrap the provider stream in an adapter that inspects each item for `Err(_)` and, on first error, emits the synthesized error frame as the final item before ending the stream — this is a straightforward `Stream` combinator, not a new dependency.
- Cost tracking on a mid-stream cut (`ADR-016`) records whatever partial usage was observed before the cut, tagged `TokenSource::Estimated`, since the final `message_delta`/`message_stop` usage frame never arrives.
- This boundary is validated by an integration test (plan Phase 4) that forces a provider stream to error partway through and asserts the client receives a well-formed in-band error frame rather than a truncated or hung connection.
