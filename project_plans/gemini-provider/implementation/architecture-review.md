# Architecture Review: gemini-provider
**Date**: 2026-09-04 (blocker-verification re-run)
**Verdict**: CONCERNS (0 blockers, 5 concerns carried forward — 2 since resolved, 3 nitpicks — 1 since resolved)

## Scope of this pass
This is a targeted re-review of the two BLOCKERs from the prior pass only, per instructions —
not a full three-lens re-review. Both are now RESOLVED at the plan level, with no new blocker
introduced. The prior review's Concerns/Nitpicks list is carried forward below unchanged, with a
note on which of those the same fix pass happened to also address.

## Constitution Violations
- [ ] N/A — no `docs/adr/ADR-000-architecture-constitution.md` found in this repo.

## Blocker A: `ResponseShapeMismatch` breaking `src/entrypoint/errors.rs`'s exhaustive matches

**Verdict: RESOLVED.**

- Story 1.4.1 (`project_plans/gemini-provider/implementation/plan.md:680-734`) now has an explicit
  "Execution note (fixes architecture-review Blocker A)" naming both functions and file/line
  ranges, and its **Files** list is `src/providers/mod.rs`, `src/entrypoint/errors.rs` (line 712)
  — the missing file from the prior pass is now present.
- Task 1.4.1c (lines 724-734) adds a `ProviderError::ResponseShapeMismatch(msg) => (...)` arm to
  **both** `map_provider_error_anthropic` and `map_provider_error_openai`, each mirroring that
  function's existing `Upstream{..}` envelope shape (`api_error` / `server_error` respectively)
  at 502 Bad Gateway, plus two new unit tests in `errors.rs`'s existing test module.
- The ★ dependency note under Dependency Visualization (plan.md:233-244) additionally makes Story
  1.4.1 a documented compile-time co-requisite of Task 1.3.4c (the first call site that constructs
  the variant), so a subagent implementing Epic 1.3 in isolation is explicitly told to land Story
  1.4.1 in the same change rather than assuming Epic 1.4 landed first.
- Cross-checked against the real file: `src/entrypoint/errors.rs:16-65`
  (`map_provider_error_anthropic`) and `:71-121` (`map_provider_error_openai`) are both still
  genuinely exhaustive matches over `ProviderError` — 8 variants, no wildcard arm, confirmed by
  reading both functions in full. The plan's description remains accurate, and the proposed 9th
  arm in each (`ResponseShapeMismatch(msg) => (StatusCode::BAD_GATEWAY, None, ...)`) is a
  syntactically ordinary addition to a `match` with no wildcard already present — it will compile.
- No new blocker introduced: the chosen 502 status and envelope shape are consistent with the
  existing `Upstream{..}` arm's pattern in both functions, and the addition doesn't touch any
  other arm.

## Blocker B: `ThoughtSignatureCache` lifetime

**Verdict: RESOLVED.**

- Epic 1.6 (`plan.md:960-1050`) carries an explicit "Redesign note (fixes architecture-review
  Blocker B / adversarial-review Blocker)" describing exactly the defect (a `send()`-local value
  can't bridge two separate HTTP requests) and the fix (promote to a `GeminiProvider`-owned field).
- **Provider-owned, not `send()`-local**: Task 1.6.1b (lines 1034-1041) adds
  `thought_signatures: ThoughtSignatureCache` as a `GeminiProvider` struct field, constructed once
  in `GeminiProvider::new()` — explicitly *not* inside `send()`. Story 1.6.1's acceptance criteria
  (lines 1004-1013) include a test asserting the same cache instance survives across two sequential
  `send()` calls on one provider instance.
- **Concurrency-safe**: backed by `dashmap::DashMap<(String, ToolUseId), CacheEntry>` (Task
  1.6.1a, line 1026), mirroring the already-established `ExecCredentialCache` pattern
  (`src/auth/exec.rs:52-55`). Confirmed `dashmap = "6"` is already a real dependency
  (`Cargo.toml:69`), so this isn't a new, unverified dependency.
