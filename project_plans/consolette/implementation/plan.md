# Implementation Plan: Consolette

**Feature (one-liner):** Evolve `claude-proxy-rs` into **Consolette**, a local
provider-agnostic LLM router — layered TOML config, pluggable per-upstream auth,
fallback+weighted routing behind one trait, per-upstream rate limiting, an internal
Model Gateway upstream, and an idempotent ndotfiles/ansible install.

**Date:** 2026-07-17
**Status:** Phase 5 (Implementation) — commit `c84491e` landed Epics 1/3/4 and the Epic 2 auth-header layer (config schema, `AuthMethodExt::apply`, `RoutingStrategy`/`HealthRegistry`/`Router::dispatch`, `governor`-based `AdmissionControl`), all unit-tested. Not yet built: Epic 2 Stories 2.2/2.3 (concrete `Provider` impls — `providers/mod.rs` only has the trait/error contract so far), the axum HTTP server / `AppState` referenced by Task 6.1.4, and the Story 6.2 feature-module port below. Epic 5 (Model Gateway upstream) is blocked on the concrete-provider work. Epics 6 (Story 6.1/6.3 rename+plist) and 7 (ndotfiles/ansible) are reported done in a prior pass and are out of scope for the current work item.
**Source spec:** `project_plans/consolette/requirements.md` (CD-1..CD-5 fixed)
**Research:** `project_plans/consolette/research/{model-gateway-auth,config-layering,weighted-router,rate-limiting}.md`

## ADRs

| ADR | Decision |
|---|---|
| ADR-001 | Config engine = `figment` (toml+env), sorted glob deep-merge, plain `Arc<Config>` for v1 (hot-reload + ArcSwap deferred) |
| ADR-002 | Model Gateway auth = `bearer` to the SBN Dev Agent local proxy now; the internal identity system integrates later via ADR-007's `exec` credential-helper plugin, not a core auth mode; no personal vendor key |
| ADR-003 | Routing = one `RoutingStrategy` trait (`fallback`\|`weighted`) + `HealthRegistry`; `Availability` predicate scoped to health/cooldown ONLY |
| ADR-004 | Rate limiting = `governor` direct limiter per upstream per dimension behind an `AdmissionControl` trait, invoked post-selection in the router loop |
| ADR-005 | Rename `claude-proxy-rs`→`consolette`; `com.consolette` launchd migration off `com.claude-proxy-rs` |
| ADR-006 | ndotfiles/ansible install block mirroring the `aimee` block + cfgcaddy config links |

## Requirement → Epic traceability

| FR | Epic |
|---|---|
| FR-1 Layered config.d | Epic 1 — Config foundation |
| FR-2 Upstream + pluggable auth | Epic 2 — Upstream + auth abstraction |
| FR-3 Routing (fallback\|weighted) | Epic 3 — Router strategy interface |
| FR-4 Per-upstream rate limiting | Epic 4 — Rate limiting |
| FR-5 Internal Model Gateway upstream | Epic 5 — Model Gateway upstream |
| FR-6 Rename → consolette | Epic 6 — Rename + launchd migration |
| FR-7 ndotfiles/ansible install | Epic 7 — Install automation |
| NFR-1..6 | Cross-cutting; asserted in per-story acceptance criteria + Epic 8 (validation hooks) |

All paths below are relative to `stapler-scripts/claude-proxy-rs/` (the crate root,
renamed to `consolette` in Epic 6) unless prefixed with `ndotfiles/`.

## Implementation context: scaffold vs. legacy source

**The `consolette` scaffold this plan is implemented against is a bare `clap`
stub** (`src/main.rs`: a `Cli`/`Command{Run,Mcp}` enum, no `AppState`, no axum
router, no `fallback.rs`) — it is NOT a checkout of the legacy
`stapler-scripts/claude-proxy-rs/` crate. Tasks below that reference existing
files/behavior (`AppState`, `fallback.rs`, `config.rs`, `handle_messages`,
`handle_openai_compat`, etc.) are describing the **legacy `claude-proxy-rs`
codebase** as the design/behavior source to port — a reference implementation,
not files already present in this repo. Every such task is **new
implementation in this scaffold**, guided by the referenced legacy behavior
and the ADRs above; it is not an in-place refactor of code that already
exists here.

**Core facade requirement:** Epics 1–4 build a shared `Core` facade —
encapsulating config load/validation, upstream+auth resolution, and router
dispatch behind one API — before `cli.rs` (the `Run` subcommand) and `mcp.rs`
(the `Mcp` subcommand) are split out of `main.rs` as thin transport adapters
over that facade in Epic 6 (Task 6.1.4, below). Concretely: the axum HTTP
handlers, the `rmcp` stdio handlers, and the `Run`/`Mcp` CLI subcommands all
call the same `Core` methods (`Core::dispatch`, `Core::health`, etc.) rather
than each re-implementing config/routing bootstrap.

  - **Task 1.0.1** Introduce `src/core.rs`: a `Core { config: Arc<Config>, router: Arc<Router> }` (fields grow as Epics 2–4 land) with an async `Core::bootstrap(config_dir) -> anyhow::Result<Core>` that runs load→validate→build-upstreams→build-router once. `main.rs`'s `run()` calls `Core::bootstrap` instead of `config::load` directly. Files: `src/core.rs`, `src/main.rs`.
  - **Task 6.1.4** Split `main.rs` into `cli.rs` (the axum HTTP server + `Run` subcommand, calling `Core::dispatch`) and `mcp.rs` (the `rmcp` stdio server + `Mcp` subcommand, calling the same `Core::dispatch`); `main.rs` shrinks to `Cli::parse()` + dispatch to one of the two, with no routing/config logic of its own. Files: `src/main.rs`, `src/cli.rs`, `src/mcp.rs`.

---

## Dependency Visualization

```
              ┌────────────────────────────┐
              │ Epic 1: Config foundation   │  (figment, schema, ArcSwap,
              │ FR-1                         │   validate_references, env allowlist)
              └──────────────┬──────────────┘
                             │ Config schema + Arc<Config> is the substrate
              ┌──────────────▼──────────────┐
              │ Epic 2: Upstream + auth      │  (Upstream, AuthMethod,
              │ FR-2                         │   openai provider, no-regression)
              └───────┬───────────────┬──────┘
                      │               │
        ┌─────────────▼───┐   ┌───────▼───────────┐
        │ Epic 3: Router   │   │ Epic 4: Rate limit │
        │ FR-3 (trait,     │◄──┤ FR-4 (governor,    │
        │ health-only      │   │ AdmissionControl   │
        │ Availability)    │   │ ::admit at dispatch)│
        └───────┬──────────┘   └────────┬──────────┘
                │  E4 integrates via a SINGLE seam:   │
                │  Router calls AdmissionControl::admit│
                │  AFTER strategy.select, before the   │
                │  provider call. Availability =       │
                │  health/cooldown ONLY.               │
                └───────────────┬────────────────────┘
                                │ routing + limiting compose
                 ┌──────────────▼──────────────┐
                 │ Epic 5: Model Gateway         │  (openai/anthropic upstream
                 │ FR-5                          │   via SBN Dev Agent, docs)
                 └──────────────┬──────────────┘
                                │ feature-complete behavior
                 ┌──────────────▼──────────────┐
                 │ Epic 6: Rename → consolette   │  (crate/bin/plist/Makefile,
                 │ FR-6                          │   launchd migration)
                 └──────────────┬──────────────┘
                                │ new binary name + com.consolette label
                 ┌──────────────▼──────────────┐
                 │ Epic 7: ndotfiles/ansible     │  (tasks.yml block, .plist.j2,
                 │ FR-7                          │   cfgcaddy links)
                 └──────────────┬──────────────┘
                                │
                 ┌──────────────▼──────────────┐
                 │ Epic 8: Validation & soak     │  (integration tests, metrics,
                 │ NFR-1..6                      │   zero-regression soak)
                 └─────────────────────────────┘

Critical path: E1 → E2 → E3 → E5 → E6 → E7.
E4 depends on E2 and integrates into E3's router dispatch loop through a SINGLE
mechanism — `AdmissionControl::admit()` called after `strategy.select()` and
before the provider call (NOT through the Availability predicate). `Availability`
is scoped to health/cooldown only. A shedding upstream is handled by the router's
`already_tried` re-select loop (rejection sampling — fine for 2–5 upstreams), so
weighted redistribution around a shed upstream falls out of the same loop that
handles a 429. E4 can proceed in parallel with E3 once the `AdmissionControl`
trait exists.
E6 (crate rename) MAY be pulled forward as a mechanical first commit; if so, all
later epics author modules under `consolette` directly. Sequenced here after the
functional work to keep the risky launchd cutover last.
```

