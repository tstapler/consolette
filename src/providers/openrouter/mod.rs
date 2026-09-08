//! `OpenrouterProvider`: `Provider` impl for `OpenRouter`'s OpenAI-compatible
//! Chat Completions API, plus free-model discovery (`list_free_models`) that
//! backs `OpenrouterScoringStrategy` (Epic 3+) and the money-safety cache
//! this provider owns as an invariant of its own construction (Story 2.1.2's
//! Blocker-1 fix — see plan.md's Risk Control, "Money-safety backstop"
//! mechanism 1).
//!
//! Mirrors `OpenaiProvider`'s two-`reqwest::Client` shape (ADR-004) and
//! `GeminiProvider`/`AnthropicProvider`'s hardcoded-`base_url` precedent:
//! `UpstreamKind::Openrouter` carries no configurable base URL, unlike
//! `UpstreamKind::Openai` (see Domain Glossary).

pub mod cache;
mod models;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures_core::Stream;
use futures_util::StreamExt;
use http::HeaderMap;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, warn};

use crate::auth::exec::ExecCredentialCache;
use crate::auth::SecretResolver;
use crate::config::schema::Upstream;

use super::anthropic::apply_auth_headers;
use super::{map_openai_finish_reason, ModelInfo, Provider, ProviderError, ProviderResponse};

pub use cache::{FreeModelEntry, ModelListCache};

/// Base URL for `OpenRouter`'s OpenAI-compatible API. Hardcoded, matching
/// `AnthropicProvider`/`GeminiProvider`'s precedent — `UpstreamKind::Openrouter`
/// carries no configurable `base_url` field (see plan.md's Domain Glossary).
const BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Static attribution headers `OpenRouter`'s docs ask API consumers to send
/// (openrouter.ai/docs) so usage shows up correctly attributed in their
/// dashboard (Task 1.2.1c).
const HTTP_REFERER: &str = "https://github.com/tstapler/consolette";
const X_TITLE: &str = "consolette";

/// How often the background task (Story 2.1.2) refreshes `model_cache`,
/// well under `cache::MODEL_LIST_TTL` (15 minutes) so in normal operation
/// the TTL is a backstop, not the primary refresh driver.
const MODEL_LIST_REFRESH_INTERVAL: Duration = Duration::from_mins(5);

/// Provider for `OpenRouter`'s OpenAI-compatible Chat Completions API.
pub struct OpenrouterProvider {
    /// Pooled client for non-streaming requests.
    client: Client,
    /// Non-pooled client for SSE streaming (prevents pool exhaustion, ADR-004).
    stream_client: Client,
    /// The upstream this provider was constructed for — supplies `name`
    /// (for exec-cache keying) and `auth`.
    upstream: Arc<Upstream>,
    /// Resolves `SecretRef`s (env/keychain/inline) to plaintext.
    resolver: Arc<dyn SecretResolver + Send + Sync>,
    /// Shared cache for `exec` auth-method subprocess results.
    exec_cache: Arc<ExecCredentialCache>,
    /// Free-model list cache. Populated eagerly by `new()` before it
    /// returns — an invariant of construction, not of any particular
    /// `Strategy` choosing to use it (architecture-review Blocker 1 fix;
    /// see plan.md's Risk Control, money-safety mechanism 1). Full
    /// TTL/background-refresh logic lands in Epic 2.1 — see `cache.rs`'s
    /// module doc comment.
    model_cache: Arc<ModelListCache>,
}

