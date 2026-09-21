# Research: Features (Anthropic server-tool landscape)

**Date**: 2026-09-13 | **Sources**: Anthropic platform docs (web-search-tool,
server-tools guides, SDK type defs), OpenRouter server-tools/web-search docs,
consolette `src/providers/mod.rs` translation code.

## Incoming request shape (what Claude Code sends)

- `tools[]` entry: `{"type": "web_search_20250305" | "web_search_20260209" |
  "web_search_20260318", "name": "web_search", ...optional config}`.
  No `input_schema` — this is exactly what makes `translate_tool_definition`
  emit a description-less empty-parameter function today.
- Optional config fields (all optional, must be preserved/honored on a
  best-effort basis): `max_uses` (int — directly reusable as the loop-iteration
  cap), `allowed_domains` / `blocked_domains` (domain filtering),
  `user_location` (`{type: "approximate", city, region, country, timezone}`),
  `allowed_callers` (ZDR; meaningless in emulation — ignore),
  `strict` (ignore).
- Detection rule: a `tools[]` entry is a server web_search def iff
  `type` starts with `web_search_` AND `name == "web_search"`. (The `type`
  prefix match future-proofs new dated versions; the `name` check avoids
  colliding with a user function coincidentally named `web_search` — if both
  appear, the server def takes the emulation path and the user function is
  passed through untouched under a renamed id. See plan.md edge case.)

## Upstream response shape we must finally emit (fidelity target)

- `server_tool_use` block: `{"type": "server_tool_use", "id": "srvtoolu_…",
  "name": "web_search", "input": {"query": "…"}}`.
- `web_search_tool_result` block: `{"type": "web_search_tool_result",
  "tool_use_id": "<same id>", "content": [{"type": "web_search_result",
  "url": "…", "title": "…", ...}]}`.
- Terminal text block may carry `citations` of type
  `web_search_result_location` referencing `encrypted_index` — **we cannot
  mint these** (Anthropic-encrypted blobs). V1 emits plain text without
  citation attachments; documented as lossy but client-compatible (Claude Code
  renders uncited text fine).
- `usage.server_tool_use.web_search_requests`: count of searches performed —
  we MUST populate this (clients and cost tracking read it).
- `stop_reason`: `end_turn` on success (never `tool_use` — the loop is fully
  internal; the client sees one finished turn). `pause_turn` is never emitted
  (server-side-loop continuation protocol is out of scope).

## Intermediate (upstream-facing) shape

- Rewrite the server def to a normal function tool the upstream CAN call:
  `{"name": "web_search", "description": "Search the web…",
  "input_schema": {"type": "object", "properties": {"query": {"type":
  "string", "description": "…"}}, "required": ["query"]}}`.
- Upstream returns ordinary `tool_use` (Anthropic-native upstreams, if any
  ever take this path) or `tool_calls` → translated by the existing
  `openai_tool_calls_to_blocks` to `tool_use`. The loop matches
  `tool_use.name == "web_search"` and executes.
- `max_uses` maps to the loop cap: `iterations = min(max_uses (if present),
  configured max, hard cap)`. `allowed/blocked_domains` and `user_location`
  are forwarded into the search call only if the backend supports them —
  `brave_web_search` takes `{query, count}` only, so V1 **logs-and-ignores**
  domain/location filters (documented limitation, surfaced in validation).

## Edge cases & failure modes the design must handle

1. Model never calls the rewritten tool → return upstream answer verbatim
   (minus def restoration: strip our synthetic function def from any echoed
   surface; final message must look like a native server-tool turn).
2. Model calls `web_search` with missing/empty `query` → error-content
   `tool_result` fed back (`{"error": "empty query"}`), loop continues; counts
   against iterations.
3. Model calls unknown/other tools alongside → only `web_search` is executed
   in-proxy; any other `tool_use` passes through to the client with
   `stop_reason: tool_use` (mixed server+client turn — matches Anthropic's
   documented mixed-turn semantics).
4. Multiple `web_search` calls in one turn → execute sequentially (V1; parallel
   fan-out deferred), each appended as its own result block.
5. `max_uses: 0` or negative → treat as 1? No — treat absent/≤0 as "use
   configured default" and document.
6. Replay hazard: assistant messages containing our synthesized
   `server_tool_use`/`web_search_tool_result` blocks must never be sent back
   to a non-executing upstream raw (known 422 class on some gateways) — the
   loop converts history into function `tool_use`/`tool_result` pairs on
   re-dispatch (same conversion `anthropic_blocks_to_openai` already does).

## Users' unstated needs

- "It just works with my existing config" — zero-config default (backend path
  auto-detected, e.g. `stapler-mcp` on `PATH`), tuning knobs optional.
- Cost visibility — searches consume tokens + time; `usage` and cost metrics
  must reflect the extra round-trips (no silent spend).
- No new secrets to manage — keys stay where they are (daemon env).