---

# Phase A — Foundation

## Epic 1: Config foundation (figment layered config.d + env overlay)

**FR-1.** Replace `Config::from_env()` with a figment pipeline:
`defaults < conf.d/*.toml (lexical) < env allowlist`, wrapped in `ArcSwap`,
with `deny_unknown_fields` and a semantic `validate_references()` pass.

### Concrete TOML schema (the contract for the whole feature)

**`~/.config/consolette/conf.d/00-providers.toml`** — owns `[[upstreams]]`:

```toml
# Each [[upstreams]] = one named LLM endpoint. This array is owned by THIS file
# only (arrays replace on deep-merge; do not split [[upstreams]] across files).
# To ADD an upstream you edit this whole [[upstreams]] array in this owning file —
# a later conf.d file defining [[upstreams]] REPLACES this array, it does not append.

[[upstreams]]
name     = "anthropic"           # unique key referenced by routes + ratelimit
kind     = "anthropic"           # anthropic | bedrock | openai
base_url = "https://api.anthropic.com"
can_cooldown = true              # optional; default true (bedrock sets false)
[upstreams.auth]
type      = "bearer"             # bearer | apikey | internal
token_env = "CLAUDE_CODE_OAUTH_TOKEN"   # indirect secret ref (env var NAME)

[[upstreams]]
name     = "bedrock"
kind     = "bedrock"
can_cooldown = false             # preserves "Bedrock never cooled down"
[upstreams.auth]
type = "aws"                     # implicit AWS credential chain (SigV4)
[upstreams.options]              # kind-specific options (bedrock)
aws_profile = "Sandbox.AdministratorAccess"
aws_region  = "us-west-2"
max_retries = 3                  # was BEDROCK_MAX_RETRIES

[[upstreams]]
name     = "model-gateway"
kind     = "openai"              # OpenAI-compatible path
base_url = "http://localhost:9123/proxy/PROJECT_ID"   # SBN Dev Agent local proxy
[upstreams.auth]
type   = "bearer"
token  = "sk-dummy"              # dummy; real identity injected by the agent (ADR-002)
```

**`~/.config/consolette/conf.d/10-routing.toml`** — owns `[[routes]]`:

```toml
# First route whose match_model + match_endpoint globs match the request wins.
# `upstreams` is an inline array-of-tables: order matters for fallback; weight
# matters for weighted. One schema serves both strategies.

[[routes]]
match_model    = "*"             # glob over the request "model" field
match_endpoint = "*"             # "/v1/messages" | "/v1/chat/completions" | "*"
strategy       = "fallback"      # fallback | weighted
upstreams = [
  { name = "anthropic", weight = 1 },   # tried first
  { name = "bedrock",   weight = 1 },   # fallback
]

# Example weighted A/B split (commented out by default):
# [[routes]]
# match_model    = "claude-*"
# match_endpoint = "/v1/messages"
# strategy       = "weighted"
# upstreams = [
#   { name = "anthropic",     weight = 80 },
#   { name = "model-gateway", weight = 20 },
# ]
```

**`~/.config/consolette/conf.d/20-ratelimit.toml`** — table-of-tables (deep-merges):

```toml
[ratelimit.defaults]
on_breach    = "shed"            # shed | delay
max_delay_ms = 2000              # used only when on_breach = "delay"

[ratelimit.upstreams.anthropic]
rpm = 50
tpm = 100000
on_breach = "shed"

[ratelimit.upstreams.model-gateway]
rpm = 200
tpm = 400000
on_breach = "delay"
max_delay_ms = 3000
# upstreams absent here are unlimited; omitted rpm/tpm = that dimension unlimited
```

Env overlay allowlist (highest precedence, top-level scalars only):
`CONSOLETTE_PORT`, `CONSOLETTE_LOG`, `CONSOLETTE_REQUEST_TIMEOUT`,
`CONSOLETTE_COOLDOWN_SECONDS`, `CONSOLETTE_CONFIG_DIR`. Secret *values* are never
in env-overlay scalars — auth blocks name an env var, and that env var is read
directly by the auth resolver (Epic 2), not merged into the config tree.

