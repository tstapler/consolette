# Requirements: gemini-provider

**Date**: 2026-09-04
**Type**: feature addition
**Complexity**: 3 — system design (external integration, Large appetite)

## Problem Statement
Consolette (a local, provider-agnostic LLM router) currently supports only Anthropic, AWS Bedrock, and generic OpenAI-compatible upstreams (`UpstreamKind::{Anthropic, Bedrock, Openai}` in `src/config/schema.rs:107`). Tyler has a Google Antigravity subscription and wants to route requests to Gemini models through consolette — for unified fallback/weighted-routing, cost tracking, and dashboard visibility alongside his existing Anthropic/Bedrock upstreams — rather than using Gemini only inside the separate Antigravity IDE.

## Baseline
Today there is no Gemini upstream at all. Using Gemini means opening the Antigravity IDE directly — no shared routing, no fallback into/out of the Claude/Bedrock upstreams, no unified cost/metrics tracking, no per-session model override through consolette's existing `routing/session_overrides.rs`.

## Users / Consumers
Tyler, as the sole operator, configuring a `kind = "gemini"` upstream in `~/.config/consolette/conf.d/00-providers.toml` and routing to it via the existing weighted/fallback routing strategy, same as the Anthropic/Bedrock/OpenAI upstreams today.

## Success Metrics
- A configured Gemini upstream successfully completes non-streaming and streaming `/v1/messages` requests end-to-end (translated to/from Anthropic Messages format), exercised the same way `AnthropicProvider`/`BedrockProvider`/`OpenaiProvider` are today.
- Tool calls (function calling) round-trip correctly through the Anthropic↔Gemini translation.
- The Gemini upstream participates in `Router::dispatch`'s health/cooldown/fallback machinery identically to the other three providers — a Gemini failure doesn't take down or block other upstreams, and vice versa.
- Cost/token metrics for Gemini requests show up in the existing `metrics`/dashboard pipeline (per-upstream attribution, per Task 3.4.5 in the older consolette plan) without upstream-specific dashboard code.
- **Usage/value-realization metric** (added 2026-09-04, Phase 4 product-lens repair loop — closes a gap the pre-mortem itself flagged, #5): within 4 weeks of Phase 1 shipping, `/metrics`'s per-upstream request counter for `gemini` shows non-trivial real traffic, not just test requests exercised during implementation. If it doesn't, that absence is itself a signal to revisit the Fallback-last routing default (see Risk Control) — e.g. shifting weight toward Gemini via a per-session override or `Strategy::Weighted` — rather than being silently accepted as "the feature works, it's just rarely used."

## Appetite
Large (3–6 weeks). *(Scope must fit the appetite. If it doesn't fit, cut scope — do not move the deadline.)*

## Constraints
- No deadline; this is personal infrastructure, not team-coordinated work.
- Must not embed a harvested/third-party OAuth client secret (the approach used by the now-superseded community reverse-engineering plugin). Auth must go through the official Google-maintained `antigravity-cli` OAuth flow (browser/keyring-based, client credentials owned by Google) — see Decision below.
- Must not weaken or complicate the existing three providers' auth/config contracts (`AuthMethod::{Bearer,Apikey,Exec}`) to accommodate this one upstream.

## Decision: Auth & wire protocol (made during ideation, not left open)
Investigated three paths for Gemini access:
1. **Google AI Studio API key** — fully public, documented API (`generativelanguage.googleapis.com`), zero new auth code (fits existing `AuthMethod::Apikey`). Lowest risk, but not what Tyler wants to use — his subscription entitlement is Antigravity, not a metered AI Studio key.
2. **Vertex AI** — GCP service-account/OAuth, materially bigger new auth surface, not the subscription Tyler holds.
3. **Antigravity CLI OAuth (chosen)** — Google shipped an official standalone `antigravity-cli` at I/O 2026 (May 19, 2026), replacing Gemini CLI as the sanctioned way to use a Gemini/Antigravity subscription outside the IDE ([github.com/google-antigravity/antigravity-cli](https://github.com/google-antigravity/antigravity-cli), [antigravity.google/docs/cli](https://antigravity.google/docs/cli/install/)). It does a real Google OAuth flow and stores the token in the OS keyring — no harvested secret. **However**, the resulting token is only valid against `cloudcode-pa.googleapis.com`'s `v1internal:streamGenerateContent` — Google's undocumented internal Cloud Code Assist protocol, not the public Gemini API. There is no published request/response schema for it; a community project (`elad12390/antigravity-proxy`) exists specifically to reverse-engineer that traffic.

**Explicit tradeoff accepted by Tyler with this understanding:** credential acquisition is now legitimate and stable (official Google OAuth client, keyring-stored token, standard refresh), but the wire protocol itself is undocumented and can change without notice since it's Google-internal, not a published API. This is the accepted risk driving the Large appetite and the resilience requirements below.

## Non-functional Requirements
- **Performance SLO**: not specified — match existing provider latency characteristics (no artificial throttling beyond what `AdmissionControl`/rate limiting already impose).
- **Scalability**: single local operator, not applicable.
- **Security classification**: internal/personal. The OAuth token must be handled the same way other upstream secrets are (via `SecretResolver`/`AuthMethod`, never logged, never committed to config in plaintext).
- **Data residency**: no special requirements.

## Scope
### In Scope
- New `UpstreamKind::Gemini` config variant in `src/config/schema.rs`.
- `GeminiProvider` in `src/providers/gemini.rs` implementing the `Provider` trait (buffered + streaming), mirroring `anthropic.rs`/`openai.rs` structure: ADR-004 two-`reqwest::Client` split, `ProviderError` classification (429/timeout/auth/validation arms), single-attempt-per-`send()` (cross-provider fallback stays in `Router`).
- Anthropic Messages ↔ Gemini/Cloud-Code-internal request/response translation, including tool calls and SSE streaming.
- Auth: obtaining and detecting expiry of the Antigravity CLI OAuth token (not refreshing it — resolved during Phase 3 planning, see ADR-001: consolette never attempts a standalone OAuth refresh grant, since doing so would need a client_id extracted from the `agy` binary, crossing back into the harvested-credential territory this doc's Constraints section rules out; expiry is detected and reported as an actionable fail-closed error instead). Preferred approach (to confirm in Phase 2 research): reuse the existing `AuthMethod::Exec` credential-helper pattern (ADR-007) to shell out to `antigravity-cli` for a fresh access token, rather than consolette re-implementing its own OAuth flow — mirrors how `gcloud auth print-access-token` is commonly used as an exec credential helper. If `antigravity-cli` has no such subcommand, reading its keyring/token-file storage directly is the fallback (research question, see below).
- `references/conf.d/00-providers.toml` example for a `kind = "gemini"` upstream.
- Tests mirroring existing provider test patterns (request/response translation round-trip, error classification, auth header/exec construction).
- Basic resilience to protocol drift: if the internal API's response shape changes unexpectedly, fail closed with a clear `ProviderError` (not a silent misparse) so `HealthRegistry` cooldown kicks in instead of corrupting responses.

### Out of Scope
- Google AI Studio API key path and Vertex AI path (not needed now; Tyler's entitlement is Antigravity — no dual-path requirement was requested).
- Multimodal input (images/files/audio) and embeddings — text + tool calls only for this pass.
- Gemini model IDs other than `gemini-3-pro` — best-effort passthrough only, no first-class support or testing.
- Consolette re-implementing the OAuth browser/device flow itself — it consumes a token `antigravity-cli` already manages, it does not become an OAuth client.
- Redistributing or documenting the reverse-engineered protocol as a public integration (this is for Tyler's personal use of his own subscription).

## Rabbit Holes
- **Protocol discovery.** The exact `v1internal:streamGenerateContent` request/response shape (including tool-call and streaming chunk format) isn't documented anywhere official — Phase 2 research needs to actually capture real traffic (via `antigravity-cli`/Antigravity IDE network inspection, and reading `elad12390/antigravity-proxy`'s implementation as a reference) rather than guessing from partial blog posts. This is the single biggest unknown driving the Large appetite.
- **Token acquisition mechanism.** Whether `antigravity-cli` exposes a `print-access-token`-style subcommand (clean `AuthMethod::Exec` fit) or only stores the token in an OS keyring/local file with no CLI accessor (requiring new code to read it) is unverified — `antigravity-cli` isn't installed on this machine yet (only Antigravity IDE state exists at `~/.gemini/antigravity-cli/`). Install it and check before designing the auth integration.
- **Endpoint/header spoofing requirements.** The old reverse-engineered plugin sent IDE-mimicking headers (`User-Agent`, `Client-Metadata`) to the internal endpoint. Whether the *official* `antigravity-cli`'s traffic still needs equivalent headers (and what happens if consolette's `reqwest` client doesn't send them) is unverified — could be a silent-rejection trap.
- **Protocol churn during development.** Because this is undocumented and Google-internal, the shape could change between the research/plan phase and shipping. Budget slack for this in Phase 5.

## Alternatives Considered
- Google AI Studio API key and Vertex AI — see Decision above; rejected because they don't use Tyler's actual subscription entitlement.
- Not building this at all and using the Antigravity IDE directly for Gemini work — rejected because it loses consolette's unified routing/fallback/cost-tracking value.

## Feasibility Risks
- Google could change or block the internal `cloudcode-pa.googleapis.com` protocol without notice (it's explicitly not a supported external surface), breaking the provider until re-reverse-engineered.
- `antigravity-cli` itself is new (May 2026) and its CLI surface/token-storage mechanism may still be in flux.
- ToS risk remains nonzero even with official OAuth: Google's terms may not contemplate third-party tools consuming Antigravity-issued tokens against the internal API. (Acknowledged and accepted by Tyler for personal use.)

## Observability Requirements
- Reuse the existing per-upstream metrics/dashboard attribution (`src/metrics/`) for the Gemini upstream — no upstream-specific dashboard code.
- Add explicit logging/metric on "unexpected response shape from Gemini upstream" (schema-drift signal) distinct from ordinary `ProviderError::Upstream` — this is the earliest warning that Google changed the internal protocol.
- Log (not just error) OAuth token refresh failures distinctly from request-level auth errors, since the former means the whole upstream is down until re-authenticated via `antigravity-cli`.

## Risk Control
- **Feature flag**: none needed — the provider is inert unless a `kind = "gemini"` upstream is explicitly added to `conf.d`. Rollback is deleting that config entry; no schema migration required.
- **Blast-radius isolation**: `HealthRegistry` cooldown (already shared by all providers) must trip on repeated Gemini failures so a broken/blocked internal endpoint doesn't get retried in a tight loop, and — per existing `Router::dispatch` behavior — never blocks routing to the Anthropic/Bedrock/OpenAI upstreams.
- **Staged rollout**: land non-streaming text completions first (mirroring the Small-appetite fallback plan from the interview, folded into this Large-appetite plan as an internal milestone), then streaming, then tool calls — each independently testable before moving on, so a protocol surprise on tool calls doesn't block basic text completions from shipping.
- **Drift-detection habit** (added 2026-09-04, Phase 4 product-lens repair loop — closes pre-mortem P2 #1): because the shipped example config uses Fallback-last, a broken/drifted Gemini upstream produces zero user-visible symptoms — traffic just reverts to Anthropic/Bedrock and the router keeps "working." This must not rely on noticing it by chance. Check the dashboard's `gemini` row at least weekly during the first month (looking for `status-schema-drift`/`status-auth-required`, not just a green dot), the same dated go/no-go-checkpoint pattern already used above for the auth-cadence risk. This is a documented habit, not new code or alerting infrastructure — deliberately lightweight for a personal single-operator tool.

## Open Questions — resolved by Phase 2 research (2026-09-04)
- **Token-printing subcommand**: No. `antigravity-cli` (`agy`) has no auth/login/print-access-token subcommand. Consolette reads the OAuth token directly from `~/.gemini/antigravity-cli/antigravity-oauth-token` (JSON, mode 600). `AuthMethod::Exec` still applies, but via a new small credential-helper script consolette owns, not `agy` itself.
- **Extra headers**: Confirmed required beyond the bearer token: `X-Goog-Api-Client`, `Client-Metadata`, and a Cloud-Code-Assist request envelope (`{project, model, requestType, request:{...}}`) wrapping Gemini-native `contents`/`parts`.
- **Streaming framing**: Confirmed SSE (`data: {...}` lines) against `POST https://cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse` — closer to `openai.rs`'s `Eventsource` pattern than Anthropic's bracketed-event framing. No new streaming crate needed.
- **Model identifiers**: Decision made — **`gemini-3-pro` only for v1** (the model this subscription is actually for), including first-class handling of its mandatory `thought_signature` echo-back on every `functionCall` turn. Other model IDs are out of scope / best-effort passthrough, not a hard requirement.

## New Open Questions raised by Phase 2 research (unresolved — carried into Phase 3 planning)
- **Token refresh mechanism is unknown.** The stored OAuth token found on this machine was already expired, and `antigravity-cli` exposes no refresh subcommand. Phase 3 must design how consolette detects an expired token and either refreshes it (if a refresh-token grant against Google's OAuth endpoint turns out to work standalone) or fails closed with a clear "run `antigravity-cli login` again" error — this is now on the critical path, not a nice-to-have.
- **`HealthRegistry` cooldown gap** (architecture research): `Router::dispatch` today only trips cooldown on rate-limit errors, not `Timeout`/`Upstream` (parse-failure) errors — conflicts with this project's requirement that repeated protocol-drift failures trip cooldown. Phase 3 must decide: classify Gemini parse-drift as a synthetic rate-limit for cooldown purposes, or extend `Router`/`HealthRegistry` with a real consecutive-failure cooldown path (which would also benefit the other three providers).
- **Error-state UX priority** (UX research): the three-way distinction between self-healing transient errors, "needs `antigravity-cli login`" auth errors, and "protocol drifted, needs a code fix" parse errors should land in the *first* internal milestone (non-streaming text), not be deferred as follow-up dashboard polish — a silent misparse undermines trust in the whole router more than missing tool-call support would.

## Risk re-confirmation (2026-09-04, after Phase 2 research)
Phase 2 pitfalls research found that Google has already run **documented mass account suspensions** for third-party-tool usage of Antigravity/Gemini-CLI credentials (`google-gemini/gemini-cli` Discussion #20632, corroborated by other reports) — this is an occurring pattern, not a hypothetical "nonzero ToS risk" as originally framed in the Decision section above. Tyler was shown this evidence directly and chose to **proceed anyway, accepting the account-suspension risk**, for personal use. This supersedes the softer risk language in the original Decision/Feasibility Risks sections above — treat "documented, occurring risk, accepted" as the operative framing going forward, not "nonzero risk."
