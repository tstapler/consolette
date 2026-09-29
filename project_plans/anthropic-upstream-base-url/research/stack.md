# Stack research: configurable `anthropic` upstream base URL

## 1. Current constructor signatures

**`AnthropicProvider::new`** — `src/providers/anthropic.rs:82-87`:

```rust
pub fn new(
    upstream: Arc<Upstream>,
    resolver: Arc<dyn SecretResolver + Send + Sync>,
    exec_cache: Arc<ExecCredentialCache>,
    request_timeout_secs: u64,
) -> Result<Self, ProviderError>
```

No `base_url` parameter today. The hardcoded value lives inside the body (not
shown in this range, but referenced by the struct doc comment) and the struct
field's doc comment states it explicitly — `src/providers/anthropic.rs:53-56`:

```rust
/// Base URL for the Anthropic API. `UpstreamKind::Anthropic` carries no
/// base-URL override field in the new schema (only `Openai` does), so
/// this is hardcoded, matching legacy's default. See the final port
/// report for this gap.
base_url: String,
```

This comment is the one the requirements doc flags as needing an update
once `base_url` becomes configurable — it currently documents the exact gap
this feature closes.

**`OpenaiProvider::new`** — `src/providers/openai/mod.rs:99-106`:

```rust
pub fn new(
    upstream: Arc<Upstream>,
    base_url: String,
    resolver: Arc<dyn SecretResolver + Send + Sync>,
    exec_cache: Arc<ExecCredentialCache>,
    request_timeout_secs: u64,
    metrics: Arc<ProxyMetrics>,
) -> Result<Self, ProviderError>
```

`base_url` is the second positional parameter, right after `upstream`, as a
plain owned `String` (caller does `base_url.clone()` — see §4).

**Target signature for `AnthropicProvider::new`** (mirrors Openai's
`base_url` placement exactly; Anthropic has no `metrics` param, so nothing
else changes):

```rust
pub fn new(
    upstream: Arc<Upstream>,
    base_url: String,
    resolver: Arc<dyn SecretResolver + Send + Sync>,
    exec_cache: Arc<ExecCredentialCache>,
    request_timeout_secs: u64,
) -> Result<Self, ProviderError>
```

The struct doc comment on the `base_url` field (`src/providers/anthropic.rs:53-56`)
needs to drop the "carries no base-URL override field... hardcoded, matching
legacy's default" language and instead describe it as coming from
`UpstreamKind::Anthropic::base_url` (defaulted via serde, see §2).

## 2. Serde "optional field, default to hardcoded string" precedent

The codebase already has this exact shape on `AuthMethod::Apikey.header` —
`src/config/schema.rs:82-86`:

```rust
Apikey {
    key: SecretRef,
    #[serde(default = "default_apikey_header")]
    header: String,
},
```

with the default fn at `src/config/schema.rs:9-11`:

```rust
fn default_apikey_header() -> String {
    "x-api-key".to_string()
}
```

This is a `String` field (not `Option<String>`) on an enum variant, defaulted
via a named `#[serde(default = "fn")]` function — same shape needed for
`UpstreamKind::Anthropic { base_url: String }`. Other `default = "fn"` sites
follow the identical pattern (`default_cache_ttl_secs`, `default_exec_timeout_secs`
at schema.rs:95/97; `default_port`, `default_log`, `default_request_timeout`,
`default_cooldown_seconds`, `default_config_dir`, `default_compress_floor_bytes`,
`default_verbosity_level`, `default_memory_max_entries` — all listed via
`rg -n 'default[[:space:]]*=' src/config/schema.rs`).

Recommended addition, following the `default_apikey_header` naming
convention exactly:

```rust
fn default_anthropic_base_url() -> String {
    "https://api.anthropic.com".to_string()
}
```

and on the enum variant:

```rust
Anthropic {
    #[serde(default = "default_anthropic_base_url")]
    base_url: String,
},
```

