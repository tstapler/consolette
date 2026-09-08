# ADR-002: Gemini schema-drift error classification and `HealthRegistry` cooldown

**Status**: Accepted
**Date**: 2026-09-04
**Deciders**: Tyler Stapler (via Phase 3 planning)

## Context

Two related gaps, both raised by Phase 2 research and both required to close in the **first**
internal milestone (non-streaming text) per `requirements.md`'s carried-forward open questions and
`research/ux.md` §5's job-to-be-done analysis:

1. **No distinct error variant for "200 OK but the response didn't parse."** `ProviderError`
   (`src/providers/mod.rs:33-55`) has `Upstream { status, body }` for HTTP-level failures, but
   nothing for "the HTTP call succeeded and the body didn't match the documented/expected shape."
   `research/ux.md` calls this out directly: `Validation` is semantically wrong (that variant means
   "client sent bad input," per its own doc string at `src/providers/mod.rs:40`), and folding a
   schema-drift signal into generic `Upstream{status:200,..}` makes it indistinguishable from any
   other error in the dashboard's error table.
2. **`Router::dispatch`'s catch-all error arm never trips cooldown.** `src/routing/router.rs:336-339`:
   ```rust
   Err(e) => {
       self.record_attempt(&chosen.name, attempt_started, Err(&e), &model);
       last_error = Some(e);
   }
   ```
   This arm handles every error that isn't validation/auth/rate-limited — including
   `is_transient()` errors (`Timeout`, `Upstream{..}`, per `src/providers/mod.rs:94-99`'s own doc
   comment: "worth failing over... but NOT worth tripping cooldown the way a rate limit does").
   That's a reasonable default for Bedrock/Anthropic/OpenAI, where a parse failure is a rare,
   presumably-transient blip against a stable, versioned API. It is the **wrong** default for an
   actively-drifting, undocumented Google-internal protocol (`research/pitfalls.md`'s §1, citing
   `shekohex/opencode-google-antigravity-auth#9` and four `NoeFabris/opencode-antigravity-auth`
   issues of exactly this shape): without a distinct trip condition, a permanently-broken Gemini
   endpoint gets retried on every single request forever, with no cooldown ever engaging.

Two structural options were posed for closing gap 2:

- **(a)** Classify Gemini schema-drift failures as a *synthetic* `ProviderError::RateLimitedWithRetry`
  so the existing `Err(e) if e.is_rate_limited() => health.trip(...)` arm
  (`src/routing/router.rs:330-335`) fires with zero `Router`/`HealthRegistry` changes.
- **(b)** Extend `Router`/`HealthRegistry` with a real, additive, opt-in
  consecutive-failure-triggered cooldown mechanism that doesn't change behavior for the other
  three providers.

## Decision

**Neither (a) nor a full consecutive-failure counter.** Add a genuine new variant,
`ProviderError::ResponseShapeMismatch(String)`, plus one new classification method
`is_response_shape_mismatch()`, and **one new `Router::dispatch` match arm** that trips
`HealthRegistry` immediately (first occurrence, not after N consecutive failures) using a longer
override duration than the default rate-limit cooldown.

```rust
// src/providers/mod.rs — new variant + classifier, alongside the existing ones
#[error("unexpected response shape from upstream: {0}")]
ResponseShapeMismatch(String),
// ...
#[must_use]
pub fn is_response_shape_mismatch(&self) -> bool {
    matches!(self, ProviderError::ResponseShapeMismatch(_))
}
```

```rust
// src/routing/router.rs — new arm, inserted before the existing catch-all
Err(e) if e.is_response_shape_mismatch() => {
    self.record_attempt(&chosen.name, attempt_started, Err(&e), &model);
    self.health.trip(chosen.index, Some(Duration::from_secs(DRIFT_COOLDOWN_SECS)));
    last_error = Some(e);
}
```