### Story 1.0 — dashmap 5→6 tree-wide bump (pre-work, earliest)
- **As a** maintainer **I want** `dashmap` bumped to 6 across the whole crate first **so that** later routing/rate-limit work (which needs governor's `dashmap ^6.1`) builds on a compiling tree.
- **Acceptance:** direct dep `dashmap = "6"`; every existing call site compiles and tests pass; done BEFORE Epics 3/4 depend on it.
- **Files:** `Cargo.toml`, plus any `DashMap` call sites surfaced by the inventory.

  - **Task 1.0.1** Inventory all `DashMap` usages: `cache.rs`, `slots.rs`, `memory/`, `metrics/`, and the `mcp-proxy` binary (grep `DashMap`); note any 5→6 API differences. Files: (read-only inventory).
  - **Task 1.0.2** Bump `dashmap = "5"` → `"6"` in `Cargo.toml`; fix any call-site breakage; `cargo build && cargo test` clean. Files: `Cargo.toml`, inventoried call sites.

### Story 1.1 — figment dependency + module skeleton
- **As a** maintainer **I want** the config crate wired **so that** later stories build on it.
- **Acceptance:** `cargo build` succeeds with figment added; `src/config/` module tree compiles; no behavior change yet.
- **Files:** `Cargo.toml`, `src/config/mod.rs`, `src/main.rs`

  - **Task 1.1.1** Add deps to `Cargo.toml`: `figment = { version = "0.10", default-features = false, features = ["toml", "env"] }`, `arc-swap = "1.9"`. Keep existing `toml`, `glob`, `serde`. Files: `Cargo.toml`.
  - **Task 1.1.2** Convert `config.rs` into a `config/` module dir: create `src/config/mod.rs` re-exporting current `Config` unchanged; update `mod config;` in `src/main.rs` (no-op behaviorally). Files: `src/config/mod.rs`, `src/config.rs` (delete after move), `src/main.rs`.

### Story 1.2 — Config schema structs (serde, deny_unknown_fields)
- **As a** maintainer **I want** typed structs for the TOML schema **so that** config is parsed and validated.
- **Acceptance:** structs deserialize the three example TOML files; unknown keys error; a Python `tomllib.load` of each example succeeds (parity check).
- **Files:** `src/config/schema.rs`, `src/config/mod.rs`

  - **Task 1.2.1** Define `Config` (top-level: `port`, `log`, `request_timeout`, `cooldown_seconds`, `config_dir`, plus feature toggles `stapler_compress`/`compress_floor_bytes`/`cache_aligner`/`verbosity_level`/`memory_max_entries`), `#[serde(deny_unknown_fields)]`, `impl Default`. Files: `src/config/schema.rs`.
  - **Task 1.2.2** Define `UpstreamCfg { name, kind: UpstreamKind, base_url: Option<String>, can_cooldown: Option<bool>, auth: AuthCfg, options: Option<toml::Value> }` and `RouteCfg { match_model, match_endpoint, strategy: StrategyKind, upstreams: Vec<UpstreamRefCfg{name,weight:Option<u32>}> }`; enums `UpstreamKind{Anthropic,Bedrock,Openai}`, `StrategyKind{Fallback,Weighted}`, all `deny_unknown_fields`. Files: `src/config/schema.rs`.
  - **Task 1.2.3** Add `RateLimitConfig { defaults, upstreams: HashMap<String,UpstreamLimit> }` per rate-limiting research §4. Files: `src/config/schema.rs`.
  - **Task 1.2.4** Add a `#[cfg(test)]` parity test that loads `references/conf.d/*.toml` fixtures via `toml::from_str` into the structs. Files: `src/config/schema.rs`, `references/conf.d/{00-providers,10-routing,20-ratelimit}.toml`.

### Story 1.3 — figment loader with lexical deep-merge + env allowlist
- **As an** operator **I want** conf.d loaded in filename order with env on top **so that** precedence is deterministic.
- **Acceptance:** given two conf.d files, the later file's scalars/arrays win, tables union; `CONSOLETTE_PORT=X` overrides file value; unlisted env vars are ignored.
- **Files:** `src/config/load.rs`, `src/config/mod.rs`

  - **Task 1.3.1** Implement `conf_d_files(dir) -> Vec<PathBuf>` using `glob::glob` + explicit `.sort()` (config-layering research Q1 gotcha). Files: `src/config/load.rs`.
  - **Task 1.3.2** Implement `load(dir) -> anyhow::Result<Config>`: `Figment::new().merge(Serialized::defaults(Config::default()))`, loop `.merge(Toml::file(f))` over sorted files, then `.merge(Env::prefixed("CONSOLETTE_").split("__").only(&[...allowlist...]))`, `.extract()`. Files: `src/config/load.rs`.
  - **Task 1.3.3** Resolve `config_dir`: default `~/.config/consolette/conf.d`, overridable via `CONSOLETTE_CONFIG_DIR`; empty/missing dir yields working defaults (FR-1.3). Files: `src/config/load.rs`.

### Story 1.4 — Fail-fast validation naming file + key
- **As an** operator **I want** clear startup errors **so that** a typo names the offending file/key.
- **Acceptance:** bad TOML, unknown key, unknown route→upstream reference, and invalid auth type each abort startup with a message naming file+key; process exits non-zero.
- **Files:** `src/config/validate.rs`, `src/config/mod.rs`, `src/main.rs`

  - **Task 1.4.1** Map `figment::Error` into an `anyhow` error that surfaces the figment metadata source (file) + key path (config-layering research Q4). Files: `src/config/load.rs`.
  - **Task 1.4.2** Implement `Config::validate_references()`: every `route.upstreams[].name` and every `ratelimit.upstreams` key resolves to a defined upstream; every `auth.type` is a known variant (`bearer`/`apikey`/`exec` — no `internal`); each `route.strategy` consistent (weighted needs ≥1 positive weight). Return `thiserror` errors naming the route/upstream. Files: `src/config/validate.rs`.
  - **Task 1.4.3** Call `load()?` then `validate_references()?` in `async_main`; on error, log + `std::process::exit(1)` before binding the socket. Files: `src/main.rs`.

### Story 1.5 — Immutable `Arc<Config>` + complete env back-compat shim
- **As a** maintainer **I want** a plain `Arc<Config>` and EVERY legacy env-var name honored **so that** v1 is simple and today's plist still works with zero regression.
- **Acceptance:**
  - `AppState` holds a plain immutable `Arc<Config>` (v1). Hot-reload and `ArcSwap`
    are explicitly deferred (ADR-001); handlers read `state.config` directly.
  - EVERY env var the current `Config::from_env()` reads still takes effect via the
    shim or `Config::default()`: `PROXY_PORT, COOLDOWN_SECONDS, REQUEST_TIMEOUT,
    BEDROCK_MAX_RETRIES, STAPLER_COMPRESS, COMPRESS_FLOOR_BYTES, CACHE_ALIGNER,
    VERBOSITY_LEVEL, MEMORY_MAX_ENTRIES, AWS_PROFILE, AWS_REGION,
    CLAUDE_CODE_OAUTH_TOKEN`.
  - A one-time deprecation `warn!` names each legacy var seen and its replacement.
  - Precedence is explicit and tested (see Task 1.5.3).
- **Files:** `src/config/load.rs`, `src/config/mod.rs`, `src/main.rs`

  - **Task 1.5.1** Store a plain `Arc<Config>` in `AppState` (no `ArcSwap` in v1 — dropped per ADR-001). Handlers read `state.config` directly; the load path returns `Config` once at startup. Files: `src/main.rs`, `src/config/mod.rs`.
  - **Task 1.5.2** Back-compat shim in `load()`: before the figment `Env` merge, build a legacy→new mapping and, for each legacy var present whose new equivalent is absent, feed its value into the figment layer (top-level scalars → the `CONSOLETTE_`-prefixed key; secrets/AWS → the upstream auth/options resolution described below), emitting one `warn!` per var. Mapping:
    - `PROXY_PORT`→`port`, `COOLDOWN_SECONDS`→`cooldown_seconds`, `REQUEST_TIMEOUT`→`request_timeout`, `BEDROCK_MAX_RETRIES`→bedrock upstream `options.max_retries`, `STAPLER_COMPRESS`→`stapler_compress`, `COMPRESS_FLOOR_BYTES`→`compress_floor_bytes`, `CACHE_ALIGNER`→`cache_aligner`, `VERBOSITY_LEVEL`→`verbosity_level`, `MEMORY_MAX_ENTRIES`→`memory_max_entries`.
    - `CLAUDE_CODE_OAUTH_TOKEN`: remains the default `token_env` for the implicit `anthropic` upstream's `bearer` auth (read by the Epic 2 auth resolver, not merged into the config tree — keeps the secret out of `Config`).
    - `AWS_PROFILE`/`AWS_REGION`: feed the implicit `bedrock` upstream's `options.aws_profile`/`options.aws_region`. **Precedence (explicit): conf.d `options.aws_region` (if set) > `AWS_REGION` env > built-in default `us-west-2`.** Same ordering for `aws_profile`. Document that a conf.d value always wins over the legacy env.
    Files: `src/config/load.rs`, `src/upstream/mod.rs`.
  - **Task 1.5.3** Add a `#[cfg(test)]` matrix test asserting each of the 12 legacy env names, set in isolation with an empty conf.d, produces the expected field on the built `Config`/upstreams; and one test asserting conf.d `options.aws_region` overrides `AWS_REGION`. Files: `tests/config.rs` or `src/config/load.rs`.

---

## Epic 2: Upstream + pluggable auth abstraction

**FR-2.** Introduce an `Upstream` model with pluggable `AuthMethod`
(`bearer`|`apikey`|`exec`), add an `openai`-kind provider, and re-express
today's Anthropic(OAuth)+Bedrock as upstreams with **no behavioral regression**.
There is no core-native `internal` auth variant — ADR-007 supersedes it with the
generic `exec` credential-helper protocol, delivered as a plugin (Epic 7) rather
than core auth code.

### Story 2.1 — Upstream model + shared `AuthMethod` header-injector
- **As a** maintainer **I want** a runtime `Upstream` and a shared bearer/apikey header-injector **so that** routing operates over uniform upstreams without duplicating auth code.
- **Acceptance:**
  - `Upstream { name, kind, provider: Arc<dyn Provider>, can_cooldown }` built from config.
  - **Auth layering is explicitly split:** `AuthMethod` is a generic **header
    injector** for `bearer`/`apikey`, shared by the reqwest-based providers
    (`OpenAiProvider` and any generic HTTP upstream). **Anthropic OAuth-vs-`sk-ant-api-*`
    stays provider-native inside `AnthropicProvider`; Bedrock SigV4 stays
    provider-native inside `BedrockProvider`** — neither is routed through
    `AuthMethod::resolve()`.
  - Secret values are resolved behind a mockable seam and are redacted everywhere
    they could surface (NFR-6).
- **Files:** `src/upstream/mod.rs`, `src/upstream/auth.rs`

  - **Task 2.1.1** Define `AuthMethod` enum used by HTTP providers: `Bearer{token: SecretRef}`, `ApiKey{header: String, key: SecretRef}`, `Exec{command, args, cache_ttl_secs, timeout_secs}`. (Anthropic/Bedrock use `type = anthropic-native`/`aws` markers that the factory maps to provider-native auth, NOT `AuthMethod`.) Implement `apply(&self, &mut HeaderMap, &dyn SecretResolver)`: `bearer`→`Authorization: Bearer <v>`, `apikey`→`<header>: <v>` (default `x-api-key`), `exec`→ run the configured helper via the ADR-007 protocol (Task 2.5.x) and apply its returned headers. There is no `Internal` variant — see ADR-007. Files: `src/upstream/auth.rs`.
  - **Task 2.1.2** Define `SecretRef = Inline(String) | Env(String) | Keychain(String)` and a `SecretResolver` trait (`fn resolve(&self, &SecretRef) -> Result<String>`) with a default impl reading env / `security find-generic-password`. Put resolution behind the trait so keychain+env are mockable in tests (Task 8.1). Implement `Debug`/`Display` for `SecretRef` and `AuthMethod` as `"<redacted>"`; never log resolved values. **Warn (or reject when `--strict`) on a non-`sk-dummy` `SecretRef::Inline`** — inline plaintext secrets are discouraged (NFR-2/6); `sk-dummy` is the documented exception for the Model Gateway. Files: `src/upstream/auth.rs`.
  - **Task 2.1.3** ~~`internal` variant~~ — dropped. Superseded by ADR-007: my employer's internal identity system integrates purely as an `exec` plugin (Task 2.5.x + the ndotfiles `internal-identity` plugin bundle in Epic 7), never as a core `AuthMethod` variant. (FR-2.2, CD-2)

### Story 2.5 — ADR-007 plugin discovery + exec credential-helper protocol
- **As a** maintainer **I want** plugin bundles and the `exec` auth type **so that** identity systems that can't ship in core (e.g. my employer's internal identity CLI) integrate without forking consolette.
- **Acceptance:** `plugins.d/*/plugin.toml` bundles are discovered and their `conf.d/*.toml` fragments merge after core fragments (plugin name lexical order) under the same `deny_unknown_fields` validation; `auth = { type = "exec", ... }` spawns the configured helper, sends `{upstream, method, url}` as one JSON line on stdin, and applies the JSON `{headers, cache_ttl_secs}` response; any failure (spawn error, non-zero exit, timeout, unparseable stdout) is treated as upstream-unavailable with no separate routing path and helper stdout/stderr is never logged (ADR-007 §4/§6).
- **Files:** `src/config/plugins.rs`, `src/auth/exec.rs`

  - **Task 2.5.1** Discover plugin bundles under `${XDG_CONFIG_HOME:-~/.config}/consolette/plugins.d/*/` (or `CONSOLETTE_PLUGIN_PATH`), parse each `plugin.toml` manifest, and merge each plugin's `conf.d/*.toml` fragments into the layered figment source after core conf.d, in plugin-name lexical order. Files: `src/config/plugins.rs`.
  - **Task 2.5.2** Implement `resolve_command()` to check the plugin's `bin/<helper>` directory ahead of `PATH` for exec-auth commands, so a plugin bundle can ship its own helper binary without requiring a system-wide install. Files: `src/auth/exec.rs`.
  - **Task 2.5.3** **Permission check (TOCTOU-relevant, ADR-007 §5):** before executing a resolved helper binary, verify it is owned by the current EUID (`libc::geteuid()` vs `fs::metadata().uid()`) and not world-writable (`mode & 0o002`); reject with a clear `AuthError::Exec` otherwise. Document the residual TOCTOU window (check-then-exec is not atomic on POSIX without re-checking the fd post-open) as an accepted risk for a single-user local Mac daemon, not a multi-tenant server. Files: `src/auth/exec.rs`.
  - **Task 2.5.4** Bound the per-`(upstream, command, args)` credential cache with `cache_ttl_secs` (helper-supplied `cache_ttl_secs` in its response overrides the config default when present); expired entries are re-run, never served stale. Add a fake-exec-helper test exercising cache hit/expiry (Task 8.1.x). Files: `src/auth/exec.rs`.
  - **Task 2.5.5 (Epic 7)** Ship the `internal-identity` plugin bundle in ndotfiles (`plugin.toml` + `conf.d/*.toml` + `bin/<helper>` wrapping the internal identity CLI) per ADR-007's planned structure — this is the concrete consumer of Tasks 2.5.1-2.5.4. Files: `ndotfiles` (out of this repo).
  - **Task 2.5.6 (Epic 8)** Add a plugin-merge precedence test (core conf.d < plugin conf.d, plugin order is lexical by bundle name) and the fake-exec-helper integration test referenced in Task 2.5.4. Files: `src/config/tests.rs`, `src/auth/exec/tests.rs`.

