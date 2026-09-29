# Features research: configurable `anthropic` upstream `base_url`

## 1. Existing `base_url` validation (openai kind) — VERIFIED

`UpstreamKind::Openai { base_url: String }` ([src/config/schema.rs:117-119](../../../src/config/schema.rs#L117-L119))
has **no URL-shape validation anywhere**:

- `src/config/validate.rs` only has two checks — `validate_references` (upstream-name
  existence) and `validate_model_selectors` (model/model_family mutual exclusivity). Neither
  touches `base_url`'s contents. Confirmed by reading the whole file — no `Url::parse`, no
  `starts_with("http")`, no non-empty check anywhere in `src/config/`.
- `grep -rn "base_url" src/config/` turns up only the field declaration and tests that always
  supply a valid-looking placeholder (`"https://example.invalid"`, `"https://x"`).
- Consequence: an empty string, a bare hostname without scheme, or a typo'd URL is accepted at
  config-load time and only surfaces later as a `reqwest::Error` (`builder error` for a
  scheme-less URL, or a connection/DNS failure) inside `OpenaiProvider::send`'s existing
  `ProviderError::Upstream` mapping — i.e., first-request time, not config-load time.
- **Recommendation**: match this precedent exactly — do not add new validation for the
  Anthropic `base_url` that the Openai one doesn't have. Adding asymmetric strictness (e.g.
  `Url::parse` only for anthropic) would be surprising and inconsistent, and isn't asked for in
  the requirements doc's "desired outcome."

## 2. Edge cases in the construction pattern

- **Trailing slash**: `AnthropicProvider` builds its request URL via
  `format!("{}/v1/messages", self.base_url)` ([src/providers/anthropic.rs:224](../../../src/providers/anthropic.rs#L224))
  and `format!("{}/v1/models", self.base_url)` ([src/providers/anthropic.rs:373](../../../src/providers/anthropic.rs#L373)).
  `OpenaiProvider` has the *identical* pattern in 4+ places (`format!("{}/v1/chat/completions", self.base_url)` etc.,
  [src/providers/openai/mod.rs:171,234,293,345,491](../../../src/providers/openai/mod.rs#L171)).
  Neither strips a trailing slash. A configured `base_url = "https://host.example/"` produces a
  double-slash URL (`https://host.example//v1/messages`) — reqwest/most HTTP servers normalize
  this fine in practice, but it's an existing, un-flagged wart shared by both providers, not
  something new to solve for this feature. Since the requirements doc says "mirror
  `OpenaiProvider::new`'s existing pattern exactly," the correct scope is to **replicate the
  same un-normalized `format!` pattern**, not silently fix it only for Anthropic (that would
  create behavioral drift between the two kinds). Flagging trailing-slash normalization as an
  optional stretch/follow-up, not in scope.
- **http vs https**: Nothing in the codebase distinguishes or restricts scheme; `reqwest` handles
  both transparently. No action needed — an `http://` override (e.g. a local mock/dev proxy) will
  work exactly like it does for `openai` kind today (openai's own test harness uses `http://127.0.0.1:<port>`
  for its axum mock server, confirmed at [src/providers/openai/mod.rs:1320-1341](../../../src/providers/openai/mod.rs#L1320)).
- **Different Anthropic API version**: `AnthropicProvider` doesn't version-namespace its path
  beyond `/v1/` (`anthropic-version` is presumably a header, not investigated further — out of
  this feature's scope since the requirements doc doesn't ask for pluggable API versions, only a
  pluggable host/base).

## 3. Serde default-value pattern (matches "unset stays unchanged" convention)

Requirements doc cites `model_family` as the "unchanged when unset" precedent, but that field is
`Option<String>` (`#[serde(default)]` → `None`) — a plain `Option` default, not a *non-trivial*
default value. The correct precedent for "unset field takes a specific hardcoded default" is the
`#[serde(default = "fn_name")]` pattern already used throughout `src/config/schema.rs` for
non-Option fields with real defaults: `default_apikey_header`, `default_cache_ttl_secs`,
`default_exec_timeout_secs`, `default_max_delay_ms`, `default_port`, `default_log`,
`default_request_timeout`, `default_cooldown_seconds`, `default_config_dir`, `default_true`,
`default_compress_floor_bytes`, `default_verbosity_level`, `default_memory_max_entries`
([src/config/schema.rs:84-280](../../../src/config/schema.rs#L84)). The new field should follow
this exact idiom:
```rust
Anthropic {
    #[serde(default = "default_anthropic_base_url")]
    base_url: String,
},
```
with a `fn default_anthropic_base_url() -> String { "https://api.anthropic.com".to_string() }`
free function alongside the other `default_*` fns. This keeps existing TOML configs (with no
`base_url` key under `kind = "anthropic"`) byte-identical in behavior, satisfying the
requirements doc's explicit ask, and keeps `Anthropic` a struct-variant (breaking the current
bare-unit-variant shape) — every call site matching `UpstreamKind::Anthropic` as a unit pattern
(`src/routing/router.rs:89`, `src/entrypoint/mod.rs:173`, `src/config/validate.rs:44`,
`src/server_tools/mod.rs:217`, plus test fixtures at `src/config/schema.rs:353`,
`src/cost_metrics/estimator.rs:464`, `src/routing/router.rs:1865,1908`) will need updating to
`UpstreamKind::Anthropic { .. }` or `UpstreamKind::Anthropic { base_url }` — a mechanical but
non-trivial fan-out (8+ sites found via `grep -rn "UpstreamKind::Anthropic"`).

## 4. Construction site (`routing/router.rs::build_providers`)

Current openai arm ([src/routing/router.rs:98-105](../../../src/routing/router.rs#L98)):
```rust
UpstreamKind::Openai { base_url } => Arc::new(OpenaiProvider::new(
    Arc::new(upstream.clone()),
    base_url.clone(),
    Arc::clone(&resolver),
    Arc::clone(&exec_cache),
    config.request_timeout,
    Arc::clone(&metrics),
)?),
```
Current anthropic arm ([src/routing/router.rs:89-94](../../../src/routing/router.rs#L89)) takes no
`base_url` today. Mirroring the openai pattern means destructuring `UpstreamKind::Anthropic { base_url }`
in the match arm and passing `base_url.clone()` as a new `AnthropicProvider::new` parameter,
matching `OpenaiProvider::new`'s existing parameter ordering/style (`base_url: String` taken by
value after the `Arc<Upstream>`).

## 5. Doc comment to update

`AnthropicProvider`'s struct field doc ([src/providers/anthropic.rs:53-56](../../../src/providers/anthropic.rs#L53))
currently reads: *"`UpstreamKind::Anthropic` carries no base-URL override field in the new schema
... this is hardcoded, matching legacy's default. See the final port report for this gap."* This
is the exact comment the requirements doc calls out as needing an update — it should be rewritten
to describe the new configurable field and drop the "gap"/"port report" framing, since the gap is
being closed by this feature.

## 6. Test harness

No local mock-server harness exists in `src/providers/anthropic.rs` today (confirmed: no
`axum`/`TcpListener`/`mock` hits when grepping that file). `src/providers/openai/mod.rs` has one
(`start_capturing_chat_completions_server`, [src/providers/openai/mod.rs:1320-1341](../../../src/providers/openai/mod.rs#L1320)) —
a `tokio::net::TcpListener` bound to `127.0.0.1:0` serving a minimal `axum::Router` that captures
the request body into an `Arc<Mutex<Option<Value>>>`. Given `axum` is already a direct dependency
(not a dev-only mock crate), the requirements doc's "reuse/generalize" option is realistic: the
handler and server-bootstrap could be extracted into a small shared test-support module (e.g.
`src/providers/test_support.rs` or a `mod test_helpers` under `#[cfg(test)]`) parameterized by
path (`/v1/messages` vs `/v1/chat/completions`) and response shape, then used by both provider
test modules. This is a reasonable scope addition for the plan phase, not something to build in
this research doc.

## 7. Auth-method question (open question in requirements doc)

`Upstream.auth: Option<AuthMethod>` (`Bearer`/`Apikey`/`Exec`) is already upstream-kind-agnostic —
it's a field on `Upstream`, not inside `UpstreamKind::Anthropic`, and `AnthropicProvider` already
reads `self.upstream.auth` generically (header selection driven by config, per the module's own
doc comment, [src/providers/anthropic.rs:13-17](../../../src/providers/anthropic.rs#L13)). So this
"open question" is effectively already answered by the existing schema shape: **no new work is
needed** — any Anthropic-compatible endpoint already gets to pick `Bearer`/`Apikey`/`Exec`
independent of `base_url`, exactly like `openai` kind does today. Worth stating explicitly in the
plan so it isn't mistaken for a design decision still pending.

## 8. Industry precedent (light web check)

- **LiteLLM** supports `ANTHROPIC_API_BASE`/`ANTHROPIC_BASE_URL` env vars to override the
  Anthropic base URL, and by default auto-appends `/v1/messages` (or `/v1/complete`) to whatever
  base is given — i.e., the same `format!("{base}/v1/messages")` shape this codebase already
  uses. They later added `LITELLM_ANTHROPIC_DISABLE_URL_SUFFIX` after user reports that internal
  API gateways proxy Anthropic under non-standard paths (`/messages`, `/api/chat` instead of
  `/v1/messages`), and that suffix auto-append broke those integrations
  ([BerriAI/litellm#13945](https://github.com/BerriAI/litellm/pull/13945),
  [BerriAI/litellm#4803](https://github.com/BerriAI/litellm/issues/4803) — confusion between
  `ANTHROPIC_API_BASE` "exact endpoint" vs `ANTHROPIC_BASE_URL` "base to suffix" semantics is a
  real recurring source of bug reports there).
- **Takeaway for consolette**: the requirements doc's scope (simple `base_url` + hardcoded
  `/v1/messages` suffix, matching what `openai` kind already does) is the right-sized MVP and
  matches what LiteLLM shipped *first*, before user pressure forced the more flexible
  suffix-disable escape hatch. Not recommending building that escape hatch now — it's a real but
  currently-hypothetical need (no consolette user has asked for a non-standard-path gateway), and
  the requirements doc doesn't call for it. Worth a one-line callout in the plan as a known,
  deliberately-deferred generalization, so a future "my gateway doesn't use `/v1/messages`"
  request isn't a surprise.
- Sources: [LiteLLM Anthropic provider docs](https://docs.litellm.ai/docs/providers/anthropic),
  [BerriAI/litellm PR #13945](https://github.com/BerriAI/litellm/pull/13945),
  [BerriAI/litellm issue #4803](https://github.com/BerriAI/litellm/issues/4803).

## 9. Gemini/openrouter project-plan cross-check

`project_plans/gemini-provider/research/` and `project_plans/openrouter-routing/research/` were
checked for prior art on adding a provider-adjacent config field. Neither surfaced base_url- or
trailing-slash-specific edge cases directly transferable here — their `pitfalls.md` docs focus on
different concerns (multimodal content translation gaps in gemini's case, scoring/weighting
defaults in openrouter's case). No reusable edge-case list found there beyond what's already
captured above from directly reading the openai/anthropic provider code.

## Unstated needs worth flagging in planning

- **8+ call-site fan-out** from turning `Anthropic` into a struct variant (see §3) — plan should
  size this as "small but touches many files," not "one field."
- **Doc-comment currency**: the struct field comment (§5) referencing "the final port report" is
  itself now stale the moment this ships — should be rewritten, not just amended.
- **Test harness reuse decision (§6)** is a real design fork the plan phase must make explicitly:
  build Anthropic its own minimal mock server vs. extract/share the openai one. Recommend
  extracting a shared helper given both need near-identical "capture JSON body, return canned
  response" behavior — duplicating ~20 lines of axum bootstrap per provider module isn't a big
  cost either way, so this is a taste call, not a correctness one.
- **Auth-method open question is already resolved by existing schema** (§7) — don't let planning
  treat it as new work.
