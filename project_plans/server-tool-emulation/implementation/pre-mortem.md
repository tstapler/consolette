# Pre-mortem: Server-Tool Emulation (web_search)

**Date**: 2026-09-13 — written before implementation; assume the feature
shipped and failed, then work backward.

## Assumed failure 1: runaway spend on a looping model

- **Story**: a model in a research-heavy session called `web_search` 10× per
  turn across hundreds of turns; the bill spiked before anyone noticed.
- **Prevention already in plan**: hard ceiling 10, default 5, per-request
  deadline, `server_tool_use.web_search_requests` in usage, per-iteration
  `CostTracker` records. **Gap to close in implementation**: alert on
  `server_tool_searches_total` rate in the operator's dashboard (noted, not a
  code task in V1).

## Assumed failure 2: stapler-mcp upgrade renames the tool

- **Story**: `brave_web_search` renamed/restructured upstream; every search
  request silently degraded to drop for a week before anyone noticed.
- **Prevention**: contract test pins tool name + shapes (fails fast in CI);
  degrade path logs + meters (`outcome=backend_error`). **Gap**: add a
  startup probe log line at proxy boot (emulation enabled, backend reachable:
  yes/no) so "silently degraded since restart" is visible in logs.

## Assumed failure 3: streaming client hangs on buffered search

- **Story**: Claude Code with a long search turn hit a client-side read
  timeout waiting for the first SSE byte (buffered V1 holds the stream).
- **Prevention**: document TTFB tradeoff in PR notes; keep a `server_tools
  .stream_mode = "buffered"` knob reserved so a future `passthrough_fallback`
  (abort buffering after N seconds and emit what exists) can land without
  config-shape churn. Consider emitting SSE `ping`/comment keepalives while
  buffering if the client tolerates them (spike in implementation).

## Assumed failure 4: double-billed tokens erode trust

- **Story**: usage dashboard showed 2× actual tokens for search turns; owner
  stopped trusting cost metrics.
- **Prevention**: V-STREAM-03 single-count assertion + sum-of-parts == total
  invariant in both Full and Stream e2e tests (validation.md).

## Assumed failure 5: domain filters silently ignored cause wrong answers

- **Story**: user set `allowed_domains: [sec.gov]`, got general-web answers,
  filed a correctness bug.
- **Prevention**: per-request log line when filters are ignored (plan T3.1.3a);
  lossy-mapping table in code comments + PR description. V1 explicitly does
  not implement server-side domain filtering (Brave API passthrough is a
  follow-up).

## Top residual risk (carry into implementation)

Streaming TTFB (failure 3) is the likeliest user-visible complaint; everything
else is contained by tests. If implementation finds buffering unacceptable in
manual testing, the fallback is NOT mid-stream execution (deferred) but a
documented limitation: search + stream works, first byte waits for the final
answer.
