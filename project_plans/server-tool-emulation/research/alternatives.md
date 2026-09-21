# Research: Alternatives (OpenRouter web-plugin passthrough)

**Date**: 2026-09-13 | **Sources**: OpenRouter docs (web-search plugin,
server-tools web-search, Responses web-search), OpenRouter benchmarks blog
(2026-08-12).

## Options considered

### 1. `plugins: [{id: "web"}]` / `:online` model suffix — CONSIDERED, DEFERRED

- What: append `:online` to the model slug or add the `web` plugin; OpenRouter
  runs one search per request (Exa, or native for Anthropic/Google/OpenAI/
  xAI models) and grounds the prompt. Deprecated upstream in favor of the
  server tool, but still functional.
- Why deferred, not chosen:
  1. **Violates C-1 in spirit**: introduces a second search vendor (Exa via
     OpenRouter) with its own per-request pricing ("extra costs, even with
     free models"), bypassing the mandated stapler-mcp/Brave backend. Two
     search bills, two quality profiles, new key/cost surface in consolette.
  2. **Wrong control shape**: plugin searches once per request unconditionally
     (no model discretion, no multi-turn refinement), while the client asked
     for model-driven server-tool search (`max_uses`, follow-ups). Quality
     ceiling is lower (OpenRouter's own benchmarks: search budget matters more
     than any other factor — the plugin fixes budget at 1).
  3. **Scope-limited**: only helps OpenRouter-kind upstreams; direct
     OpenAI-compatible and Gemini upstreams still need emulation anyway. We'd
     build the loop regardless.
- Revisit if: stapler-mcp backend proves unusable AND owner explicitly relaxes
  C-1. Tracked as a follow-up note, not a V1 story.

### 2. `openrouter:web_search` server-tool translation — CONSIDERED, DEFERRED

- What: translate the incoming Anthropic server def to OpenRouter's
  `{type: "openrouter:web_search", parameters: {engine, max_results,
  max_uses, ...}}` instead of emulating. Model-driven, budgeted — closer to
  the real semantics than the plugin.
- Why deferred:
  1. Same C-1 problem (Exa/native engines, OpenRouter-billed).
  2. Engine matrix complexity (native vs exa vs firecrawl vs parallel vs
     perplexity, per-model capability badges) becomes consolette's routing
     problem — a support burden for a single-tenant proxy.
  3. Still OpenRouter-only; doesn't cover other upstream kinds.
- Revisit under the same conditions as (1).

### 3. Do nothing (keep the Cohere drop-fix only) — REJECTED as the end state

- The drop-fix is the correct *safety net* (never 400), but it silently removes
  capability the client explicitly requested. Emulation restores it. Drop stays
  only as the degrade path (backend down), per requirements S-4.

## Conclusion

Emulation via stapler-mcp is the only option satisfying C-1 while covering all
upstream kinds with model-driven multi-turn search. OpenRouter-native paths are
documented here as considered-and-deferred with explicit revisit conditions.
