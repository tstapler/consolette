# Validation Plan: context-audit-real-tokens

**Date**: 2026-09-04

## Happy Path Scenario

Given the Baseline (`context_audit.py` already computes and reports both real
(`actual_total_tokens`) and estimated (`total_estimated_tokens`) token counts correctly,
but has zero automated tests protecting that logic), when a maintainer runs
`python3 -m unittest discover -s .claude/skills/context-audit/scripts/tests` against a
fixture transcript containing two non-synthetic assistant turns with distinct `usage`
values, then the suite passes and asserts `report["actual_total_tokens"]` equals the
*second* turn's usage sum (not the sum of both turns) — proving the regression-test
safety net for the `c2ed9ce` bug class actually exists and catches it.

## Requirement → Test Mapping

All file paths are relative to `~/dotfiles`, per `implementation/plan.md`'s
"Implementation Location" note. Source under test: `.claude/skills/context-audit/scripts/context_audit.py`
(`analyze()`, `store_sqlite()`). New test file:
`.claude/skills/context-audit/scripts/tests/test_context_audit.py`.

Tests marked **Required** correspond 1:1 to a task already named in `plan.md`
(Tasks 1.1.1b–1.1.1e) and to one of requirements.md's four in-scope Scope bullets.
Tests marked **Recommended** are additional error-path coverage this validation pass
identified by reading `context_audit.py`'s defensive branches (`.get()` defaults,
the `except json.JSONDecodeError: continue` in `load()`) — they are not in plan.md's
task list. Per requirements.md's Rabbit Holes ("resist the urge to add a full suite...
that's scope creep") and plan.md's explicit rejection of a 5th speculative test case,
these are optional follow-ups for the implementer to include or defer, not blocking
acceptance criteria.

| Requirement | Test File | Test Name | Type | Priority | Scenario |
|-------------|-----------|-----------|------|----------|----------|
| Real-usage total = last non-synthetic turn, not sum | `test_context_audit.py` | `test_should_useLastTurnTotal_when_multipleNonSyntheticAssistantTurnsPresent` | Unit (happy) | Required (= Task 1.1.1b) | Two non-synthetic assistant lines with `usage` summing to 120 and 850; `analyze(path)["actual_total_tokens"] == 850`, not `1420`. |
| Real-usage total = last non-synthetic turn, not sum | `test_context_audit.py` | `test_should_defaultMissingUsageFields_when_usageDictIsPartial` | Unit (error/edge) | Recommended | A `usage` dict missing one of the four read keys (e.g. no `cache_creation_input_tokens`); `analyze()` defaults that field to `0` via `.get(key, 0)` rather than raising `KeyError`. |
| Synthetic assistant lines excluded from usage | `test_context_audit.py` | `test_should_excludeSyntheticTurn_when_trailingSyntheticFollowsRealTurn` | Unit (happy) | Required (= Task 1.1.1c) | Real assistant turn (sum 850) followed by a `message.model == "<synthetic>"` line with `usage` summing to 9999; `actual_total_tokens == 850`. |
| Synthetic assistant lines excluded from usage | `test_context_audit.py` | `test_should_returnNoneUsage_when_allAssistantTurnsAreSynthetic` | Unit (error/edge) | Required (= Task 1.1.1c) | Every assistant line has `message.model == "<synthetic>"`; `actual_total_tokens is None` and `usage_breakdown is None`. |
| No usage data anywhere → graceful `None` | `test_context_audit.py` | `test_should_returnNoneGracefully_when_transcriptHasNoUsageData` | Unit (happy) | Required (= Task 1.1.1d) | `user`/`assistant` lines whose `message` dict has no `usage` key at all; `actual_total_tokens is None`, `usage_breakdown is None`, no exception. |
| No usage data anywhere → graceful `None` | `test_context_audit.py` | `test_should_skipMalformedJsonLine_when_transcriptContainsInvalidJson` | Unit (error path) | Recommended | Fixture includes one line that is not valid JSON; `load()`'s `except json.JSONDecodeError: continue` swallows it and `analyze()` still returns a valid report from the remaining lines, no exception propagates. |
| `store_sqlite()` `ADD COLUMN` migration is idempotent against a pre-existing table lacking the column | `test_context_audit.py` | `test_should_insertRowWithActualTokens_when_databaseIsFresh` | Integration (happy) | Recommended | Fresh tempfile-backed sqlite DB (no `compactions` table yet); `store_sqlite()`'s `CREATE TABLE IF NOT EXISTS` path already includes `actual_total_tokens`; insert + read-back round-trips the value. |
| `store_sqlite()` `ADD COLUMN` migration is idempotent against a pre-existing table lacking the column | `test_context_audit.py` | `test_should_insertNullActualTokens_when_reportLacksActualTotalTokensKey` | Integration (error/edge) | Recommended | `report` dict has no `"actual_total_tokens"` key at all; `store_sqlite()` uses `report.get("actual_total_tokens")` → `None` → stored as SQL `NULL` in the nullable `INTEGER` column, no exception. |
| `store_sqlite()` `ADD COLUMN` migration is idempotent against a pre-existing table lacking the column | `test_context_audit.py` | `test_should_beIdempotent_when_columnAlreadyExistsOnLegacyTable` | **Migration** | Required (= Task 1.1.1e) | Pre-create a `compactions` table via a hand-copied *pre-migration* schema (all columns from `context_audit.py`'s `CREATE TABLE`, `context_audit.py:229-244`, except `actual_total_tokens` — simulating an `f83b7b5`-era DB). Call `store_sqlite()` twice against the same on-disk tempfile DB. Assert neither call raises, the column exists after the first call, and both rows read back with correct `actual_total_tokens` values. |

**On the Migration row**: this is *not* a reversible up/down schema migration — there is
no down-migration, no migration-versioning table, and nothing to roll back. It is a
one-directional, additive, backward-compatible `ALTER TABLE ... ADD COLUMN`, guarded by
`try/except sqlite3.OperationalError: pass` so a second (or Nth) call against a DB that
already has the column is a silent no-op. The test's job is narrower than a real
migration test: prove that guard makes repeated calls safe against both an old-schema
DB (pre-`actual_total_tokens`) and a new-schema DB, not that data can be migrated
backward.

Requirement coverage: all 4 in-scope Scope bullets from `requirements.md` (last-usage-
wins, synthetic-skip, no-usage-data-None, migration-idempotency) have at least one
Required test — **4/4 (100%)**.

### Docs audit (not a test)

`requirements.md`'s Scope also lists "Confirming (not re-writing) that CLI/JSON output
and `SKILL.md` still accurately distinguish real vs. estimated tokens." This is a
read-only diff-against-code check, not an automated test — no fixture/assertion applies
to prose. It's already covered by `plan.md` Tasks 1.2.1a (re-verify `SKILL.md:30-45`
against `context_audit.py:106-112,153-162,306-312`) and 1.2.1b (add a `Tests:` line
pointing at the new suite). No corresponding row in the table above; do not invent one.

