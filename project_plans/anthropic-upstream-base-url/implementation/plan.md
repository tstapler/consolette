# Implementation Plan: anthropic-upstream-base-url

**Feature**: Give `UpstreamKind::Anthropic` an optional `base_url` field (default `https://api.anthropic.com`), mirroring `UpstreamKind::Openai { base_url }`, so an Anthropic Messages API-compatible endpoint (internal proxy, corporate gateway, self-hosted relay) can be configured as `kind = "anthropic"` instead of being forced into `kind = "openai"` or a fork.
**Date**: 2026-09-25
**Status**: Ready for implementation (revised after architecture + adversarial review — see "Revision log" below)
**ADRs**: None (see rationale in Step 5 discussion below the template body)

## Revision log (post-review)

Both reviewers returned CONCERNS (0 blockers each), so no formal repair loop ran, but the
following seven findings had real teeth and are folded into this revision:

1. **Architecture — concern:** `normalize_model_name`'s gate no longer does a raw string
   comparison against `default_anthropic_base_url()` inside `anthropic.rs` — that coupled a
   provider module to config-schema internals just to make a comparison. The default-vs-custom
   decision is now computed once in `router.rs::build_providers` (which already holds both
   values) and passed into `AnthropicProvider::new` as a `bool`. See revised Pattern Decisions
   and Story 3.1.1/3.1.2.
2. **Architecture — concern:** `upstream_kind_label` is a byte-identical duplicate in
   `entrypoint/mod.rs:171` and `validate.rs:42`. Since Task 2.1.1a already touches both, this
   revision adds a one-line dedup: a `UpstreamKind::label(&self)` method on the enum itself,
   with both free functions delegating to it.
3. **Adversarial — concern:** Phase 1's three acceptance criteria (default/override/typo-reject)
   had no test-writing task backing them. Added Task 1.1.1c.
4. **Adversarial — concern:** Phase 2's goal claimed to restore compilation, but omitted
   `router.rs:89` (deferred to Phase 3 since it's the one behavioral site). Reworded Phase 2's
   goal and the dependency diagram to state explicitly that the crate does not compile again
   until Phase 3 lands — Phases 1–3 ship as one atomic change, not incrementally.
5. **Adversarial — concern:** `AnthropicCountTokensEstimator::new` (`estimator.rs:276-287`)
   hardcodes `Self::DEFAULT_BASE_URL`, ignoring the upstream's configured `base_url` entirely.
   Confirmed via `grep -rn "AnthropicCountTokensEstimator::new" src/` that it has zero call
   sites today (only `with_base_url` is used, at `estimator.rs:475`) — not a live bug, but a
   latent trap for whoever wires cost estimation into the live per-request path next. Added
   Story 3.1.3 to fix it while this feature is already touching base_url plumbing.
6. **Adversarial — concern:** Neither Phase 4 test exercised `send_streaming_request` (the SSE
   path real clients predominantly use) — both tests only covered `send_request`. Added Task
   4.1.1c and Task 4.1.2b for the streaming path.
7. **Adversarial — concern:** The original gate's raw string comparison meant a trailing-slash
   typo on an otherwise-default `base_url` (e.g. `"https://api.anthropic.com/"`) would be
   misclassified as "custom," silently disabling Bedrock-id normalization against the *real*
   Anthropic API — a functional regression, not the cosmetic double-slash issue the plan
   originally framed it as. Fixed by trimming a trailing `/` once in `build_providers` before
   computing `is_default_endpoint` and before storing `base_url` — this also incidentally closes
   the double-slash URL wart for Anthropic specifically (openai's identical wart is untouched,
   see revised Tech Debt Disposition).

## Revision log 2 (post pre-mortem — /sdd:4-validate)

Pre-mortem returned 1 P1, 3 P2, 1 P3. Per the `/sdd:4-validate` readiness gate, the P1 is fixed
below (required to proceed); the cheapest two P2s are fixed alongside it since they sit in the
exact same code the P1 fix already touches; the remaining P2 and the P3 are noted but not
blocking.

8. **P1, fixed:** Story 3.1.1's original acceptance criteria asserted directly on
   `AnthropicProvider`'s private `base_url`/`is_default_endpoint` fields via `build_providers`'s
   return value — but `build_providers` returns `Vec<(String, Arc<dyn Provider>)>`, a trait
   object with no downcast and no public getters on `AnthropicProvider`. That criterion was
   literally unwritable as specified, and every Phase 4 test bypasses `build_providers` entirely
   by constructing `AnthropicProvider` directly — so the actual `router.rs:89-94` wiring had zero
   real test coverage. Fixed by extracting the trim-and-compare logic into a small, pure,
   directly-testable free function (`resolve_anthropic_endpoint`) and adding one true end-to-end
   test that goes through `build_providers` and the `Provider` trait's `send` method (not a
   hand-constructed `AnthropicProvider`). See revised Task 3.1.1b, new Task 3.1.1c, and new Story
   4.1.3.
9. **P2, fixed:** The prose in the original Pattern Decisions row said "trim exactly one trailing
   `/`," but Task 3.1.1b's own code sample used `trim_end_matches('/')`, which strips *all*
   trailing slashes, not just one — a `base_url = "https://host///"` prose/code mismatch that
   risked a future "fix" reintroducing the multi-slash misclassification bug item 7 above closed.
   Fixed by correcting the prose to match the (correct) code.
10. **P2, fixed:** `AnthropicCountTokensEstimator::new`'s Story 3.1.3 fix (added in Revision log
    item 5) copied `base_url` verbatim from `upstream.kind`, without applying the same
    trailing-slash trim `router.rs::build_providers` applies for `AnthropicProvider`. Dormant
    today (confirmed zero call sites for `new()`), but the two config-derived-`base_url` code
    paths would silently disagree — resurfacing the exact double-slash bug item 7 closed — the
    day `AnthropicCountTokensEstimator::new()` gets wired into a live path. Fixed by having Story
    3.1.3 reuse the same `resolve_anthropic_endpoint` helper (item 8) instead of a second,
    untrimmed copy.
11. **P2, not fixed (noted only, does not block):** No test integrates the trim/compare logic
    with an actual HTTP call in one scenario — `resolve_anthropic_endpoint`'s unit tests (Task
    3.1.1c) and the mock-server request tests (Phase 4) exercise trimming and request-honoring
    separately, never in the same test. The new Story 4.1.3 end-to-end test (item 8, above)
    substantially closes this by construction (it runs a real `base_url` with a trailing slash
    through both `resolve_anthropic_endpoint` *and* a live mock-server request in one test), so
    this is considered addressed as a side effect rather than needing its own separate task.