impl OpenrouterProvider {
    /// Construct a new `OpenrouterProvider` for one configured `Upstream`.
    ///
    /// Returns `Arc<Self>` rather than a bare `Self` — a deliberate
    /// divergence from `OpenaiProvider::new`/`GeminiProvider::new` — built
    /// via `Arc::new_cyclic` so `model_cache` can hold a `Weak<Self>`
    /// back-reference (Story 2.1.2), which Story 2.1.3's on-demand refetch
    /// trigger needs to call back into `list_free_models()` without the
    /// `RoutingStrategy`/`Router` orchestrating it.
    ///
    /// Eagerly populates `model_cache` before returning (architecture-review
    /// Blocker 1 fix, Story 2.1.2): a failed refresh is logged, not
    /// propagated, so a transient `OpenRouter` outage at startup doesn't
    /// fail the whole process — `model_cache.snapshot()` is simply `None`
    /// until a later refresh succeeds. This runs unconditionally for every
    /// `openrouter`-kind upstream `build_providers` constructs, regardless
    /// of which `Strategy` (if any) later references it — see plan.md's
    /// "Cache-population lifecycle ownership" Pattern Decision.
    ///
    /// Also spawns a background task that refreshes `model_cache` every
    /// `MODEL_LIST_REFRESH_INTERVAL`, holding only `Weak<ModelListCache>` +
    /// `Weak<Self>` so it stops (rather than leaking forever) once a route
    /// hot-swap orphans this provider.
    ///
    /// # Errors
    ///
    /// Returns `Err` if either `reqwest::Client` fails to build.
    pub async fn new(
        upstream: Arc<Upstream>,
        resolver: Arc<dyn SecretResolver + Send + Sync>,
        exec_cache: Arc<ExecCredentialCache>,
        request_timeout_secs: u64,
    ) -> anyhow::Result<Arc<Self>> {
        let timeout = Duration::from_secs(request_timeout_secs);

        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(timeout)
            .build()?;

        // ADR-004: separate client with pool_max_idle_per_host(0) for SSE.
        let stream_client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(0)
            .build()?;

        let provider = Arc::new_cyclic(|weak_self| Self {
            client,
            stream_client,
            upstream,
            resolver,
            exec_cache,
            model_cache: Arc::new(ModelListCache::new(weak_self.clone())),
        });

        if let Err(e) = provider.model_cache.refresh(provider.as_ref()).await {
            warn!(
                error = %e,
                "openrouter: eager model-cache refresh failed at construction; \
                 model_cache.snapshot() will be None until a later refresh succeeds"
            );
        }

        spawn_background_refresh_task(&provider);

        Ok(provider)
    }

    /// Test-only bypass of `new()`'s async construction (which performs a
    /// live network call as an invariant — see `new()`'s doc comment): most
    /// unit tests in this module only need a provider instance to exercise
    /// header-building/error-classification, not a populated cache, and
    /// must not depend on live network access. Mirrors
    /// `OpenaiProvider::new`'s test module, which never needs this bypass
    /// only because `OpenaiProvider::new` itself never makes a network call.
    #[cfg(test)]
    #[allow(clippy::expect_used)]
    fn test_provider() -> Self {
        Self::test_provider_with_upstream(Upstream {
            name: "test-openrouter".to_string(),
            kind: crate::config::schema::UpstreamKind::Openrouter {},
            auth: Some(crate::config::schema::AuthMethod::Bearer {
                token: crate::config::schema::SecretRef::Inline {
                    value: "sk-or-v1-test".to_string(),
                },
            }),
        })
    }

    /// Like [`Self::test_provider`], but with a caller-supplied `Upstream`
    /// (e.g. a broken `AuthMethod::Exec` so `build_headers`/`refresh()`
    /// fail hermetically, without live network access — see
    /// `cache.rs`'s `broken_auth_provider` test helper).
    #[cfg(test)]
    #[allow(clippy::expect_used)]
    pub(crate) fn test_provider_with_upstream(upstream: Upstream) -> Self {
        Self {
            client: Client::builder().build().expect("client should build"),
            stream_client: Client::builder().build().expect("client should build"),
            upstream: Arc::new(upstream),
            resolver: Arc::new(crate::auth::SystemSecretResolver),
            exec_cache: Arc::new(ExecCredentialCache::new()),
            model_cache: Arc::new(ModelListCache::new_with_ttl(cache::MODEL_LIST_TTL)),
        }
    }

    /// Cheap `Arc::clone` accessor for `model_cache`: lets
    /// `ModelListCache::trigger_immediate_refresh` (Story 2.1.3) get back
    /// from an upgraded `Weak<Self>` to the cache it should refresh, and
    /// will let `OpenrouterScoringStrategy` (Epic 3+) share the same cache
    /// `send()` verifies against.
    #[must_use]
    pub fn model_cache(&self) -> Arc<ModelListCache> {
        Arc::clone(&self.model_cache)
    }

    /// Build the outgoing request headers: `Content-Type` plus auth per the
    /// upstream's configured `AuthMethod`, plus `OpenRouter`'s static
    /// attribution headers (Task 1.2.1c).
    async fn build_headers(&self, url: &str) -> Result<HeaderMap, ProviderError> {
        let mut out = HeaderMap::new();
        out.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        apply_auth_headers(
            &self.upstream,
            self.resolver.as_ref(),
            &self.exec_cache,
            &mut out,
            url,
        )
        .await?;
        out.insert(
            reqwest::header::HeaderName::from_static("http-referer"),
            reqwest::header::HeaderValue::from_static(HTTP_REFERER),
        );
        out.insert(
            reqwest::header::HeaderName::from_static("x-title"),
            reqwest::header::HeaderValue::from_static(X_TITLE),
        );
        Ok(out)
    }

