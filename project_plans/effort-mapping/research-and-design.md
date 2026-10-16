# Effort mapping: research and design proposal

Status: 2026-10-16. Phases 0 and 1 implemented (parse, capability table, config, trace field); emitters
(phases 2-4) not started. Open decisions are listed at the end.

## Problem

Claude Code sends a reasoning-effort level with each request. Consolette forwards that to Anthropic
upstreams unchanged but silently drops it for every other upstream kind, and an operator can't
tune it. Goal: (1) map the client's effort onto each target's own switch, (2) let operators
override, floor or cap it per tier/upstream.

## Current behavior (VERIFIED in this repo at `4a43890`)

| Upstream kind | `output_config` / `thinking` today | Source |
|---|---|---|
| `anthropic` | forwarded verbatim; the old field-stripping was removed 2026-09-25 | `src/providers/anthropic.rs:795-799` |
| `openai` (chat) | dropped; only `model`, `max_tokens`, `temperature`, `stream`, `tools`, `tool_choice` are copied | `src/providers/mod.rs:883-983` |
| `openai` (Responses) | dropped; response-side reasoning items are already mapped to `thinking` blocks | `src/providers/openai/responses.rs:125,351` |
| `gemini` | dropped; `generationConfig` only carries `maxOutputTokens`, `temperature`, `topP`, `stopSequences` | `src/providers/gemini/translate.rs:502-533` |
| `openrouter` | not yet read; assume it shares the OpenAI translator |  |
| `bedrock` | `output_config` is removed in `prepare_body` (with `stream`, `model`, `context_management`); only a beta-header allowlist entry `effort-2025-11-24` for `claude-opus-4-5` exists | `src/providers/bedrock.rs:90,484` |

Hook point: `Router::apply_model_tiers` (`src/routing/router/tiers.rs:41`) already rewrites the model
per candidate before dispatch, so a per-candidate effort decision fits the same place.

## Research findings

Source reports were read 2026-10-16; pages saved under `/tmp/r/` during research. I re-opened the
Anthropic effort page and the OpenRouter page to spot-check (default levels, `xhigh` description,
OpenRouter budget formula). Everything else is as reported by the research agent; labels below
say which.

### Anthropic Messages API (VERIFIED, [effort docs](https://platform.claude.com/docs/en/build-with-claude/effort))

- Field: top-level `output_config.effort`; values `low | medium | high | xhigh | max`. GA, no beta header.
- Per-message effort (a `role:"system"` message with `output_config.effort`) needs beta
  `mid-conversation-output-config-2026-07-01`; without it the API returns 400.