12. **P3, not fixed (noted only, does not block):** `is_default_endpoint`'s exact-string
    comparison has no scheme/case normalization (e.g. `HTTPS://API.ANTHROPIC.COM` or a bare
    `api.anthropic.com` without scheme would both be classified "custom" even though they'd
    resolve to the same host). Unlikely in practice (operators copy-paste the documented example
    verbatim) and not catastrophic (worst case is the existing, already-documented "stripping
    skipped" behavior firing when it technically shouldn't, not corruption) — deliberately left
    as a known limitation, matching the existing "no URL-shape validation" precedent from
    features.md #1. Follow-up if a real user hits it.
13. **Fixed during the readiness-gate criterion-8 layering check (not from either prior review or
    pre-mortem):** the P1 fix (item 8) originally placed `resolve_anthropic_endpoint` in
    `src/routing/router.rs`. Task 3.1.3a's reuse of it from `src/cost_metrics/estimator.rs` would
    then have created a new dependency edge from a lower-level module up into the
    routing/orchestration layer (which itself depends on `providers`) — for a two-line pure
    string function that has nothing to do with dispatch. Fixed by moving it to
    `src/config/schema.rs` instead, alongside `default_anthropic_base_url` (which it already
    calls) — both `router.rs` and `estimator.rs` already depend on `config::schema`, so this adds
    zero new cross-module edges. See revised Task 3.1.1b/3.1.1c/3.1.3a.

---

## Domain Glossary

This is a small, additive config-plumbing change. Genuinely new domain terms: 2.

| Term | Definition | Notes |
|------|-----------|-------|
| `base_url` (on `UpstreamKind::Anthropic`) | The Anthropic Messages API-compatible host this upstream sends requests to; TOML-configurable, defaults to `https://api.anthropic.com` when omitted. | Same name/shape as the existing `UpstreamKind::Openai { base_url: String }` field — not a new concept, a new field on an existing enum. |
| `default_anthropic_base_url` | The `#[serde(default = "...")]` function supplying `"https://api.anthropic.com"` when `base_url` is absent from TOML. | Lives in `src/config/schema.rs`, follows the same naming/shape convention as `default_apikey_header` (`src/config/schema.rs:9-11`). Also called once from `router.rs::build_providers` (not from `anthropic.rs`) to compute `is_default_endpoint` — see revised Pattern Decisions. |
| `is_default_endpoint` | A `bool` computed once in `build_providers`, `true` when the (trailing-slash-trimmed) configured `base_url` equals `default_anthropic_base_url()`. Stored on `AnthropicProvider`, gates `normalize_model_name`. | New in this revision (see Revision log item 1) — replaces a raw string comparison that previously lived inside `anthropic.rs`. |

Everything else touched (`AnthropicProvider::new`, `normalize_model_name`, `build_providers`) is an existing type/method gaining a parameter, not a new domain concept.

---

## Pattern Decisions

| Component | Pattern Chosen | Source | Alternative Rejected | Reason |
|-----------|---------------|--------|---------------------|--------|
| Config schema field shape for `UpstreamKind::Anthropic.base_url` | `String` + `#[serde(default = "default_anthropic_base_url")]` | stack.md §2 | `Option<String>` field with the `None`→default resolution done at the `router.rs::build_providers` call site (architecture.md §1) | stack.md and architecture.md disagree here; resolved in favor of `String`+default-fn because it matches the dominant in-repo idiom 13:1 (`default_apikey_header`, `default_cache_ttl_secs`, `default_exec_timeout_secs`, `default_port`, etc. — all "field has one concrete non-`None` default" cases use this shape, never `Option`), it needs no `unwrap_or_else` at the `router.rs:89` call site, and it keeps that arm's shape identical to the adjacent `Openai { base_url } => ... base_url.clone() ...` arm (`router.rs:98-105`) this feature is explicitly modeled on. `model_family`'s `Option<String>` (schema.rs:167-168) is a different case — it truly defaults to *absent*, not to a concrete fallback string. |
| Overall design pattern for the change | Transaction Script / plain config plumbing — no GoF/PoEAA pattern | architecture.md "Recommendation: EXTEND-AS-IS" | A dedicated `AnthropicEndpointConfig` value object, or a Builder for `AnthropicProvider` construction | The "endpoint" is one string with one default and zero validation rules (features.md #1: no URL-shape validation exists for `openai.base_url` either, and this change must not add asymmetric validation). Wrapping it in a new type adds an abstraction with no behavior to own, for a field that behaves identically to the already-bare-`String` `Openai::base_url`. Premature generalization. |
| `normalize_model_name` Bedrock-prefix/suffix stripping | Gate stripping on an `is_default_endpoint: bool` computed **once in `build_providers`** (not re-derived per-request inside `anthropic.rs`) and passed into `AnthropicProvider::new`; skip stripping when `false` | pitfalls.md #2, option (b), REVISED per architecture-review.md concern 1 | (original) raw string comparison against `default_anthropic_base_url()` performed inside `normalize_model_name` itself; (a) leave unconditional; (c) add a separate per-upstream opt-out config knob | The original in-provider string comparison coupled `anthropic.rs` to a config-module default value it had no other reason to know about, and was fragile: a trailing-slash variant of the default URL would silently misclassify as "custom" (see the trailing-slash row below). Computing the bool once where both the configured value and the default already live (`build_providers`) removes the cross-module coupling and, combined with trimming (below), removes the fragility. (a) risks corrupting a verbatim Bedrock-shaped model id sent to a non-default relay. (c) adds config surface for something already inferable from `base_url`. |
| Trailing-slash handling on `base_url` | Trim **all** trailing `/` characters (`str::trim_end_matches('/')`, so `"https://host///"` and `"https://host/"` both normalize to `"https://host"`) from the configured `base_url`, once, inside a new pure free function `resolve_anthropic_endpoint(base_url: &str) -> (String, bool)` called from `build_providers`, before passing the result into `AnthropicProvider::new` | Added in this revision per adversarial-review.md concern 7; corrected in Revision log 2 item 9 (an earlier draft of this row said "trim exactly one trailing `/`," which didn't match the `trim_end_matches` code sample even then — `trim_end_matches` strips *all* trailing matches, not one) | Leave untrimmed (original plan's stance, matching `openai.base_url`'s identical untrimmed wart); trim only one slash | For `openai`, an untrimmed trailing slash is purely cosmetic (a harmless double-slash URL). For `anthropic`, once `normalize_model_name`'s gate exists, an untrimmed trailing slash on an otherwise-default URL would be misclassified as a custom endpoint and silently disable Bedrock-id normalization against the real Anthropic API — a functional regression, not a cosmetic one. Trimming *all* trailing slashes (not just one) closes the `"https://host///"` case too, at no extra cost. Extracting the trim+compare into its own named function (rather than inlining it in the match arm) is also what makes it directly unit-testable without needing a `Provider` trait downcast — see Revision log 2 item 8. |
| Test harness for the new base-URL-honoring behavior | Anthropic gets its own small local mock-server helper inside `anthropic.rs`'s `#[cfg(test)] mod tests`, not shared with `openai`'s | build-vs-buy.md verdict; features.md §6 | Extract/generalize `openai/mod.rs`'s `start_capturing_chat_completions_server` (~line 1320) into a shared cross-module test-support helper | Anthropic's and OpenAI's canned response shapes genuinely differ (Anthropic `content` blocks vs OpenAI `choices`/`chatcmpl`); a shared helper would need parameterizing for exactly two call sites. ~25 lines of duplicated `TcpListener`/`axum::serve` boilerplate is cheaper than the abstraction. Matches build-vs-buy.md's explicit "adapt, don't necessarily share" verdict. |
| `upstream_kind_label` duplication | Add `UpstreamKind::label(&self) -> &'static str` on the enum in `schema.rs`; both `entrypoint/mod.rs` and `validate.rs`'s free functions delegate to it | Added in this revision per architecture-review.md concern 2 | Leave the byte-identical duplicate as-is | Both call sites are already being touched by Task 2.1.1a for the unrelated `{ .. }` pattern fix; deduplicating costs one extra method + two one-line delegate bodies while already in both files. |

---

## Tech Debt Disposition

| Area | Existing Issue | Disposition | Justification |
|------|----------------|--------------|----------------|
| `UpstreamKind::Anthropic` / `AnthropicProvider` construction | `Anthropic` was the only upstream kind with no `base_url` override, forcing users onto `kind = "openai"` or a fork for non-default endpoints | EXTEND-AS-IS (no refactor/isolation pass first) | architecture.md: "No existing SOLID/Clean-Architecture violation in the touched files worth fixing/isolating first — the enum/provider split is already the working pattern; extending it to a third variant removes an inconsistency... rather than adding complexity." `anthropic.rs` (606 lines, 7 commits) and `schema.rs` (534 lines, 7 commits) show no churn-hotspot signal. |
| `normalize_model_name` unconditional stripping | Strips Bedrock-shaped prefixes/suffixes from every model string regardless of destination, a real corruption risk once `base_url` is configurable | Fix now, in this change (gate on the precomputed `is_default_endpoint` bool — see revised Pattern Decisions) | pitfalls.md #2 flags this as a risk *created reachable* specifically by this feature — not pre-existing in the sense of "already shipped and load-bearing," so it doesn't get deferred like the two items below. |
| `format!("{}/v1/messages", base_url)` trailing-slash double-slash wart | A `base_url` ending in `/` produces `https://host//v1/messages`; shared identically by `openai.base_url` today | REVISED: Fix now, for `anthropic` only (trim once in `build_providers`, see Pattern Decisions); `openai.base_url`'s identical wart is untouched | Originally deferred as "not in scope" per features.md #2, but adversarial-review.md concern 7 showed the untrimmed case now has a functional consequence for `anthropic` (see the `normalize_model_name` gating row above) that `openai` doesn't share — trimming closes a real bug, not a cosmetic one, without touching `openai`'s still-cosmetic case. |
| `anthropic-version`/`anthropic-beta` header forwarding to non-Anthropic relays | Headers are sent unconditionally, now reachable against arbitrary hosts for the first time | Document only, don't gate | pitfalls.md #1: low severity, pre-existing "we always send Anthropic-shaped headers" assumption merely made reachable, not created. A per-upstream header allow/deny-list is a follow-up if someone actually hits it. |
| `AnthropicCountTokensEstimator::new` hardcodes `Self::DEFAULT_BASE_URL` (`estimator.rs:276-287`), ignoring `upstream.kind`'s configured `base_url` | A latent bug: `new()` has zero call sites in `src/` today (confirmed via grep — only `with_base_url` is used, at `estimator.rs:475`), so nothing is broken yet, but it will silently ignore an operator's override the day someone wires cost estimation into the live per-request path | Fix now (added in this revision per adversarial-review.md concern 5) | Cheap to fix while this feature is already touching `base_url` plumbing end-to-end (Story 3.1.3); leaving it would mean shipping a feature called "configurable base_url" alongside a second, adjacent constructor that silently doesn't honor it. |

---

## Migration Plan

This section covers the **config-schema shape change** (`UpstreamKind::Anthropic` unit variant → struct variant), not a database migration — this project has no DB. No migration guide or automated rewrite is needed:

- Existing `kind = "anthropic"` TOML blocks with no `base_url` key deserialize identically to today (serde `#[serde(default = "default_anthropic_base_url")]` fills in `"https://api.anthropic.com"`), satisfying the codebase's "completely unchanged for unset fields" convention (matches `model_family`'s existing precedent).
- The only breakage is at the **Rust API** level (constructor signatures for both `AnthropicProvider::new` and, per Story 3.1.3, `AnthropicCountTokensEstimator::new`; ~9-10 match/construction call sites), entirely internal to this crate and entirely caught by `cargo build` — not a runtime or user-facing migration concern. Note: the crate does not compile again until Phase 3 lands (see Phase 2's revised goal) — Phases 1-3 must ship as one atomic commit/PR, not landed incrementally.
- `tests/references_conf_d.rs` and `tests/toml_parity.rs` re-run unchanged against `references/conf.d/00-providers.toml`'s existing `kind = "anthropic"` block (no `base_url` there today) as a live regression check that the default path still parses cleanly.

## Observability Plan
- **Logs**: No new logging needed. `AnthropicProvider::send_request`/`send_streaming_request` already log `debug!("Anthropic non-stream POST {url}")` / `debug!("Anthropic stream POST {url}")` (anthropic.rs:214, 306), and `url` is built from `self.base_url` — so the configured host is already visible in existing debug logs once this ships, with zero new instrumentation.
- **Metrics**: None added. `openai.base_url` carries no dedicated metric label today (grepped, none found); matching that precedent, `anthropic.base_url` gets none either — avoids introducing an inconsistency between the two kinds.
- **Alerts**: None. No SLO or failure mode is introduced beyond the pre-existing "malformed base_url surfaces as a generic `reqwest::Error` at first-request time" behavior `openai.base_url` already has (pitfalls.md #3).

## Risk Control
- **Feature flag**: None needed. Purely additive/opt-in — an operator must explicitly set `base_url` in their own TOML to change behavior; every existing config is unaffected.
- **Rollback procedure**: Revert the commit. Because the default preserves old behavior byte-for-byte, there is no config or data state to unwind — a rollback is a plain code revert.
- **Staged rollout**: Not applicable. This is a self-service config knob for whoever edits their own `conf.d/*.toml`; there is no fleet-wide rollout mechanism in this project to stage across.

## Unresolved Questions
- [ ] Should a future non-standard-path escape hatch (à la LiteLLM's `LITELLM_ANTHROPIC_DISABLE_URL_SUFFIX`, added after real user reports of gateways proxying under non-`/v1/messages` paths — BerriAI/litellm#13945, #4803) be built? — **Deliberately deferred**, does not block any story in this plan (build-vs-buy.md + features.md §8: the fixed-suffix MVP is the right-sized first cut) — owner: whoever files that follow-up requirement if they hit a real gateway needing it.
- [x] ~~Trailing-slash `base_url` normalization (double-slash URLs) — pre-existing wart shared with `openai.base_url`, not blocking any story here~~ — **Resolved in this revision**: trimmed for `anthropic` in Story 3.1.1 (see Tech Debt Disposition). `openai.base_url`'s identical, still-cosmetic wart remains out of scope.

## Dependency Visualization

```
Phase 1: Schema
  Epic 1.1 / Story 1.1.1
    Task 1.1.1a (enum field + default fn) ──> Task 1.1.1b (Config::default() fixture)
                        │                          │
                        └────────────┬─────────────┘
                                     ▼
                    Task 1.1.1c (default/override/typo-reject unit tests)
                        │
                        ▼
Phase 2: Compiler-driven call-site fixups, EXCLUDING router.rs:89 (the one
         behavioral site — deferred to Phase 3, see its revised Goal below).
         NOTE: the crate does NOT compile again until Phase 3 lands.
  Epic 2.1
    Story 2.1.1 (label/no-op arms + upstream_kind_label dedup)   Story 2.1.2 (remaining test fixtures)
                        │                                                       │
                        └───────────────────────────┬───────────────────────────┘
                                                     ▼
Phase 3: Provider constructor + behavior (depends on Phase 2; crate compiles
         clean again only once this phase lands)
  Epic 3.1
    Story 3.1.1 (resolve_anthropic_endpoint pure fn in schema.rs + unit tests +
                 constructor params [base_url, is_default_endpoint] + router.rs:89 wiring)
                        │
                        ▼
    Story 3.1.2 (gate normalize_model_name on the passed-in is_default_endpoint bool)
                        │
                        ▼
    Story 3.1.3 (fix AnthropicCountTokensEstimator::new's hardcoded DEFAULT_BASE_URL,
                 reusing resolve_anthropic_endpoint)
                        │
                        ▼
Phase 4: Test coverage (depends on Phase 3)
  Epic 4.1
    Story 4.1.1 (mock-server: base_url honored — send_request AND send_streaming_request)
    Story 4.1.2 (normalize_model_name gating regression test — both request paths)
    Story 4.1.3 (end-to-end: build_providers itself, via the Provider trait — closes
                 the pre-mortem P1 that Phase 4's other tests all bypass build_providers)

Phase 5: Documentation (independent — can run any time after Phase 1)
  Epic 5.1
    Story 5.1.1 (reference conf.d example comment)
```

---

## Phase 1: Schema

### Epic 1.1: Config schema field
**Goal**: `UpstreamKind::Anthropic` becomes a struct variant carrying an optional, defaulted `base_url`.

#### Story 1.1.1: `UpstreamKind::Anthropic` gains a defaulted `base_url`
**As a** consolette operator, **I want** to set `base_url` on a `kind = "anthropic"` upstream, **so that** I can point it at an internal Anthropic-compatible relay instead of `api.anthropic.com`.

**Acceptance Criteria**:
- Omitting `base_url` from a `kind = "anthropic"` TOML block deserializes to `"https://api.anthropic.com"`, unchanged from today.
  - *Given* the TOML fragment
    ```toml
    [[upstreams]]
    name = "anthropic"
    kind = "anthropic"

    [upstreams.auth]
    type = "bearer"

    [upstreams.auth.token]
    source = "env"
    var = "CLAUDE_CODE_OAUTH_TOKEN"
    ```
  *When* it is parsed via `toml::from_str::<Config>`, *Then* `upstreams[0].kind` equals `UpstreamKind::Anthropic { base_url: "https://api.anthropic.com".to_string() }`.
- Setting `base_url` explicitly overrides the default.
  - *Given* the TOML fragment
    ```toml
    [[upstreams]]
    name = "internal-claude-gateway"
    kind = "anthropic"
    base_url = "https://gateway.internal.example/anthropic"

    [upstreams.auth]
    type = "bearer"

    [upstreams.auth.token]
    source = "env"
    var = "GATEWAY_TOKEN"
    ```
  *When* it is parsed, *Then* `upstreams[0].kind` equals `UpstreamKind::Anthropic { base_url: "https://gateway.internal.example/anthropic".to_string() }`.
- An unrecognized field on an `anthropic`-kind block is still rejected (no regression to `deny_unknown_fields` typo protection).
  - *Given* a `kind = "anthropic"` block with a stray `bas_url = "typo"` line, *When* parsed, *Then* `toml::from_str::<Config>` returns an error naming the unknown field (unchanged `deny_unknown_fields` behavior on `UpstreamKind`).

**Files**: `src/config/schema.rs`

##### Task 1.1.1a: Add default fn and convert `Anthropic` to a struct variant (~4 min)
- In `src/config/schema.rs`, near the other `default_*` functions (after `fn default_apikey_header` at lines 9-11), add:
  ```rust
  pub(crate) fn default_anthropic_base_url() -> String {
      "https://api.anthropic.com".to_string()
  }
  ```
  (`pub(crate)`, not private like the other `default_*` fns, because Task 3.1.1b needs to call it from `src/routing/router.rs::build_providers` to compute `is_default_endpoint` — this is the one place this feature's default-fn precedent diverges from the other 12 `default_*` fns, which are all serde-only and stay private. Revised in this pass: earlier drafts had `anthropic.rs` itself calling this fn directly; architecture-review.md concern 1 moved that comparison to `build_providers` instead, so the cross-module reach is now router→schema, not provider→schema.)
- Change line 108 from `Anthropic,` to:
  ```rust
  Anthropic {
      #[serde(default = "default_anthropic_base_url")]
      base_url: String,
  },
  ```
- Files: `src/config/schema.rs`

##### Task 1.1.1b: Update `Config::default()`'s Anthropic fixture (~2 min)
- At `src/config/schema.rs:353`, change `kind: UpstreamKind::Anthropic,` to `kind: UpstreamKind::Anthropic { base_url: default_anthropic_base_url() },` (same file as Task 1.1.1a, so the fn is already in scope).
- Files: `src/config/schema.rs`

##### Task 1.1.1c: Add unit tests backing Story 1.1.1's three acceptance criteria (~5 min)
- Added in this revision — the original plan named these three criteria (default, override,
  typo-rejection) but had no task writing the tests. In `src/config/schema.rs`'s `#[cfg(test)]
  mod tests`, add:
  - `anthropic_upstream_should_default_base_url_when_omitted()` — parses the Story 1.1.1
    "omitted" TOML fixture via `toml::from_str::<Config>`, asserts
    `upstreams[0].kind == UpstreamKind::Anthropic { base_url: default_anthropic_base_url() }`.
  - `anthropic_upstream_should_honor_an_explicit_base_url()` — parses the Story 1.1.1
    "override" TOML fixture, asserts the parsed `base_url` matches the configured value verbatim.
  - `anthropic_upstream_should_reject_an_unknown_field_via_deny_unknown_fields()` — parses a
    `kind = "anthropic"` block with a stray `bas_url = "typo"` line, asserts
    `toml::from_str::<Config>` returns `Err`.
- Files: `src/config/schema.rs`

---

## Phase 2: Compiler-driven call-site fixups

### Epic 2.1: Fix every non-behavioral `UpstreamKind::Anthropic` site
**Goal**: Revised in this pass (adversarial-review.md concern 4) — this phase fixes every
match/construction site that needs only a pattern-shape update (`{ .. }` or a fixture literal),
which is every site *except* `router.rs:89`. That one site is deliberately deferred to Phase 3
because destructuring it also requires the new behavioral wiring (passing `base_url`/
`is_default_endpoint` into the constructor), not just a shape fix — bundling it into Phase 2
would misleadingly suggest the crate compiles again after this phase. **It does not**: `cargo
build` still fails on `router.rs:89` until Phase 3 lands. All breakage in this phase is
compiler-caught, not runtime-hidden (pitfalls.md #5).

#### Story 2.1.1: Fix bare-unit match arms, and dedup the label helper
**As a** maintainer, **I want** the label/no-op match arms updated to `{ .. }` and the duplicate label helper collapsed into one method, **so that** the crate compiles with no behavior change and one less byte-identical duplicate.

**Acceptance Criteria**:
- `cargo build` no longer errors on non-exhaustive/mismatched-pattern for `UpstreamKind::Anthropic` in these three files.
  - *Given* `src/server_tools/mod.rs:217`'s `Some(UpstreamKind::Anthropic) => {}`, *When* changed to `Some(UpstreamKind::Anthropic { .. }) => {}`, *Then* the no-op reachability-probe skip behavior for Anthropic upstreams is unchanged.
  - *Given* the new `UpstreamKind::label(&self)` method added to `schema.rs` (Task 2.1.1a), *When* called with `UpstreamKind::Anthropic { base_url: "https://x".to_string() }`, *Then* it returns `"anthropic"` — and `src/entrypoint/mod.rs` and `src/config/validate.rs`'s call sites (`entrypoint/mod.rs:151,325,333`, `validate.rs:88`) produce identical output to today after switching to it.
**Files**: `src/entrypoint/mod.rs`, `src/config/validate.rs`, `src/config/schema.rs`, `src/server_tools/mod.rs`

##### Task 2.1.1a: Add `{ .. }` to the three label/no-op arms, and dedup `upstream_kind_label` (~6 min)
- `src/server_tools/mod.rs:217`: `Some(UpstreamKind::Anthropic) => {}` → `Some(UpstreamKind::Anthropic { .. }) => {}`
- Added in this revision (architecture-review.md concern 2): `src/entrypoint/mod.rs:171-179` and
  `src/config/validate.rs:42-50` currently define byte-identical `upstream_kind_label` free
  functions. Add a method on the enum itself in `src/config/schema.rs`:
  ```rust
  impl UpstreamKind {
      pub fn label(&self) -> &'static str {
          match self {
              UpstreamKind::Anthropic { .. } => "anthropic",
              UpstreamKind::Bedrock { .. } => "bedrock",
              UpstreamKind::Openai { .. } => "openai",
              UpstreamKind::Gemini { .. } => "gemini",
              UpstreamKind::Openrouter {} => "openrouter",
          }
      }
  }
  ```
  Then replace both free functions' bodies with `kind.label()` (or remove them and update their
  call sites — `validate.rs:88`, `entrypoint/mod.rs:151,325,333` — to call `.label()` directly;
  either is fine, removing the now-redundant free functions is preferred since they'd otherwise
  be a second, now-pointless layer).
- Files: `src/entrypoint/mod.rs`, `src/config/validate.rs`, `src/config/schema.rs`, `src/server_tools/mod.rs`

#### Story 2.1.2: Fix remaining test-fixture construction sites
**As a** maintainer, **I want** every test-only `UpstreamKind::Anthropic` construction updated to supply `base_url`, **so that** existing tests keep compiling and keep exercising the default-URL path.

**Acceptance Criteria**:
- All pre-existing tests in `src/cost_metrics/estimator.rs` and `src/routing/router.rs` compile and pass unchanged in behavior.
  - *Given* `src/cost_metrics/estimator.rs:464`'s `kind: UpstreamKind::Anthropic,` inside `test_upstream()`, *When* changed to `kind: UpstreamKind::Anthropic { base_url: "https://api.anthropic.com".to_string() },`, *Then* `AnthropicCountTokensEstimator`'s existing tests (which never inspect `base_url`) pass unchanged.
  - *Given* `src/routing/router.rs:1865`'s struct-literal `kind: UpstreamKind::Anthropic,` and the bare-value uses at lines 1908, 2092, 2144 (`UpstreamKind::Anthropic` passed to `bearer_upstream(name, kind, token)`), *When* each is updated to `UpstreamKind::Anthropic { base_url: "https://api.anthropic.com".to_string() }`, *Then* `cargo test --lib routing::router` passes with the same assertions as before (these tests never inspect `base_url`).
**Files**: `src/cost_metrics/estimator.rs`, `src/routing/router.rs`

##### Task 2.1.2a: Update `estimator.rs`'s and `router.rs`'s test fixtures (~5 min)
- `src/cost_metrics/estimator.rs:464`: add `base_url: "https://api.anthropic.com".to_string()` to the struct literal.
- `src/routing/router.rs:1865`: same, struct literal.
- `src/routing/router.rs:1908, 2092, 2144`: change bare `UpstreamKind::Anthropic` to `UpstreamKind::Anthropic { base_url: "https://api.anthropic.com".to_string() }` at each of the three `bearer_upstream(...)` call sites.
- Files: `src/cost_metrics/estimator.rs`, `src/routing/router.rs`

---

## Phase 3: Provider constructor + behavior

### Epic 3.1: `AnthropicProvider` takes and honors a configurable `base_url`
**Goal**: `AnthropicProvider::new` accepts `base_url: String` exactly like `OpenaiProvider::new`,
plus a precomputed `is_default_endpoint: bool` (revised in this pass — see Revision log item 1),
both wired from `build_providers`, with the Bedrock-model-id-normalization risk (pitfalls.md #2)
closed via gating on that bool rather than a per-request string comparison.

#### Story 3.1.1: Constructor parameters + `router.rs` wiring
**As a** consolette operator, **I want** my configured `base_url` to actually reach the outgoing HTTP requests, **so that** `kind = "anthropic"` upstreams hit my configured host instead of always hitting `api.anthropic.com`.

**Acceptance Criteria**:
- `AnthropicProvider::new` takes `base_url: String` and `is_default_endpoint: bool` as its 2nd
  and 3rd positional parameters.
  - *Given* a call `AnthropicProvider::new(Arc::new(upstream), "https://gateway.internal.example/anthropic".to_string(), false, resolver, exec_cache, 30)`, *When* `send_request` is later called, *Then* the outgoing POST URL is `https://gateway.internal.example/anthropic/v1/messages`.
- **Revised in Revision log 2 (item 8)** — the original two criteria here asserted directly on
  `AnthropicProvider`'s private fields via `build_providers`'s return value, which is
  unwritable: `build_providers` returns `Arc<dyn Provider>` trait objects with no downcast and
  no public getters (verified: `src/providers/mod.rs:155`'s `Provider` trait has no
  `as_any`/getter methods). Replaced with two criteria that are actually writable:
  - A new pure function, `resolve_anthropic_endpoint(base_url: &str) -> (String, bool)`, trims
    all trailing `/` and compares to the default, with no dependency on `Provider` or
    `build_providers` at all.
    - *Given* `resolve_anthropic_endpoint("https://gateway.internal.example/anthropic/")`, *When* called, *Then* it returns `("https://gateway.internal.example/anthropic".to_string(), false)`.
    - *Given* `resolve_anthropic_endpoint("https://api.anthropic.com/")` (default host, trailing-slash typo), *When* called, *Then* it returns `("https://api.anthropic.com".to_string(), true)` — the typo does NOT cause a false "custom endpoint" classification (the specific regression adversarial-review.md concern 7 flagged).
  - `build_providers`'s `Anthropic` arm calls `resolve_anthropic_endpoint` and passes both
    results through to `AnthropicProvider::new` — verified observably (not via private-field
    inspection) by Story 4.1.3's end-to-end test, which drives the resulting `Arc<dyn Provider>`
    through its `Provider::send` trait method against a real mock server.
- The struct field's doc comment and the module-level doc comment no longer claim the base URL is hardcoded/a "gap."
  - *Given* `src/providers/anthropic.rs`'s current module doc (lines 3-4) and struct field doc (lines 53-56: "carries no base-URL override field... this is hardcoded... See the final port report for this gap"), *When* this story lands, *Then* neither comment references a hardcoded value or an unclosed gap; the `base_url` field carries no doc comment at all (matching `OpenaiProvider`'s own undocumented `base_url` field).
**Files**: `src/providers/anthropic.rs`, `src/routing/router.rs`, `src/config/schema.rs`

##### Task 3.1.1a: Change `AnthropicProvider::new`'s signature and body (~6 min)
- In `src/providers/anthropic.rs`, change the signature at lines 82-87 from:
  ```rust
  pub fn new(
      upstream: Arc<Upstream>,
      resolver: Arc<dyn SecretResolver + Send + Sync>,
      exec_cache: Arc<ExecCredentialCache>,
      request_timeout_secs: u64,
  ) -> Result<Self, ProviderError> {
  ```
  to:
  ```rust
  pub fn new(
      upstream: Arc<Upstream>,
      base_url: String,
      is_default_endpoint: bool,
      resolver: Arc<dyn SecretResolver + Send + Sync>,
      exec_cache: Arc<ExecCredentialCache>,
      request_timeout_secs: u64,
  ) -> Result<Self, ProviderError> {
  ```
- Add a new field to the `AnthropicProvider` struct: `is_default_endpoint: bool,` (next to
  `base_url: String,`).
- Change the struct-literal `base_url: "https://api.anthropic.com".to_string(),` (in the `Ok(Self { .. })` body) to `base_url, is_default_endpoint,`.
- Delete the struct field's doc comment at lines 53-56 (leave `base_url: String,` undocumented, matching `OpenaiProvider`).
- Update the module-level doc comment (lines 3-4) to stop asserting a fixed host, e.g.: "Forwards requests to the configured `base_url`'s `/v1/messages` (defaulting to `https://api.anthropic.com`) unchanged, per the gateway compatibility guide's 'Feature pass-through' contract."
- Files: `src/providers/anthropic.rs`

##### Task 3.1.1b: Add `resolve_anthropic_endpoint` (in `schema.rs`) and wire `router.rs`'s `Anthropic` arm (~6 min)
- Revised in this pass (Revision log 2 item 8: extracted into a named, independently-testable
  function instead of being inlined in the match arm — the inline version was the source of the
  pre-mortem's P1 finding). **Revised again during the `/sdd:4-validate` readiness gate's
  criterion-8 layering check (Revision log 2 item 13)**: put this function in
  `src/config/schema.rs`, next to `default_anthropic_base_url` (which it already calls), not in
  `src/routing/router.rs`. Both `router.rs` and `estimator.rs` already depend on
  `config::schema`; if it lived in `router.rs` instead, Task 3.1.3a's reuse from
  `cost_metrics::estimator` would create a new dependency edge from a lower-level module up into
  the routing/orchestration layer (which itself depends on `providers`) — an avoidable layering
  smell for a two-line pure string function. In `src/config/schema.rs`, near
  `default_anthropic_base_url`, add:
  ```rust
  pub(crate) fn resolve_anthropic_endpoint(base_url: &str) -> (String, bool) {
      let trimmed = base_url.trim_end_matches('/').to_string();
      let is_default = trimmed == default_anthropic_base_url();
      (trimmed, is_default)
  }
  ```
- In `src/routing/router.rs:89-94`, change:
  ```rust
  UpstreamKind::Anthropic => Arc::new(AnthropicProvider::new(
      Arc::new(upstream.clone()),
      Arc::clone(&resolver),
      Arc::clone(&exec_cache),
      config.request_timeout,
  )?),
  ```
  to:
  ```rust
  UpstreamKind::Anthropic { base_url } => {
      let (resolved_base_url, is_default_endpoint) =
          crate::config::schema::resolve_anthropic_endpoint(base_url);
      Arc::new(AnthropicProvider::new(
          Arc::new(upstream.clone()),
          resolved_base_url,
          is_default_endpoint,
          Arc::clone(&resolver),
          Arc::clone(&exec_cache),
          config.request_timeout,
      )?)
  }
  ```
  This is the one site in the crate that decides "is this endpoint the real Anthropic API,"
  computed once at provider-construction time rather than re-derived per request inside
  `anthropic.rs` — closing architecture-review.md concern 1.
- Files: `src/config/schema.rs`, `src/routing/router.rs`

##### Task 3.1.1c: Unit-test `resolve_anthropic_endpoint` directly (~4 min)
- Added in this revision (Revision log 2 item 8, closing the pre-mortem's P1). In
  `src/config/schema.rs`'s existing `#[cfg(test)] mod tests`, add:
  - `resolve_anthropic_endpoint_should_trim_all_trailing_slashes_and_flag_a_custom_host()` —
    asserts `resolve_anthropic_endpoint("https://gateway.internal.example/anthropic///")` returns
    `("https://gateway.internal.example/anthropic".to_string(), false)`.
  - `resolve_anthropic_endpoint_should_not_misclassify_a_trailing_slash_typo_on_the_default_host()`
    — asserts `resolve_anthropic_endpoint("https://api.anthropic.com/")` returns
    `("https://api.anthropic.com".to_string(), true)`.
  - `resolve_anthropic_endpoint_should_flag_the_exact_default_host_with_no_trailing_slash()` —
    asserts `resolve_anthropic_endpoint("https://api.anthropic.com")` returns
    `("https://api.anthropic.com".to_string(), true)`.
  These require no `Provider` trait object, no mock server, and no downcast — they test the
  pure function directly, which is what makes this P1 finding fixable at all.
- Files: `src/config/schema.rs`

#### Story 3.1.2: Gate `normalize_model_name` on the precomputed `is_default_endpoint`
**As a** consolette operator pointing `kind = "anthropic"` at a non-default relay, **I want** Bedrock-shaped model ids left untouched, **so that** a relay expecting a verbatim `us.anthropic.*` id doesn't get a silently corrupted request.

**Acceptance Criteria**:
- When `is_default_endpoint` is `true`, stripping behavior is unchanged from today.
  - *Given* `is_default_endpoint == true` and a request body with `"model": "us.anthropic.claude-3-5-sonnet-20241022-v1:0"`, *When* `send_request` runs, *Then* the outgoing body's `model` is normalized to `"claude-3-5-sonnet-20241022"` (unchanged from current behavior).
- When `is_default_endpoint` is `false`, stripping is skipped.
  - *Given* `is_default_endpoint == false` and the same request body, *When* `send_request` OR `send_streaming_request` runs, *Then* the outgoing body's `model` is forwarded verbatim as `"us.anthropic.claude-3-5-sonnet-20241022-v1:0"`, unmodified — **both** request paths, not just `send_request` (adversarial-review.md concern 6: the original criteria only named `send_request`, leaving the SSE path — the one real clients predominantly use — unverified).
**Files**: `src/providers/anthropic.rs`

##### Task 3.1.2a: Add the `is_default_endpoint` parameter and gating check (~5 min)
- Revised in this pass (architecture-review.md concern 1): the gate no longer takes `base_url`
  or performs a string comparison — it takes the precomputed bool instead. In
  `src/providers/anthropic.rs`, change `normalize_model_name`'s signature (line ~124) from
  `fn normalize_model_name(model: &str) -> String` to `fn normalize_model_name(model: &str, is_default_endpoint: bool) -> String`, with a leading guard:
  ```rust
  fn normalize_model_name(model: &str, is_default_endpoint: bool) -> String {
      if !is_default_endpoint {
          return model.to_string();
      }
      // ... existing strip_prefix/regex body unchanged ...
  }
  ```
  This removes the need for `default_anthropic_base_url` to be visible from `anthropic.rs` at
  all — it's now only called from `router.rs::build_providers` (Task 3.1.1b), so it can stay
  `pub(crate)` for that one cross-module call rather than being reached into from request-time
  logic in a different provider module.
- Update the doc comment above it to note the gating (one line: "Only applied when constructed
  against the default `api.anthropic.com` host — see plan.md Story 3.1.2.").
- Update both call sites: `send_request` (line ~220) → `Self::normalize_model_name(model, self.is_default_endpoint)`; `send_streaming_request` (line ~295) → same.
- Files: `src/providers/anthropic.rs`

##### Task 3.1.2b: Update the two existing `normalize_model_name` unit tests (~2 min)
- In `src/providers/anthropic.rs`'s `mod tests` (lines ~585-599), update both existing calls to pass a bool, e.g. `AnthropicProvider::normalize_model_name("us.anthropic.claude-3-5-sonnet-20241022-v1:0", true)`.
- Files: `src/providers/anthropic.rs`

#### Story 3.1.3: Fix `AnthropicCountTokensEstimator::new`'s hardcoded `DEFAULT_BASE_URL`
Added in this revision (adversarial-review.md concern 5). **As a** maintainer, **I want**
`AnthropicCountTokensEstimator::new` to derive its base URL from the upstream's configured
`kind` instead of a hardcoded constant, **so that** the day someone wires cost estimation into
the live per-request path, it doesn't silently ignore an operator's `base_url` override.

**Acceptance Criteria**:
- `new()` uses the upstream's configured, trimmed `base_url` when its `kind` is `UpstreamKind::Anthropic`.
  - *Given* `AnthropicCountTokensEstimator::new(Arc::new(Upstream { kind: UpstreamKind::Anthropic { base_url: "https://gateway.internal.example/anthropic/".to_string() }, .. }), resolver, exec_cache)` (note trailing slash), *When* constructed, *Then* the resulting estimator's `base_url` field is `"https://gateway.internal.example/anthropic"` (trimmed), not `Self::DEFAULT_BASE_URL` and not the untrimmed input.
  - Confirmed via `grep -rn "AnthropicCountTokensEstimator::new" src/` that this constructor has
    zero call sites in `src/` today (only `with_base_url`, at `estimator.rs:475`, is used) — this
    fix has no observable behavior change today; it closes a latent trap for the next caller.
**Files**: `src/cost_metrics/estimator.rs`

##### Task 3.1.3a: Derive `base_url` from `upstream.kind` in `new()`, reusing `resolve_anthropic_endpoint` (~4 min)
- Revised in this pass (Revision log 2 item 10): the original version of this task copied
  `base_url` verbatim, with no trim — which would have silently disagreed with
  `router.rs::build_providers`'s trimmed value the moment this constructor gets a real caller.
  In `src/cost_metrics/estimator.rs:276-287`, change `new()`'s body from unconditionally calling
  `Self::with_base_url(upstream, resolver, exec_cache, Self::DEFAULT_BASE_URL.to_string())` to
  extracting and trimming the configured value via the same helper Task 3.1.1b added:
  ```rust
  pub fn new(
      upstream: Arc<Upstream>,
      resolver: Arc<dyn SecretResolver + Send + Sync>,
      exec_cache: Arc<ExecCredentialCache>,
  ) -> Self {
      let base_url = match &upstream.kind {
          UpstreamKind::Anthropic { base_url } => {
              crate::config::schema::resolve_anthropic_endpoint(base_url).0
          }
          _ => Self::DEFAULT_BASE_URL.to_string(),
      };
      Self::with_base_url(upstream, resolver, exec_cache, base_url)
  }
  ```
  (The `_ =>` fallback covers the theoretical case of this estimator being constructed for a
  non-`anthropic`-kind upstream, which doesn't happen anywhere in `src/` today but keeps the
  function total rather than panicking. `is_default_endpoint` — the second element of
  `resolve_anthropic_endpoint`'s tuple — isn't needed here, since this estimator doesn't call
  `normalize_model_name`.)
- Files: `src/cost_metrics/estimator.rs`

---

## Phase 4: Test coverage

### Epic 4.1: Regression tests proving `base_url` is actually honored
**Goal**: Close pitfalls.md #4 — "MUST ADD a mock-server test," since a unit test on deserialization alone proves nothing about the outgoing request.

#### Story 4.1.1: Mock-server test proving the configured `base_url` is used
**As a** future maintainer, **I want** an integration-shaped test covering both request paths, **so that** a regression in `base_url` wiring (e.g. someone reverting Task 3.1.1b) is caught by `cargo test`, not discovered in production.

**Acceptance Criteria**:
- A local mock server bound to `127.0.0.1:0` receives the request at the configured host when `AnthropicProvider` is pointed at it — for **both** `send_request` and `send_streaming_request` (revised in this pass per adversarial-review.md concern 6; the original criteria only covered `send_request`, leaving the SSE path — the one real clients predominantly use — unverified).
  - *Given* a mock server listening at `http://127.0.0.1:54321` (actual port assigned by the OS) that captures the request path and body, and an `AnthropicProvider` constructed via `AnthropicProvider::new(upstream, "http://127.0.0.1:54321".to_string(), true, resolver, exec_cache, 30)`, *When* `provider.send_request(json!({"model": "claude-3-5-sonnet-20241022", "messages": []}), &HeaderMap::new())` is awaited, *Then* the mock server's captured request path is `/v1/messages` and the captured body's `"model"` field is `"claude-3-5-sonnet-20241022"`.
  - *Given* the same provider, *When* `provider.send_streaming_request(json!({"model": "claude-3-5-sonnet-20241022", "messages": []}), &HeaderMap::new())` is awaited instead, *Then* the mock server's captured request path is also `/v1/messages` (note: `send_streaming_request` returns the raw `reqwest::Response` for the caller to drive as an SSE byte stream — `anthropic.rs:276-278` — so the mock server's response body doesn't need to be real SSE; any `200 OK` response is sufficient to let the test assert on the captured *request*).
**Files**: `src/providers/anthropic.rs`

##### Task 4.1.1a: Add a local Anthropic mock-server test helper (~5 min)
- In `src/providers/anthropic.rs`'s `mod tests` (after line 599), add a `TcpListener`/`axum::serve`-based helper modeled on `openai/mod.rs`'s `start_capturing_chat_completions_server` (~line 1320), but Anthropic-shaped:
  ```rust
  async fn start_capturing_messages_server() -> (
      String,
      Arc<std::sync::Mutex<Option<(String, Value)>>>, // (path, body)
      tokio::task::JoinHandle<()>,
  ) { /* axum::Router with POST /v1/messages capturing (uri path, json body),
         returning a canned {"id":"msg_test","type":"message","role":"assistant",
         "content":[{"type":"text","text":"ok"}],"model":"claude-3-5-sonnet-20241022",
         "usage":{"input_tokens":1,"output_tokens":1}} */ }
  ```
- Files: `src/providers/anthropic.rs`

##### Task 4.1.1b: Add the base-URL-honoring regression test for `send_request` (~4 min)
- Add `#[tokio::test] async fn send_request_should_hit_the_configured_base_url()` constructing an `AnthropicProvider` via `AnthropicProvider::new` pointed at the mock server's address (with `AuthMethod::Bearer { token: SecretRef::Inline { value: "sk-ant-test".to_string() } }`, matching the `bearer_upstream` test-fixture convention already used in `router.rs`'s tests, and `is_default_endpoint: true`), calling `send_request` with a minimal body, and asserting the captured `(path, body)` is `("/v1/messages", <the sent body>)`.
- Files: `src/providers/anthropic.rs`

##### Task 4.1.1c: Add the base-URL-honoring regression test for `send_streaming_request` (~3 min)
- Added in this revision (adversarial-review.md concern 6). Add `#[tokio::test] async fn send_streaming_request_should_hit_the_configured_base_url()`, reusing the Task 4.1.1a mock server and the same provider setup as Task 4.1.1b, calling `send_streaming_request` instead and asserting the captured request path is `/v1/messages` (response body assertions aren't needed — see Story 4.1.1's second acceptance criterion).
- Files: `src/providers/anthropic.rs`

#### Story 4.1.2: Regression test for the `normalize_model_name` gating decision
**As a** future maintainer, **I want** a test proving the gating in Story 3.1.2 actually works end-to-end for both request paths, **so that** the corruption risk pitfalls.md #2 identified stays closed.

**Acceptance Criteria**:
- A non-default `base_url` leaves a Bedrock-shaped model id untouched through the full `send_request` path.
  - *Given* the mock server from Task 4.1.1a and an `AnthropicProvider` pointed at it with `is_default_endpoint: false`, *When* `send_request` is called with `"model": "us.anthropic.claude-3-5-sonnet-20241022-v1:0"`, *Then* the mock server's captured body has `"model": "us.anthropic.claude-3-5-sonnet-20241022-v1:0"` (unstripped).
  - *Given* the same provider, *When* `send_streaming_request` is called instead with the same body, *Then* the captured body is likewise unstripped (added in this revision per adversarial-review.md concern 6).
**Files**: `src/providers/anthropic.rs`

##### Task 4.1.2a: Add the gating regression test for `send_request` (~4 min)
- Add `#[tokio::test] async fn send_request_should_not_normalize_model_name_when_endpoint_is_not_default()` reusing the Task 4.1.1a mock server, constructing the provider with `is_default_endpoint: false`, asserting the captured body's `model` field is the unstripped Bedrock-shaped string.
- Files: `src/providers/anthropic.rs`

##### Task 4.1.2b: Add the gating regression test for `send_streaming_request` (~3 min)
- Added in this revision (adversarial-review.md concern 6). Add `#[tokio::test] async fn send_streaming_request_should_not_normalize_model_name_when_endpoint_is_not_default()`, same setup as Task 4.1.2a but calling `send_streaming_request`, asserting the captured body's `model` field is unstripped.
- Files: `src/providers/anthropic.rs`

#### Story 4.1.3: End-to-end test that `build_providers` itself wires `base_url` correctly
Added in this revision (Revision log 2 item 8, closing the pre-mortem's P1). **As a** future
maintainer, **I want** a test that exercises the real `router.rs::build_providers` construction
path — not a hand-built `AnthropicProvider` — **so that** a regression in the match arm's wiring
(e.g. someone forgetting to call `resolve_anthropic_endpoint`, or swapping its two return values)
is caught by `cargo test`, since every other Phase 4 test bypasses `build_providers` entirely.

**Acceptance Criteria**:
- Constructing providers via `build_providers` with a custom `base_url` produces a provider that
  actually sends requests to that host, verified through the `Provider` trait's public `send`
  method (not through private-field inspection, which isn't possible on a trait object).
  - *Given* a `Config` whose only `upstreams` entry is `Upstream { kind: UpstreamKind::Anthropic { base_url: "<mock-server-address>/".to_string() }, .. }` (trailing slash included, pointed at the Task 4.1.1a mock server) with bearer auth, *When* `build_providers(&config, metrics)` runs and `.send(json!({"model": "claude-3-5-sonnet-20241022", "messages": []}), HeaderMap::new(), false)` is called on the resulting `Arc<dyn Provider>`, *Then* the mock server's captured request path is `/v1/messages` at the trimmed host (proving both the trim and the constructor wiring worked end-to-end, through the real dispatch path).
**Files**: `src/routing/router.rs`

##### Task 4.1.3a: Add the `build_providers` end-to-end wiring test (~5 min)
- In `src/routing/router.rs`'s existing `mod tests`, add
  `#[tokio::test] async fn build_providers_should_wire_a_configured_anthropic_base_url_through_to_a_real_request()`,
  using the Task 4.1.1a mock-server helper (re-exported or duplicated as needed — `router.rs`'s
  test module doesn't currently depend on `anthropic.rs`'s test module) and a minimal `Config`
  fixture matching the existing `bearer_upstream`-style helpers already in this file. Assert on
  the mock server's captured request, not on any field of the returned provider.
- Files: `src/routing/router.rs`

---

## Phase 5: Documentation

### Epic 5.1: Reference example discoverability
**Goal**: Anyone copying the canonical `references/conf.d/00-providers.toml` example (the file `tests/references_conf_d.rs` already treats as "the copy-pasteable example showing all... upstream kinds side by side") can discover the new field without reading source.

#### Story 5.1.1: Mention the optional `base_url` override in the reference file
**As a** new consolette operator reading `references/conf.d/00-providers.toml`, **I want** to see that `kind = "anthropic"` supports an optional `base_url`, **so that** I don't have to read `schema.rs` to discover it.

**Acceptance Criteria**:
- The reference file documents the override without changing its parsed shape (still exactly 4 upstreams, `anthropic` first, no `base_url` line active by default).
  - *Given* `references/conf.d/00-providers.toml`'s existing `anthropic` block (lines 22-31, no `base_url` line), *When* a commented-out example line is added below it (e.g. `# base_url = "https://gateway.internal.example/anthropic"  # optional, defaults to https://api.anthropic.com`), *Then* `tests/references_conf_d.rs::references_00_providers_toml_should_parse_cleanly_against_config_schema` and `tests/toml_parity.rs::conf_d_fixtures_are_tomllib_portable` both still pass unchanged (a `#`-prefixed line is inert to both parsers).
**Files**: `references/conf.d/00-providers.toml`

##### Task 5.1.1a: Add the commented-out `base_url` example (~2 min)
- In `references/conf.d/00-providers.toml`, immediately after line 24 (`kind = "anthropic"`), add a comment line documenting the optional override, mirroring the file's existing convention of showing variations as comments (e.g. the `openrouter`-via-`openai` block at lines 6-20).
- Files: `references/conf.d/00-providers.toml`