### Story 2.2 — OpenAI-kind provider (+ `Provider::send` body contract)
- **As a** user **I want** a `kind = openai` upstream **so that** any OpenAI-compatible base_url (incl. Model Gateway) works.
- **Body contract (documented, applies to all providers):** the router ALWAYS passes
  the canonical **Anthropic Messages** JSON body to `Provider::send` (this is already
  true today — the OpenAI HTTP handler translates to Anthropic before dispatch). Each
  provider translates to/from its own wire format **internally**: `AnthropicProvider`
  and `BedrockProvider` are already Anthropic-native; `OpenAiProvider` translates
  Anthropic→OpenAI on the way out and OpenAI→Anthropic on the way back (buffered) or
  Anthropic-native passthrough when configured. The OpenAI↔Anthropic translation
  helpers in `providers/mod.rs` move to / are called from inside `OpenAiProvider`.
- **Acceptance:** `OpenAiProvider` implements `Provider`, receives an Anthropic-format
  body, forwards to `{base_url}/v1/chat/completions` (or Anthropic-native `/v1/messages`
  passthrough), injects auth via the shared `AuthMethod`, classifies errors via the
  existing `ProviderError` arms, and returns an Anthropic-format response; streaming
  errors surface before first byte (weighted-router research §6). An **OpenAI
  round-trip test** (Anthropic-in → OpenAI wire → Anthropic-out) asserts semantic
  equivalence.
- **Files:** `src/providers/openai.rs`, `src/providers/mod.rs`

  - **Task 2.2.1** Implement `OpenAiProvider::new(name, base_url, auth, passthrough, request_timeout)` + `Provider` impl (buffered + streaming) mirroring `anthropic.rs` request/stream structure; reuse `reqwest` client patterns. Files: `src/providers/openai.rs`.
  - **Task 2.2.2** Move the Anthropic↔OpenAI translation into `OpenAiProvider` (call the `providers/mod.rs` helpers); map upstream HTTP status → `ProviderError` (429/529→RateLimited, 4xx→Validation, 401→Auth, 5xx/timeout→Upstream/Timeout) reusing the classification contract. Add the round-trip test. Files: `src/providers/openai.rs`, `src/providers/mod.rs`.
  - **Task 2.2.3** Add an `anthropic_passthrough: bool` (or path derived from `base_url`) so an `openai`-kind upstream pointed at `/proxy/{PROJECT}` carries native `/v1/messages` for Claude Code with NO translation (FR-5.2). Files: `src/providers/openai.rs`.

### Story 2.3 — Upstream factory (config → providers)
- **As a** maintainer **I want** a factory that builds all upstreams from config **so that** startup wiring is data-driven.
- **Acceptance:** `build_upstreams(&Config) -> Result<Vec<Arc<Upstream>>>` constructs Anthropic/Bedrock/OpenAI providers by `kind`; missing `base_url` for anthropic/openai errors at startup; bedrock reads `options.aws_profile/region/max_retries`.
- **Files:** `src/upstream/mod.rs`

  - **Task 2.3.1** Match on `UpstreamKind`: build `AnthropicProvider` (from base_url+auth), `BedrockProvider` (from options), `OpenAiProvider`. Files: `src/upstream/mod.rs`.
  - **Task 2.3.2** Adapt `AnthropicProvider::new` / `BedrockProvider::new` to accept per-upstream fields instead of global `Config` (keep defaults identical). Files: `src/providers/anthropic.rs`, `src/providers/bedrock.rs`.

### Story 2.4 — No-regression default config
- **As** Tyler **I want** the default (empty conf.d) to reproduce today's Anthropic→Bedrock behavior **so that** daily usage is unaffected (FR-2.4, Success Metric 6).
- **Acceptance:** with no conf.d files, `build_upstreams`+router yields an implicit `anthropic`(bearer, OAuth env)→`bedrock`(aws) fallback route identical to current dispatch; existing `fallback.rs` unit tests (or their ported equivalents) pass.
- **Files:** `src/config/load.rs`, `src/upstream/mod.rs`, `references/conf.d/00-providers.toml`

  - **Task 2.4.1** Encode the implicit default upstreams+route in `Config::default()` (used by `Serialized::defaults`) so an empty conf.d is a working proxy. Files: `src/config/schema.rs`.
  - **Task 2.4.2** Ship `references/conf.d/*.toml` documented examples that reproduce the default explicitly (for ndotfiles linking in Epic 7). Files: `references/conf.d/*.toml`.

---

# Phase B — Routing & rate limiting

## Epic 3: Router strategy interface

**FR-3.** Refactor `fallback.rs` into a strategy-agnostic `Router`: pure
`RoutingStrategy` selection, a factored-out `HealthRegistry`, a composable
`Availability` predicate, and a dispatch loop that reuses the existing error arms.
Grounded in weighted-router research §1–§6.

### Story 3.1 — RoutingStrategy trait + strategies
- **As a** maintainer **I want** pluggable pure selection **so that** fallback and weighted share one seam.
- **Acceptance:** `RoutingStrategy::select(&self, healthy: &[UpstreamRef]) -> Option<UpstreamRef>`; `FallbackStrategy` = `healthy.first()`; `WeightedStrategy` uses `WeightedIndex`; unit tests cover redistribution when a candidate is absent.
- **Files:** `src/routing/strategy.rs`, `src/routing/mod.rs`, `Cargo.toml`

  - **Task 3.1.1** Add `rand = "0.8"` to `Cargo.toml` (weighted-router research §3; pin 0.8 for `rand::distributions::WeightedIndex` + `thread_rng`). Files: `Cargo.toml`.
  - **Task 3.1.2** Define `UpstreamRef { idx: usize, name, weight: u32 }` and the `RoutingStrategy` trait. Files: `src/routing/strategy.rs`.
  - **Task 3.1.3** Implement `FallbackStrategy` and `WeightedStrategy` (`.max(1)` weight guard, `WeightedIndex::new().ok()?`); make weighted generic-over-RNG or inject seed for deterministic tests. Files: `src/routing/strategy.rs`.

