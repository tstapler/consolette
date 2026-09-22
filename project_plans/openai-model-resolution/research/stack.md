# Research: Stack — libraries, versions, patterns

Scope: what Rust crates/versions/patterns apply to (a) Responses API SSE/HTTP handling, (b) per-upstream resolved-model caching with failure-triggered invalidation, (c) classifying OpenAI-style errors. Bottom line: **no new dependencies are needed.** Every mechanism this feature needs already has a shipped, tested precedent in this repo — the work is applying the existing patterns to a new call site, not sourcing new crates.

## What's already in `Cargo.toml` (resolved versions from `Cargo.lock`)

| Crate | Declared | Locked | Role today |
|---|---|---|---|
| `reqwest` | `0.12` (`default-features = false`, `json`/`stream`/`rustls-tls`) | `0.12.28` | HTTP client for `OpenaiProvider` (`src/providers/openai.rs:129-292`). A second `reqwest` `0.13.4` also appears in `Cargo.lock` — transitive-only (not a direct dep of this crate; likely pulled in via `rmcp` or `aws-sdk-*`), not something to touch. |
| `eventsource-stream` | `0.2` | `0.2.3` | SSE parsing — `Eventsource::eventsource()` trait turns a `Stream<Item = Result<Bytes, _>>` into `eventsource_stream::EventStream<S>` (`src/providers/openai.rs:22,412,443`). |
| `moka` | `0.12` (features `future`, `sync`) | `0.12.16` | Caching. Repo already has 6 usages; `sync::Cache` is used specifically where a caller needs synchronous, non-async `get`/`insert`/`invalidate` (`src/providers/openrouter/cache.rs`), `future::Cache` elsewhere (`src/memory/store.rs`). |
| `dashmap` | `6` | `6.2.1` | Concurrent maps for hand-rolled TTL/verdict caches (`src/routing/capability.rs`'s `CapabilityCache`, `openrouter/cache.rs`'s `recent_not_found` sliding window). |
| `tracing` | `0.1` | `0.1.44` | Structured logging for cache state transitions, resolution attempts, exhaustion. |
| `arc-swap` | `1.9.2` | — | Used for hot-swappable router state (`run_eval_loop(dispatch_router: Arc<ArcSwap<Router>>)` in `src/routing/capability.rs:305`) — relevant if resolution state needs to survive a config hot-reload the way capability verdicts do. |
| `backoff` (`tokio` feature), `tokio-retry` | `0.4`, `0.3` | — | Already-present retry crates, currently *not* wired into `openai.rs`'s error paths — available if candidate-probing needs bounded retry, but the existing `CapabilityCache`/`ModelListCache` patterns don't use them (they rely on "record failure, invalidate, let the next real request retry" rather than an in-call retry loop), so probably not needed here either. |
| `uuid` | `1` (`v4`) | — | Already used for streaming message ids (`format!("msg_{}", uuid::Uuid::new_v4())`, `openai.rs:444`) — Responses API translation will need the same for synthetic ids. |

No crate search or version bump is warranted for any of (a)/(b)/(c) below. This directly satisfies the requirements doc's Risk Control framing ("additive," not a new-dependency surface) and the constraint that static-pin upstreams must stay zero-overhead.

## (a) HTTP/SSE handling for a second wire format (Responses API)

**Pattern to extend, not a new dependency.** `OpenaiProvider` (`src/providers/openai.rs`) already has the exact three-method shape this needs:
- `send_request` (non-stream POST + `map_error_status`)
- `send_streaming_request` (POST with `stream["stream"] = true`, returns raw `reqwest::Response`)
- `OpenaiToAnthropicStream<S>` (wraps `.bytes_stream().map(...)` in `.eventsource()`, implements `Stream` for Anthropic-shaped SSE frames)

