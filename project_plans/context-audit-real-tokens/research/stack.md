# Research: Stack / Testing Approach — context-audit-real-tokens

## Target file structure (confirmed by reading)

`~/dotfiles/.claude/skills/context-audit/scripts/context_audit.py` (328 lines), stdlib-only:
imports `argparse`, `json`, `sqlite3`, `sys`, `collections.defaultdict` — no third-party deps.

- `analyze(path)` — lines 66-181. Builds `last_usage` by overwriting (not summing) on each
  non-synthetic assistant line (106-112), returns `actual_total_tokens`/`usage_breakdown`
  (`None`/`None` when no `usage` data ever seen, since `last_usage` stays `None` and the
  `if last_usage:` guard at 155 short-circuits).
- `recommend(report)` — lines 184-217, out of scope (unchanged).
- `store_sqlite(report, db_path, session_id, trigger, transcript_path)` — lines 220-278.
  `CREATE TABLE IF NOT EXISTS` includes `actual_total_tokens INTEGER` (242), then an
  `ALTER TABLE compactions ADD COLUMN actual_total_tokens INTEGER` wrapped in
  `try/except sqlite3.OperationalError: pass` (246-249) — the idempotency path the test
  needs to exercise against a pre-existing table lacking the column.
- `main()` — lines 281-328, CLI via `argparse`, positional `transcript` arg, `--json`,
  `--sqlite DB_PATH`, `--session-id`, `--trigger`.

## Existing test precedent in this repo

`find ~/dotfiles/.claude -iname "test_*.py" -o -iname "*_test.py"` turns up test files under
several `.claude/worktrees/agent-*/stapler-scripts/...` trees (stale worktree copies, not
canonical) plus exactly one canonical hit inside `~/dotfiles/.claude/skills/*/scripts/`:

- `~/dotfiles/.claude/skills/golang-profiling/scripts/tests/test_pct_breakdown.py`
- `~/dotfiles/.claude/skills/golang-profiling/scripts/tests/test_annotate_tree.py`

Both are plain `unittest.TestCase` classes, **not** pytest, **not** an assert-based
`demo()`/`__main__` script. Pattern, read directly from `test_pct_breakdown.py`:

```python
import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent))

from pct_breakdown import compute_breakdown  # noqa: E402


class TestComputeBreakdown(unittest.TestCase):
    def test_leaf_is_last_frame_not_first(self):
        # Regression test: <one-line rationale for why this case exists>
        ...

if __name__ == "__main__":
    unittest.main()
```

Layout: a `tests/` subdirectory sibling to the script being tested (`scripts/tests/`, not
`scripts/test_*.py` flat). `~/dotfiles/.claude/skills/golang-profiling/SKILL.md:24` documents
the run command: `python3 -m unittest discover -s scripts/tests`. Test names are one
`test_*` method per behavior/regression, each with a short comment explaining *why* the case
exists (mirrors the `c2ed9ce` synthetic-line regression this project is guarding against).

No Makefile target or CI step currently runs these (`grep -n "python\|unittest\|scripts/tests"
~/dotfiles/.github/workflows/ci.yml` only shows an unrelated `uv python install 3.12` line) —
the golang-profiling tests are run manually today. Not this project's problem to fix (out of
scope per requirements.md), but worth knowing the new test won't be CI-gated either unless a
separate step is added later.

## Environment check

```
$ python3 --version
Python 3.14.7          # linuxbrew python3, i.e. what's on PATH
$ which pytest
/usr/bin/pytest
$ python3 -m pytest --version
/home/linuxbrew/.linuxbrew/opt/python@3.14/bin/python3.14: No module named pytest
```

`pytest` exists as a binary but is bound to a *different* Python install (system `/usr/bin`)
than the `python3` actually on `PATH` (linuxbrew 3.14) — running `pytest` directly would
silently test under a different interpreter than the one that runs `context_audit.py`, and
`python3 -m pytest` fails outright. So pytest is not actually a lower-friction option here;
it would require adding it as a dependency to the linuxbrew python3 environment, which
requirements.md's "no new runtime dependencies" constraint already rules out. This resolves
the tradeoff without a judgment call: stdlib `unittest` is both the constraint-compliant
choice and the path of least resistance.

## Recommendation

Use stdlib `unittest`, following the golang-profiling precedent exactly, not an
assert-based `demo()`/`__main__` block:

- **Why unittest over assert/demo() here, despite ponytail's "no frameworks" default**:
  ponytail's bar is "ONE runnable check ... an assert-based demo()/__main__ self-check *or*
  one small test_*.py" — both are sanctioned, and this repo already has a concrete,
  in-scope-adjacent precedent (golang-profiling) using small `unittest.TestCase` files
  with one `test_*` per regression case. Matching it is lower-friction than introducing a
  second, different self-check style in the same skills/ tree, and `unittest` is stdlib so
  it doesn't violate the "no frameworks" spirit (no install, no third-party fixture system).
- **File**: `~/dotfiles/.claude/skills/context-audit/scripts/tests/test_context_audit.py`
  (new `tests/` subdir, mirroring golang-profiling's layout), importing via the same
  `sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent))` +
  `from context_audit import analyze, store_sqlite  # noqa: E402` idiom.
- **Fixtures**: build minimal JSONL transcripts inline as Python string/dict literals written
  to a `tempfile.NamedTemporaryFile` per test (or a shared `_write_jsonl(lines)` helper) —
  matches requirements.md's "Rabbit Holes" warning to keep fixtures minimal (only fields
  `analyze()` actually reads: `type`, `timestamp`, `message.usage`, `message.model`,
  `message.content`). No fixtures directory, no golden files.
- **Four cases to cover** (per requirements.md scope): (1) two non-synthetic assistant lines
  with different `usage` → `actual_total_tokens` reflects the *last* one, not a sum; (2) a
  trailing assistant line with `message.model == "<synthetic>"` and its own `usage` is
  excluded, so `last_usage`/`actual_total_tokens` still reflects the prior real turn; (3) a
  transcript with zero `usage` dicts anywhere → `actual_total_tokens is None` and
  `usage_breakdown is None`; (4) `store_sqlite()` called twice against a `tempfile`-backed
  sqlite db — or a db pre-seeded with the `compactions` table minus `actual_total_tokens` —
  to prove the `ALTER TABLE ADD COLUMN` path doesn't raise on a table that already has (or
  never had, then gets) the column.
- **Run command** (mention in SKILL.md alongside the existing real/estimated-tokens section,
  optional but cheap and consistent with golang-profiling's SKILL.md:24 documenting the same
  thing): `python3 -m unittest discover -s ~/dotfiles/.claude/skills/context-audit/scripts/tests`.