- Default is `high`, except `medium` on Opus 5.5 and Haiku 5.5.
- Opus 4.6 / Sonnet 4.6 accept `max` but not `xhigh`.
- `thinking` mode depends on the model ([troubleshooting](https://platform.claude.com/docs/en/build-with-claude/thinking-troubleshooting), agent-reported):
  `{type:"enabled", budget_tokens}` only on 4.5 and earlier (deprecated on 4.6, 400 on 4.7+);
  `adaptive` only on Opus 4.7+, Sonnet 5+, Haiku 5.5, Fable, Mythos (400 on 4.5);
  `disabled` is a 400 on Fable, Mythos, Opus 5.5, Sonnet 5.5 and, at `xhigh`/`max`, on Opus 5.
- Thinking tokens count toward `max_tokens`; Anthropic suggests 64k at `xhigh`/`max`.
- Changing effort between requests invalidates the prompt cache.

### What Claude Code sends (partly INFERRED, [model-config](https://code.claude.com/docs/en/model-config))

- Effort resolves from `CLAUDE_CODE_EFFORT_LEVEL`, `--effort`, `/effort`, then `effortLevel` / `modelSettings`.
  `effortLevel` accepts low/medium/high/xhigh; `max` is session-only. The client clamps to what the model supports.
- **Wire format, VERIFIED (phase 0, 2026-10-16):** Claude Code 2.1.296 pointed at a local listener sent
  `output_config: {"effort": "<level>"}` plus `thinking: {"type": "adaptive", "display": "omitted"}` for
  `claude-opus-4-8`, `claude-sonnet-5-5` and `claude-haiku-5-5`, with `--effort` set to low, medium, high,
  xhigh and max (xhigh/max/high not captured for every model; see matrix below). Other top-level fields:
  `context_management`, `metadata`, `safeguards`, `stream`. `max_tokens` was 64000 (Opus 4.8) and 128000
  (Sonnet 5.5, Haiku 5.5). Captured combinations: haiku-5-5 low/xhigh; opus-4-8 low/medium/max;
  sonnet-5-5 low/medium/high. Reproduce with a local HTTP listener and
  `claude -p … --effort X --settings '{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:PORT"}}'`
  (the `--settings` env override is needed because `~/.claude/settings.json` sets `ANTHROPIC_BASE_URL`).

### Other targets

| Target | Field and values | Notes | Confidence |
|---|---|---|---|
| Bedrock (Anthropic models) | same `output_config.effort` / `thinking` as first-party | betas go in body `anthropic_beta` on legacy InvokeModel, header on the mantle endpoint; Converse placement unknown | VERIFIED for shape, UNVERIFIED for Converse |
| OpenAI chat | flat `reasoning_effort` | values model-dependent: `none, minimal, low, medium, high, xhigh, max`; GPT-6 Astra rejects `none`, GPT-6.1 Sol rejects `none`/`minimal`; Chat Completions has no function calling on those two | VERIFIED ([guide](https://developers.openai.com/api/docs/guides/reasoning)) |
| OpenAI Responses | `reasoning.effort`, `reasoning.summary` | `store:false` returns encrypted reasoning items that must be passed back | VERIFIED |
| OpenAI tools + effort on chat | gpt-5.6 error "Function tools with reasoning_effort are not supported ... use /v1/responses" | already handled by the repo's wrong-endpoint retry (`openai/mod.rs:1911`, `resolution.rs:325`) | docs UNVERIFIED, repo behavior VERIFIED |
| OpenRouter | `reasoning: {effort \| max_tokens, exclude, enabled}`, only one of effort/max_tokens | effort `max, xhigh, high, medium, low, minimal, none`; Anthropic budget = `max(min(max_tokens × ratio, 128000), 1024)` with ratios 0.95 / 0.8 / 0.5 / 0.2 / 0.1; mandatory-reasoning models must not get `none` (`reasoning.mandatory` in `/models`) | VERIFIED ([docs](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens)) |
| Gemini 3/4 (public) | `generationConfig.thinkingConfig.thinkingLevel` = `minimal/low/medium/high`, subset per model | `minimal` errors on Gemini 4 Argon; Gemini 4 can't disable thinking | VERIFIED ([docs](https://ai.google.dev/gemini-api/docs/generate-content/thinking)) |
| Gemini 2.5 | `thinkingConfig.thinkingBudget` (Pro 128–32768, Flash 0–24576, `-1` dynamic) | `thinkingLevel` unsupported on 2.5 | VERIFIED |
| Gemini via Cloud Code `v1internal` | `thinkingConfig` inside `request.generationConfig`, budget form only | no official thinking spec found; this is the endpoint consolette's `gemini` kind uses | UNVERIFIED (community sources) |

### Existing proxies (LiteLLM [docs](https://docs.litellm.ai/docs/reasoning_content), VERIFIED)

- Claude 4.6+: `{"thinking":{"type":"adaptive"},"output_config":{"effort":X}}`, `minimal` → `low`.
- Older Claude: `budget_tokens` minimal/low 1024, medium 2048, high 4096, xhigh 8192, max 16384.
- Routes tools + `reasoning_effort` on gpt-5.4+ through the Responses API.
- claude-code-router: not read at source. Issue summaries (UNVERIFIED) report it sends both `effort` and
  `max_tokens` to OpenRouter (400) and `enabled:false` to mandatory-reasoning models. Both are traps to avoid.

### Not verified

Exact Claude Code wire JSON; Bedrock Converse effort field; `temperature` vs thinking on current
models; Gemini 3 `thinkingBudget` deprecation; any official `v1internal` thinking spec;
whether consolette's Bedrock path currently preserves `output_config`.

## Proposed mapping (client level → target)

Client level is first clamped by any operator floor/cap, then emitted per target kind.

| Client | Anthropic / Bedrock | OpenAI `reasoning_effort` | OpenRouter `reasoning.effort` | Gemini 3/4 `thinkingLevel` | Gemini 2.5 / pre-4.6 Claude `budget_tokens` |
|---|---|---|---|---|---|
| low | `low` | `low` | `low` | `low` | 1024 |
| medium | `medium` | `medium` | `medium` | `medium` (not on 3 Pro) | 2048 |
| high | `high` | `high` | `high` | `high` | 4096 |
| xhigh | `xhigh` (else `high`) | `xhigh` if the model supports it, else `high` | `xhigh` | `high` | 8192 |
| max | `max` | `max` on GPT-5.6/6, else `xhigh`→`high` | `max` | `high` | 16384 |

Rows `xhigh`/`max` for OpenAI and Gemini are clamp choices (mine, not from a source); the budget
column is LiteLLM's table, clamped to the model's allowed range. Budgets must stay below `max_tokens`.

## Design

### 1. Parse once: `ClientEffort`

Extract from the inbound body: `level: Option<Level>` from `output_config.effort`, plus
`thinking` mode/budget. Absent effort stays `None`, which means "emit nothing" for every
non-Anthropic target (never invent a level; the client's silence usually means the model default).
If a client sends only `thinking.budget_tokens`, derive a level by nearest budget (reverse of the
budget column).

### 2. Config: policy at the tier/upstream layer

Extend what already exists rather than add a new top-level concept:

```toml
[[model_tiers]]
name = "haiku"
match = ["claude-haiku"]
[model_tiers.upstreams.gw-openai]
model = "gpt-5.6-mini"    # optional; `effort` alone keeps the route's model
effort = "low"            # force (ignores client)

[effort.gw-openai]        # keyed by upstream name (implemented in phase 1)
mode = "map"              # off (default) | map
min = "low"               # optional floor
max = "high"              # optional cap
```

- `mode = "off"` (default) keeps today's behavior exactly: nothing translated. This avoids a
  silent behavior change for existing configs.
- `effort` on a tier override wins over the client's level; `min`/`max` clamp
  whichever level results.
- Validation follows `validate_model_tiers`: unknown level names rejected at load; `effort` on an
  upstream whose kind can't express effort is a load error rather than a silent no-op.

### 3. Resolve per attempt, not per request

`apply_model_tiers` runs before `dispatch_attempts` and fallback can swap upstreams, so the
effective effort must be computed for each candidate (kind + final model + tier + policy) at the
moment its body is built. Otherwise a fallback to a different kind would inherit the wrong switch.

### 4. Per-kind emitters

One function per kind taking `(ClientEffort, policy, resolved_model, &mut body)`:

- **anthropic / bedrock:** set `output_config.effort` clamped to the model's levels (e.g. drop `xhigh`
  to `high` on 4.6). If the tier swapped the model across a thinking-mode boundary, rewrite `thinking`
  (`enabled`+budget ↔ `adaptive`), because the wrong mode is a 400. Unknown model → leave the body untouched.
- **openai chat / responses:** set `reasoning_effort` / `reasoning.effort` only when the model is
  known to be a reasoning model, since non-reasoning models reject the field. Respect the per-model
  rejection of `none`/`minimal`. The existing tools+effort wrong-endpoint retry stays as the safety net.
- **openrouter:** emit `reasoning.effort` **or** `reasoning.max_tokens`, never both; never send
  `none`/`enabled:false` to a model whose `/models` entry says `reasoning.mandatory`.
- **gemini:** `thinkingLevel` for the 3/4 family, `thinkingBudget` for 2.5 and for `v1internal` until
  its spec is verified; set inside `generationConfig` (extend `GeminiGenerationConfig`).

A small capability table (model prefix → supported levels, thinking mode, reasoning yes/no) backs
all four. Unknown models get no effort field, which fails safe.

### 5. Response side

Existing handling stays: Responses reasoning items and OpenRouter `reasoning_details` already become
`thinking` blocks. Newer Anthropic models default `thinking.display` to `omitted`; when the proxy
emits `adaptive` itself, it should not change display, so client behavior matches what they'd get direct.

### 6. Observability

Record client effort and applied effort (plus whether it was forced, clamped or dropped) on the trace
next to the requested/routed model, so the dashboard answers "why did this run at `low`?".

### 7. Tests

Table-driven unit tests per emitter (level × model → body), a router test that a fallback candidate
gets its own effort, config-validation tests, and one wiremock-style test per kind asserting the
outbound body. Add the captured Claude Code request as a fixture.

## Risks

- **400 from over-eager emission.** Sending reasoning params to a non-reasoning model, or the wrong
  `thinking` mode to a model, fails the whole request. Mitigation: known-model table, unknown = omit.
- **Capability-table drift.** Model names and supported levels change monthly (the research pages list
  Fable, Opus 5.5, GPT-6, Gemini 4). Mitigation: table in config-overridable data, not code constants.
- **Cache invalidation.** Per-request effort changes bust Anthropic's prompt cache; forcing a constant
  per tier avoids that.
- **`max_tokens` headroom.** Budgets and high effort consume `max_tokens`; clamp budgets to below it.
- **Per-message effort (beta).** Out of scope; if a client sends the beta system-message form to a
  non-Anthropic upstream it should be dropped, not translated.

## Phasing and cost

Cost classes (agent execution, output-token weighted), not human time:

| Phase | Scope | Cost class | Wall-clock blocker |
|---|---|---|---|
| 0 | Capture real Claude Code request; read Bedrock/OpenRouter body handling | S | one live Claude Code session |
| 1 | `ClientEffort` parse, capability table, config schema + validation, trace fields | M | none |
| 2 | Emitters: anthropic/bedrock, openai chat+responses | M | none |
| 3 | Emitters: openrouter, gemini (public-API shapes) | M | none |
| 4 | Gemini `v1internal` thinking spec | S | needs a live call or primary source |

Phases 2 and 3 are independent and can run as parallel worker agents after phase 1.

## Open decisions for you

1. **Default mode.** `effort_mode = "off"` preserves today's behavior; `"map"` as the default would
   make the proxy forward effort everywhere, but changes behavior on upgrade. Recommendation: `off`
   for one release, then flip.
2. **Forced vs mapped.** Is a tier's `effort` meant to override the client (cost control) or only fill
   in when the client sent none? The sketch above forces; say if you want fill-in.
3. **Clamp direction for `xhigh`/`max` on targets without them.** Proposed: clamp down. The alternative,
   reject the request, is safer for cost but surfaces errors to the client.
4. **Gemini `v1internal`.** Ship budget-only until the endpoint's thinking spec is verified, or hold Gemini?