`HealthRegistry` itself needs **no structural change** — `trip(idx, override_duration)`
(`src/routing/health.rs:58`) already accepts an explicit duration; this reuses it exactly the way
a parsed `retry-after` header does today. `DRIFT_COOLDOWN_SECS` (e.g. 1800s / 30 minutes, well
above the default `cooldown_seconds` of 300s) lives as a constant in `src/providers/gemini/error.rs`
(re-exported via `src/providers/gemini/mod.rs` so the `router.rs` call site is unaffected by the
submodule split — see plan.md's Domain Glossary and Task 1.4.3a) —
this is deliberately Gemini-adjacent context, not a `Router`-wide default, since the other three
providers never construct this variant.

## Rationale

- **Why not (a), alias onto `RateLimitedWithRetry`:** a rate-limit is, by definition, self-healing
  — retry later at the same endpoint with the same request shape and it'll likely succeed. Schema
  drift is the opposite: retrying the *same* request against the *same* unchanged translation code
  will fail identically until a human patches the code. Surfacing schema drift as "rate limited" in
  logs/dashboard/metrics is actively misleading — it directly betrays `ux.md`'s stated
  job-to-be-done ("I trust this system to tell me when something's wrong"), which is the reason
  this work is pulled into the first milestone at all. It would also inflate
  `err_rate_limit`/`RateLimited` counters (`src/metrics/counters.rs:44,189-191`) with an unrelated
  failure class, corrupting that metric for every other provider's dashboard reading too.
- **Why not a full consecutive-failure counter (pure (b)):** `research/pitfalls.md`'s own
  recommendation #1 hedges this as "a SINGLE malformed response shouldn't cool down (could be one
  bad chunk), a RUN of them should" — but that caveat is about **streaming** chunk-level
  corruption (Phase 2 of this rollout), not a **non-streaming** full-body parse failure. For a
  non-streaming response, there is no "one bad chunk out of many" — either the complete JSON body
  matched the documented `GenerateContentResponse` shape or it didn't; a single non-streaming parse
  failure already means the whole response was unusable. Building new per-index
  consecutive-failure-counter state in `HealthRegistry` for a scenario not yet observed to need it
  is exactly the speculative-generality ADR-001 already rejected for the auth question — same
  principle, applied here. Immediate-trip-on-first-failure, using machinery `HealthRegistry`
  already has, is the smaller and safer diff for this appetite.
- **Why a real new variant, not folding into `Upstream{status:200,..}`:** the whole point of this
  ADR is dashboard/log distinguishability (`requirements.md`'s Observability Requirement:
  "distinct from ordinary `ProviderError::Upstream`"). A same-shaped-but-different-status hack
  would still require new dashboard logic to special-case status 200, with no compile-time
  signal — a real variant is caught by every existing exhaustive `match` on `ProviderError` at
  compile time, and is self-documenting in `/errors/summary`.

## Consequences

- `GeminiProvider`'s non-streaming response parsing (plan.md Story 1.4.2) uses strict `serde`
  deserialization into a typed `GeminiGenerateContentResponse` struct; a `serde_json::Error` there
  becomes `ProviderError::ResponseShapeMismatch(format!("..."))`, never a lenient
  `.unwrap_or_default()` — the opposite of `bedrock.rs`/`openai.rs`'s existing lenient-parsing
  style, deliberately, per `requirements.md`'s "fail closed" scope item.
- The dashboard needs a genuinely new status class (plan.md Story 1.5.2) since neither existing
  `status-active`/`status-cooldown` distinguishes "will self-heal" from "needs a code fix."
  `ErrorTracker`'s existing `error_type` field (`src/metrics/error_tracker.rs:86`) is currently
  derived by *regex-guessing keywords out of the error's `Display` string*
  (`extract_signature`, `src/metrics/counters.rs`... err, `src/metrics/error_tracker.rs:141-194`)
  rather than being told the real `ProviderError` variant by the caller that already has it typed
  — this is a genuine pre-existing fragility (confirmed by reading
  `src/routing/router.rs:379-383`: `record_attempt` calls `error_tracker.push(&e.to_string(), ...)`,
  discarding the typed variant before the string ever reaches `ErrorTracker`). Plan.md Story 1.4.4
  fixes this as a small, additive, root-cause fix (pass the real classification through, not
  guessed text) rather than adding a fifth regex pattern for `ResponseShapeMismatch` on top of the
  existing keyword-matching approach.
- Whether streaming chunk-level drift (Phase 2) needs the fuller consecutive-failure-counter
  treatment is left as an explicit Unresolved Question in plan.md — build it only if single-bad-chunk
  false-positive cooldowns are actually observed once streaming ships, per the same
  build-only-when-verified principle as ADR-001.

## Rejected alternatives

- **(a)** Alias onto `ProviderError::RateLimitedWithRetry` — rejected: conflates two semantically
  opposite failure modes (self-healing vs. needs-a-code-fix), corrupts the existing rate-limit
  metric, and actively misleads the exact job-to-be-done this work exists to serve.
- **(b) full form** — a new per-index consecutive-failure counter in `HealthRegistry` — rejected
  for v1: unverified need for non-streaming (a single failure there already means total failure),
  larger diff than the appetite calls for; revisit only if streaming (Phase 2) proves it's needed.
