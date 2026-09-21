# Implementation Plan: context-audit-real-tokens

**Feature**: Regression test coverage for `context_audit.py`'s already-shipped real-usage
token tracking (`analyze()`'s last-usage-wins/synthetic-skip logic) and `store_sqlite()`'s
`ADD COLUMN` migration.
**Date**: 2026-09-04
**Status**: Ready for implementation
**ADRs**: None — `unittest` is stdlib, zero-dependency, and already precedented in this repo;
no non-standard technology choice requires a stub.

---

## Implementation Location

**All file paths in this plan are relative to `~/dotfiles` (the `tstapler/dotfiles`
repo) — NOT the `tstapler/consolette` repo this SDD planning session ran in.** There is
no submodule, symlink, or other connection between the two repos. Phase 5
(`/sdd:5-implement`) for this plan must be run from a **fresh session with `~/dotfiles`
as the working directory/repo** (e.g. `cd ~/dotfiles` or a dedicated worktree of it) —
not from this consolette worktree — or edits will either land in the wrong repo's
working tree or fail to resolve at all. See `adversarial-review.md`'s Blocker for the
full verification behind this.

---

## Domain Glossary

N/A — no new domain types, this is test coverage for an existing feature. The only new
identifiers this plan introduces are test method names and one local fixture helper
(`_write_jsonl`, `_realistic_usage`), which are implementation detail, not domain
vocabulary — see Task descriptions below for their exact names.

---

## Pattern Decisions

**Step 0.5 creative pass — three test-structure approaches considered:**

1. **`unittest.TestCase` in a `scripts/tests/` subdirectory**, mirroring
   `~/dotfiles/.claude/skills/golang-profiling/scripts/tests/test_pct_breakdown.py`
   exactly (class-per-module, one `test_*` method per behavior, `python3 -m unittest
   discover`). Strength: matches the one existing in-repo precedent for stdlib-only
   Python skill scripts, so a maintainer scanning `.claude/skills/*/scripts/tests/`
   later finds one consistent style, not two competing ones. Weakness: a `TestCase`
   class plus five small methods is more boilerplate than four bare `assert`
   statements would need.
2. **Plain `assert`-based `demo()`/`__main__` self-check** in a flat
   `test_context_audit.py` next to the script (build-vs-buy.md's recommendation).
   Strength: minimal ceremony — matches ponytail's leanest sanctioned bar for "one
   runnable check." Weakness: a bare `AssertionError` traceback gives no case name on
   failure (vs. `unittest`'s `FAIL: test_trailing_synthetic_turn_excluded_from_usage`),
   and it would introduce a *second*, different self-check style into the same
   `skills/*/scripts/` tree that golang-profiling already established a pattern for —
   inconsistency cost without a corresponding capability gain.
3. **pytest with fixtures/parametrize.** Strength: nicest ergonomics (fixtures,
   parametrize, rich assertion introspection). Weakness: confirmed by research
   (`research/stack.md`, `research/build-vs-buy.md`) that no pytest infra exists
   anywhere in `~/dotfiles`, `pytest` on `PATH` binds to a *different* Python
   interpreter than the linuxbrew `python3` that actually runs `context_audit.py`,
   and requirements.md's "no new runtime dependencies" constraint rules it out
   outright. Already rejected in requirements.md's own Alternatives Considered.

**Chosen: option 1, `unittest.TestCase`.** Both stdlib options (1 and 2) satisfy every
hard constraint (zero new dependency, ponytail's "one runnable check" bar,
requirements.md's stdlib-only line). The tie-break the task brief asks for — conform to
existing precedent vs. minimize ceremony for 4 assertions — resolves in favor of
precedent here for two additional reasons beyond "matches the sibling skill": (a) test 4
(sqlite migration) needs a tempfile-backed DB per test with cleanup, which `unittest`'s
per-method isolation and `tearDown` handle more cleanly than a single flat `demo()`
would with manual `try/finally`; (b) `unittest`'s named-failure output is meaningfully
more useful than a bare `AssertionError` specifically for *this* test's purpose —
pinning down a silent-wrong-number regression like `c2ed9ce`, where knowing *which*
named case failed (trailing-synthetic vs. all-synthetic vs. last-wins) is the whole
point.

| Component | Pattern Chosen | Source | Alternative Rejected | Reason |
|---|---|---|---|---|
| Test structure | `unittest.TestCase`, one class, one `test_*` method per case, in `scripts/tests/` | `research/stack.md`, matches `golang-profiling/scripts/tests/test_pct_breakdown.py` | Plain `assert`-based `demo()`/`__main__` (`research/build-vs-buy.md`) | Zero added dependency cost either way (both stdlib); `unittest` avoids introducing a second self-check style into the same `skills/*/scripts/` tree and gives named-failure output + per-test tempfile isolation, both directly useful for pinning a `c2ed9ce`-class regression |
| Test structure | (same) | — | pytest + fixtures | No pytest infra anywhere in `~/dotfiles`; `pytest` on `PATH` binds to a different interpreter than the `python3` that runs the script; violates "no new runtime dependencies" |
| Fixture data | Hand-built JSONL written to a `tempfile`-backed path per test, using a *realistic* 9-key `usage` dict (4 real fields + 5 extra fields matching pitfalls.md's captured sample, not a minimal 4-key one) for every case | `research/pitfalls.md` §4 | Minimal 4-key `usage` dict for every fixture | A 4-key-only fixture would never exercise "does the code correctly ignore the nested/newer `cache_creation` shape," and wouldn't catch a future edit that reads the nested shape instead of the flat one — the exact field-shape gap pitfalls.md flags as the real risk |
| sqlite migration test | Pre-create a `compactions` table *without* `actual_total_tokens`, call `store_sqlite()` twice against the same on-disk (not `:memory:`) tempfile DB | `research/pitfalls.md` §2 | `:memory:` DB | `:memory:` doesn't exercise the same DDL-migration path against a durable file the way the real `~/.claude/context-audit/trend.db` is used, and the marginal cost of a `tempfile.NamedTemporaryFile` is negligible |
| 5th case: null-valued `usage` field (documented-crash) | Excluded from this plan | `research/pitfalls.md` §3 (flagged as optional) | Adding a 5th test asserting the current `TypeError` crash on `{"cache_read_input_tokens": null, ...}` | pitfalls.md itself calls this speculative (no observed real transcript has a null value) and explicitly not one of the 4 scoped items in requirements.md; requirements.md's Rabbit Holes section and the user's "nothing more, nothing less" instruction both argue against adding scope beyond the 4 named cases — noted here as a legitimate follow-up, not silently dropped |

---

## Migration Plan

N/A — no schema or data changes. `store_sqlite()`'s existing `ADD COLUMN` migration
logic is being *tested*, not modified or extended.

## Observability Plan

N/A per requirements.md (complexity 1, offline analysis script, no production
observability surface).

## Risk Control

N/A per requirements.md (complexity 1, test-only change, no feature flag or rollback
needed — the change set is additive test files plus one documentation line).

## Unresolved Questions

The `unittest` vs. `assert`-based framework disagreement between `research/stack.md`
and `research/build-vs-buy.md` is resolved above (Pattern Decisions). The optional 5th
"null-valued usage field" test case from `research/pitfalls.md` is explicitly excluded
from this plan's scope (see Pattern Decisions table) rather than left ambiguous.

- **How does this `project_plans/context-audit-real-tokens/` tree itself get to
  `~/dotfiles`?** This plan's own artifacts (including this file) live in the
  `tstapler/consolette` worktree, not `~/dotfiles`. The Implementation Location note
  above says Phase 5 must run from a `~/dotfiles` session, but doesn't specify whether
  the implementer copies this `project_plans/context-audit-real-tokens/` directory into
  `~/dotfiles` first, or whether `/sdd:5-implement` is pointed at this plan file via an
  explicit path from a `~/dotfiles`-rooted session. Left for whoever kicks off Phase 5
  to resolve pragmatically (most likely: copy the directory over) — not a blocker to
  writing the plan, but worth deciding before running `/sdd:5-implement`.

## Dependency Visualization

```
Epic 1.1: Test coverage
│
├─ Story 1.1.1: analyze() + store_sqlite() regression tests
│   Task 1.1.1a (skeleton + fixture helpers)
│        │
│        ▼
│   Task 1.1.1b (last-usage-wins test)
│        │
│        ▼
│   Task 1.1.1c (synthetic-skip tests: trailing + all-synthetic)
│        │
│        ▼
│   Task 1.1.1d (no-usage-data → None test)
│        │
│        ▼
│   Task 1.1.1e (store_sqlite migration idempotency test)
│        │
│        ▼
│   Task 1.1.1f (run suite, confirm all green)
│
└─ Story 1.2.1: docs audit + optional run-command doc line
    Task 1.2.1a (confirm SKILL.md real/estimated section needs no change)
         │
         ▼
    Task 1.2.1b (add one-line test-run doc mention to SKILL.md)
```

All of Story 1.1.1's tasks edit the same new file sequentially (each appends a method to
the class started in 1.1.1a), so they're a straight chain. Story 1.2.1 is independent of
Story 1.1.1 (different file) and could run in parallel, but is sequenced after it here
since 1.2.1b's doc line references the `tests/` directory 1.1.1a creates.

---

## Phase 1: Close the test-coverage gap on already-shipped real-usage tracking

### Epic 1.1: Regression tests for `analyze()` usage-tracking and `store_sqlite()` migration
**Goal**: Leave one runnable, named-failure-output check behind that fails if a future
edit to `context_audit.py` reintroduces the `c2ed9ce` bug class (synthetic-line
contamination of "last usage wins") or breaks the sqlite migration's idempotency —
without touching any of the already-correct production logic. **Accepted gap**:
`~/dotfiles/.github/workflows/ci.yml`'s `test` job does not discover or run this suite
(it's hardcoded to one unrelated directory) — enforcement is manual-only
(`python3 -m unittest discover -s scripts/tests` before shipping a `context_audit.py`
change), matching the existing `golang-profiling` precedent, which is also unrun in CI.
Wiring CI discovery is out of scope for this Small-appetite task.

#### Story 1.1.1: Automated tests for real-usage tracking and sqlite migration
**As a** maintainer editing `context_audit.py` in the future, **I want** an automated
test that fails loudly on a `c2ed9ce`-class regression, **so that** a silent
wrong-number bug can't ship again undetected.

**Acceptance Criteria**:
- Real-usage total reflects the *last* non-synthetic assistant turn's `usage`, not a
  sum across turns.
  - *Given* a fixture JSONL with two non-synthetic assistant lines — the first with
    `usage = {input_tokens: 100, output_tokens: 20, cache_creation_input_tokens: 0,
    cache_read_input_tokens: 0, ...extra realistic fields}` and the second with
    `usage = {input_tokens: 300, output_tokens: 40, cache_creation_input_tokens: 500,
    cache_read_input_tokens: 10, ...extra realistic fields}` — *when* `analyze(path)` is
    called, *then* `report["actual_total_tokens"] == 850` (300+40+500+10, the second
    turn's sum) and not `1420` (the sum of both turns' totals: 120 + 850).
- Synthetic assistant lines (`message.model == "<synthetic>"`) are excluded from
  "last usage wins," in both the trailing-synthetic and all-synthetic-transcript shapes.
  - *Given* a fixture with one real assistant turn (`usage` summing to 850, as above,
    `message.model` absent/non-synthetic) followed by a trailing assistant line with
    `message.model == "<synthetic>"` and its own distinct `usage` (e.g. summing to
    `9999`), *when* `analyze(path)` is called, *then*
    `report["actual_total_tokens"] == 850`, not `9999` — proving the synthetic turn's
    usage was skipped rather than overwriting `last_usage`.
  - *Given* a fixture where **every** assistant line has `message.model ==
    "<synthetic>"` (no real assistant turn at all), *when* `analyze(path)` is called,
    *then* `report["actual_total_tokens"] is None` and `report["usage_breakdown"] is
    None` — the purest form of the `c2ed9ce` regression and the cheapest fixture to
    construct.
- A transcript with no `usage` data anywhere yields a graceful `None`, not an exception
  or a wrong number.
  - *Given* a fixture with `user`/`assistant` lines whose `message` dicts have no
    `usage` key at all (e.g. an older-format transcript), *when* `analyze(path)` is
    called, *then* `report["actual_total_tokens"] is None` and
    `report["usage_breakdown"] is None`, and no exception is raised.
- `store_sqlite()`'s `ALTER TABLE ... ADD COLUMN actual_total_tokens` migration is
  idempotent against a pre-existing `compactions` table that lacks the column.
  - *Given* a tempfile-backed SQLite DB pre-seeded with a `compactions` table created
    via the *pre-migration* schema (all columns from `store_sqlite()`'s `CREATE TABLE`
    except `actual_total_tokens`, simulating an `f83b7b5`-era DB), *when*
    `store_sqlite(report, db_path, "sess-1", "manual", "/fake/path")` is called twice in
    a row against that same DB, *then* neither call raises, the `compactions` table has
    an `actual_total_tokens` column after the first call, and both inserted rows are
    readable back with their `actual_total_tokens` values intact.

**Files**: `.claude/skills/context-audit/scripts/tests/test_context_audit.py` (new, relative to `~/dotfiles` — see Implementation Location above)

##### Task 1.1.1a: Test file skeleton + fixture helpers (~4 min)
- Create `.claude/skills/context-audit/scripts/tests/test_context_audit.py`.
- Add the standard import block matching `test_pct_breakdown.py`'s idiom:
  ```python
  import json
  import pathlib
  import sqlite3
  import sys
  import tempfile
  import unittest

  sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent))

  from context_audit import analyze, store_sqlite  # noqa: E402
  ```
- Add a module-level `_realistic_usage(input_tokens, output_tokens,
  cache_creation_input_tokens, cache_read_input_tokens)` helper returning a dict with
  those four keys plus the extra realistic fields observed in a real transcript
  (`server_tool_use`, `service_tier`, `cache_creation` nested dict, `inference_geo`,
  `speed`) per `research/pitfalls.md` §3's captured sample — so fixtures exercise "does
  the code correctly ignore the nested/extra shape," not just the four flat keys it
  reads.
- Add a module-level `_write_jsonl(tmpdir, lines)` helper that writes a list of dicts as
  one-JSON-object-per-line to a `tempfile.NamedTemporaryFile(dir=tmpdir, suffix=".jsonl",
  delete=False)` and returns its path.
- Add an empty `class TestAnalyzeUsageTracking(unittest.TestCase):` with a `setUp` that
  creates a `tempfile.TemporaryDirectory()` (stored as `self.tmpdir`) and a `tearDown`
  that cleans it up.
- Add `if __name__ == "__main__": unittest.main()` at the bottom.
- Files: `scripts/tests/test_context_audit.py`

##### Task 1.1.1b: Last-usage-wins test (~3 min)
- Add `test_actual_total_tokens_uses_last_turn_not_sum` to
  `TestAnalyzeUsageTracking`: build a fixture with two non-synthetic `assistant` lines
  (using `_realistic_usage` for each, sums 120 and 850 respectively as in the
  acceptance criterion), one `user` line for realism, write via `_write_jsonl`, call
  `analyze(path)`, assert `report["actual_total_tokens"] == 850` and
  `report["usage_breakdown"] == {"input": 300, "output": 40, "cache_creation": 500,
  "cache_read": 10}`.
- Files: `scripts/tests/test_context_audit.py`

##### Task 1.1.1c: Synthetic-skip tests — trailing and all-synthetic (~5 min)
- Add `test_trailing_synthetic_turn_excluded_from_usage`: fixture = one real assistant
  turn (sum 850) followed by one assistant line with `message.model = "<synthetic>"`
  and a distinct `usage` (sum 9999). Assert `report["actual_total_tokens"] == 850`.
- Add `test_all_synthetic_transcript_yields_none`: fixture = every assistant line has
  `message.model = "<synthetic>"`. Assert `report["actual_total_tokens"] is None` and
  `report["usage_breakdown"] is None`.
- Files: `scripts/tests/test_context_audit.py`

##### Task 1.1.1d: No-usage-data graceful-None test (~2 min)
- Add `test_no_usage_data_yields_none`: fixture with `user`/`assistant` lines whose
  `message` dict has no `usage` key at all. Assert `report["actual_total_tokens"] is
  None` and `report["usage_breakdown"] is None`, and that calling `analyze(path)`
  raises nothing (implicit — the test itself would raise if it did).
- Files: `scripts/tests/test_context_audit.py`

##### Task 1.1.1e: `store_sqlite()` migration idempotency test (~5 min)
- Add `test_store_sqlite_migrates_legacy_table_idempotently`: create a tempfile DB path
  inside `self.tmpdir`, manually `sqlite3.connect(...)` and `CREATE TABLE compactions`
  with every column from `store_sqlite()`'s schema *except* `actual_total_tokens`
  (copy the column list from `context_audit.py:229-244` minus that one line), commit,
  close. Since this is a hand-copied literal, not derived from the real schema, add a
  code comment on the `CREATE TABLE` in the test pointing back to `store_sqlite()`'s
  `CREATE TABLE` statement (`context_audit.py:229-244`) as the source of truth, so a
  future column change there is more likely to prompt updating this fixture too.
- Build a minimal `report` dict (matching what `analyze()`/`recommend()` would produce:
  `total_estimated_tokens`, `breakdown` dict with the 5 keys, `by_tool`,
  `actual_total_tokens`).
- Call `store_sqlite(report, db_path, "sess-1", "manual", "/fake/path")` once, assert no
  exception; call it a second time, assert no exception either (the idempotent-`ALTER`
  path).
- Re-open the DB, `SELECT actual_total_tokens FROM compactions ORDER BY id`, assert two
  rows come back with the expected value.
- Files: `scripts/tests/test_context_audit.py`

##### Task 1.1.1f: Run the suite and confirm green (~2 min)
- Run:
  ```
  python3 -m unittest discover -s .claude/skills/context-audit/scripts/tests
  ```
- Confirm all 5 tests pass (`OK` with `Ran 5 tests`). This is the proof-before-claiming-done
  step — do not report the story complete without this output.
- Files: none (verification only).

---

### Epic 1.2: Confirm existing docs stay accurate, document how to run the new tests
**Goal**: Verify (not rewrite) that `SKILL.md` and the CLI output already correctly
distinguish real vs. estimated tokens, and add a one-line pointer to the new test suite
matching the `golang-profiling` precedent — cheap, consistent, not required by scope but
recommended by `research/stack.md`.

#### Story 1.2.1: Docs audit + test-run doc line
**As a** future maintainer, **I want** `SKILL.md` to point at the new test suite the way
`golang-profiling`'s `SKILL.md` does, **so that** the tests are discoverable without
grepping for them.
**Acceptance Criteria**:
- `SKILL.md`'s existing "Two totals are reported, on different bases" section (lines
  30-45) is confirmed still accurate against the current `analyze()`/`main()` code —
  no edit needed there.
  - *Given* `SKILL.md:30-45`'s claim that actual tokens come from "the *last* assistant
    turn" and skip `<synthetic>` lines, *when* compared against `context_audit.py:106-112`
    (verified in this same planning pass), *then* the claim matches the code exactly —
    confirmed, no diff required.
- `SKILL.md`'s "Running it" section gains one line documenting how to run the new test
  suite.
  - *Given* `SKILL.md`'s "Running it" section (currently lines 47-65, ending after the
    `--sqlite` flag description), *when* a maintainer reads it after this change, *then*
    it includes a line equivalent to `golang-profiling/SKILL.md:24`'s pattern: `Tests:
    python3 -m unittest discover -s scripts/tests`.

**Files**: `.claude/skills/context-audit/SKILL.md` (relative to `~/dotfiles` — see Implementation Location above)

##### Task 1.2.1a: Confirm SKILL.md needs no correction (~2 min)
- Re-read `SKILL.md:30-45` against `context_audit.py:106-112,153-162,306-312` (already
  done once during this planning pass — this task is the implementation-time
  re-confirmation after the test file lands, in case anything shifted).
- No file edit expected; if a discrepancy is found, it is out of this plan's scope
  (requirements.md: "a docs/output audit, not new copy") — flag it instead of editing.
- Files: none (read-only verification).

##### Task 1.2.1b: Add test-run doc line to SKILL.md (~2 min)
- In `SKILL.md`'s "Running it" section, after the existing step 2 (`--sqlite` flag
  description, ~line 65), add one line: a `Tests:` pointer with the exact command from
  Task 1.1.1f, e.g.:
  ```
  Tests: `python3 -m unittest discover -s ~/.claude/skills/context-audit/scripts/tests`
  ```
- Files: `SKILL.md`
