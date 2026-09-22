# Requirements: context-audit-real-tokens

**Date**: 2026-09-04
**Type**: bug fix / small gap-closure (verification found the described feature already shipped)
**Complexity**: 1 — quick task

## Problem Statement

Backlog item `432f18b7-e02d-489e-a873-8e47e9490628` asks that the `context-audit` Claude
Code skill (`~/dotfiles/.claude/skills/context-audit/scripts/context_audit.py`) report
real API-reported token usage (`message.usage.*` on assistant transcript lines) instead
of relying solely on the `len(text)//4` heuristic for its headline "how much context did
this session use" number.

**Pre-implementation finding (VERIFIED by reading the file directly, not inferred):**
this has already been built. `git log --oneline -- .claude/skills/context-audit/scripts/context_audit.py`
in `~/dotfiles` shows two commits:

- `f83b7b5` "feat(claude): add context-audit skill and postcompact hook"
- `c2ed9ce` "fix(context-audit): skip synthetic assistant lines when computing real usage total"

Reading the current file (328 lines) confirms every item in the original description is
implemented:

| Ask | Status | Evidence |
|---|---|---|
| Track last assistant `usage` snapshot (not summed) | ✅ done | `analyze()` sets `last_usage` from each non-synthetic assistant line's `message.usage`, overwriting rather than accumulating — [context_audit.py:97-112](../../.claude/skills/context-audit/scripts/context_audit.py) |
| Skip `message.model == "<synthetic>"` lines | ✅ done | `is_synthetic` check, same block |
| Report authoritative "actual tokens" total | ✅ done | `actual_total_tokens` + `usage_breakdown` (input/output/cache_creation/cache_read) in the returned report — lines 153-162 |
| Keep char/4 heuristic for per-category attribution | ✅ done | `by_type`/`breakdown` (thinking/text/tool_use/tool_results/attachments) still computed exactly as before, `recommend()` unchanged |
| CLI/JSON output distinguish real vs. estimated | ✅ done | CLI text output prints "Actual tokens (from usage, last assistant turn)" and "Estimated tokens (chars/4 heuristic...)" as two separate labeled lines (main(), ~line 306-312); JSON report carries both fields un-conflated |
| SKILL.md docs distinguish real vs. estimated | ✅ done | "Two totals are reported, on different bases" section, SKILL.md:30-45 |
| `store_sqlite()` schema/migration for real usage | ✅ done | `compactions` table gets an `actual_total_tokens INTEGER` column; `ALTER TABLE ... ADD COLUMN` wrapped in `try/except sqlite3.OperationalError` so it's a no-op against a DB that already has the column — safe against both a fresh DB and the pre-existing `f83b7b5`-era schema |

**Remaining gap found during this triage** (not in the original ask, but flagged by the
repo's own engineering-discipline bar — "non-trivial logic leaves one runnable check"):
`context_audit.py` has zero tests. There is no `test_*.py` next to it, and nothing in CI
exercises `analyze()`, `is_synthetic` filtering, or the `store_sqlite()` migration path.
This is real content risk: the synthetic-line filter and the "last usage wins" logic are
exactly the kind of one-line-changes-silently-wrong bugs a regression test exists to catch
(the `c2ed9ce` fix commit itself was a silent correctness bug that shipped once already).

## Baseline

Today: `context_audit.py` already computes and reports both real and estimated tokens
correctly (per the file read above) but has no automated test, so a future edit (e.g.
touching `analyze()`'s usage-tracking branch) could silently reintroduce the exact bug
`c2ed9ce` fixed, or a new one, with nothing to catch it before it reaches a user's
PostCompact hook output.

## Users / Consumers

- The user (Tyler) invoking the `context-audit` skill manually or via `/status`-adjacent
  workflows.
- The `PostCompact` hook (`~/dotfiles/.claude/hooks/context-audit-postcompact.sh`), which
  runs this script unattended after every compaction and writes to the shared
  `~/.claude/context-audit/trend.db`.

## Success Metrics

- `context_audit.py` has one runnable self-check (`test_context_audit.py` or an
  `assert`-based `__main__`/`demo()` block) that fails if `analyze()`'s real-usage
  tracking (last-usage-wins, synthetic-line skip) regresses.
- No behavior change to already-shipped output — this closes a test-coverage gap, it does
  not re-implement the feature.

## Appetite

Small (well under a day — this is a coverage-only addition to an already-complete feature).

## Constraints

- Do not modify the `PostCompact` hook's invocation flags/behavior beyond what's needed to
  keep recording accurate data (explicit non-goal from the original item).
- Do not rewrite the per-block breakdown/recommendation logic (explicit non-goal).
- No new runtime dependencies — stdlib `unittest` (or plain `assert` + `__main__`) only,
  consistent with the script's current zero-dependency stdlib-only style.

## Non-functional Requirements

- **Performance SLO**: not applicable (offline analysis script).
- **Scalability**: not applicable.
- **Security classification**: internal (local dev tooling, no network/secrets).
- **Data residency**: not applicable.

## Scope

### In Scope

- A regression test (or `assert`-based self-check) covering: (1) real-usage total takes
  the *last* non-synthetic assistant `usage`, not a sum; (2) synthetic assistant lines
  (`message.model == "<synthetic>"`) are excluded; (3) a transcript with no `usage` data
  at all yields `actual_total_tokens: None` gracefully; (4) `store_sqlite()`'s
  `ADD COLUMN actual_total_tokens` migration is idempotent against a pre-existing table
  that lacks the column.
- Confirming (not re-writing) that CLI/JSON output and `SKILL.md` still accurately
  distinguish real vs. estimated tokens — a docs/output audit, not new copy.

### Out of Scope

- Re-implementing or changing `analyze()`'s existing usage-tracking logic — it is already
  correct per the file read above.
- Changing the `PostCompact` hook.
- Any change to `cmdcrush`/`claude-proxy-rs` (`src/bin/cmdcrush/main.rs`) — it was cited
  in the original item purely as a reference implementation for extracting `usage` from
  assistant lines, and needs no changes itself.

## Rabbit Holes

- Building fixture JSONL transcripts by hand is fiddly (real transcripts have many
  incidental fields). Keep fixtures minimal — only the fields `analyze()` actually reads.
- Resist the urge to add a full `pytest` suite/fixtures directory for one script; that's
  scope creep against the Small appetite and the repo's existing zero-dependency style.

## Alternatives Considered

- **Do nothing** (item is already done) — rejected: the missing test is a real,
  independently-justified gap or a silent-regression risk exists on the next edit.
- **Full pytest suite with fixtures** — rejected as over-scoped for one 328-line script;
  stdlib `unittest`/`assert` matches the codebase's existing lightweight style.

## Feasibility Risks

- None identified — this is additive test coverage against fully-understood, already-read
  code.

## Observability Requirements

N/A — complexity 1.

## Risk Control

N/A — complexity 1, test-only change, not needed.

## Open Questions

None — the original item's technical questions are all resolved by reading the current
file (see table above). The only remaining question (whether to add tests) is answered by
this repo's own engineering-discipline bar, not left open.