Responses API groundwork:
- Add sibling methods (e.g. `send_responses_request` / `send_responses_streaming_request`) hitting `POST {base_url}/v1/responses` instead of `/v1/chat/completions`, reusing `build_headers`, `map_error_status`, and the same `client`/`stream_client` split (ADR-004's pool-exhaustion rationale for a dedicated non-pooled SSE client applies identically to the new endpoint).
- The Responses API's SSE taxonomy (`response.output_item.added`, `response.output_text.delta`, `response.completed`, etc. — confirmed against OpenAI's public Responses API docs as of the last verified check; the exact event list should be re-confirmed against `platform.openai.com/docs/api-reference/responses-streaming` since this is the fastest-moving part of OpenAI's API surface) is still standard `text/event-stream`, so `eventsource_stream::Eventsource` is reused unchanged — only the per-event `match` arm logic inside a new `ResponsesToAnthropicStream<S>` (mirroring `OpenaiToAnthropicStream`) differs, keyed on `event.event` (Responses API's SSE frames carry a named `event:` field, unlike Chat Completions' undifferentiated `chat.completion.chunk` deltas) rather than parsing `data` alone.
- `serde_json::Value` dynamic parsing (the repo's existing style — no `derive(Deserialize)` wire structs for OpenAI shapes in `openai.rs`/`mod.rs`) is consistent with how `translate_anthropic_request_to_openai` etc. already work; no new serde derive machinery needed. Given the requirements doc's own warning (Rabbit Holes) that the Responses wire shape is structurally different (typed `input`/`output` item arrays, not `messages`/`choices`), a new module (e.g. `src/providers/openai_responses.rs` or a `responses` submodule) is a cleaner split than growing `openai.rs` further — matches the existing precedent of `src/providers/openrouter/` and `src/providers/gemini/` as sibling provider-specific modules rather than one giant file.
- Reasoning-item passthrough and tool_use↔function_call translation are pure `serde_json::Value` tree transforms — same style as the existing `translate_anthropic_request_to_openai`/`translate_openai_response_to_anthropic` pair in `src/providers/mod.rs`.

## (b) Caching a per-upstream resolved-model choice, invalidated on failure not fixed TTL

Two directly-reusable precedents already exist for exactly this shape, and both point at the same crate choice:

1. **`src/providers/openrouter/cache.rs`'s `ModelListCache`** — `moka::sync::Cache<(), Arc<Vec<FreeModelEntry>>>` for a single-entry, TTL-*backstopped* (not TTL-*driven*) cache, plus a `DashMap<String, Instant>` sliding window (`recent_not_found`, pruned to a 60s `NOT_FOUND_WINDOW`) to distinguish "this one candidate looks stale" from "everything just broke" (systemic-vs-minority classification) before deciding to invalidate. `record_not_found_and_maybe_invalidate` is the direct template for "cache invalidates on observed failure, not blind TTL," including the single-flight re-fetch guard (`Arc<AtomicBool>` compare-exchange, `tokio::spawn`) so a burst of concurrent failures doesn't stampede re-resolution.
2. **`src/routing/capability.rs`'s `CapabilityCache`** — a hand-rolled `DashMap<String, (CapabilityVerdict, Instant)>` with TTL-on-read (`get()` treats an expired entry as absent) — simpler than `moka` because it needs a *per-key* verdict+timestamp pair, not a single-slot snapshot. `error_verdict()` is the direct template for part (c) below, and `CapabilityVerdict::{Pass,Fail,Unknown}` (fail-open on ambiguous errors) is the exact three-way distinction the requirements doc's Rabbit Holes section asks for ("deprecated" vs "transient" vs "wrong endpoint" must not collapse into one bit).

**Recommendation for the new per-upstream resolved-model cache**: model it on `CapabilityCache`'s `DashMap<UpstreamKey, (ResolvedModel, Instant)>` shape rather than `ModelListCache`'s single-slot `moka::sync::Cache` — the resolution cache is keyed per-upstream (potentially many concurrent upstreams, unlike the single global free-model list `ModelListCache` guards), so a `DashMap` keyed by upstream name (already the pattern `HealthRegistry`/`recent_not_found` use) fits better than `moka`'s single-key builder pattern. `moka::sync::Cache` remains a fine choice if a TTL *backstop* (independent of failure-driven invalidation, matching this feature's own NFR: "cache invalidation triggers on the cached choice starting to fail, not on a fixed TTL alone") is still wanted as a belt-and-suspenders bound — `ModelListCache` demonstrates that combination (TTL backstop + explicit `invalidate()` call on failure) directly. Either `dashmap` or `moka::sync` already ships in `Cargo.toml`; no version bump needed either way (`dashmap = "6"` / `moka = { version = "0.12", features = ["future", "sync"] }`).

**Zero-overhead-for-static-pins constraint**: both precedents show the right shape already — the cache/lookup is only consulted on the opt-in code path (`ModelListCache` only exists on `OpenrouterProvider`; static-`model` upstreams never touch it). The new resolution cache should live behind the same kind of `Option<Arc<ResolutionCache>>` on `OpenaiProvider`, populated only when `RouteUpstreamRef` carries the new opt-in field — a `None` short-circuits before any lookup, matching the Constraints section's "no per-request overhead for upstreams that don't opt in."

## (c) Classifying OpenAI-style errors (deprecated vs transient vs wrong-endpoint)

**Current state (gap to close):** `map_error_status` (`src/providers/openai.rs:297-331`) and `send_streaming_request`'s inline duplicate (`openai.rs:263-289`) only inspect the HTTP status code:
- `429` → `ProviderError::RateLimited` (drops the JSON body entirely — no `error.type`/`error.code` parsed)
- any other 4xx → `ProviderError::Validation(body_str, status)` — `body_str` is the *raw* response text, not parsed JSON; nothing downstream currently looks at OpenAI's `error.type`/`error.code`/`error.message` fields.
- other non-2xx → `ProviderError::Upstream { status, body }`

So today there is no code path anywhere in the repo that parses an OpenAI error envelope (`{"error": {"message", "type", "code", "param"}}`) — this is genuinely new logic, not an extension of existing parsing, but the *classification pattern* (map an error into a small enum with an explicit "don't know, don't exclude" state) is exactly `src/routing/capability.rs`'s `error_verdict()` (`openai.rs:204-218` in that file... actually `capability.rs:204-218`), reproduced here as the template:

```rust
pub fn error_verdict(error: &ProviderError) -> CapabilityVerdict {
    match error {
        ProviderError::Validation(message, _) | ProviderError::ModelUnsupported(message) => {
            CapabilityVerdict::Fail { reason: format!("upstream rejected the model id: {message}") }
        }
        ProviderError::Upstream { status, body } if (400..500).contains(status) => {
            CapabilityVerdict::Fail { reason: format!("upstream {status}: {body}") }
        }
        _ => CapabilityVerdict::Unknown,
    }
}
```

For model-resolution's own classifier, this needs to go one level deeper than `capability.rs` does: not just 4xx-vs-not, but distinguishing within 4xx bodies by OpenAI's actual `error.type`/`error.code`/`message` text, since the requirements doc's concrete triggering incident had **two different 4xx shapes that must NOT be treated the same**:
- deprecated-model 400 (`error.type == "invalid_request_error"`, message containing `"has been deprecated"`) → *should* advance to the next candidate.
- wrong-endpoint 404 (message containing `"not supported in the v1/chat/completions endpoint"` / `"Use the v1/responses endpoint instead"`) → this is a *capability* signal (route to Responses API), not "model is dead" — advancing the candidate list on this would be wrong; the correct action is retrying the same model against `/v1/responses`.
- a genuine rate-limit/5xx must stay `Unknown`/transient, matching `CapabilityVerdict::Unknown`'s fail-open philosophy already established in this repo.

**Concretely**: `map_error_status` needs to additionally `serde_json::from_str::<Value>(&body_str)` and pull `.get("error").get("type")`/`.get("code")`/`.get("message")` before constructing `ProviderError::Validation` — either by widening `ProviderError::Validation`'s payload (adding a structured field) or by doing the OpenAI-error-envelope parsing at the call site in the new resolution module (keeping `ProviderError` provider-agnostic, which is more consistent with how `ProviderError` is currently shared across `gemini`/`openrouter`/`bedrock`/`openai` — see `src/providers/gemini/error.rs`'s precedent of doing provider-specific error-body parsing in a provider-specific module rather than in the shared enum). No library is needed for this — `serde_json::Value` ad-hoc parsing (the repo's dominant style) is sufficient; there's no evidence anywhere in the repo of a schema/derive-based error type for any upstream's error envelope, so introducing one here (e.g. a `schemars`/typed-error crate) would be inconsistent with the rest of the codebase's parsing style.

No new crate is needed for classification logic itself — this is a pure `match`/pattern-matching problem on a parsed `serde_json::Value`, same as everywhere else the translators already work (`translate_openai_response_to_anthropic`, `map_openai_finish_reason`, etc. in `src/providers/mod.rs`).

## `max_tokens` → `max_completion_tokens`

Not a library question — `src/providers/mod.rs:691-741`'s `translate_anthropic_request_to_openai` already has the single call site (`body["max_tokens"] = Value::from(max_tokens)`, line ~740-741) where a family/model-id-conditional branch (or a probed-and-cached flag, per the Open Questions in requirements.md) would swap the key. No new dependency; whatever signal decides the branch (static family-prefix table vs. probed-and-flipped-on-a-specific-error) is a data/logic design question for Phase 3, not a stack question — the existing `CapabilityCache`/`ModelListCache` machinery above is reusable verbatim if the answer is "probe and cache," since it would be the same DashMap-keyed-by-model-id shape as the resolution cache in (b).

## Summary of concrete recommendations

1. **No `Cargo.toml` changes.** `reqwest 0.12.28`, `eventsource-stream 0.2.3`, `moka 0.12.16`, `dashmap 6.2.1` (all already locked) cover HTTP/SSE, caching, and concurrent-map needs.
2. **Responses API module**: new sibling module (not a `Cargo.toml` addition) reusing `OpenaiProvider`'s `client`/`stream_client`/`build_headers`/`map_error_status`, with a new `ResponsesToAnthropicStream<S>` built the same way as `OpenaiToAnthropicStream<S>` (wrap `.bytes_stream().eventsource()`, `impl Stream`).
3. **Resolution cache**: `DashMap<String, (ResolvedModel, Instant)>` keyed by upstream name, modeled on `CapabilityCache`, not `ModelListCache`'s single-slot `moka` cache — per-upstream keying fits the multi-upstream case better than a singleton.
4. **Error classification**: extend `map_error_status` (or a new sibling function) to parse the OpenAI JSON error envelope (`error.type`/`error.code`/`message`) — genuinely new logic (no existing OpenAI-error-body parser in the repo), but templated directly on `src/routing/capability.rs`'s `error_verdict()` three-way `Fail`/`Unknown`(transient)/new-fourth-case(wrong-endpoint→retry-as-Responses) pattern.
