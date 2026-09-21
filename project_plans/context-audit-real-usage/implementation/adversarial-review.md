# Adversarial Review: context-audit-real-usage

**Date**: 2026-09-20
**Verdict**: CONCERNS

Checked against real files in `/home/tstapler/dotfiles` (read-only). Verified: `4a5ec3d` is an ancestor of HEAD (master); `discover -s scripts` prints `Ran 6 tests` / `OK`, exit 0; test names and lines match the plan (:93 last-turn, :106 trailing synthetic, :116 all-synthetic, :126 no-usage, :164 migration; Pi test at :41); `context_audit.py` :26-29 (`estimate_tokens`), :125-131 (synthetic skip), :172-190 (usage_breakdown / sum) and SKILL.md:30-45 and :67 all match the plan's claims; `git status` on the skill dir is empty and `__pycache__` is gitignored (.gitignore:110).

## Blockers
None.

## Concerns
- [ ] **D1 scope (edit to ~/dotfiles).** The worktree is a different repo (Rust proxy); the item's target repo is dotfiles (requirements.md "Target repo"), and AC 6 only forbids production-logic changes, so a test-only commit is not an AC violation. But no AC requires it, dotfiles `master` is dirty with unrelated files, and `git switch -c` in a shared dirty tree can mix work. The plan handles this correctly by defaulting to "do not touch until approved" and staging a single file. Recommendation: keep D1 default-off; if approved, prefer a `git worktree add` on a new branch rather than `switch -c` in the dirty checkout. Move the D1 approval into the coordinator hand-off, not a mid-run question, so Epic 1.4 does not stall the run.
- [ ] **AC 6 command is wrong as written.** Story 1.1.2 uses `git diff --stat main -- '*.rs'` and expects "no changes to main.rs". Run here it lists 52 files (main has advanced past this branch), and `main.rs` matches 2 lines (`src/main.rs`, `src/bin/*/main.rs`) as noise. Use the three-dot form `git diff --stat main...HEAD -- '*.rs'` (verified: empty), and name `src/bin/cmdcrush/main.rs` explicitly. Also the plan should say the PostCompact hook check is via dotfiles `git log`, not this repo.
- [ ] **Task count/numbering inconsistent.** The plan has 9 tasks (1.1.1a, 1.1.2a, 1.2.1a, 1.2.1b, 1.3.1a, 1.3.1b, 1.4.1a, 1.4.1b, 1.5.1a); 7 unconditional, 2 gated on D1. The planner's "8 tasks" matches neither. Cross-references are also off: D1 says it blocks "Story 1.4" and D2 lives in "Story 1.3", but D2 is Task 1.2.1b and the dependency diagram uses 1.1-1.5 as if they were tasks. Recommendation: state "9 tasks (7 required + 2 conditional on D1)" and fix the labels.
- [ ] **Mutation procedure safety (mostly OK).** It copies both files into `$SCRATCH/mut` and edits only copies, so dotfiles is not touched, and the post-run `git status` check is present. Gaps: (1) running `python3 -m unittest` in `$SKILL/scripts` for Story 1.1.2 creates `__pycache__` in dotfiles (gitignored, harmless, but not "read-only" as claimed; set `PYTHONDONTWRITEBYTECODE=1`); (2) Task 1.4.1b says "copy the new test into scratch", so the scratch copy of the test file must be refreshed from the dotfiles branch and the scratch context_audit.py must be re-pristined before applying M6; (3) the M2 prediction "fails last-turn test and Pi test" is asserted, not verified. Record actual results and do not treat the predicted counts as pass conditions.
- [ ] **AC mapping: AC 5 and AC 4 pass conditions are soft.** AC 5's "all claims hold" has no enumerated checklist result format, and Task 1.2.1a defers checking `main()` printed labels (~L306-332) to the implementer. AC 4's literal path (`scripts/tests`, "5 tests") cannot pass; D0 records the deviation, which is reasonable, but the tracker call must actually carry the deviation text (Story 1.5.1 does say so). Recommendation: add an explicit per-claim table for AC 5, and confirm the L306-332 labels during the run.
- [ ] **Evidence location undecided.** Story 1.5.1 says the evidence goes to "implementation/ (or the PR/report text)". Pick one (`implementation/evidence.md`) so AC 0-6 rows are checkable.

## Minors
- AC 0-6 all map to a task: AC 0 to 3 via 1.1.2a, AC 4 via 1.1.1a/1.1.2a, AC 5 via 1.2.1a, AC 6 via 1.1.2a, tracker via 1.5.1a. Only AC 4 is a documented deviation.
- The Pi test at :41 is pre-existing, and "5 + 1 = 6" holds (verified by test listing).
- The `command cp -f ... < /dev/null` workaround is fine; the Edit/Read tools plus a scratch copy would avoid the alias entirely.
- The plan cites research line numbers (:106, :116) and says to adjust if they differ; they match today, but dotfiles master is dirty and moving, so grep names first (the plan already says so).
- Untracked `project_plans/context-audit-real-usage/implementation/` in this worktree is only docs; consistent with "this repo gets docs/evidence only".