### Story 3.2 — HealthRegistry (cooldown, per upstream)
- **As a** maintainer **I want** per-upstream cooldown state **so that** 429 cooldown applies to both strategies.
- **Acceptance:** `HealthRegistry` (DashMap keyed by idx) reuses the ADR-006 `ProviderState`/TOCTOU-safe check-and-clear; `is_healthy`/`trip`/`try_expire`/`healthy_subset`/`remaining_secs`; per-upstream `can_cooldown=false` (Bedrock) never trips.
- **Files:** `src/routing/health.rs`, `src/routing/mod.rs`, `Cargo.toml`

  - **Task 3.2.1** Move `ProviderState` enum out of `fallback.rs` into `health.rs`; keep the atomic expiry transition (weighted-router research §4). Files: `src/routing/health.rs`, `src/fallback.rs`.
  - **Task 3.2.2** Implement `HealthRegistry` over `DashMap<usize, ProviderState>`; hard rule: never hold a DashMap guard across `.await`. Files: `src/routing/health.rs`.
  - **Task 3.2.3** (dashmap 6 already in place from Story 1.0.) Confirm `HealthRegistry`'s `DashMap<usize, ProviderState>` compiles against dashmap 6. Files: `src/routing/health.rs`.

### Story 3.3 — Availability predicate (health/cooldown ONLY)
- **As a** maintainer **I want** a health-availability predicate **so that** the pre-selection candidate filter excludes cooled-down upstreams without the strategy knowing.
- **Scope (explicit):** `Availability` gates on **health/cooldown ONLY**. Rate
  limiting is NOT an availability source — it integrates post-selection via
  `AdmissionControl::admit()` in Story 4.3 (governor commits state on a successful
  check, so it must be called at most once per attempt, after a candidate is
  chosen). A shed upstream is handled by the dispatch loop's `already_tried`
  re-select, not by this predicate.
- **Acceptance:** `Availability::is_available(idx) -> bool`; `HealthRegistry` impls it; the router's pre-selection filter uses it; a health-cooldown test excludes an upstream from selection.
- **Files:** `src/routing/availability.rs`, `src/routing/mod.rs`

  - **Task 3.3.1** Define `Availability` trait (health-only); impl for `HealthRegistry`. The trait is a thin seam kept for testability/future health sources — NOT for rate limiting. Files: `src/routing/availability.rs`.
  - **Task 3.3.2** Router computes `candidates = all.filter(|u| health.is_available(u.idx))` before calling `strategy.select`. Files: `src/routing/router.rs`.

### Story 3.4 — Router dispatch loop (reuse error arms + streaming discipline)
- **As a** user **I want** the router to try candidates until one opens a body **so that** failover is correct for streaming and non-streaming.
- **Acceptance:**
  - dispatch loop shrinks the candidate set per attempt (`already_tried`), reuses the existing `is_validation`/`is_auth`/`is_rate_limited`/transient arms verbatim, trips `HealthRegistry` on 429, keeps Bedrock same-upstream backoff inside the provider, and hands the stream out only after a body is open (weighted-router research §6). All-unhealthy → 503 (FR-3.6).
  - **The OpenAI-compatible path routes too:** `/chat/completions` and
    `/v1/chat/completions` reach `Arc<Router>` (they translate to Anthropic then call
    the same dispatch), so weighted/rate-limit/model-gateway routing applies to the
    OpenAI path (NFR-1, Success Metric 1). Verified by an integration test hitting
    `/v1/chat/completions` and asserting the selected upstream via metrics.
  - `handle_dry_run` (local compression preview only — no dispatch) and `/v1/models`
    (static list) are confirmed to NOT need routing.
- **Files:** `src/routing/router.rs`, `src/routing/mod.rs`, `src/main.rs`

  - **Task 3.4.1** Implement `Router { upstreams: Vec<Arc<Upstream>>, routes, health: Arc<HealthRegistry>, ratelimit: Arc<dyn AdmissionControl>, strategy_per_route }` and `route_for(model, endpoint)` (first matching glob route). Files: `src/routing/router.rs`.
  - **Task 3.4.2** Implement `dispatch(body, headers, stream, request_id)`: pick route → filter by `health.is_available` → loop {`strategy.select` → `ratelimit.admit` (Story 4.3) → provider call}, porting the `fallback.rs` match arms; preserve the pre-first-byte failover invariant. Files: `src/routing/router.rs`.
  - **Task 3.4.3** Replace `AppState.fallback`/`fallback_state` with `Arc<Router>` (plain `Arc`, no ArcSwap — ADR-001). Migrate ALL dispatch call sites: `handle_messages`, `handle_count_tokens`, and (transitively, since `handle_openai_compat` calls `handle_messages`) `/chat/completions` + `/v1/chat/completions` → `state.router.dispatch(...)`. Update `/metrics` cooldown block to iterate `health.remaining_secs()` per upstream. Files: `src/main.rs`.
  - **Task 3.4.4** Add an explicit integration assertion that the OpenAI path (`/v1/chat/completions`) flows through the router (Success Metric 1 / NFR-1). Files: `src/main.rs`, `tests/routing.rs`.
  - **Task 3.4.5** Generalize per-upstream REQUEST counters: replace hardcoded `requests_anthropic`/`requests_bedrock` with a `requests: DashMap<String, AtomicU64>` keyed by upstream name (mirrors the `rate_limits` map), incremented on each successful dispatch. Needed to verify weighted split by request count (Success Metric 2). Files: `src/metrics/counters.rs`, `src/routing/router.rs`, `src/main.rs`, `src/dashboard.rs`.
  - **Task 3.4.6** Port `fallback.rs` unit tests to `router.rs` (normal→first, 429→cooldown+next, 4xx→no failover, cooldown skips, weighted redistribution). Files: `src/routing/router.rs`.

## Epic 4: Per-upstream rate limiting

**FR-4.** `governor` direct limiter per upstream per dimension behind an
`AdmissionControl` trait, `shed`/`delay`, metrics map. Grounded in rate-limiting research.

### Story 4.1 — governor deps + limiter type
- **As a** maintainer **I want** the limiter primitive **so that** RPM/TPM can be enforced.
- **Acceptance:** `UpstreamLimiter` with two optional `Direct` limiters (rpm cost 1, tpm cost via `check_n`); `Breach::{Shed,Delay{max}}`; `Admit::{Allowed,Shed,Delayed(Duration)}`.
- **Files:** `Cargo.toml`, `src/ratelimit/limiter.rs`, `src/ratelimit/mod.rs`

  - **Task 4.1.1** Add `governor = "0.10"`; `dashmap` is already at `6` (Story 1.0); reuse existing `tiktoken-rs = "0.5"`. Files: `Cargo.toml`.
  - **Task 4.1.2** Implement `UpstreamLimiter::admit(est_tokens)` (shed + bounded-delay paths) per rate-limiting research §2, incl. `InsufficientCapacity`→shed and the TPM-first ordering caveat. Make `UpstreamLimiter` **generic over `governor::clock::Clock`** so tests inject `FakeRelativeClock` for deterministic RPM/TPM assertions (default `DefaultClock`). Files: `src/ratelimit/limiter.rs`.

### Story 4.2 — RateLimiters registry + AdmissionControl trait
- **As a** maintainer **I want** a per-upstream registry behind a trait **so that** the router depends on an interface, not governor.
- **Acceptance:** `#[async_trait] AdmissionControl::admit(&self, upstream, est_tokens) -> Admit`; `RateLimiters` builds `DashMap<String, UpstreamLimiter>` from `RateLimitConfig` (defaults inheritance); upstreams absent from config are unlimited.
- **Files:** `src/ratelimit/mod.rs`

  - **Task 4.2.1** Build limiters from config (`Quota::per_minute`, burst=rate for tpm). Files: `src/ratelimit/mod.rs`.
  - **Task 4.2.2** Define + impl `AdmissionControl`. Files: `src/ratelimit/mod.rs`.

### Story 4.3 — Wire rate limiting into the router loop (post-selection, single seam)
- **As a** user **I want** a rate-limited upstream to be skipped/redistributed **so that** limits compose with routing (FR-4.3).
- **Mechanism (single, committed):** `AdmissionControl::admit()` is called **after
  `strategy.select()` and before the provider call** — the ONLY rate-limit
  integration point (governor commits state on a successful check, so it must run at
  most once per attempt, on the chosen candidate). It is NOT an `Availability`
  source. On `Shed`, the router adds the candidate to `already_tried` and re-selects
  from the remaining healthy pool — this is **rejection sampling** (fine for 2–5
  upstreams) and is exactly how weighted redistribution around a shed upstream is
  achieved (same loop that handles a 429).
