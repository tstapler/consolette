# Feature Landscape Research: http-entrypoint

Agent 2 (Features) — SDD Phase 2 research, `http-entrypoint`.

## 0. Codebase grounding

- `Command::Run` is a real enum variant already wired to a stub `fn run()` in
  `src/main.rs:24,95,115` — no listener is bound today.
- `src/config/schema.rs:243-244` already has `Config.port: u16` (`default_port`
  at `schema.rs:284`); `src/config/load.rs:17,100` allowlists `port` as an
  env-overridable field. No new config surface is needed for bind port.
- `src/routing/mod.rs` module doc states plainly: "Nothing outside tests calls
  this yet — CLI/MCP wiring lands with the HTTP provider implementations."
  `Router` (`src/routing/router.rs:26`) and `RoutingStrategy`/`HealthRegistry`
  (ADR-003) are fully built and unit-tested but have zero production callers.
- `src/providers/mod.rs:132` (`translate_openai_to_anthropic`),
  `:243` (`translate_anthropic_to_openai`), and `:335`
  (`translate_and_record`) are the three functions requirements.md calls out.
  The comment at `:293-296` explicitly notes `translate_anthropic_to_openai`
  "has no production caller in this codebase" — same gap as the router.
  `translate_and_record`'s doc comment (`:326-334`) says cost recording via
  `record_actual_usage_from_anthropic_response` is additive, not yet reachable
  from a live request.
- ADR-007 (`project_plans/consolette/decisions/ADR-007-plugin-format-and-credential-helper.md`)
  and ADR-003 (`.../ADR-003-routing-strategy-trait.md`) are the two decisions
  this feature must finally activate end-to-end; ADR-004 (rate limiting) is
  explicitly deferred post-selection and not required for this pass.

This confirms the requirement's framing: every piece except the listener and
its request/response glue already exists and is tested in isolation.

## 1. Comparable local-proxy tools — converged feature surface

Surveyed from general knowledge of Ollama's OpenAI-compat server, LM Studio's
local server, litellm proxy, and claude-code-router (all popular
single-binary/local-daemon front ends that translate between Anthropic-shaped
and OpenAI-shaped chat APIs and an upstream provider):

- **Two endpoint families on one process, one port.** Every one of these
  tools exposes both `/v1/chat/completions` (OpenAI shape) and, where the
  backend is Anthropic-capable, `/v1/messages` (Anthropic-native) from the
  same bound server — exactly the shape requirements.md asks for. None of
  them do content-based route switching by default; a single upstream/model
  target per logical route is the common case, matching this project's
  "exactly one default Route" scope decision.
- **Streaming is SSE, and it's the load-bearing code path, not an
  afterthought.** All four treat `stream: true` as the common case for
  interactive CLI clients (Claude Code, Cursor, etc.), and all have hit bugs
  around SSE framing (double-encoding `data:` lines, missing final
  `[DONE]`/`message_stop` event, not flushing on client disconnect) at some
  point in their issue trackers. The Anthropic SSE event sequence
  (`message_start`, `content_block_start/delta/stop` × N,
  `message_delta`, `message_stop`) and the OpenAI SSE sequence (chunked
  `delta` objects terminated by `data: [DONE]`) are different enough that a
  translating proxy must pick one wire format to speak per endpoint and
  faithfully replicate it — half-translating (e.g. OpenAI-shaped deltas
  wrapped in Anthropic SSE framing) breaks every real client.
