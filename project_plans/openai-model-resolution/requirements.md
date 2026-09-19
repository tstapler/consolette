# Requirements: openai-model-resolution

**Date**: 2026-09-18
**Type**: feature addition
**Complexity**: 3 — system design

## Problem Statement
Consolette's OpenAI-compatible provider (`src/providers/openai.rs`) requires a route's upstream ref to pin an exact `model` string (`RouteUpstreamRef.model`). When the upstream deprecates that model id, every request through that upstream fails until a human manually edits the config to a new pin. This was hit in practice against the ExampleCorp Model Gateway upstream: `model = "gpt-5.1-codex-max"` started returning `400 invalid_request_error` ("has been deprecated") on `/v1/chat/completions`. Manually probing replacement candidates surfaced two compounding issues, not one:
- The upstream's `/v1/models` list does **not** reliably flag deprecation — `gpt-5.1-codex-max` is listed with `shutdown_date: null` yet 400s on use. Deprecation is only observable by attempting a real request.
- Newer codex-family models (`gpt-5.2-codex`, `gpt-5.3-codex`, ...) 404 on `/v1/chat/completions` with "This model is not supported in the v1/chat/completions endpoint. Use the v1/responses endpoint instead" — a hard capability gap, since `src/providers/openai.rs` only implements the Chat Completions API.
- Even chat/completions-compatible models (`gpt-5`, `gpt-5.1`) reject the `max_tokens` param consolette always sends (`src/providers/mod.rs` `translate_anthropic_request_to_openai`), requiring `max_completion_tokens` instead.

## Baseline
Today, when an OpenAI-compatible upstream deprecates a pinned model, every request through that upstream/route fails until a human notices, manually probes the upstream for a working replacement model id (as done manually in this investigation, see chat transcript 2026-09-16/17), and edits the plugin/core conf.d by hand. No automatic recovery exists. Newer model generations on this class of upstream (Responses-API-only) are entirely unreachable regardless, since only Chat Completions is implemented.

## Users / Consumers
- Consolette operators running any `kind = "openai"` upstream where the remote provider periodically deprecates/rotates model ids (the concrete trigger is the ExampleCorp Model Gateway plugin in `ndotfiles`, symlinked at `~/.config/consolette/plugins.d/example/`, but the feature itself is generic core behavior — not ExampleCorp-specific).
- Claude Code and other coding-agent clients proxied through consolette, whose requests (including tool_use / streaming) must keep working when routed through a Responses-API-only model.

## Success Metrics
- A route upstream ref opted into dynamic resolution recovers automatically (no manual config edit) when its currently-selected model starts failing due to deprecation — verified by: deprecating/blocking the current pin in a test double and observing consolette fall over to the next candidate **within the single triggering request** (resolved during Phase 3 planning, ADR-001: resolution runs synchronously inside one `OpenaiProvider::send()` call on a cache miss/invalidation, not spread across N separate client requests), without editing config.
- Requests to Responses-API-only models (e.g. `gpt-5.3-codex`) succeed end-to-end through consolette for: non-streaming text, streaming text, tool_use (both directions), and reasoning-item passthrough — parity with what the chat/completions path supports today for Anthropic-shaped requests.
- Requests to models requiring `max_completion_tokens` succeed without hand-editing the request shape per-upstream.
- Zero regressions: existing static `model` pins on any upstream (ExampleCorp plugin's current config, and core's own `references/conf.d/00-providers.toml` examples) continue to work unchanged — dynamic resolution is additive, not a behavior change for configs that don't opt in.

