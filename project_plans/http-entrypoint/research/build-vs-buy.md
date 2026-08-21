# Research: Build vs. Buy for the HTTP-binding layer

**Scope**: this only covers the HTTP-binding/wiring layer (bind port, route handlers,
SSE plumbing, request/response schema types feeding `Router::dispatch`). It does not
reopen the existing `Router`, `Provider`, or translation-function architecture, all of
which are already built and out of scope for replacement per the requirements doc.

Key fact discovered during research that bears on every option below:
`Router::dispatch(&self, body: serde_json::Value, headers: HeaderMap, ...)`
(`src/routing/router.rs:62`) already takes an untyped `serde_json::Value`, not a typed
request struct, and `translate_openai_to_anthropic`/`translate_anthropic_to_openai`
(`src/providers/mod.rs:132`, `:243`) operate on `&serde_json::Value` in and
`serde_json::Value` out. There is no typed Anthropic/OpenAI schema anywhere in the
existing dispatch path to plug a third-party SDK's types into without an adapter layer.

## 1. Existing OSS library/framework for proxy scaffolding

Candidates found:

- **`anthropic-proxy-rs`** (m0n0x41d, MIT, 31 stars/12 forks, active as of the
  research snapshot) — Anthropic→OpenAI proxy for routing Claude Code traffic to
  OpenAI-compatible backends (OpenRouter, Together, etc.). SSE streaming, tool-call
  translation, one direction only (Anthropic-in, OpenAI-out upstream).
- **`claude-max-api-proxy-rs`** (thhuang) — dual-protocol axum server (`/v1/messages`
  and `/v1/chat/completions` on the same port) but backed by shelling out to the
  `claude` CLI subprocess, not a generic multi-provider router.
- **`axum-reverse-proxy`** / **`axum-proxy`** — generic Tower/axum reverse-proxy
  building blocks (request forwarding, load balancing via `tower::discover::Discover`).
  No Anthropic/OpenAI awareness at all; would only replace the raw hyper-forwarding
  bits, which consolette doesn't need since it must forward through `Router::dispatch`
  and `Provider::send`, not do a byte-level HTTP proxy.
- **`openapi-to-rust`** — OpenAPI-spec-driven codegen that produces axum server
  scaffolding + typed models from official Anthropic/OpenAI OpenAPI specs. Interesting
  as a generator but it dictates its own typed request/response structs, which
  wouldn't match `Router::dispatch`'s `serde_json::Value` signature without an
  adapter layer, defeating the point of adopting it.

**Assessment**: no crate provides "bind a port, speak both Anthropic-native and
OpenAI-compatible, dispatch through an injectable router/provider abstraction" as a
library — every existing option bakes in its own upstream-selection logic (proxy to
OpenRouter, or proxy to a CLI subprocess) that would need to be gutted and replaced
with consolette's `Router`/`Provider`/`RoutingStrategy`/exec-auth stack. At that point
the "adoption" is a rewrite of everything except the axum route wiring, which is the
smallest and lowest-risk part of the task anyway.

**Verdict**: Not recommended. None of these narrow the actual task (axum handlers
calling `Router::dispatch` + SSE forwarding); they'd add a dependency whose own
routing/upstream logic must be entirely bypassed.

## 2. SaaS / managed gateway (LiteLLM proxy, Portkey, OpenRouter)

These are hosted or self-hosted *services* that themselves do multi-provider routing,
failover, and cost tracking — arguably the same problem statement as consolette.

**Assessment**: this misreads what consolette is for. Per
`project_plans/http-entrypoint/requirements.md`, consolette is a **local,
loopback-only proxy** whose value is (a) config-driven routing/failover already built
against ADR-003, (b) exec/plugin credential-helper auth (ADR-007) resolving secrets
from local keychains/1Password/etc. without ever putting them in a third-party's
hands, and (c) a `CostTracker` tied to the user's own local session data. Routing
traffic through LiteLLM's proxy or Portkey would mean either:

- Running LiteLLM/Portkey *instead of* consolette — but then consolette's own
  `Router`, `Provider` impls, exec-auth, and cost tracker are unused, which
  defeats the reason this codebase exists. This isn't a component decision, it's
  "do we still build consolette," which is outside this research question's frame
  (the requirements doc already answers that: yes).
- Running LiteLLM/Portkey *behind* consolette as one more upstream `Provider` — viable
  as a *future* provider backend, but orthogonal to this task, which is about
  consolette's own listening socket, not about adding a new upstream target.
- Using OpenRouter as a hosted multi-provider backend — same as above, a provider
  choice, not a replacement for consolette needing to bind its own port.

None of these obviate the need for *some* process to bind 127.0.0.1:47000 and speak
to local clients, because that's the seam ADR-007's exec-auth and the local
`CostTracker` depend on staying in-process.

**Verdict**: Not recommended as a replacement for building the HTTP layer. Viable only
as a *future* candidate for a new `Provider` backend (out of scope here).

