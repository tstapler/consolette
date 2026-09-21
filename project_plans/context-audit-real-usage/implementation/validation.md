# Validation Plan: context-audit-real-usage

**Date**: 2026-09-20
**Calibration**: Complexity 1. One check per acceptance criterion (no unit/error/integration triad). Implementation is verification only; tests shipped in dotfiles `4a5ec3d`.

Shorthand: `SKILL=/home/tstapler/dotfiles/.claude/skills/context-audit`. All commands are read-only against dotfiles and use `PYTHONDONTWRITEBYTECODE=1` so no `__pycache__` is written there. Tracker `criteria_index` is 0-indexed and equals the AC number.

## Happy Path Scenario
Given dotfiles at a HEAD containing `4a5ec3d` (6 tests, feature shipped in `f83b7b5`/`c2ed9ce`), when the suite is run from `$SKILL` with `discover -s scripts`, then it exits 0 with `Ran 6 tests ... OK`, and each of the regression tests named below passes individually.

## Requirement -> Test Mapping

Test names verified by `grep -nE 'def test_|^class '` on `test_context_audit.py` (2026-09-20): lines 41, 93, 106, 116, 126, 164.

| AC (tracker idx) | Requirement | Test File | Check (test name / command) | Type | Expected observable result |
|---|---|---|---|---|---|
| 0 | `actual_total_tokens` = last non-synthetic turn, not a sum | `scripts/test_context_audit.py` | `ClaudeCodeUsageTrackingTest.test_actual_total_tokens_uses_last_turn_not_sum` (:93). Cmd: `cd $SKILL/scripts && PYTHONDONTWRITEBYTECODE=1 python3 -m unittest test_context_audit.ClaudeCodeUsageTrackingTest.test_actual_total_tokens_uses_last_turn_not_sum -v` | Existing unit | `... ok`, `Ran 1 test`, `OK`, exit 0. Deviation D0: file is in `scripts/`, not `scripts/tests/`. |
| 1 | Trailing-synthetic and all-synthetic never contaminate total | same | `test_trailing_synthetic_turn_excluded_from_usage` (:106) and `test_all_synthetic_transcript_yields_none` (:116). Cmd: same as above with both dotted names in one invocation | Existing unit (2 tests, one AC) | Both `ok`, `Ran 2 tests`, `OK`, exit 0 |
| 2 | No usage anywhere -> `actual_total_tokens=None`, `usage_breakdown=None`, no raise | same | `test_no_usage_data_yields_none` (:126). Cmd: `... unittest test_context_audit.ClaudeCodeUsageTrackingTest.test_no_usage_data_yields_none -v` | Existing unit | `ok`, `Ran 1 test`, `OK`, exit 0 (no exception) |
| 3 | `store_sqlite()` ALTER TABLE migration idempotent on legacy table; rows read back | same | `StoreSqliteMigrationTest.test_migrates_legacy_table_idempotently` (:164). Cmd: `... unittest test_context_audit.StoreSqliteMigrationTest -v` | Existing integration (sqlite) | `ok`, `Ran 1 test`, `OK`, exit 0 |
| 4 | Suite exits 0 (AC text: `discover -s .../scripts/tests`, 5 tests) | all of `scripts/` | Cmd: `cd $SKILL && PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -v; echo rc=$?` | Suite run | `Ran 6 tests`, `OK`, `rc=0` (verified today). Report as "met in substance": 6 = 5 regression tests + pre-existing `ContextAuditTest.test_analyzes_pi_session_messages_and_usage` (:41). Literal `scripts/tests/` path and "5 tests" cannot match (D0). |
| 5 | SKILL.md real-vs-estimated docs accurate; one line on running tests | `$SKILL/SKILL.md`, `scripts/context_audit.py` | (a) `grep -n "unittest test_context_audit" $SKILL/SKILL.md`; (b) per-claim table: SKILL.md:30-45 vs `context_audit.py:26-29,125-131,172-190` and `main()` labels ~L306-332 | Doc check (manual, read-only) | (a) prints line 67 (verified: `Tests: python3 -m unittest test_context_audit -v (run from scripts/)`). (b) every claim (last turn only; four usage fields; `None` when no usage; synthetic skipped; estimate is `len//4`) maps to a code line with a match; the `main()` labels are confirmed during the run (not yet checked). Note: total includes `output_tokens` (`:185`), consistent with docs. |
| 6 | No production-logic change (analyze/store_sqlite/hook/cmdcrush) | dotfiles git history; this worktree | (a) `git -C /home/tstapler/dotfiles log --oneline 4a5ec3d..HEAD -- .claude/skills/context-audit/scripts/context_audit.py`; (b) `git -C /home/tstapler/dotfiles status --short -- .claude/skills/context-audit`; (c) in this worktree `git diff --stat main...HEAD -- '*.rs'` (three dots; two-dot lists ~52 unrelated files) | Git gate | (a) empty (verified: 0 lines); (b) empty; (c) empty (verified: 0 lines). Full-history sanity: `git -C /home/tstapler/dotfiles log --oneline -- .claude/skills/context-audit/scripts/context_audit.py` lists exactly `5d68be7`, `c2ed9ce`, `f83b7b5`. |