    /// Send a non-streaming request to `POST /chat/completions`.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if auth resolution, the HTTP request, or
    /// upstream error-status mapping fails.
    pub async fn send_request(&self, body: Value) -> Result<Value, ProviderError> {
        let model_id = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let url = format!("{BASE_URL}/chat/completions");
        let headers = self.build_headers(&url).await?;
        let body_bytes = serde_json::to_vec(&body).map_err(|e| ProviderError::Upstream {
            status: 0,
            body: e.to_string(),
        })?;

        debug!("OpenRouter non-stream POST {url}");

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .body(body_bytes)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ProviderError::Timeout
                } else {
                    ProviderError::Upstream {
                        status: 0,
                        body: e.to_string(),
                    }
                }
            })?;

        let status = response.status();
        if !status.is_success() {
            return Err(classify_error_response(status, response, Some(&model_id)).await);
        }

        let value: Value = response.json().await.map_err(|e| ProviderError::Upstream {
            status: status.as_u16(),
            body: e.to_string(),
        })?;

        self.check_for_unexpected_cost(&model_id, &value);

        Ok(value)
    }

    /// Send a streaming request to `POST /chat/completions`.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if auth resolution, the HTTP request, or
    /// upstream error-status mapping fails.
    pub async fn send_streaming_request(
        &self,
        mut body: Value,
    ) -> Result<reqwest::Response, ProviderError> {
        let model_id = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        body["stream"] = Value::Bool(true);

        let url = format!("{BASE_URL}/chat/completions");
        let headers = self.build_headers(&url).await?;
        let body_bytes = serde_json::to_vec(&body).map_err(|e| ProviderError::Upstream {
            status: 0,
            body: e.to_string(),
        })?;

        debug!("OpenRouter stream POST {url}");

        let response = self
            .stream_client
            .post(&url)
            .headers(headers)
            .body(body_bytes)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ProviderError::Timeout
                } else {
                    ProviderError::Upstream {
                        status: 0,
                        body: e.to_string(),
                    }
                }
            })?;

        let status = response.status();
        if !status.is_success() {
            return Err(classify_error_response(status, response, Some(&model_id)).await);
        }

        Ok(response)
    }

    /// Money-safety backstop mechanism 3 (plan.md Risk Control): would
    /// hard-invalidate `model_cache` and log at `error!` if `OpenRouter`'s
    /// response reported a nonzero cost for a request whose model was drawn
    /// from the free-model cache.
    ///
    /// Currently a **documented no-op**. Task 1.2.4a's research spike found
    /// that live network access *was* available in this sandboxed
    /// environment (confirmed: an unauthenticated `GET /models` succeeded,
    /// and an invalid-bearer-token `POST /chat/completions` returned a real
    /// `{"error":{"message":"User not found.","code":401}}`), but no valid
    /// `OpenRouter` API key was available to exercise the one candidate this
    /// story needed to check — a request-time `usage: {include: true}`
    /// opt-in surfacing `usage.cost` in a *successful* response — without
    /// spending real money, which is out of scope for research capture.
    /// So: no accessible per-request cost field confirmed as of 2026-09-07
    /// in this sandboxed environment; see plan.md Story 1.2.4 / Risk
    /// Control — Tyler must sign off on this residual risk before relying
    /// on mechanism 3 of the money-safety backstop.
    // `&self` is unused today (documented no-op), but this will become a
    // real `self.model_cache.invalidate()` call once Task 1.2.4a's field is
    // confirmed — keeping the method signature `&self`-shaped now avoids
    // another call-site churn later.
    #[allow(clippy::unused_self)]
    fn check_for_unexpected_cost(&self, _model_id: &str, _response: &Value) {
        // Intentional no-op — see doc comment above and plan.md Story 1.2.4.
    }

    /// Fetch every model `OpenRouter` reports (`Provider::list_models`'s
    /// underlying implementation) — see `list_models` below.
    async fn fetch_models(&self) -> Result<Value, ProviderError> {
        models::fetch_models_raw(self).await
    }

    /// Fetch only the currently free (`pricing.prompt == pricing.completion
    /// == "0"`) models, carrying their (zero) price forward per-entry
    /// (Story 1.2.2's `FreeModelEntry` shape).
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] on auth failure or a non-2xx response
    /// from `OpenRouter`'s `/models` endpoint.
    pub async fn list_free_models(&self) -> Result<Vec<FreeModelEntry>, ProviderError> {
        let value = models::fetch_models_raw(self).await?;
        Ok(models::parse_free_model_entries(&value))
    }

    /// Pre-flight per-dispatch price recheck (Task 2.1.2c, money-safety
    /// backstop mechanism 2): verifies the *specific selected model's*
    /// cached price is `(0.0, 0.0)`, not just that its id is present in the
    /// free-model list — defense-in-depth against a future bug in
    /// `list_free_models()`'s own filter, not by itself a fix for the
    /// free→paid-mid-TTL gap (that's Story 1.2.4's post-hoc cost check).
    ///
    /// A `None` snapshot (cold cache) falls through and lets the real API
    /// call be the source of truth — reachable only for a session-pinned
    /// dispatch (an unpinned candidate at a cold cache is dropped earlier
    /// by `expand_candidates`, Story 4.2.4), so reaching this branch means
    /// a request is about to be forwarded with zero local price
    /// verification. Logged via `tracing::warn!` so that bypass is visible
    /// rather than indistinguishable from the normal, verified path
    /// (adversarial-review Concern, 2026-09-07 re-review).
    fn check_cached_price(&self, model: &str) -> Result<(), ProviderError> {
        let Some(list) = self.model_cache.snapshot() else {
            warn!(
                model,
                "openrouter: dispatching model with no cache snapshot to verify price against \
                 — price recheck bypassed"
            );
            return Ok(());
        };

        let is_verified_free = list
            .iter()
            .any(|e| e.id == model && e.price_prompt == 0.0 && e.price_completion == 0.0);
        if is_verified_free {
            Ok(())
        } else {
            Err(ProviderError::ModelUnsupported(model.to_string()))
        }
    }

    /// Feeds a dispatch error back into `model_cache`'s data-policy-vs-
    /// staleness invalidation (Story 2.1.3) when it's a model-not-found
    /// classification, then returns the error unchanged so call sites can
    /// keep using `?`/`map_err` normally.
    fn observe_dispatch_error(&self, err: ProviderError) -> ProviderError {
        if let ProviderError::ModelUnsupported(ref id) = err {
            self.model_cache.record_not_found_and_maybe_invalidate(id);
        }
        err
    }
}

