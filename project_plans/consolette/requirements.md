# Requirements: Consolette

**Date**: 2026-07-17
**Type**: Feature evolution + rename of an existing service (`claude-proxy-rs` → `consolette`)
**Status**: Requirements confirmed (Phase 1 complete)

## Problem Statement

Tyler runs a personal, always-on LLM proxy on his Mac (`stapler-scripts/claude-proxy-rs`,
Rust, macOS LaunchAgent, port 47000). Today it does exactly one routing job — Anthropic
(OAuth) primary with an AWS Bedrock fallback on 429 — and is configured entirely through
environment variables read once at startup ("No hot-reloading — restart the proxy").

This is too rigid. Tyler wants a **local, provider-agnostic LLM router/mixer** that can:
- Point Claude Code (and any OpenAI-compatible client) at arbitrary upstreams, including
  the **internal Model Gateway** (my employer's) and any third-party OpenAI-compatible API, not just
  Anthropic + Bedrock.
- Be **configured declaratively** (files, not just env vars) and version-controlled in
  his dotfiles.
- **Route** per-model/per-route by either ordered fallback (today's behavior) or
  weighted split (OpenRouter-style A/B mixing).
- **Rate-limit** per upstream so a personal key or a shared gateway quota is respected.
- Be **installed and managed by ndotfiles/ansible** the same way his other LaunchAgents
  (e.g. the `aimee` block) are.

The renamed product is **Consolette** (name locked 2026-07-17): repo/binary `consolette`,
config at `~/.config/consolette/conf.d/`, launchd label `com.consolette`.

## Users / Consumers

- **Primary user**: Tyler, on his personal Mac (single-tenant, localhost-only).
- **Client software**: Claude Code (Anthropic Messages API on `/v1/messages`), plus any
  OpenAI-compatible client/tool (LiteLLM, scripts) on `/chat/completions` and
  `/v1/chat/completions`.
- **Upstreams**: Anthropic API, AWS Bedrock (existing); the internal Model Gateway and
  arbitrary OpenAI-compatible LLM APIs (new).
- **Operators of the config**: ndotfiles/ansible + cfgcaddy on Tyler's machine(s).

## Success Metrics

1. Consolette can route to **≥3 upstream types** — Anthropic, Bedrock, and at least one
   new OpenAI-compatible upstream (ideally the internal Model Gateway) — selected by config,
   with zero code changes to add a new upstream.
2. A route configured `strategy = fallback` reproduces today's Anthropic→Bedrock behavior
   exactly; a route configured `strategy = weighted` splits traffic across upstreams in
   the configured ratio (verified by request counts in metrics).
3. Configuration loads from `~/.config/consolette/conf.d/*.toml` via lexical deep-merge
   with env-var overrides on top; a documented precedence order is enforced and tested.
4. Per-upstream rate limiting (RPM/TPM) throttles or sheds requests to a single upstream
   without affecting others.
5. `ansible-playbook` install is **idempotent**: it builds the binary, renders the
   launchd plist, links config via cfgcaddy, and loads the agent; a second run is a no-op.
6. Zero regression in Tyler's daily Claude Code usage during a soak period.

## Confirmed Decisions (fixed constraints — signed off 2026-07-17)

These are **requirements, not open questions**. Research and planning must design *around*
them, not relitigate them.

- **CD-1 — Rust-first.** Extend `claude-proxy-rs` in Rust. A later Python port
  (`stapler-scripts/claude-proxy`, FastAPI) will reuse the *same config schema*; schema
  choices must be portable to Python (`tomllib`).
- **CD-2 — Pluggable per-upstream auth**, keyed `auth = bearer | apikey | exec`.
  Ship `bearer` and `apikey` now (both usable from the Mac today). **There is no
  core-native `internal`/mTLS auth variant** — per [ADR-007](decisions/ADR-007-plugin-format-and-credential-helper.md),
  my employer's internal identity system (mTLS via the internal identity CLI, formerly
  described here as `auth = internal`) is delivered entirely as an `exec`
  credential-helper plugin bundle (ndotfiles, Epic 7), not a core auth mode.
- **CD-3 — Both routing strategies**, selected per-route: `strategy = fallback` (ordered,
  current behavior) OR `strategy = weighted` (OpenRouter-style split). Health/429 cooldown
  applies to **both** strategies.
- **CD-4 — config.d is TOML** at `~/.config/consolette/conf.d/*.toml`, merged in **lexical
  filename order** via deep-merge, one file per concern (`00-providers.toml`,
  `10-routing.toml`, `20-ratelimit.toml`); **env vars override on top**. TOML chosen
  because it is Rust-idiomatic AND read natively by Python (`tomllib`) → one shared schema
  across both proxies. Config is source-controlled in **ndotfiles**, cfgcaddy-linked
  (note: `~/.config/consolette/*` does NOT need the `vendor-` prefix that `.claude/*` does),
  and installed/managed by a **new ansible block that `cargo build`s the binary and
  renders a launchd plist**, mirroring the existing `aimee` block.
- **CD-5 — Naming.** Repo/binary `consolette`; config dir `~/.config/consolette/conf.d/`;
  launchd label `com.consolette`. The `claude-proxy-rs` → `consolette` rename is in scope.

## Functional Requirements (INVEST)

### FR-1: Layered config.d configuration
- FR-1.1: Load all `*.toml` files from `~/.config/consolette/conf.d/` in **lexical
  filename order** and **deep-merge** them into one config tree (later files override
  earlier keys; tables merge, scalars/arrays replace unless otherwise specified).
- FR-1.2: Apply **environment variables as the highest-precedence overlay** on top of the
  merged file config. Precedence, lowest→highest: built-in defaults → conf.d files
  (lexical) → environment variables.
- FR-1.3: One-file-per-concern convention: `00-providers.toml` (upstreams+auth),
  `10-routing.toml` (routes/strategies), `20-ratelimit.toml` (limits). No file is
  mandatory; a fully-empty conf.d yields working defaults (back-compat with env-only).
- FR-1.4: The schema must be expressible in and parseable by Python `tomllib` unchanged
  (no Rust-only constructs). (Supports CD-1.)
- FR-1.5: Config load errors (bad TOML, unknown upstream reference in a route, invalid
  auth type) fail fast at startup with a clear, actionable message naming the file+key.
- FR-1.6 (stretch): Optional hot-reload — watch conf.d and re-load on change without a
  restart; behind a flag, default off. Must be safe for in-flight requests. If hot-reload
  is deferred, `make restart` / `launchctl kickstart` remains the supported path.

### FR-2: Upstream + pluggable auth abstraction
- FR-2.1: An **upstream** is a named, configured LLM endpoint: `name`, `kind`
  (`anthropic` | `bedrock` | `openai`), `base_url`, `auth`, and kind-specific options.
- FR-2.2: Auth is pluggable per upstream via `auth = { type = "bearer" | "apikey" |
  "exec", ... }`:
  - `bearer`: send `Authorization: Bearer <token>`; token from config, env, or keychain
    reference.
  - `apikey`: send an API key in a configurable header (default `x-api-key`, or
    `Authorization` for OpenAI-style).
  - `exec`: spawn a configured helper command, feed it a JSON request context on stdin,
    and apply the JSON headers it returns on stdout ([ADR-007](decisions/ADR-007-plugin-format-and-credential-helper.md)).
    This is the extension point for identity systems (e.g. my employer's internal identity
    CLI) that cannot ship in core — there is no core-native `internal` auth type.
    (Supports CD-2.)
- FR-2.3: Secrets are never written to logs; token/key values may be given as an indirect
  reference (env var name or macOS keychain item) rather than inline plaintext.
- FR-2.4: The existing Anthropic (OAuth) and Bedrock providers must be re-expressible as
  upstreams in this model with **no behavioral regression** (default config reproduces
  today's Anthropic-primary/Bedrock-fallback setup).
- FR-2.5: OpenAI-compatible upstreams (`kind = openai`) accept an arbitrary `base_url`
  and forward OpenAI Chat Completions requests, enabling both the internal Model Gateway
  (OpenAI-compatible path) and third-party APIs.

### FR-3: Routing strategies (fallback | weighted)
- FR-3.1: A **route** maps an incoming request (by model name and/or endpoint) to an
  ordered/weighted set of upstream references plus a `strategy`.
- FR-3.2: `strategy = fallback` — try upstreams in listed order; on failure/429/health-down
  move to the next. This reproduces current behavior and is the default.
- FR-3.3: `strategy = weighted` — select an upstream probabilistically by configured
  weights (OpenRouter-style). Unhealthy/cooling-down upstreams are excluded and their
  weight redistributed among healthy peers.
- FR-3.4: **Health/cooldown applies to both strategies**: a 429 (or configurable error
  class) places an upstream in a cooldown for `cooldown_seconds` (default 300); it is
  skipped by fallback and excluded from weighted selection until cooldown expires.
  (Supports CD-3.)
- FR-3.5: Both strategies must live behind **one strategy interface/trait**, reusing the
  existing `fallback.rs` state machine where practical rather than forking it.
- FR-3.6: If all upstreams for a route are unhealthy, return a clear 503 with retry
  guidance (mirrors current graceful-degradation behavior).

### FR-4: Per-upstream rate limiting
- FR-4.1: Each upstream may declare limits: requests-per-minute (RPM) and/or
  tokens-per-minute (TPM). Config lives in `20-ratelimit.toml`.
- FR-4.2: Rate limiting is **per-upstream and independent** — throttling upstream A must
  not throttle upstream B.
- FR-4.3: On limit breach, behavior is configurable per upstream: either **shed** (treat
  like a cooldown so routing falls through to the next upstream / redistributes weight) or
  **queue/delay** up to a bounded wait. Default: shed (compose cleanly with FR-3.4).
- FR-4.4: TPM accounting uses the request's estimated token count (reuse existing
  `tiktoken-rs` tokenizer already in the tree).
- FR-4.5: Rate-limit state and decisions are visible in `/metrics` and `/dashboard`.

### FR-5: Internal Model Gateway upstream
- FR-5.1: Ship a working, documented example config for an internal Model Gateway upstream
  usable **from Tyler's personal Mac** (not only from a workspace/mesh sidecar), using the
  auth method determined in research (`bearer` expected).
- FR-5.2: Support the gateway's OpenAI-compatible endpoint path; support its
  Anthropic-compatible path if research confirms one exists and it is useful for Claude
  Code.
- FR-5.3: Document the concrete base URL(s), endpoint paths, and auth handshake, and how
  the internal identity system's mTLS path is delivered as an `exec` credential-helper
  plugin bundle ([ADR-007](decisions/ADR-007-plugin-format-and-credential-helper.md)) rather than a core auth type.

### FR-6: Rename `claude-proxy-rs` → `consolette`
- FR-6.1: Rename the crate/binary to `consolette`; keep the second `mcp-proxy` binary
  working.
- FR-6.2: New launchd label `com.consolette` and plist; provide a migration path off the
  old `com.claude-proxy-rs` agent (unload old, load new) without dropping Tyler's daily
  usage.
- FR-6.3: Update config dir references, log paths, and Makefile targets to the new name.
- FR-6.4: Preserve all existing features (compression, cache-aligner, verbosity,
  memory, metrics, dashboard, mcp-gateway) through the rename — this is a rename +
  extension, not a rewrite.

### FR-7: ndotfiles / ansible-managed install
- FR-7.1: Add a new ansible block in ndotfiles `bootstrap/tasks.yml` that mirrors the
  `aimee` block: install/`cargo build --release` the binary, render a launchd
  `.plist.j2`, `launchctl load` it.
- FR-7.2: Config files (`conf.d/*.toml`) are source-controlled in ndotfiles and linked to
  `~/.config/consolette/conf.d/` via cfgcaddy (`.cfgcaddy.yml` `links:`), no `vendor-`
  prefix needed for `~/.config/consolette/*`.
- FR-7.3: The install is **idempotent** — re-running the playbook does not rebuild
  unnecessarily, duplicate the agent, or break a running instance.
- FR-7.4: Provide the launchd plist as a Jinja2 template (`.plist.j2`) parameterized by
  port, binary path, and log paths.

## Non-Functional Requirements

- NFR-1 (Compatibility): Wire-compatible with Claude Code (`/v1/messages`) and
  OpenAI-compatible clients (`/chat/completions`, `/v1/chat/completions`) — no client
  changes.
- NFR-2 (Portability of schema): The TOML config schema must load unchanged in Python
  `tomllib` for the future port. No secrets in the repo — only indirect references.
- NFR-3 (Performance): Preserve existing characteristics — <100ms startup, <50MB idle,
  50+ concurrent streams. Config layering and routing must not add meaningful per-request
  latency (routing decision O(routes)).
- NFR-4 (Reliability): A misconfigured single upstream/route must not take down the proxy;
  fail fast on bad config at startup, degrade gracefully at runtime.
- NFR-5 (Maintainability): `cargo clippy --deny warnings` clean; new modules documented;
  routing/auth abstractions covered by unit + integration tests.
- NFR-6 (Security): localhost-bind only; secrets via env/keychain references; secrets
  redacted in all logs and the dashboard.

## Scope

### In Scope
- config.d TOML layering + env overlay (FR-1)
- Upstream + `bearer`/`apikey` auth abstraction (FR-2)
- `fallback` and `weighted` routing behind one interface, shared cooldown (FR-3)
- Per-upstream RPM/TPM rate limiting (FR-4)
- Internal Model Gateway upstream via bearer/apikey, documented (FR-5)
- `claude-proxy-rs` → `consolette` rename (FR-6)
- ndotfiles/ansible install block + cfgcaddy config linking (FR-7)

### Out of Scope
- **A core-native `internal`/mTLS auth type.** Superseded by ADR-007: the internal
  identity system integrates via the generic `exec` credential-helper plugin protocol
  (ndotfiles plugin bundle, Epic 7), not a core auth mode (CD-2).
- The **Python port** — this effort only ensures schema portability; the port is later.
- Multi-tenant / non-localhost / remote-hosted operation.
- GUI configuration beyond the existing dashboard (read-only monitoring).
- Rewriting existing features (compression/memory/etc.) — they carry over unchanged.
- Windows support.

## Open Questions (for research phase)
- OQ-1: Is there a **bearer-token / API-key path to the internal Model Gateway usable from
  a personal Mac** (not only the workspace mesh sidecar)? Concrete base URL(s),
  OpenAI-compatible and Anthropic-compatible endpoint paths, and the auth handshake.
  How does the `exec` credential-helper plugin protocol (ADR-007) need to shape up to
  carry mTLS via the internal identity system later? — THE critical risk.
- OQ-2: `figment` vs the `config` crate for directory/lexical deep-merge + env overlay;
  `notify` for optional hot-reload — which to adopt?
- OQ-3: Health-aware **weighted** cross-provider selection + 429 cooldown in Rust,
  coexisting with ordered fallback behind one trait — design, reusing `fallback.rs`.
- OQ-4: Per-upstream token-bucket rate limiting in Rust (`governor`?) — RPM + TPM, and the
  `20-ratelimit.toml` config shape.
