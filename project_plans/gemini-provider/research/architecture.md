# Research: Architecture — gemini-provider

Scope: how `GeminiProvider` fits the existing `Provider`/`Router`/`AuthMethod`
contracts, and exactly what changes where. Sources are all read in full or by
targeted range from this repo's current `main` (commit `36238f0` at research
time) — no prior architecture research existed for `src/providers/`.

## 1. The `Provider` contract (`src/providers/mod.rs:1-143`)

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    async fn send(&self, body: Value, headers: HeaderMap, stream: bool)
        -> Result<ProviderResponse, ProviderError>;
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError>;
}
```

`ProviderResponse` is `Full(Value)` or `Stream(Pin<Box<dyn Stream<Item=Result<Bytes, anyhow::Error>> + Send>>)` — the streaming arm is a raw byte stream, format-agnostic at this layer (SSE bytes today, but nothing requires SSE specifically; `GeminiProvider` can produce SSE-framed Anthropic-shaped bytes even if Gemini's own wire format isn't SSE).

`ProviderError` (mod.rs:33-100) has 7 variants with classification methods the router branches on: `is_rate_limited`, `is_validation`, `is_auth`, `is_transient` (`Timeout | Upstream{..}`), `retry_after_secs`. **`GeminiProvider` must map every failure mode into this fixed vocabulary** — there's no room for a Gemini-specific error variant; protocol drift has to surface as one of these (see §3 for which one matters).

Only unchanged, load-bearing contract for `gemini.rs`: implement `Provider` and return errors correctly classified. Nothing else in the trait needs touching.

## 2. Config schema and the upstream factory

**`src/config/schema.rs:105-120`** — `UpstreamKind` is a `#[serde(tag = "kind")]` enum with `deny_unknown_fields`. Adding Gemini is one more variant, following the `Openai { base_url }` precedent (a struct variant carries whatever kind-specific fields are needed — for Gemini plausibly a `model_id`/`project_id`-style field per the open question about model-identifier mapping, or nothing if that's handled entirely by `RouteUpstreamRef.model`):

```rust
Gemini {
    // e.g. base_url override for cloudcode-pa.googleapis.com, if ever needed
},
```

Two places match on `UpstreamKind` **exhaustively** (compiler forces a new arm, which is the safety net — no risk of silently forgetting a call site):
- **`src/routing/router.rs:57-84`**, `build_providers()` — the factory. Each arm builds one concrete provider: `AnthropicProvider::new(upstream, resolver, exec_cache, timeout)`, `BedrockProvider::new(upstream).await`, `OpenaiProvider::new(upstream, base_url, resolver, exec_cache, timeout)`. A `UpstreamKind::Gemini { .. } => Arc::new(GeminiProvider::new(Arc::new(upstream.clone()), Arc::clone(&resolver), Arc::clone(&exec_cache), config.request_timeout)?)` arm slots in identically to `Anthropic`'s (both take resolver+exec_cache+timeout, no base_url needed if the internal endpoint is hardcoded like Anthropic's is).
- **`src/entrypoint/mod.rs:121-127`**, `upstream_kind_label()` — dashboard label lookup (`"anthropic"`/`"bedrock"`/`"openai"` → add `"gemini"`). This is the *only* dashboard-facing code that names a provider kind; per the Sept-2026 "remove hardcoded anthropic/bedrock assumptions" commit (`41996c9`), nothing else in metrics/dashboard hardcodes provider identity — confirms the requirement "no upstream-specific dashboard code" is achievable by touching only this one match arm.

One more registration site, **not** an exhaustive match but still required: `src/routing/router.rs:137-148` in `from_config()` sets `health.set_can_cooldown(idx, false)` for every Bedrock index (ADR-003: "Bedrock never cools down" — its failures are usually local/ambient-credential issues, not something a cooldown period fixes). Gemini should **not** be added to this list — it's a real network upstream like Anthropic/OpenAI where cooldown is exactly the right response to repeated failure (see §3 for a real gap here).

`src/config/load.rs` needs **no change** for the happy path — its `UpstreamKind::Bedrock` pattern matches are all legacy-env-var back-compat shims (`bedrock_upstream_mut`, `apply_legacy_env_shim`) that only exist because Bedrock predates the new config schema. Gemini has no legacy env var to shim, so `load.rs` is untouched.

## 3. Router dispatch / health — where a "wildly different protocol" bites

