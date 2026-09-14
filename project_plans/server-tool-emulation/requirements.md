# Requirements: Server-Tool Emulation (web_search)

**Date**: 2026-09-13
**Type**: New feature in an existing project (consolette proxy)
**Status**: Draft for owner review — planning only, no code written

## Problem Statement

Claude Code sends Anthropic server tool definitions (starting with `web_search`,
e.g. `{"type": "web_search_20250305", "name": "web_search"}` — no `input_schema`)
in `tools[]` on `POST /v1/messages`. Consolette is single-shot passthrough:
`post_v1_messages` (`src/entrypoint/messages.rs:46`) calls `Router::dispatch`
once and returns; nothing executes tools. `translate_tool_definition`
(`src/providers/mod.rs:589`) forwards the server def as an OpenAI function with
neither description nor parameters, and Cohere-backed models via OpenRouter 400
the whole request (`the 'web_search' tool must have at least a description,
input, or output`).

A just-applied working-tree fix drops description-less empty-parameter tools for
Cohere-bound requests only (gated on model id containing "cohere"). That fix
stands. This feature is the follow-up: instead of dropping search capability,
**emulate the server tool inside the proxy** so clients keep working search
answers on upstreams that cannot execute server tools.

## Users / Consumers

- **Primary**: Claude Code (Anthropic Messages API on `/v1/messages`), single
  tenant, localhost-only (Tyler's Mac).
- **Upstreams affected**: OpenAI-compatible / OpenRouter (incl. Cohere-backed
  models), Gemini. Anthropic-native upstreams are unaffected (passthrough).
- **Operators**: ndotfiles/ansible + cfgcaddy (config deployment); no new
  operator workflow expected beyond optional tuning knobs.

## Success Metrics

1. An Anthropic request carrying a `web_search` server def dispatched to a
   non-executing upstream returns a 200 with a faithful
   `server_tool_use` + `web_search_tool_result` block pair and a grounded final
   text answer — on both `stream: false` and `stream: true` requests.
2. Cohere-via-OpenRouter requests that previously 400'd now succeed with search
   results (the drop-fix behavior is superseded by emulation, never regressed
   to a 400).
3. Requests with no server tools behave byte-identically to today (zero
   regression; emulation path not entered).
4. stapler-mcp backend down ⇒ request still succeeds, degraded to today's
   drop behavior (no whole-request failure), with a logged + metered event.

## Constraints (hard, not revisited in planning)

- **C-1 — Search backend MUST be the existing web-search tooling in
  `tstapler/stapler-mcp`.** No new search API integration (no Brave keys, no
  new vendor in consolette). `BRAVE_API_KEY` stays in the daemon's environment;
  it MUST NOT appear in consolette config, logs, or error bodies.
- **C-2 — Transport kept thin** (repo rule): real logic lives in an
  independently-testable module; CLI/MCP/entrypoint layers only orchestrate.
- **C-3 — Planning only.** No code changes, no implementation in this run.
  Artifacts land under `project_plans/server-tool-emulation/`.

## Scope

### In Scope

- `web_search` server-tool emulation only (all three versioned type strings:
  `web_search_20250305`, `web_search_20260209`, `web_search_20260318`).
- Non-streaming (`Full`) emulation loop + streaming requests handled via
  buffer-then-synthesize SSE (V1; mid-stream execution explicitly deferred —
  see architecture.md).
- Bounded iteration / cost guards; error degradation to drop behavior.
- Composition with existing router semantics (ADR-003): validation/auth ⇒ no
  failover; rate-limit ⇒ cooldown; streaming failover limits; session pins;
  `count_tokens`/cost tracking across extra round-trips.
- Config surface limited to tuning knobs (max iterations, timeouts, result
  caps, backend path) with safe defaults; zero-config working out of the box.

### Out of Scope (explicit exclusions)

- Other server tools (`code_execution`, `web_fetch` as a server tool,
  `computer_use`, future Anthropic tools) — architecture must not preclude
  them, but no stories/tasks for them.
- Mid-stream incremental tool execution (streaming SSE frames interleaved with
  live search calls) — deferred; V1 buffers.
- `pause_turn` continuation semantics (Anthropic long-loop protocol) — out of
  scope; our loop is proxy-internal, the client sees one finished turn.
- Dynamic filtering (`web_search_20260209` code-execution nesting),
  `allowed_callers`/ZDR semantics, `citations`/`encrypted_content` fidelity
  beyond title+URL+text (we cannot mint Anthropic's encrypted blobs).
- Bedrock Converse server-tool support changes; Bedrock keeps current behavior.
- Any change to the Cohere drop-fix except superseding it on the emulation
  path (drop remains the fallback when the backend is down).

## Open Questions (for research phase — now resolved, see research/)

1. Integration seam to stapler-mcp (stdio MCP client vs shared crate vs HTTP)?
   → Resolved: Spoon — see `research/stapler-mcp-seam.md` (recommended: managed
   persistent stdio MCP child-process pool).
2. Buffer vs mid-stream execution for SSE? → Resolved: buffer-then-synthesize
   V1 (see `research/architecture.md`).
3. OpenRouter `:online` / `plugins: [{id: web}]` / `openrouter:web_search`
   passthrough as an alternative? → Considered and deferred (see
   `research/alternatives.md`).
4. Exact `web_search_tool_result` fidelity limits (no `encrypted_content`)?
   → Resolved: title+URL+text mapping, documented as lossy (see
   `research/features.md`).