## Test Stack

- **Unit**: `unittest.TestCase`, stdlib only, one class (`TestAnalyzeUsageTracking`) in
  `.claude/skills/context-audit/scripts/tests/test_context_audit.py`, matching the
  existing `golang-profiling/scripts/tests/test_pct_breakdown.py` precedent exactly
  (import-path-insert idiom, one `test_*` method per case, `unittest.main()` at the
  bottom). Fixtures are hand-built JSONL written to a `tempfile.TemporaryDirectory()`
  per test (`setUp`/`tearDown`), using a realistic 9-key `usage` dict (not a minimal
  4-key one) so tests exercise "ignores the extra/nested fields correctly," not just
  the four keys `analyze()` reads. No mocking — `analyze()` and `store_sqlite()` are
  pure enough (file-path in, dict out; file-path + dict in, DB rows out) that fixture
  files stand in for mocks.
- **Integration**: same `unittest.TestCase` class and file — no separate suite/runner.
  Any test that calls `store_sqlite()` is integration-classed per the task brief's own
  definition ("`store_sqlite()`'s migration test counts as integration since it touches
  a real sqlite file"), using an on-disk `tempfile`-backed DB path, not `:memory:`, so
  the same DDL-migration path used against the real
  `~/.claude/context-audit/trend.db` is exercised.
- **E2E / UX**: N/A — no user-facing surface (Complexity 1, pure infrastructure per
  task brief; skipped entirely per instruction).

## Coverage Targets and How to Measure

| Stack | Coverage command | Target |
|---|---|---|
| Unit + Integration (Python/`unittest`) | `python3 -m unittest discover -s .claude/skills/context-audit/scripts/tests` | No formal coverage-percentage gate. This repo has no `pytest`/`coverage.py` infrastructure anywhere (confirmed in `research/stack.md`/`research/build-vs-buy.md`), and the one existing precedent for a stdlib-only `.claude/skills/*/scripts/tests/` suite — `golang-profiling/scripts/tests/` — also has no coverage threshold and is not wired into `~/dotfiles/.github/workflows/ci.yml`. This suite matches that precedent: "all tests pass" (`OK`, `Ran N tests`) is the bar, not a %. Enforcement is manual (`plan.md`'s Epic 1.1 "Accepted gap": run the command before shipping a `context_audit.py` change), not CI-gated. |

- All public service methods: `analyze()` and `store_sqlite()` — the only two functions
  with the real-usage logic in scope — each have a happy-path and an error/edge-path
  test above.
- All external integrations: `store_sqlite()`'s sqlite calls are exercised by unit-style
  fixture tests plus the dedicated migration test above; there is no other external
  integration (`analyze()` only reads a local file, no network/secrets per
  requirements.md's Security classification).