Note: the requirements doc's own reference to `model_family` as precedent
(`src/config/schema.rs:167-168`, `#[serde(default)] pub model_family: Option<String>`)
is a *different* pattern — that's a bare `#[serde(default)]` on an
`Option<T>`, defaulting to `None` (Rust's own `Default::default()`), not to
a hardcoded non-empty string. For this feature, `default_apikey_header` is
the closer and correct precedent since the field must default to a
*specific non-empty string*, not `None`/empty.

Also note: `Bedrock`'s fields use bare `#[serde(default)]` three times
(`src/config/schema.rs:110/113/115`, aws_region/aws_profile/max_retries, all
`Option<T>`) — again defaulting to `None`, not applicable here for the same
reason.

## 3. Cargo.toml dependency versions — no new dependency needed

Confirmed via `rg -n "^serde|^serde_json|^reqwest|^axum|^tokio" Cargo.toml`:

- `serde = { version = "1", features = ["derive"] }` (Cargo.toml:50)
- `serde_json = "1"` (Cargo.toml:51)
- `reqwest = { version = "0.12", default-features = false, features = ["json", "stream", "rustls-tls"] }` (Cargo.toml:80)
- `axum = { version = "0.8", features = ["macros"] }` (Cargo.toml:76) — **not** dev-only; it's a direct dependency already used outside test code elsewhere in the binary, and is already exercised for exactly this kind of test in `src/providers/openai/mod.rs`.
- `axum-extra = { version = "0.10", features = ["typed-header"] }` (Cargo.toml:77)
- `tokio = { version = "1", features = ["full"] }` (Cargo.toml:47) plus a second `tokio = { version = "1", features = ["test-util"] }` under `[dev-dependencies]` (Cargo.toml:122)

Nothing here needs bumping or adding. The `#[serde(default = "fn")]` pattern
needs no serde feature beyond `derive` (already enabled). Regression-testing
a configurable base URL against a local mock server needs no new dependency
either — `axum::serve` + `tokio::net::TcpListener` bound to `127.0.0.1:0` is
already the exact harness used in `src/providers/openai/mod.rs` (e.g. around
lines 1285-1341, 1556-1617: an `axum::Router` with a captured-body handler,
served via `axum::serve(listener, app)` in a spawned task, with the test
pointing its provider's `base_url` at the ephemeral port). The anthropic
module has no such harness today (per the requirements doc's "Known
constraints" section) — reusing/generalizing this openai one, or copying its
shape into `anthropic.rs`, is the implementation-time decision; no new crate
is required either way.

## 4. `build_providers` construction call sites

`src/routing/router.rs:75-114`, function `build_providers`:

- **`UpstreamKind::Anthropic` arm** — `src/routing/router.rs:89-94`:

  ```rust
  UpstreamKind::Anthropic => Arc::new(AnthropicProvider::new(
      Arc::new(upstream.clone()),
      Arc::clone(&resolver),
      Arc::clone(&exec_cache),
      config.request_timeout,
  )?),
  ```

  Note the match pattern itself is a bare `UpstreamKind::Anthropic` (unit
  variant, no bindings) — once `base_url` is added to the variant, this
  pattern must become `UpstreamKind::Anthropic { base_url }` (or
  `{ base_url, .. }`) to bind and pass it through, matching how the `Openai`
  arm already destructures its field (next bullet).

- **`UpstreamKind::Openai { base_url }` arm** — `src/routing/router.rs:98-105`:

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

  This is the exact pattern to mirror for the `Anthropic` arm: destructure
  `base_url` in the match pattern, then pass `base_url.clone()` as the
  second constructor argument (matching the new `AnthropicProvider::new`
  signature proposed in §1).

Other `UpstreamKind::Anthropic`/`UpstreamKind::Openai` references (for
context, not construction sites): `src/config/schema.rs:353` (a test
fixture using the bare `UpstreamKind::Anthropic` unit-variant construction —
will need `{ base_url: ... }` or rely on `Default`/a test-only helper once
the field is added); `src/routing/router.rs:1265,1299,1333` (`Openai` test
fixtures); `src/routing/router.rs:1865,1908,2092,2144` (`Anthropic` test
fixtures — all currently constructed as the bare unit variant and will need
updating to the struct-variant form, or a helper constant/fn, once
`base_url` is added).

## Open question carried over (not resolved by this research)

The requirements doc's open question — whether `base_url` should also allow
the auth-method shape to vary — is a design decision, not a stack/library
question, and is out of scope for this research pass.