/// Spawns `OpenrouterProvider::new()`'s background model-list refresh task
/// (Story 2.1.2): refreshes `model_cache` every `MODEL_LIST_REFRESH_INTERVAL`,
/// holding only `Weak<ModelListCache>` + `Weak<OpenrouterProvider>` so it
/// exits — rather than leaking for the process's lifetime — the first time
/// either fails to upgrade (e.g. a route hot-swap orphaned this provider).
fn spawn_background_refresh_task(provider: &Arc<OpenrouterProvider>) {
    let weak_cache = Arc::downgrade(&provider.model_cache);
    let weak_provider = Arc::downgrade(provider);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(MODEL_LIST_REFRESH_INTERVAL).await;
            let (Some(cache), Some(provider)) = (weak_cache.upgrade(), weak_provider.upgrade())
            else {
                break;
            };
            if let Err(e) = cache.refresh(&provider).await {
                warn!(error = %e, "openrouter model-list background refresh failed");
            }
        }
    });
}

/// The `OpenRouter` error envelope: `{"error": {"message": ..., "code": ...}}`.
///
/// VERIFIED live (Task 1.2.1a): `POST /chat/completions` with an invalid
/// bearer token returned exactly `{"error":{"message":"User not
/// found.","code":401}}` (captured 2026-09-07) — `code` mirrors the HTTP
/// status as an integer here, not a semantic string like `OpenAI`'s own
/// `"model_not_found"`. The model-not-found variant specifically could NOT
/// be captured live: every request in this sandboxed environment without a
/// valid API key hit the auth check first (confirmed above), and no valid
/// key was available to get past it without risking real spend. For that
/// one case only, `looks_like_model_not_found` below falls back to a
/// message-text heuristic based on `OpenRouter`'s documented behavior
/// (openrouter.ai/docs) as of 2026-09 — see plan.md Story 1.2.1's
/// Unresolved Question.
#[derive(Debug, Clone, Deserialize)]
struct OpenrouterErrorBody {
    error: OpenrouterErrorDetail,
}

#[derive(Debug, Clone, Deserialize)]
struct OpenrouterErrorDetail {
    message: String,
    #[allow(dead_code)] // carried for completeness; VERIFIED live to mirror the HTTP status, not read today
    #[serde(default)]
    code: Option<Value>,
}

