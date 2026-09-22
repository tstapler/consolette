# Research: Pitfalls — regression test for `context_audit.py` usage tracking

Source read in full: `~/dotfiles/.claude/skills/context-audit/scripts/context_audit.py` (328 lines).

## 1. The `c2ed9ce` regression, and what could recur

**The bug** (`git show c2ed9ce` in `~/dotfiles`): the original `f83b7b5` version set
`last_usage = usage` for *every* assistant line with a `dict` usage, with no check on
`message.model`. Claude Code's internal `<synthetic>` assistant turns (summarization
turns injected by Claude Code itself, not part of the visible conversation) also carry a
`usage` dict. Because `analyze()` uses "last usage wins" (the last assistant turn's usage
reflects cumulative input tokens for the whole call, so summing would double-count), a
trailing synthetic turn silently became the reported "actual" context size — wrong,
because a synthetic turn's usage doesn't correspond to the real conversation's context.
The fix added `is_synthetic = message.get("model") == "<synthetic>"` and gated the
assignment on `not is_synthetic`.

**Why this is exactly the shape of bug a test should pin down**: it produced no
exception, no crash, no visibly-wrong-looking type — just a plausible-looking wrong
number. Nothing short of an assertion on the *value* of `actual_total_tokens` given a
transcript with a trailing synthetic turn would have caught it, and nothing but a test
prevents a near-identical regression on the next edit to lines 97–112.

**Recurrence risk if that branch (`analyze()` ~97–112) is touched again without a test**:
- Reordering the `is_synthetic` check relative to the `isinstance(usage, dict)` check, or
  loosening the condition (e.g. `or` instead of `and not`), silently reintroduces the
  original bug.
- Adding a new kind of non-representative assistant turn (Claude Code has added other
  internal turn types before, e.g. compaction-summary turns) without extending the
  exclusion list would reproduce the same class of bug under a different trigger.
- Changing "last wins" to "last non-empty wins" or similar could accidentally treat a
  synthetic turn's usage as a fallback when the real last turn lacks `usage`, again
  silently corrupting the total.

A test should assert on the **exact numeric output** of `actual_total_tokens` /
`usage_breakdown` for a fixture with (a) a normal last turn, (b) a trailing synthetic
turn after the real one — asserting the synthetic turn's numbers are *not* what's
reported — not just that the function runs without raising.

## 2. `store_sqlite()` migration risk under WAL + concurrent PostCompact writers

Read: `store_sqlite()`, lines 220–278.

Pattern: `CREATE TABLE IF NOT EXISTS ...` (schema already includes
`actual_total_tokens INTEGER` in the `CREATE` for a fresh DB) → then unconditionally
attempt `ALTER TABLE compactions ADD COLUMN actual_total_tokens INTEGER`, catching
`sqlite3.OperationalError` and treating any such error as "column already exists."

**Concurrency analysis**:
- `PRAGMA journal_mode=WAL` + `PRAGMA busy_timeout=30000` are set per-connection before
  the DDL. SQLite serializes schema changes (`CREATE`/`ALTER`) via its normal write lock;
  a second concurrent process blocks (up to the 30s busy timeout) rather than corrupting
  state, then either sees the table already exists (`CREATE ... IF NOT EXISTS` no-ops) or
  gets `OperationalError: duplicate column` on the redundant `ALTER`, which is caught.
  This part is safe as long as 30s is enough headroom — plausible for occasional
  PostCompact-triggered runs, but if many disowned background invocations pile up
  concurrently (e.g. rapid compactions across parallel sessions sharing one
  `trend.db`), lock contention could exceed the timeout.
- **Real (if narrow) risk**: the `except sqlite3.OperationalError: pass` is broad — it
  matches on exception *type*, not message content. If the `ALTER TABLE` genuinely fails
  because of a lock timeout (message "database is locked") rather than a duplicate
  column, the code silently treats it as "already migrated" and proceeds to `INSERT`.
  On a legitimately pre-migration legacy DB (one missing the column and hitting a lock
  timeout on this specific run), this does **not** silently corrupt data — the
  subsequent `INSERT` still references `actual_total_tokens`, and since that `INSERT` is
  *not* wrapped in a try/except, it raises `sqlite3.OperationalError: table compactions
  has no column named actual_total_tokens`, which propagates out of `store_sqlite()`
  uncaught. So worst case is a loud crash of that invocation, not silent data loss —
  bounded but still an unhandled-exception surface in an unattended hook context (stderr
  from a disowned background process is easy to lose).
