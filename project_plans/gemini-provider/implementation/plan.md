# Implementation Plan: gemini-provider

**Feature**: Add a `UpstreamKind::Gemini` upstream so consolette can route `/v1/messages`
requests to Google Antigravity's Gemini 3 Pro (via the undocumented Cloud Code Assist internal
protocol) alongside the existing Anthropic/Bedrock/OpenAI upstreams — same routing, health/cooldown,
cost metrics, and dashboard, no separate tool.
**Date**: 2026-09-04
**Status**: Ready for implementation
**ADRs**:
- `project_plans/gemini-provider/decisions/ADR-001-gemini-auth-token-source.md` (Exec wrapper
  script, not `keyring`)
- `project_plans/gemini-provider/decisions/ADR-002-gemini-schema-drift-cooldown-classification.md`
  (`ResponseShapeMismatch` variant + immediate-trip cooldown)
- `project_plans/gemini-provider/decisions/ADR-003-gemini-project-id-resolution.md`
  (config-supplied `project_id`, not dynamic `loadCodeAssist`)

---

## Creative pass (Step 0.5) — alternatives considered

Three high-level approaches were weighed before committing to the architecture below:

1. **Native Rust `GeminiProvider`** (chosen) — implements the existing `Provider` trait
   (`src/providers/mod.rs:117-143`) in-process, exactly like `anthropic.rs`/`bedrock.rs`/`openai.rs`.
   *Strength*: fits the existing `ProviderError` taxonomy, ADR-004 two-client split, and
   `Router::dispatch` health/cooldown machinery exactly — no new integration seam. *Weakness*: the
   two-layer translation (Anthropic↔Gemini-native, then Gemini-native↔Cloud-Code-envelope) and SSE
   reconstruction have to be hand-written from scratch; no existing code in this repo does anything
   this involved.
2. **Sidecar proxy** (e.g. `frieser/antigravity-proxy`, `NikkeTryHard/zerogravity`) fronted by the
   existing `UpstreamKind::Openai { base_url }` pointed at `localhost:<port>`. *Strength*: zero new
   Rust translation code — technically the least-code path. *Weakness*: every functional candidate
   is archived/discontinued (`research/build-vs-buy.md`), and the two most-functional ones use
   non-official token-extraction auth mechanisms that directly violate `requirements.md`'s
   "official OAuth only, no harvested credentials" constraint — adopting one would trade "write
   code" for "depend on somebody else's already-abandoned code plus a second failure source."
