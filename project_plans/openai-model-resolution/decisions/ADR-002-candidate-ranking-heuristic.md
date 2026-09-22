# ADR-002: Candidate ranking uses a numeric-token version heuristic, not catalog metadata

**Status**: Accepted
**Date**: 2026-09-18

## Context

`ModelInfo` (`src/providers/mod.rs:142-149`) carries only `id: String` and `owned_by: Option<String>` — no `created` timestamp, no version field. The requirements' motivating incident showed the catalog's own deprecation metadata (`shutdown_date`) is unreliable (`null` on a model that 400s). Resolution must still order same-family candidates "newest-first" (e.g. prefer `gpt-5.3-codex` over `gpt-5.2-codex` over `gpt-5.1-codex-max`) using only the model id string.

## Decision

Rank candidates by extracting dot/hyphen-delimited numeric tokens from each candidate id after stripping the shared family prefix (e.g. `"gpt-5.3-codex"` → tokens `[5, 3]`), compare tuples of tokens lexicographically (descending), and treat a shorter or non-numeric-tail id (e.g. a `-preview`/`-max` suffix with no further digits) as ranking below any candidate with a strictly greater numeric tuple at the first point of difference. Ids with no extractable numeric token after the prefix fall back to reverse-lexicographic string ordering among themselves, sorted after all numerically-tokenized candidates.

## Alternatives rejected

- **Trust `/v1/models` catalog metadata (`created`, `shutdown_date`) for ordering**: rejected — `ModelInfo` doesn't carry a `created` field today, and the requirements' own motivating bug is that `shutdown_date` is not trustworthy even when present.
- **Plain reverse-lexicographic string sort on the full id**: rejected as the sole mechanism — `"gpt-5.10-codex"` would sort before `"gpt-5.2-codex"` lexicographically (`"1" < "2"`) despite being numerically newer; the numeric-token comparison avoids this class of bug, which is a realistic future failure mode as version numbers grow past one digit.

## Consequences

- This is bespoke, testable parsing logic (`rank_candidates(prefix: &str, ids: &[String]) -> Vec<String>`), colocated with the resolution module, unit-tested against deliberately adversarial synthetic ids (`family-v2`, `family-v10`, `family-v3-preview`) per the pitfalls research's testing guidance — never against real current model names, so the test doesn't silently start validating "today's catalog snapshot" instead of the algorithm.
- If a future upstream's naming convention doesn't embed a comparable numeric token (e.g. purely date-coded ids), this heuristic degrades to the lexicographic fallback, which may pick a sub-optimal candidate first — acceptable because a wrong first pick still gets displaced by resolution's own failure-triggered walk; it costs at most one extra probe cycle, not a correctness failure.
