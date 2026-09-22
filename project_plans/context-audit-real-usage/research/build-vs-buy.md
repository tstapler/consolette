# Research: Build vs. Buy for context-audit real-usage tests

Builds on `project_plans/context-audit-real-tokens/research/build-vs-buy.md` (prior run).
Requirements: `../requirements.md`. Target: `~/dotfiles/.claude/skills/context-audit/`.

## Correction to prior research

The prior doc recommended a plain `assert`/`demo()` script over `unittest`. That is moot:
the tests already shipped in dotfiles `4a5ec3d` as stdlib `unittest` (VERIFIED: `git show --stat 4a5ec3d`,
2 files, 134 insertions; classes `ClaudeCodeUsageTrackingTest`, `StoreSqliteMigrationTest`;
imports are stdlib only plus `from context_audit import analyze, store_sqlite`).
Re-run 2026-09-20: `python3 -m unittest discover -s scripts` from the skill dir -> `Ran 6 tests ... OK`.
The still-valid parts of the prior doc: no pytest infra in dotfiles; cmdcrush has no adaptable fixtures.

## 1. Existing OSS library (pytest, hypothesis, unittest)

- Pros: pytest gives nicer diffs; hypothesis could fuzz usage shapes.
- Cons: no pytest config anywhere applicable to this skill; new dependency for one 328-line stdlib-only script;
  hypothesis is disproportionate for four deterministic cases; the code under test has no algorithmic core.
- Verdict: pytest/hypothesis **Not recommended**. stdlib `unittest` **Recommended** (already in use; SKILL.md:67 documents it).

## 2. SaaS / managed API

- Pros: none relevant (a hosted test/CI service adds nothing to a local pure-function test).
- Cons: network dependency, account/cost, no data-locality benefit; transcripts may contain private content.
- Verdict: **Not recommended**.

## 3. LLM-generated implementation vs battle-tested library (for the algorithm)

The "algorithm" is trivial: iterate JSONL, skip `message.model == "<synthetic>"`, keep the last `message.usage`.
There is no library equivalent (tokenizers are for the `len//4` estimate, which is explicitly out of scope).
The risk with LLM-written tests is tautology (asserting whatever the code does). Mitigation, already present:
the test names encode distinct failure classes (sum vs last, trailing synthetic, all synthetic, none, legacy migration),
tied to the c2ed9ce bug class. Reviewer check: mutate `analyze()` locally (sum instead of last; drop synthetic filter)
and confirm the tests fail; this is cheap and gives real evidence, but production code must be restored afterward.
- Verdict: hand-written fixtures + stdlib **Recommended**; a third-party token library for this purpose **Not recommended**.

## 4. Fork/adapt existing implementation (4a5ec3d)

- Pros: criteria 0-3 are already covered one-to-one (requirements.md table); zero production change satisfies AC 6;
  no new code means no new bugs; SKILL.md line for AC 5 already exists.
- Cons: nothing to "build", so value is in verification: AC 5 accuracy re-check, AC 4 literal-path mismatch,
  and confirming tests fail under mutation (not just pass).
- Verdict: **Recommended** — "no new code; verify and close the criteria". Any work in this repo is documentation/evidence only.

## 5. Move test into `scripts/tests/` vs keep in place

The import `from context_audit import ...` has no `sys.path` manipulation (VERIFIED by grep). It works under
`discover -s scripts` because `scripts/` is the start dir and is put on `sys.path`. Moving to `scripts/tests/`
would make `discover -s scripts/tests` fail with ImportError unless a `sys.path.insert` or `-t scripts` top-level is added,
and would break the SKILL.md:67 command (`python3 -m unittest test_context_audit -v` run from `scripts/`).

| Option | Pros | Cons | Verdict |
|---|---|---|---|
| Keep in `scripts/` (b) | Zero churn; documented command keeps working; already green | AC 0/4 path text not literally met | **Recommended** (record deviation; substance met: 5 new tests + 1 pre-existing = 6, exit 0) |
| Move to `scripts/tests/` (a) | Literal AC match | Needs sys.path hack or `-t`; changes SKILL.md command; extra commit in another repo for no behavioral gain | Viable, only if the reviewer insists |

## Recommendation

Do not build. Verify against dotfiles master, run the suite, optionally do a mutation check, confirm SKILL.md:67 and the
real-vs-estimated wording, and close AC 0-6 with the AC 0/4 path deviation stated.