- **Model-name passthrough/aliasing.** Because these proxies front multiple
  possible backends, they all support the client sending a model name that
  isn't the literal upstream model id — either passthrough (forward whatever
  string arrives) or a small alias table. A proxy that rejects unrecognized
  model strings surprises integrators who reasonably expect "any string same
  route" behavior when there's only one upstream. Given this project's scope
  is a single default Route with no per-request selection, the model field
  is decorative for routing purposes but must still round-trip correctly in
  the response (clients often assert response.model matches what they sent,
  or at least that it's non-empty/well-formed).
- **Error-envelope shape matching the real API, not a generic proxy error.**
  litellm and claude-code-router both go out of their way to reshape
  upstream/backend errors into the exact JSON error envelope the client SDK
  (Anthropic's or OpenAI's) expects — `{"type": "error", "error": {"type":
  ..., "message": ...}}` for Anthropic, `{"error": {"message": ..., "type":
  ..., "code": ...}}` for OpenAI — because official client SDKs
  pattern-match on that shape to decide whether to retry, and a bare
  `{"error": "..."}` or an HTML 502 page causes the SDK to throw an
  unhandled/unparseable exception instead of a typed API error.
- **Correct HTTP status codes are part of "compatible," not polish.** 400 for
  malformed/schema-invalid request bodies, 401/403 for auth failures
  (relevant here since ADR-007 exec-auth failures must surface as such, not
  as a 500), 404 for unknown routes, 429 for rate-limited/overloaded
  upstream (mapped through even though ADR-004 governor itself is out of
  scope this pass — an upstream 429 must still map to a 429, not a 500),
  and 5xx only for genuine proxy/upstream failure. Anthropic SDK retry logic
  keys off exact status codes; getting this wrong silently disables client
  retry.
- **Health/readiness endpoint conventions.** Ollama and LM Studio both expose
  a trivial unauthenticated `GET /` or `/api/tags`-style liveness check
  separate from the chat endpoints — useful for a maintainer's own
  smoke-testing and for clients that probe before sending real traffic.
  Not in requirements.md's explicit scope, but cheap and consistent with
  "admin endpoints beyond `serve-cost`" being explicitly out of scope — a
  bare liveness check is arguably not an "admin endpoint" and worth a
  scope-clarifying note rather than silently adding or silently omitting it.
- **Request/response logging distinct from cost tracking.** litellm and
  claude-code-router both log full or truncated request/response bodies at
  debug level for local debugging — separate concern from
  `CostTracker`/`record_actual_usage_from_anthropic_response`, but the two
  are easy to conflate when wiring the same dispatch seam. Worth keeping
  logging and cost-recording as two independent hooks off the same
  send/response point rather than one bolted onto the other.

## 2. Edge cases the design must handle

Grouped by where they bite:

**Malformed / partial client input**
- Invalid JSON body, missing required fields (`model`, `messages`), empty
  `messages` array, `messages` with only a `system` role and no user turn —
  each must map to a 400 with the correct error envelope (Section 1), not a
  panic or 500. Because `translate_openai_to_anthropic` is a pure function
  operating on `serde_json::Value` (`src/providers/mod.rs:132`), the HTTP
  layer must validate/deserialize *before* calling it — a malformed request
  should never reach translation code that assumes well-formed shape.
- System-prompt-only request (system message, no user turn) — legal for
  Anthropic's API in some client patterns (e.g. priming) but easy to mishandle
  if the translator assumes at least one user-role block exists.

**Model / capability mismatches**
- Unsupported/unknown model name: given single-Route scope, likely
  passthrough rather than rejection (Section 1) — but this is a design
  decision the plan phase should make explicit rather than leaving implicit,
  since silent passthrough vs. explicit 400 are both defensible and an
  integrator will notice either way.
- Requests exceeding context limits: this is fundamentally an upstream
  concern (Anthropic/Bedrock will reject with their own 400/413-equivalent),
  but the proxy must relay that rejection with the right status/envelope
  rather than swallowing it into a generic error — and must not attempt to
  silently truncate content, which would corrupt cost accounting and client
  expectations.

**Tool calls / multi-block content**
- Anthropic tool_use/tool_result blocks and OpenAI function/tool_calls have
  different shapes and different multi-turn conventions (OpenAI: separate
  `tool` role messages keyed by `tool_call_id`; Anthropic: `tool_result`
  content blocks with `tool_use_id`, embedded in a `user` message). The
  `translate_*` functions are unit-tested in isolation per requirements.md's
  own "Rabbit Holes" section — first live exercise of tool-call round-tripping
  is exactly where subtle mismatches (missing `tool_choice` translation,
  parallel tool calls, tool_result ordering) will surface. Multi-block content
  (text + image, multiple text blocks) has similar risk: OpenAI's
  content-as-array-of-parts vs. Anthropic's content-blocks-with-type both
  need faithful round-tripping, and stop_reason mapping
  (`tool_use`/`end_turn`/`max_tokens` ↔ `tool_calls`/`stop`/`length`) is a
  common source of client-visible bugs in comparable proxies.

**Streaming-specific**
- Client disconnects mid-stream: the HTTP server must detect this (dropped
  connection / cancelled future) and treat it as an early-terminate on the
  in-flight upstream call — this is exactly the "mid-stream cut" cost-tracking
  path requirements.md's Rabbit Holes section names. Concretely: does the
  proxy cancel the upstream request, and does partial usage get recorded or
  discarded? litellm's issue tracker has repeated reports of orphaned
  upstream requests continuing (and being billed) after client disconnect
  because the proxy didn't propagate cancellation.
- ADR-003 streaming-failover boundary: once the first SSE byte is flushed to
  the client, failing over to a different upstream is not observable-safe
  (the client has already seen a partial response from upstream A). The only
  safe in-band recovery is an error event within the same stream, matching
  requirements.md's explicit rabbit-hole callout. This must be enforced at
  the exact point bytes are flushed, not just "after send returns."
- Non-streaming client hitting what the server treats as a streaming-only
  code path (or vice versa): if the internal dispatch always streams from
  upstream (common implementation shortcut — treat everything as SSE
  internally and buffer for non-streaming clients), a client that sent
  `stream: false` (or omitted it) must get one clean buffered JSON response,
  not a dangling `text/event-stream` body or a response that only contains
  the first chunk. This buffer-vs-passthrough decision affects
  cost-recording (buffered case can record once at the end cleanly; streamed
  case needs the mid-stream/first-byte-failure/clean-success three-way split
  requirements.md's Rabbit Holes already flags).

**Concurrency**
- Concurrent requests to the same upstream: `HealthRegistry`/cooldown state
  (ADR-003) is shared mutable state accessed from multiple in-flight request
  handlers — needs to behave correctly under concurrent access (it's already
  designed for this per the ADR, but the HTTP layer is the first place
  concurrent callers actually exist; single-threaded test-only callers so far
  couldn't exercise races). Concurrent requests also stress
  `CostTracker`'s "replace not accumulate" semantics for a given request id
  (per the existing unit test at `src/providers/mod.rs:513`) — the HTTP layer
  must ensure request ids are unique per actual client request, not reused
  across concurrent connections.

**Auth**
- ADR-007 exec-credential-helper failures (helper binary missing, non-zero
  exit, malformed stdout, timeout) must surface to the HTTP client as a
  clean 401/403 with proper envelope, not a 500 — and must not leak
  credential-helper stderr/stdout verbatim into the client-visible error
  body (information disclosure — internal auth plumbing detail vs.
  Anthropic/OpenAI's own opaque auth-error message shape).

## 3. Unstated needs (what "compatible" implies beyond the letter of requirements.md)

- **SDK-compatible error envelopes and status codes** (detailed in Section 1)
  — an integrator pointing the real `anthropic` or `openai` Python/TS SDK at
  consolette will exercise the SDK's own response-parsing and retry logic;
  anything short of the exact expected shapes breaks silently inside the SDK
  rather than producing a debuggable proxy-side symptom.
- **`request_id`/`x-request-id` continuity.** Anthropic's real API returns an
  `id` field in the response body and often correlatable request headers;
  clients and this project's own `CostTracker` keyed by request id
  (`src/providers/mod.rs:513` test) both benefit from a stable id generated
  once per inbound HTTP request and threaded through translation, dispatch,
  and cost recording — not regenerated at each layer.
- **CORS / non-CLI client expectations are explicitly out of scope for this
  pass** given "local CLI tools ... same machine" in the Users section, but
  worth a one-line note in the plan so a future browser-based integrator
  doesn't assume it's covered.
- **Idempotent behavior on retry-safe methods.** CLI clients with their own
  retry logic (e.g. Claude Code's client) will retry a failed non-streaming
  request; the proxy shouldn't double-record cost or double-consume
  rate-limit budget for what the client perceives as one logical call if the
  first attempt never reached the upstream (i.e., failures before dispatch
  should not touch `CostTracker` at all — only the paths after
  `Router::dispatch` actually reaches an upstream should ever record usage).
- **Predictable behavior when the bound port is already in use** — not
  glamorous, but every comparable tool's first-run failure mode is "port
  already bound," and a clear error message (vs. a generic OS-level panic)
  is part of what a maintainer expects from a `run` command that's supposed
  to "just work."
- **Graceful shutdown** (SIGINT/SIGTERM) that lets in-flight requests
  finish or fails them cleanly rather than dropping connections mid-response
  — relevant to local dev loops where the maintainer will Ctrl-C the process
  constantly while iterating; not called out in requirements.md but a
  reasonable "for free" expectation for a `run` command bound to a real
  socket.

## Sources

- In-repo: `src/main.rs`, `src/config/schema.rs`, `src/config/load.rs`,
  `src/routing/mod.rs`, `src/routing/router.rs`, `src/providers/mod.rs`,
  `project_plans/consolette/decisions/ADR-003-routing-strategy-trait.md`,
  `project_plans/consolette/decisions/ADR-007-plugin-format-and-credential-helper.md`,
  `project_plans/http-entrypoint/requirements.md`.
- General/industry knowledge (model training data, not fetched this session):
  Ollama OpenAI-compatibility docs and issue-tracker patterns, LM Studio local
  server docs, litellm proxy docs/issues, claude-code-router project
  conventions, Anthropic Messages API and OpenAI Chat Completions API
  reference shapes (error envelopes, SSE event types, stop-reason enums).
  These are drawn from general familiarity rather than a fetched citation in
  this session — flagged here as INFERRED/background knowledge, not verified
  against current upstream docs.