/// Heuristic detection of `OpenRouter`'s model-not-found error message.
/// UNVERIFIED against a live authenticated call (see `OpenrouterErrorBody`'s
/// doc comment) — based on `OpenRouter`'s documented error behavior
/// (openrouter.ai/docs) as of 2026-09-07, describing an invalid `model`
/// field producing a 4xx response whose message names the model and says
/// it isn't a valid/available id.
fn looks_like_model_not_found(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("model")
        && (lower.contains("not found") || lower.contains("not a valid") || lower.contains("no endpoints"))
}

/// Extracts a `Retry-After` header value (whole seconds) from a 429
/// response's headers, mirroring `OpenaiProvider`'s/`GeminiProvider`'s
/// existing 429-handling shape. Pure/sync so it's directly unit-testable
/// against an in-memory `HeaderMap` (Task 1.2.1e) — no live/mocked HTTP
/// needed.
fn classify_rate_limit(headers: &HeaderMap) -> ProviderError {
    let retry_after = headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    match retry_after {
        Some(retry_after) => ProviderError::RateLimitedWithRetry { retry_after },
        None => ProviderError::RateLimited,
    }
}

/// Maps a parsed `OpenRouter` error body onto the shared `ProviderError`
/// vocabulary, mirroring `gemini::error::classify_gemini_error`'s
/// status-code branching style (`src/providers/gemini/error.rs:47-64`).
/// `model_id` is `Some(..)` only at a chat-completions call site (where a
/// model-not-found classification is meaningful); `/models` fetches pass
/// `None` since there's no specific model in that request.
#[must_use]
fn classify_openrouter_error(
    status: StatusCode,
    body: &OpenrouterErrorBody,
    model_id: Option<&str>,
) -> ProviderError {
    if let Some(model_id) = model_id {
        if looks_like_model_not_found(&body.error.message) {
            return ProviderError::ModelUnsupported(model_id.to_string());
        }
    }

    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return ProviderError::Auth(body.error.message.clone());
    }

    if status.is_client_error() {
        return ProviderError::Validation(body.error.message.clone(), status.as_u16());
    }

    ProviderError::Upstream {
        status: status.as_u16(),
        body: body.error.message.clone(),
    }
}

/// Converts a non-2xx `OpenRouter` `reqwest::Response` into a `ProviderError`,
/// consuming the response body for error detail. Thin async glue around the
/// pure `classify_rate_limit`/`classify_openrouter_error` functions above —
/// those, not this, are what Task 1.2.1e's unit tests exercise directly.
async fn classify_error_response(
    status: StatusCode,
    response: reqwest::Response,
    model_id: Option<&str>,
) -> ProviderError {
    if status == StatusCode::TOO_MANY_REQUESTS {
        return classify_rate_limit(response.headers());
    }

    let body_text = response.text().await.unwrap_or_default();
    match serde_json::from_str::<OpenrouterErrorBody>(&body_text) {
        Ok(body) => classify_openrouter_error(status, &body, model_id),
        Err(_) => {
            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                ProviderError::Auth(body_text)
            } else if status.is_client_error() {
                ProviderError::Validation(body_text, status.as_u16())
            } else {
                ProviderError::Upstream {
                    status: status.as_u16(),
                    body: body_text,
                }
            }
        }
    }
}

#[async_trait]
impl Provider for OpenrouterProvider {
    fn name(&self) -> &'static str {
        "openrouter"
    }

    async fn send(
        &self,
        body: Value,
        _headers: HeaderMap,
        stream: bool,
    ) -> Result<ProviderResponse, ProviderError> {
        // `Provider::send` always receives/returns Anthropic-wire-format
        // JSON at the trait boundary — translate to/from native
        // OpenAI-compatible shape here, reusing `OpenaiProvider`'s existing
        // translation helpers verbatim (Story 1.2.1's acceptance criterion:
        // "no new translation logic").
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        self.check_cached_price(&model)?;
        let openai_body = super::translate_anthropic_request_to_openai(&body);

        if stream {
            let response = self
                .send_streaming_request(openai_body)
                .await
                .map_err(|e| self.observe_dispatch_error(e))?;
            let byte_stream = response
                .bytes_stream()
                .map(|r| r.map_err(anyhow::Error::from));
            let translated = OpenrouterToAnthropicStream::new(byte_stream, model);
            Ok(ProviderResponse::Stream(Box::pin(translated)))
        } else {
            let value = self
                .send_request(openai_body)
                .await
                .map_err(|e| self.observe_dispatch_error(e))?;
            let anthropic_value = super::translate_openai_response_to_anthropic(&value);
            Ok(ProviderResponse::Full(anthropic_value))
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let value = self.fetch_models().await?;
        Ok(models::parse_model_infos(&value))
    }
}