## Appetite
Large (3-6 weeks)
*(Scope must fit the appetite. If it doesn't fit, cut scope — do not move the deadline.)*

## Constraints
- No deadline pressure beyond "the ExampleCorp Model Gateway route is currently broken" — not a production outage (other routes/upstreams in the same config still work as fallback), so no rushed/unreviewed path is justified.
- Must not introduce real per-request cost/latency overhead for upstreams that don't opt into resolution (static-pin configs are the common case and must stay a zero-overhead passthrough).

## Non-functional Requirements
- **Performance SLO**: for upstreams using dynamic resolution, the resolved model choice must be cached (not re-probed on every request) — a fresh probe request per user request is not acceptable overhead or cost. Cache invalidation triggers on the cached choice starting to fail, not on a fixed TTL alone (a fixed TTL alone would either probe too often or recover too slowly).
- **Scalability**: not applicable — resolution state is per-process, per-upstream; no shared/distributed cache needed at consolette's current scale (single local daemon).
- **Security classification**: internal. No new secrets; probe requests reuse the upstream's already-configured auth.
- **Data residency**: no special requirements — probe/resolution traffic goes to the same upstream the real traffic would have gone to anyway.

## Scope
### In Scope
1. **Dynamic model resolution** (opt-in, per `RouteUpstreamRef`): a new config field (e.g. `model_family` or `resolve`) that, instead of a fixed `model` string, gives a family/prefix. Consolette queries the upstream's `/v1/models`, orders candidates newest-first within that family, and tries them — via real request attempts, since the list's own metadata (`shutdown_date`) is not a reliable deprecation signal — until one succeeds. The successful choice is cached per upstream; cache is invalidated and re-resolution triggered when the cached choice starts failing (not on a blind fixed TTL).
2. **Responses API (`/v1/responses`) support** in `src/providers/openai.rs`, full parity with the existing chat/completions translation path: non-streaming, streaming, tool_use (both directions: Anthropic `tool_use`/`tool_result` ↔ Responses API `function_call`/`function_call_output` items), and reasoning-item passthrough. This is required groundwork for (1) to ever resolve to newer codex-family models, which are Responses-API-only.
3. **`max_tokens` → `max_completion_tokens` compatibility fix** in `src/providers/mod.rs`'s `translate_anthropic_request_to_openai` (~line 702/740): send the param newer OpenAI-family models require, without breaking older/other OpenAI-compatible upstreams that still expect `max_tokens`. Needs a compatibility rule — likely keyed off the same family/model-id signal used for resolution, or a try-one-fall-back-on-that-specific-error approach consistent with (1)'s general "detect failure, adapt" philosophy.
4. Config schema, validation, and docs updates so the new opt-in field is documented alongside the existing `model` field (`src/config/schema.rs`, `src/config/validate.rs`, `references/conf.d/`).
5. Observability: emit a metric/counter per resolution attempt (success/failure, which candidate, which family) so an operator can see resolution happening and diagnose exhaustion (see Observability Requirements below).
6. Once (1)-(3) exist in core, update the ExampleCorp plugin's `conf.d/50-model-gateway.toml` (in the separate `ndotfiles` repo, not this repo) to use the new family-based field instead of the hardcoded `gpt-5.1-codex-max` pin — tracked as follow-up, not part of this repo's PR.

### Out of Scope
- Any change to the ExampleCorp plugin itself (lives in `ndotfiles`, a different repo) — this project only builds the core capability it will consume.
- Cross-upstream family resolution (the existing "auto-model-family" feature at `src/routing/family.rs` already does health/latency-based selection *across* upstreams/free-vs-paid aliases; this project's resolution is *within* a single upstream's own model catalog and is a different, narrower mechanism). No attempt to unify or replace `family.rs`.
- Multi-process/distributed resolution-state sharing.
- Speculative support for Responses-API features consolette's chat/completions path doesn't already support today (e.g. if reasoning items aren't currently surfaced in the chat/completions translator either, this project doesn't add net-new capability beyond parity).

## Rabbit Holes
- **Responses API wire shape is structurally different from Chat Completions**, not just a renamed field: `input` (not `messages`) can be a flat string or a typed array of items; `output` (not `choices[0].message`) is an array of typed items (`message`, `function_call`, `reasoning`, ...); streaming uses a different SSE event taxonomy (`response.output_item.added`, `response.output_text.delta`, etc., not chat's `chat.completion.chunk`). Translating this to/from consolette's existing Anthropic-shaped internal representation is materially more work than the chat/completions translator was — budget real design time here, don't assume it's a thin wrapper.
- **Probing has real side effects**: a "try a real request" resolution strategy sends real (billable, logged) traffic to the upstream. Needs a cheap probe shape (minimal tokens, no side-effecting tool calls) and must not probe on every request once a working choice is cached.
- **Distinguishing "deprecated model" from "wrong endpoint" from "transient upstream error"**: the fix must not treat a rate-limit or transient 5xx as "this model is dead, try the next one" — only specific deprecation/not-found-style errors should advance the candidate list, or resolution will misbehave under normal transient failures. Needs explicit error-classification logic, not a blanket "request failed → next candidate."
- **`max_completion_tokens` compatibility rule scope**: research needs to determine whether this is reliably predictable from model id/family, or must itself be probed/cached similarly to model resolution — don't assume a static family-prefix table will stay accurate over time.

## Alternatives Considered
- **Static allowlist of "known good" models per family**, manually curated and updated in config when things break — rejected as it doesn't solve the actual problem (still requires a human to notice and update).
- **Reuse the existing multi-upstream fallback mechanism** (`routes[].upstreams`, tried in order) by declaring one pseudo-upstream per candidate model — rejected: fallback is currently upstream-granularity, not model-granularity within one upstream, and would require duplicating upstream config (auth, base_url, etc.) per model candidate, which is what this project's new field avoids.
- **Cross-upstream family resolution (`src/routing/family.rs`) extended to cover this case** — considered and rejected for this project's scope (see Out of Scope); that mechanism solves a different problem (health/latency selection across configured members) and doesn't address discovering unknown-in-advance model ids from a live `/v1/models` catalog.

## Feasibility Risks
- Full Responses API parity (streaming + tool_use + reasoning) is the largest unknown — if research in Phase 2 finds the SSE/event-shape translation is significantly harder than estimated, the Large appetite may still be tight; plan should flag a fallback to a smaller Responses API slice (non-streaming/text-only) as a de-scope option without blocking the resolution-logic and max_completion_tokens work, which don't strictly depend on full parity.
- Real-request probing against a live upstream (the ExampleCorp Model Gateway, reached only via the local SBN Dev Agent per `ndotfiles`' plugin docs) means integration testing this fully requires that agent/VPN context; automated tests must exercise resolution logic against a mock/fake OpenAI-compatible server, not the real gateway.

## Observability Requirements
- Per-upstream, per-resolution-attempt counter (success/failure, candidate model id, family) — extends the existing `/metrics` surface (`src/metrics/counters.rs` already has family-related counters from `auto-model-family`; this is a sibling addition, not a reuse of those specific counters since the mechanism differs).
- A log line (or dashboard indicator, consistent with `src/dashboard.rs`'s existing per-upstream health display) when resolution exhausts all candidates in a family with no working model — this is the "all fallbacks dead" signal an operator needs to notice before assuming the feature "just works."

## Risk Control
Opt-in via a new config field (e.g. `model_family` / `resolve`) on `RouteUpstreamRef`, additive alongside the existing `model` field. No existing config (including core's own `references/conf.d/` examples and the ExampleCorp plugin's current pin) changes behavior unless explicitly migrated to the new field. Rollback is trivial: unset the new field, revert to a static `model` pin.

## Open Questions
- Should `max_completion_tokens` vs `max_tokens` selection be a static per-family rule seeded by research, or itself probed-and-cached like model resolution (per the Rabbit Holes note above)? Defer to Phase 2 research.
- Exact shape of the new config field name and whether "family" is a literal prefix match, a glob, or a small DSL — defer to Phase 3 planning.
- Whether the deprecation-vs-transient-error classification should be a hardcoded list of OpenAI error `code`/`type` values or something more general — defer to Phase 2 research (check what error taxonomy real OpenAI-compatible gateways actually use here).
