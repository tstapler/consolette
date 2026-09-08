# Adversarial Review: gemini-provider

**Date**: 2026-09-04 (re-review after fix pass targeting the 2026-09-04 blocker + 4 concerns)
**Verdict**: CONCERNS

## Blocker (prior review) — RESOLVED

- [x] **`ThoughtSignatureCache` cross-call / cross-conversation design.** Verified against the
  current `plan.md`:
  1. **Provider-owned, concurrency-safe field**: confirmed. `ThoughtSignatureCache` is now a
     `DashMap`-backed field on `GeminiProvider`, constructed once in `GeminiProvider::new` (Task
     1.6.1b, plan.md:1034-1041), not per-`send()`. Story 1.6.1's acceptance criteria explicitly
     assert cross-call survival on the *same* provider instance (plan.md:1007-1013), which is the
     exact property the original design lacked.
  2. **Session-scoped key**: confirmed, and correctly wired. The cache key is `(session_key,
     ToolUseId)`, not `ToolUseId` alone (plan.md:1014-1016, Task 1.6.1a at plan.md:1026-1031).
     `session_key` comes from `extract_session_id(&body)` (`src/routing/session_overrides.rs:32`),
     falling back to a fixed `"anonymous"` sentinel when absent (plan.md:984-985). I read
     `session_overrides.rs` directly: `extract_session_id` exists exactly as cited, reads
     `body.metadata.user_id` verbatim, and returns `Option<String>` (lines 32-37). Story 3.3.1's
     acceptance criteria (plan.md:1339-1361) correctly thread this through both the stash side
     (Task 3.3.1b) and the replay side (Task 3.3.1c), including a cross-session-isolation test
     (Task 3.3.1d). Domain Glossary (plan.md:68), Pattern Decisions (plan.md:89-90), Epic 1.6
     narrative (plan.md:960-992), and Story 3.3.1 text are internally consistent — no new
     contradiction introduced between them.
  3. **Bounded growth**: confirmed. `THOUGHT_SIGNATURE_TTL_SECS = 900` with sweep-on-insert
     (plan.md:1017-1020, 1028-1029), plus a dedicated unit test (Task 1.6.1c, plan.md:1048-1049).
  4. **New-problem check — residual gap, not a new blocker**: `extract_session_id`'s own doc
     comment (`src/routing/session_overrides.rs:4-13`) is explicit that "the exact shape Claude
     Code's CLI puts there hasn't been directly captured against a live request through this
     proxy (no session was pointed at it during development of this feature)" and that it should
     be verified before relying on it "for anything beyond 'some client sent a stable
     `metadata.user_id`.'" The plan's redesign narrative states as settled fact that "When the
     client *does* send a `user_id`... cross-conversation collision is fully closed" (plan.md:986)
     and that "Claude Code does [send it], per the existing session-pin feature's own precedent"
     (plan.md:985-986) — but that precedent is the same unverified assumption, not independent
     confirmation. Separately, Anthropic's Messages API documents `metadata.user_id` as an
     identifier for the *end user*, not necessarily a per-conversation/session identifier; if a
     single Claude Code user's `user_id` is stable across multiple concurrent conversations (e.g.
     two terminal tabs on two projects), those conversations would still share one `session_key`
     bucket and could theoretically collide on a repeated `ToolUseId` within the 15-minute TTL
     window — a narrower version of the original leak, not a new one, but the plan's "fully
     closed" language overclaims relative to what's actually verified. This doesn't rise to a new
     BLOCKER: the fix is a strict improvement (bounded by TTL + requires same-account concurrency
     + `ToolUseId` collision, vs. the original's unbounded, any-two-conversations exposure), and
     the missing-field case is already honestly flagged as an accepted residual risk. But the
     "field-present" case deserves the same honesty — see new Concern below.

**Net**: the blocker's core defect (data structure architecturally incapable of the cross-call
survival Story 3.3.1 needs) is fixed, correctly and consistently wired into the plan. Treat as
resolved.

## Concerns

- [x] **Story 1.2.2's per-cause auth messages being debug-only** — addressed. Plan.md:411-426 now
  explicitly states the per-cause stderr messages are debug-only (only visible running the script
  by hand), names `run_helper`'s exit-code-only surfacing as the reason, and explains why the plan
  deliberately doesn't thread a cause-code through `src/auth/exec.rs` to recover the distinction.
