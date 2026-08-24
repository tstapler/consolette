# ADR-004: Subagent token attribution — separate ledger, not rolled into parent totals

**Status**: Accepted
**Date**: 2026-08-24
**Relates to**: `research/pitfalls.md` §4 ("needs a decision before schema design, not an implicit default")

## Context

Claude Code subagent (Task tool) invocations run their own nested conversation with their own `usage` figures, written into the transcript as `isSidechain: true` rows, which `claude_code_session::transcript::build_turns` explicitly excludes from the main chain (`transcript.rs:249-254`). Today's turn/chain reconstruction has no representation of subagent token spend at all.

Four options were identified: (a) count a subagent's spend against the parent turn that spawned it, (b) treat it as its own independent session, (c) both, clearly labeled (deliberate double-count), or (d) a separate `subagents` ledger that doesn't roll up into session-level totals automatically. Getting this wrong either double-counts subagent-heavy sessions or makes them look artificially cheap — both are correctness failures for a tool whose entire premise is trustworthy numbers.

## Decision

(d): subagent invocations are recorded in their own `subagents` table (Phase 4, Story 4.2.2), correlated via `SubagentStart`/`SubagentStop` hook events. Parent-session `PeakContext`/composition/cost queries never join against `subagents` by default, and never will once real usage totals are added — this part of the decision doesn't change with the scope note below.

**Scope note (v1 vs. future pass)**: `SubagentStart`/`SubagentStop` hook events carry timestamps, not token usage — populating `SubagentRow`'s `AnthropicUsage` fields requires parsing the subagent's own transcript content (`isSidechain: true` rows in the parent `.jsonl`, currently excluded wholesale by `claude_code_session::transcript::build_turns`), which is a separate, non-trivial parsing task that Phase 4's Story 4.2.2 does not include. Rather than promise a number Phase 4 can't produce, v1 scopes `SubagentRow` to invocation correlation only: start/end timestamps, giving invocation count and duration per parent session. The dashboard's "Subagent spend" line (a dollar/token figure) is **deferred to a future pass** that adds sidechain-transcript parsing; until then there is no dashboard affordance promising real subagent token totals. This still satisfies requirements.md's "subagent activity" capture bullet, which does not itself require token-level figures.

## Consequences

- A subagent-heavy session's parent-session numbers stay accurate and comparable across sessions (no silent inflation or deflation) — true today and unaffected by the scope note above.
- v1 dashboard/store surfaces subagent invocation count and duration only, not cost/tokens; a future pass adds `AnthropicUsage` totals once sidechain-transcript parsing is implemented, at which point "Subagent spend" can be added as its own explicitly labeled line, never folded into the parent session's `tool_io_tokens`/`peak_context_tokens`.
- Total spend across a parent session *and* its subagents (once implemented) will require an explicit, separate query/UI affordance — not automatic — which is the intended tradeoff: visibility without ambiguity about what a given number includes.
- This mirrors context-analyzer's own separate `subagents`/`subagent_api_calls` table design, so future comparison against its behavior (if ever revisited) stays apples-to-apples.