- **Accepted trade-off (documented):** because `admit` can charge one dimension then
  the request may fail over, a shed/failover can **double-charge TPM/RPM** on the
  first (rejected) candidate. Accepted for a single-tenant personal proxy; noted in
  ADR-004.
- **Acceptance:** router estimates tokens once per request **only when at least one
  selectable upstream has a TPM limiter** (otherwise skip the tiktoken cost); calls
  `admit` post-selection; `Shed` → `already_tried` + re-select; `Delayed` → dispatch
  after wait; per-upstream independence verified with `FakeRelativeClock` (FR-4.2).
- **Files:** `src/routing/router.rs`, `src/ratelimit/mod.rs`

  - **Task 4.3.1** Add a token-estimation helper reusing the tiktoken counter used by compression/`count_tokens`; **gate it** so estimation runs only when a TPM limiter exists for a candidate upstream (avoid paying tiktoken cost when no TPM limit is configured). Files: `src/routing/router.rs`, `src/ratelimit/mod.rs`.
  - **Task 4.3.2** In the dispatch loop, after `strategy.select`, call `AdmissionControl::admit(name, est_tokens)`; on `Shed`, add idx to `already_tried` and re-select (rejection sampling); on `Delayed(d)`, dispatch after the wait; on `Allowed`, dispatch. Do NOT set a `HealthRegistry` cooldown from a local shed (keep the two mechanisms separate — a proactive local limit is a short refill window, not a 300s 429 cooldown). Files: `src/routing/router.rs`.

### Story 4.4 — Rate-limit metrics (`/metrics` + `/dashboard`)
- **As** Tyler **I want** rate-limit decisions visible **so that** I can see shed/delay/tokens (FR-4.5).
- **Acceptance:** `/metrics` gains a `"ratelimit"` map keyed by upstream (allowed/shed/delayed/tokens_charged/avg_delay_ms/limits) per rate-limiting research §6; dashboard renders it.
- **Files:** `src/metrics/counters.rs`, `src/ratelimit/mod.rs`, `src/dashboard.rs`, `src/main.rs`

  - **Task 4.4.1** Add `rate_limits: DashMap<String, UpstreamRateMetrics>` to metrics; increment in the admit path. Files: `src/metrics/counters.rs`, `src/ratelimit/mod.rs`.
  - **Task 4.4.2** Expose under `"ratelimit"` in `to_metrics_json` and render in dashboard. Files: `src/main.rs`, `src/dashboard.rs`.
  - **Task 4.4.3** Secret redaction audit for every surface that renders config/upstreams: `/metrics`, `/dashboard` (`dashboard.rs`), `/health`, and startup logs must show upstream `name`/`kind`/`base_url` but NEVER resolved tokens/keys (render `SecretRef` as `env:NAME`/`keychain:ITEM`/`<redacted>`). Add a test that serializes an upstream with a secret and asserts the secret value is absent (NFR-6). Files: `src/dashboard.rs`, `src/main.rs`, `src/upstream/auth.rs`.

---

# Phase C — Integration & delivery

## Epic 5: Internal Model Gateway upstream

**FR-5.** Ship a working, documented Model Gateway upstream usable from Tyler's
Mac via the SBN Dev Agent local proxy with a dummy bearer (ADR-002); document
prereqs and the ADR-007 `exec` credential-helper plugin follow-up (the internal
identity system integrates as a plugin, not a core auth mode — see Story 2.5).

### Story 5.1 — Model Gateway example conf.d entry
- **As** Tyler **I want** a ready example **so that** I can point a route at the gateway.
- **Acceptance:** `references/conf.d/00-providers.toml` includes a `model-gateway` upstream (`kind=openai`, `base_url=http://localhost:9123/proxy/{PROJECT_ID}`, `auth=bearer token="sk-dummy"`) and a commented weighted route; a documented Anthropic-native variant (`base_url=.../proxy/{PROJECT_ID}`, passthrough `/v1/messages`) is included.
- **Files:** `references/conf.d/00-providers.toml`, `references/conf.d/10-routing.toml`

  - **Task 5.1.1** Add the OpenAI-path gateway upstream + example. Files: `references/conf.d/00-providers.toml`.
  - **Task 5.1.2** Add the Anthropic-native passthrough example (uses Story 2.2.3 passthrough) for Claude Code. Files: `references/conf.d/00-providers.toml`, `references/conf.d/10-routing.toml`.
  - **Task 5.1.3** Optional startup reachability probe for gateway (`kind=openai` local-agent) upstreams: a best-effort TCP/HTTP check that emits an **actionable** startup `warn!` distinguishing **agent-down** (`:9123` connection refused) vs **401** (in Gandalf policy? see ADR-002) vs **404** (wrong/absent project). Non-fatal — the proxy still starts and the router falls through. Files: `src/upstream/mod.rs`.

### Story 5.2 — Model Gateway documentation
- **As** Tyler **I want** the base URLs, ports, and prereqs documented **so that** setup is reproducible (FR-5.3).
- **Acceptance:** `references/model-gateway.md` documents the SBN Dev Agent path (`:9123`), port variance (`:9123/:2002/:7002/:7004` — never hardcode), the VPN + `go/modelgateway` project + Gandalf policy prereqs, the `sk-dummy` bearer semantics, and the ADR-007 `exec` plugin follow-up (shelling to the internal identity CLI: `internal-identity curl -a copilotdppython ... :7004` via a `plugins.d` helper, not a core `AuthMethod`).
- **Files:** `references/model-gateway.md`

  - **Task 5.2.1** Write the doc from `research/model-gateway-auth.md` (Options A/B/C, prereqs, risks). Files: `references/model-gateway.md`.

### Story 5.3 — Manual gateway smoke verification
- **As** Tyler **I want** a documented smoke test **so that** I can confirm the gateway upstream works end-to-end.
- **FR-5 end-to-end is MANUAL/gated, NOT CI- or soak-automatable:** it depends on VPN,
  a running SBN Dev Agent (`:9123`), a `go/modelgateway` project, and Gandalf
  membership — none available in CI or an unattended soak. CI covers only the
  `OpenAiProvider` unit/round-trip tests against a mock; the live gateway is verified
  by this manual checklist.
- **Acceptance:** documented `curl` against Consolette that routes to `model-gateway` returns a completion when the SBN Dev Agent is running; a clear error (per Task 5.1.3 probe) when it is not (agent down / 401 / 404).
- **Files:** `references/model-gateway.md`

  - **Task 5.3.1** Add smoke-test steps + expected failures (agent down, missing Gandalf → 401, wrong project → 404). Files: `references/model-gateway.md`.

## Epic 6: Rename claude-proxy-rs → consolette + launchd migration

**FR-6.** Rename crate/binary, keep `mcp-proxy`, new `com.consolette` plist, migrate
off `com.claude-proxy-rs` without dropping daily usage, preserve all features.

### Story 6.1 — Crate + binary rename
- **As a** maintainer **I want** the crate/binary named `consolette` **so that** naming (CD-5) is satisfied.
- **Acceptance:** `cargo build --release` produces `consolette` + `mcp-proxy`; crate docs/log tags updated; `cargo clippy --deny warnings` clean (NFR-5).
- **Files:** `Cargo.toml`, `src/main.rs`, `Makefile`

  - **Task 6.1.1** `Cargo.toml`: `package.name = "consolette"`, `[[bin]] name = "consolette" path = "src/main.rs"`, keep `[[bin]] mcp-proxy`. Files: `Cargo.toml`.
  - **Task 6.1.2** Update crate-level doc comment, `handle_root` `"service"` string, and `"claude-proxy-rs starting"` log to `consolette`. Files: `src/main.rs`.
  - **Task 6.1.3** Rename log paths `/tmp/claude-proxy-rs.*` → `/tmp/consolette.*` in `init_logging`; keep the mcp metrics cache path or migrate `~/.cache/claude-proxy` → `~/.cache/consolette` (note both in migration doc). Files: `src/main.rs`.

