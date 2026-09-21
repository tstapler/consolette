# Research: Stack — context-audit-real-usage

Builds on `project_plans/context-audit-real-tokens/research/stack.md` (earlier run). Re-verified 2026-09-20 against `~/dotfiles/.claude/skills/context-audit/` (master, clean tree for that dir; last commit `4a5ec3d`).

## Stack (VERIFIED)

- Python 3.14.7 (`python3 --version`, linuxbrew). Stdlib only: `argparse, json, sqlite3, sys, collections.defaultdict` (`scripts/context_audit.py:19-23`). Tests add `tempfile, unittest, pathlib` (`scripts/test_context_audit.py:2-8`).
- Dependencies needed: none. pytest is not usable on PATH python (earlier research: `python3 -m pytest` fails), so `unittest` is the only compliant runner.

## Correction to earlier research

Earlier stack.md described a 328-line file and a not-yet-written test. Now: `context_audit.py` is 351 lines, `test_context_audit.py` 191 lines (already landed, `4a5ec3d`). Line refs moved:

- `analyze()` last-usage-wins: `context_audit.py:125-131` (`is_synthetic = message.get("model") == "<synthetic>"`; assign only if dict and not synthetic).
- `None`/`None` guard: `:172-190` (`if last_usage:` at 174).
- `store_sqlite()` ALTER migration: `:270` (in CREATE TABLE at `:265`).

## Test run (VERIFIED)

From skill dir: `python3 -m unittest discover -s scripts -v` -> 6 tests, OK (4 usage-tracking, 1 sqlite migration, 1 Pi). Also `cd scripts && python3 -m unittest test_context_audit -v` (the command in SKILL.md:67) -> 6 tests, OK.

## tests/ layout feasibility (VERIFIED in a scratch copy under /tmp, dotfiles untouched)

Moving `test_context_audit.py` to `scripts/tests/` as-is breaks:

- `unittest discover -s scripts/tests` -> 1 error (import fails: `context_audit` not on `sys.path`; the file has a bare `from context_audit import ...` at `test_context_audit.py:9`, no path shim).
- `unittest discover -s scripts` -> "NO TESTS RAN" (no `tests/__init__.py`, so not descended).
- `discover -s scripts/tests -t scripts` -> ImportError "Start directory is not importable" (no `__init__.py`).

A move requires the golang-profiling shim (`sys.path.insert(0, str(Path(__file__).resolve().parent.parent))` before the import, precedent `golang-profiling/scripts/tests/test_pct_breakdown.py` per earlier research) and a SKILL.md:67 command update. That is the only cost; the requirements default (b, keep in `scripts/`) needs no code change. AC 4's literal command (`discover -s .../scripts/tests`) only passes with option (a).

## SKILL.md accuracy vs code (VERIFIED)

- SKILL.md:31-38 "Actual tokens ... from the *last* assistant turn", `None` when no usage, synthetic skipped: matches `context_audit.py:125-131, 172-174`.
- SKILL.md:39-45 estimated = `len(text)//4` (`estimate_tokens`, `context_audit.py:26-29`), cumulative, drives category breakdown/recommendations: matches `recommend()` comment at `:209`.
- One nit: SKILL.md:67 says run `python3 -m unittest test_context_audit -v` "from `scripts/`"; it works (6 OK) but differs from the `discover -s` form in AC 4. No inaccuracy found; doc edit is optional.
- Minor un-documented nuance: `usage_breakdown` also accepts Pi-style keys (`input/output/cacheRead/cacheWrite`, `:176-183`); `actual_total_tokens` sums all four including `output` (`:185`). SKILL.md says "current context window size" without mentioning output is included. Not wrong enough to require a change; flag only.

## Recommendation

Keep (b): tests stay in `scripts/`; record AC 0/4 as satisfied in substance (6 = 5 + pre-existing Pi test) with the path deviation stated. No dependency changes, no production code changes (AC 6 holds: `git status` clean under the skill dir).
