# Architecture Review — Consolette Implementation Plan (SDD Phase 3)

**Reviewer:** architecture-quality pass over `implementation/plan.md`
**Date:** 2026-07-17
**Scope:** plan document only (no code); evaluated against `requirements.md`, the 4 research files, and current `claude-proxy-rs` source (`fallback.rs`, `main.rs`, `config.rs`, `providers/mod.rs`).

## Verdict: **NEEDS-WORK**

The core architecture is sound: the three-way split the current `FallbackHandler` conflates (health state / selection policy / dispatch orchestration) is correctly separated into `HealthRegistry` + `RoutingStrategy` + `Router`, reuse of the existing `Provider` trait and ADR-006 cooldown machine is disciplined (not a rewrite), and scope is well-bounded for a single-tenant proxy (internal-identity auth stubbed, hot-reload deferred, mesh sidecar out). The router depends on interfaces (`dyn Provider`, `dyn RoutingStrategy`, `dyn Availability`, `AdmissionControl`), and governor is confined to the `ratelimit` module.

It is **NEEDS-WORK rather than SOUND** because of two *specified-but-incoherent* items the plan must resolve before implementation — the rate-limiting seam is described two contradictory ways, and the "ArcSwap from day one" claim does not cover the substrate that actually needs reloading — plus several under-specified contracts. All are fixable in planning; none require a redesign.

---

## MUST-FIX

