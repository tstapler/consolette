# Build vs. buy: configurable `base_url` for `UpstreamKind::Anthropic`

## 1. Existing OSS crate ("anthropic-sdk"-style client)

**Pros**
- Crates like `anthropic-sdk`, `misanthropy`, `clust` on crates.io do expose a configurable base
  URL / custom endpoint on their client builders, so the capability itself isn't novel.
- Would offload request/response schema maintenance for the Anthropic Messages API to an
  upstream maintainer.

**Cons**
- `Cargo.toml` has no Anthropic SDK dependency today — confirmed via `grep -n anthropic Cargo.toml`
  (only match is the crate's own top-level `description` string). The whole HTTP layer is built on
  a direct `reqwest = { version = "0.12", ... }` dependency
  ([Cargo.toml:80](Cargo.toml#L80)), with `AnthropicProvider` (`src/providers/anthropic.rs`)
  hand-rolling requests/streaming/SSE parsing, auth header injection (`Bearer`/`Apikey`/`Exec`),
  metrics, and retry logic directly.
- Swapping to an SDK crate would mean rewriting all of that hand-rolled logic to fit the SDK's
  request/response types and streaming model — a full rewrite of `AnthropicProvider`, not a
  base-URL change. Far larger blast radius than the ~10-20 line diff the requirements doc scopes.
- No evidence any of these third-party crates are more battle-tested or better maintained than
  this project's existing, working, tested hand-rolled client.

**Verdict: do not adopt an SDK crate.** The base-URL feature is orthogonal to the client
implementation; pulling in an SDK to get one field would force an unrelated, much larger rewrite.
Not worth it for this change.

## 2. SaaS/managed API

Not applicable. This is an internal proxy feature (letting `consolette` point an
`Anthropic`-kind upstream at a non-default endpoint) — there's no SaaS product that solves "add an
optional field to a config struct in my own Rust binary."

## 3. LLM-generated implementation vs. battle-tested library (URL-joining risk)

**Pros of hand-rolling (current pattern)**
- The exact same pattern already exists and works: `OpenaiProvider` builds its request URL via
  plain `format!("{}/v1/chat/completions", self.base_url)`
  ([src/providers/openai/mod.rs:171](src/providers/openai/mod.rs#L171), and identically at
  lines 234, 293, 345, 491). `AnthropicProvider` does the same today for its hardcoded URL
  ([src/providers/anthropic.rs:224](src/providers/anthropic.rs#L224), `:373`). No trailing-slash
  bug has surfaced in the `Openai` variant despite it being user-configurable since it was added.
- The `url` crate (a real dependency worth considering for join-safety) is already present, but
  only *transitively* — `grep -n '^name = "url"' -A2 Cargo.lock` shows `url 2.5.8` resolved (via
  `reqwest`'s stack), not as a direct dependency in `Cargo.toml`. Introducing `Url::join`-based
  construction would be a net-new direct dependency and a deviation from the `Openai` provider's
  established string-concatenation convention, for a case (fixed, code-controlled `/v1/...` path
  suffixes) where there's no untrusted or variable path segment to get wrong.

**Cons / risk**
- `format!("{base_url}/v1/messages")` will double up or drop a slash if a user supplies a
  base URL with a trailing `/` (e.g. `https://gateway/api/`) — a real but low-severity risk (wrong
  URL fails fast and loud with a connection/404 error; it's not a silent-corruption class of bug).
  The `Openai` variant has carried this identical risk since its base_url field was added, without
  a reported issue.

**Verdict:** this is squarely "too small to matter" for the build-vs-buy question. The diff is
~10-20 lines touching one struct field, one constructor param, one call site. Mirror the
`Openai` variant's plain `format!` string-concatenation exactly — do not introduce the `url` crate
as a direct dependency for this. If trailing-slash robustness is wanted, trimming a trailing `/`
off the input at construction time (one line) is proportionate; a full `Url::join` rewrite is not,
and would also mean auditing/changing the `Openai` provider for consistency (out of scope here).

## 4. Fork or adapt (existing in-repo pattern)

**Pros**
- `UpstreamKind::Openai { base_url: String }` ([src/config/schema.rs:117-118](src/config/schema.rs#L117))
  and `OpenaiProvider::new`'s `base_url: String` parameter
  ([src/providers/openai/mod.rs:101](src/providers/openai/mod.rs#L101), stored at line 59,
  used at lines 171/234/293/345/491) are exactly the shape the requirements doc asks for:
  a plain owned `String`, threaded straight through from config to provider constructor to
  request-URL formatting, with the identical `format!("{}/<path>", self.base_url)` idiom
  `AnthropicProvider` already uses for its hardcoded value.
- The call site to change is one match arm in `build_providers`
  ([src/routing/router.rs:89-94](src/routing/router.rs#L89), directly adjacent to the `Openai`
  arm at lines 98-101) — literally copy the `Openai` arm's `base_url` pass-through into the
  `Anthropic` arm.
- Test-harness gap the requirements doc flags (`src/providers/anthropic.rs` has no local mock
  server, unlike `openai/mod.rs`'s axum-based one) is also solvable by adapting-in-place: the
  `openai` module's `start_capturing_chat_completions_server`/`start_server` helpers
  (`src/providers/openai/mod.rs:1245`+, `resolution.rs:1411`+) are the concrete template for
  a new, smaller Anthropic equivalent (only need to capture the `Host` header / URL used, not
  simulate full chat-completions responses).

**Cons**
- None significant — `Upstream.auth`'s method flexibility (Bearer/Apikey/Exec) is a separate,
  already-orthogonal mechanism from `base_url`; the requirements doc's "open question" about
  auth-method flexibility doesn't block adopting the `Openai` pattern for `base_url` itself.

**Verdict — this is the recommended path, and it's what the requirements doc already directs.**
Copy `UpstreamKind::Openai`'s `base_url: String` field/constructor-param/call-site pattern onto
`UpstreamKind::Anthropic`, defaulting via serde's field default (matching the `model_family`
precedent the requirements doc cites) so existing configs stay byte-identical. Add a small
axum-based mock server to `src/providers/anthropic.rs`'s test module, modeled on (not necessarily
sharing code with) the `openai` module's, to regression-test that the configured URL is actually
used for `/v1/messages`.

## Summary

No SDK crate exists to adopt without a disproportionate rewrite; no SaaS applies; the size/risk
here doesn't justify introducing the `url` crate over the existing `format!`-based convention. The
actual answer is "fork/adapt," and the fork target is already named in the requirements doc: copy
`OpenaiProvider`'s `base_url: String` field, constructor parameter, and `router.rs::build_providers`
call-site pattern onto `AnthropicProvider` verbatim.
