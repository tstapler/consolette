# Research: Build vs. Buy — test approach for `context_audit.py`

**Scope**: one regression test/self-check for `analyze()`'s usage-tracking logic
(last-usage-wins, synthetic-line skip, no-usage-data fallback, idempotent
`ADD COLUMN` migration) in
`~/dotfiles/.claude/skills/context-audit/scripts/context_audit.py` (328 lines,
stdlib-only). See
[`../requirements.md`](../requirements.md) for full scope; out of scope:
touching `analyze()` itself or `cmdcrush`/`claude-proxy-rs`.

## 1. Existing OSS test framework (pytest, hypothesis, …)?

**Checked**: `find ~/dotfiles -iname "pytest.ini" -o -iname "pyproject.toml" -o -iname "conftest.py"`.

Result: no repo-wide pytest config. Every `pyproject.toml` hit is scoped to an
independent `stapler-scripts/<tool>/` subproject (`cfgcaddy`, `bootstrap-pyinfra`,
`vaping-at-home`, `llm-sync`, `display-switch`, `ark-mod-manager`, `claude-proxy`,
`slack-emoji-export`) plus stray duplicates under `.claude/worktrees/agent-*/` — each
is that subproject's own isolated packaging config, not a shared test harness the
`context-audit` skill participates in or could piggyback on. The one `conftest.py`
hit is inside a vendored `numpy` package under a different skill's `.venv`
(`pdf-proof/.venv/.../numpy/conftest.py`) — third-party, irrelevant. **No pytest
infrastructure exists anywhere that this script could extend at near-zero marginal
cost.**

Adopting pytest here means introducing it as a new dependency (`pip install
pytest` or a per-skill `.venv`) for exactly one test file testing one function,
with no existing runner/CI hook in this repo to invoke it. That's setup cost with
no amortization — nothing else in `~/dotfiles` benefits.

## 2. stdlib `unittest` vs. plain `assert`-based `demo()`/`__main__`

The session's own `ponytail` engineering-discipline rule (quoted verbatim in the
task) is explicit: *"leaves ONE runnable check behind: an assert-based
demo()/__main__ self-check or one small test_*.py. No frameworks, no fixtures, no
per-function suites unless asked."* The requirements.md constraints section
independently arrives at the same place: *"No new runtime dependencies — stdlib
`unittest` (or plain `assert` + `__main__`) only."*

Both `unittest` and `assert`-based self-check satisfy "no new dependency" (both
are stdlib/zero-dep). The distinguishing factor is ceremony vs. the size of what's
being tested: four narrow assertions (last-usage-wins, synthetic-skip, no-usage
graceful `None`, idempotent `ALTER TABLE`) don't need `unittest`'s
`TestCase`/`setUp`/test-discovery machinery — a single `test_context_audit.py`
with a `demo()` function building three minimal in-memory JSONL fixtures and
asserting on `analyze()`'s return dict, runnable directly (`python3
test_context_audit.py`) or imported, is proportionate and matches the sibling
script's own style (no classes, no fixtures directory, flat functions).

**Recommendation**: plain `assert`-based script, not `unittest.TestCase`. `unittest`
adds no capability this scope needs (no test parametrization, no shared fixtures
across many cases, no mocking) — it's the "framework you don't need" case the
ponytail rule is guarding against, just at the stdlib rather than third-party
tier.

## 3. Rust `cmdcrush` — adaptable test fixtures?

**Checked**: `src/bin/cmdcrush/main.rs`, `tests/` (`cost_metrics_end_to_end.rs`,
`toml_parity.rs`, `tests/fixtures/{compression_corpus,toml_parity}/`).

`run_model_stats` (`src/bin/cmdcrush/main.rs:479-548`, confirmed by direct read)
has **no unit tests of its own** — no `#[test]`/`#[cfg(test)]` block in
`main.rs`, and neither `tests/cost_metrics_end_to_end.rs` nor `tests/toml_parity.rs`
exercises it. `tests/fixtures/` holds `compression_corpus/*` (plain-text log/diff
samples for a dedup/compaction feature) and `toml_parity/*.toml` (config-parity
fixtures) — neither is a JSONL transcript fixture, and neither contains
`message.usage`/`message.model`/synthetic-line data. **There is nothing to import
or literally adapt.**

What *is* useful, read directly rather than assumed: `run_model_stats`'s inline
parsing confirms the exact JSON shape context_audit.py's fixtures need to mimic —
`value.get("type") == "assistant"`, `value.pointer("/message/model")` compared
against the literal string `"<synthetic>"` to skip, and `value.pointer("/message/usage")`
for the token fields (input/output/cache_creation/cache_read), matching what
`context_audit.py`'s own `analyze()` already reads. This is corroboration that
`context_audit.py`'s fixture format (skip `<synthetic>` models, read
`/message/usage`) matches the sibling Rust tool's understanding of the same
Claude Code transcript format — useful as a second reference for what fields a
minimal fixture must include, not as fixture data to copy.

## 4. Verdict

| Option | Pros | Cons |
|---|---|---|
| pytest/hypothesis | Property-based testing (hypothesis) could fuzz usage-dict shapes | No existing pytest infra anywhere in `~/dotfiles` to amortize into; adds a dependency for one function; explicitly rejected in requirements.md's Alternatives Considered |
| stdlib `unittest` | Zero new dependency; built into every Python 3 | `TestCase`/discovery ceremony buys nothing for 4 flat assertions; heavier than the script it's testing |
| **`assert`-based `demo()`/`__main__` self-check** | **Zero dependency, runnable standalone or importable, proportionate to a 328-line script, matches ponytail rule and requirements.md constraints exactly, matches sibling script's existing flat-function style** | Weaker output on failure than a test-runner (a plain `AssertionError` traceback, not a diff); acceptable given only 4 cases |
| Adapt `cmdcrush` Rust fixtures | None — no transcript fixtures exist there to adapt | N/A |

**Recommended approach**: a standalone `test_context_audit.py` (or a `demo()` under
`if __name__ == "__main__":` in a new small file) next to `context_audit.py`,
using hand-built minimal JSONL fixture strings (per requirements.md's own
"Rabbit Holes" guidance — keep fixtures to only the fields `analyze()` reads) and
plain `assert` statements, covering the four in-scope cases. No new dependency, no
framework, no fixtures directory — consistent with both the ponytail
engineering-discipline rule this session runs under and the repo's total absence
of any pytest precedent to build on.