- **Keyed beyond raw `tool_use` id**: the cache key is `(String, ToolUseId)` — a `session_key`
  plus the tool-use id — not `ToolUseId` alone (Task 1.6.1a, acceptance criterion at lines
  1014-1016). Story 3.3.1's cross-session isolation acceptance criterion (lines 1356-1361) and
  Task 3.3.1d both explicitly test that session B cannot read session A's signature for the same
  `tool_use` id.
  - **Session-identifier source claim verified**: the plan says `session_key` comes from
    `extract_session_id(&body)` (`src/routing/session_overrides.rs:32`), reading
    `metadata.user_id`. Read the real file — `extract_session_id` exists at exactly that line,
    with that exact signature and behavior: `body.get("metadata")?.get("user_id")?.as_str()...`,
    returning `Option<String>`. The plan's claim is accurate, not fabricated.
  - Falls back to a fixed `"anonymous"` sentinel when the client sends no `user_id`
    (`.unwrap_or_else(|| "anonymous".to_string())`, plan.md:984-992) — an accepted, documented
    residual risk for v1 (narrower collision surface than the pre-fix design, not a full close),
    not silently glossed over.
- **Eviction/TTL policy present**: `THOUGHT_SIGNATURE_TTL_SECS: u64 = 900` (Task 1.6.1a, line
  1029), swept on every `.insert()` call (sweep-on-insert, no background task) — Story 1.6.1's
  acceptance criteria and Task 1.6.1c both include a TTL-sweep test. Growth is bounded modulo the
  documented gap that a sweep only runs on insert (a long idle period with zero new tool calls
  leaves stale entries until the next insert) — this is an existing, disclosed limitation of the
  chosen sweep strategy, not a new defect this fix pass introduced.
- **Type safety**: `ToolUseId(String)` newtype (Task 1.6.1a) is used consistently as the key type
  in both `ThoughtSignatureCache` and Phase 3's `GeminiToolCallState`, closing the
  primitive-obsession concern from the prior review's Concerns list (see below).
- No new blocker introduced: the redesign doesn't touch `Provider` trait signatures or any other
  exhaustive match, and `ToolUseId`/`CacheEntry`/`DashMap` all derive/require only `Send + Sync`
  types (`String`, `Instant`), so no new concurrency-safety gap was spotted.

## Concerns (carried forward from the prior review — not blocking; not required to re-verify)

- [ ] `Router::dispatch`'s `ProviderError` branching is guard-based (`is_x()` chain), not
  compiler-exhaustive — still true; not fixed by this pass, and the prior review already noted
  this is a pre-existing pattern, not a defect in this plan.
- [x] **Since resolved**: `GeminiToolCallState`/`ThoughtSignatureCache` raw-`String` key/value —
  now uses the `ToolUseId` newtype consistently (Story 1.6.1, Domain Glossary line 66).
- [~] **Partially addressed**: `gemini.rs` single-file size/responsibility concern — no submodule
  split was added, but the plan's Risk Control section now has an explicit "Code-hotspot
  checkpoint before Phase 2 starts" (plan.md:140-148) directing a `code-hotspot-analysis` run and
  a submodule split if warranted, before streaming reconstruction lands on top.
- [x] **Since resolved**: `research/build-vs-buy.md`'s stale keyring recommendation — now carries
  an explicit "Superseded in part by ADR-001" addendum (`research/build-vs-buy.md:204-208`)
  cross-referencing ADR-001 as the actual final decision.
- [ ] Two Unresolved Questions (`GEMINI_3_PRO_OUTPUT_CEILING`, header fidelity) still block
  specific Phase 1 stories despite the plan's "Ready for implementation" status — still true,
  disclosed honestly in the Unresolved Questions section, not a structural defect.

## Nitpicks (carried forward — not blocking)

- [x] **Since resolved**: `DRIFT_COOLDOWN_SECS` visibility hedge — Task 1.4.3a now says plainly
  `pub(crate)` (plan.md:779), matching Task 1.4.3b's code sample; the "or re-exported" hedge is
  gone.
- [ ] `translate_anthropic_request_to_gemini`'s signature changes from infallible to
  `Result<..., ProviderError>` between Phase 1 and Phase 3 — still true, still a deliberate,
  low-cost, plan-acknowledged churn.
- [ ] `ProviderError::ResponseShapeMismatch` won't increment `ProxyMetrics`'s existing
  `err_timeout`/`err_auth`/`err_rate_limit`/`err_validation` counters, only the new
  `last_error_kind` field — still true, likely fine, unverified whether any dashboard view still
  reads only the old counters.
