# Design: Session-Level Message-List Compaction

**Status**: Draft — design pass only, no implementation yet
**Date**: 2026-08-14
**Tracks**: [tstapler/consolette#7](https://github.com/tstapler/consolette/issues/7) (ported from
[tstapler/dotfiles#42](https://github.com/tstapler/dotfiles/issues/42))

## Scope

Issue #7 asks for a design, not code, for a second orchestration layer inspired by
[`claw-compactor`'s fusion pipeline v8](https://github.com/open-compress/claw-compactor/tree/main/scripts/lib/fusion):

- `TieredCompaction` — micro/auto/full 3-tier strategy, gated on context-pressure %
- `ConversationSummarizer` — deterministic, LLM-free turn summarization
- `ToolResultBudget` — age-based truncation, keep only the N most recent tool results
- `PlanReinjection` / `SkillReinjection` — re-inject active plans/tool schemas after compaction
- `CompactHooks` — pre/post-compaction plugin callbacks

This document proposes where these five pieces plug into consolette, what state they need
that the proxy doesn't have today, and which existing consolette primitives they should
reuse vs. where a genuinely new abstraction is warranted. It does not commit to Rust APIs
or file layout in detail — that's plan-phase work once this design is accepted.

## Why this isn't an incremental `compression/` stage

Every existing `src/compression/*` stage (`text_compressor`, `structural_collapse`,
`semantic_dedup`, `path_collapse`, `log_crunch`, `smart_crusher`, `code_compressor`,
`diff_compactor`) and `CompressionEngine::compress_request`
([`src/compression/engine.rs`](../../../src/compression/engine.rs)) share one invariant:
**they operate on a single already-fully-formed request body, once, statelessly.** They
have no memory of turn N-1, no concept of "context pressure," and nothing triggers them
except "a request arrived." `SystemPromptPipeline::apply`
([`src/system_prompt/mod.rs`](../../../src/system_prompt/mod.rs)) is the same shape.

`TieredCompaction` and its siblings need three things none of those stages have:

1. **Multi-turn state.** `ToolResultBudget`'s "N most recent tool results" and
   `ConversationSummarizer`'s "summarize turns older than X" both require knowing the
   full message history across requests, not just the current body. `PlanReinjection`
   requires remembering what was injected in an *earlier* turn so it can be restored
   after that turn gets summarized away.
2. **A trigger, not a per-request pass.** `TieredCompaction`'s tiers are gated on
   context-pressure percentage (current tokens ÷ model context window). That
   percentage is meaningless computed from a single request in isolation the way
   `CompressionConfig::compress_floor_bytes` is a meaningless per-request byte
   threshold once history spans many turns — it has to be evaluated against the
   accumulated conversation, and different tiers of the pipeline (micro vs. full) fire
   at different pressure thresholds, not on every request.
3. **A place to hook.** `CompactHooks` is explicitly a plugin seam around whenever
   compaction runs — there is no equivalent seam anywhere in `compression/` or
   `system_prompt/` today because nothing there is triggered conditionally.

None of this fits inside `CompressionEngine` without changing its contract from "stateless
per-request transform" to "stateful session orchestrator," which is exactly the
incremental-stage trap the issue calls out.

## Relationship to `project_plans/compaction-hook/` — a different problem, adjacent shape

This repo already has a **separate, shipped** feature under
[`project_plans/compaction-hook/`](../../compaction-hook/) (ADR-008 through ADR-011,
`src/claude_code_session/`) that also uses the word "compaction." It is important not to
conflate the two:

| | `compaction-hook` (shipped) | This design (#7) |
|---|---|---|
| Data model | On-disk Claude Code JSONL transcript rows (`~/.claude/projects/.../*.jsonl`) | In-flight Anthropic Messages API request body (`messages[]` array) |
| Consumer | Claude Code itself, via `/resume <session-id>` | The upstream Anthropic/Bedrock provider, next request |
| Trigger | Explicit CLI/MCP call (`consolette compact-session`, `read_omitted_content`) | Proxy-internal, gated on context-pressure % of the live conversation |
| Summarizer | Subprocess (`claude -p --resume`), can be LLM-backed | `ConversationSummarizer` is specified as **deterministic, LLM-free** |
| Lifetime of pruned content | Omission cache must survive until a much-later `/resume`, potentially days ([`src/claude_code_session/omission_cache.rs`](../../../src/claude_code_session/omission_cache.rs), persistent) | Only needs to survive the current session/process |

They are not the same feature wearing different clothes — one rewrites a session file for
Claude Code to resume into; the other reshapes the live request body the proxy forwards.
That said, two conventions from `compaction-hook` transfer directly and should be reused
rather than re-invented:

- **The subprocess-with-timeout pattern** (`src/auth/exec.rs`, formalized as the house
  style in ADR-007 §2 and reused by `ClaudeCliSummarizer` per ADR-010) — *not* for
  `ConversationSummarizer` itself (which must stay LLM-free per the issue's own spec,
  so it has no subprocess to call), but as the shape for `CompactHooks`' plugin
  callbacks if any hook needs to shell out.
- **Module placement precedent**: `compaction-hook` added a new top-level
  `src/claude_code_session/` sibling to `compression`, rather than nesting inside it,
  specifically because the data model didn't match `compression/`'s stateless
  proxy-message-`Value` shape ([`research/architecture.md`](../../compaction-hook/research/architecture.md)
  §1). The same reasoning applies here (see below).

## Proposed placement: new `src/session_compaction/` module, driven by session state the proxy doesn't hold today

### The real gap: consolette's request path is currently stateless per-connection

`Router::dispatch` ([`src/routing/router.rs`](../../../src/routing/router.rs)) takes one
body and forwards it; nothing in the router, `Provider` trait, or `main.rs` accumulates
message history across requests today — `Command::Run` doesn't even wire up an HTTP
server yet (it only loads and prints config,
[`src/main.rs:53-62`](../../../src/main.rs)). So before any of the five components can be
built, something has to own **per-conversation state that survives across requests
within one client session**: at minimum, the running message list as last seen, the
current context-pressure estimate, what was injected by `PlanReinjection`/
`SkillReinjection` last time, and whatever `ToolResultBudget` needs to know about tool
result age. This state does not exist anywhere in the codebase yet and is the actual
prerequisite this design has to specify, not just the five named components.

Proposed shape: a `SessionState` keyed by whatever conversation-identity signal the proxy
can observe (candidate: a stable prefix hash of the first user message, or a client-
supplied session header if one exists in the actual traffic — needs verification against
real Claude Code traffic before committing, same "verify before implementing" gate as
issue #8). `SessionState` lives behind an `Arc<DashMap<SessionKey, SessionState>>` in
`AppState`, following the existing `HealthRegistry`/`DashMap<usize, ProviderState>`
pattern from ADR-003, with a TTL eviction policy (reuse `moka` the way
`RewindStore` does, [`src/compression/rewind.rs`](../../../src/compression/rewind.rs))
since an abandoned session's state should not accumulate forever.

### Module: `src/session_compaction/`

New top-level sibling of `compression` and `system_prompt`, for the same reason
`compaction-hook` chose a new top-level module over nesting: this operates on a
different data shape (accumulated multi-turn state, not a single request body) with a
different lifecycle (spans many requests, needs a trigger) than anything currently under
`compression/`.

```
src/session_compaction/
  mod.rs              # SessionCompactionPipeline::apply(session_key, body) -> Value
  session_state.rs     # SessionState struct + DashMap<SessionKey, SessionState> store
  tiered.rs            # TieredCompaction: pressure % -> {Off, Micro, Auto, Full}
  summarizer.rs         # ConversationSummarizer trait + deterministic default impl
  tool_result_budget.rs # ToolResultBudget: age-ranks tool_result blocks, truncates past N
  reinjection.rs        # PlanReinjection / SkillReinjection
  hooks.rs              # CompactHooks trait + registry
```

`SessionCompactionPipeline` sits **before** `CompressionEngine::compress_request` and
`SystemPromptPipeline::apply` in the request path: it operates on the full `messages[]`
array and decides what history survives into this request; the existing per-request
compression/system-prompt stages then run on whatever it emits, exactly as they do today
for any other body. This ordering also answers where `CompactHooks` fires: pre-hooks run
before `SessionCompactionPipeline` mutates the message list, post-hooks run after, both
before the (unchanged) `compression`/`system_prompt` stages.

## Component designs

### `TieredCompaction`

```rust
enum CompactionTier { Off, Micro, Auto, Full }

trait TieredCompaction {
    fn tier_for_pressure(&self, pressure_pct: f32) -> CompactionTier;
}
```

Pressure = `estimated_tokens(messages) / model_context_window`. Token estimation should
reuse whatever `router.rs`'s planned `TokenEstimator` seam ends up being (flagged as
architecture-review N8, currently hardcoded in `router.rs`'s plan) rather than a second,
divergent estimator — if that seam doesn't exist yet when this is implemented, build it
there first, not here.

- **Micro**: `ToolResultBudget` only (cheapest, no summarization, reversible).
- **Auto**: Micro + `ConversationSummarizer` on turns older than a configurable window.
- **Full**: Auto, with a smaller "keep verbatim" window and re-run of `PlanReinjection`/
  `SkillReinjection` to restore anything summarization dropped.

Thresholds (e.g. Micro at 60%, Auto at 75%, Full at 90%) should be config-driven
(`Config` struct extension, figment-backed per ADR-001), not hardcoded — this is exactly
the kind of per-deployment tuning knob the rest of consolette's config already handles.

**Open question**: whether tier transitions should be sticky (once Full, stay Full until
pressure drops below a lower watermark — hysteresis) or purely a function of current
pressure. Flapping between tiers on borderline pressure would re-run summarization
wastefully; needs a decision in the plan phase, not resolved here.

### `ConversationSummarizer` (deterministic, LLM-free)

The issue is explicit that this must not call an LLM — a hard divergence from
`compaction-hook`'s `ClaudeCliSummarizer`, which shells out to `claude -p --resume`
specifically because it can afford an LLM round-trip (it's an out-of-band CLI command,
not a hot path). This one runs inline on every request once a tier requires it, so it
must be cheap and reproducible.

Deterministic summarization here means structural reduction, not prose generation:
collapse a turn to `{role, first N chars of first text block, tool names called, tool
call count}` — closer to `structural_collapse.rs`'s "first+summary+last" template
collapse than to a written summary. This should be built as a **new** function reusing
`collapse_repeated_templates`'s run-detection shape
([`src/compression/structural_collapse.rs`](../../../src/compression/structural_collapse.rs))
where the *pattern* transfers (identify a run, keep first/last, replace the middle with a
compact marker) even though the *input* (parsed message turns, not text lines) doesn't,
mirroring how `compaction-hook` reused `compression`'s primitives without reusing its
data model.

**Open question**: what "summarized" looks like on the wire — does the model see a
synthetic `text` block describing the dropped turns, or are they silently removed?
`compaction-hook`'s `compact_boundary` row convention (ADR-011) is the closest prior art
in this repo for "mark a summarization boundary explicitly" and should be evaluated for
reuse.

### `ToolResultBudget`

Age-ranks `tool_result` content blocks across the message list (oldest first) and
truncates/elides all but the N most recent, where N is config-driven. This is the
simplest of the five components and the most reusable: it can lean directly on
`SmartCrusher`'s existing JSON-array field-elision approach
([`src/compression/smart_crusher.rs`](../../../src/compression/smart_crusher.rs)) applied
across the message list instead of within one message, and on `RewindStore`
(`src/compression/rewind.rs`) for the "elided but retrievable via hash" contract — a
truncated tool result should be rewind-able the same way an in-request compressed block
is today, not a one-way deletion.

### `PlanReinjection` / `SkillReinjection`

Re-inject the active plan (TODO list equivalent) and tool/skill schemas after a
summarization pass that might have dropped the turn where they were originally
established. Requires `SessionState` to track "what plan/skill context is currently
active" independent of the raw message history — this is genuinely new state, not a
transform of existing state, and is the part of this design most dependent on the not-
yet-verified session-identity question above (see "the real gap"). Blocked on that
being resolved first.

### `CompactHooks`

`trait CompactHooks { fn pre_compact(&self, ctx: &SessionState); fn post_compact(&self,
ctx: &SessionState, report: &CompactionReport); }`, registered in a `Vec<Arc<dyn
CompactHooks>>` on `SessionCompactionPipeline`, mirroring the existing
`Vec<Arc<dyn Availability>>` seam from ADR-003. Given this repo has no plugin-loading
mechanism today (ADR-007 only covers credential-helper subprocesses), start with
in-process hook registration only; document that dynamic/external plugin loading is
explicitly out of scope for this pass, the same way ADR-003 scoped hot-reload out.

## Recommendation

1. **Resolve the session-identity prerequisite first** (how consolette recognizes "this
   request continues that session") against real traffic — this blocks `SessionState`,
   which blocks all five components, most acutely `PlanReinjection`/`SkillReinjection`.
   This is a verification spike in the same spirit as issue #8, not a design question.
2. Write an ADR (next available consolette number after ADR-007) once the session-
   identity spike lands, covering: `SessionState` storage/eviction, the `SessionKey`
   derivation, and the tier-hysteresis question flagged above. This document is deliberately
   one level above ADR granularity — five components share one architectural placement,
   but each still needs its own accepted-decision record before implementation.
3. Implement `ToolResultBudget` first — it needs no new summarization logic, reuses
   `SmartCrusher`/`RewindStore` directly, and validates the `SessionState` plumbing
   end-to-end before the harder `ConversationSummarizer`/reinjection work builds on it.
