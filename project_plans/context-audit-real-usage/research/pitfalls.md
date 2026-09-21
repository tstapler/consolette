# Pitfalls: context-audit real-usage regression tests

Builds on `project_plans/context-audit-real-tokens/research/pitfalls.md` (read first; its
c2ed9ce analysis and ALTER-swallow analysis still hold and are not repeated).
Test file: `~/dotfiles/.claude/skills/context-audit/scripts/test_context_audit.py` (192 lines).
Production: `context_audit.py` `analyze()` L66-; usage gate at L121-131; ALTER at L270-271.

## 1. Do the tests actually fail on regression? (VERIFIED by mutation, on a scratch copy)

Copied both files to the session scratchpad and mutated `context_audit.py` there; nothing under
`~/dotfiles` was modified (`git status` of the skill dir is clean). Baseline: 6 tests OK.

| Mutant | Result | Killed by |
|---|---|---|
| M1 drop `and not is_synthetic` (L130; the c2ed9ce bug) | FAIL x2 | test_context_audit.py:106 (trailing), :116 (all-synthetic) |
| M2 sum usage across turns | FAIL x2 | :93 and Pi test :41 |
| M3 first usage wins | FAIL x1 | :93 |
| M4 ALTER `except` removed (non-idempotent) | ERROR x1 | :164 (second `store_sqlite` call at :179 is what catches it) |
| M5 synthetic line resets `last_usage=None` | FAIL x1 | :106 |
| **M6 usage-less assistant turn clears `last_usage`** | **SURVIVES** | nothing |

Conclusion: the c2ed9ce regression and last-wins/sum regressions are each caught. Trailing
synthetic is last in file order (L108-109), so the fixture is positioned correctly.

## 2. Weak spots / gaps (file:line)

- **M6 survivor / untested ordering.** `test_no_usage_data_yields_none` (:126-131) is a single
  usage-less line, so it can't distinguish "no usage anywhere" from "usage-less turn after a
  real one". No test has real-usage then usage-less trailing assistant (a real transcript
  shape: partial/streaming lines). Current code keeps the prior usage (L130 guard); a refactor
  to "last assistant line's usage" would pass all 6 tests.
- **Synthetic-in-the-middle not covered.** Only trailing (:109) and sole (:118). Fine for the
  original bug (only trailing manifests it, per prior research), but a mid-position case
  guards against an "abort on first synthetic" style edit. Low value; optional.
- **Synthetic line without `usage`** not covered; and a synthetic line's `model` check is only
  ever exercised with an exact `"<synthetic>"` string.
- **Fixture shape.** `_realistic_usage` (:11-22) adds `service_tier` and `server_tool_use` but
  not the nested `cache_creation` dict with `ephemeral_*` keys that real transcripts carry
  (prior research section 3). Mutation "read nested shape" would not be caught. Cheap to add
  one nested key to the helper.
- **Migration test asserts only one column** (:182-187: `SELECT actual_total_tokens`). It proves
  the ALTER ran and the value round-trips, but not that the other 11 columns were written
  correctly, nor the fresh-DB path (CREATE already containing the column). The legacy DDL is a
  hand-copied schema (:142-155): if production's `CREATE TABLE` gains/renames a column, this
  copy drifts silently and the test still passes against a stale schema. Comment at :136
  ("Pre-c2ed9ce schema") is also a slightly wrong label: the column arrived in c2ed9ce-era
  or f83b7b5 work; unverified which, so don't cite it.
- **Docstring/comment truthiness.** :136 says legacy schema mirrors "every column except
  actual_total_tokens"; verify against the CREATE in `context_audit.py` if editing.
- **Known-unfixed crash** (prior research): `usage` value of `null` (e.g. `cache_read_input_tokens: None`)
  makes the breakdown sum raise TypeError. Out of scope (AC 6 forbids production change); no
  test documents it. Do not add a test asserting the crash unless deliberately pinning it.
- **Sqlite lock-contention conflation** (bare `except OperationalError`, L271) remains
  untestable cheaply; leave as noted in prior research.

## 3. Risks from the acceptance-criteria mismatches

