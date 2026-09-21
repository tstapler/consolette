# Requirements: context-audit-real-usage

**Backlog item**: 432f18b7-e02d-489e-a873-8e47e9490628 (source: tstapler/consolette#1)
**Type**: test-coverage gap closure. Complexity: 1 (quick).
**Target repo**: `~/dotfiles` (`.claude/skills/context-audit/`), not this repo.

## Problem

`context_audit.py` should report real API-reported usage (`message.usage.*`, last
non-synthetic assistant turn) as its headline "actual tokens", keeping `len(text)//4`
only for per-category attribution. The item's AC narrows the work to regression tests
for that behavior; production code must not change.

## Baseline (VERIFIED 2026-09-20)

- Feature already shipped: `f83b7b5` (skill), `c2ed9ce` (skip synthetic lines).
- Tests already landed in dotfiles `master` as `4a5ec3d`
  ("test(context-audit): cover last-usage-wins, synthetic-skip, and sqlite migration"),
  in `.claude/skills/context-audit/scripts/test_context_audit.py`.
- `python3 -m unittest discover -s scripts -v` (run from the skill dir): 6 tests, OK.
- `SKILL.md:67` already documents `python3 -m unittest test_context_audit -v`.

## Acceptance criteria (from item, 0-indexed as in the tracker)

| # | Criterion | Current state |
|---|---|---|
| 0 | Test: `analyze()` `actual_total_tokens` = last non-synthetic turn, not a sum | Covered (`test_actual_total_tokens_uses_last_turn_not_sum`) |
| 1 | Tests: trailing-synthetic and all-synthetic never contaminate total | Covered (two tests) |
| 2 | Test: no usage anywhere -> `actual_total_tokens=None`, `usage_breakdown=None`, no raise | Covered (`test_no_usage_data_yields_none`) |
| 3 | Test: `store_sqlite()` ALTER TABLE migration idempotent on legacy table; rows read back | Covered (`test_migrates_legacy_table_idempotently`) |
| 4 | `python3 -m unittest discover -s .../scripts/tests` exits 0, 5 tests pass | **Literal mismatch**: no `tests/` dir; file lives in `scripts/`; 6 tests (5 + 1 pre-existing Pi test) |
| 5 | SKILL.md real-vs-estimated docs confirmed accurate; one line on running tests | Line exists; accuracy re-check needed |
| 6 | No production-logic changes (analyze/store_sqlite/hook/cmdcrush) | Must hold |

## Open decision

AC 0 and 4 name `scripts/tests/test_context_audit.py`; the shipped file is
`scripts/test_context_audit.py`. Options: (a) move into `tests/` to match the AC
literally (touches dotfiles, needs `sys.path` handling for the import), or
(b) keep as is and record AC 0/4 as satisfied in substance with the path deviation
stated. Default: (b) unless the reviewer objects, since moving adds churn with no
behavioral gain.

## Non-goals

No changes to `analyze()`/`store_sqlite()` logic, the PostCompact hook, or
`cmdcrush/main.rs`.

## Notes

An earlier triage left `project_plans/context-audit-real-tokens/` (requirements,
research, plan, validation) on this branch; it reaches the same conclusion. This
directory is the one requested for the current run.