- Verdict: the existing try/except is **sufficient for the idempotency guarantee the
  requirements ask for** (item 4 in scope: migration idempotent against a pre-existing
  table lacking the column) on a single-writer or lightly-contended case. It is not
  provably safe against exception-message conflation under lock contention, but that
  gap already fails loud rather than silent, and reproducing genuine WAL lock contention
  in a unit test is disproportionate to a Small-appetite, stdlib-only test — this is
  worth a one-line note in the test file, not new test code.
- **Recommended test scope for #4**: create a real on-disk (or `:memory:` — note
  `:memory:` doesn't test file-based WAL locking, but does test the DDL/migration
  sequence itself) SQLite DB with the `compactions` table created *without*
  `actual_total_tokens` (simulating a `f83b7b5`-era DB), call `store_sqlite()`, and
  assert both that no exception is raised and that the column now exists with the
  inserted value readable back. Then call `store_sqlite()` a second time on the same DB
  to assert the now-idempotent `ALTER` path doesn't raise either.

## 3. Edge cases in real transcripts vs. "last usage wins" (lines 153–162, `if last_usage:`)

Verified against a real transcript on this machine
(`~/.claude/projects/.../*.jsonl`) — a real assistant `message.usage` dict looks like:

```json
{
  "input_tokens": 2,
  "cache_creation_input_tokens": 51789,
  "cache_read_input_tokens": 0,
  "output_tokens": 439,
  "server_tool_use": {"web_search_requests": 0, "web_fetch_requests": 0},
  "service_tier": "standard",
  "cache_creation": {"ephemeral_1h_input_tokens": 51789, "ephemeral_5m_input_tokens": 0},
  "inference_geo": "not_available",
  "iterations": [...],
  "speed": "standard"
}
```

All four fields the code reads (`input_tokens`, `output_tokens`,
`cache_creation_input_tokens`, `cache_read_input_tokens`) are present at the top level
in this real sample; the extra keys (`server_tool_use`, `cache_creation` nested dict,
`iterations`, etc.) are ignored safely because the code only reads four named keys via
`.get(..., 0)` and never iterates the dict's keys. **This confirms the code doesn't break
on the "extra/unknown fields present" case** — a real risk for a hand-written fixture
that includes only the four known fields and nothing else (see point 4).

Specific edge cases and whether the current code handles them:

- **Zero assistant lines** (transcript is all `user`/other types): `last_usage` stays
  `None` for the whole loop → `if last_usage:` is falsy → `actual_total_tokens = None`,
  `usage_breakdown = None`. Handled gracefully; `main()` line 306 checks
  `report.get("actual_total_tokens") is not None` and prints the "unavailable" branch.
  No exception. **Confirmed safe by reading, not yet by a test.**
- **All-synthetic assistant lines**: same as above — `is_synthetic` gates every
  assignment, `last_usage` never gets set, same graceful `None` path. This is the exact
  case the `c2ed9ce` fix targets and the most important one for the new test to cover
  explicitly (not just "some but not all are synthetic" — an all-synthetic transcript
  is the purest form of the regression, and cheaper to construct as a fixture).
