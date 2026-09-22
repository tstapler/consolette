# Verification evidence: context-audit-real-usage

Run 2026-09-20. All commands used `PYTHONDONTWRITEBYTECODE=1`. Tracker criteria are 0-indexed.

## Decision D1 (decided by coordinator, no user present)

Added one test, `test_trailing_usageless_turn_keeps_last_real_usage`, in a separate dotfiles worktree on
branch `test/context-audit-usageless-turn`, commit `62cf5b6` (1 file, +10 lines, test file only).
Mutation evidence (scratch copy, not in `~/dotfiles`):

| Mutant | Result |
|---|---|
| M6: usage-less assistant line resets `last_usage` to None | new test FAILS (1 failure) — mutant now killed |
| M1: drop the synthetic check | 2 failures (`test_all_synthetic_transcript_yields_none`, `test_trailing_synthetic_turn_excluded_from_usage`) |

Branch is local only (not pushed, not merged into dotfiles `master`); `master` still has the 6-test file.

## Per-criterion results

| AC | Evidence | Result |
|---|---|---|
| 0 | `test_actual_total_tokens_uses_last_turn_not_sum` passes (asserts 850 from last turn, not sum) | Met |
| 1 | `test_trailing_synthetic_turn_excluded_from_usage`, `test_all_synthetic_transcript_yields_none` pass; M1 mutant fails both | Met |
| 2 | `test_no_usage_data_yields_none` passes (None/None, no raise) | Met |
| 3 | `test_migrates_legacy_table_idempotently` passes (two `store_sqlite` calls on a legacy table, row read back) | Met |
| 4 | `discover -s scripts`: master = 6 tests OK; branch `62cf5b6` = 7 tests OK. No `scripts/tests/` dir exists and the count is not 5 | Met in substance (D0: path and count deviation; see below) |
| 5 | Per-claim table below; one test-run line already at SKILL.md:67, still valid | Met, no edit |
| 6 | `git diff --stat main...HEAD -- '*.rs'` empty; 0 commits touching `context_audit.py` or hooks after `4a5ec3d`; `62cf5b6` touches only the test file | Met |

### AC 4 deviation (D0)
The AC names `scripts/tests/` and "5 tests". Moving the file breaks `from context_audit import` and the
SKILL.md:67 command, so it stays in `scripts/`. The 5 named behaviors are covered; the run count is 6 on
master (5 + a pre-existing Pi-session test) and 7 on the D1 branch.

### AC 5 per-claim table (SKILL.md vs `context_audit.py`)

| SKILL.md claim | Code | Match |
|---|---|---|
| Actual = last assistant turn's usage fields (:32-36) | `last_usage = usage` overwrites each turn (:125-131); sum of 4 fields (:172-190) | Yes |
| `None` if no assistant usage (:35) | `actual_total_tokens = None` unless `last_usage` (:172-174) | Yes |
| Synthetic lines skipped (:36-38) | `is_synthetic` guard (:125-131) | Yes |
| Estimated = `len//4` over whole transcript, drives category breakdown (:39-45) | `estimate_tokens`, `total = sum(by_type.values())` (:170) | Yes |
| CLI shows both as separate labeled lines (:70) | `main()` :331-335 prints "Actual tokens (from usage, last assistant turn)" and "Estimated tokens (chars/4 ...)"; unavailable case :334 | Yes |
| Trend db records actual total | `actual_total_tokens INTEGER` column + guarded ALTER (:262-270) | Yes |

Nit, not edited (AC says no edit needed): `actual_total_tokens` sums output tokens too (:185), and the docs call it
"current context window size"; the sum also accepts Pi-style key names (`input`, `cacheWrite`, ...) that the
docs don't mention.

## Review round 1 fix (verdict FAIL: test path)

Reviewer required the literal `scripts/tests/` path. Done on dotfiles branch
`test/context-audit-usageless-turn`, commit `fb2db1d` (test file moved via `git mv` with a `sys.path`
shim; SKILL.md test-run line changed to `python3 -m unittest discover -s scripts/tests -v`; no production change).

- AC command, from the dotfiles repo root: `python3 -m unittest discover -s .claude/skills/context-audit/scripts/tests` -> `Ran 7 tests ... OK`.
- SKILL.md command, from the skill dir: `python3 -m unittest discover -s scripts/tests` -> `Ran 7 tests ... OK`.
- AC 4 count: 7 tests, not 5 (5 required behaviors + pre-existing Pi test + the D1 test). This supersedes the earlier D0 path deviation; only the count differs.
- `git diff --stat HEAD~2 -- .../context_audit.py` on the branch is empty (AC 6 holds).