### M1. Rate limiting is specified as BOTH a pure `Availability` predicate AND a stateful post-selection `admit()` — pick one
- **Where:** ADR-003/ADR-004 (plan lines 19–20), Dependency diagram ("Availability seam is shared", lines 60–62, 84–85), Story 3.3 (lines 306–312), Story 4.3.2 (line 351). Backed by conflicting research: weighted-router §5 frames rate limiting as `Availability::is_available(idx) -> bool` (pre-selection filter, side-effect-free); rate-limiting §3 frames it as `AdmissionControl::admit(upstream, est_tokens) -> Admit` (post-selection, **consumes** the token bucket).
- **Problem:** these are not interchangeable. `Availability::is_available` is called during candidate *filtering* — for weighted it may be evaluated against every candidate. If a rate limiter charged tokens there it would charge upstreams that were never selected, and governor's `check_n()` *commits state on success*, so it cannot serve as a non-consuming predicate. Conversely, `admit()` must run exactly once, on the selected upstream, after selection. The plan's actual implementation (Story 4.3.2: estimate once, `admit` the *chosen* upstream, on `Shed` add to `already_tried` and re-select) is the correct `AdmissionControl` model — but Epic 3 (Story 3.3.2) still populates a `Vec<Arc<dyn Availability>>` and claims rate limiting "joins at the Availability seam," which the implementation never does. The `Availability` vec ends up holding only `HealthRegistry`.
- **Consequence if unresolved:** implementers will build the `Availability` seam expecting a rate-limit source that never arrives, and the "weight redistribution excludes rate-limited upstreams" behavior (FR-3.3) is only *approximately* honored via rejection-sampling retry, not via the clean pre-selection filter the diagram implies.
- **Recommendation:** commit to **`AdmissionControl` post-selection admit as the single rate-limit mechanism** (correct for governor). Then: (a) scope `Availability` explicitly to health/cooldown only and stop describing rate limiting as an availability source (fix ADR-003, ADR-004, the diagram legend, and Story 3.3's "stubbed second source" framing); (b) state in Story 4.3 that weighted redistribution around a shedding upstream is achieved by the `already_tried` re-select loop, not by candidate filtering, and note this is rejection sampling (fine for 2–5 upstreams). *If* you instead want true pre-selection exclusion in weighted mode, you must add a **non-consuming peek** (`governor` `NotUntil`/`wait_time_from` without commit) as the `Availability` impl AND keep `admit()` as the charge — a deliberate two-step (peek-to-filter, admit-to-charge) with documented TOCTOU drift. Do not leave both framings implicit.

### M2. "ArcSwap from day one" covers only top-level `Config`, not the router/limiters — the reload-readiness claim is false as designed
- **Where:** ADR-001 (line 17: "`ArcSwap` from day one"), Story 1.5 (lines 227–234: `AppState` holds `Arc<ArcSwap<Config>>`), vs Story 3.4.3 (line 321: `AppState` holds `Arc<Router>`) and Story 4.2 (`RateLimiters` built once at startup). config-layering research Q5 states the *actual* hot-reload use case is "changing routing tables live … the actual use case."
- **Problem:** the `Router` (upstreams, routes, strategies) and `RateLimiters` (governor `DashMap`) are constructed once from config and stored as plain `Arc<_>`, **outside** any `ArcSwap`. `ArcSwap<Config>` only makes scalars (port, timeouts, cooldown) reload-ready — and several of those (listen port) can't be changed live anyway. The one thing an operator would actually reload — routes/upstreams/limits — is *not* in the swap. The claim "ship `ArcSwap` now so hot-reload is a purely additive change" (research Q5, ADR-001) is therefore not delivered by this design.
- **Recommendation:** either (a) wrap an immutable `Arc<Runtime>` bundling `{upstreams, routes, strategies, health?, limiters}` in the `ArcSwap` (health/cooldown state must persist across swaps, so keep `HealthRegistry` *outside* the swapped bundle or migrate it on swap), making the meaningful reload additive; or (b) honestly scope the ADR-001 claim to "scalar config only; routing/upstream/limit reload is explicitly out of scope for v1" and drop the "reload-ready substrate" justification. As written, the plan asserts reload-readiness it does not provide.

---

## SHOULD

### S3. `Provider::send`'s canonical body format is undefined once a second upstream wire-format enters
- **Where:** Story 2.2 (lines 252–259), against the existing `Provider::send` contract (`fallback.rs:53`: "the (already cleaned) **Anthropic-format** JSON request body") and the OpenAI↔Anthropic translators in `providers/mod.rs`.
- **Problem:** today every body flowing to `Provider::send` is Anthropic-format. An `openai`-kind upstream pointed at Model Gateway's OpenAI path (`/v1/chat/completions`) receives that Anthropic-format body and must translate out (Anthropic→OpenAI) and back (OpenAI→Anthropic). The plan adds `anthropic_passthrough` for the native `/v1/messages` path (Task 2.2.3) but never states the canonical internal body format nor *where* translation lives for the OpenAI path. This is a latent leaky abstraction: the trait's implicit "body is Anthropic-format" invariant is undocumented and the new provider either honors it (and translates internally) or breaks it.
- **Recommendation:** in Epic 2, document the `Provider::send` body contract explicitly — e.g. "the router always passes the canonical Anthropic Messages body; each provider translates to/from its wire format internally" — and put the Anthropic↔OpenAI translation inside `OpenAiProvider` (reusing the existing `translate_*` functions), keeping the router format-agnostic. Add an acceptance criterion for the OpenAI-path round-trip, not just the passthrough path.

### S4. The auth abstraction is applied unevenly — generic for openai, baked-in for anthropic/bedrock
- **Where:** Story 2.1 (`AuthMethod` enum + `SecretRef`, lines 243–250), Task 2.1.2 (line 249: "reuse anthropic's existing OAuth-vs-`sk-ant-api-*` logic for `kind = anthropic`").
- **Problem:** `AuthMethod::{Bearer,ApiKey}` header injection is generic HTTP and belongs in a shared send/header layer; but anthropic's OAuth-vs-`sk-ant` selection and bedrock's SigV4 stay inside their providers. So "pluggable per-upstream auth" (FR-2) is truly pluggable only for `openai`-kind. That is acceptable for no-regression, but the plan doesn't articulate the split, so the seam looks uniform while it isn't — inviting duplicated header logic and confusion about which auth path a given upstream uses.
- **Recommendation:** state the intended layering: a generic `AuthMethod` header-injector shared by all `reqwest`-based providers for `bearer`/`apikey`; provider-native auth (anthropic OAuth, bedrock AWS/SigV4) remains internal and is modeled as `AuthCfg::Aws`/anthropic's own path rather than routed through `AuthMethod::resolve()`. Keep Task 2.1.2's OAuth logic *in the anthropic provider*, not in `auth.rs`.

### S5. Python `tomllib` parity (CD-1 / NFR-2) is asserted but not verified by any task
- **Where:** Story 1.2 acceptance (line 201: "a Python `tomllib.load` … succeeds (parity check)"); Task 1.2.4 (line 207) only wires a Rust `toml::from_str` fixture test; Epic 8 has no Python parity task.
- **Problem:** schema portability to the future Python port is a fixed constraint (CD-1) and an NFR (NFR-2), but nothing in the task list actually executes `tomllib` against the reference `conf.d`. It will be verified by hand once, then silently drift.
- **Recommendation:** add a test/CI task (Epic 1 or Epic 8) that runs `python3 -c 'import tomllib; tomllib.load(open(f,"rb"))'` over each `references/conf.d/*.toml` and fails the build on error. Cheap, and it is the only thing that actually enforces the shared-schema constraint.

### S6. Rate-limiter tests will be time-dependent — inject a mock clock
- **Where:** Story 4.1/4.2 build `UpstreamLimiter` on `DefaultClock` (rate-limiting research §2 type alias); Story 8.1.3 (line 465: rate-limit shed/independence test).
- **Problem:** governor's admission is time-arithmetic; with `DefaultClock`, exercising "bucket empties then refills" requires real sleeps → slow, flaky tests, and RPM/TPM refill windows (per-minute) are impractical to test in wall-clock.
- **Recommendation:** make `UpstreamLimiter` generic over `governor::clock::Clock` (or hold a `DefaultClock` field injected at construction) and use `clock::FakeRelativeClock` in unit tests to advance time deterministically. Note this in Story 4.1 so the limiter type is clock-injectable from the start rather than retrofitted.

### S7. `SecretRef::Keychain` resolution is not behind a mockable seam
- **Where:** Task 2.1.1 (line 248: `resolve()` reads env / `security find-generic-password`).
- **Problem:** the `Env` variant is unit-testable, but `Keychain` shells out to `security`, so auth-type resolution can't be tested in isolation (fails the "each auth type unit-tested" goal). Env-var reads via `std::env` are also process-global, which makes parallel auth tests interfere.
- **Recommendation:** define a small `SecretResolver` trait (or pass a resolver closure) so `resolve()` takes its source as a dependency; provide a real impl (env + keychain) and a test double. Keeps NFR-5 ("auth abstractions covered by unit tests") honest.

---

## NICE-TO-HAVE

### N8. Extract token estimation behind a seam instead of hardcoding it in `router.rs`
Task 4.3.1 (line 350) puts tiktoken estimation in `router.rs`, coupling the orchestrator to a concrete tokenizer. A `TokenEstimator` trait (reusing the existing `count_tokens`/compression counter) keeps the router format-agnostic, makes TPM tests deterministic, and mirrors a boundary the Python port will also need. Low effort, improves S3/S6 too.

### N9. Record the ADR-006 deviation (RwLock → DashMap) explicitly
Story 3.2 (lines 297–303) moves `ProviderState` from `Arc<RwLock<ProviderState>>` (ADR-006's deliberate choice for atomic check-and-transition) to `DashMap<usize, ProviderState>` with `get_mut`. The invariant still holds (per-key exclusive guard = atomic transition), and weighted-router §4 justifies it — but ADR-006 is currently the standing decision. Add a superseding ADR note (or amend ADR-003) so the change is traceable, not silent.

### N10. `options: Option<toml::Value>` bypasses `deny_unknown_fields`
Task 1.2.2 (line 205) models kind-specific options as an untyped `toml::Value`, so a typo like `aws_regionn` inside `[upstreams.options]` is silently ignored — weakening the fail-fast guarantee (FR-1.5) exactly for the bedrock knobs that matter. Consider a typed, `deny_unknown_fields` `BedrockOptions` struct (and per-kind option enums) rather than a catch-all blob.

### N11. `Vec<Arc<dyn Availability>>` is mild over-abstraction for one impl
Once M1 scopes `Availability` to health only, the vec holds exactly one source. Keeping the trait is still worthwhile for unit-test stubbing (Story 3.3), so this is fine as-is — just don't add further indirection on top of it. Flagging only so it isn't mistaken for the rate-limit integration point (see M1).

---

## What is well-designed (keep as-is)
- **Concern separation** (weighted-router §1): `RoutingStrategy::select(&[healthy])` is pure and health-blind; `HealthRegistry` owns cooldown; `Router` owns the dispatch loop and error arms. This is the right factoring and directly fixes the current `FallbackHandler` god-method.
- **Reuse discipline:** `Provider` trait unchanged, `ProviderState`/TOCTOU cooldown moved not rewritten, error-classification arms ported verbatim, Bedrock same-upstream backoff kept inside the provider. This is an evolution, not a rewrite (satisfies FR-3.5, FR-6.4).
- **Config engine choice:** figment for source-metadata-rich fail-fast errors (FR-1.5) + `deny_unknown_fields` + a semantic `validate_references()` pass is the correct three-layer split; env-as-indirect-secret-ref (lines 187–189) cleanly satisfies NFR-6.
- **Scope control:** internal-identity auth reserved-and-stubbed (CD-2), hot-reload deferred with `notify` out, mesh sidecar out. Appropriately under-engineered for a personal single-tenant proxy.
- **Interface-based coupling:** router → traits, governor quarantined behind `AdmissionControl`. Correct dependency direction.