- **Missing `message.usage` key entirely** (older transcript format, or a line where
  Claude Code hasn't attached usage yet): `message.get("usage")` returns `None` →
  `isinstance(usage, dict)` is `False` → skipped, `last_usage` keeps its prior value (or
  stays `None`). Handled.
- **`usage` present but not a dict** (defensive case, e.g. malformed/truncated JSON
  producing `usage: null` or `usage: "..."`): same `isinstance` guard — skipped safely.
- **`usage` dict present but missing one of the four expected keys** (e.g. a future API
  response omitting `cache_creation_input_tokens` when it's zero, rather than including
  it as `0`): `.get(key, 0)` degrades gracefully to `0` for that key. Handled — *but*
  this only guards against a **missing** key; it does **not** guard against a key that's
  present with value `None` (e.g. `{"input_tokens": 100, "cache_read_input_tokens":
  None, ...}`). In that case `.get("cache_read_input_tokens", 0)` returns `None` (the
  default only applies when the key is absent, not when it's `None`), and the subsequent
  `sum(usage_breakdown.values())` on line 162 raises `TypeError: unsupported operand
  type(s) for +: 'int' and 'NoneType'` — **unhandled, would crash the whole script**,
  including under the PostCompact hook. This is speculative (no observed real transcript
  has a null value in `usage` — the one inspected here had all four as real integers),
  so it's not proven to occur in practice, but it's a plausible enough API-response shape
  (nullable optional field) that it's worth one defensive test case: a fixture with an
  explicit `"cache_read_input_tokens": null` to document current behavior (crash) even
  if the fix itself is out of scope for this Small-appetite item. Flagging it is in
  scope; fixing it is arguably scope creep unless the test simply documents the
  known-crash case as an expected-`TypeError` assertion (cheap, and prevents a *silent*
  behavior change later if someone "fixes" it without noticing the crash was the
  documented contract).

## 4. Risk of hand-crafted fixtures being unrepresentative

Confirmed real risk, and the two biggest gaps to avoid:

- **Over-simplified `usage` shape**: as shown above, a real `message.usage` dict has 6+
  keys beyond the four the code reads, plus a nested `cache_creation` dict that
  *duplicates* the same information under different key names (`ephemeral_1h_input_tokens`
  / `ephemeral_5m_input_tokens` vs. the flat `cache_creation_input_tokens`). If a fixture
  includes *only* the four flat keys the code reads, the test would pass without ever
  exercising "does the code correctly ignore the nested/newer `cache_creation` shape,"
  and — more importantly — would not catch a future edit that mistakenly reads from the
  nested shape instead of (or in addition to) the flat one. Recommend at least one
  fixture line with the full real-shape `usage` dict (copy the structure verified above,
  values are non-sensitive token counts) rather than a minimal 4-key dict, so the test
  reflects what `analyze()` actually has to parse in production.
- **Missing the `<synthetic>` line's realistic position**: `c2ed9ce`'s bug only manifests
  when the synthetic turn is the *last* assistant line (or at least, last non-synthetic
  vs. synthetic ordering matters for "last wins"). A fixture that puts the synthetic line
  in the middle of the transcript, followed by a normal assistant turn, would not
  exercise the regression at all — it would pass both before and after the `c2ed9ce` fix,
  giving false confidence. The fixture must place the synthetic assistant turn *last* in
  file order to actually pin the bug down.
- **Missing incidental line types**: real transcripts interleave `type: "attachment"`,
  `"file-history-snapshot"`, `"mode"`, `"permission-mode"`, etc. between `user`/
  `assistant` lines (per the script's own module docstring, lines 6–9). A fixture with
  only `user`/`assistant` lines is fine for testing the usage-tracking logic specifically
  (in scope), but if the test also exercises `by_type`/breakdown totals, omitting these
  other line types means the test never verifies the `etype not in ("user", "assistant")`
  fallback branch (line 93–95) still works — low risk since that logic is unchanged/out
  of scope per requirements, but worth a one-line fixture entry if convenient, not worth
  new scope otherwise.
- **JSONL malformity**: `load()` (lines 54–63) silently `continue`s past
  `json.JSONDecodeError` and blank lines. A hand-crafted fixture that's always
  perfectly-formed JSON never exercises this path; not required by the requirements.md
  scope (items 1–4 don't mention malformed-line tolerance), so this is explicitly a
  "don't add scope" case per the Rabbit Holes section, not a gap to close.

Net guidance for the fixture: keep it minimal per requirements.md's own Rabbit Holes
warning, but make sure the two fixtures that matter most (all-synthetic transcript;
trailing-synthetic-after-real transcript) use a *realistic* full-shape `usage` dict
copied from an actual transcript, not an invented minimal one — that's the one place
"keep it minimal" and "keep it representative" are in tension, and representative should
win since it's exactly the field-shape mismatch that caused the one bug this script has
already had.