## 3. LLM-generated/hand-rolled implementation vs. a battle-tested schema crate

The question is narrower than "hand-roll everything": the translation functions and
`Router::dispatch` already exist, are unit-tested, and operate on `serde_json::Value`.
The remaining work is: axum handlers, `serde_json::Value` extraction, SSE
(`axum::response::sse::Sse` / `axum-extra`'s SSE helpers, both already dependencies),
and forwarding upstream SSE frames.

Crates surveyed for typed schema replacement:

- **`async-openai`** — OpenAI-schema-centric; no native Anthropic Messages support.
  Its `byot` ("bring your own type") feature lets you pass `serde_json::Value`
  directly, which only re-derives what consolette already has.
- **`async-anthropic`**, **`anthropic-rs`**, **`anthropic-api`**, **`anthropic-ai-sdk`**,
  **`anthropic-sdk-rust`**, **`rusty-anthropic`**, **`anthropic-types`** — all
  unofficial, community-maintained, varying completeness ("quickly drafted,"
  "several features are missing" per one project's own README), and all are
  *client* SDKs (calling out to Anthropic), not server-side request/response schema
  crates designed to be deserialized from an inbound HTTP request body. Adopting one
  would mean converting `Router::dispatch`'s `serde_json::Value` into the crate's
  request struct and back — a translation layer *in addition to* the existing
  hand-rolled translation functions, not instead of them.

**Assessment**: because `Router::dispatch` and the translation functions already
commit to `serde_json::Value` as the interchange type, introducing a typed
third-party SDK crate at the HTTP boundary adds a conversion step without removing
any existing code or risk — the existing hand-rolled `translate_openai_to_anthropic`/
`translate_anthropic_to_openai` are staying regardless (they're explicitly in scope,
already unit-tested, out of scope to replace per the requirements doc). The
higher-risk surface flagged in the requirements doc's Rabbit Holes — SSE
streaming-failover boundary correctness and cost-tracking on partial streams — is
not schema-shaped work a client SDK would help with; it's plumbing between
`Router::dispatch`'s stream and axum's `Sse` response, which no surveyed crate
addresses (they're all *outbound* SSE clients, not inbound SSE re-emission helpers).

**Verdict**: Recommended to continue hand-rolling the HTTP-layer schema/streaming code
directly against `serde_json::Value` and axum/axum-extra's existing SSE support, per
the requirements doc's own constraint that no new HTTP framework dependency is
expected. A dedicated Anthropic/OpenAI client SDK crate is Not recommended for this
task; each would need to be limited to the future scope of item 2 above (new
`Provider` backends) if ever pulled in, not the HTTP-binding layer.

## 4. Fork or adapt an existing local-proxy project

Candidates: `claude-code-router` (TypeScript, not Rust — wrong language, would need a
rewrite anyway), `litellm-rust`/LiteLLM-Labs (explicitly marked by its own README as
"a proof of concept repo, the official LiteLLM is now moving to rust" — i.e.,
abandoned/superseded, not something to build on), `anthropic-proxy-rs` and
`claude-max-api-proxy-rs` (both single-direction/single-upstream-shape proxies, as in
section 1).

**Assessment**: every candidate assumes its own upstream-selection/auth model
(OpenRouter passthrough, or shelling to the `claude` CLI). Consolette already has a
`Router`/`RoutingStrategy`/`HealthRegistry`/exec-auth stack (ADR-003, ADR-007) that
is explicitly required to be the dispatch path (see Constraints in the requirements
doc: "Must dispatch through the existing `Router::dispatch`... rather than talking to
`Provider`s directly"). Forking any of these means deleting their routing/auth core
and grafting in consolette's — at which point nothing of substance is being reused
except the general shape of "axum + two route families + SSE," which is not enough
surface area to justify fork overhead (upstream license/attribution tracking, drift
on future `axum` upgrades, unfamiliar code to a maintainer who already knows
consolette's own conventions).

**Verdict**: Not recommended.

## Summary table

| Option | Verdict |
|---|---|
| 1. OSS proxy-scaffolding crate | Not recommended |
| 2. SaaS/managed gateway (LiteLLM proxy, Portkey, OpenRouter) | Not recommended as replacement; Viable only as a future `Provider` backend (separate task) |
| 3. Hand-rolled axum + `serde_json::Value` (current plan) | Recommended |
| 3b. Third-party Anthropic/OpenAI client SDK crate for schema types | Not recommended for this task |
| 4. Fork an existing local-proxy project | Not recommended |

**Bottom line**: build the HTTP-binding layer from scratch on axum/axum-extra, as the
requirements doc already assumes. No surveyed OSS crate, SaaS gateway, or fork
candidate reduces the actual remaining work — axum route handlers, SSE
plumbing/failover-boundary handling, and `CostTracker` wiring at the
`Router::dispatch` seam — without introducing an upstream-routing/auth model that
would need to be ripped out and replaced with consolette's own ADR-003/ADR-007 stack
anyway.
