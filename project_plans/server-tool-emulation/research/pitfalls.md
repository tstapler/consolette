# Research: Pitfalls

**Date**: 2026-09-13

## P1 — Infinite / expensive loops (severity: high)

- A model that always answers with another `web_search` call burns tokens and
  search calls unboundedly. **Mitigations (all required)**: hard iteration cap
  (`min(max_uses, configured max ≤ hard ceiling, default 5)`), total
  loop-deadline (e.g. 120s default), per-search timeout (e.g. 15s), result-size
  cap with truncation marker. All four are config knobs with safe defaults.
  Validation must include a "model always searches" test asserting termination.

## P2 — Poisoning upstream health (severity: high)

- If a search-round-trip failure were classified as a provider error, the
  router would trip cooldown / fail over spuriously, degrading healthy
  upstreams. **Rule**: executor outcomes NEVER become `ProviderError`; they
  become `tool_result` content (success or error). No `HealthRegistry` writes
  from the emulation path. Test: executor failure ⇒ same upstream reused next
  iteration, no cooldown entry.

## P3 — Streaming contract breakage (severity: high)

- Clients (Claude Code) parse SSE strictly. Risks: emitting `tool_use`
  blocks with `stop_reason != tool_use` (known stall class, already handled
  in `translate_openai_response_to_anthropic`), emitting our synthetic
  function `tool_use` on the wire (leaks internals), emitting zero content
  blocks. **Mitigations**: the SSE synthesizer emits only native server-tool
  shapes (`server_tool_use`, `web_search_tool_result`, `text`), always ends
  `end_turn` on success, and is golden-tested frame-by-frame.

## P4 — Credential leakage (severity: high)

- `BRAVE_API_KEY` must never transit consolette. **Rules**: keys live only in
  the daemon's env (inherited at daemon spawn); consolette config carries only
  a binary path + timeouts; `redact_bodies` key list reviewed for any new
  logged field; error bodies from the executor are sanitized (no env, no
  socket paths) before reaching the client. Test: config snapshot + log
  redaction assertions.

## P5 — Fidelity gaps mistaken for bugs (severity: medium)

- No `encrypted_content`/`encrypted_index`, no `citations` attachments, no
  dynamic filtering, domain/location filters logged-and-ignored in V1. Users
  WILL file these as bugs. **Mitigation**: document the lossy-mapping table
  in the PR description and code comments; emit a `note`-style log per request
  when filters are ignored (not in the client body).

## P6 — stapler-mcp backend unavailable (severity: medium; expected, not edge)

- Daemon not running, binary missing, socket stale, search times out, tool
  renamed upstream. **Rule**: degrade to today's drop behavior — strip the
  server def, dispatch once, return the answer without search blocks. NEVER
  fail the whole request for a search-backend outage. Startup probe +
  per-call timeout + consecutive-failure circuit breaker (open → skip
  emulation fast-path for N seconds). Test every degrade branch.

## P7 — History replay 422s (severity: medium)

- Re-dispatching assistant content containing server blocks to strict
  gateways can 422. **Mitigation**: always convert to function-shape history
  before re-dispatch (features.md edge 6). Test with a strict fake upstream.

## P8 — Cost/tokening double-count or under-count (severity: medium)

- Extra round-trips invisible to cost tracking = silent spend; double-counted
  = false alarms. **Mitigation**: accumulate per-iteration `usage` into the
  final envelope AND record each round-trip in `CostTracker` under the same
  session/request. Reconcile in tests: sum of parts == reported total.

## P9 — Concurrency / resource exhaustion (severity: medium)

- Parallel search-heavy requests × process spawns = fork storms. **Mitigation**:
  bounded persistent child-process pool (default small, e.g. 2–4) with queue +
  timeout; backpressure degrades to drop, not to unbounded spawn. Load test in
  validation (N concurrent search requests, assert pool bound held).

## P10 — Version-string drift (severity: low)

- New `web_search_YYYYMMDD` type strings. **Mitigation**: prefix match on
  `web_search_` (+ `name == web_search`), covered by a parametrized test over
  all three known versions plus a synthetic future one.
