# ADR-007: Plugin Format + Credential-Helper Exec Protocol

**Status**: Accepted
**Date**: 2026-07-20
**Refines**: ADR-002 (drops the core `internal` auth type; it becomes an internal-identity-plugin helper)

## Context

Directive from Tyler (2026-07-20): Consolette's core lives in his **public**
dotfiles repo, but the Model Gateway integration carries **employer-specific
details** (endpoints, project IDs, VIP hostnames, the internal identity system's
auth mechanism). None of that — neither code nor values — may land in the public
repo. It must live on the employer's side (ndotfiles) and be **installed/loaded later**.

The original plan split *config values* (public code with `sk-dummy`/`{PROJECT_ID}`
placeholders; real values in ndotfiles `conf.d`). That covers values but not
*behavior*: `auth = internal` (ADR-002 §3) still baked an employer-named auth mode
and its internal identity CLI shell-out into the public core. We need a seam where
employer-specific behavior is contributed, not hardcoded.

Key enabler: the Model Gateway is OpenAI/Anthropic-compatible, so an internal
upstream needs **no custom Rust** — only (a) config pointing at it and (b) a way
to inject internal auth. So the plugin surface can be small: config + an auth hook.

## Decision

### 1. A plugin is a discoverable bundle (no dynamic code loading in v1)

```
<plugin-dir>/<name>/
  plugin.toml        # manifest: name, version, description, provides = ["conf.d","credential-helper"]
  conf.d/*.toml      # config fragments merged into the core config (upstreams/routes/ratelimit)
  bin/<helper>       # optional executable hook(s) (credential helpers, etc.)
```

- **Discovery**: core scans a plugin search path — `${XDG_CONFIG_HOME:-~/.config}/consolette/plugins.d/*/`
  plus any dirs in `CONSOLETTE_PLUGIN_PATH` (colon-separated). Each dir with a
  valid `plugin.toml` is a plugin. Missing dir = no plugins (core still runs).
- **Config merge order**: core `conf.d/*.toml` first, then each plugin's
  `conf.d/*.toml` in plugin-name lexical order, all under the same figment
  sorted-glob deep-merge (ADR-001). Plugin fragments are peers of core fragments;
  later wins, same as any conf.d layering. `deny_unknown_fields` + reference
  validation run over the *merged* result.
- **v1 explicitly does NOT load native code** (`.so`/`.dylib`/`cdylib`/WASM). The
  only executable extension point is the exec-helper protocol below (out-of-process,
  ABI-free, language-agnostic). This is deliberate: dynamic linking is ABI-fragile
  and a security/complexity cost we don't need while config + an auth hook fully
  express the internal model gateway.

### 2. Credential-helper exec protocol (`auth = { type = "exec", ... }`)

A generic per-upstream auth type. The core invokes an external command to obtain
auth material; the command encapsulates whatever provider-specific logic it needs.

- **Schema**: `auth = { type = "exec", command = "<name-or-path>", args = [...],
  cache_ttl_secs = <u64, default 300>, timeout_secs = <u64, default 10> }`.
  `command` resolves against the owning plugin's `bin/` first, then `PATH`.
- **Invocation**: core runs `command args...` with a **request context on stdin**
  as one JSON line: `{"upstream":"<name>","method":"POST","url":"<full-url>"}`.
- **Response**: helper prints one JSON object on **stdout** and exits `0`:
  `{"headers":{"Authorization":"Bearer …","x-internal-project-id":"…"},"cache_ttl_secs":300}`.
  Core merges `headers` into the outbound request (helper headers win over static
  ones). Optional `cache_ttl_secs` overrides the configured TTL.
- **Failure**: non-zero exit (or timeout, or unparseable stdout) → the upstream is
  treated as **unavailable** (same seam as a health/cooldown miss, ADR-003) so the
  router fails over; stderr is captured into a redacted, rate-limited log line.
- **Caching**: successful results cached per `(upstream, command-hash)` for
  `cache_ttl_secs` to avoid a subprocess per request. `SIGHUP`/reload clears it.
- **Security/redaction**: helper stdout is a secret — never logged, never rendered
  in `/metrics`, `/dashboard`, `/health`. Only header *names* may appear in debug.
  Helper `command` must resolve to a file owned by the user and not world-writable,
  else the upstream is rejected at startup with a clear error.

### 3. `internal` leaves the core (refines ADR-002 §3)

The core ships exactly three auth types: `bearer`, `apikey`, `exec`. There is **no
`internal` type in the core schema anymore.** Auth via the internal identity system
is delivered as the **internal-identity plugin's** `exec` credential-helper
(`bin/consolette-internal-auth-helper`), which shells out to the internal identity
CLI (`internal-identity curl -a copilotdppython … :7004`) (or, on the SBN Dev
Agent path, is simply unused because the dummy bearer suffices). ADR-002's Options
A/B (dummy bearer to a local proxy) are unchanged and remain the default; Option C
(native mTLS) is now just "the internal-identity plugin's helper matures," with zero core
change.

### 4. The internal-identity plugin (ships in ndotfiles, Epic 7)

```
internal-identity/
  plugin.toml                     # name = "internal-identity"
  conf.d/50-model-gateway.toml    # the Model Gateway upstream: real base_url, project_id,
                                  #   kind = openai|anthropic-passthrough, auth = exec → internal identity helper
  bin/consolette-internal-auth-helper  # the only place internal-identity/VIP/project details live
```

Installed by the ndotfiles ansible block into `~/.config/consolette/plugins.d/internal-identity/`
(cfgcaddy-linked). The public core repo contains **no** employer hostnames, project
IDs, or internal-identity-system logic — only the generic `exec` machinery and, at most, a
`plugins.d/example/` bundle using placeholders.

## Alternatives Considered

| Option | Rejected because |
|--------|-----------------|
| Native dynamic plugins (`cdylib` + `dlopen`/`abi_stable`) | ABI-fragile across rustc versions, security surface, and unnecessary — config + exec-hook fully express the internal upstream. Revisit only if a plugin ever needs in-process request/response transforms hot-path. |
| WASM plugins (wasmtime) | Real isolation but heavyweight for a single-user local proxy; large dep + host-binding work for no v1 benefit. |
| Keep `auth = internal` in core (ADR-002 as-was) | Bakes an employer-named mode + the internal identity CLI into the public repo — the exact thing Tyler asked to avoid. |
| Config-only plugins (no exec hook) | Can't express internal *auth behavior*, only values; the dummy-bearer path would work but native mTLS (Option C) would force core changes later. `exec` future-proofs it. |

## Consequences

- Public core stays employer-free: generic `bearer|apikey|exec` auth + plugin
  discovery. Adding a new provider's auth = drop a plugin, no core change.
- The exec protocol is a well-worn pattern (kubectl exec-auth, `aws
  credential_process`, git/docker credential helpers) — precedent for the JSON
  stdin/stdout + TTL-cache + exit-code-as-availability design.
- One subprocess per upstream per `cache_ttl_secs` (amortized ~0 with caching);
  helper latency counts against request latency on cache-miss — helpers must be
  fast or cache long. Documented in the helper-authoring notes.
- Epic 2 (auth) gains: plugin discovery/loader, `plugin.toml` parse, `exec` auth
  type + cache + redaction. Epic 7 gains: the `internal-identity` plugin bundle in ndotfiles.
  Validation (Epic 8) gains: plugin-merge precedence test + a fake exec-helper test
  (headers injected, non-zero exit → upstream skipped, TTL cache hit).
- `deny_unknown_fields` must tolerate plugin-contributed tables — validate over the
  merged config, not per-fragment.