/// Translates an `OpenRouter` (OpenAI-shaped) `chat.completion.chunk` SSE byte
/// stream into an Anthropic Messages SSE event stream. Deliberately a
/// near-verbatim duplicate of `openai::OpenaiToAnthropicStream` — that
/// struct is private to `openai.rs`, and `openai.rs` isn't one of this
/// epic's files (see plan.md's Files list for Story 1.2.1), so sharing it
/// would widen this epic's blast radius rather than shrink it. A follow-up
/// epic touching both providers could extract a shared translator if this
/// duplication becomes a maintenance burden.
struct OpenrouterToAnthropicStream<S> {
    inner: eventsource_stream::EventStream<S>,
    id: String,
    model: String,
    started: bool,
    finished: bool,
    done: bool,
    pending: std::collections::VecDeque<Bytes>,
}

impl<S> OpenrouterToAnthropicStream<S>
where
    S: Stream<Item = Result<Bytes, anyhow::Error>>,
{
    fn new(inner: S, model: String) -> Self {
        Self {
            inner: inner.eventsource(),
            id: format!("msg_{}", uuid::Uuid::new_v4()),
            model,
            started: false,
            finished: false,
            done: false,
            pending: std::collections::VecDeque::new(),
        }
    }

    fn frame(event: &str, data: &Value) -> Bytes {
        Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
    }

    fn ensure_started(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        self.pending.push_back(Self::frame(
            "message_start",
            &serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": self.id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": self.model,
                    "stop_reason": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            }),
        ));
        self.pending.push_back(Self::frame(
            "content_block_start",
            &serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            }),
        ));
    }

    fn push_delta(&mut self, text: &str) {
        self.ensure_started();
        self.pending.push_back(Self::frame(
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": text}
            }),
        ));
    }

    fn close(&mut self, stop_reason: &str) {
        if self.finished {
            return;
        }
        self.ensure_started();
        self.finished = true;
        self.pending.push_back(Self::frame(
            "content_block_stop",
            &serde_json::json!({"type": "content_block_stop", "index": 0}),
        ));
        self.pending.push_back(Self::frame(
            "message_delta",
            &serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason},
                "usage": {"output_tokens": 0}
            }),
        ));
        self.pending.push_back(Self::frame(
            "message_stop",
            &serde_json::json!({"type": "message_stop"}),
        ));
    }
}