### AC 6 history note (verified 2026-09-20)
The full history of `context_audit.py` is three commits: `f83b7b5` (2026-08-14, feature), `c2ed9ce` (2026-08-14, synthetic skip), and `5d68be7` (2026-09-11, "Add kibitzer/dotfiles-hooks pi plugins ... and misc fixes", +43/-10 lines in `context_audit.py`, +60 in the test file). `5d68be7` is an older, pre-existing commit that predates `4a5ec3d` (2026-09-15), so the gate is "no commits after `4a5ec3d`" (0 found), not "nothing beyond c2ed9ce/f83b7b5". Any reader expecting only c2ed9ce/f83b7b5-era history should note `5d68be7` as a known earlier change, not new work on this run. The PostCompact hook is checked through dotfiles history only (not this repo); `src/bin/cmdcrush/main.rs` is covered by check (c).

## UX Acceptance Tests
N/A. No user-facing surface (no `design/ux.md`).

## Migration Test
N/A for this validation. AC 3 exercises an existing idempotent migration; no new migration is introduced (`migration_should_be_reversible` not applicable; the migration is add-column only).

## Test Stack
- **Unit / integration**: stdlib `unittest` (already used; SKILL.md:67). No pytest.
- **E2E / UX**: none.
- **Optional (D1, not an AC)**: mutation check on a scratch copy under `$SCRATCH/mut/`, never in dotfiles. Record observed results, not predicted ones. Mutant M6 (usage-less assistant turn clears `last_usage`) is expected to survive; that is a known gap, not an AC failure.

## Coverage Targets and How to Measure

| Stack | Coverage command | Target |
|---|---|---|
| Python (stdlib) | Not measured; coverage is requirements-mapped (below), not line-coverage gated | N/A |

## Requirements Coverage: 7/7

| AC | Covered by | Status |
|---|---|---|
| 0 | test :93 | Covered |
| 1 | tests :106, :116 | Covered |
| 2 | test :126 | Covered |
| 3 | test :164 | Covered |
| 4 | discover run, 6 tests | Covered in substance (path/count deviation D0) |
| 5 | grep + per-claim table | Covered (main() labels to be confirmed in run) |
| 6 | 3 git checks | Covered |

## Gaps
- AC 4 literal wording (`scripts/tests/`, 5 tests) cannot pass; tracker call must carry the D0 deviation text.
- AC 5 `main()` label check (~L306-332) is pending, to be done during implementation.
- M6 (usage-less trailing turn) has no test; optional and gated on D1 approval. Not required by any AC.
