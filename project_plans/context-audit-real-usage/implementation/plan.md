# Implementation Plan: context-audit-real-usage

**Feature**: Close backlog item 432f18b7 (consolette#1) by verifying the already-shipped regression tests for real-usage token reporting in `~/dotfiles/.claude/skills/context-audit/`.
**Date**: 2026-09-20
**Status**: Ready for implementation
**ADRs**: None (both decisions below are recorded inline)

Verification-only plan. Tests landed in dotfiles `4a5ec3d`; no production change is planned. Nothing under `/home/tstapler/dotfiles` is modified except, if and only if Decision D1 is approved, one added test in its own commit.

Shorthand: `SKILL=/home/tstapler/dotfiles/.claude/skills/context-audit`, `SCRATCH=<session scratchpad dir>`.

---

## Pattern Decisions

| Component | Pattern Chosen | Source | Alternative Rejected | Reason |
|-----------|---------------|--------|---------------------|--------|
| Approaches (step 0.5) | A: verify-only, keep tests in `scripts/` | research/build-vs-buy.md sec. 4-5 | B: move to `scripts/tests/` + `sys.path` shim + SKILL.md:67 edit; C: rewrite tests with pytest/hypothesis | A: zero churn, already green. B: literal AC path but churn in another repo, no behavior gain. C: new dependency, pytest is not available on PATH python |
| Test runner | stdlib `unittest` | existing | pytest | Already used; documented at SKILL.md:67 |
| Mutation evidence | Manual mutants on a scratch copy | research/pitfalls.md sec. 1 | Mutating in place | Production code must not change (AC 6) |

## Tech Debt Disposition

None identified. (Known-unfixed null-`usage` TypeError and sqlite lock conflation are out of scope per AC 6; not touched.)

## Migration Plan
N/A — complexity 1.

## Observability Plan
N/A — complexity 1.

## Risk Control
N/A — complexity 1. Standard revert applies; this repo gets docs/evidence only.

## Recorded Decisions

- **D0 (decided): AC 0/4 path deviation and 5-vs-6 count — keep in place.** Tests stay at `scripts/test_context_audit.py` (AC text says `scripts/tests/`). AC 4 is satisfied in substance: `discover -s scripts` exits 0 with 6 tests = 5 regression tests + 1 pre-existing Pi test. Do not create an empty `tests/` dir, do not report "Ran 5", do not drop the Pi test.
- **D1 (FLAGGED, needs user approval): close the surviving-mutant gap M6?** Mutant "usage-less assistant turn after a real turn clears `last_usage`" survives all 6 tests (research/pitfalls.md sec. 1-2). **Recommendation: yes, add one ~10-line test** (real-usage turn, then trailing assistant line with no `usage` -> `actual_total_tokens` still equals the real turn). It guards a realistic transcript shape (partial/streaming lines) and a plausible refactor ("last assistant line's usage"). Cost: needs a commit in `~/dotfiles` on its own branch, only that one file staged. Not required by any AC; if declined, record M6 as a known gap in the evidence file and close AC 0-6 anyway. Default until approved: do not touch dotfiles.
- **D2 (decision in Story 1.3): SKILL.md clarifying line.** Recommendation: no edit. AC 5 says none needed beyond the existing test line (SKILL.md:67).
- **Real gap in production code found?** No. AC 6 holds; no production change is justified by the evidence.

## Unresolved Questions
- [ ] Approve or decline D1 (add M6 test to dotfiles)? — blocks Story 1.4 only — owner: user/coordinator

## Dependency Visualization

```
1.1 baseline ──> 1.2 AC0-4,6 mapping ──┐
        │                              ├──> 1.5 evidence file + report_progress
        ├──> 1.3 SKILL.md AC5 check ───┤
        └──> 1.4 mutation check ──> [D1 approved?] ──> 1.4d optional M6 test
```

Backlog mapping: tracker criteria are 0-indexed (`report_progress` criteria_index 0..6), matching the AC numbers below.

---

## Phase 1: Verify and close

### Epic 1.1: Evidence for AC 0-6
**Goal**: Every acceptance criterion has a command run against `~/dotfiles` and its output recorded.

#### Story 1.1.1: Baseline
**As a** reviewer, **I want** the suite run from the documented locations, **so that** later checks rest on a known-green baseline.
**Acceptance Criteria**:
- Suite is green from both invocations.
  - *Given* dotfiles at HEAD containing `4a5ec3d`, *When* `cd $SKILL && python3 -m unittest discover -s scripts -v` and `cd $SKILL/scripts && python3 -m unittest test_context_audit -v`, *Then* each prints `Ran 6 tests` ... `OK`, exit 0.
**Files**: none modified.

##### Task 1.1.1a: Confirm commit and run suite (~3 min)
- `git -C /home/tstapler/dotfiles merge-base --is-ancestor 4a5ec3d HEAD && echo ok` -> `ok`
- Run both commands above; save output to `$SCRATCH/baseline.txt`.

#### Story 1.1.2: Map AC 0-4 and 6 to commands
**Acceptance Criteria** (one row per criterion, all read-only):
- AC 0 (last turn, not sum)
  - *Given* a transcript with two assistant turns of different usage, *When* `cd $SKILL/scripts && python3 -m unittest test_context_audit.ClaudeCodeUsageTrackingTest.test_actual_total_tokens_uses_last_turn_not_sum -v`, *Then* `... ok`, `Ran 1 test`, `OK`.
- AC 1 (trailing/all synthetic)
  - *Given* fixtures with a trailing `<synthetic>` line and an all-`<synthetic>` file, *When* `grep -n "def test_.*synthetic" $SKILL/scripts/test_context_audit.py` then run both names with `-v`, *Then* two tests found (research: :106, :116), both `ok`.
- AC 2 (no usage -> None)
  - *Given* one assistant line without `usage`, *When* run `test_no_usage_data_yields_none -v`, *Then* `ok`; test asserts `actual_total_tokens is None` and `usage_breakdown is None`.
- AC 3 (idempotent ALTER)
  - *Given* a legacy `snapshots`-style table without `actual_total_tokens`, *When* run `StoreSqliteMigrationTest -v`, *Then* `test_migrates_legacy_table_idempotently ... ok`; the test calls `store_sqlite` twice and reads the value back.
- AC 4 (suite exit 0)
  - *Given* Task 1.1.1a output, *When* `echo $?` after `discover -s scripts`, *Then* `0` and `Ran 6 tests`; record D0 deviation and the 5+1 breakdown in the evidence file.
- AC 6 (no production change)
  - *Given* dotfiles HEAD, *When* `git -C /home/tstapler/dotfiles diff --stat -- .claude/skills/context-audit` and `git -C /home/tstapler/dotfiles log --oneline 4a5ec3d..HEAD -- .claude/skills/context-audit/scripts/context_audit.py`, *Then* both print nothing. Also confirm `cmdcrush/main.rs` and the PostCompact hook are untouched by this branch: `git -C <this worktree> diff --stat main -- '*.rs'` shows no changes to `main.rs`.
**Files**: none modified.

##### Task 1.1.2a: Run the six mapped commands (~5 min)
- Run each command above; append output to `$SCRATCH/ac-evidence.txt`. Confirm the exact test names with grep first; adjust names if they differ from research.

### Epic 1.2: SKILL.md accuracy (AC 5)
**Goal**: Confirm the real-vs-estimated docs match code.

#### Story 1.2.1: Line-by-line accuracy check
**Acceptance Criteria**:
- SKILL.md:30-45 matches `context_audit.py`.
  - *Given* SKILL.md:30-45 and `context_audit.py:125-131,172-190`, *When* each claim is checked, *Then* all hold: last turn only; four usage fields; `None` when no usage; synthetic skipped; estimate is `len//4` cumulative (`estimate_tokens` at :26-29).
- Test line present.
  - *Given* SKILL.md, *When* `grep -n "unittest test_context_audit" $SKILL/SKILL.md`, *Then* line 67 matches.
**Files**: read-only `$SKILL/SKILL.md`, `$SKILL/scripts/context_audit.py`.

##### Task 1.2.1a: Check claims (~5 min)
- Read SKILL.md:28-46 and 64-69; read `context_audit.py:26-29,121-131,170-190`; also check `main()` printed labels (~L306-332) since research did not fully re-read them.
- Note: `actual_total_tokens = sum(usage_breakdown.values())` at `context_audit.py:185` includes `output` tokens. That is consistent with "current context window size" (the turn's output becomes next-turn context), so not an inaccuracy.

##### Task 1.2.1b: Decide D2 (~2 min)
- Recommendation: no SKILL.md edit (AC 5 needs none). If the reviewer wants one line, the candidate is: after "This is the current context window size." add "(sum of all four usage fields, including `output_tokens`.)". Any edit is a dotfiles change requiring its own branch/commit with only `SKILL.md` staged; default is to skip and record the note.

### Epic 1.3: Mutation check
**Goal**: Show the tests fail when behavior regresses, without touching dotfiles.

#### Story 1.3.1: Mutants on a scratch copy
**Acceptance Criteria**:
- Known mutants are killed.
  - *Given* a copy of both files in `$SCRATCH/mut/`, *When* mutant M1 (drop `and not is_synthetic` at ~L130) and M2 (sum usage across turns) are applied in turn and `python3 -m unittest discover -s . -v` is run there, *Then* M1 fails 2 tests (trailing and all-synthetic) and M2 fails 2 (last-turn test and Pi test); baseline copy passes 6.
- Dotfiles unchanged.
  - *Given* the run finished, *When* `git -C /home/tstapler/dotfiles status --short -- .claude/skills/context-audit`, *Then* empty (ignoring `__pycache__` if untracked; check `.gitignore`).
**Files**: scratch only.

##### Task 1.3.1a: Set up scratch copy (~2 min)
- `mkdir -p $SCRATCH/mut && command cp -f $SKILL/scripts/context_audit.py $SKILL/scripts/test_context_audit.py $SCRATCH/mut/ < /dev/null` (`cp` is aliased interactive here; `command cp -f` avoids the hang).
- Run baseline in `$SCRATCH/mut`: expect 6 OK.

##### Task 1.3.1b: Apply M1, M2, and the M6 mutant (~5 min)
- For each mutant: re-copy pristine `context_audit.py`, edit the scratch copy with Edit, run the suite, record fail/ok counts.
- M6 (usage-less assistant turn sets `last_usage = None`): expect the suite to still pass (survivor); this is the evidence for D1.

### Epic 1.4: Optional test for M6 (only if D1 approved)
**Goal**: Kill mutant M6.

#### Story 1.4.1: Add trailing usage-less turn test
**Acceptance Criteria**:
- *Given* a transcript with a real assistant turn (usage input 100, output 50, cache_creation 10, cache_read 40) followed by a trailing assistant line without `usage`, *When* `analyze()` runs, *Then* `actual_total_tokens == 200` and `usage_breakdown["input"] == 100`.
- *Given* the M6 mutant in the scratch copy, *When* the suite runs, *Then* the new test fails; on real code it passes (7 tests OK).
**Files**: `/home/tstapler/dotfiles/.claude/skills/context-audit/scripts/test_context_audit.py` (add one method to `ClaudeCodeUsageTrackingTest`, reuse `_realistic_usage` helper).

##### Task 1.4.1a: Add test on a dotfiles branch (~5 min)
- Precondition: user approved D1. dotfiles `master` has unrelated dirty files, so: `git -C /home/tstapler/dotfiles switch -c test/context-audit-usageless-turn` (branch off HEAD; dirty unrelated files carry over untouched).
- Add the test; run suite (expect `Ran 7 tests ... OK`).
- `git -C /home/tstapler/dotfiles add .claude/skills/context-audit/scripts/test_context_audit.py` (never `add -A`/`add .`), verify `git diff --cached --stat` shows only that file, commit.

##### Task 1.4.1b: Confirm it kills M6 (~3 min)
- Apply M6 in `$SCRATCH/mut` with the new test copied in; expect FAIL. Restore nothing in dotfiles (mutation stays in scratch).

### Epic 1.5: Close out
**Goal**: Record evidence and update the tracker.

#### Story 1.5.1: Evidence and progress
**Acceptance Criteria**:
- *Given* all outputs above, *When* evidence is summarized in `project_plans/context-audit-real-usage/implementation/` (or the PR/report text) with commands and outputs, *Then* each of AC 0-6 has a command, output, and status.
- *Given* verification passes, *When* `report_progress` is called with `criteria_index` 0..6 (0-indexed, one per AC), *Then* each shows done; AC 0 and 4 carry the D0 deviation note.
**Files**: this repo only (docs/evidence).

##### Task 1.5.1a: Report progress (~3 min)
- Call `report_progress` for indices 0-6 with the evidence line per criterion. Do not cite cargo/Rust CI as evidence.

## Adversarial-review resolutions (CONCERNS, 0 blockers)

Source: `adversarial-review.md`. Folded in before implementation:

- Task count is **9** (7 required, 2 gated on D1), not 8.
- AC 6 gate command: use `git diff --stat main...HEAD -- '*.rs'` (three dots; expected empty). The two-dot form lists ~52 unrelated files.
- Run every test/mutation command with `PYTHONDONTWRITEBYTECODE=1` so nothing is written under `~/dotfiles` (`__pycache__`).
- Mutation predictions (e.g. M2 failing-test counts) are unverified; record observed results, not predicted ones.
- D1, if approved: use a separate `git worktree` of dotfiles, not `switch -c` in the dirty checkout; stage only `test_context_audit.py`.
- AC 4 cannot pass literally (no `scripts/tests/`); it rests on D0 and is reported as "met in substance".
- AC 5: record a per-claim table (SKILL.md claim -> code line -> match?) in the evidence file. Keep one evidence file: `implementation/verification.md`.