### Story 6.2 — Feature-module port onto the new config/auth/routing/ratelimit abstractions
- **As** Tyler **I want** the legacy feature modules ported into consolette's `src/` tree, wired through the new `Upstream`/`AuthMethod`/`Provider`/`Router`/`AdmissionControl` abstractions **so that** the rename is a genuine rename + extension rather than a scaffold that dropped functionality (FR-6.4).
- **Context (superseding the original framing):** the original acceptance text ("confirm all feature modules compile+run unchanged post-rename") assumed a git-mv-style rename of the legacy crate. What actually happened in `c84491e` is a from-scratch scaffold of the ADR-001..004/007 abstractions with none of the legacy request-handling code carried over — there is nothing to "confirm unchanged" because it was never ported. This story is the real port, plus the two prerequisite layers the legacy code had that the scaffold doesn't yet:
  1. Concrete `Provider` impls (`AnthropicProvider`, `BedrockProvider`, new `OpenAiProvider`) satisfying `providers::Provider`, built from `Upstream`/`AuthMethod` config and calling `AuthMethodExt::apply` for headers — reconciling legacy `providers/anthropic.rs` + `providers/bedrock.rs` request/response translation against the new trait (Epic 2 Stories 2.2/2.3, previously unscheduled against this story).
  2. An axum HTTP server (`AppState`, route handlers) that constructs `Upstream`s/providers from `Config`, builds a `Router` (ADR-003) with a `RoutingStrategy` + `HealthRegistry` + `AdmissionControl`, and dispatches inbound requests through `Router::dispatch` — legacy `main.rs`/`fallback.rs` is the behavioral reference, not a file to keep, since `fallback.rs`'s ad hoc failover is superseded by the new `Router`.
  3. The feature modules themselves: `compression/`, `system_prompt/` (cache-aligner + verbosity), `memory/`, `metrics/`, `learn/`, `dashboard.rs`, `mcp_gateway.rs` — ported with minimal adaptation (legacy `crate::config::Config` reads become the equivalent new `schema::Config` fields, which already exist: `compress`, `compress_floor_bytes`, `cache_aligner`, `verbosity_level`, `memory_max_entries`).
  4. The `cmdcrush` and `mcp-proxy` standalone binaries (FR-6.1), including resolving the `rmcp` version gap (current `Cargo.toml` pins `0.1` for an unimplemented stub; legacy `mcp-proxy`/`mcp_gateway.rs` need `2.1`'s `server,client,transport-io,transport-streamable-http-client-reqwest,transport-streamable-http-server` feature set).
- **Acceptance:** `cargo build --release` produces both `consolette` and `mcp-proxy` binaries; `cargo clippy --deny warnings` clean (NFR-5); ported unit/integration tests pass; feature set (compression/cache-aligner/verbosity/memory/metrics/dashboard/mcp-gateway) present and exercised through the new `Router`/`Provider` path, not the legacy ad hoc one.
- **Files:** `Cargo.toml`, `src/main.rs`, `src/providers/{anthropic,bedrock,openai}.rs`, `src/compression/`, `src/system_prompt/`, `src/memory/`, `src/metrics/`, `src/learn/`, `src/dashboard.rs`, `src/mcp_gateway.rs`, `src/bin/cmdcrush/`, `src/bin/mcp-proxy/`

  - **Task 6.2.1** Update `Makefile` targets, binary install path, and launchctl label (already done in a prior pass per Epic 6 status note above — verify, don't redo). Files: `Makefile`.
  - **Task 6.2.2** Concrete providers: `AnthropicProvider`/`BedrockProvider`/`OpenAiProvider` implementing `providers::Provider`, constructed per-`Upstream` from `UpstreamKind` + `AuthMethod`, porting legacy request/response translation and compression/cache-aligner hooks. Files: `src/providers/anthropic.rs`, `src/providers/bedrock.rs`, `src/providers/openai.rs`, `Cargo.toml` (reqwest, aws-sdk-bedrockruntime, aws-config).
  - **Task 6.2.3** axum HTTP server: `AppState`, route handlers (`/v1/messages` etc.), `Router`/`RoutingStrategy`/`HealthRegistry`/`AdmissionControl` construction from `Config` at startup, replacing `run()`'s current one-line summary. Files: `src/main.rs` (or split per Task 6.1.4 into `src/cli.rs`/`src/server.rs`), `Cargo.toml` (axum, tower, tower-http).
  - **Task 6.2.4** Port `compression/`, `system_prompt/`, `memory/`, `metrics/`, `learn/` as their own modules, adapted to read the new `schema::Config` feature-toggle fields instead of the legacy `Config`. Files: `src/compression/*.rs`, `src/system_prompt/*.rs`, `src/memory/*.rs`, `src/metrics/*.rs`, `src/learn/*.rs`, `Cargo.toml` (regex, once_cell, sha2, hex, tiktoken-rs, simhash, lshdedup-core, tree-sitter + grammars, chrono, uuid).
  - **Task 6.2.5** Port `dashboard.rs` and `mcp_gateway.rs` against the new `AppState`. Files: `src/dashboard.rs`, `src/mcp_gateway.rs`, `Cargo.toml` (rmcp bump to `2.1`).
  - **Task 6.2.6** Port `cmdcrush` and `mcp-proxy` binaries; add matching `[[bin]]` entries. Files: `src/bin/cmdcrush/*.rs`, `src/bin/mcp-proxy/*.rs`, `Cargo.toml`.
  - **Task 6.2.7** Port applicable tests (compression regression, handshake, e2e where not environment-dependent) and run `cargo build --release && cargo clippy --deny warnings && cargo test` as the acceptance gate.

### Story 6.3 — com.consolette plist + launchd migration
- **As** Tyler **I want** a clean cutover **so that** daily Claude Code usage never drops (FR-6.2).

- **Canonical plist contract (SINGLE source of truth — Task 7.1.1's `.plist.j2`
  renders exactly this; the hand-authored `com.consolette.plist` is generated from
  the same contract):**
  - **Label:** `com.consolette`. **ProgramArguments:** `[<consolette binary path>]`.
  - **`RunAtLoad = false`** (settled value, used by BOTH the hand plist and the
    ansible-rendered agent). **Rationale:** both `com.claude-proxy-rs` and
    `com.consolette` bind port 47000, so loading must never auto-start before the old
    agent is confirmed unloaded; start is always explicit (`make migrate` /
    `launchctl kickstart` in ansible after the old agent is stopped — BLOCKER 11).
  - **`KeepAlive = true`**, **`ProcessType = Background`**.
  - **EnvironmentVariables — the FULL set the live `com.claude-proxy-rs.plist`
    injects, translated to the new names:**
    - `CONSOLETTE_PORT = 47000` (the loader reads `CONSOLETTE_PORT`; the shim also
      accepts legacy `PROXY_PORT` — bare `PORT` is never read).
    - `CLAUDE_CODE_OAUTH_TOKEN` (secret; the default `token_env` for the implicit
      `anthropic` upstream), `AWS_PROFILE = Sandbox.AdministratorAccess`,
      `AWS_REGION = us-west-2` (feed the implicit `bedrock` upstream options).
    - `HOME`, `PATH` (homebrew-first, as today).
    - **Former tuning env vars** (`COOLDOWN_SECONDS`, `REQUEST_TIMEOUT`,
      `BEDROCK_MAX_RETRIES`, `STAPLER_COMPRESS`, `COMPRESS_FLOOR_BYTES`,
      `CACHE_ALIGNER`, `VERBOSITY_LEVEL`, `MEMORY_MAX_ENTRIES`) now live in
      `conf.d/*.toml` (preferred). The plist MAY still set them (honored by the
      Story 1.5 shim) for a zero-diff cutover; the shipped plist sets them via conf.d
      and omits them from env to avoid drift. Document this choice in the plist
      comment so there is exactly one place per setting.
  - **StandardOut/ErrorPath:** `/tmp/consolette.log` / `/tmp/consolette.error.log`.

- **Acceptance:** the `.plist.j2` (Task 7.1.1) and the hand `com.consolette.plist`
  carry the identical env contract above and the same `RunAtLoad=false`; `make
  migrate` unloads `com.claude-proxy-rs`, loads + starts `com.consolette`, verifies
  `/health`; old plist retained until soak passes.
- **Files:** `com.consolette.plist`, `Makefile`

  - **Task 6.3.1** Author `com.consolette.plist` from the canonical contract above (label, ProgramArguments, full env set, `RunAtLoad=false`, log paths). Files: `com.consolette.plist`.
  - **Task 6.3.2** Add `make migrate`: `launchctl unload com.claude-proxy-rs` (ignore missing) → confirm it is stopped → `launchctl load com.consolette` → `launchctl kickstart -k gui/$(id -u)/com.consolette` → `curl -sf localhost:47000/health`. Files: `Makefile`.

## Epic 7: ndotfiles / ansible-managed install

**FR-7.** New ansible block mirroring the `aimee` block; cfgcaddy links for
conf.d; parameterized `.plist.j2`. Grounded in the `aimee` block (tasks.yml:109-189)
and `.cfgcaddy.yml`.

**Pre-Epic-7 branch check (resolved):** `ndotfiles`' local-only `consolette-install`
branch (tip `02239f2`) was confirmed via `git merge-base --is-ancestor` to be a
**fully-contained ancestor of `ndotfiles` `main`** — main has since added 14 files
on top of that tip (not deleted any, as originally assumed), so the branch holds
no commits main lacks and there is nothing to rebase. It was tagged
(`consolette-install-preserved-02239f2`) and both the branch and tag were pushed
to `origin` (`{internal-git-host}/tstapler/ndotfiles`) as a reference point, so it is
no longer local-only. Epic 7 work should branch from `main` directly, not from
`consolette-install`.

### Story 7.1 — Parameterized launchd template
- **As an** operator **I want** a `.plist.j2` **so that** ansible renders it per-machine (FR-7.4).
- **Acceptance:** `ndotfiles/launchd/com.consolette.plist.j2` renders the **canonical
  plist contract defined in Story 6.3** (identical env set + `RunAtLoad=false`),
  parameterized by binary path, port, and log paths (mirrors `com.employer.aimee.plist.j2`
  shape); renders valid plist.
- **Files:** `ndotfiles/launchd/com.consolette.plist.j2`

  - **Task 7.1.1** Author the template from the Story 6.3 canonical contract: `Label=com.consolette`, `ProgramArguments=[{{ consolette_bin }}]`, EnvironmentVariables = the full set — `CONSOLETTE_PORT={{ consolette_port }}`, `CLAUDE_CODE_OAUTH_TOKEN`, `AWS_PROFILE`, `AWS_REGION`, `HOME`, `PATH`, **plus the former tuning vars enumerated in Story 6.3** (`COOLDOWN_SECONDS`, `REQUEST_TIMEOUT`, `BEDROCK_MAX_RETRIES`, `STAPLER_COMPRESS`, `COMPRESS_FLOOR_BYTES`, `CACHE_ALIGNER`, `VERBOSITY_LEVEL`, `MEMORY_MAX_ENTRIES`) — templated as Jinja2 vars that default to unset/omitted (conf.d is the preferred home per Story 6.3; the template only emits them if an operator overrides the ansible var, preserving zero-diff-cutover parity with the legacy plist without duplicating tuning values by default) — `StandardOut/ErrorPath={{ consolette_log }}`/`{{ consolette_error_log }}`, `KeepAlive=true`, **`RunAtLoad=false`** (matches the hand plist; ansible starts explicitly), `ProcessType=Background`. Secrets come from ansible vars/vault, never inline in the repo. Files: `ndotfiles/launchd/com.consolette.plist.j2`.

### Story 7.2 — cfgcaddy config links
- **As an** operator **I want** conf.d source-controlled + linked **so that** config lives in ndotfiles (FR-7.2).
- **Acceptance:** `ndotfiles/.config/consolette/conf.d/*.toml` linked to `~/.config/consolette/conf.d/*.toml` via `.cfgcaddy.yml` `links:` (no `vendor-` prefix); secrets are indirect refs only (NFR-2/6).
- **Files:** `ndotfiles/.cfgcaddy.yml`, `ndotfiles/.config/consolette/conf.d/{00-providers,10-routing,20-ratelimit}.toml`

  - **Task 7.2.1** Add three `links:` entries (src `.config/consolette/conf.d/*.toml` → dest same path). Files: `ndotfiles/.cfgcaddy.yml`.
  - **Task 7.2.2** Add the conf.d files (copied from crate `references/conf.d/`, `PROJECT_ID` placeholder). Files: `ndotfiles/.config/consolette/conf.d/*.toml`.

### Story 7.3 — ansible install block (build → render → load, idempotent)
- **As an** operator **I want** an idempotent install block **so that** a second run is a no-op (FR-7.1, FR-7.3, Success Metric 5).
- **Acceptance:** new block in `ndotfiles/bootstrap/tasks.yml` (tag `consolette`) that `cargo build --release`s the binary, installs it, renders the `.plist.j2`, and does `launchctl list`/unload-if-changed/load exactly like the `aimee` block; re-run reports `ok` with no changes.
- **Files:** `ndotfiles/bootstrap/tasks.yml`

  - **Task 7.3.1** Tasks: `stat` the built binary + source mtime → `command: cargo build --release` in the crate dir `when` sources changed (`changed_when` on build output); `copy`/`file` the binary to the plist's path. Files: `ndotfiles/bootstrap/tasks.yml`.
  - **Task 7.3.2** Tasks mirroring aimee lines 160-189: `template` the plist (`register: consolette_plist`), `launchctl list com.consolette` (`register`, `failed_when: false`, `changed_when: false`), unload-when-changed, then load `com.consolette` **only after Task 7.3.3's teardown task has run and its `assert` has passed** (both bind 47000); `launchctl kickstart` to start (RunAtLoad=false). Load-when: `rc != 0 or plist.changed`. This load task has a **hard `ansible.builtin.assert` dependency on Task 7.3.3's post-teardown check** — the play halts (not just logs a warning) if that assert fails, so a stuck old agent cannot be silently skipped past. Files: `ndotfiles/bootstrap/tasks.yml`.
  - **Task 7.3.3** Real idempotent old-agent teardown, **a hard gate, not advisory ordering**: `launchctl list com.claude-proxy-rs` (`register: old_agent`, `failed_when: false`, `changed_when: false`); `launchctl unload ~/Library/LaunchAgents/com.claude-proxy-rs.plist` **only when `old_agent.rc == 0`** (present/loaded), `changed_when` on actual unload; idempotent (a second run finds `rc != 0` and no-ops). Immediately after (whether or not an unload ran), re-run `launchctl list com.claude-proxy-rs` (`register: old_agent_post`, `failed_when: false`, `changed_when: false`) and `ansible.builtin.assert: old_agent_post.rc != 0` **with `fail_msg` naming the still-loaded old agent and no `ignore_errors`** — this halts the entire play (Task 7.3.2's load never runs) rather than degrading to a warning, so the two agents cannot both bind port 47000. Files: `ndotfiles/bootstrap/tasks.yml`.

---

# Phase D — Validation & soak

## Epic 8: Validation, metrics, zero-regression soak (NFR-1..6)

### Story 8.1 — Integration tests for routing + auth + rate limiting
- **Acceptance:** integration tests cover fallback reproduction, weighted split ratios (via mock upstreams + request counts, Success Metric 2), per-upstream rate-limit independence (FR-4.2), and config precedence (Success Metric 3).
- **Files:** `tests/routing.rs`, `tests/config.rs`, `tests/ratelimit.rs`

  - **Task 8.1.1** Config precedence test (defaults < files < env). Files: `tests/config.rs`.
  - **Task 8.1.2** Weighted-ratio test with seeded RNG asserting approximate split. Files: `tests/routing.rs`.
  - **Task 8.1.3** Rate-limit shed/independence test using `FakeRelativeClock` (deterministic). Files: `tests/ratelimit.rs`.
  - **Task 8.1.4** Python `tomllib` parity as a REAL build gate (CD-1/NFR-2): a test (or `make`/CI step) that runs `python3 -c 'import tomllib; tomllib.load(open(f,"rb"))'` over EVERY shipped `references/conf.d/*.toml` fixture and fails the build on any error. Files: `tests/toml_parity.rs` (invokes `python3`) or `Makefile` CI target.

### Story 8.2 — Zero-regression soak
- **Acceptance:** after `make migrate`, Claude Code daily usage runs on `com.consolette` for the soak window with no dropped requests (Success Metric 6); `/metrics` shows expected provider usage; old `com.claude-proxy-rs` removed only after sign-off.
- **Files:** `references/model-gateway.md` (soak checklist section), `Makefile`

  - **Task 8.2.1** Document the soak checklist + rollback (`make migrate` reverse). Files: `references/model-gateway.md`.

---

## Notes on risky / flagged technology choices

- **`figment` 0.10.19** — no release since 2024 (stable, Rocket's engine). Acceptable; `config` 0.15 is the fallback if figment goes unmaintained. (ADR-001 alternatives.)
- **`auth = bearer` to a *local* SBN Dev Agent** — the whole Model Gateway path depends on a Java-oriented agent (`:9123`) that must be installed and survive reboots; native mTLS via the internal identity system in Rust is unproven (no public crate) and is not being built in core — it's the ADR-007 `exec` plugin's job. Mitigated: out of scope for core. (ADR-002, ADR-007.)
- **`dashmap` 5→6 bump** — forced by governor 0.10; low risk but a direct-dep major bump; verify no API breakage in health/metrics maps.
- **`rand` pin 0.8** — 0.9 moved `WeightedIndex`/`thread_rng`; pin 0.8 deliberately (ADR-003).
- **TPM accuracy** — `tiktoken-rs` is an OpenAI BPE estimate applied to Anthropic/Bedrock/gateway; TPM is approximate (documented guardrail, not exact accounting).
- **Two-dimension (RPM∧TPM) non-atomicity** — minor accounting drift on cross-dimension denial; accepted for single-tenant use (ADR-004).
</content>
</invoke>