1. **tests/ vs scripts/ (AC 0, 4).** Literal `discover -s .../scripts/tests` fails: no such dir
   (`ls scripts` = `context_audit.py`, `test_context_audit.py`, `__pycache__`). Moving the file
   breaks `from context_audit import ...` (:8) unless `sys.path` is patched or `scripts/` is
   made importable, and it would break the documented command at `SKILL.md:67`
   (`python3 -m unittest test_context_audit -v`, "run from `scripts/`"). Also
   `discover -s scripts` from the skill dir works today (VERIFIED: "Ran 6 tests ... OK").
   Design against: rewriting the AC's command in a verifier so it silently passes (e.g.
   `discover -s scripts/tests` printing "Ran 0 tests" and exit 0 on some Python versions is
   NOT the case for a missing dir, which errors, but an empty existing `tests/` dir would
   report 0 tests, OK: never create an empty `tests/` to satisfy the path). Record the deviation
   explicitly (default option b) and quote the real command + output.
2. **5 vs 6 tests (AC 4).** The 6th is the pre-existing Pi test (:41). Verifier must assert
   "the 5 named behaviors are covered and total passes", not `Ran 5`; a literal count check
   fails, and deleting/deselecting the Pi test to hit 5 would be wrong. Mapping: AC0 -> :93;
   AC1 -> :106,:116; AC2 -> :126; AC3 -> :164 (that is 5 regression tests + Pi = 6).
3. **Target repo is ~/dotfiles, worktree is consolette.** Risks: a commit/PR in the wrong repo;
   `git`/`unittest` run from the worktree cwd finding nothing; the worktree CLAUDE.md
   (Rust/cargo) checks (`cargo test`) proving nothing about the Python tests. Nothing to
   change is expected since tests already landed (`4a5ec3d`, on `master`, VERIFIED via
   `git branch --contains`). Design against: any edit to dotfiles from this run (this task
   forbids it); claiming "done" from consolette CI. Deliverable here should be a
   verification record (planning artifacts) in the worktree, with evidence commands run against
   the dotfiles path.
4. **Dotfiles tree dirty on master.** VERIFIED `git status --short`: modified `.config/fish/config.fish`,
   `.zprofile`, `.zshrc`, three `project_plans/pi-dotfiles/*` files; untracked
   `.claude/skills/synced/` and two pi-dotfiles docs. None are in `.claude/skills/context-audit/`
   (clean). HEAD is `a5bc60e`, 4a5ec3d is an ancestor. Risks if anyone does edit there: `git add -A`
   sweeping the user's unrelated work (CLAUDE.md forbids), commits on `master` directly, and
   `.claude/skills/synced/` (untracked) confusing skill discovery. Any change must be
   path-scoped staging on a branch; better, none.
5. **Runtime pitfalls when re-verifying.** `__pycache__/` exists in the scripts dir (untracked?
   check .gitignore before believing "clean"); running unittest from the wrong cwd gives
   `ModuleNotFoundError: context_audit` (import at :8 depends on cwd/discover's `-s` path
   insertion). `cp` is aliased interactive in this shell and hangs on overwrite: use
   `command cp -f` with stdin closed in scripted mutation runs (hit this once).
6. **AC 5 accuracy re-check.** `SKILL.md:45` states the actual total vs estimate are "not
   directly comparable"; `:67` gives the test command. Confirm text still matches
   `main()`'s printed labels (`context_audit.py` ~L306-332) before ticking it; not re-read in
   full here (UNVERIFIED beyond the grep).
7. **AC 6 (no production change).** Gate with `git -C ~/dotfiles diff --stat -- .claude/skills/context-audit`
   empty plus `git log 4a5ec3d..HEAD -- .claude/skills/context-audit/scripts/context_audit.py`; mutation
   experiments must stay in scratch copies (as done here).

## 4. Explicitly design against

- Editing or "fixing" anything under `~/dotfiles`; running mutants in place.
- Moving the test file to satisfy AC literalism without updating SKILL.md:67 and imports.
- Reporting `Ran 5`; empty `tests/` dir; skipping the Pi test.
- Tests that only assert "does not raise" (the c2ed9ce bug raised nothing).
- Adding scope: null-usage crash fix, lock-contention test, malformed-JSONL test.
- Cargo/Rust CI results being cited as evidence for a Python skill in another repo.
- Optional, only if a follow-up edit is approved: add (a) real-then-usage-less trailing turn
  test (kills M6), (b) nested `cache_creation` in `_realistic_usage`, (c) assert full row in
  migration test or derive legacy DDL from production.