3. **Vendor the public `generativelanguage.googleapis.com` Gemini API** instead of the internal
   Cloud Code Assist protocol. *Strength*: documented, stable, no reverse-engineering risk at all.
   *Weakness*: confirmed protocol mismatch — an Antigravity-issued OAuth token is not valid against
   the public API (`requirements.md`'s Decision section). This doesn't satisfy the actual baseline
   (routing Tyler's *Antigravity subscription* traffic) — it would need a completely different,
   separately-billed credential, defeating the stated purpose.

**Chosen**: (1), native Rust. (2) and (3) are recorded as rejected alternatives in the Pattern
Decisions table below (row "Overall integration approach").

---

## Domain Glossary
| Term | Definition | Notes |
|------|-----------|-------|
| `UpstreamKind::Gemini` | New config enum variant (`src/config/schema.rs`) identifying a Cloud Code Assist upstream; carries `project_id: String` (ADR-003). | Added to the existing `#[serde(tag = "kind", deny_unknown_fields)]` enum at `src/config/schema.rs:107-120`. |
| `GeminiProvider` | Struct in new `src/providers/gemini/mod.rs` implementing the `Provider` trait for the Cloud Code Assist endpoint; structurally mirrors `OpenaiProvider` (`src/providers/openai.rs:40-102`) more than `AnthropicProvider`. `src/providers/gemini/` is a submodule tree from the start of Phase 1 (`mod.rs`/`translate.rs`/`tools.rs`/`error.rs`, plus `stream.rs` from Phase 2 — see the "module organization" Pattern Decisions row and Risk Control). | Owns `client`/`stream_client` (ADR-004 split), `base_url` (hardcoded `https://cloudcode-pa.googleapis.com`), `upstream`, `resolver`, `exec_cache`, `thought_signatures: ThoughtSignatureCache` (Story 1.6.1 — provider-owned, not per-`send()`; the `ThoughtSignatureCache` *type* lives in `tools.rs`, this field lives on the struct in `mod.rs`). |
| `CloudCodeEnvelope` | Private struct in `gemini/translate.rs` representing the outer wrapper `{project, model, requestType, userAgent, request}` that every Cloud Code Assist call is wrapped in, sitting *around* the native Gemini request, not inside it. | See ADR-003; `project` comes from `UpstreamKind::Gemini::project_id`. |
| `GeminiContent` | Native Gemini `{role: "user"\|"model", parts: Vec<GeminiPart>}` struct — the Gemini analog of an Anthropic `messages[]` entry. | Role vocabulary differs from Anthropic (`"model"` not `"assistant"`). |
| `GeminiPart` | Native Gemini part representation: `text`, `functionCall{name,args,id}`, or `functionResponse{name,id,response}`. | `functionCall`/`functionResponse` carry no independent call-id the way Anthropic's `tool_use`/`tool_result` do (see `GeminiToolCallState`). |
| `GeminiGenerationConfig` | Struct mapping Anthropic's `max_tokens`/`temperature`/`top_p`/`stop_sequences` onto Gemini's `generationConfig` object (`maxOutputTokens`, `temperature`, `topP`, `topK`, `stopSequences`, `thinkingConfig`). | Built fresh per request in `translate_anthropic_request_to_gemini`. |
| `GeminiUsageMetadata` | Raw wire struct for Gemini's `usageMetadata` (`promptTokenCount`, `cachedContentTokenCount`, `candidatesTokenCount`, `thoughtsTokenCount`, `totalTokenCount`). | Mapped into the existing `AnthropicUsage` shape (`src/providers/mod.rs:477-482`); `cache_creation_input_tokens`/`cache_read_input_tokens` stay `0` (no Gemini equivalent). |
| `ProviderError::ResponseShapeMismatch(String)` | New `ProviderError` variant (ADR-002) distinguishing "200 OK but the body didn't match the documented shape" from an HTTP-level `Upstream{status,body}` failure. | Added to `src/providers/mod.rs:33-55`. |
| `is_response_shape_mismatch()` | New classification method on `ProviderError`, alongside `is_auth()`/`is_validation()`/`is_rate_limited()` (`src/providers/mod.rs:80-99`). | Used by `Router::dispatch`'s new match arm (ADR-002). |
| `DRIFT_COOLDOWN_SECS` | Constant defined `pub(crate)` in `src/providers/gemini/error.rs` (e.g. `1800`) — the `HealthRegistry::trip` override duration used when a `ResponseShapeMismatch` occurs, longer than the default `cooldown_seconds` (300s) since schema drift won't self-heal on its own. Re-exported from `src/providers/gemini/mod.rs` via `pub(crate) use error::DRIFT_COOLDOWN_SECS;` so the existing call-site path `crate::providers::gemini::DRIFT_COOLDOWN_SECS` (`router.rs`) is unchanged by the module split. | Passed as `Router::dispatch`'s new arm's `override_duration`. |
| `sanitize_function_schema` | New recursive fn in `gemini/translate.rs` that walks an Anthropic tool's `input_schema` and strips JSON-Schema keywords Gemini's `functionDeclarations[].parameters` rejects (`$ref`, `$defs`, `patternProperties`). | Nested walk, unlike `bedrock.rs`'s flat `clean_body` field-stripping. |
| `map_gemini_finish_reason` | New fn mapping Gemini's `finishReason` (`STOP`/`MAX_TOKENS`/`SAFETY`/`RECITATION`/`OTHER`) onto Anthropic's `stop_reason` vocabulary (`end_turn`/`max_tokens`/`tool_use`/`stop_sequence`). | `SAFETY`/`RECITATION` → `end_turn` + a synthesized explanatory text block (decision #3, see Story 1.3.2). |
| `classify_gemini_error` | New fn mirroring `map_error_status` (`src/providers/anthropic.rs:556-592`) — maps Gemini's `{"error":{code,message,status,details}}` body + HTTP status onto `ProviderError`, including 429 `retryDelay` → `RateLimitedWithRetry`. | |
| `GeminiToAnthropicStream` | New struct in `gemini/stream.rs` mirroring `OpenaiToAnthropicStream` (`src/providers/openai.rs:369-535`), but tracking **multiple** indexed content blocks (text/functionCall/thinking) instead of a single hardcoded index-0 text block. | Phase 2 only — `stream.rs` and its `mod stream;` declaration in `mod.rs` are created in Story 2.1.1, not scaffolded empty in Phase 1. |
| `ToolUseId(String)` | New newtype wrapping an Anthropic `tool_use` block's `id` string, used consistently as the key type in both `GeminiToolCallState` and `ThoughtSignatureCache` instead of a raw `String`, mirroring the existing typed-ID precedent `cost_metrics::types::RequestId`. Prevents a call site accidentally swapping a key and value that are otherwise both plain `String`s. Defined in `src/providers/gemini/tools.rs`. | Introduced in Story 1.6.1 (Phase 1, ahead of its first real use) so both Phase 3 consumers share one type. |
| `GeminiToolCallState` | New small struct maintained per-request by `GeminiProvider`, mapping a `ToolUseId` ↔ the Gemini `functionCall`'s `name` (and position) via `HashMap<ToolUseId, String>`, since Gemini's function-call parts carry no explicit call id. Defined in `src/providers/gemini/tools.rs`. | Phase 3 only; correctly request-scoped (unlike `ThoughtSignatureCache` below) since the client resends `tool_use` history on every turn. |
| `ThoughtSignatureCache` | `GeminiProvider`-owned, concurrency-safe cache (`DashMap<(String, ToolUseId), CacheEntry>`, mirroring `ExecCredentialCache`'s `DashMap` pattern at `src/auth/exec.rs:52-55`) storing Gemini 3 Pro's opaque per-`functionCall` `thought_signature` so it can be echoed back unmodified on a *later, separate* HTTP request (decision #2). Keyed by `(session_key, ToolUseId)`, not `ToolUseId` alone: `session_key` comes from `extract_session_id(&body)` (`src/routing/session_overrides.rs:32`, reading `metadata.user_id` — the same field `SessionOverrideStore` already keys route pins on) when the client sends it, else a fixed `"anonymous"` sentinel. This matters because `GeminiProvider` is one shared `Arc<dyn Provider>` serving every concurrent conversation (`src/routing/router.rs:57`; `Provider::send(&self, ..)`) — a bare `ToolUseId` key would let two unrelated concurrent conversations cross-wire signatures. Each entry carries `inserted_at: Instant`; entries older than `THOUGHT_SIGNATURE_TTL_SECS` are swept on insert, bounding growth. The *type* (`ThoughtSignatureCache` struct + its `insert`/`get`/sweep impl) is defined in `src/providers/gemini/tools.rs`; the *instance* is a field on `GeminiProvider` in `src/providers/gemini/mod.rs`, constructed once in `GeminiProvider::new`. | Scaffolded in Phase 1 (Story 1.6.1) as a provider-owned field — constructed once per `GeminiProvider`, not per `send()` call — with `.insert()`/`.get()` unused until tool calls exist; load-bearing from Phase 3 (Story 3.3.1) onward. |
| `antigravity-token-auth.py` | New Exec credential-helper script at `references/bin/antigravity-token-auth.py` (ADR-001) implementing the ADR-007 §2 stdin/stdout JSON contract: reads `~/.gemini/antigravity-cli/antigravity-oauth-token`, checks expiry, emits `{"headers": {...}}`. | Zero new Rust dependency; Python 3 stdlib only. |
| `antigravity-oauth-token` file | The verified concrete credential artifact at `~/.gemini/antigravity-cli/antigravity-oauth-token` (JSON, mode 600): `{"token":{"access_token","token_type","refresh_token","expiry"},"auth_method":"consumer"}`. | Written by the Antigravity IDE / `agy` CLI, not by consolette. |
| `project_id` | Required `String` field on `UpstreamKind::Gemini` (ADR-003), the Cloud Code Assist envelope's `project` field. | Config-supplied, resolved once at load time — no dynamic `loadCodeAssist` call in v1. |
| `GEMINI_3_PRO_OUTPUT_CEILING` | New constant in `gemini/translate.rs` — the single-row (v1 has exactly one first-class model) per-model max-output-token ceiling used to clamp/validate an incoming `max_tokens`. | Exact value is an Unresolved Question (see below) — must be confirmed against `fetchAvailableModels` or Google's docs before hardcoding, not guessed. |
| `build_providers` | Existing fn (`src/routing/router.rs:57-84`) — gets one new `UpstreamKind::Gemini { project_id }` match arm constructing `GeminiProvider::new(...)`. | Exhaustive match — compiler-enforced. |
| `upstream_kind_label` | Existing fn (`src/entrypoint/mod.rs:121-127`) — gets one new `UpstreamKind::Gemini { .. } => "gemini"` arm. | The only dashboard-facing code that names a provider kind by string. |
| `status-schema-drift` | New dashboard CSS class (`src/dashboard.rs`) — a new hue (e.g. violet, unused elsewhere in the existing dark palette) rendered when an upstream's last recorded error was `ResponseShapeMismatch`. | Generic — not keyed to "gemini" by name (dashboard.rs's own `no_upstream_is_hardcoded_by_name` test, `src/dashboard.rs:613-628`, forbids that). |
| `status-auth-required` | New dashboard CSS class reusing the existing error-badge red hue — rendered when an upstream's last recorded error was `ProviderError::Auth`. | Distinct from `status-cooldown` (amber): amber implies "will self-heal," this doesn't. |
| `UpstreamCounters::last_error_kind` | New field on the existing per-upstream `UpstreamCounters` struct (`src/metrics/counters.rs:16-24`) — a `Mutex<Option<&'static str>>` recording the most recent *typed* `ProviderError` classification for that upstream. | Root-cause fix (Story 1.4.4): replaces having the dashboard re-derive error kind from `ErrorTracker`'s regex-guessed `error_type` string. |
| `Router::cooldown_snapshot` | New method on `Router` (`src/routing/router.rs`) returning real per-candidate `{name: {cooling_down, remaining_seconds}}` JSON from `HealthRegistry::remaining_secs`, replacing the hardcoded `anthropic`/`bedrock`-only placeholder currently in `MetricsCollector::to_metrics_json` (`src/metrics/mod.rs:386-390`). | Pre-existing gap, generic fix — benefits all four upstream kinds, not Gemini-specific. |

**Glossary term count: 27.**

---

## Pattern Decisions
| Component | Pattern Chosen | Source | Alternative Rejected | Reason |
|-----------|---------------|--------|---------------------|--------|
| Overall integration approach | In-process `Provider` implementation (existing Strategy-pattern seam) | GoF Strategy (already established by `Router`/`Provider`) | (1) Sidecar proxy behind `UpstreamKind::Openai`; (2) vendor public `generativelanguage.googleapis.com` | (1) every functional sidecar candidate is archived/discontinued and the working ones use non-official token-harvesting auth, violating the official-OAuth-only constraint; (2) an Antigravity token isn't valid against the public API at all — doesn't satisfy the baseline |
| Request/response translation | Transaction Script — stateless pure functions (`translate_anthropic_request_to_gemini`, `translate_gemini_response_to_anthropic`) mirroring `openai.rs`'s module-level translation fns | PoEAA | Domain Model (rich `GeminiRequest`/`GeminiResponse` objects carrying translation behavior as methods) | Translation is a one-shot, stateless mapping identical in shape to the existing `openai.rs` precedent (`translate_anthropic_request_to_openai`, `src/providers/mod.rs:335-380`); a full domain model adds ceremony with no reuse payoff at this scale |
| `thought_signature` persistence | `GeminiProvider`-owned, session-keyed `DashMap` (`ThoughtSignatureCache`), a Memento-like side channel — **not** request-scoped, since the signature must survive from one `send()` call to a later, separate one (see Domain Glossary) | GoF (loosely, Memento); concurrency shape mirrors `ExecCredentialCache` (`src/auth/exec.rs:52-55`) | (a) Re-encode the signature into an unused Anthropic response field (e.g. smuggled into `tool_use.id`); (b) a single `send()`-local `HashMap` (Phase 1's original scaffolding) | (a) Smuggling an opaque Google blob into a field Claude Code itself parses and may treat as a stable identifier risks breaking Claude Code's own id-uniqueness assumptions; a provider-internal cache keeps the hack invisible to the wire contract entirely. (b) Cannot work at all — a value dropped when `send()` returns can't bridge to the client's later, separate HTTP request (both architecture and adversarial review flagged this as a blocker); fixed by promoting the cache to a provider field keyed by `(session_key, ToolUseId)` with TTL eviction (Story 1.6.1) |
| Tool-call/signature key typing | `ToolUseId(String)` newtype used as the key type in both `GeminiToolCallState` and `ThoughtSignatureCache` | Type-driven design; existing precedent `cost_metrics::types::RequestId` | Raw `String` keys (as originally scaffolded) | Two same-typed `String` parameters (key/value) at a call site like `insert(tool_use_id: &str, signature: String)` give the compiler nothing to catch an accidental argument swap — the classic primitive-obsession risk this codebase already avoids elsewhere via `RequestId` |
| Auth token access | Exec wrapper script (existing `AuthMethod::Exec`, ADR-007 convention) | ADR-001; existing ADR-007 §2/§4 precedent | `keyring` crate + new `SecretRef::Keyring` core variant | Zero verified need on this machine (live `secret-tool search` empty); adds a new core auth surface plus async-wrapping complexity for a storage backend nothing here currently exercises |
| Auth failure UX | Fail closed with an actionable message, no inline-reauth attempt — conceptually parallel to `BedrockProvider::do_sso_login` (`src/providers/bedrock.rs:572-605`) in that both give an actionable re-auth message on auth failure, but Gemini's path has no inline-reauth-attempt branch (unlike Bedrock's has-TTY fork) since `antigravity-cli` requires a real browser session | Existing precedent (Transaction Script style) | Inline reauth attempt (spawn `antigravity-cli login` automatically) | `agy` requires an interactive browser/TTY flow consolette can't drive headlessly; an inline attempt would hang or silently fail in the `ssh-bastion-client` headless case, which is a real runtime state on this machine, not a hypothetical |
| Schema-drift signal + cooldown | New `ProviderError::ResponseShapeMismatch` variant, classified via `is_response_shape_mismatch()`, tripped via existing `HealthRegistry::trip(idx, override_duration)` on first occurrence | ADR-002 | (a) Alias onto `ProviderError::RateLimitedWithRetry`; (b) new consecutive-failure-counter state in `HealthRegistry` | (a) conflates two opposite failure semantics (self-healing vs. needs-a-code-fix) and corrupts the existing rate-limit metric; (b) is unverified need for a non-streaming full-body parse failure (already total, not partial) — build only if observed for streaming |
| Project-id resolution | Config-supplied `project_id: String` field on `UpstreamKind::Gemini`, resolved at config-load time | ADR-003; existing `UpstreamKind::Bedrock` field-carrying precedent | Dynamic `loadCodeAssist` call at provider startup/first-use | Adds a new startup network call + cache-invalidation policy + failure mode before the feature has worked once; no payoff at Tyler's single-project usage scale |
| `finishReason` mapping | Adapter — `map_gemini_finish_reason` maps `STOP`/`MAX_TOKENS`/`OTHER` onto existing Anthropic vocabulary; `SAFETY`/`RECITATION` → `end_turn` + synthesized explanatory text content block | Adapter (GoF); mirrors `map_openai_finish_reason` precedent (`src/providers/mod.rs:448-453`) | `ProviderError::Validation` for `SAFETY`/`RECITATION` | `Validation` means "client sent bad input" per its own doc comment (`src/providers/mod.rs:40`) — a safety-blocked-but-otherwise-completed generation is not a bad *request*; surfacing it as a normal completed message (with explanatory text) keeps the Anthropic-shape contract intact instead of turning a completed response into a hard failure |
| Tool schema compatibility | Schema-walking recursive sanitizer (`sanitize_function_schema`) stripping `$ref`/`$defs`/`patternProperties` at any nesting depth | GoF Visitor-ish recursive walk | Top-level field stripping (`bedrock.rs`'s `clean_body` style, `src/providers/bedrock.rs`) | Gemini's incompatible keywords appear nested arbitrarily deep inside `functionDeclarations[].parameters` schemas (common in Claude Code's own `$ref`/`$defs`-heavy tool schemas) — a shallow top-level stripper wouldn't reach them |
| Streaming reconstruction | Dedicated multi-block-tracking struct `GeminiToAnthropicStream`, new `content_block_start` emitted on every part-type change | Adapter/State (GoF) | Reuse `OpenaiToAnthropicStream` as-is (hardcoded single index-0 text block) | Gemini/Cloud-Code streaming interleaves text + `functionCall` (+ possibly thinking) within one candidate's accumulating `parts[]`; a single-block assumption would silently drop or corrupt non-text parts |
| Config field shape | Flat fields directly on `UpstreamKind::Gemini` (`project_id`) | Existing convention (`UpstreamKind::Bedrock`/`Openai` precedent) | Nested `[upstreams.gemini]` sub-table | `#[serde(flatten)]` + `deny_unknown_fields` interaction already establishes flat-only kind-specific fields (`src/config/schema.rs:122-125`'s own comment); nesting would be the one inconsistent kind |
| Auth config shape | Unchanged separate `[upstreams.auth]` table, `type = "exec"` | Existing convention (`tests/fixtures/toml_parity/exec_auth_upstream.toml`) | A new Gemini-specific auth type/table | Zero new concepts needed — `exec` is already fully generic per ADR-007 |
| Route strategy default | `Strategy::Fallback` behind `anthropic`/`bedrock` in the example config | Existing `Strategy` enum (GoF Strategy) | `Strategy::Weighted` default | The accepted protocol-instability risk should not be spread across live weighted traffic by default; Fallback isolates blast radius to "try Gemini first, drop silently to Anthropic on any failure" only |
| Error-kind attribution for dashboard | Pass the real `ProviderError` classification through explicitly (`UpstreamCounters::last_error_kind`) rather than re-deriving it from message text | Root-cause fix, not a new pattern per se | Add a fifth regex/keyword pattern to `ErrorTracker::extract_signature` for `ResponseShapeMismatch` | The existing regex-based `error_type` derivation (`src/metrics/error_tracker.rs:141-194`) is already fragile (keyword-sniffs "auth"/"timeout"/etc out of `Display` text); adding another guessed pattern compounds the fragility instead of fixing it — the caller already has the typed variant and should just pass it through |
| Module organization | Submodule tree from the start of Phase 1: `src/providers/gemini/{mod,translate,tools,error}.rs`, plus `stream.rs` added in Phase 2 when streaming reconstruction first exists — `mod.rs` owns the `GeminiProvider` struct/`Provider` impl/constructor/`build_headers` and re-exports what `router.rs` needs; `translate.rs` owns the stateless Anthropic↔Gemini translation fns and wire structs; `tools.rs` owns `ToolUseId`/`GeminiToolCallState`/`ThoughtSignatureCache`'s type+impl; `error.rs` owns `classify_gemini_error`/`GeminiErrorBody`/`DRIFT_COOLDOWN_SECS` | Decided upfront by Tyler, 2026-09-04, overriding this plan's original approach (below) | Single `src/providers/gemini.rs` file for all of Phase 1-3, with a `code-hotspot-analysis` checkpoint before Phase 2 to decide whether to split | The single-file approach was this plan's original default (mirroring `anthropic.rs`/`openai.rs`'s one-file-per-provider shape) and deferred the split decision to a measured checkpoint. Tyler chose to split upfront instead: the Domain Glossary and Pattern Decisions already establish that Gemini's blended responsibilities (two-layer translation, SSE reconstruction, tool-call bookkeeping, error classification) exceed what any existing one-file provider does, so measuring first adds a review cycle without changing the answer. See the superseded Risk Control bullet below for what this replaces. |

---

## Observability Plan
- **Logs**:
  - `tracing::warn!` on every `ResponseShapeMismatch`, with the raw unexpected shape (truncated to
    ~2KB) logged once in full detail, then rate-limited on repeat (mirrors
    `research/ux.md`'s recommendation: "so Tyler can grep+paste into a bug report" without spamming
    on every subsequent identical failure).
  - `tracing::error!` (not just a returned `Err`) on Exec auth-helper failure, distinct from a
    request-level 401/403 — this already falls out for free from `AuthError::Exec`'s existing
    `From<AuthError> for ProviderError` (`src/providers/mod.rs:57-61`) plus the existing
    `tracing::warn!` calls in `run_helper` (`src/auth/exec.rs:241-246,254-259`); Story 1.2.2 adds a
    `gemini`-specific message text ("token refresh failed via antigravity-token-auth.py — run
    `antigravity-cli` login again or reopen the Antigravity IDE") without touching `exec.rs` itself.
  - `tracing::info!` on every successful Gemini `send()` at the same verbosity level the other
    three providers log at — no new verbosity tier.
- **Metrics**: reuse `RequestDetail::from_body` (`src/metrics/mod.rs:68-80`) and the per-upstream
  `UpstreamCounters` DashMap (`src/metrics/counters.rs:39`) unchanged — both are already
  name-keyed, not kind-keyed, so Gemini requires zero Gemini-specific metrics code. New:
  `UpstreamCounters::last_error_kind` (Story 1.4.4) and a real `Router::cooldown_snapshot()` feed
  (Story 1.5.1) — both generic fixes that benefit all four upstream kinds.
- **Alerts**: none — this is single-operator personal infrastructure with no external alerting
  pipeline (per `requirements.md`'s Non-functional Requirements). The dashboard's new
  `status-auth-required`/`status-schema-drift` visual states are the alerting mechanism.

## Risk Control
- **Feature flag**: none needed. The Gemini provider is inert unless a `kind = "gemini"` upstream
  is explicitly added to a `conf.d` file. Rollback is deleting that entry — no schema migration,
  since `UpstreamKind::Gemini` is a brand-new variant nobody has existing config for.
- **Rollback procedure**: delete the `[[upstreams]]` block with `kind = "gemini"` (and any
  `[[routes.upstreams]]` reference to it) from the operator's `conf.d`; restart consolette. No data
  to migrate back.
- **Staged rollout**: non-streaming text (Phase 1, **including** the three-way error
  classification/dashboard work and the `thought_signature` scaffolding) → streaming (Phase 2) →
  tool calls, where `thought_signature` round-tripping becomes load-bearing (Phase 3). Each phase
  is independently testable and shippable — Phase 1 alone is a complete, useful feature (text-only
  Gemini chat through the router) before Phase 2/3 begin.
- **Go/no-go checkpoint after Phase 1, before starting Phase 2** (pre-mortem P1 #3 — "no refresh
  mechanism, token expiry likely frequent under this machine's SSH/headless usage pattern per
  `antigravity-cli#57`'s documented once-per-session re-auth forcing"): ADR-001 already rules out
  the only way to *implement* a refresh path (a standalone OAuth refresh-token grant needs a
  client_id extracted from the `agy` binary — exactly the harvested-credential approach
  requirements.md's Constraints section forbids), so this cannot be engineered away. Instead: run
  Phase 1 for real, day-to-day use for at least a week (including normal SSH sessions) before
  investing Phase 2/3's remaining time, and observe the actual re-auth cadence (how often
  `status-auth-required` appears and how often Tyler has to run `antigravity-cli`/reopen the IDE).
  If it needs interactive re-auth more than roughly once a day, that's a strong signal the
  "unified routing" value proposition doesn't hold under this constraint — stop and reassess before
  sinking the remaining Large-appetite budget into streaming/tool-calls, rather than discovering
  this only after all three phases ship. Record the observed cadence in a follow-up note to this
  plan either way.
- **Real-usage check-in, 2-4 weeks after Phase 1 ships (added 2026-09-04, Phase 4 product-lens
  repair loop — closes pre-mortem P2 #5)**: Story 1.8.1's example config ships `Strategy::Fallback`
  with Gemini listed last specifically to bound blast radius from the accepted protocol/ToS risk —
  but on a single-operator setup that also means Gemini is only ever invoked when both
  Anthropic and Bedrock fail, i.e. rarely. Set a concrete 2-4 week checkpoint against
  `requirements.md`'s new usage/value-realization success metric (real, non-test traffic on the
  `gemini` per-upstream request counter): if it's still near-zero, decide explicitly whether to (a)
  shift real weight toward Gemini — e.g. via `routing/session_overrides.rs`'s existing per-session
  override, or reconfiguring the route to `Strategy::Weighted` — or (b) accept the safety-net
  framing deliberately (Gemini stays a rarely-exercised fallback, not a day-to-day driver). Either
  outcome is fine; what this checkpoint prevents is the appetite silently buying a mostly-idle
  safety net without anyone having actually decided that's the goal.
- **Superseded (2026-09-04): "Code-hotspot checkpoint before Phase 2 starts."** This plan
  originally deferred the single-file-vs-submodule decision to a `code-hotspot-analysis` checkpoint
  run on `src/providers/gemini.rs` right before Phase 2 started, splitting into a
  `providers/gemini/{mod,translate,stream,tools,error}.rs` tree only if the file had grown past a
  size/responsibility count comparable to `anthropic.rs`/`openai.rs`. Tyler decided instead to split
  into the submodule tree upfront, from the start of Phase 1 — see the "Module organization" row in
  Pattern Decisions above and the per-Story/Task `Files:` paths throughout this plan, which already
  target `src/providers/gemini/mod.rs`/`translate.rs`/`tools.rs`/`error.rs` (and `stream.rs` from
  Phase 2). The hotspot-analysis checkpoint itself is now moot — there is no single-file state left
  to measure — so it is not carried forward as a Phase 2 gate.
- **Accepted risk, not engineered around**: Google has run documented mass account suspensions for
  third-party Antigravity/Gemini-CLI tool usage (`research/pitfalls.md`, `google-gemini/gemini-cli`
  Discussion #20632). Tyler was shown this evidence directly and chose to proceed for personal use.
  This plan does not attempt TLS-fingerprint evasion or other adversarial-hardening work (explicitly
  out of scope per `research/pitfalls.md`'s recommendation #5) — the only engineering response is
  detecting-and-failing-closed (Phase 1's error classification work), not preventing the suspension.

## Unresolved Questions
- [ ] **Exact `gemini-3-pro` max-output-token ceiling** (`GEMINI_3_PRO_OUTPUT_CEILING`) is not
      confirmed by any research pass — blocks Story 1.3.2 — owner: Tyler, resolve by reading the
      real `v1internal:fetchAvailableModels` response (Story 1.3.4 already calls this endpoint for
      `list_models`) or Google's Gemini 3 Pro docs page before hardcoding a number; do not guess.
- [ ] **Whether official `antigravity-cli`-issued tokens tolerate imperfect header fidelity**
      (exact `User-Agent` version string, `X-Goog-Api-Client` value) is unverified — no live call
      was possible during research (no valid token was available) — blocks Story 1.3.1's exact
      header values — owner: Tyler, resolve during Story 1.3.4's manual first-integration-test task
      by trying the researched header values first and adjusting from whatever error (if any) comes
      back.
- [ ] **Whether streaming chunk-level schema drift needs full consecutive-failure-counter cooldown
      logic** (ADR-002's rejected-for-now option (b)) — blocks Story 2.2.1 — owner: Tyler, resolve
      by observing real behavior after Phase 1+2 ship; only add the counter if single-bad-chunk
      false-positive cooldowns are actually seen in practice.
- [ ] **Dynamic `loadCodeAssist` project-id fallback** (ADR-003's rejected alternative) — not
      blocking any Phase 1-3 story — owner: Tyler, build only if the config-supplied `project_id`
      proves insufficient (e.g. a future multi-project Antigravity setup).
- [ ] **`keyring`-crate/`SecretRef::Keyring` fallback** (ADR-001's rejected alternative) — not
      blocking any Phase 1-3 story — owner: Tyler, build only if a real keyring-stored Antigravity
      token is ever actually observed on a real machine (today's live `secret-tool search` found
      none).
- [ ] **Exact HTTP method for `fetchAvailableModels`** (Task 1.3.4d) — "verify exact HTTP method
      during implementation, not assumed here" — blocks Story 1.3.4's `list_models()` task — owner:
      Tyler, resolve during Task 1.3.4d/1.3.4f's implementation by trying `POST` first (matching
      `generateContent`'s method) and falling back to `GET` if that 405s.
- [ ] **Whether `extract_session_id`'s `metadata.user_id` is genuinely per-conversation (not
      per-account/per-client)** — `src/routing/session_overrides.rs:4-13`'s own doc comment states
      this has never been verified against live Claude Code traffic. `ThoughtSignatureCache`'s
      cross-conversation isolation (Epic 1.6) relies on this being a real per-conversation key —
      if it's actually per-account (e.g. one fixed value for all of Tyler's sessions), two
      concurrent conversations could still collide on the same cache entry. Not blocking Phase
      1-3 (single-operator, low concurrency in practice), but — owner: Tyler, verify empirically
      during Story 1.6.1's implementation (log the extracted `session_key` across two concurrent
      Claude Code sessions and confirm they differ) before relying on it for real isolation
      guarantees; found by the Phase 3 adversarial-review repair-loop re-check, 2026-09-04.

---

## Dependency Visualization

```
Phase 1 — Non-streaming text + three-way error classification
┌─────────────────────────────────────────────────────────────────────┐
│ Epic 1.1  Config & routing skeleton                                 │
│   Story 1.1.1 UpstreamKind::Gemini variant                          │
│   Story 1.1.2 Exhaustive match arms (build_providers, kind_label)   │
└───────────────┬───────────────────────────────────────────────────┬─┘
                │                                                    │
                ▼                                                    ▼
┌───────────────────────────────┐                  ┌─────────────────────────────────┐
│ Epic 1.2  Auth wrapper (ADR-001)│                 │ Epic 1.7  project_id (ADR-003)   │
│  1.2.1 wrapper script           │                 │  1.7.1 project_id field          │
│  1.2.2 expiry fail-closed        │                 └────────────────┬─────────────────┘
└───────────────┬─────────────────┘                                  │
                │                                                    │
                └───────────────┬───────────────────────────────────┘
                                 ▼
                  ┌────────────────────────────────────────┐
                  │ Epic 1.3  GeminiProvider translation    │
                  │  1.3.1 request translation              │
                  │  1.3.2 response translation + finishReason│
                  │  1.3.3 error classification              │
                  │  1.3.4 send() wiring + list_models        │
                  │   1.3.4c ◀┄┄┄ atomic unit with Story 1.4.1┊│
                  └───────────────────┬────────────────────┼──┘
                                       │                    ┊
                 ┌─────────────────────┼─────────────────────┐  ┊ (out-of-order
                 ▼                     ▼                     ▼  ┊  dependency —
   ┌─────────────────────┐ ┌───────────────────────┐ ┌────────────────────┐  see ★ below)
   │ Epic 1.4  Drift +    │ │ Epic 1.6  thought_sig  │ │ Epic 1.8  Config    │
   │  cooldown (ADR-002)  │ │  scaffolding (field,   │ │  example (00-providers.toml)│
   │  1.4.1★ new variant + │ │  ToolUseId newtype)    │ └────────────────────┘
   │   classifier + entrypoint/errors.rs mapping ┄┄┄┄┄┄┄┄┄┄┘
   │  1.4.2 strict parse   │
   │  1.4.3 Router arm     │
   │  1.4.4 real error-kind│
   │        attribution    │
   └──────────┬────────────┘
              ▼
   ┌───────────────────────────┐
   │ Epic 1.5  Dashboard 3-way  │
   │  1.5.1 real cooldown feed  │
   │  1.5.2 new status classes  │
   └───────────────────────────┘
```

★ **Story 1.4.1 is drawn inside Epic 1.4 for narrative grouping (it's the first
of that epic's four drift/cooldown stories), but it is a compile-time
prerequisite of Task 1.3.4c, not something that happens after Epic 1.3
finishes.** Adding the `ProviderError::ResponseShapeMismatch` variant
anywhere in the crate immediately breaks the two exhaustive matches in
`src/entrypoint/errors.rs` (Blocker fixed by Task 1.4.1c below) — so Story
1.4.1 (Tasks 1.4.1a-c: the variant, its classifier, and the `errors.rs`
arms) and Task 1.3.4c (the first call site that constructs the variant) must
land together as one atomic unit of work, regardless of which Epic number
each is filed under. A fresh subagent implementing Epic 1.3 in isolation
must implement Story 1.4.1 first (or as part of the same change) rather than
assuming Epic 1.4 already landed.

```
        ═══════════ Phase 1 ships (independently useful) ═══════════

Phase 2 — Streaming (depends on all of Phase 1's Epic 1.3)
┌────────────────────────────┐   ┌─────────────────────────────┐
│ Epic 2.1  SSE translation   │──▶│ Epic 2.2  Streaming drift    │
│  2.1.1 GeminiToAnthropicStream│ │  2.2.1 chunk-parse failure    │
│  2.1.2 send() stream:true wiring│ │        handling               │
└────────────────────────────┘   └─────────────────────────────┘

        ═══════════ Phase 2 ships ═══════════

Phase 3 — Tool calls (depends on Phase 1's Epic 1.6 scaffolding + Phase 2's stream struct)
┌────────────────────┐   ┌──────────────────────────┐   ┌───────────────────────────┐
│ Epic 3.1  Schema     │──▶│ Epic 3.2  Tool round-trip │──▶│ Epic 3.3  thought_signature│
│  sanitization        │   │  3.2.1 request direction   │   │  load-bearing              │
│  3.1.1 sanitize_      │   │  3.2.2 response direction  │   │  3.3.1 stash + replay       │
│  function_schema      │   │        + GeminiToolCallState│   │  3.3.2 hard-error surfacing │
└────────────────────┘   └──────────────────────────┘   └───────────────────────────┘

        ═══════════ Phase 3 ships — full gemini-provider feature complete ═══════════
```

---

## Phase 1: Non-streaming text completions + three-way error classification

### Epic 1.1: Config schema & routing wiring skeleton
**Goal**: `UpstreamKind::Gemini` exists, compiles, and both exhaustive match sites accept it —
with a stub provider — before any real translation logic is written, so every later Epic in this
phase builds on top of a wired-but-inert skeleton.

#### Story 1.1.1: Add `UpstreamKind::Gemini` config variant
**As a** consolette operator, **I want** a `kind = "gemini"` upstream to parse successfully,
**so that** I can start configuring a Gemini upstream in `conf.d` before any translation code exists.
**Acceptance Criteria**:
- A TOML fragment `[[upstreams]]\nname = "gemini"\nkind = "gemini"\nproject_id = "my-gcp-project"`
  parses into `Upstream { kind: UpstreamKind::Gemini { project_id: "my-gcp-project".to_string() }, .. }`.
  - *Given* the `UpstreamKind` enum defined at `src/config/schema.rs:107-120`, *When* that TOML
    fragment is deserialized via `toml::from_str::<Config>`, *Then* the resulting
    `Config.upstreams[0].kind` matches `UpstreamKind::Gemini { project_id }` where
    `project_id == "my-gcp-project"`.
- An upstream with `kind = "gemini"` and no `project_id` field fails to parse (required field).
  - *Given* the TOML fragment `[[upstreams]]\nname = "gemini"\nkind = "gemini"`, *When* deserialized,
    *Then* `toml::from_str::<Config>` returns `Err` (missing required field `project_id`).
**Files**: `src/config/schema.rs`

##### Task 1.1.1a: Add the `Gemini { project_id: String }` variant (~3 min)
- Add `Gemini { project_id: String },` to the `UpstreamKind` enum at `src/config/schema.rs:107-120`,
  after the existing `Openai { base_url: String }` arm.
- No `#[serde(default)]` on `project_id` — it's required (ADR-003).
- Files: `src/config/schema.rs`

##### Task 1.1.1b: Add parity fixture + unit test (~4 min)
- Add `tests/fixtures/toml_parity/gemini_upstream.toml` mirroring
  `tests/fixtures/toml_parity/bedrock_upstream.toml`'s structure but with `kind = "gemini"` and a
  `project_id` field.
- Add a `#[test]` in `src/config/schema.rs`'s existing test module asserting the two acceptance
  criteria above (parses with `project_id`, fails without it).
- Files: `tests/fixtures/toml_parity/gemini_upstream.toml`, `src/config/schema.rs`

#### Story 1.1.2: Wire the two exhaustive match sites
**As a** consolette maintainer, **I want** the compiler to force every `UpstreamKind::Gemini`
match site to be handled, **so that** forgetting to wire Gemini into routing or the dashboard is a
compile error, not a silent runtime gap.
**Acceptance Criteria**:
- `build_providers` constructs *some* `Arc<dyn Provider>` for a `kind = "gemini"` upstream (a stub
  returning `ProviderError::ModelUnsupported` is an acceptable intermediate state at this task).
  - *Given* a `Config` with one upstream `Upstream { name: "gemini", kind: UpstreamKind::Gemini { project_id: "p1" }, auth: None }`,
    *When* `build_providers(&config)` is called, *Then* it returns `Ok(vec![("gemini".to_string(), <some Arc<dyn Provider>>)])`
    without panicking or returning `Err`.
- `upstream_kind_label` returns `"gemini"` for the new variant.
  - *Given* `UpstreamKind::Gemini { project_id: "p1".to_string() }`, *When*
    `upstream_kind_label(&kind)` is called, *Then* it returns `"gemini"`.
**Files**: `src/routing/router.rs`, `src/entrypoint/mod.rs`, `src/providers/gemini/mod.rs` (new, module
skeleton + stub only), `src/providers/gemini/translate.rs` (new, empty), `src/providers/gemini/tools.rs`
(new, empty), `src/providers/gemini/error.rs` (new, empty)

##### Task 1.1.2a: Create the `gemini` submodule tree with a stub `GeminiProvider` (~6 min)
- **Module-skeleton task (per Tyler's upfront-split decision, 2026-09-04 — see the "Module
  organization" Pattern Decisions row and the superseded Risk Control bullet)**: create
  `src/providers/gemini/` as a directory, not a single `gemini.rs` file, from this first task
  onward.
- Create `src/providers/gemini/mod.rs` with `mod translate; mod tools; mod error;` (note: **no**
  `mod stream;` yet — `stream.rs` doesn't exist until Story 2.1.1 populates it in Phase 2; declaring
  an empty placeholder module now would just be dead weight until then), a minimal `GeminiProvider`
  struct (no fields yet beyond what's needed to compile) and a stub `Provider` impl: `send()`
  returns `Err(ProviderError::ModelUnsupported("gemini stub not yet implemented".to_string()))`,
  `list_models()` returns `Ok(vec![])`, `name()` returns `"gemini"`.
- Create `src/providers/gemini/translate.rs`, `src/providers/gemini/tools.rs`,
  `src/providers/gemini/error.rs` as empty files (populated starting in Epic 1.3/Epic 1.6/Story
  1.3.3 respectively) — they must exist for `mod.rs`'s `mod` declarations to compile.
- Add `pub mod gemini;` to `src/providers/mod.rs:9-11` — unchanged text, now resolving to the
  `gemini/mod.rs` directory-module form instead of a bare `gemini.rs` file (Rust treats both
  identically at the call site: `crate::providers::gemini::GeminiProvider` still resolves,
  satisfying `build_providers`' import — see Task 1.1.2b, unchanged).
- Files: `src/providers/gemini/mod.rs`, `src/providers/gemini/translate.rs`,
  `src/providers/gemini/tools.rs`, `src/providers/gemini/error.rs`, `src/providers/mod.rs`

##### Task 1.1.2b: Wire `build_providers`'s new match arm (~3 min)
- Add `UpstreamKind::Gemini { .. } => Arc::new(GeminiProvider::stub(Arc::new(upstream.clone()))),`
  to the match in `build_providers` at `src/routing/router.rs:63-80` (exact constructor signature
  finalized in Story 1.3.4 — this task just needs it to compile and satisfy exhaustiveness).
- Add `use crate::providers::gemini::GeminiProvider;` to the imports at `src/routing/router.rs:17-25`.
- Files: `src/routing/router.rs`

##### Task 1.1.2c: Wire `upstream_kind_label`'s new match arm (~2 min)
- Add `UpstreamKind::Gemini { .. } => "gemini",` to `src/entrypoint/mod.rs:121-127`.
- Files: `src/entrypoint/mod.rs`

##### Task 1.1.2d: Confirm `Router::from_config`'s `can_cooldown` list is untouched (~2 min)
- Verify (via a code comment, not a code change) that the `bedrock_indices` /
  `set_can_cooldown(idx, false)` loop at `src/routing/router.rs:137-148` is **not** extended to
  include Gemini indices — Gemini is a real network upstream, unlike Bedrock's
  local-credential-issue failure mode, so cooldown must apply normally.
- Add a one-line comment above that loop: `// Gemini is a real network upstream — do NOT add its
  indices here (see project_plans/gemini-provider/implementation/plan.md Story 1.1.2).`
- Files: `src/routing/router.rs`

---

### Epic 1.2: Auth wrapper script (ADR-001)
**Goal**: `AuthMethod::Exec` pointed at a new wrapper script successfully produces the headers
`GeminiProvider` needs, independently testable via `echo '{...}' | ./antigravity-token-auth.py`
without touching any Rust code.

#### Story 1.2.1: `antigravity-token-auth.py` reads the token file and emits headers
**As a** `GeminiProvider` instance, **I want** a credential helper that resolves the Antigravity
OAuth token into request headers, **so that** I can reuse `AuthMethod::Exec`/`apply_auth_headers`
unchanged (ADR-001).
**Acceptance Criteria**:
- Given a valid, unexpired token file, the script emits one JSON line with `Authorization`,
  `X-Goog-Api-Client`, and `Client-Metadata` headers, and exits 0.
  - *Given* `~/.gemini/antigravity-cli/antigravity-oauth-token` containing
    `{"token":{"access_token":"ya29.abc123","token_type":"Bearer","refresh_token":"1//xyz","expiry":"2027-01-01T00:00:00Z"},"auth_method":"consumer"}`,
    *When* `echo '{"upstream":"gemini","method":"POST","url":"https://cloudcode-pa.googleapis.com/v1internal:generateContent"}' | references/bin/antigravity-token-auth.py`
    is run, *Then* stdout is exactly one JSON line matching
    `{"headers":{"Authorization":"Bearer ya29.abc123","X-Goog-Api-Client":"google-cloud-sdk vscode_cloudshelleditor/0.1","Client-Metadata":"{\"ideType\":\"ANTIGRAVITY\",\"platform\":\"LINUX\",\"pluginType\":\"GEMINI\"}"}}`
    and the process exits 0.
- Given a missing token file, the script exits non-zero with no stdout.
  - *Given* `~/.gemini/antigravity-cli/antigravity-oauth-token` does not exist, *When* the script
    runs, *Then* it exits with a non-zero status and prints nothing to stdout (stderr may contain a
    human-readable message, which `run_helper`'s ADR-007 §6 contract already discards from logs).
**Files**: `references/bin/antigravity-token-auth.py` (new)

##### Task 1.2.1a: Write the token-file read + header-emit logic (~5 min)
- Create `references/bin/antigravity-token-auth.py` (executable, `chmod +x`, shebang
  `#!/usr/bin/env python3`, stdlib-only: `json`, `sys`, `os`, `pathlib`, `datetime`).
- Read `~/.gemini/antigravity-cli/antigravity-oauth-token`, parse JSON, extract
  `token.access_token`. Ignore stdin content beyond reading and discarding it (ADR-007 §2 says the
  helper "can ignore" the request context).
- Emit `{"headers": {"Authorization": "Bearer <token>", "X-Goog-Api-Client": "...", "Client-Metadata": "..."}}`
  on stdout, exit 0.
- Files: `references/bin/antigravity-token-auth.py`

##### Task 1.2.1b: Handle missing/unparseable file (~3 min)
- Wrap the file read + JSON parse in `try/except (FileNotFoundError, json.JSONDecodeError, KeyError)`;
  on any of these, print a message to stderr and `sys.exit(1)` with no stdout.
- Files: `references/bin/antigravity-token-auth.py`

#### Story 1.2.2: Expiry detection and actionable fail-closed error
**As** Tyler, **I want** a clear, actionable error when my Antigravity token has expired, **so that**
I know to re-run `antigravity-cli`/reopen the IDE instead of staring at an opaque 401.
**Acceptance Criteria**:
- Given an expired token, the script exits non-zero (never emits stale headers as if they were valid).
  - *Given* the token file's `expiry` field is `"2026-08-20T00:00:00Z"` and "today" is 2026-09-04,
    *When* the script runs, *Then* it exits non-zero and prints
    `antigravity-cli token expired at 2026-08-20T00:00:00Z — run 'antigravity-cli login' (or reopen the Antigravity IDE) to mint a fresh token`
    to stderr, with no stdout.
- The resulting `ProviderError::Auth` message (surfaced up through `AuthError::Exec` →
  `ProviderError::Auth`, `src/providers/mod.rs:57-61`) is logged at `tracing::error!`, distinct from
  a request-level 401 from Gemini itself.
  - *Given* `GeminiProvider::build_headers` calls `apply_auth_headers` and the exec helper exits
    non-zero, *When* `Router::dispatch` records the resulting `ProviderError::Auth(..)` via
    `record_attempt` (`src/routing/router.rs:361-386`), *Then* a `tracing::error!` line distinct
    from ordinary request-level logging is emitted (Task 1.2.2b) — conceptually parallel to
    `BedrockProvider::do_sso_login` (`src/providers/bedrock.rs:572-605`) in giving an actionable
    re-auth message on failure, but with no inline-reauth-attempt branch (unlike Bedrock's has-TTY
    fork), since `agy` needs a real browser session consolette can't drive headlessly.

**Note (accepted simplification for v1, not an oversight — adversarial-review Concern)**: the
per-cause messages Task 1.2.2a's script prints to stderr (missing token file / expired /
unparseable) are **debug-only** — they are only visible if Tyler runs
`antigravity-token-auth.py` by hand. `run_helper` (`src/auth/exec.rs`, unchanged by ADR-001)
never surfaces helper stdout/stderr content in consolette's own error text or logs (ADR-007 §6);
a non-zero exit becomes only `"{command} exited with status {:?}"`. So Task 1.2.2b's generic
Rust-side message is what actually reaches the operator's logs/dashboard, and
`research/pitfalls.md`'s recommendation to distinguish "no token source" from "token
rejected/expired" is **not** surfaced distinctly anywhere in the running system — both collapse to
one generic auth-failure state. This plan deliberately does **not** thread a coarse cause-code
(e.g. an extra JSON field on success, or distinct exit codes on failure) through `run_helper` to
recover that distinction, because doing so would require changing `src/auth/exec.rs`'s
non-zero-exit handling — which ADR-001 explicitly commits to leaving unchanged (see ADR-001
Rationale #3) — for a debug convenience Tyler can already get by running the script directly. If
this proves insufficient in practice, revisit as a small, separate follow-up to ADR-007's generic
exec-helper contract, not a Gemini-specific carve-out.
**Files**: `references/bin/antigravity-token-auth.py`, `src/providers/gemini/mod.rs`

##### Task 1.2.2a: Add expiry comparison to the wrapper script (~4 min)
- Parse `token.expiry` (RFC3339, normalize a trailing `Z` to `+00:00` for
  `datetime.fromisoformat` compatibility on Python <3.11), compare to `datetime.now(timezone.utc)`.
- If expired, print the actionable message shown in the acceptance criterion above to stderr, exit
  non-zero (no stdout).
- Files: `references/bin/antigravity-token-auth.py`

##### Task 1.2.2b: Log-distinctly on `GeminiProvider`'s side (~3 min)
- In `GeminiProvider::build_headers` (Story 1.3.1), when `apply_auth_headers` returns
  `Err(ProviderError::Auth(msg))`, add a
  `tracing::error!(upstream = "gemini", %msg, "gemini upstream: token refresh failed — run 'antigravity-cli login' or reopen the Antigravity IDE to mint a fresh token")`
  line before propagating the error — distinct in both level and message from the generic
  `tracing::warn!` already emitted inside `src/auth/exec.rs:241-246,254-259`. **The exact
  remediation command must be inlined in this Rust-side message** (not just the script name) —
  this is the only place it reliably reaches consolette's own logs, since `run_helper` never
  surfaces the helper script's own stderr text (ADR-007 §6, see the Note above). Fixes a
  cross-artifact-consistency BLOCKER: `design/ux.md`'s log sample and UX Acceptance Criterion #3
  promise Tyler can `grep` consolette's logs for the remediation command — the original
  "token refresh failed via antigravity-token-auth.py" wording (naming only the script, not the
  fix) did not satisfy that promise.
- Files: `src/providers/gemini/mod.rs` (`build_headers` lives on `GeminiProvider` in `mod.rs`)

---

### Epic 1.3: `GeminiProvider` non-streaming request/response translation
**Goal**: A real (non-stub) `GeminiProvider::send()` completes one full round trip: Anthropic
Messages request in, Cloud Code Assist call out, Anthropic Messages response out — text only, no
tool calls yet.

#### Story 1.3.1: Request translation (Anthropic → Gemini-native → `CloudCodeEnvelope`)
**As a** consolette router, **I want** an Anthropic-shaped request body translated into the exact
two-layer Cloud Code Assist envelope, **so that** the Gemini upstream accepts the call.
**Acceptance Criteria**:
- An Anthropic request with a `system` string and one user text message translates to a
  `CloudCodeEnvelope` with `request.systemInstruction` as an **object**, not a string (stack.md:
  "a plain string 400s").
  - *Given* the Anthropic request body
    `{"model":"gemini-3-pro","system":"You are terse.","messages":[{"role":"user","content":"hi"}],"max_tokens":1024}`,
    *When* `translate_anthropic_request_to_gemini(&body, "my-gcp-project")` is called, *Then* it
    returns `Ok(envelope)` where `envelope.request.systemInstruction` equals
    `{"parts":[{"text":"You are terse."}]}` (an object, never `"You are terse."` as a bare string)
    and `envelope.request.contents` equals `[{"role":"user","parts":[{"text":"hi"}]}]`.
- `max_tokens`/`temperature` map onto `generationConfig`.
  - *Given* the same request body with `"max_tokens":1024,"temperature":0.5`, *When* translated,
    *Then* `Ok(envelope)` has `envelope.request.generationConfig` equal to
    `{"maxOutputTokens":1024,"temperature":0.5}` (only fields actually present in the Anthropic
    request are included — no `topP`/`topK`/`stopSequences` keys when the Anthropic request didn't
    specify them).
- The envelope's `project` field comes from `UpstreamKind::Gemini::project_id`, not the request body.
  - *Given* `project_id = "my-gcp-project"` on the configured upstream, *When*
    `translate_anthropic_request_to_gemini` builds the envelope, *Then* the resulting `Ok(envelope)`
    has `envelope.project == "my-gcp-project"` and `envelope.requestType == "agent"`.
- The function is fallible (`Result`-returning) from Phase 1 onward, even though every Phase 1/2
  input returns `Ok` — Phase 3's Story 3.3.2 needs an `Err` path (a `tool_use` id with no cached
  `thought_signature`), and designing the signature fallible now avoids a breaking-change signature
  swap later (a one-call-site churn the architecture review flagged as avoidable).
  - *Given* any Phase 1/2 input (no tool calls), *When* translated, *Then* the result is always
    `Ok(..)`, never `Err(..)` — Phase 1/2 code paths cannot yet produce the Phase-3-only error case.
**Files**: `src/providers/gemini/translate.rs`

##### Task 1.3.1a: Define `CloudCodeEnvelope`, `GeminiContent`, `GeminiPart` (text-only), `GeminiGenerationConfig` structs (~5 min)
- Add `#[derive(Serialize)]` structs in `src/providers/gemini/translate.rs`: `CloudCodeEnvelope { project,
  model, request_type, user_agent, request: GeminiRequest }`, `GeminiRequest { contents:
  Vec<GeminiContent>, system_instruction: Option<GeminiSystemInstruction>, generation_config:
  Option<GeminiGenerationConfig> }`, `GeminiContent { role: String, parts: Vec<GeminiPart> }`,
  `GeminiPart { text: Option<String> }` (functionCall/functionResponse variants added in Phase 3),
  `GeminiSystemInstruction { parts: Vec<GeminiPart> }`, `GeminiGenerationConfig { max_output_tokens:
  Option<u32>, temperature: Option<f64>, top_p: Option<f64>, top_k: Option<u32>, stop_sequences:
  Option<Vec<String>> }` with `#[serde(rename_all = "camelCase", skip_serializing_if =
  "Option::is_none")]` throughout.
- Files: `src/providers/gemini/translate.rs`

##### Task 1.3.1b: Write `translate_anthropic_request_to_gemini` (text-only, `Result`-returning) (~5 min)
- Pure fn: `fn translate_anthropic_request_to_gemini(anthropic: &Value, project_id: &str) ->
  Result<CloudCodeEnvelope, ProviderError>` — **fallible from Phase 1**, always returning `Ok(..)`
  until Phase 3's Story 3.3.2 needs the `Err` path (a missing cached `thought_signature`). Designing
  it fallible now avoids the breaking one-call-site signature change the architecture review
  flagged between the old Phase 1 (infallible) and Phase 3 (`Result`) shapes.
- Map `messages[].role` (`"user"`→`"user"`, `"assistant"`→`"model"`), `messages[].content` (string
  or array of `{"type":"text","text":...}` blocks, reusing `extract_text_from_content`'s pattern
  from `src/providers/mod.rs:253-265` as a starting point but producing `Vec<GeminiPart>` instead of
  a joined string — one `GeminiPart{text}` per text block).
- Map `system` (Anthropic's top-level `system` string) into `GeminiSystemInstruction`.
- Map `max_tokens`/`temperature`/`top_p`/`stop_sequences` into `GeminiGenerationConfig`, omitting
  absent fields.
- Files: `src/providers/gemini/translate.rs`

##### Task 1.3.1c: Unit tests for Story 1.3.1's acceptance criteria (~4 min)
- Add `#[cfg(test)] mod tests` cases (in `src/providers/gemini/translate.rs`, following the existing
  `openai.rs:539` / `anthropic.rs:644` inline-test-module convention) for each Given-When-Then
  above.
- Files: `src/providers/gemini/translate.rs`

#### Story 1.3.2: Response translation, including `finishReason` mapping
**As a** consolette router, **I want** a Gemini response translated back into Anthropic Messages
shape, **so that** Claude Code (or any Anthropic-API client) can consume it unchanged.
**Acceptance Criteria**:
- A `STOP` finish reason maps to Anthropic's `end_turn`.
  - *Given* the Gemini response
    `{"candidates":[{"content":{"role":"model","parts":[{"text":"hello"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}`,
    *When* `translate_gemini_response_to_anthropic(&response, "gemini-3-pro")` is called, *Then*
    the result's `content` equals `[{"type":"text","text":"hello"}]`, `stop_reason` equals
    `"end_turn"`, and `usage` equals `{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}`.
- A `SAFETY` finish reason maps to `end_turn` plus synthesized explanatory text (decision #3), not
  a hard error.
  - *Given* the Gemini response
    `{"candidates":[{"content":{"role":"model","parts":[]},"finishReason":"SAFETY"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":0,"totalTokenCount":10}}`,
    *When* translated, *Then* `stop_reason` equals `"end_turn"` and `content` equals
    `[{"type":"text","text":"[Gemini stopped generating: response blocked by safety filters (finishReason=SAFETY)]"}]`
    — never a `ProviderError::Validation`.
- `MAX_TOKENS` maps to Anthropic's `max_tokens` stop reason.
  - *Given* a response identical to the first example but `"finishReason":"MAX_TOKENS"`, *When*
    translated, *Then* `stop_reason` equals `"max_tokens"`.
**Files**: `src/providers/gemini/translate.rs`

##### Task 1.3.2a: Define `GeminiGenerateContentResponse`/`GeminiUsageMetadata` typed structs (~4 min)
- `#[derive(Deserialize)]` structs for strict parsing (ADR-002 requires typed, not lenient,
  deserialization): `GeminiGenerateContentResponse { candidates: Vec<GeminiCandidate>,
  usage_metadata: Option<GeminiUsageMetadata> }`, `GeminiCandidate { content: GeminiContent,
  finish_reason: Option<String> }`, `GeminiUsageMetadata { prompt_token_count: u64,
  candidates_token_count: u64, cached_content_token_count: Option<u64>, thoughts_token_count:
  Option<u64>, total_token_count: u64 }`, all `#[serde(rename_all = "camelCase")]`.
- Files: `src/providers/gemini/translate.rs`

##### Task 1.3.2b: Write `map_gemini_finish_reason` (~3 min)
- `fn map_gemini_finish_reason(reason: Option<&str>) -> (&'static str, Option<String>)` returning
  `(stop_reason, synthesized_text_if_any)`: `"STOP"` → `("end_turn", None)`, `"MAX_TOKENS"` →
  `("max_tokens", None)`, `"SAFETY"` → `("end_turn", Some("[Gemini stopped generating: response
  blocked by safety filters (finishReason=SAFETY)]".to_string()))`, `"RECITATION"` → analogous
  message naming RECITATION, `_`/`None` → `("end_turn", None)`.
- Files: `src/providers/gemini/translate.rs`

##### Task 1.3.2c: Write `translate_gemini_response_to_anthropic` (~5 min)
- Pure fn assembling the Anthropic response shape: `content` (from `GeminiPart.text` values, plus
  any synthesized safety/recitation text appended as its own block), `stop_reason` (from Task
  1.3.2b), `usage` (map `GeminiUsageMetadata` fields onto the existing `AnthropicUsage`-shaped JSON,
  `cache_creation_input_tokens`/`cache_read_input_tokens` hardcoded to `0`), `model` (echoed from
  the request), `role: "assistant"`.
- Files: `src/providers/gemini/translate.rs`

##### Task 1.3.2d: Unit tests for Story 1.3.2's acceptance criteria (~4 min)
- Files: `src/providers/gemini/translate.rs`

#### Story 1.3.3: Error classification (`classify_gemini_error`)
**As a** `Router`, **I want** every Gemini failure mode mapped onto the existing `ProviderError`
vocabulary, **so that** `Router::dispatch`'s existing branching (validation/auth/rate-limit/transient)
works for Gemini exactly as it does for the other three providers.
**Acceptance Criteria**:
- A 429 with a `retryDelay` in `details[]` maps to `RateLimitedWithRetry`.
  - *Given* HTTP status 429 and body
    `{"error":{"code":429,"message":"Resource exhausted","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"3.957525076s"}]}}`,
    *When* `classify_gemini_error(status, &body)` is called, *Then* it returns
    `ProviderError::RateLimitedWithRetry { retry_after: 4 }` (ceiling of 3.957525076 seconds).
- A 401/403 maps to `Auth`.
  - *Given* HTTP status 401 and body
    `{"error":{"code":401,"message":"Request had invalid authentication credentials.","status":"UNAUTHENTICATED"}}`,
    *When* `classify_gemini_error(status, &body)` is called, *Then* it returns
    `ProviderError::Auth("Request had invalid authentication credentials.".to_string())`.
- A generic 400 maps to `Validation`.
  - *Given* HTTP status 400 and body
    `{"error":{"code":400,"message":"Invalid value at 'request.contents[0].role'","status":"INVALID_ARGUMENT"}}`,
    *When* `classify_gemini_error(status, &body)` is called, *Then* it returns
    `ProviderError::Validation("Invalid value at 'request.contents[0].role'".to_string(), 400)`.
**Files**: `src/providers/gemini/error.rs`

##### Task 1.3.3a: Define the error-body struct and `classify_gemini_error` (~5 min)
- `#[derive(Deserialize)] struct GeminiErrorBody { error: GeminiErrorDetail }`, `struct
  GeminiErrorDetail { code: u16, message: String, status: String, #[serde(default)] details:
  Vec<Value> }`.
- `fn classify_gemini_error(status: StatusCode, body: &GeminiErrorBody) -> ProviderError` mirroring
  `map_error_status`'s status-code branching (`src/providers/anthropic.rs:556-592`): 429 → parse
  `retryDelay` out of `details[]` (regex or manual `"...s"` suffix strip + `f64::parse` + `.ceil()`)
  → `RateLimitedWithRetry`; 401/403 → `Auth`; other 4xx → `Validation`; 5xx → `Upstream`.
- Files: `src/providers/gemini/error.rs`

##### Task 1.3.3b: Unit tests for Story 1.3.3's acceptance criteria (~4 min)
- Files: `src/providers/gemini/error.rs`

#### Story 1.3.4: `GeminiProvider::send()` wiring + `list_models` stub
**As a** `Router`, **I want** `GeminiProvider::new`/`send`/`list_models` fully wired, **so that**
`build_providers`'s stub arm (Task 1.1.2b) becomes the real, working provider.
**Acceptance Criteria**:
- `GeminiProvider::new` follows the ADR-004 two-client split identically to
  `AnthropicProvider::new`/`OpenaiProvider::new`.
  - *Given* `GeminiProvider::new(Arc::new(upstream), Arc::new(resolver), Arc::new(exec_cache), 60)`
    is called with a valid `Upstream`, *Then* it returns `Ok(GeminiProvider { .. })` with
    `base_url == "https://cloudcode-pa.googleapis.com"` and two distinct `reqwest::Client`
    instances (one pooled, one `pool_max_idle_per_host(0)`), matching
    `AnthropicProvider::new`'s shape (`src/providers/anthropic.rs:80-115`).
- `send(body, headers, stream: false)` performs the full round trip against
  `v1internal:generateContent` and returns `ProviderResponse::Full`.
  - *Given* a `GeminiProvider` configured with `project_id = "my-gcp-project"` and a mocked HTTP
    response returning the `STOP`-finish-reason body from Story 1.3.2's first example, *When*
    `send(anthropic_request_body, HeaderMap::new(), false)` is called, *Then* it returns
    `Ok(ProviderResponse::Full(anthropic_shaped_json))` where `anthropic_shaped_json["content"][0]["text"] == "hello"`.
- `list_models()` calls `v1internal:fetchAvailableModels` and returns at least the confirmed model id.
  - *Given* a mocked `fetchAvailableModels` response listing `"gemini-3-pro"`, *When*
    `list_models()` is called, *Then* it returns
    `Ok(vec![ModelInfo { id: "gemini-3-pro".to_string(), owned_by: Some("google".to_string()) }])`.
**Files**: `src/providers/gemini/mod.rs` (struct, constructor, `send`, `build_headers`, `list_models` —
calling into `translate.rs`'s translation fns and `error.rs`'s `classify_gemini_error`/`GeminiErrorBody`,
neither of which this story modifies), `src/routing/router.rs`

##### Task 1.3.4a: Replace the stub `GeminiProvider` struct/constructor with the real one (~5 min)
- Fields: `client`, `stream_client`, `base_url: "https://cloudcode-pa.googleapis.com".to_string()`,
  `upstream: Arc<Upstream>`, `resolver: Arc<dyn SecretResolver + Send + Sync>`, `exec_cache:
  Arc<ExecCredentialCache>`, `thought_signatures: ThoughtSignatureCache` (Task 1.6.1b) — identical
  field set/order to `OpenaiProvider` (`src/providers/openai.rs:40-102`) plus the one Gemini-only
  field, minus the `base_url` constructor parameter (hardcoded, per `AnthropicProvider`'s
  precedent).
- `pub fn new(upstream: Arc<Upstream>, resolver: Arc<dyn SecretResolver + Send + Sync>, exec_cache:
  Arc<ExecCredentialCache>, request_timeout_secs: u64) -> Result<Self, ProviderError>`.
- Files: `src/providers/gemini/mod.rs`

##### Task 1.3.4b: Write `build_headers` reusing `apply_auth_headers` verbatim (~4 min)
- `async fn build_headers(&self, url: &str) -> Result<HeaderMap, ProviderError>`: sets
  `Content-Type: application/json`, then calls
  `crate::providers::anthropic::apply_auth_headers(&self.upstream, self.resolver.as_ref(),
  &self.exec_cache, &mut out, url).await?` — the shared free fn (`src/providers/anthropic.rs:390-441`),
  imported via `use crate::providers::anthropic::apply_auth_headers;`.
- Files: `src/providers/gemini/mod.rs`

##### Task 1.3.4c: Write `send()`'s non-streaming path (~5 min)
- POST to `{base_url}/v1internal:generateContent`, body = `serde_json::to_vec(&translate_anthropic_request_to_gemini(&body, &self.upstream_project_id())?)`
  (the `?` reflects Task 1.3.1b's `Result`-returning signature — always `Ok` at this point in the
  plan since no tool-call `Err` path exists until Story 3.3.2).
- On non-2xx: parse into `GeminiErrorBody`, call `classify_gemini_error`. **When the result is
  `ProviderError::Auth(..)` from THIS path specifically** (a real HTTP 401/403 from Gemini itself,
  meaning `apply_auth_headers` already succeeded — i.e. the local token was NOT expired per the
  exec helper's own check in Story 1.2.2), log a message distinct from Story 1.2.2's
  locally-detected-expiry message: e.g.
  `tracing::error!("gemini upstream: request rejected with 401/403 despite a non-expired local token — this may indicate account suspension/revocation (see requirements.md's accepted ToS risk), not routine expiry; running 'antigravity-cli login' will not fix a suspension")`.
  This is the pre-mortem-flagged (P1 #2) fix distinguishing "locally expired, needs routine
  re-auth" (Story 1.2.2's path, fixable by `antigravity-cli login`) from "server rejected a
  token that wasn't locally expired" (this path, a suspension/revocation signal that a re-auth
  loop won't resolve) — the two currently collapse to the same generic auth-failure appearance
  everywhere else (dashboard status, `last_error_kind == "auth"`), so this log line is the only
  place the distinction is preserved. Not a new `ProviderError` variant (would ripple through the
  same exhaustive matches Blocker A already touched) — just a differently-worded log line gated on
  which code path produced the `Auth` variant.
- On 2xx: strict `serde_json::from_slice::<GeminiGenerateContentResponse>` — parse failure becomes
  `ProviderError::ResponseShapeMismatch` (wired fully in Epic 1.4; this task can construct the
  variant inline even before Epic 1.4 lands the classification/cooldown machinery, since the
  variant itself is added in Story 1.4.1, which is a dependency of this task).
- On successful parse: call `translate_gemini_response_to_anthropic`.
- Files: `src/providers/gemini/mod.rs` (calls `translate.rs`'s `translate_anthropic_request_to_gemini`/
  `translate_gemini_response_to_anthropic` and `error.rs`'s `classify_gemini_error`/`GeminiErrorBody`
  via `use` imports; neither submodule is modified by this task)

##### Task 1.3.4d: Write `list_models()` (~4 min)
- POST/GET (per confirmed `fetchAvailableModels` shape — verify exact HTTP method during
  implementation, not assumed here) to `{base_url}/v1internal:fetchAvailableModels`, parse into a
  `Vec<ModelInfo>`.
- Files: `src/providers/gemini/mod.rs`

##### Task 1.3.4e: Wire the real constructor into `build_providers` (~2 min)
- Replace Task 1.1.2b's stub call with
  `UpstreamKind::Gemini { project_id } => Arc::new(GeminiProvider::new(Arc::new(upstream.clone()), Arc::clone(&resolver), Arc::clone(&exec_cache), config.request_timeout)?),`
  at `src/routing/router.rs:63-80`.
- Files: `src/routing/router.rs`

##### Task 1.3.4f: Fixture-based tests for `send()`'s three acceptance criteria (~5 min)
**Rescoped 2026-09-04 (Phase 4 engineering-lens repair loop)**: originally called for "an
integration test using a mocked HTTP server," which contradicts this repo's actual test
convention — no `wiremock`/`httpmock`/`mockito` crate exists in `Cargo.toml`'s dev-dependencies,
and none of `anthropic.rs`/`openai.rs`/`bedrock.rs`'s existing provider tests spin up a real HTTP
listener; they test pure translation functions directly (`validation.md`'s Test Stack Notes
confirms this via `grep -n dev-dependencies -A5 Cargo.toml`). Rather than adding new test tooling
used nowhere else in the codebase, this task instead:
- Tests `translate_anthropic_request_to_gemini`/`translate_gemini_response_to_anthropic`/
  `classify_gemini_error` directly against fixture `Value`s covering Story 1.3.4's three acceptance
  criteria (matching the existing `openai.rs`/`anthropic.rs` inline-test-module convention already
  used elsewhere in this plan).
- Adds one `Router`-level test using a fake `Provider` (existing pattern at
  `src/routing/router.rs:403,430,882`) for the "does the translated response actually flow back
  through `Router::dispatch` correctly" question, in place of a real network round trip.
- A true wire-level HTTP integration test (an actual mocked listener) is explicitly deferred — out
  of scope for this appetite, since it would require new test infrastructure this codebase doesn't
  have anywhere else. The one real network call this feature needs verified is covered manually by
  Task 1.3.4h instead.
- Files: `src/providers/gemini/mod.rs`

##### Task 1.3.4g: PREREQUISITE — ensure a fresh, non-expired Antigravity token exists (unbounded, manual, Tyler-only)
**Added 2026-09-04 (Phase 4 engineering-lens repair loop, per pre-mortem #3)**: the Antigravity
OAuth token found on this machine during Phase 2 research was already expired, and `antigravity-cli`
has no refresh subcommand — acquiring a fresh token requires an interactive browser re-auth (run
`antigravity-cli`/reopen the Antigravity IDE and complete login). This is a manual, unbounded,
Tyler-only step — **not** sized like the rest of this plan's 2-5 min task-sizing convention, and
not something a fresh implementer subagent should assume is quick or automatable. Complete this
before attempting Task 1.3.4h.
- Files: none (operator action, no code change)

##### Task 1.3.4h: Manual first-call verification against the live endpoint (~5 min, requires Task 1.3.4g done first)
- With a confirmed fresh, non-expired token (Task 1.3.4g), manually send one real request against
  the live Cloud Code Assist endpoint and confirm the response, resolving the header-fidelity
  Unresolved Question above (try the researched header values first, adjust from whatever error, if
  any, comes back).
- Files: none (manual verification, no code change)

---

### Epic 1.4: Schema-drift detection & cooldown classification (ADR-002)
**Goal**: A response that doesn't match the documented Gemini shape is surfaced as a genuinely
distinct, cooldown-tripping error — not silently misparsed, not indistinguishable from an ordinary
5xx.

#### Story 1.4.1: `ProviderError::ResponseShapeMismatch` + classification method + entrypoint error mapping
**As a** `GeminiProvider`, **I want** a dedicated error variant for "200 OK but wrong shape," **so
that** downstream code (Router, dashboard, logs) can treat it distinctly from every existing
failure class.

**Execution note (fixes architecture-review Blocker A)**: `ProviderError` is matched exhaustively
(no wildcard arm) in two other live call sites this story does not otherwise touch:
`map_provider_error_anthropic` and `map_provider_error_openai`
(`src/entrypoint/errors.rs:16-65` and `:71-121`, wired into the `/v1/messages` and
`/v1/chat/completions` handlers respectively). Adding a 9th variant here breaks compilation of
`errors.rs` until both arms are extended — Task 1.4.1c below does that in the same story so the
crate never sits in a broken-compile state. See also the ★ note under Dependency Visualization:
this story is a compile-time prerequisite of Task 1.3.4c despite being numbered under Epic 1.4.
**Acceptance Criteria**:
- The new variant exists and `is_response_shape_mismatch()` returns `true` only for it.
  - *Given* `ProviderError::ResponseShapeMismatch("missing field `candidates`".to_string())`,
    *When* `.is_response_shape_mismatch()` is called, *Then* it returns `true`; *When*
    `.is_auth()`, `.is_validation()`, `.is_rate_limited()`, `.is_transient()` are called on the
    same value, *Then* all four return `false`.
- Both `map_provider_error_anthropic` and `map_provider_error_openai` handle the new variant with a
  502 Bad Gateway status, mirroring each function's existing envelope shape.
  - *Given* `ProviderError::ResponseShapeMismatch("missing field `candidates`".to_string())`,
    *When* `map_provider_error_anthropic(&err)` is called, *Then* it returns
    `(StatusCode::BAD_GATEWAY, None, json!({"type":"error","error":{"type":"api_error","message":"unexpected response shape from upstream: missing field `candidates`"}}))`
    (mirroring the existing `Upstream{..}` arm's `api_error` envelope shape).
  - *Given* the same error, *When* `map_provider_error_openai(&err)` is called, *Then* it returns
    `(StatusCode::BAD_GATEWAY, None, envelope("unexpected response shape from upstream: missing field `candidates`", "server_error"))`
    (mirroring the existing `Upstream{..}` arm's `server_error` envelope shape).
  - Rationale for 502 over the 529/`overloaded_error` shape used for `Timeout`/`Exhausted`: 502
    communicates "consolette reached the upstream but the response didn't parse," which is a
    different failure mode than "upstream is overloaded" — ADR-002's own framing (schema drift
    ≠ transient overload).
**Files**: `src/providers/mod.rs`, `src/entrypoint/errors.rs`

##### Task 1.4.1a: Add the variant and classifier (~3 min)
- Add `#[error("unexpected response shape from upstream: {0}")] ResponseShapeMismatch(String),` to
  `ProviderError` at `src/providers/mod.rs:33-55`.
- Add `#[must_use] pub fn is_response_shape_mismatch(&self) -> bool { matches!(self, ProviderError::ResponseShapeMismatch(_)) }`
  next to `is_transient()` at `src/providers/mod.rs:93-99`.
- Files: `src/providers/mod.rs`

##### Task 1.4.1b: Unit test for the classifier (~2 min)
- Files: `src/providers/mod.rs`

##### Task 1.4.1c: Add the `ResponseShapeMismatch` arm to both `entrypoint/errors.rs` mappers (~5 min)
- In `map_provider_error_anthropic` (`src/entrypoint/errors.rs:16-65`), add
  `ProviderError::ResponseShapeMismatch(msg) => (StatusCode::BAD_GATEWAY, None, json!({"type":"error","error":{"type":"api_error","message":msg}})),`
  — same `api_error` envelope shape as the existing `Upstream{..}` arm.
- In `map_provider_error_openai` (`src/entrypoint/errors.rs:71-121`), add
  `ProviderError::ResponseShapeMismatch(msg) => (StatusCode::BAD_GATEWAY, None, envelope(msg, "server_error")),`
  — same `server_error` envelope shape as the existing `Upstream{..}` arm, reusing the file's local
  `envelope()` helper.
- Add two unit tests in `errors.rs`'s existing `#[cfg(test)] mod tests` alongside the current
  `upstream_*`/`openai_upstream_*` tests, asserting both acceptance-criteria examples above.
- Files: `src/entrypoint/errors.rs`

#### Story 1.4.2: Strict typed deserialization in `GeminiProvider`
**As** Tyler, **I want** any Gemini response that doesn't match the documented shape to fail
loudly, **so that** a silent misparse never produces a garbled-but-valid-looking Anthropic
response.
**Acceptance Criteria**:
- A response missing the required `candidates` field returns `ResponseShapeMismatch`, not a panic
  or a defaulted-empty response.
  - *Given* the HTTP 200 body `{"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":0,"totalTokenCount":1}}`
    (no `candidates` key at all), *When* `GeminiProvider::send`'s non-streaming path parses it,
    *Then* it returns `Err(ProviderError::ResponseShapeMismatch(msg))` where `msg` contains the
    `serde_json` deserialization error text (e.g. mentions "missing field `candidates`"), and the
    call never panics.
**Files**: `src/providers/gemini/mod.rs` (`send()`'s parse-failure branch; the
`GeminiGenerateContentResponse` struct it deserializes into is defined in `translate.rs`, unmodified
by this story)

##### Task 1.4.2a: Confirm/finalize the strict-parse error path in `send()` (~3 min)
- Ensure Task 1.3.4c's parse-failure branch uses `serde_json::from_slice::<GeminiGenerateContentResponse>(&bytes).map_err(|e| ProviderError::ResponseShapeMismatch(format!("{e}")))?`
  — no `.unwrap_or_default()` anywhere in this path (the opposite of `bedrock.rs`/`openai.rs`'s
  lenient style, deliberately, per ADR-002).
- Files: `src/providers/gemini/mod.rs`

##### Task 1.4.2b: Unit test for the acceptance criterion (~3 min)
- Files: `src/providers/gemini/mod.rs`

#### Story 1.4.3: `Router::dispatch`'s new cooldown-tripping match arm
**As a** `Router`, **I want** a `ResponseShapeMismatch` to trip an upstream's cooldown immediately,
**so that** a permanently-broken Gemini endpoint isn't retried on every single request forever.
**Acceptance Criteria**:
- A `ResponseShapeMismatch` from the Gemini candidate trips its cooldown for `DRIFT_COOLDOWN_SECS`,
  then fails over to the next candidate.
  - *Given* a `Router` with two candidates `["gemini", "anthropic"]` (Fallback strategy) where the
    `gemini` provider's `send()` always returns `Err(ProviderError::ResponseShapeMismatch("bad shape".to_string()))`
    and the `anthropic` provider's `send()` succeeds, *When* `dispatch(body, headers, false, 100)`
    is called, *Then* the response comes from `anthropic`, and `health.remaining_secs(gemini_index)`
    returns a value `> 0` for at least `DRIFT_COOLDOWN_SECS` seconds afterward (`src/routing/health.rs`
    exposes `remaining_secs`, not an `is_available(idx)` inherent method — check cooldown via
    `remaining_secs(idx) > 0`, matching `HealthRegistry`'s actual public surface).
- The other three providers' existing dispatch behavior is unchanged (regression check).
  - *Given* the existing `dispatch_attributes_per_upstream_metrics_across_a_failover` test at
    `src/routing/router.rs:984+`, *When* the test suite runs after this change, *Then* it still
    passes unmodified.
**Files**: `src/routing/router.rs`, `src/providers/gemini/error.rs` (defines `DRIFT_COOLDOWN_SECS`),
`src/providers/gemini/mod.rs` (re-exports it)

##### Task 1.4.3a: Add the `DRIFT_COOLDOWN_SECS` constant (~2 min)
- `pub(crate) const DRIFT_COOLDOWN_SECS: u64 = 1800;` in `src/providers/gemini/error.rs`,
  module-level — it lives here rather than `mod.rs` because it's ADR-002 schema-drift-cooldown
  policy, the same concern `error.rs` otherwise owns (`classify_gemini_error`/`GeminiErrorBody`).
- Add `pub(crate) use error::DRIFT_COOLDOWN_SECS;` to `src/providers/gemini/mod.rs` so the constant
  is still reachable at `crate::providers::gemini::DRIFT_COOLDOWN_SECS` — the exact path Task
  1.4.3b's `router.rs` code sample already references, unaffected by which submodule actually
  defines it. **`pub(crate)` (not `pub`) is correct and sufficient here** — the constant is used by
  `router.rs`'s `Router::dispatch` (same crate) and needs no external/library visibility; confirmed
  against the same rationale as the original single-file plan, just re-checked across the new
  `error.rs` → `mod.rs` re-export boundary rather than assumed unchanged.
- Files: `src/providers/gemini/error.rs`, `src/providers/gemini/mod.rs`

##### Task 1.4.3b: Add the new `Router::dispatch` match arm (~4 min)
- Insert, immediately before the existing catch-all arm at `src/routing/router.rs:336-339`:
  ```rust
  Err(e) if e.is_response_shape_mismatch() => {
      self.record_attempt(&chosen.name, attempt_started, Err(&e), &model);
      self.health.trip(chosen.index, Some(Duration::from_secs(crate::providers::gemini::DRIFT_COOLDOWN_SECS)));
      last_error = Some(e);
  }
  ```
- Files: `src/routing/router.rs`

##### Task 1.4.3c: Integration test for the acceptance criteria + regression run (~5 min)
- Add a test in `src/routing/router.rs`'s existing `#[cfg(test)] mod tests` (`src/routing/router.rs:786+`)
  using a fake `Provider` impl (following the existing test-double pattern already used by the
  neighboring tests) that always returns `ResponseShapeMismatch`.
- Run the full existing `router.rs` test module to confirm no regression.
- Files: `src/routing/router.rs`

#### Story 1.4.4: Real per-upstream error-kind attribution (root-cause fix)
**As** Tyler, **I want** the dashboard to know an upstream's *actual* last error classification,
**so that** the three-way status distinction (Story 1.5.2) isn't built on regex-guessed text.
**Acceptance Criteria**:
- `UpstreamCounters` records the real classification, not a re-derived guess.
  - *Given* `record_attempt("gemini", started, Err(&ProviderError::Auth("token expired".to_string())), "gemini-3-pro")`
    is called, *When* the corresponding `UpstreamCounters` entry for `"gemini"` is inspected, *Then*
    its `last_error_kind` field equals `Some("auth")` — not derived from regex-matching the string
    `"token expired"`.
  - *Given* the same call but with `ProviderError::ResponseShapeMismatch("missing field `candidates`".to_string())`,
    *Then* `last_error_kind` equals `Some("response_shape_mismatch")`.
- **A successful request clears `last_error_kind`, so the dashboard self-heals** (fixes a
  cross-artifact-consistency BLOCKER: `design/ux.md`'s interaction flow and UX Acceptance
  Criterion #5 both require the status dot to return to green automatically on the next
  successful request, with no manual clear — the original design only ever *set*
  `last_error_kind` on the `Err` branch, never reset it on `Ok`, so one auth failure would leave
  `status-auth-required` shown forever even after Gemini recovered).
  - *Given* `"gemini"`'s `last_error_kind` is currently `Some("auth")` from a prior failed attempt,
    *When* `record_attempt("gemini", started, Ok(&response), "gemini-3-pro")` is called for a
    subsequent successful request, *Then* `last_error_kind` is reset to `None`.
**Files**: `src/metrics/counters.rs`, `src/routing/router.rs`

##### Task 1.4.4a: Add `kind_label()` to `ProviderError` (~3 min)
- `#[must_use] pub fn kind_label(&self) -> &'static str` on `ProviderError` (`src/providers/mod.rs`),
  returning one of `"rate_limited"`, `"auth"`, `"validation"`, `"timeout"`, `"model_unsupported"`,
  `"upstream"`, `"response_shape_mismatch"`, `"exhausted"` per variant.
- Files: `src/providers/mod.rs`

##### Task 1.4.4b: Add `last_error_kind` field to `UpstreamCounters` + a setter (~4 min)
- Add `pub last_error_kind: std::sync::Mutex<Option<&'static str>>` to `UpstreamCounters`
  (`src/metrics/counters.rs:16-24`), defaulted via `#[derive(Default)]` (already present).
- Add a method (on `ProxyMetrics` or `UpstreamCounters` directly, following whichever existing
  per-upstream-mutation pattern `record_request`/`record_error_kind` already use at
  `src/metrics/counters.rs:141,182-195`) to set it: acquire the `self.upstreams.entry(upstream).or_default()`
  entry, `*entry.last_error_kind.lock().unwrap() = Some(kind_label);`.
- Files: `src/metrics/counters.rs`

##### Task 1.4.4c: Wire `record_attempt` to pass the real kind through, including clearing on success (~4 min)
- In `record_attempt` (`src/routing/router.rs:361-386`), on the `Err(e)` branch, call the new
  per-upstream setter with `e.kind_label()` in addition to the existing
  `self.metrics.counters.record_error_kind(e)` and `self.metrics.error_tracker.push(...)` calls —
  additive, doesn't remove either existing call.
- **On the `Ok(_)` branch, call the same setter with `None`** to clear `last_error_kind` —
  without this, a single past failure permanently pins the dashboard's status class (see the new
  Story 1.4.4 acceptance criterion above). This is the only new behavior on the success path;
  everything else about `Ok(_)` handling is unchanged.
- Files: `src/routing/router.rs`

##### Task 1.4.4d: Expose `last_error_kind` in `/metrics` JSON (~3 min)
- In `upstream_json` (`src/metrics/counters.rs:197-...`), add `"last_error_kind":
  entry.last_error_kind.lock().unwrap().clone()` to each upstream's JSON object.
- Files: `src/metrics/counters.rs`

##### Task 1.4.4e: Unit tests for Story 1.4.4's acceptance criteria (~4 min)
- Include a test for the clear-on-success case: set `last_error_kind` to `Some("auth")` via a
  failed `record_attempt`, then call `record_attempt` again with `Ok(_)` and assert it's `None`.
- Files: `src/metrics/counters.rs`

---

### Epic 1.5: Dashboard three-way error-state classification
**Goal**: The dashboard visually distinguishes "self-healing" (existing amber `status-cooldown`)
from "needs re-auth" (new red `status-auth-required`) from "needs a code fix" (new violet
`status-schema-drift`) — for every upstream generically, not hardcoded to "gemini."

#### Story 1.5.1: Real cooldown feed (fix the pre-existing hardcoded placeholder)
**As** Tyler, **I want** `/metrics`'s `cooldowns` object to reflect every configured upstream's
*real* `HealthRegistry` state, **so that** Gemini's cooldown dot (and everyone else's) isn't fake
data.
**Acceptance Criteria**:
- `/metrics`'s `cooldowns` object includes an entry for every candidate the live `Router` actually
  dispatches to, keyed by real name, with real `remaining_seconds`.
  - *Given* a `Router` with candidates `["anthropic", "gemini"]` where `gemini`'s `HealthRegistry`
    index is currently tripped with 900 seconds remaining, *When* `Router::cooldown_snapshot()` is
    called, *Then* it returns
    `{"anthropic": {"cooling_down": false, "remaining_seconds": 0}, "gemini": {"cooling_down": true, "remaining_seconds": 900}}`
    — not the hardcoded `{"anthropic": ..., "bedrock": ...}` placeholder currently at
    `src/metrics/mod.rs:386-390`.
**Files**: `src/routing/router.rs`, `src/entrypoint/observability.rs`, `src/metrics/mod.rs`

##### Task 1.5.1a: Add `Router::cooldown_snapshot()` (~4 min)
- `pub fn cooldown_snapshot(&self) -> serde_json::Value` iterating `self.candidates` (which already
  carries `name`+`index`, per `UpstreamRef`, referenced at `src/routing/router.rs:35`) and calling
  `self.health.remaining_secs(idx)` per candidate, building the JSON object shown above.
- Files: `src/routing/router.rs`

##### Task 1.5.1b: Remove the hardcoded placeholder from `MetricsCollector::to_metrics_json` (~2 min)
- Delete lines `src/metrics/mod.rs:386-390`'s hardcoded `result["cooldowns"] = json!({...})` block
  entirely — this data will instead be merged in by the HTTP handler (Task 1.5.1c), since
  `MetricsCollector` itself has no reference to `Router`/`HealthRegistry` today and adding one would
  be a larger structural change than needed.
- Files: `src/metrics/mod.rs`

##### Task 1.5.1c: Merge the real snapshot into `GET /metrics`'s response (~4 min)
- In `observability::get_metrics` (`src/entrypoint/observability.rs:19-23`), after calling
  `state.metrics.to_metrics_json()`, set `result["cooldowns"] = state.router.cooldown_snapshot();`
  before returning — requires `EntrypointState` to expose its `Router` (confirm/add accessor if not
  already public).
- Files: `src/entrypoint/observability.rs`

##### Task 1.5.1d: Update/extend `dashboard.rs`'s existing hardcoded-name test + integration test (~5 min)
- Confirm the existing `no_upstream_is_hardcoded_by_name` test (`src/dashboard.rs:613-628`) still
  passes (it inspects `dashboard.rs`'s own HTML/JS text, unaffected by this backend change).
- Add an integration test hitting `GET /metrics` with a Gemini upstream configured and cooled down,
  asserting the response's `cooldowns.gemini` entry matches real `HealthRegistry` state.
- **Regression requirement (adversarial-review Concern)**: this story deletes the hardcoded
  `anthropic`/`bedrock`-only placeholder (`src/metrics/mod.rs:386-390`) that both of those
  providers' `/metrics` cooldown display currently depends on — that's a bigger blast radius than
  "add Gemini." Add a second integration test, independent of the Gemini-specific one above,
  configuring a route with **all three** of Anthropic, Bedrock, and Gemini as candidates (Bedrock
  in cooldown via `set_can_cooldown`/normal cooldown state where applicable, Anthropic healthy,
  Gemini tripped), asserting `GET /metrics`'s `cooldowns` object has correct, non-cross-contaminated
  entries for all three names — not just that `cooldowns.gemini` looks right in isolation.
- Files: `src/entrypoint/observability.rs`

#### Story 1.5.2: New `status-auth-required`/`status-schema-drift` dashboard classes
**As** Tyler, **I want** an upstream whose last error was auth-related to show red, and one whose
last error was schema drift to show violet, **so that** I immediately know *what kind* of action
(if any) to take without reading the error table.

**Design correction (found by the Phase 3 UX design agent cross-referencing this story against
`Router::dispatch`'s real behavior, 2026-09-04):** `Router::dispatch`'s existing `is_auth()`/
`is_validation()` arm (`src/routing/router.rs:326-329`) returns immediately WITHOUT calling
`health.trip()` — this is deliberate, unchanged, cross-provider behavior (an auth/validation
error is normally a per-request client-config problem, not upstream-wide degradation). That means
a real Gemini auth failure (expired Antigravity token) will show `cooling_down: false` in
`/metrics`, **not** `true` — the original AC below assumed the opposite. Consequently the status
classes must NOT be gated behind `cooling`; they must be driven by `last_error_kind` directly,
falling back to the cooling-based active/cooldown check only when `last_error_kind` isn't one of
the two new special cases. This is a client-side (JS) fix only — it does not require or propose
changing `Router::dispatch`'s cooldown-trip conditions for auth/validation errors.

**Acceptance Criteria**:
- An upstream whose `last_error_kind` (Story 1.4.4) is `"auth"` renders with
  `status-auth-required`, regardless of `cooling_down`'s value (realistically `false`, since
  `is_auth()` never trips `HealthRegistry`).
  - *Given* `/metrics` reports `{"providers": {"gemini": {"last_error_kind": "auth"}}, "cooldowns": {"gemini": {"cooling_down": false, "remaining_seconds": 0}}}`,
    *When* the dashboard's `loadMetrics()` JS renders the status bar, *Then* the `gemini` entry's
    `<span class="status-indicator ...">` carries class `status-auth-required`, not
    `status-active`.
- An upstream whose `last_error_kind` is `"response_shape_mismatch"` renders with
  `status-schema-drift`, regardless of `cooling_down`'s value.
  - *Given* the same shape but `last_error_kind == "response_shape_mismatch"` (this case DOES also
    carry `cooling_down: true` in practice, since ADR-002 has `ResponseShapeMismatch` trip
    `HealthRegistry`), *Then* the rendered class is `status-schema-drift`, not `status-cooldown`.
- A cooling-down upstream with any other `last_error_kind` (e.g. `"rate_limited"`,
  `"upstream"`, `"timeout"`) keeps the existing `status-cooldown` amber behavior — no regression.
  - *Given* `last_error_kind == "rate_limited"` and `cooling_down == true`, *Then* the rendered
    class is still `status-cooldown` (existing behavior at `src/dashboard.rs:360-366` unchanged for
    this case).
- A freshly-started upstream with no requests yet (no `last_error_kind`, `cooling_down: false`)
  renders `status-active` — no false "needs re-auth"/"schema drift" on cold start.
  - *Given* `/metrics` reports `{"providers": {"gemini": {}}, "cooldowns": {"gemini": {"cooling_down": false, "remaining_seconds": 0}}}`
    (no `last_error_kind` key at all), *Then* the rendered class is `status-active`.
**Files**: `src/dashboard.rs`

##### Task 1.5.2a: Add the two new CSS classes (~2 min)
- Add `.status-auth-required { background: #ef4444; }` (reusing the existing generic error-badge
  red already in the palette per `research/ux.md`'s recommendation) and `.status-schema-drift {
  background: #8b5cf6; }` (a new violet hue, unused elsewhere in the existing palette) near the
  existing `.status-active`/`.status-cooldown` rules at `src/dashboard.rs:41-42`.
- Files: `src/dashboard.rs`

##### Task 1.5.2b: Extend `/metrics`'s `providers` JSON to carry `last_error_kind` per upstream (~2 min)
- Already done by Task 1.4.4d — this task just confirms the dashboard's `loadMetrics()` fetch
  (`src/dashboard.rs:342-343`) has access to it via `data.providers[name].last_error_kind`.
- Files: `src/dashboard.rs` (verification only, likely no diff — recorded as a task so the
  dependency is explicit)

##### Task 1.5.2c: Extend the status-class JS logic (~4 min)
- In the `statusBar.innerHTML = ...` block (`src/dashboard.rs:356-366`), change the `cls`
  computation from the current binary `cooling ? 'status-cooldown' : 'status-active'` to check
  `last_error_kind` FIRST, before falling back to the cooling check — do NOT gate the two new
  classes behind `cooling`, since a real Gemini auth failure has `cooling: false` (see the Story
  1.5.2 design-correction note above):
  ```js
  const lastKind = (data.providers[name] || {}).last_error_kind;
  const cls = lastKind === 'auth' ? 'status-auth-required'
      : lastKind === 'response_shape_mismatch' ? 'status-schema-drift'
      : cooling ? 'status-cooldown'
      : 'status-active';
  ```
  Keep the always-paired text label (existing accessibility pattern per `research/ux.md`: color is
  never the sole signal) — extend the label to append `" (needs re-auth)"` /
  `" (schema drift — code fix needed)"` for the two new classes.
- Files: `src/dashboard.rs`

##### Task 1.5.2d: Update the `no_upstream_is_hardcoded_by_name` test's neighbor tests / add new ones (~4 min)
- Add tests asserting the new CSS class strings exist in `DASHBOARD_HTML` and that the JS
  classification logic references `last_error_kind` generically (not a hardcoded upstream name),
  consistent with the existing `src/dashboard.rs:613-628` test's intent.
- Add a regression test for the design-correction above: assert the classification logic checks
  `last_error_kind` BEFORE `cooling` (e.g. by asserting the string `lastKind === 'auth' ?` appears
  before `cooling ?` in the extracted JS block) — this is the concrete guard against the
  cooling-gated bug found in Phase 3 review, so it must fail loudly if reintroduced.
- Add a cold-start test: `last_error_kind` absent + `cooling: false` → `status-active` (no false
  "needs re-auth"/"schema drift" on a freshly-started upstream with zero requests).
- Files: `src/dashboard.rs`

---

### Epic 1.6: `ToolUseId` newtype + `ThoughtSignatureCache` scaffolding (decision #2, non-load-bearing)
**Goal**: The `ToolUseId` newtype and a `GeminiProvider`-owned, concurrency-safe
`ThoughtSignatureCache` field exist and compile, doing nothing yet (no tool calls exist until
Phase 3) — so Phase 3 only has to *activate* the mechanism, not invent its lifetime under time
pressure.

**Redesign note (fixes architecture-review Blocker B / adversarial-review Blocker)**: the
original scaffolding here constructed `ThoughtSignatureCache` as a value local to one `send()`
call, dropped when it returned. That cannot satisfy Story 3.3.1: a `functionCall`'s
`thoughtSignature` arrives on one HTTP response and must be replayed on a **separate, later**
HTTP request once the client resends the `tool_use` turn — no value scoped to a single `send()`
call can bridge that gap, and the plan's own Pattern Decisions table already rules out smuggling
the signature into `tool_use.id` to let the client carry it instead. The corrected design below
promotes the cache to a field on `GeminiProvider` itself (a single shared `Arc<dyn Provider>`
serving every concurrent conversation routed to "gemini," per `src/routing/router.rs:57` and
`Provider::send(&self, ..)`), backed by a `DashMap` (mirroring `ExecCredentialCache`'s pattern,
`src/auth/exec.rs:52-55`), and additionally keyed by a per-conversation session identifier so two
unrelated concurrent conversations can't cross-wire signatures.

**Session-key design decision**: this codebase already has a stable, best-effort per-conversation
identifier available at the `Provider::send(&self, body: Value, ..)` boundary:
`extract_session_id(&body)` (`src/routing/session_overrides.rs:32`), which reads
`body.metadata.user_id` — the same field `SessionOverrideStore` already keys session route-pins on
(`src/routing/router.rs:28`). It returns `Option<String>`, since not every client populates
`metadata.user_id`. Chosen policy: use `extract_session_id(&body).unwrap_or_else(|| "anonymous"
.to_string())` as the cache's session-key component. When the client *does* send a `user_id`
(Claude Code does, per the existing session-pin feature's own precedent), cross-conversation
collision is closed **assuming `metadata.user_id` is genuinely per-conversation** — which
Unresolved Questions below flags as still unverified against live traffic, not a confirmed
guarantee (corrected 2026-09-04, Phase 4 engineering-lens repair loop: this paragraph previously
said "fully closed" unqualified, overclaiming past what's actually been verified). When it
doesn't, entries collapse onto the shared `"anonymous"` bucket —
narrower than today's design (still requires *also* colliding on the same `ToolUseId` within the
TTL window, whereas today's design collides on `ToolUseId` alone with no time bound), and is an
accepted, documented residual risk for v1 rather than a solved one: building real end-to-end
session-id threading through every possible client is out of this feature's appetite. Bounded
growth is handled independently of session-key quality via a TTL sweep (below).

#### Story 1.6.1: `ToolUseId` newtype + provider-owned `ThoughtSignatureCache`
**As a** future Phase-3 implementer, **I want** the stash-and-replay data structure already in
place with a lifetime that can actually work, **so that** activating it for real tool calls in
Phase 3 is a small, low-risk change, not a second architecture rewrite.
**Acceptance Criteria**:
- `ToolUseId` is a newtype, not a raw `String`, used as the key type here and in Phase 3's
  `GeminiToolCallState`.
  - *Given* `ToolUseId::from("toolu_01".to_string())`, *When* used as a `HashMap`/`DashMap` key,
    *Then* it compiles only where a `ToolUseId` is expected — a raw `String` (e.g. a signature
    value) cannot be passed where a `ToolUseId` key is expected without an explicit conversion.
- `ThoughtSignatureCache` is a field on `GeminiProvider` (constructed once in `GeminiProvider::new`,
  not per `send()` call), is concurrency-safe, and is currently never populated or read (no tool
  calls exist yet in Phase 1).
  - *Given* two sequential calls to `GeminiProvider::send(body, headers, false)` on the **same**
    `GeminiProvider` instance with text-only requests, *When* both calls complete, *Then* the same
    `ThoughtSignatureCache` instance (verified via a `#[cfg(test)]` accessor or a code-inspection
    task, per Task 1.6.1b) persisted across both calls — i.e., unlike Phase 1's original scaffolding,
    the cache is *provably able to* survive across calls, even though nothing populates it yet.
  - *Given* the same setup, *When* either call completes, *Then* no `.insert()`/`.get()` call was
    made (still a no-op with respect to cache *contents* in Phase 1 — only its lifetime changed).
- The cache is keyed by `(String, ToolUseId)` (session key, tool-use id), not `ToolUseId` alone.
  - *Given* the cache's `insert`/`get` method signatures, *When* inspected, *Then* both take a
    `session_key: &str` parameter in addition to a `ToolUseId`.
- Stale entries are swept, bounding growth.
  - *Given* an entry inserted with `inserted_at` older than `THOUGHT_SIGNATURE_TTL_SECS`, *When*
    the next `.insert()` call runs, *Then* that stale entry is removed from the `DashMap` as part of
    the same call (sweep-on-insert, no separate background task needed at this traffic scale).
**Files**: `src/providers/gemini/tools.rs` (`ToolUseId`/`ThoughtSignatureCache` type + impl),
`src/providers/gemini/mod.rs` (the field on `GeminiProvider`, constructed in `new()`)

##### Task 1.6.1a: Define `ToolUseId` and `ThoughtSignatureCache` (~5 min)
- `#[derive(Debug, Clone, PartialEq, Eq, Hash)] pub(crate) struct ToolUseId(String);` with a
  `From<String>`/`AsRef<str>` impl, in `src/providers/gemini/tools.rs`.
- `pub(crate) struct ThoughtSignatureCache { entries: dashmap::DashMap<(String, ToolUseId),
  CacheEntry> }` where `struct CacheEntry { signature: String, inserted_at: std::time::Instant }`,
  with `new()`, `insert(&self, session_key: &str, id: ToolUseId, signature: String)` (sweeping
  entries older than `const THOUGHT_SIGNATURE_TTL_SECS: u64 = 900;` before inserting), and
  `get(&self, session_key: &str, id: &ToolUseId) -> Option<String>` methods — mirroring
  `ExecCredentialCache`'s `DashMap`-backed shape (`src/auth/exec.rs:52-88`). `pub(crate)` (not
  `pub`) is correct: only `mod.rs` (same crate, parent module) and Phase 3's `tools.rs`/`translate.rs`
  need to name these types.
- Files: `src/providers/gemini/tools.rs`

##### Task 1.6.1b: Add it as a `GeminiProvider` field, constructed once in `new()` (~3 min)
- Add `thought_signatures: ThoughtSignatureCache` (imported via `use super::tools::ThoughtSignatureCache;`)
  to the `GeminiProvider` struct (Task 1.3.4a) in `src/providers/gemini/mod.rs`, initialized via
  `ThoughtSignatureCache::new()` in `GeminiProvider::new`'s constructor body — **not** constructed
  inside `send()`. Add a doc comment on the field: `// Scaffolded per
  project_plans/gemini-provider/implementation/plan.md Story 1.6.1 — provider-owned so it survives
  across the separate send() calls Story 3.3.1 needs to bridge; populated/read starting in Story
  3.3.1, once tool calls exist. Type defined in tools.rs.`
- Files: `src/providers/gemini/mod.rs`

##### Task 1.6.1c: Unit tests confirming cross-call survival, session-keying, and TTL sweep (~4 min)
- One test constructing a `GeminiProvider` and calling `send()` twice, confirming (via a
  `#[cfg(test)]`-only accessor returning a reference/count) the same cache instance persists — lives
  in `mod.rs`'s test module since it needs a `GeminiProvider` instance.
- One test confirming `get("session-a", &id)` doesn't see an entry inserted under `("session-b",
  &id)` — same `ToolUseId`, different session key — a pure `ThoughtSignatureCache` unit test, lives
  in `tools.rs`.
- One test confirming an entry older than `THOUGHT_SIGNATURE_TTL_SECS` is gone after a subsequent
  `insert()` call — also a pure `tools.rs` unit test.
- Files: `src/providers/gemini/tools.rs`, `src/providers/gemini/mod.rs`

---

### Epic 1.7: `project_id` resolution (ADR-003)
**Goal**: `UpstreamKind::Gemini::project_id` flows correctly into `CloudCodeEnvelope.project` for
every request.

#### Story 1.7.1: Thread `project_id` from config into every outgoing envelope
**As a** `GeminiProvider`, **I want** the configured `project_id` used on every request, **so that**
the Cloud Code Assist call is attributed to the right Google Cloud project.
**Acceptance Criteria**:
- Every outgoing `CloudCodeEnvelope.project` matches the configured `project_id`, never empty or a
  guessed default.
  - *Given* `UpstreamKind::Gemini { project_id: "tystapler-personal" }` on the configured upstream,
    *When* `GeminiProvider::send` builds the outgoing envelope for any request, *Then*
    `envelope.project == "tystapler-personal"` — confirmed via Story 1.3.1's translation fn already
    taking `project_id` as an explicit parameter (Task 1.3.1b), not read from any other source.
**Files**: `src/providers/gemini/mod.rs`

##### Task 1.7.1a: Add a `project_id()` accessor on `GeminiProvider` (~2 min)
- `fn project_id(&self) -> &str` reading `self.upstream.kind`, pattern-matching
  `UpstreamKind::Gemini { project_id } => project_id` (the `unreachable!()` arm documented as
  "can't happen — `GeminiProvider` is only ever constructed for a `Gemini`-kind upstream, per
  `build_providers`'s match arm").
- Files: `src/providers/gemini/mod.rs`

##### Task 1.7.1b: Confirm `send()` passes it to the translation fn (~1 min)
- Verify Task 1.3.4c's call site (in `mod.rs`'s `send()`) is
  `translate_anthropic_request_to_gemini(&body, self.project_id())?` (the trailing `?` per Task
  1.3.1b's `Result`-returning signature defined in `translate.rs`).
- Files: `src/providers/gemini/mod.rs`

---

### Epic 1.8: Config example & docs
**Goal**: A working, copy-pasteable example exists showing exactly how to configure a Gemini
upstream, defaulting to the safer Fallback strategy (decision #7).

#### Story 1.8.1: `references/conf.d/00-providers.toml`
**As** Tyler, **I want** a single reference file showing all four upstream kinds side by side, **so
that** adding Gemini to my real `conf.d` is a five-minute copy-paste-edit, not a guess.
**Acceptance Criteria**:
- The example's Gemini route uses `Strategy::Fallback` behind `anthropic`/`bedrock`, not
  `Strategy::Weighted`.
  - *Given* `references/conf.d/00-providers.toml`'s `[[routes]]` block for a route including
    `gemini`, *When* the file is read, *Then* `strategy = "fallback"` and the `[[routes.upstreams]]`
    order lists `anthropic`/`bedrock` ahead of `gemini` — never `strategy = "weighted"`.
- The example parses successfully against the real `Config` schema.
  - *Given* `references/conf.d/00-providers.toml`, *When* loaded via consolette's existing conf.d
    TOML-parity test pattern (`tests/fixtures/toml_parity/`), *Then* it parses into a valid
    `Config` with no `deny_unknown_fields` errors.

**Invariant this config example depends on (noted 2026-09-04, Phase 4 engineering-lens repair
loop)**: `requirements.md`'s success metric "a Gemini failure doesn't block other upstreams" holds
for this shipped example specifically *because* Gemini is ordered last in the `Strategy::Fallback`
chain. `Router::dispatch`'s existing `is_validation()||is_auth()` arm (`src/routing/router.rs:326-329`,
pre-existing, not introduced by this feature) returns immediately with no fallback to later
candidates — that only matters when the failing candidate is tried *before* others that could
still succeed. Since Gemini is last here, an auth/validation error on Gemini simply ends dispatch
with nothing left to fall through to anyway, which is indistinguishable from today's behavior for
Anthropic/Bedrock/OpenAI in that same last position. If a future config ever places Gemini earlier
in a chain, an auth/validation error on Gemini would abort the whole request rather than falling
through to a later candidate that might have succeeded — exactly as it already would for any other
provider in that position. This is pre-existing `Router::dispatch` behavior; this plan does not
change it, and doing so is explicitly out of scope here.
**Files**: `references/conf.d/00-providers.toml` (new)

##### Task 1.8.1a: Write the file (~5 min)
- One `[[upstreams]]` block per kind (anthropic, bedrock, openai, gemini) side by side, each with
  its matching `[upstreams.auth]` table where applicable; the Gemini block: `kind = "gemini"`,
  `project_id = "your-gcp-project-id"`, `[upstreams.auth] type = "exec" command =
  "references/bin/antigravity-token-auth.py" cache_ttl_secs = 300`; one `[[routes]]` block,
  `strategy = "fallback"`, upstreams listed `anthropic`, `bedrock`, `gemini` in that order.
- Files: `references/conf.d/00-providers.toml`

##### Task 1.8.1b: Add it to the TOML-parity test fixtures / confirm it parses (~3 min)
- Add a test (alongside the existing `tests/fixtures/toml_parity/*.toml`-driven tests) confirming
  this file parses cleanly.
- Files: `references/conf.d/00-providers.toml`, relevant existing test file under `tests/`

---

## Phase 2: Streaming

### Epic 2.1: SSE streaming translation
**Goal**: `stream: true` requests against the Gemini upstream produce a correctly-reconstructed
Anthropic SSE stream, handling multiple interleaved content-block types (not just a single
hardcoded text block).

#### Story 2.1.1: `GeminiToAnthropicStream` multi-block reconstruction
**As a** streaming client (e.g. Claude Code), **I want** a Gemini SSE stream translated into
well-formed Anthropic bracketing events with correct block indices, **so that** interleaved text
(and, later, function-call) content renders correctly.
**Acceptance Criteria**:
- A single-part streaming response (text only) produces the same bracketing shape
  `OpenaiToAnthropicStream` already produces for a single block.
  - *Given* two SSE chunks `data: {"response":{"candidates":[{"content":{"parts":[{"text":"Hel"}]}}]}}\n\n`
    then `data: {"response":{"candidates":[{"content":{"parts":[{"text":"lo"}]}},"finishReason":"STOP"],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7}}}\n\n`,
    *When* consumed through `GeminiToAnthropicStream`, *Then* the emitted event sequence is
    `message_start` → `content_block_start(index:0,type:text)` → `content_block_delta("Hel")` →
    `content_block_delta("lo")` → `content_block_stop(index:0)` → `message_delta(stop_reason:end_turn,
    usage:{input_tokens:5,output_tokens:2})` → `message_stop`.
- A part-type change (text → a later part type, exercised for real starting Phase 3) opens a new
  indexed content block rather than corrupting/overwriting index 0.
  - *Given* a streaming chunk sequence where part 0 is `{"text":"thinking..."}` and part 1 (in a
    later chunk) is a different part type, *When* consumed, *Then* the second part gets
    `content_block_start(index:1,...)`, never reusing or silently merging into index 0.
**Files**: `src/providers/gemini/stream.rs` (new), `src/providers/gemini/mod.rs` (adds `mod stream;`)

##### Task 2.1.1a: Create `stream.rs` and define `GeminiToAnthropicStream` struct with an index-tracking `Vec` (~6 min)
- **This is the first Phase 2 task that populates `stream.rs`** — per the Phase-1 module-skeleton
  decision (Task 1.1.2a, and the "Module organization" Pattern Decisions row), `stream.rs` was
  deliberately *not* created empty in Phase 1 to avoid a dead placeholder file; create it now and
  add `mod stream;` to `src/providers/gemini/mod.rs`'s existing `mod translate; mod tools; mod
  error;` line.
- Mirror `OpenaiToAnthropicStream`'s shape (`src/providers/openai.rs:369-388`) but replace the
  single implicit index-0 assumption with `active_blocks: Vec<BlockKind>` (`BlockKind` = `Text` for
  now, extended in Phase 3), tracking which Gemini part maps to which already-opened Anthropic
  block index.
- Files: `src/providers/gemini/stream.rs`, `src/providers/gemini/mod.rs`

##### Task 2.1.1b: Implement the `Stream` impl's `poll_next`, following `OpenaiToAnthropicStream`'s `VecDeque<Bytes>` buffering pattern (~5 min)
- Files: `src/providers/gemini/stream.rs`

##### Task 2.1.1c: Unit tests for both acceptance criteria (~4 min)
- Files: `src/providers/gemini/stream.rs`

#### Story 2.1.2: Wire `GeminiProvider::send`'s `stream: true` path
**As a** `Router`, **I want** `send(body, headers, true)` to return `ProviderResponse::Stream`, **so
that** streaming requests dispatch through Gemini exactly like the other three providers.
**Acceptance Criteria**:
- `send(..., true)` uses `self.stream_client` (the `pool_max_idle_per_host(0)` client), not
  `self.client`.
  - *Given* a `GeminiProvider` and `stream: true`, *When* `send` is called, *Then* the outgoing
    HTTP request is issued via `self.stream_client.post(...)`, targeting
    `{base_url}/v1internal:streamGenerateContent?alt=sse`, with `Accept: text/event-stream` added
    to the outgoing headers.
**Files**: `src/providers/gemini/mod.rs` (`send()`'s `stream: true` branch, using `stream.rs`'s
`GeminiToAnthropicStream`, unmodified by this story)

##### Task 2.1.2a: Add the `stream: true` branch in `send()` (~4 min)
- Files: `src/providers/gemini/mod.rs`

##### Task 2.1.2b: Fixture-based test for the `stream:true` dispatch path (~5 min)
**Rescoped 2026-09-04 (Phase 4 engineering-lens repair loop)**, for the same reason as Task
1.3.4f: no mocked-HTTP/SSE-server crate exists in this repo. Assert the outgoing-request
construction (client selection, target URL, `Accept` header) directly against a captured/`mockall`-free
request builder, and exercise SSE chunk parsing via `GeminiToAnthropicStream` (Story 2.1.1, already
fixture-tested against literal `data:` byte sequences) rather than a live mocked listener. A true
HTTP-level integration test (a real `eventsource_stream` server) is deferred as out of scope for
this appetite, for the same reason given in Task 1.3.4f.
- Files: `src/providers/gemini/mod.rs`

---

### Epic 2.2: Streaming error classification
**Goal**: A malformed SSE chunk breaks the stream loudly (fail closed, per requirements' scope
item), consistent with `bedrock.rs`'s existing streaming fail-closed precedent
(`src/providers/bedrock.rs:719-735`).

#### Story 2.2.1: Unparseable-chunk handling
**As** Tyler, **I want** a corrupted SSE chunk to end the stream with a clear error rather than
silently emit garbage, **so that** a partial protocol drift during a long streaming response is
never mistaken for a legitimate (if truncated) answer.
**Acceptance Criteria**:
- An unparseable SSE data line ends the stream with `Err(ProviderError::ResponseShapeMismatch(..))`
  down the channel, not a panic or silently-dropped chunk.
  - *Given* an SSE stream where the second `data:` line is `not valid json`, *When* consumed
    through `GeminiToAnthropicStream`, *Then* the stream yields
    `Some(Err(anyhow::Error))` wrapping a `ResponseShapeMismatch`-shaped message, and no further
    items are yielded after it (`bedrock.rs:719-735`'s "breaks the stream" precedent, not "skip and
    continue").
**Files**: `src/providers/gemini/stream.rs`

##### Task 2.2.1a: Add the parse-failure branch to `poll_next` (~4 min)
- Files: `src/providers/gemini/stream.rs`

##### Task 2.2.1b: Unit test for the acceptance criterion (~3 min)
- Files: `src/providers/gemini/stream.rs`

*(Whether this first-failure-ends-stream behavior needs upgrading to consecutive-run counting
before it can also trip `HealthRegistry`'s cooldown — as opposed to just ending the one stream — is
the Unresolved Question already flagged above; not resolved by this Epic.)*

---

## Phase 3: Tool calls

### Epic 3.1: Tool schema sanitization
**Goal**: Claude Code's `$ref`/`$defs`-heavy tool schemas are sanitized into shapes Gemini's
`functionDeclarations[].parameters` accepts, before any tool-call round-trip can be tested.

#### Story 3.1.1: `sanitize_function_schema`
**As a** `GeminiProvider`, **I want** unsupported JSON-Schema keywords stripped at any nesting
depth, **so that** a tool definition with `$ref`/`$defs`/`patternProperties` doesn't 400 the whole
request.
**Acceptance Criteria**:
- A nested `$ref` inside a tool's `input_schema.properties.foo.$ref` is removed, without removing
  the sibling `type`/`description` fields.
  - *Given* the Anthropic tool schema
    `{"type":"object","properties":{"foo":{"$ref":"#/$defs/Foo","description":"a foo"}},"$defs":{"Foo":{"type":"string"}}}`,
    *When* `sanitize_function_schema(&schema)` is called, *Then* the result is
    `{"type":"object","properties":{"foo":{"description":"a foo"}}}` — the top-level `$defs` key and
    the nested `$ref` key are both gone, `description` is preserved.
- `patternProperties` at any depth is stripped.
  - *Given* `{"type":"object","patternProperties":{"^S_":{"type":"string"}}}`, *When* sanitized,
    *Then* the result is `{"type":"object"}`.
**Files**: `src/providers/gemini/translate.rs`

##### Task 3.1.1a: Write the recursive walker (~5 min)
- `fn sanitize_function_schema(schema: &Value) -> Value` — recursively walks `Value::Object`s and
  `Value::Array`s, dropping keys `"$ref"`, `"$defs"`, `"patternProperties"` at every level,
  recursing into every remaining value.
- Files: `src/providers/gemini/translate.rs`

##### Task 3.1.1b: Unit tests for both acceptance criteria + one deeply-nested case (~4 min)
- Files: `src/providers/gemini/translate.rs`

##### Task 3.1.1c: Wire it into the request-translation path for `tools[]` (~3 min)
- In `translate_anthropic_request_to_gemini` (extended in Story 3.2.1), call
  `sanitize_function_schema` on each tool's `input_schema` before emitting it as a
  `functionDeclarations[].parameters`.
- Files: `src/providers/gemini/translate.rs`

---

### Epic 3.2: Tool call round-trip translation
**Goal**: `tool_use`/`tool_result` blocks round-trip correctly through Gemini's
`functionCall`/`functionResponse` shape in both directions.

#### Story 3.2.1: Request direction — `tool_use`/`tool_result` → `functionCall`/`functionResponse`
**As a** consolette router, **I want** an Anthropic request containing tool-call history
translated into Gemini's native shape, **so that** a multi-turn tool-using conversation continues
correctly against Gemini.
**Acceptance Criteria**:
- An Anthropic assistant message with a `tool_use` block translates to a `model`-role
  `GeminiContent` with a `functionCall` part.
  - *Given* the Anthropic message
    `{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01","name":"get_weather","input":{"city":"Boise"}}]}`,
    *When* translated, *Then* the resulting `GeminiContent` is
    `{"role":"model","parts":[{"functionCall":{"name":"get_weather","args":{"city":"Boise"}}}]}` (no
    `id` field on the Gemini side — Gemini's `functionCall` doesn't carry one, per `stack.md`).
- A subsequent Anthropic user message with a matching `tool_result` translates to a `user`-role
  `GeminiContent` with a `functionResponse` part, correctly re-associated with `get_weather` via
  `GeminiToolCallState` (since the wire has no id to match on).
  - *Given* the same conversation continuing with
    `{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"58F and sunny"}]}`,
    *When* translated (with `GeminiToolCallState` populated from the prior turn's `toolu_01` →
    `"get_weather"` mapping), *Then* the resulting `GeminiContent` is
    `{"role":"user","parts":[{"functionResponse":{"name":"get_weather","response":{"result":"58F and sunny"}}}]}`.
**Files**: `src/providers/gemini/translate.rs` (extends `translate_anthropic_request_to_gemini`/
`GeminiPart`), `src/providers/gemini/tools.rs` (defines `GeminiToolCallState`, consulted by
`translate.rs`)

##### Task 3.2.1a: Define `GeminiToolCallState` using the `ToolUseId` newtype (~3 min)
- `pub(crate) struct GeminiToolCallState { by_tool_use_id: std::collections::HashMap<ToolUseId, String> }`
  (`ToolUseId` → function name, reusing the newtype from Story 1.6.1 rather than a raw `String` key
  — see Pattern Decisions' "Tool-call/signature key typing" row), populated while walking
  `messages[]` in translation order.
- Files: `src/providers/gemini/tools.rs`

##### Task 3.2.1b: Extend `GeminiPart` with `functionCall`/`functionResponse` variants (~3 min)
- Files: `src/providers/gemini/translate.rs`

##### Task 3.2.1c: Extend `translate_anthropic_request_to_gemini` for both block types (~5 min)
- While iterating `messages[]` in order, populate `GeminiToolCallState` (imported via `use
  super::tools::GeminiToolCallState;`) on each `tool_use` block encountered, and consult it (erroring
  via `ProviderError::Validation` with a clear message, per existing precedent, if a `tool_result`
  references an unknown `tool_use_id`) on each `tool_result`.
- Files: `src/providers/gemini/translate.rs`

##### Task 3.2.1d: Unit tests for both acceptance criteria (~4 min)
- Files: `src/providers/gemini/translate.rs`

#### Story 3.2.2: Response direction — `functionCall` → `tool_use`
**As a** consolette router, **I want** a Gemini response's `functionCall` parts translated back
into Anthropic `tool_use` blocks, **so that** the calling client (Claude Code) can execute the tool
call as normal.
**Acceptance Criteria**:
- A Gemini `functionCall` part becomes a `tool_use` block with a synthesized id.
  - *Given* the Gemini response part `{"functionCall":{"name":"get_weather","args":{"city":"Boise"}}}`,
    *When* translated, *Then* the resulting Anthropic content block is
    `{"type":"tool_use","id":"toolu_<synthesized-uuid>","name":"get_weather","input":{"city":"Boise"}}`,
    and `stop_reason` for the overall response is `"tool_use"`.
- The synthesized id is recorded in `GeminiToolCallState` so a later `tool_result` referencing it
  translates back correctly (closing the loop with Story 3.2.1).
  - *Given* the synthesized id `toolu_abc123` from the criterion above, *When* the client's next
    turn includes `{"type":"tool_result","tool_use_id":"toolu_abc123",...}`, *Then* Story 3.2.1's
    translation finds `"get_weather"` in `GeminiToolCallState` for that id.
**Files**: `src/providers/gemini/translate.rs` (extends `translate_gemini_response_to_anthropic`),
`src/providers/gemini/tools.rs` (registers the synthesized id into `GeminiToolCallState`)

##### Task 3.2.2a: Extend `translate_gemini_response_to_anthropic` for `functionCall` parts (~5 min)
- Synthesize a `toolu_{uuid}` id (mirroring the existing `msg_{uuid}` pattern at
  `src/providers/openai.rs:384`), map `finishReason` presence-of-`functionCall` to `stop_reason:
  "tool_use"` (taking precedence over the plain `STOP`→`end_turn` mapping from Story 1.3.2 when a
  `functionCall` part is present).
- Register the synthesized id in the request-scoped `GeminiToolCallState` (imported via `use
  super::tools::GeminiToolCallState;`).
- Files: `src/providers/gemini/translate.rs`, `src/providers/gemini/tools.rs`

##### Task 3.2.2b: Unit tests for both acceptance criteria (~4 min)
- Files: `src/providers/gemini/translate.rs`

---

### Epic 3.3: `thought_signature` becomes load-bearing
**Goal**: Gemini 3 Pro's mandatory per-`functionCall` `thought_signature` is captured and echoed
back correctly — this is on the critical path for `gemini-3-pro` tool use per `requirements.md`,
not deferrable.

#### Story 3.3.1: Stash on inbound `functionCall`, replay on outbound `functionResponse` turn
**As a** `GeminiProvider`, **I want** every `functionCall`'s opaque `thought_signature` preserved
and replayed unmodified on the matching turn — even across the two separate HTTP requests this
requires — **so that** Gemini 3 Pro doesn't hard-error with "Function call is missing a
thought_signature."

This activates the mechanism Story 1.6.1 scaffolded as a `GeminiProvider`-owned, session-keyed
`DashMap`; `send()` derives `session_key` via `extract_session_id(&body).unwrap_or_else(|| "anonymous".to_string())`
once per call and threads it into the translation functions below alongside `&self.thought_signatures`.

**Acceptance Criteria**:
- A `functionCall` part carrying `thoughtSignature` gets it stashed in `ThoughtSignatureCache`,
  keyed by `(session_key, ToolUseId)` using the synthesized `tool_use` id from Story 3.2.2.
  - *Given* the Gemini response part
    `{"functionCall":{"name":"get_weather","args":{"city":"Boise"}},"thoughtSignature":"opaque-blob-xyz"}`
    received while handling a request whose `session_key` is `"session-a"`, *When* translated
    (Story 3.2.2's logic, now extended), *Then* `thought_signatures.get("session-a",
    &ToolUseId::from("toolu_abc123".to_string()))` (the synthesized id) returns
    `Some("opaque-blob-xyz".to_string())`.
- The next request turn containing the matching `tool_use`/`tool_result` pair, from the **same**
  session, echoes that exact signature back on the re-sent `functionCall` part.
  - *Given* the same `ThoughtSignatureCache` entry (keyed under `"session-a"`) and a follow-up
    Anthropic request — also carrying `metadata.user_id` resolving to `"session-a"` — replaying the
    prior assistant `tool_use` block with `id: "toolu_abc123"`, *When*
    `translate_anthropic_request_to_gemini` rebuilds that turn's `functionCall` part, *Then* it
    includes `"thoughtSignature":"opaque-blob-xyz"` unmodified — byte-for-byte identical to what was
    received, never re-encoded or truncated.
- A different session presenting the identical `tool_use` id does **not** see the first session's
  signature (the fix for the cross-conversation cache-collision risk both reviews flagged).
  - *Given* the same cache state as above, *When* a request with `session_key == "session-b"`
    (different `metadata.user_id`) replays a `tool_use` block with the same `id: "toolu_abc123"`,
    *Then* `thought_signatures.get("session-b", &ToolUseId::from("toolu_abc123".to_string()))`
    returns `None` — Story 3.3.2 governs what happens next in this case.
**Files**: `src/providers/gemini/translate.rs` (`GeminiPart.thoughtSignature` field, and
`translate_anthropic_request_to_gemini`'s extended signature taking `thought_signatures`/
`session_key`), `src/providers/gemini/tools.rs` (the `ThoughtSignatureCache` being read/written —
already defined since Story 1.6.1, not modified here), `src/providers/gemini/mod.rs` (`send()`
derives `session_key` and performs the actual `self.thought_signatures.insert(..)` stash, since
`translate.rs`'s functions are stateless per the Pattern Decisions "Request/response translation"
row and can't hold `&self`)

##### Task 3.3.1a: Extend `GeminiPart` with an optional `thoughtSignature` field (~2 min)
- Files: `src/providers/gemini/translate.rs`

##### Task 3.3.1b: Wire the stash on the response side (Story 3.2.2's translation) (~3 min)
- `translate_gemini_response_to_anthropic` stays a stateless fn (Pattern Decisions row "Request/
  response translation") — it (in `translate.rs`) surfaces each `functionCall` part's
  `thoughtSignature` alongside its synthesized `tool_use` id in its return value; its caller in
  `send()` (`mod.rs`) is what actually calls `self.thought_signatures.insert(&session_key,
  tool_use_id, signature)`, since only `mod.rs` holds `&self`.
- Files: `src/providers/gemini/translate.rs`, `src/providers/gemini/mod.rs`

##### Task 3.3.1c: Wire the replay on the request side (Story 3.2.1's translation) (~4 min)
- `translate_anthropic_request_to_gemini` (extended to accept `thought_signatures:
  &ThoughtSignatureCache` and `session_key: &str`) looks up `thought_signatures.get(session_key,
  &tool_use_id)` when re-emitting a `functionCall` part for a `tool_use` block; when present,
  attach it; when absent (e.g. history from a non-Gemini-originated turn, a different session, or
  an evicted/expired entry), omit the field rather than fabricate one — Story 3.3.2 covers the case
  where omitting isn't safe to do silently. Update `send()`'s call site (`mod.rs`, Task 1.7.1b) to
  pass `&self.thought_signatures` and the derived `session_key`.
- Files: `src/providers/gemini/translate.rs`, `src/providers/gemini/mod.rs`

##### Task 3.3.1d: Unit tests for all three acceptance criteria, including the cross-session isolation case (~5 min)
- Files: `src/providers/gemini/translate.rs`

#### Story 3.3.2: Hard-error surfacing when a signature is missing/stale
**As** Tyler, **I want** a clear error (not a confusing raw Gemini 400) when a `thought_signature`
can't be supplied, **so that** I can tell "consolette's cache dropped it" from "Gemini rejected my
request for an unrelated reason."
**Acceptance Criteria**:
- A request replaying a `tool_use` id that was never seen for this `(session_key, ToolUseId)` pair
  (e.g. the process restarted between turns, the TTL evicted it, or — per Story 3.3.1's third
  criterion — it was cached under a different session) surfaces a `ProviderError::Validation`
  naming the gap, rather than silently omitting the field and letting Gemini's opaque 400 propagate
  unexplained.
  - *Given* a `tool_use` block `id: "toolu_unknown999"` with no corresponding
    `ThoughtSignatureCache` entry under the request's `session_key`, *When*
    `translate_anthropic_request_to_gemini` rebuilds that turn, *Then* it returns
    `Err(ProviderError::Validation("no cached thought_signature for tool_use id toolu_unknown999 — this conversation's tool-call history may predate a consolette restart or TTL eviction".to_string(), 400))`
    rather than silently sending the `functionCall` part without a signature.
**Files**: `src/providers/gemini/translate.rs`

##### Task 3.3.2a: Implement the `Err` path in `translate_anthropic_request_to_gemini` (~4 min)
- The fn has returned `Result<CloudCodeEnvelope, ProviderError>` since Task 1.3.1b (designed
  fallible from Phase 1 specifically to avoid a signature change here) — no signature churn, no
  `send()` call-site change needed. This task only adds the first real `Err(..)` return: when
  rebuilding a `tool_use` turn whose id has no matching `ThoughtSignatureCache` entry, return
  `Err(ProviderError::Validation(..))` per the acceptance criterion above instead of `Ok(..)`.
- Files: `src/providers/gemini/translate.rs`

##### Task 3.3.2b: Unit test for the acceptance criterion (~3 min)
- Files: `src/providers/gemini/translate.rs`

---

**End of plan.** Phase 1 alone (Epics 1.1-1.8) is independently shippable: a working, health-checked,
dashboard-visible, three-way-error-classified, text-only Gemini upstream. Phases 2 and 3 build on
it without requiring any Phase 1 rework.
