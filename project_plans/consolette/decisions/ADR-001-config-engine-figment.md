# ADR-001: Config Engine — figment (layered TOML conf.d + env overlay)

**Status**: Accepted
**Date**: 2026-07-17

## Context

FR-1 requires layered configuration loaded from `~/.config/consolette/conf.d/*.toml`
in lexical filename order, deep-merged, with environment variables as the
highest-precedence overlay (`defaults < conf.d files < env`). CD-4 fixes TOML as
the format (Rust-idiomatic and readable by Python `tomllib` for the future port,
CD-1). FR-1.5 requires fail-fast startup errors that name the offending file+key.
FR-1.6 reserves optional hot-reload as a deferred stretch.

The current `src/config.rs` reads ~13 unprefixed env vars via hand-rolled
`parse_env`/`parse_bool_env`/`parse_env_clamped` helpers with no file layer and no
prefix. It must be replaced with a layering engine.

## Decision

Adopt **`figment = { version = "0.10", default-features = false, features =
["toml", "env"] }`** as the config-layering engine.

- Precedence chain maps directly onto figment's provider model:
  `Figment::new().merge(Serialized::defaults(Config::default()))` → loop
  `.merge(Toml::file(f))` over **explicitly `sort()`-ed** glob results →
  `.merge(Env::prefixed("CONSOLETTE_").split("__").only(&[allowlist]))` →
  `.extract::<Config>()`.
- `.merge()` gives the required deep-merge semantics: **tables union recursively,
  scalars replace, arrays replace, later file wins.**
- Each array-of-tables is owned by exactly one file (`00-providers.toml` owns
  `[[upstreams]]`, `10-routing.toml` owns `[[routes]]`) because `.merge()` replaces
  arrays wholesale (no element-wise merge). `20-ratelimit.toml` uses
  **table-of-tables** (`[ratelimit.upstreams.<name>]`) so a later file can override
  one upstream's limit while others survive.
- **`glob` returns filesystem order, not sorted** — we `sort()` the results
  ourselves (true for figment and `config` alike).
- **Env overlay is restricted to a small allowlist of top-level scalars**
  (`.only(&["port","log","request_timeout","cooldown_seconds","config_dir"])`).
  `upstreams`/`routes` are file-only — env onto arrays-of-tables is not ergonomic.
- `#[serde(deny_unknown_fields)]` on all config structs turns typos into hard
  errors; a post-`extract()` `validate_references()` pass resolves route→upstream
  references semantically.
- Config is loaded once at startup into a **plain immutable `Arc<Config>`** for v1.
  Hot-reload (FR-1.6) AND `ArcSwap` are **deferred** — a local single-user proxy
  restarts cheaply, and adding `ArcSwap` now with no reload path is unjustified
  complexity/YAGNI. When hot-reload is actually built, the reload unit is an
  immutable `Arc<Runtime>` bundle (upstreams/routes/strategies/limiters) swapped via
  `ArcSwap`, with `HealthRegistry` kept OUTSIDE the swap so cooldown state persists
  across reloads. That is a self-contained future change; `notify` + the `Runtime`
  bundle land together, not piecemeal.

The on-disk shape is plain TOML with no serde renames or figment-only constructs,
so Python `tomllib` reads the same files unchanged (CD-1, NFR-2). The `__` env
nesting separator lives only in the process environment, never in the TOML.

## Alternatives Considered

| Option | Rejected because |
|--------|-----------------|
| `config` crate 0.15.25 (2026-active) | Viable runner-up; loses on **error precision** — less consistent at naming the *specific* file among many merged conf.d sources, which is exactly what FR-1.5 needs. figment attaches per-value source metadata (its headline feature). |
| Hand-rolled loader (extend current `parse_env`) | Reimplements deep-merge, precedence, and source-tracking that figment provides; more code, worse errors. |
| YAML/JSON config | Violates CD-4 (TOML is fixed for `tomllib` portability). |
| `notify` hot-reload in v1 | Deferred (FR-1.6 stretch): adds debounce, partial-write races, and validation-on-reload surface. Restart is cheap for a local single-user proxy. |
| `ArcSwap<Arc<Config>>` "reload-ready" in v1 | Rejected for v1 — wrapping a never-swapped `ArcSwap` with no reload path is speculative plumbing (YAGNI). When reload lands it swaps an `Arc<Runtime>` bundle, not `Config`, with `HealthRegistry` outside the swap. Use plain `Arc<Config>` now. |
| Env overrides onto `[[upstreams]]`/`[[routes]]` | Neither crate handles arrays-of-tables via env ergonomically; restricted to a top-level scalar allowlist instead. |

## Consequences

- New deps: `figment` only (small — reuses existing `toml`/`serde`/`glob`). No
  `arc-swap` in v1 (deferred with hot-reload).
- Startup gains a fail-fast path: bad TOML, unknown key (`deny_unknown_fields`),
  unknown route→upstream reference, and invalid `auth.type` each abort with a
  message naming file+key before the socket binds.
- A back-compat shim reads today's unprefixed env names (`PROXY_PORT`,
  `COOLDOWN_SECONDS`, `AWS_PROFILE`, `CLAUDE_CODE_OAUTH_TOKEN`, etc.) with a
  one-time deprecation warning, so the existing plist keeps working through the
  Epic 6 cutover.
- Handlers read the shared `Arc<Config>` directly (no per-request `.load()`), since
  v1 config is immutable after startup.
- The back-compat shim maps EVERY legacy env var the current `Config::from_env()`
  reads (`PROXY_PORT, COOLDOWN_SECONDS, REQUEST_TIMEOUT, BEDROCK_MAX_RETRIES,
  STAPLER_COMPRESS, COMPRESS_FLOOR_BYTES, CACHE_ALIGNER, VERBOSITY_LEVEL,
  MEMORY_MAX_ENTRIES, AWS_PROFILE, AWS_REGION, CLAUDE_CODE_OAUTH_TOKEN`) to a new
  target (top-level scalar, or the implicit anthropic/bedrock upstream's auth/options)
  with a one-time deprecation warning. Precedence for AWS: conf.d `options.aws_region`
  > `AWS_REGION` env > built-in default (same for `aws_profile`). A matrix test
  asserts each legacy name still takes effect (plan Story 1.5).
- Silent clamping (`parse_env_clamped`) becomes explicit validation — clearer.
</content>