impl<S> Stream for OpenrouterToAnthropicStream<S>
where
    S: Stream<Item = Result<Bytes, anyhow::Error>> + Unpin,
{
    type Item = Result<Bytes, anyhow::Error>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            if let Some(frame) = this.pending.pop_front() {
                return std::task::Poll::Ready(Some(Ok(frame)));
            }
            if this.done {
                return std::task::Poll::Ready(None);
            }
            if this.finished {
                this.done = true;
                continue;
            }

            match std::pin::Pin::new(&mut this.inner).poll_next(cx) {
                std::task::Poll::Pending => return std::task::Poll::Pending,
                std::task::Poll::Ready(None) => {
                    this.close("end_turn");
                }
                std::task::Poll::Ready(Some(Err(e))) => {
                    warn!(error = %e, "openrouter->anthropic stream translator: eventsource parse error");
                    this.close("end_turn");
                }
                std::task::Poll::Ready(Some(Ok(event))) => {
                    if event.data == "[DONE]" {
                        this.close("end_turn");
                        continue;
                    }
                    let Ok(parsed) = serde_json::from_str::<Value>(&event.data) else {
                        continue;
                    };

                    let choice = parsed
                        .get("choices")
                        .and_then(Value::as_array)
                        .and_then(|a| a.first());

                    if let Some(text) = choice
                        .and_then(|c| c.get("delta"))
                        .and_then(|d| d.get("content"))
                        .and_then(Value::as_str)
                    {
                        if !text.is_empty() {
                            this.push_delta(text);
                        }
                    }

                    if let Some(reason) = choice
                        .and_then(|c| c.get("finish_reason"))
                        .and_then(Value::as_str)
                    {
                        this.close(map_openai_finish_reason(Some(reason)));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// `FreeModelEntry` constructor for tests that just need one
    /// zero-priced entry seeded into `model_cache`.
    fn free_entry(id: &str) -> FreeModelEntry {
        FreeModelEntry {
            id: id.to_string(),
            price_prompt: 0.0,
            price_completion: 0.0,
        }
    }

    // REQ-1 (Story 1.2.1, Task 1.2.1e). Per this epic's established test
    // pattern deviation (no wiremock/mockito in this repo): header
    // construction is unit-tested directly, matching
    // `openai::tests::build_headers_sets_content_type`, rather than
    // spinning up a live network listener to inspect an outgoing request.

    #[tokio::test]
    async fn send_should_forward_request_with_auth_and_referer_headers() {
        let provider = OpenrouterProvider::test_provider();

        let headers = provider
            .build_headers(&format!("{BASE_URL}/chat/completions"))
            .await
            .unwrap();

        assert_eq!(
            headers.get(reqwest::header::AUTHORIZATION).unwrap(),
            "Bearer sk-or-v1-test"
        );
        assert_eq!(headers.get("http-referer").unwrap(), HTTP_REFERER);
        assert_eq!(headers.get("x-title").unwrap(), X_TITLE);
    }

    #[test]
    fn send_should_return_model_unsupported_when_model_not_found_response() {
        let body: OpenrouterErrorBody = serde_json::from_value(serde_json::json!({
            "error": {"message": "foo/bar:free is not a valid model ID", "code": 400}
        }))
        .unwrap();

        let err = classify_openrouter_error(StatusCode::BAD_REQUEST, &body, Some("foo/bar:free"));

        assert!(matches!(
            err,
            ProviderError::ModelUnsupported(model) if model == "foo/bar:free"
        ));
    }

    #[test]
    fn send_should_classify_rate_limit_with_and_without_retry_after() {
        let mut with_header = HeaderMap::new();
        with_header.insert("retry-after", "20".parse().unwrap());
        assert!(matches!(
            classify_rate_limit(&with_header),
            ProviderError::RateLimitedWithRetry { retry_after: 20 }
        ));

        let without_header = HeaderMap::new();
        assert!(matches!(
            classify_rate_limit(&without_header),
            ProviderError::RateLimited
        ));
    }

    #[test]
    fn classify_openrouter_error_should_return_auth_for_401() {
        let body: OpenrouterErrorBody = serde_json::from_value(serde_json::json!({
            "error": {"message": "User not found.", "code": 401}
        }))
        .unwrap();

        let err = classify_openrouter_error(StatusCode::UNAUTHORIZED, &body, None);

        assert!(matches!(err, ProviderError::Auth(msg) if msg == "User not found."));
    }

    // REQ-3 (Story 1.2.4, money-safety backstop) — documented no-op per
    // Task 1.2.4a's finding (see `check_for_unexpected_cost`'s doc
    // comment). Both tests confirm current behavior (no invalidation) so a
    // future implementer flips these assertions once a real cost field is
    // confirmed and wired up, rather than silently leaving stale tests.

    #[test]
    fn send_should_hard_invalidate_cache_on_nonzero_cost_for_free_model() {
        let provider = OpenrouterProvider::test_provider();
        provider.model_cache().seed_for_test(vec![free_entry("a/b:free")]);

        let response_with_nonzero_cost = serde_json::json!({
            "id": "gen-1",
            "model": "a/b:free",
            "choices": [],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "cost": 0.002}
        });

        provider.check_for_unexpected_cost("a/b:free", &response_with_nonzero_cost);

        // Documented no-op (Task 1.2.4a found no confirmed cost field in
        // this sandboxed environment) — cache is NOT invalidated today.
        assert!(
            provider.model_cache().snapshot().is_some(),
            "no-op check must not invalidate until a real cost field is confirmed and wired up"
        );
    }

    #[test]
    fn send_should_not_invalidate_cache_when_cost_is_zero_or_absent() {
        let provider = OpenrouterProvider::test_provider();
        provider.model_cache().seed_for_test(vec![free_entry("a/b:free")]);

        let ordinary_response = serde_json::json!({
            "id": "gen-2",
            "model": "a/b:free",
            "choices": [],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });

        provider.check_for_unexpected_cost("a/b:free", &ordinary_response);

        assert!(provider.model_cache().snapshot().is_some());
    }

    // REQ-3 (Story 2.1.2, Task 2.1.2c/d) — per-dispatch price recheck.

    #[tokio::test]
    async fn send_should_return_model_unsupported_when_cached_price_is_nonzero() {
        let provider = OpenrouterProvider::test_provider();
        provider.model_cache().seed_for_test(vec![FreeModelEntry {
            id: "a/b:paid".to_string(),
            price_prompt: 0.000_002,
            price_completion: 0.000_004,
        }]);

        let err = provider
            .check_cached_price("a/b:paid")
            .expect_err("a cached nonzero price must be rejected without a network call");

        assert!(matches!(
            err,
            ProviderError::ModelUnsupported(model) if model == "a/b:paid"
        ));
    }

    #[test]
    fn check_cached_price_should_reject_model_absent_from_cache() {
        let provider = OpenrouterProvider::test_provider();
        provider.model_cache().seed_for_test(vec![free_entry("a/b:free")]);

        let err = provider
            .check_cached_price("not-in-list:free")
            .expect_err("a model absent from the cached list must be rejected");

        assert!(matches!(
            err,
            ProviderError::ModelUnsupported(model) if model == "not-in-list:free"
        ));
    }

    #[test]
    fn check_cached_price_should_pass_through_on_cold_cache() {
        // A `None` snapshot (cold cache) is the only case Task 2.1.2c
        // explicitly falls through on — the real API call becomes the
        // source of truth. This is reachable only for a session-pinned
        // dispatch in production (`expand_candidates` drops an unpinned
        // candidate at a cold cache first), so it also logs a
        // `tracing::warn!` naming the model — see `check_cached_price`'s
        // doc comment; asserting on emitted log output isn't covered here
        // (this repo doesn't have a tracing-test-style dev-dependency), so
        // this test covers the behavioral half of that acceptance
        // criterion only.
        let provider = OpenrouterProvider::test_provider();
        assert!(provider.model_cache().snapshot().is_none());

        assert!(provider.check_cached_price("anything:free").is_ok());
    }

    // REQ-3 (Story 2.1.3) — `send()` wires a `ModelUnsupported` classification
    // into the cache's data-policy-vs-staleness invalidation.

    #[tokio::test]
    async fn observe_dispatch_error_should_record_not_found_for_model_unsupported() {
        let provider = OpenrouterProvider::test_provider();
        provider
            .model_cache()
            .seed_for_test(vec![free_entry("only/model:free")]);

        let returned = provider.observe_dispatch_error(ProviderError::ModelUnsupported(
            "only/model:free".to_string(),
        ));

        assert!(matches!(returned, ProviderError::ModelUnsupported(model) if model == "only/model:free"));
        assert!(
            provider.model_cache().snapshot().is_none(),
            "a model-not-found error must invalidate the cache (single-model pool, Blocker 3)"
        );
    }

    #[test]
    fn observe_dispatch_error_should_ignore_non_model_unsupported_errors() {
        let provider = OpenrouterProvider::test_provider();
        provider
            .model_cache()
            .seed_for_test(vec![free_entry("only/model:free")]);

        provider.observe_dispatch_error(ProviderError::RateLimited);

        assert!(
            provider.model_cache().snapshot().is_some(),
            "a non-model-not-found error must not touch the cache"
        );
    }

    // REQ-3 (Story 2.1.2, Task 2.1.2d) — background refresh task's weak-ref
    // exit. Uses a broken `AuthMethod::Exec` (fails hermetically in
    // `build_headers`, no network access needed) so `new()`'s eager refresh
    // fails fast and deterministically, matching the "unreachable
    // endpoint" acceptance criterion's *effect* (refresh fails, `new()`
    // still returns `Ok` with a `None` snapshot) without depending on live
    // network access.
    #[tokio::test(start_paused = true)]
    async fn background_refresh_task_should_exit_when_provider_is_dropped() {
        let upstream = Upstream {
            name: "test-openrouter-broken-auth".to_string(),
            kind: crate::config::schema::UpstreamKind::Openrouter {},
            auth: Some(crate::config::schema::AuthMethod::Exec {
                command: "/nonexistent-binary-xyz-consolette-test".to_string(),
                args: vec![],
                cache_ttl_secs: 0,
                timeout_secs: 1,
            }),
        };
        let provider = OpenrouterProvider::new(
            Arc::new(upstream),
            Arc::new(crate::auth::SystemSecretResolver),
            Arc::new(ExecCredentialCache::new()),
            5,
        )
        .await
        .expect("new() must return Ok even though the eager refresh fails");

        assert!(
            provider.model_cache().snapshot().is_none(),
            "eager refresh should have failed against the broken auth method"
        );

        let weak_cache = Arc::downgrade(&provider.model_cache());
        let weak_provider = Arc::downgrade(&provider);
        drop(provider);

        // No other strong references exist (the background task holds only
        // `Weak`s per Story 2.1.2) — both should already be gone.
        assert!(weak_cache.upgrade().is_none());
        assert!(weak_provider.upgrade().is_none());

        // Advance virtual time past one refresh tick so the background
        // task's loop body actually runs its `Weak::upgrade()` calls and
        // exits, proving it doesn't panic once its targets are gone.
        tokio::time::advance(MODEL_LIST_REFRESH_INTERVAL + Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
    }
}