- [x] **Refresh-token race condition** — addressed. `ADR-001-gemini-auth-token-source.md:90-97`
  now has a dedicated "Refresh-token race condition — deliberate non-decision, not an unconsidered
  gap" section, correctly narrowing the exposure to a manual-login-vs-file-read race (no
  consolette-internal refresh loop exists to race with itself).
- [x] **Task 1.3.4c / Story 1.4.1 dependency-order contradiction** — addressed. Dependency
  Visualization now carries an explicit ★ annotation (plan.md:210, 233-243) stating Story 1.4.1 is
  a compile-time prerequisite of Task 1.3.4c despite being numbered under Epic 1.4, and Story
  1.4.1's own text (plan.md:690-692) cross-references the same note. Contradiction resolved via
  explicit annotation rather than renumbering — acceptable per the original recommendation's
  either/or framing.
- [x] **Story 1.5.1d's regression-test requirement for Anthropic/Bedrock's dashboard behavior** —
  addressed. Plan.md:892-899 adds an explicit "Regression requirement (adversarial-review
  Concern)" with a concrete second integration test spec: all three of Anthropic/Bedrock/Gemini as
  candidates, asserting non-cross-contaminated `cooldowns` entries for all three — not just
  Gemini-in-isolation.
- [ ] **NEW — `extract_session_id`'s stability-per-conversation is unverified, and the plan's
  "fully closed" framing overclaims it.** See point 4 under the resolved blocker above. The fix
  correctly reuses existing, sanctioned infrastructure and materially narrows the original leak,
  but the plan should say explicitly (the way it already does for the missing-`user_id` case) that
  the field-present case's isolation guarantee has not been verified against real Claude Code
  traffic and rests on an assumption (`metadata.user_id` is per-conversation, not per-account) that
  the cited source file itself does not confirm. Recommend either: (a) add the same "accepted
  residual risk for v1" framing to this case instead of "fully closed," or (b) actually capture a
  real Claude Code request's `metadata.user_id` across two concurrent conversations before Phase 3
  starts (the file's own doc comment already calls for this verification via `GET
  /requests/{id}`, independent of this feature).

## Minors

(carried forward unchanged from the prior review — not re-verified this pass)

- The Pattern Decisions table and Task 1.2.2b both describe the auth-failure design as "mirroring `BedrockProvider::do_sso_login`'s has-TTY-vs-not fork shape" — but no TTY-detection code appears anywhere in the plan (confirmed via grep: the only "tty" hits are this same descriptive prose, repeated). The actual design unconditionally never attempts inline reauth, which is a perfectly reasonable choice (agy needs a real browser), but describing it as "mirroring a fork" that doesn't exist could mislead a reviewer looking for the branch logic.
- Story 1.4.3's acceptance criterion references `health.is_available(gemini_index)`, but `src/routing/health.rs` only defines `new`, `set_can_cooldown`, `trip`, and `remaining_secs` — no `is_available` method exists. Harmless since the AC's own parenthetical clarifies the real check is `remaining_secs(idx) > 0`, but the wording should be tightened before someone tries to call a method that isn't there.
- Task 1.3.4d leaves the `fetchAvailableModels` HTTP method as "verify exact HTTP method during implementation, not assumed here" — reasonable honesty, but this open item isn't listed in the plan's top-level "Unresolved Questions" section alongside the other four, making it easy to miss on a scan.
- `DRIFT_COOLDOWN_SECS` trips a 30-minute cooldown on a *single* non-streaming parse failure (ADR-002's deliberate, well-reasoned choice). The ADR doesn't separately consider that a one-off truncated/corrupted 2xx body (network blip, intermediate proxy hiccup) fails the strict parse identically to genuine Google-side schema drift, incurring the same half-hour penalty for what might be transient. Reasoned-through tradeoff, not a gap — just worth a footnote given the cooldown's length.
- Verified positive: `elad12390/antigravity-proxy` — the project `build-vs-buy.md` found non-functional and said should be dropped from the reference list — does **not** appear anywhere in `plan.md` (confirmed by grep). The plan's own rejected-sidecar list cites different (also-rejected) repos instead. No finding here; noted because the review was asked to check for it specifically.
- Verified positive: the three-way error classification (self-healing / needs-reauth / schema-drift) genuinely lands in Phase 1's epic structure (Epics 1.2, 1.4, 1.5), not deferred to a later phase in prose only — the dependency diagram enforces this, not just narrative text.