**`Router::dispatch`** (`src/routing/router.rs:242-344`) branches on the error classification, not on provider identity, so Gemini participates in fallback "for free" *as long as its errors are classified correctly*:

```rust
Err(e) if e.is_validation() || e.is_auth() => return Err(e);          // no failover, no cooldown
Err(e) if e.is_rate_limited() => { health.trip(idx, retry_after); }   // cooldown, then failover
Err(e) => { /* record, continue */ }                                  // failover, NO cooldown
```

**Concrete gap this surfaces for Gemini, not present for the other three providers today:** the catch-all arm (`Timeout`, `Upstream{..}`, `ModelUnsupported`) never trips `HealthRegistry::trip`. That's fine for Anthropic/OpenAI/Bedrock because their failure modes in that bucket are genuinely transient (one bad request, a flaky connection) — the next request to the same upstream is expected to succeed. But the requirements' Risk Control section explicitly wants:

> "`HealthRegistry` cooldown ... must trip on repeated Gemini failures so a broken/blocked internal endpoint doesn't get retried in a tight loop."

and the resilience requirement:

> "if the internal API's response shape changes unexpectedly, fail closed... so `HealthRegistry` cooldown kicks in instead of corrupting responses."

As the router is written today, a schema-drift failure (malformed/unrecognized JSON from `cloudcode-pa.googleapis.com`) that `GeminiProvider::send` correctly turns into `ProviderError::Upstream{..}` (fail-closed, doesn't corrupt the response — good) will **not** trip cooldown — it'll be retried on literally the next request, forever, adding one wasted round-trip of latency per request indefinitely rather than backing off. This is an implicit assumption in `Router::dispatch` that the "other errors" bucket is self-healing, which an undocumented, can-break-without-notice protocol violates.

Two ways to close this gap, to weigh in the plan phase (not decided here — this is a research finding, not a design decision):
1. **Classify schema-drift errors as `ProviderError::RateLimitedWithRetry`** with a short synthetic `retry_after` — semantically a lie (it's not a rate limit) but reuses the existing cooldown-trip path with zero `Router`/`HealthRegistry` changes. Cheap, but muddies `is_rate_limited()`'s meaning for anyone reading logs/metrics.
2. **Extend `Router`/`HealthRegistry`** with a consecutive-failure-triggered cooldown for `is_transient()` errors (e.g., trip after N failures in a row per upstream) — correct and reusable by any future provider, but touches shared code all four providers depend on, which the requirements explicitly say not to weaken/complicate for the other three ("Must not weaken or complicate the existing three providers' auth/config contracts"). A purely *additive* change (new opt-in behavior, default off / off for the existing three) would satisfy that constraint, but it's still shared-code surface area the Small/Large appetite tradeoff should weigh.

Recommend flagging this explicitly as an open design question for `sdd:3-plan`, not silently picking option 1.

**`HealthRegistry`** (`src/routing/health.rs`) itself needs no structural change either way — `can_cooldown` is already a per-index boolean the `Router` sets at construction time; Gemini simply gets the default (`true`, i.e., cooldown-eligible) by not being added to the Bedrock exclusion list. Only `Router::dispatch`'s trip *conditions* are the question, not `HealthRegistry`'s mechanism.

## 4. `AuthMethod::Exec` fit for the Antigravity CLI OAuth token

Read in full: `src/auth/exec.rs`, `src/auth/mod.rs`, `project_plans/consolette/decisions/ADR-007-plugin-format-and-credential-helper.md`.

The `exec` protocol (ADR-007 §2) is a **specific stdin/stdout JSON contract**, not "run any command and capture its output":

- consolette writes one JSON line to the child's stdin: `{"upstream":"<name>","method":"POST","url":"<full-url>"}`
- the child must print one JSON line to stdout and exit 0: `{"headers":{"Authorization":"Bearer ..."},"cache_ttl_secs":<optional override>}`
- non-zero exit / timeout / unparseable stdout → `AuthError::Exec` → `ProviderError::Auth` (via the `From<AuthError>` impl at `mod.rs:57-61`) — this already gives OAuth-refresh failures a *distinct* error class from a request-level 401 from the upstream itself (see below), satisfying the observability requirement's "log OAuth token refresh failures distinctly from request-level auth errors" essentially for free.
- results are cached per `(upstream, command, args)` hash for `cache_ttl_secs` (`ExecCredentialCache::get_or_run`, `exec.rs:102-138`).

**Verdict: `AuthMethod::Exec` fits, but only through a thin wrapper script, not by pointing `command` directly at `antigravity-cli`.** No existing helper (including the ADR-007 doc's own `gcloud auth print-access-token` analogy) speaks this stdin/stdout JSON protocol natively — `gcloud`'s equivalent flag prints a bare token to stdout with no stdin read at all. The pattern **already assumes a wrapper** for every real-world credential helper; Gemini is not a special case here, it's the normal case. Concretely: a small shim (shell one-liner, or a `bin/antigravity-cli-consolette-helper` script shipped alongside config, per the ADR-007 plugin `bin/` convention) that:
1. reads (and can ignore) the stdin JSON line,
2. runs `antigravity-cli <token-subcommand>` (if one exists — open question, needs the CLI installed and inspected) or reads its keyring/file-stored token directly,
3. emits `{"headers":{"Authorization":"Bearer <token>"}}` on stdout.

This requires **no new `AuthMethod` variant and no new consolette code** for the auth mechanism itself — it's the existing `Exec` machinery plus an external script, exactly matching how ADR-007 §4 already handles the employer-internal-identity plugin (`consolette-internal-auth-helper`) shelling out to a different internal CLI. The only fallback path that *would* require new code is if `antigravity-cli` exposes **no** CLI-accessible token at all and the token can only be read from its OS-keyring entry directly — then the wrapper script itself does the keyring read (e.g., via `secret-tool`/`libsecret` on Linux, shelled out from the wrapper, mirroring how `SystemSecretResolver::resolve_keychain` in `src/auth/mod.rs:60-79` already shells out to macOS `security` rather than pulling in a keyring crate) — still no core consolette code change, just a different wrapper implementation. A new `AuthMethod::Keyring` core variant is **not** architecturally warranted under either sub-case; it would only become necessary if a *future* provider needed keyring access **and** couldn't tolerate a subprocess per cache-refresh, which isn't a stated constraint here.

This resolves the open question in requirements.md ("does `antigravity-cli` expose a token-printing subcommand...or must consolette read its keyring/file storage directly") as a **wrapper-implementation detail, not an architecture decision** — either answer keeps the same `AuthMethod::Exec` config shape (`command` points at the wrapper, not at `antigravity-cli` itself).

## 5. Where Gemini-specific headers (`Client-Metadata`, spoofed `User-Agent`) live

Confirmed pattern from `AnthropicProvider::build_headers` (`src/providers/anthropic.rs:169-201`) and `OpenaiProvider::build_headers` (`src/providers/openai.rs:106-121`): each concrete provider has its **own** private `build_headers` method that:
1. sets `Content-Type` and any provider-specific static headers (Anthropic forwards `anthropic-version`/`anthropic-beta` from the incoming request),
2. calls the **shared** `apply_auth_headers` free function (defined once, in `anthropic.rs:390`, imported by `openai.rs:33` as `use super::anthropic::apply_auth_headers`) to layer in `Bearer`/`Apikey`/`Exec` per `AuthMethod` — this is the one function that must stay provider-agnostic.

`GeminiProvider::build_headers` follows the identical shape: set `Content-Type`, set any Cloud-Code-internal headers (`Client-Metadata`, a spoofed `User-Agent`, or whatever Phase 2's traffic capture determines is required) as static/derived values local to `gemini.rs`, then call the same imported `apply_auth_headers`. This cleanly keeps the shared auth path untouched (satisfying "must not weaken or complicate the existing three providers' auth/config contracts") while giving Gemini a private place for protocol-specific header spoofing that never leaks into `anthropic.rs`/`openai.rs`/`auth/mod.rs`.

Streaming note: `eventsource-stream = "0.2"` is already a direct dependency (`Cargo.toml:81`), used today for Anthropic/OpenAI SSE parsing — reusable as-is *if* Cloud Code's `streamGenerateContent` framing turns out to be SSE (a still-open Phase 2 question). If it's raw JSON-lines or gRPC-web framing instead, that reuse doesn't apply and `gemini.rs` needs its own framing/decoder — this is the single biggest unknown flagged in requirements.md's Rabbit Holes and isn't resolvable from this repo's code alone.

## 6. Integration points — files, in landing order

Ordered for the requirements' staged rollout (non-streaming → streaming → tool calls), each step buildable/testable independently:

1. **`src/config/schema.rs`** — add `UpstreamKind::Gemini { .. }` variant. Compiles alone (nothing constructs it yet); unblocks everything else and is where `cargo test` first catches any newly-non-exhaustive `match`.
2. **`src/routing/router.rs`** (`build_providers`) + **`src/entrypoint/mod.rs`** (`upstream_kind_label`) — add the two exhaustive-match arms. The compiler forces both the moment step 1 lands and `GeminiProvider` doesn't exist yet, so in practice steps 1–3 land in one commit (a stub `GeminiProvider` returning `ProviderError::ModelUnsupported` for everything is a legitimate intermediate compiling state if a smaller first commit is wanted).
3. **`src/providers/gemini.rs`** (new) — `GeminiProvider` struct + `Provider` impl, non-streaming path only for the first milestone: `send_request`, `build_headers` (reusing `apply_auth_headers`), request/response translation functions (Anthropic Messages ↔ Cloud-Code-internal JSON, text-only), error-status mapping mirroring `map_error_status` (`anthropic.rs:556-592`). `list_models` can start as a hardcoded/static list or `ProviderError::ModelUnsupported` stub if the internal API's model-listing endpoint (if any) isn't yet reverse-engineered — the `Provider` trait requires the method to exist, not that it be fully featured on day one.
4. **Auth wrapper script** (lives outside `src/`, likely in a to-be-created `references/` or a plugin-style `bin/` dir per ADR-007 §2 convention, NOT in core `src/auth/`) — the `antigravity-cli`-to-JSON-stdin/stdout shim from §4. Independently testable via `echo '{"upstream":"gemini",...}' | ./wrapper` without touching Rust at all.
5. **`Cargo.toml`** — only if streaming framing turns out to need something beyond `eventsource-stream`/`reqwest::Client::execute` streaming (e.g. a JSON-lines splitter is cheap to hand-roll and may need no new dep at all). Defer until Phase 2's traffic capture answers the framing question; don't add a speculative dependency now.
6. **Streaming milestone**: extend `gemini.rs`'s `send` to handle `stream: true` → `ProviderResponse::Stream(..)`, translating Cloud-Code-internal stream chunks to Anthropic SSE `event:`/`data:` framing, mirroring `AnthropicProvider`'s streaming arm (not read in full for this research pass — flag for the protocol/wire-format research agent to detail byte-for-byte). Independently testable against captured fixtures without the tool-call milestone existing yet.
7. **Tool-calls milestone**: extend the request/response translation to carry `tool_use`/`tool_result` content blocks both directions. Last, per the requirements' explicit staging ("a protocol surprise on tool calls doesn't block basic text completions from shipping").
8. **`references/conf.d/00-providers.toml`** (new — no `references/` directory exists in this repo yet, so this is a new file *and* new directory, not an edit) — example `kind = "gemini"` upstream entry, landed alongside or just after step 3 once the config shape is stable, so it's a living example rather than documentation of unimplemented config.
9. **`src/routing/router.rs::from_config`** — explicitly *not* touched to add Gemini to the `can_cooldown = false` list (see §3) — noted here so it isn't accidentally copy-pasted from the Bedrock precedent during implementation.

No changes needed in: `src/config/load.rs` (no legacy env shim applies), `src/routing/health.rs` (mechanism is generic; only `Router`'s *use* of it might need extending per §3), `src/auth/mod.rs`/`src/auth/exec.rs` (protocol already generic enough per §4), `src/metrics/*` (per-upstream attribution is already name-keyed, not kind-keyed, per the `41996c9` dashboard-generalization commit).

## Summary of open architectural questions carried into `sdd:3-plan`

- **Cooldown-on-schema-drift gap (§3)**: `Router::dispatch`'s catch-all error arm never trips `HealthRegistry`, which conflicts with the requirements' explicit expectation that repeated Gemini protocol-drift failures trip cooldown. Needs a decision (synthetic rate-limit classification vs. a real consecutive-failure cooldown extension) before implementation, not left to the implementer to improvise.
- **Streaming wire framing (§5)**: whether `eventsource-stream` is reusable depends entirely on Phase 2's traffic capture of `v1internal:streamGenerateContent` — this doc can't resolve it from static analysis.
- **Wrapper script's token source (§4)**: resolved architecturally (a wrapper around `AuthMethod::Exec`, no core change either way) but the wrapper's *contents* depend on installing and inspecting `antigravity-cli`, which is a Phase 2 research task, not an architecture one.
