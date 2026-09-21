# Research: Architecture

**Date**: 2026-09-13

## Pattern: proxy-internal agentic (tool-execution) loop

This is the standard "agentic loop" pattern (model → tool_use → execute →
tool_result → model, bounded), but hosted **inside a stateless proxy** rather
than in a client agent. Prior art: OpenRouter's own `openrouter:web_search`
server tool does exactly this (model calls tool, gateway executes search,
model grounds answer). Our loop is the same shape with the executor being
stapler-mcp instead of Exa/native search.

## Placement decision (challenge to the sketch: confirmed with refinement)

- **New module `src/server_tools/`** owns all real logic (repo "transport
  thin" rule): pure functions for detect/rewrite/accumulate/map + an async
  `SearchExecutor` trait + the loop orchestrator. Independently unit-testable
  with zero I/O.
- **Orchestration point: `src/entrypoint/messages.rs`** (`post_v1_messages`),
  NOT inside `Router::dispatch`. Reasons:
  1. Router semantics (ADR-003) stay untouched — validation/auth ⇒ immediate,
     rate-limit ⇒ cooldown, strategy-driven failover. The loop re-dispatches
     through the router per iteration, so every iteration gets full routing,
     failover, session-pin, and admission behavior for free.
  2. `ProviderResponse::{Full, Stream}` handling and `CostTrackingStream`
     already live in the entrypoint; the loop needs both.
  3. The router stays a single-shot dispatcher (its contract, tests, and
     dashboard attribution don't change shape).
- **Loop algorithm (Full path)**:
  1. Detect server web_search def in incoming body. If absent → `dispatch`
     once (today's path, untouched).
  2. Else rewrite body (swap server def → synthetic function def), `dispatch`.
  3. If response has no `web_search` tool_use → map to faithful server-tool
     turn (or plain answer if model never searched) and return.
  4. Else execute searches via `SearchExecutor`, append `tool_result`(s) to
     the message history (function-shape for re-dispatch), re-`dispatch`
     with `stream: false` internally. Repeat while new `web_search` tool_use
     appears and `iterations < cap`.
  5. Map final turn to Anthropic shape with `server_tool_use` +
     `web_search_tool_result` blocks, aggregated `usage` (sum input/output
     across iterations + `server_tool_use.web_search_requests` count).
- **History hygiene on re-dispatch**: convert our synthesized server blocks
  back to function `tool_use`/`tool_result` pairs before sending upstream
  (mirrors the mixed-turn replay hazard noted in features.md).

## Streaming (the known hard part): buffering vs mid-stream execution

- **Decision for V1: buffer-then-synthesize.** When the incoming request has
  `stream: true` AND a server tool def, dispatch internally with
  `stream: false` (loop over `Full` bodies), then synthesize a well-formed
  Anthropic SSE sequence (`message_start`, content-block deltas, `message_stop`)
  from the final assembled message. Rationale:
  - Mid-stream execution requires reassembling partial `tool_calls` JSON deltas
    from OpenAI-style SSE, pausing the client stream, executing, re-dispatching,
    and resuming — while the already-flushed bytes forbid failover and the
    `CostTrackingStream` tee already counted partial usage. Every one of those
    is a correctness cliff.
  - Buffering keeps exactly one code path for the loop (Full), with the stream
    difference confined to a pure SSE synthesizer (testable without I/O).
  - Cost: time-to-first-byte degrades to time-to-final-answer for these
    requests only. Acceptable for V1 (search requests are already slow);
    documented in the PR notes and validation plan.
- **Deferred**: true mid-stream execution (frames interleaved with live
  search). Explicit follow-up, not V1.

## Data flow & consistency

- Per-request state only (iteration count, accumulated usage, message history).
  No shared mutable state ⇒ no consistency problem; the executor pool is the
  only shared resource (bounded, timeout-guarded).
- **Routing/cooldown composition**: each loop iteration is a fresh
  `Router::dispatch` (same session ⇒ same pin). A search-round-trip failure is
  NOT a `ProviderError` and never touches `HealthRegistry` — executor errors
  become error-content `tool_result`s inside the turn. Only genuine provider
  errors from `dispatch` flow through the normal ADR-003 paths.
- **Cost/count tracking**: accumulate `usage.input_tokens/output_tokens` across
  iterations (sum), record each round-trip in the existing `CostTracker` under
  the same session/request id, add `server_tool_use.web_search_requests`.
  Executor latency is observed as intra-request latency (metric), not as model
  tokens.

## Capability detection (which upstreams get emulation)

- Rule: emulate iff the incoming request contains a server web_search def AND
  the selected route's upstream kind is NOT natively executing (V1: everything
  except `UpstreamKind::Anthropic`; Bedrock keeps current behavior pending a
  follow-up spike — recorded as an open decision in plan.md).
- The Cohere drop-fix in `translate_tool_definition` stays as the safety net
  underneath (emulation supersedes it when the backend succeeds; drop remains
  the degrade path).
