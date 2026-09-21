# Stack Research: gemini-provider

Research agent 1 (Stack), SDD Phase 2. All commands run from the consolette
repo root on this machine (Manjaro Linux, KDE Plasma / KWallet as the Secret
Service provider).

## 1. `antigravity-cli` (`agy`)

**VERIFIED** — installed and inspected directly (v1.1.26, downloaded via the
official installer, then removed — see cleanup note at the end of this doc).

- **Distribution**: a single ~210MB stripped native ELF binary named `agy`,
  fetched by `curl -fsSL https://antigravity.google/cli/install.sh | bash`
  from a per-platform manifest at
  `https://antigravity-cli-auto-updater-<project>.us-central1.run.app/manifests/<platform>.json`,
  SHA-512-verified. Platforms: `linux_amd64`, `linux_amd64_musl`,
  `linux_arm64[_musl]`, `darwin_{amd64,arm64}`, Windows. Self-updating
  (`agy update`).
- **Language**: Go. The binary's startup log lines are Go's own
  `glog`-style format (`ERROR: logging before google.Init: I0904 ...
  installer.go:27]`), and the installer script's own comment calls it a
  "Go-Native Setup Trigger."
- **Full subcommand list** (`agy --help`, `-h`, and any unrecognized
  subcommand all print the same top-level usage — there is no per-subcommand
  help fallback for unknown names):
  `agent`, `agents`, `changelog`, `help`, `install`, `mcp`, `mic-serve`,
  `models`, `plugin`/`plugins`, `remote-control`, `update`.
  **There is no `auth`, `login`, `logout`, or `print-access-token` subcommand.**
  `agy auth --help` / `agy login --help` are silently swallowed by the
  top-level usage printer (they're not recognized subcommands).
- Running `agy` bare requires a TTY (`bubbletea: could not open TTY`) — it's
  an interactive full-screen app by default; `--print`/`-p` runs one prompt
  non-interactively but still needs a session/auth already established.
- `/logout` is documented only as an **in-app slash command** (see the
  1.1.26 changelog entry: "Improved `/logout` execution time by
  short-circuiting token removal directly to file storage when keyring
  storage is bypassed or unreachable"). That line is the CLI's own
  confirmation of two coexisting storage paths: **keyring, with a file-storage
  fallback/bypass**.

### Token storage — VERIFIED on this machine

A real token file already exists from prior Antigravity IDE use:
`~/.gemini/antigravity-cli/antigravity-oauth-token`, mode `600`, JSON:

```json
{
  "token": {
    "access_token": "<261 chars>",
    "token_type": "Bearer",
    "refresh_token": "<103 chars>",
    "expiry": "2026-08-20T19:35:46.493304002-07:00"
  },
  "auth_method": "consumer"
}
```

(`auth_method: "consumer"` — a personal Google account, not Workspace/service
account, matching Tyler's setup.)

**Two important findings from this file:**

1. **No exec-friendly subcommand exists.** The requirements doc's plan A
   ("mirror `gcloud auth print-access-token` via `AuthMethod::Exec`") is not
   directly available — `agy` has no equivalent. The fallback path named in
   the requirements ("read its keyring/token-file storage directly") is the
   only one that exists today.
2. **This token is already expired** (expiry Aug 20, "today" for this
   research is Sep 4 — over two weeks stale), and there is no CLI subcommand
   to force a non-interactive refresh. This is a real feasibility risk beyond
   what's in the requirements doc's risk list: unless `agy`/the Antigravity
   IDE is run interactively often enough to keep the on-disk token fresh,
   consolette's Gemini upstream has no way to self-refresh. A credential
   helper can attempt the OAuth refresh-token grant itself
   (`POST https://oauth2.googleapis.com/token` with `grant_type=refresh_token`)
   using the `refresh_token` from this file, but that requires a client_id
   (and possibly client_secret) — which either has to be Google's own public
   `antigravity-cli` OAuth client id (would need to be extracted from the
   `agy` binary — not attempted here, and arguably crosses into the same
   "harvested client credential" territory the requirements doc explicitly
   rules out) or requires shelling out to `agy`/re-opening the IDE
   periodically. **Flag this as an open design question for Phase 3
   (planning), not resolved by this research.**

### Keyring check — VERIFIED (negative result)

This machine runs KDE Plasma; Secret Service is provided by `ksecretd`
(`org.freedesktop.secrets` / `org.kde.secretservicecompat` on the session
D-Bus — confirmed via `busctl --user list`). `secret-tool search --all` for
`antigravity`, `google`, `oauth`, `gemini`, and `cloudcode` under both
`service` and `label` attributes returned **no results**. So although the
CLI's changelog implies keyring-first-with-file-fallback, in practice (at
least for however this file was created — possibly a headless/IDE session
where Secret Service wasn't reachable) the artifact that actually exists is
the plain JSON file. **The file path is the concrete, verified integration
target; keyring support is a documented but unverified fallback.**

## 2. Reverse-engineered protocol (`v1internal:streamGenerateContent`)

`elad12390/antigravity-proxy`'s own README turned out to be the **weakest**
of the sources found — it documents a *failed* interception attempt
(mitmproxy, `HTTP_PROXY`/`HTTPS_PROXY` env vars, `/etc/hosts` DNS
redirection, and binary patching were all tried against
`https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal:streamGenerateContent?alt=sse`
and none reliably intercepted traffic — the author attributes this to a
"handshake followed by socket-based/TLS flow", i.e., likely gRPC-over-HTTP2
framing or connection-level pinning that a naive MITM proxy doesn't handle).
It confirms only the endpoint path and that responses stream via SSE
(`alt=sse`). No JSON schema is documented there.

Three follow-on/sibling projects (found via search, all more mature) gave
consistent, concrete schema details — **cross-checked across all three, they
agree, so confidence is reasonably high despite this being reverse-engineered
and undocumented by Google**:

- `NoeFabris/opencode-antigravity-auth` — `docs/ANTIGRAVITY_API_SPEC.md`
- `docs.picoclaw.io/docs/providers/antigravity/`
- `vahapogut/antigravity-add-model` (README — a proxy that already does
  format translation between OpenAI/Claude/Gemini/DeepSeek and this
  internal API, i.e., prior art for exactly the translation layer this
  project needs to write)

### Endpoints

- Base: `https://cloudcode-pa.googleapis.com` (picoclaw's docs; one
  `antigravity-proxy` issue shows a `daily-cloudcode-pa.googleapis.com`
  variant 404ing — there may be multiple pool hostnames, e.g. a `daily-`
  canary vs. stable host; the base host is not perfectly stable across
  sources).
- `v1internal:loadCodeAssist` (POST) — loads project/session info.
- `v1internal:fetchAvailableModels` (POST) — lists model IDs + quota state.
- `v1internal:streamGenerateContent?alt=sse` (POST) — the actual chat/completion call, non-streaming variant is presumably `v1internal:generateContent` (named in picoclaw's proxy-differentiation notes) though not directly confirmed by a schema dump.

### Request envelope (Cloud Code Assist wraps a Gemini-native body)

```json
{
  "project": "project-id",
  "model": "model-id",
  "requestType": "agent",
  "userAgent": "antigravity",
  "request": {
    "contents": [
      {"role": "user", "parts": [{"text": "..."}]},
      {"role": "model", "parts": [{"text": "..."}]}
    ],
    "systemInstruction": {"parts": [{"text": "..."}]},
    "generationConfig": {},
    "tools": [{
      "functionDeclarations": [{
        "name": "get_weather",
        "description": "...",
        "parameters": {"type": "object", "properties": {"...": {}}, "required": ["..."]}
      }]
    }]
  }
}
```

Notes cross-confirmed across sources:
- **Gemini-native `contents`/`parts` shape, not Anthropic `messages`.** Roles
  are `"user"` / `"model"` (never `"assistant"`) — this is the crux of the
  Anthropic↔Gemini translation layer, same shape as this repo's
  `translate_anthropic_request_to_openai` in `src/providers/mod.rs` but
  targeting Gemini's role/parts vocabulary instead of OpenAI's.
  `systemInstruction` must be an object (`{"parts":[...]}"`), never a plain
  string — a plain string 400s.
- Function names: `^[A-Za-z_][A-Za-z0-9_.:-]{0,63}$` roughly (must start
  with letter/underscore, allowed chars `a-zA-Z0-9_-.:`, max 64 chars, no
  slashes) — relevant if Anthropic tool names ever contain characters Gemini
  rejects.
- The IDE-facing envelope (`project`/`model`/`requestType`/`userAgent`/
  `request`) is Cloud-Code-Assist-specific wrapping; the inner `request`
  object is standard Gemini `generateContent` request shape.

### Response envelope

Non-streaming:
```json
{
  "response": {
    "candidates": [{
      "content": {"role": "model", "parts": [{"text": "..."}]},
      "finishReason": "STOP"
    }],
    "usageMetadata": {"promptTokenCount": 16, "candidatesTokenCount": 4, "totalTokenCount": 20},
    "modelVersion": "claude-sonnet-4-6",
    "responseId": "msg_vrtx_..."
  },
  "traceId": "abc123..."
}
```

Note the `modelVersion` example above (`claude-sonnet-4-6`) confirms this
internal API multiplexes non-Gemini models too (Claude, per Antigravity's
multi-model IDE support) — model IDs are not restricted to Gemini names, so
consolette's per-upstream `model` override (`RouteUpstreamRef.model`) should
pass whatever model id the internal `fetchAvailableModels` call returns
verbatim, not assume a `gemini-*` naming convention. Model id format from
one source: `antigravity/{model-id}` (e.g. `gemini-3-flash`,
`claude-opus-4-6`) — but this may be that *proxy's own* namespacing rather
than what the wire protocol itself expects; needs confirmation against a
live `fetchAvailableModels` call before finalizing (open question, not
resolved here).

Streaming: `Content-Type: text/event-stream`, frames are
`data: {json}\n\n` where each `{json}` is a partial version of the same
`{response: {candidates: [...]}, traceId}` envelope (i.e., NOT Anthropic's
bracketed `message_start`/`content_block_delta`/... event sequence — a
translation layer is required, directly analogous to this repo's existing
`OpenaiToAnthropicStream` in `src/providers/openai.rs:369-535`).

Error shape:
```json
{"error": {"code": 400, "message": "...", "status": "INVALID_ARGUMENT", "details": [...]}}
```
— maps cleanly onto `ProviderError::Validation`/`ProviderError::Upstream` the
same way `map_error_status` classifies Anthropic/OpenAI errors today.

### Required headers (cross-confirmed)

```
Authorization: Bearer {access_token}
Content-Type: application/json
User-Agent: antigravity/1.15.8 windows/amd64        (or just "antigravity" per one source — version-string format not fully pinned down)
X-Goog-Api-Client: google-cloud-sdk vscode_cloudshelleditor/0.1
Client-Metadata: {"ideType":"ANTIGRAVITY","platform":"MACOS","pluginType":"GEMINI"}
Accept: text/event-stream        (streaming only)
```

This answers the requirements doc's open question about IDE-mimicking
headers: **yes**, at least `User-Agent`, `X-Goog-Api-Client`, and
`Client-Metadata` (the last one JSON-encoded in a single header) appear
required across every source that documented headers at all — a bare
`Authorization` + `Content-Type` is not expected to be sufficient, though
this hasn't been confirmed by an actual live call from this research (no
fresh/valid token was available on this machine to test against — see
Section 1).

### Tool-call translation (prior art: `vahapogut/antigravity-add-model`)

That project's proxy already does Anthropic↔Gemini tool-call translation:
Anthropic `tool_use` content blocks become Gemini `functionCall` parts (with
per-model maps to track call IDs across parallel tool calls, since Gemini's
`functionCall`/`functionResponse` parts don't carry an explicit call-id field
the way Anthropic's `tool_use`/`tool_result` blocks do — the mapping has to
be maintained out-of-band, keyed by function name + call ordering). This is
the single most useful piece of prior art for the translation layer's
hardest part (tool calls) and is worth a deeper read during Phase 3
(planning)/implementation, though its runtime (Node/TS, not confirmed exact
language) means it's a reference for the *mapping logic*, not for code reuse.

## 3. Codebase patterns to mirror (`src/providers/anthropic.rs`, `openai.rs`)

Both read in full. `GeminiProvider` should structurally mirror `OpenaiProvider`
(`src/providers/openai.rs:39-263`) more closely than `AnthropicProvider`,
since both Gemini and OpenAI need a translation layer at the `Provider::send`
boundary (Anthropic is the trait's native wire format and needs none).

- **ADR-004 two-`Client` split**: identical in both files —
  `client` (pooled, `connect_timeout(10s)`, `read_timeout(request_timeout)`)
  for buffered calls, `stream_client` (`connect_timeout(10s)`,
  `pool_max_idle_per_host(0)`) for SSE. `GeminiProvider::new` should take the
  same `(upstream: Arc<Upstream>, resolver, exec_cache, request_timeout_secs)`
  shape as `OpenaiProvider::new` (plus no separate `base_url` param needed if
  Cloud Code Assist's host is hardcoded, matching `AnthropicProvider`'s
  choice not to expose one).
- **Auth**: reuse `anthropic::apply_auth_headers` (`src/providers/anthropic.rs:390-441`)
  verbatim — it's already a free function taking `&Upstream`, dispatching on
  `AuthMethod::{Bearer,Apikey,Exec}`. **No new `AuthMethod` variant is
  needed or wanted** — Section 1's finding is why `Exec` is the right choice:
  `command` would point at a small new credential-helper script that
  implements the ADR-007 §2 stdin/stdout JSON protocol
  (`src/auth/exec.rs:1-40`, `195-274`): reads
  `{upstream, method, url}` from stdin, must write back exactly one line of
  `{"headers": {...}, "cache_ttl_secs": <optional>}` to stdout. **This is a
  different contract than a bare `print-access-token`-style tool** — the
  helper must itself read `~/.gemini/antigravity-cli/antigravity-oauth-token`
  (or the keyring, per Section 1), check/attempt refresh, and emit the full
  `{"headers": {"authorization": "Bearer ...", "x-goog-api-client": "...",
  "client-metadata": "..."}}` object, not just a token string. `cache_ttl_secs`
  should be set short (much shorter than the token's real TTL) so a
  reauthenticated/refreshed token on disk is picked up promptly.
- **Streaming translation**: mirror `OpenaiToAnthropicStream`
  (`src/providers/openai.rs:369-535`) — a hand-rolled `Stream` impl wrapping
  `eventsource_stream::Eventsource` (via `.eventsource()` on a
  `Result<Bytes, anyhow::Error>` byte stream), buffering synthetic
  `message_start`/`content_block_start`/...`/message_stop` Anthropic SSE
  frames in a `VecDeque<Bytes>` since Gemini's flatter per-chunk envelope has
  no equivalent bracketing. `finish_reason`/`finishReason` mapping needs its
  own `map_gemini_finish_reason` analogous to `map_openai_finish_reason`
  (`src/providers/mod.rs`).
- **Error classification**: mirror `map_error_status` in both files —
  429/529 → `ProviderError::RateLimited` (honoring `retry-after`), 4xx →
  `ProviderError::Validation(body, status)`, other non-2xx →
  `ProviderError::Upstream{status,body}`. Gemini's `{"error":{"code","status"}}`
  shape maps onto this without needing new `ProviderError` variants — but the
  requirements doc's "fail closed on unexpected internal-API response shape"
  point means `send_request`'s success-path parsing (pulling
  `response.candidates[0].content.parts` etc.) should return
  `ProviderError::Upstream` (not panic / not silently return empty content)
  whenever the expected fields are missing, exactly the discipline
  `clean_request_body`/`map_error_status` already follow.
- **Cargo.toml versions actually resolved** (`Cargo.lock`, checked directly):
  `reqwest 0.12.28` (a second `reqwest 0.13.4` is also resolved
  transitively — worth a heads-up for Phase 3/6 review, not this feature's
  problem to fix), `eventsource-stream 0.2.3`, `futures-util 0.3.34`,
  `serde_json 1.0.151`, `async-trait 0.1.92`. No new crate is needed for the
  HTTP/streaming side — `GeminiProvider` reuses all of the above unchanged.
- `references/conf.d/00-providers.toml` referenced in the requirements does
  not exist yet in this repo (`references/` doesn't exist) — Phase 3
  planning should treat "add `references/conf.d/00-providers.toml`" as
  creating that file/directory, not editing an existing example.

## 4. Keyring crate (fallback path if reading the keyring directly is needed)

**Not currently a dependency** (`Cargo.lock` has no `keyring`/`secret-service`
entry). Section 1's negative `secret-tool` search means the *file* path
(`~/.gemini/antigravity-cli/antigravity-oauth-token`) is the concrete,
verified target for v1 — a keyring reader is speculative until an actual
keyring-stored token is observed on some machine. If/when needed:

- Crate: `keyring` — current version **4.2.0** (docs.rs, checked directly).
  Backend selection is feature-gated; relevant Linux stores are
  `zbus-secret-service-keyring-store` (async, `zbus`-based — the natural fit
  for a Tokio app like consolette, avoids blocking the async runtime on
  D-Bus calls) vs. `dbus-secret-service-keyring-store` (sync, blocking — the
  older `libdbus`-based implementation) vs. `linux-keyutils-keyring-store`
  (kernel keyring, no desktop Secret Service involved at all — irrelevant
  here since `agy`'s docs describe Secret-Service-style keyring use).
  Exact recommended feature flag string wasn't independently confirmed
  beyond docs.rs' feature list (worth a `cargo add keyring --dry-run` /
  reading its `Cargo.toml` on crates.io during planning if this path is
  chosen) — but the async `zbus-secret-service-keyring-store` variant is the
  one to prefer given this is a Tokio app and the exec-helper approach (a
  separate short-lived process) sidesteps the blocking-vs-async question
  entirely anyway, which is another point in favor of the `Exec` approach
  over embedding keyring-reading code directly in `consolette`.

## Cleanup note

This research installed `agy` 1.1.26 to inspect it. Its installer wrote
`export PATH=...` lines directly into `~/.bashrc`, `~/.zshrc`, `~/.zprofile`,
`~/.bash_profile`, `~/.profile`, and `~/.config/fish/config.fish` (several of
which are symlinks into the `~/dotfiles` jj/git repo) and placed the ~210MB
binary at `~/.local/bin/agy`, all without being asked. All six shell-profile
edits were reverted (verified via `jj diff` on the dotfiles repo — clean,
pure-removal diff) and `~/.local/bin/agy` was deleted. The scratchpad copy at
`/tmp/.../scratchpad/bin/agy` used for `--help`/`--version` inspection is
outside the repo and outside `$HOME`'s real dotfiles, so no further cleanup
was needed there.
