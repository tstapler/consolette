//! Generic OpenAI-compatible provider.
//!
//! Forwards requests verbatim to `POST {base_url}/v1/chat/completions` on any
//! OpenAI-compatible endpoint. Unlike `AnthropicProvider`, `base_url` is not
//! hardcoded — `UpstreamKind::Openai` carries it explicitly in config, since
//! this provider is meant to point at arbitrary OpenAI-compatible services
//! (including a future employer-specific gateway, wired up as a separate
//! plugin per ADR-007 — never here).
//!
//! Mirrors `AnthropicProvider`'s structure: the ADR-004 two-`reqwest::Client`
//! split, and `anthropic::apply_auth_headers` for auth (config-driven
//! `bearer`/`apikey`/`exec`, identical across upstream kinds — no reason to
//! duplicate it).

mod resolution;
mod responses;

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures_core::Stream;
use http::HeaderMap;
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::auth::exec::ExecCredentialCache;
use crate::auth::SecretResolver;
use crate::config::schema::Upstream;
use crate::metrics::ProxyMetrics;

use super::anthropic::apply_auth_headers;
use super::{map_openai_finish_reason, Provider, ProviderError, ProviderResponse};

use async_trait::async_trait;
use futures_util::StreamExt;

/// Internal-only request-body key `Router::dispatch` uses to hand the active
/// route upstream's `model_family` (Epic 1.2's `RouteUpstreamRef.model_family`)
/// to `OpenaiProvider::send` per-request, since `build_providers` constructs
/// providers before any route is in scope (ADR-001; Epic 1.3 Pattern
/// Decisions). `OpenaiProvider::send` strips this key before translating or
/// forwarding the body upstream — it must never reach a real OpenAI-compatible
/// endpoint.
pub(crate) const MODEL_FAMILY_BODY_KEY: &str = "__consolette_model_family";

/// Generic OpenAI-compatible API provider.
pub struct OpenaiProvider {
    /// Pooled client for non-streaming requests.
    client: Client,
    /// Non-pooled client for SSE streaming (prevents pool exhaustion).
    stream_client: Client,
    /// Base URL for the OpenAI-compatible API, from `UpstreamKind::Openai::base_url`.
    base_url: String,
    /// The upstream this provider was constructed for — supplies `name` (for
    /// exec-cache keying) and `auth`.
    upstream: Arc<Upstream>,
    /// Resolves `SecretRef`s (env/keychain/inline) to plaintext.
    resolver: Arc<dyn SecretResolver + Send + Sync>,
    /// Shared cache for `exec` auth-method subprocess results.
    exec_cache: Arc<ExecCredentialCache>,
    /// Epic 2.2 per-family model resolution cache/single-flight/backoff
    /// state. Always constructed, never `Option` — whether it's ever
    /// populated for this upstream depends solely on whether any dispatched
    /// request carries the internal `MODEL_FAMILY_BODY_KEY` (a per-request
    /// fact set by `Router::dispatch`, not knowable at construction time). A
    /// static-`model` upstream's requests never carry that key, so this
    /// state is simply never read or written for it — the zero-overhead
    /// constraint holds without a construction-time flag.
    resolution: Arc<resolution::ResolutionState>,
    /// The configured request timeout, seconds — stored (not just baked into
    /// `client`'s `read_timeout`) so Epic 2.3's walk can derive a shortened
    /// per-probe timeout from it (Task 2.3.2c's `min(request_timeout_secs /
    /// candidate_count.clamp(1, 5), 10)` formula).
    request_timeout_secs: u64,
}

impl OpenaiProvider {
    /// Construct a new `OpenaiProvider` for one configured `Upstream`.
    ///
    /// `base_url` should be the `UpstreamKind::Openai::base_url` value for
    /// this upstream (e.g. `https://api.openai.com`), with no trailing slash.
    ///
    /// `metrics` (Epic 5.1) is the process-wide `/metrics` counters instance
    /// the Epic 2.3 resolution walk records `resolution_attempts_total`/
    /// `resolution_exhausted_total` against, labeled with this upstream's own
    /// `upstream.name`. A static-`model` upstream never triggers a walk, so
    /// it never touches `metrics` beyond holding the `Arc`.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError::Upstream`] if either reqwest `Client` fails
    /// to build (e.g. an invalid TLS backend configuration).
    pub fn new(
        upstream: Arc<Upstream>,
        base_url: String,
        resolver: Arc<dyn SecretResolver + Send + Sync>,
        exec_cache: Arc<ExecCredentialCache>,
        request_timeout_secs: u64,
        metrics: Arc<ProxyMetrics>,
    ) -> Result<Self, ProviderError> {
        let timeout = Duration::from_secs(request_timeout_secs);

        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(timeout)
            .build()
            .map_err(|e| ProviderError::Upstream {
                status: 0,
                body: e.to_string(),
            })?;

        // ADR-004: separate client with pool_max_idle_per_host(0) for SSE
        let stream_client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|e| ProviderError::Upstream {
                status: 0,
                body: e.to_string(),
            })?;

        let resolution = Arc::new(resolution::ResolutionState::with_metrics(
            upstream.name.clone(),
            metrics,
        ));

        Ok(Self {
            client,
            stream_client,
            base_url,
            upstream,
            resolver,
            exec_cache,
            resolution,
            request_timeout_secs,
        })
    }

    /// Build the outgoing request headers: `Content-Type` plus auth per the
    /// upstream's configured `AuthMethod`.
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
        Ok(out)
    }

    /// Send a non-streaming request to `POST /v1/chat/completions`.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if auth resolution, the HTTP request, or
    /// upstream error-status mapping fails.
    pub async fn send_request(&self, body: Value) -> Result<Value, ProviderError> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let headers = self.build_headers(&url).await?;
        let body_bytes = serde_json::to_vec(&body).map_err(|e| ProviderError::Upstream {
            status: 0,
            body: e.to_string(),
        })?;

        debug!("OpenAI non-stream POST {url}");

        if crate::providers::bodies_logged() {
            tracing::info!(
                target: "consolette::bodies",
                upstream = %self.upstream.name,
                body = %crate::providers::redact_bodies(&body),
                "openai upstream request"
            );
        }

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
        let out = map_error_status(status, response).await;
        if crate::providers::bodies_logged() {
            if let Ok(ref ok) = out {
                tracing::info!(
                    target: "consolette::bodies",
                    upstream = %self.upstream.name,
                    status = %status,
                    body = %crate::providers::redact_bodies(ok),
                    "openai upstream response"
                );
            }
        }
        out
    }

    /// Send a non-streaming request to `POST /v1/responses`, for a model
    /// resolved (Epic 2.3/3.6) or otherwise known to require the Responses
    /// API instead of `/v1/chat/completions`. Mirrors [`Self::send_request`]
    /// exactly apart from the URL — same header-building, body
    /// serialization, and error-status mapping.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if auth resolution, the HTTP request, or
    /// upstream error-status mapping fails.
    pub async fn send_responses_request(&self, body: Value) -> Result<Value, ProviderError> {
        let url = format!("{}/v1/responses", self.base_url);
        let headers = self.build_headers(&url).await?;
        let body_bytes = serde_json::to_vec(&body).map_err(|e| ProviderError::Upstream {
            status: 0,
            body: e.to_string(),
        })?;

        debug!("OpenAI non-stream POST {url}");

        if crate::providers::bodies_logged() {
            tracing::info!(
                target: "consolette::bodies",
                upstream = %self.upstream.name,
                body = %crate::providers::redact_bodies(&body),
                "openai upstream request"
            );
        }

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
        let out = map_error_status(status, response).await;
        if crate::providers::bodies_logged() {
            if let Ok(ref ok) = out {
                tracing::info!(
                    target: "consolette::bodies",
                    upstream = %self.upstream.name,
                    status = %status,
                    body = %crate::providers::redact_bodies(ok),
                    "openai upstream response"
                );
            }
        }
        out
    }

    /// Fetch the list of models from `GET /v1/models`.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if auth resolution, the HTTP request, or
    /// upstream error-status mapping fails.
    pub async fn fetch_models(&self) -> Result<Value, ProviderError> {
        let url = format!("{}/v1/models", self.base_url);
        let headers = self.build_headers(&url).await?;

        debug!("OpenAI GET {url}");

        let response = self
            .client
            .get(&url)
            .headers(headers)
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
        map_error_status(status, response).await
    }

    /// Epic 2.3's per-candidate probe send: `POST /v1/chat/completions` with
    /// [`resolution::build_probe_body`]'s minimal body for `candidate`,
    /// using a per-request `timeout` override (Task 2.3.2c) rather than the
    /// shared `client`'s baked-in `read_timeout` — a multi-candidate walk
    /// must not risk spending the *full* configured `request_timeout_secs`
    /// on every single candidate. `token_param` (Epic 4.1) selects which
    /// token-limit key the probe body carries — `MaxCompletionTokens`
    /// substitutes it in via
    /// [`resolution::rename_max_tokens_to_max_completion_tokens`] on top of
    /// the same base body, so [`walk_candidates`](resolution) can retry the
    /// *same* candidate with the other key without needing its own body
    /// construction logic.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] under the same mapping as
    /// [`OpenaiProvider::send_request`] (auth resolution, HTTP transport, or
    /// upstream error-status mapping failures) — a 429 always surfaces as
    /// [`ProviderError::RateLimited`], never something the resolution walk
    /// needs to reclassify itself.
    async fn probe_candidate(
        &self,
        candidate: &str,
        timeout: Duration,
        token_param: resolution::TokenParamStyle,
    ) -> Result<Value, ProviderError> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let headers = self.build_headers(&url).await?;
        let mut body = resolution::build_probe_body(candidate);
        if token_param == resolution::TokenParamStyle::MaxCompletionTokens {
            resolution::rename_max_tokens_to_max_completion_tokens(&mut body);
        }
        let body_bytes = serde_json::to_vec(&body).map_err(|e| ProviderError::Upstream {
            status: 0,
            body: e.to_string(),
        })?;

        debug!("OpenAI resolution probe POST {url} (candidate={candidate})");

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .timeout(timeout)
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
        map_error_status(status, response).await
    }

    /// Story 2.3.3a: a real (non-probe) request against a cache-resolved
    /// model that fails `Deprecated` invalidates that family's cache entry
    /// immediately, so the *next* request for the family re-walks instead of
    /// repeatedly hitting the same dead model until Story 2.3.3b's TTL
    /// backstop eventually catches it. A no-op for any error that isn't
    /// `Deprecated`, or when `family` is `None` (no resolution was involved
    /// in this request at all).
    fn invalidate_cache_on_deprecated_failure(&self, family: Option<&str>, error: &ProviderError) {
        let Some(family) = family else {
            return;
        };
        let is_deprecated = match error {
            ProviderError::Validation(body, status) | ProviderError::Upstream { body, status } => {
                classify_openai_error(*status, body) == OpenaiErrorClass::Deprecated
            }
            _ => false,
        };
        if is_deprecated {
            self.resolution.cache.remove(family);
        }
    }

    /// Send a streaming request to `POST /v1/chat/completions`.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if auth resolution, the HTTP request, or
    /// upstream error-status mapping fails.
    pub async fn send_streaming_request(
        &self,
        body: Value,
    ) -> Result<reqwest::Response, ProviderError> {
        self.send_streaming_request_to(body, "/v1/chat/completions")
            .await
    }

    /// Streaming counterpart to [`Self::send_responses_request`] — same
    /// `/v1/responses` endpoint, `stream: true` body flag, raw
    /// `reqwest::Response` returned for [`responses::ResponsesToAnthropicStream`]
    /// to consume.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if auth resolution, the HTTP request, or
    /// upstream error-status mapping fails.
    pub async fn send_responses_streaming_request(
        &self,
        body: Value,
    ) -> Result<reqwest::Response, ProviderError> {
        self.send_streaming_request_to(body, "/v1/responses").await
    }

    async fn send_streaming_request_to(
        &self,
        mut body: Value,
        path: &str,
    ) -> Result<reqwest::Response, ProviderError> {
        body["stream"] = Value::Bool(true);

        if crate::providers::bodies_logged() {
            tracing::info!(
                target: "consolette::bodies",
                upstream = %self.upstream.name,
                body = %crate::providers::redact_bodies(&body),
                "openai upstream stream request"
            );
        }

        let url = format!("{}{path}", self.base_url);
        let headers = self.build_headers(&url).await?;
        let body_bytes = serde_json::to_vec(&body).map_err(|e| ProviderError::Upstream {
            status: 0,
            body: e.to_string(),
        })?;

        debug!("OpenAI stream POST {url}");

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

        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(60);
            warn!("OpenAI rate limited ({status}), retry-after {retry_after}s");
            return Err(ProviderError::RateLimited);
        }

        if status.is_client_error() {
            let status_u16 = status.as_u16();
            let body_str = response.text().await.unwrap_or_default();
            return Err(ProviderError::Validation(body_str, status_u16));
        }

        if !status.is_success() {
            let status_u16 = status.as_u16();
            let body_str = response.text().await.unwrap_or_default();
            return Err(ProviderError::Upstream {
                status: status_u16,
                body: body_str,
            });
        }

        Ok(response)
    }
}

/// Convert a non-success HTTP status into the appropriate `ProviderError`,
/// consuming the response body for error detail.
async fn map_error_status(
    status: StatusCode,
    response: reqwest::Response,
) -> Result<Value, ProviderError> {
    if status == StatusCode::TOO_MANY_REQUESTS {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(60);
        warn!("OpenAI rate limited ({status}), retry-after {retry_after}s");
        return Err(ProviderError::RateLimited);
    }

    if status.is_client_error() {
        let status_u16 = status.as_u16();
        let body_str = response.text().await.unwrap_or_default();
        return Err(ProviderError::Validation(body_str, status_u16));
    }

    if !status.is_success() {
        let status_u16 = status.as_u16();
        let body_str = response.text().await.unwrap_or_default();
        return Err(ProviderError::Upstream {
            status: status_u16,
            body: body_str,
        });
    }

    response.json().await.map_err(|e| ProviderError::Upstream {
        status: status.as_u16(),
        body: e.to_string(),
    })
}

/// Classification of an `OpenAI` error response for the (opt-in) model-resolution
/// loop — never surfaced as a `ProviderError` variant and never consulted by
/// `map_error_status` or any of its existing callers (ADR-003).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenaiErrorClass {
    /// The requested model id has been deprecated/decommissioned — advance
    /// to the next candidate.
    Deprecated,
    /// The model exists but must be sent to `/v1/responses` instead of
    /// `/v1/chat/completions` — retry the same candidate against that
    /// endpoint.
    WrongEndpoint,
    /// Malformed body, or a 5xx/network-shaped failure — must never advance
    /// the candidate list.
    Transient,
    /// Auth failure, or any 4xx that matches no known pattern — must never
    /// advance the candidate list.
    Other,
}

/// (status, message-substring, class) lookup table for `classify_openai_error`.
/// A newly observed error phrasing is a one-line entry here, not a change to
/// the classification logic itself (Story 1.1.2).
const OPENAI_ERROR_CLASSIFICATION_TABLE: &[(u16, &str, OpenaiErrorClass)] = &[
    (400, "has been deprecated", OpenaiErrorClass::Deprecated),
    // Alternate phrasing observed for the same condition — demonstrates that
    // covering it is a table entry, not a logic change.
    (400, "no longer available", OpenaiErrorClass::Deprecated),
    (404, "v1/responses", OpenaiErrorClass::WrongEndpoint),
];

/// Classify an `OpenAI` HTTP error status + response body for the
/// model-resolution loop (ADR-003). 429 is not handled here — it is already
/// turned into `ProviderError::RateLimited` upstream of this function.
pub(crate) fn classify_openai_error(status: u16, body: &str) -> OpenaiErrorClass {
    // 5xx/network-shaped failures are always transient, regardless of
    // whether the body happens to parse.
    if status >= 500 || status == 0 {
        return OpenaiErrorClass::Transient;
    }

    let Some(message) = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
    else {
        return OpenaiErrorClass::Other;
    };

    OPENAI_ERROR_CLASSIFICATION_TABLE
        .iter()
        .find(|(table_status, substring, _)| *table_status == status && message.contains(substring))
        .map_or(OpenaiErrorClass::Other, |(_, _, class)| *class)
}

#[async_trait]
impl Provider for OpenaiProvider {
    fn name(&self) -> &'static str {
        "openai"
    }

    #[allow(clippy::too_many_lines)] // one cohesive dispatch path across chat-completions/responses translation
    async fn send(
        &self,
        mut body: Value,
        _headers: HeaderMap,
        stream: bool,
    ) -> Result<ProviderResponse, ProviderError> {
        // Epic 1.3/ADR-001: `Router::dispatch` smuggles the active route
        // upstream's `model_family` in via this internal-only key (never a
        // real OpenAI field) since providers are constructed before any
        // route is in scope. Strip it before any translation/forwarding
        // logic runs so it never reaches the real upstream. A body with no
        // such key (the common, static-pin case) costs one hash-map probe —
        // no allocation, no clock read — satisfying the zero-overhead
        // requirement Phase 2's resolution logic depends on.
        let family = body
            .as_object_mut()
            .and_then(|o| o.remove(MODEL_FAMILY_BODY_KEY))
            .and_then(|v| v.as_str().map(str::to_string));

        // Epic 2.2/2.3: a request carrying the internal family key resolves
        // to a concrete model id via `resolve_family` — a cache hit within
        // Story 2.3.3b's TTL returns immediately with no HTTP call; a miss
        // (cold, TTL-stale, or invalidated by Story 2.3.3a below) runs the
        // real per-candidate probe walk, single-flighted and backoff-guarded
        // against concurrent/repeated misses. A body with no such key (the
        // common, static-pin case) never reaches this branch at all.
        // A static-`model` upstream (no family key) never populates this,
        // so it always stays the `Default` `ChatCompletions` — the
        // resolved-as-Responses path below is unreachable for that case,
        // matching the "completely unchanged" requirement for static pins
        // (Story 3.2.3).
        let mut endpoint = resolution::Endpoint::default();
        // Story 4.1.2: which token-limit body key the resolved candidate
        // needs, set from `ResolvedModel::token_param` below. A static-`model`
        // upstream (no family key) never runs the branch that sets this, so
        // it stays the default `MaxTokens` — the post-processing rename
        // later in this function is then a no-op, matching "completely
        // unchanged" for that case.
        let mut token_param = resolution::TokenParamStyle::MaxTokens;
        if let Some(ref family) = family {
            let resolved = resolution::resolve_family(
                &self.resolution,
                family,
                resolution::SINGLE_FLIGHT_WAIT_CEILING,
                self.request_timeout_secs,
                || self.list_models(),
                |candidate, timeout, probe_endpoint, token_param| async move {
                    match probe_endpoint {
                        resolution::Endpoint::ChatCompletions => {
                            self.probe_candidate(&candidate, timeout, token_param).await
                        }
                        // Epic 3.6: the `WrongEndpoint` retry reuses the
                        // real Responses send path (Story 3.2.3a) rather
                        // than a probe-specific method — this is the one
                        // extra request per walk that doesn't get the
                        // shortened per-probe timeout Task 2.3.2c gives the
                        // chat/completions probes. The Responses API has no
                        // `max_tokens`/`max_completion_tokens` distinction
                        // (it uses `max_output_tokens` — see
                        // `build_probe_body_responses`), so `token_param` is
                        // unused here; `walk_candidates` never requests a
                        // `MaxCompletionTokens` retry against this endpoint.
                        resolution::Endpoint::Responses => {
                            self.send_responses_request(resolution::build_probe_body_responses(
                                &candidate,
                            ))
                            .await
                        }
                    }
                },
            )
            .await?;
            endpoint = resolved.endpoint;
            token_param = resolved.token_param;
            if let Some(obj) = body.as_object_mut() {
                obj.insert("model".to_string(), Value::String(resolved.model_id));
            }
        }

        // Epic 3.2/3.3/3.6: a family resolved to `Endpoint::Responses` sends
        // its request to `/v1/responses` instead of `/v1/chat/completions`,
        // using the Responses-specific translation pair (non-streaming:
        // Epic 3.2; streaming, via `ResponsesToAnthropicStream`: Epic 3.3).
        if endpoint == resolution::Endpoint::Responses {
            let model_hint = body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let responses_body = responses::translate_anthropic_request_to_responses(body);
            if stream {
                let response = match self.send_responses_streaming_request(responses_body).await {
                    Ok(response) => response,
                    Err(err) => {
                        self.invalidate_cache_on_deprecated_failure(family.as_deref(), &err);
                        return Err(err);
                    }
                };
                let byte_stream = response
                    .bytes_stream()
                    .map(|r| r.map_err(anyhow::Error::from));
                let translated =
                    responses::ResponsesToAnthropicStream::new(byte_stream, model_hint);
                return Ok(ProviderResponse::Stream(Box::pin(translated)));
            }
            let value = match self.send_responses_request(responses_body).await {
                Ok(value) => value,
                Err(err) => {
                    self.invalidate_cache_on_deprecated_failure(family.as_deref(), &err);
                    return Err(err);
                }
            };
            let anthropic_value = responses::translate_responses_response_to_anthropic(value);
            return Ok(ProviderResponse::Full(anthropic_value));
        }

        // `Provider::send` always receives/returns Anthropic-wire-format JSON
        // at the trait boundary (see `BedrockProvider` for the same pattern)
        // — translate to/from native OpenAI shape here, internal to this
        // provider.
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let mut openai_body = super::translate_anthropic_request_to_openai(&body);
        // Story 4.1.2: post-process the body the shared translator already
        // produced — never widen `translate_anthropic_request_to_openai`
        // itself, since `OpenrouterProvider::send` calls it too
        // (`src/providers/openrouter/mod.rs`) with its own test coverage
        // that must stay untouched. A no-op unless resolution (Epic 2.3/4.1)
        // determined this candidate needs `max_completion_tokens` instead of
        // `max_tokens` — a static-`model` upstream's `token_param` stays the
        // default `MaxTokens`, so this is byte-identical to today for it.
        if token_param == resolution::TokenParamStyle::MaxCompletionTokens {
            resolution::rename_max_tokens_to_max_completion_tokens(&mut openai_body);
        }

        if stream {
            let response = match self.send_streaming_request(openai_body).await {
                Ok(response) => response,
                Err(err) => {
                    self.invalidate_cache_on_deprecated_failure(family.as_deref(), &err);
                    return Err(err);
                }
            };
            let byte_stream = response
                .bytes_stream()
                .map(|r| r.map_err(anyhow::Error::from));
            let translated = OpenaiToAnthropicStream::new(byte_stream, model);
            Ok(ProviderResponse::Stream(Box::pin(translated)))
        } else {
            let value = match self.send_request(openai_body).await {
                Ok(value) => value,
                Err(err) => {
                    // Story 2.3.3a: a real (non-probe) request failing
                    // `Deprecated` against a cache-resolved model invalidates
                    // that family's cache entry before the error propagates,
                    // so the next request for it re-walks instead of
                    // repeatedly hitting the same dead model.
                    self.invalidate_cache_on_deprecated_failure(family.as_deref(), &err);
                    return Err(err);
                }
            };
            let anthropic_value = super::translate_openai_response_to_anthropic(
                &value,
                body.get("model").and_then(Value::as_str),
            );
            Ok(ProviderResponse::Full(anthropic_value))
        }
    }

    async fn list_models(&self) -> Result<Vec<super::ModelInfo>, ProviderError> {
        let value = self.fetch_models().await?;
        let models = value
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| {
                let id = entry.get("id").and_then(Value::as_str)?.to_string();
                let owned_by = entry
                    .get("owned_by")
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
                Some(super::ModelInfo { id, owned_by })
            })
            .collect();
        Ok(models)
    }
}

/// Translates an `OpenAI` `chat.completion.chunk` SSE byte stream into an
/// Anthropic Messages SSE event stream — the reverse of
/// `entrypoint::openai_stream::OpenAiStreamTranslator`. Anthropic's SSE
/// protocol requires bracketing events (`message_start`,
/// `content_block_start`, ... `content_block_stop`, `message_delta`,
/// `message_stop`) that `OpenAI`'s flatter chunk stream has no equivalent
/// for, so a single inbound chunk can produce more than one outbound frame;
/// `pending` buffers those extras between polls.
///
/// Block layout: text is always block 0 (possibly empty); tool calls become
/// blocks 1..n in order of first appearance. Tool argument fragments
/// accumulate per `OpenAI` wire `index` and stream out as `input_json_delta`
/// events, so chunked arguments reassemble into one valid JSON object.
/// Four independent lifecycle flags (`started`/`finished`/`done` for the
/// overall stream plus `text_started` for block 0); splitting them into a
/// state machine would obscure the one-way pipeline, hence the allow.
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct OpenaiToAnthropicStream<S> {
    inner: eventsource_stream::EventStream<S>,
    id: String,
    model: String,
    started: bool,
    finished: bool,
    done: bool,
    pending: VecDeque<Bytes>,
    text_started: bool,
    tools: Vec<ToolSlot>,
}

/// One in-progress tool call block: arguments accumulate here until the
/// closing stop, so split fragments reassemble into valid JSON.
struct ToolSlot {
    /// `OpenAI` wire `index` from the chunk (need not be dense).
    oi_index: usize,
    /// Block index on the Anthropic face (1-based; 0 is text).
    block_index: usize,
    id: String,
    name: String,
    args: String,
    started: bool,
    stopped: bool,
}

impl<S> OpenaiToAnthropicStream<S>
where
    S: Stream<Item = Result<Bytes, anyhow::Error>>,
{
    pub(crate) fn new(inner: S, model: String) -> Self {
        Self {
            inner: inner.eventsource(),
            id: format!("msg_{}", uuid::Uuid::new_v4()),
            model,
            started: false,
            finished: false,
            done: false,
            pending: VecDeque::new(),
            text_started: false,
            tools: Vec::new(),
        }
    }

    fn frame(event: &str, data: &Value) -> Bytes {
        Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
    }

    /// Push the synthetic `message_start`, so every stream opens with a
    /// well-formed Anthropic preamble even if the first `OpenAI` chunk
    /// carries no text.
    fn ensure_message_started(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        self.pending.push_back(Self::frame(
            "message_start",
            &json!({
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
    }

    /// Push the text `content_block_start` (index 0). Text always owns block
    /// 0 — possibly empty — so tool blocks can take stable indices 1..n.
    fn ensure_text_started(&mut self) {
        self.ensure_message_started();
        if self.text_started {
            return;
        }
        self.text_started = true;
        self.pending.push_back(Self::frame(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            }),
        ));
    }

    fn push_delta(&mut self, text: &str) {
        self.ensure_text_started();
        self.pending.push_back(Self::frame(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": text}
            }),
        ));
    }

    /// Fold one `OpenAI` tool-call delta entry into its Anthropic block:
    /// accumulate id/name/arguments by wire `index`, open the block once
    /// id and name are known, and stream argument fragments as
    /// `input_json_delta` events.
    fn push_tool_delta(
        &mut self,
        oi_index: usize,
        id: Option<&str>,
        name: Option<&str>,
        args_frag: &str,
    ) {
        self.ensure_message_started();
        let pos = if let Some(pos) = self.tools.iter().position(|t| t.oi_index == oi_index) {
            pos
        } else {
            // OpenAI indices need not be dense; Anthropic block indices
            // must be, so slots take 1-based positions in appearance
            // order regardless of the wire index.
            self.tools.push(ToolSlot {
                oi_index,
                block_index: self.tools.len() + 1,
                id: String::new(),
                name: String::new(),
                args: String::new(),
                started: false,
                stopped: false,
            });
            self.tools.len() - 1
        };
        // NOTE: position-by-wire-index assumes the gateway reuses one wire
        // index per call; a gateway renumbering mid-stream would split one
        // call into two blocks (fail-closed, still well-formed).
        let slot = &mut self.tools[pos];
        if slot.id.is_empty() {
            if let Some(id) = id {
                slot.id = id.to_string();
            }
        }
        if slot.name.is_empty() {
            if let Some(name) = name {
                slot.name = name.to_string();
            }
        }
        slot.args.push_str(args_frag);
        if !slot.started && !slot.id.is_empty() && !slot.name.is_empty() {
            slot.started = true;
            let (block_index, id, name, args) = (
                slot.block_index,
                slot.id.clone(),
                slot.name.clone(),
                slot.args.clone(),
            );
            self.pending.push_back(Self::frame(
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": block_index,
                    "content_block": {
                        "type": "tool_use", "id": id, "name": name, "input": {}
                    }
                }),
            ));
            if !args.is_empty() {
                self.pending.push_back(Self::frame(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": block_index,
                        "delta": {"type": "input_json_delta", "partial_json": args}
                    }),
                ));
            }
        } else if slot.started && !args_frag.is_empty() {
            let block_index = slot.block_index;
            self.pending.push_back(Self::frame(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": block_index,
                    "delta": {"type": "input_json_delta", "partial_json": args_frag}
                }),
            ));
        }
    }

    /// Emit the closing stops for every open block plus the
    /// `message_delta`/`message_stop` sequence. Idempotent. A stream that
    /// opened any tool block always closes `tool_use` (mirroring the
    /// non-streaming rule: blocks present ⇔ matching stop reason).
    fn close(&mut self, stop_reason: &str) {
        if self.finished {
            return;
        }
        self.ensure_text_started();
        self.finished = true;
        let mut stop_reason = stop_reason.to_string();
        for slot in &mut self.tools {
            if slot.started && !slot.stopped {
                slot.stopped = true;
                let block_index = slot.block_index;
                self.pending.push_back(Self::frame(
                    "content_block_stop",
                    &json!({"type": "content_block_stop", "index": block_index}),
                ));
            }
        }
        if self.tools.iter().any(|t| t.started) {
            stop_reason = "tool_use".to_string();
        }
        self.pending.push_back(Self::frame(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": 0}),
        ));
        self.pending.push_back(Self::frame(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason},
                "usage": {"output_tokens": 0}
            }),
        ));
        self.pending.push_back(Self::frame(
            "message_stop",
            &json!({"type": "message_stop"}),
        ));
    }
}

impl<S> Stream for OpenaiToAnthropicStream<S>
where
    S: Stream<Item = Result<Bytes, anyhow::Error>> + Unpin,
{
    type Item = Result<Bytes, anyhow::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            if let Some(frame) = this.pending.pop_front() {
                return Poll::Ready(Some(Ok(frame)));
            }
            if this.done {
                return Poll::Ready(None);
            }
            if this.finished {
                this.done = true;
                continue;
            }

            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    this.close("end_turn");
                }
                Poll::Ready(Some(Err(e))) => {
                    warn!(error = %e, "openai->anthropic stream translator: eventsource parse error");
                    this.close("end_turn");
                }
                Poll::Ready(Some(Ok(event))) => {
                    if event.data == "[DONE]" {
                        this.close("end_turn");
                        continue;
                    }
                    if crate::providers::bodies_logged() {
                        tracing::info!(
                            target: "consolette::bodies",
                            model = %this.model,
                            chunk = %event.data,
                            "openai upstream stream chunk"
                        );
                    }
                    let Ok(parsed) = serde_json::from_str::<Value>(&event.data) else {
                        continue;
                    };

                    let choice = parsed
                        .get("choices")
                        .and_then(Value::as_array)
                        .and_then(|a| a.first());

                    // Delta content arrives as a string on most gateways,
                    // but OpenRouter-style responses may send an array of
                    // parts or put tokens in `reasoning_content`/`reasoning`
                    // (reasoning models). Either shape collapsing to ""
                    // is what produced empty client text with nonzero
                    // usage, so extract text from all of them.
                    let delta = choice.and_then(|c| c.get("delta"));
                    let mut text = delta
                        .and_then(|d| d.get("content"))
                        .map(crate::providers::extract_text_from_content)
                        .unwrap_or_default();
                    if text.is_empty() {
                        text = delta
                            .and_then(|d| d.get("reasoning_content").or_else(|| d.get("reasoning")))
                            .map(crate::providers::extract_text_from_content)
                            .unwrap_or_default();
                    }
                    if !text.is_empty() {
                        this.push_delta(&text);
                    }

                    if let Some(calls) = delta
                        .and_then(|d| d.get("tool_calls"))
                        .and_then(Value::as_array)
                    {
                        for (pos, call) in calls.iter().enumerate() {
                            let oi_index = call
                                .get("index")
                                .and_then(Value::as_u64)
                                .map_or(pos, |i| usize::try_from(i).unwrap_or(pos));
                            let function = call.get("function");
                            this.push_tool_delta(
                                oi_index,
                                call.get("id").and_then(Value::as_str),
                                function.and_then(|f| f.get("name")).and_then(Value::as_str),
                                function
                                    .and_then(|f| f.get("arguments"))
                                    .and_then(Value::as_str)
                                    .unwrap_or(""),
                            );
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
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::auth::SystemSecretResolver;
    use crate::config::schema::UpstreamKind;

    fn test_upstream() -> Arc<Upstream> {
        Arc::new(Upstream {
            name: "test-openai".to_string(),
            kind: UpstreamKind::Openai {
                base_url: "https://example.invalid".to_string(),
            },
            auth: None,
        })
    }

    fn test_provider() -> OpenaiProvider {
        OpenaiProvider::new(
            test_upstream(),
            "https://example.invalid".to_string(),
            Arc::new(SystemSecretResolver),
            Arc::new(ExecCredentialCache::new()),
            30,
            Arc::new(ProxyMetrics::new()),
        )
        .unwrap()
    }

    #[test]
    fn name_is_openai() {
        assert_eq!(test_provider().name(), "openai");
    }

    #[tokio::test]
    async fn build_headers_sets_content_type() {
        let provider = test_provider();
        // No auth configured on the upstream, so header building should fail
        // with an Auth error rather than panicking or silently succeeding.
        let result = provider
            .build_headers("https://example.invalid/v1/chat/completions")
            .await;
        assert!(matches!(result, Err(ProviderError::Auth(_))));
    }

    // ────────────────────────────────────────────────────────────────────
    // MODEL_FAMILY_BODY_KEY extraction/stripping (Story 1.3.3). No HTTP
    // mocking crate is a dev-dependency (see
    // `src/cost_metrics/test_support.rs`'s doc comment) and that module's
    // `MockServer` is hardcoded to `/v1/messages/count_tokens`, so this
    // reuses `openai.rs`'s own pattern (plain function tests) plus a small
    // local `axum` server — already a direct dependency — for the one case
    // that needs a real outgoing HTTP body to inspect.
    // ────────────────────────────────────────────────────────────────────

    #[allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::items_after_statements
    )]
    mod model_family_body_key {
        use super::*;
        use crate::auth::exec::ExecCredentialCache;
        use crate::config::schema::{AuthMethod, SecretRef};
        use std::sync::Mutex;
        use tokio::net::TcpListener;

        async fn handle_chat_completions(
            axum::extract::State(captured): axum::extract::State<Arc<Mutex<Option<Value>>>>,
            axum::Json(body): axum::Json<Value>,
        ) -> axum::Json<Value> {
            *captured.lock().unwrap() = Some(body);
            axum::Json(json!({
                "id": "chatcmpl-test",
                "model": "gpt-5.1-codex-max",
                "choices": [{
                    "message": {"role": "assistant", "content": "ok"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1}
            }))
        }

        /// Minimal local `/v1/chat/completions` double that captures the raw
        /// JSON body it received and always returns a well-formed chat
        /// completion, torn down when the returned `JoinHandle` is dropped.
        async fn start_capturing_chat_completions_server() -> (
            String,
            Arc<Mutex<Option<Value>>>,
            tokio::task::JoinHandle<()>,
        ) {
            let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
            let captured_for_handler = captured.clone();

            let app = axum::Router::new()
                .route(
                    "/v1/chat/completions",
                    axum::routing::post(handle_chat_completions),
                )
                .with_state(captured_for_handler);
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("mock server bind should succeed");
            let addr = listener
                .local_addr()
                .expect("mock server local_addr should succeed");
            let handle = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            (format!("http://{addr}"), captured, handle)
        }

        fn provider_for(base_url: String) -> OpenaiProvider {
            let upstream = Arc::new(Upstream {
                name: "test-openai".to_string(),
                kind: UpstreamKind::Openai {
                    base_url: base_url.clone(),
                },
                auth: Some(AuthMethod::Bearer {
                    token: SecretRef::Inline {
                        value: "test-token".to_string(),
                    },
                }),
            });
            OpenaiProvider::new(
                upstream,
                base_url,
                Arc::new(SystemSecretResolver),
                Arc::new(ExecCredentialCache::new()),
                30,
                Arc::new(ProxyMetrics::new()),
            )
            .unwrap()
        }

        #[tokio::test]
        #[allow(clippy::unwrap_used)]
        async fn send_should_strip_internal_model_family_key_before_forwarding_to_upstream() {
            let (base_url, captured, _server) = start_capturing_chat_completions_server().await;
            let provider = provider_for(base_url);

            // Epic 2.2: a family key on a cold cache now triggers a real
            // resolution walk (Epic 2.3's job, still a stub here), so seed a
            // cache hit for "gpt-5" — this test's own point is only that the
            // internal key is stripped and never forwarded, not resolution
            // itself (covered separately by `resolution.rs`'s own tests).
            provider.resolution.cache.insert(
                "gpt-5".to_string(),
                crate::providers::openai::resolution::ResolvedModel {
                    model_id: "gpt-5.1-codex-max".to_string(),
                    endpoint: crate::providers::openai::resolution::Endpoint::ChatCompletions,
                    token_param: crate::providers::openai::resolution::TokenParamStyle::MaxTokens,
                    resolved_at: tokio::time::Instant::now(),
                },
            );

            let body = json!({
                "model": "gpt-5.1-codex-max",
                "messages": [{"role": "user", "content": "hi"}],
                MODEL_FAMILY_BODY_KEY: "gpt-5",
            });

            let result = provider.send(body, HeaderMap::new(), false).await;
            assert!(result.is_ok());

            let outgoing = captured
                .lock()
                .unwrap()
                .clone()
                .expect("mock server must have received a request");
            assert!(
                outgoing.get(MODEL_FAMILY_BODY_KEY).is_none(),
                "internal key must never reach the real upstream: {outgoing:?}"
            );
        }

        #[tokio::test]
        #[allow(clippy::unwrap_used)]
        async fn send_should_skip_resolution_entirely_when_body_has_no_internal_model_family_key() {
            // Regression/zero-overhead check: a static-pin request (no
            // internal key) produces the exact outgoing body it did before
            // this story landed — `translate_anthropic_request_to_openai`'s
            // output is otherwise deterministic given the same input.
            let (base_url, captured, _server) = start_capturing_chat_completions_server().await;
            let provider = provider_for(base_url);

            let body = json!({
                "model": "gpt-5.1-codex-max",
                "messages": [{"role": "user", "content": "hi"}],
            });
            let expected = crate::providers::translate_anthropic_request_to_openai(&body);

            let result = provider.send(body, HeaderMap::new(), false).await;
            assert!(result.is_ok());

            let outgoing = captured
                .lock()
                .unwrap()
                .clone()
                .expect("mock server must have received a request");
            assert_eq!(outgoing, expected);
        }

        // ────────────────────────────────────────────────────────────────
        // Story 4.1.2: post-processing the shared translator's output for a
        // resolved `TokenParamStyle`, entirely inside `OpenaiProvider::send`
        // — `translate_anthropic_request_to_openai` itself is never widened
        // (architecture-review.md Concern: that function is also called
        // from `OpenrouterProvider::send`).
        // ────────────────────────────────────────────────────────────────

        #[tokio::test]
        #[allow(clippy::unwrap_used)]
        async fn send_should_rename_max_tokens_to_max_completion_tokens_when_resolved_token_param_requires_it(
        ) {
            let (base_url, captured, _server) = start_capturing_chat_completions_server().await;
            let provider = provider_for(base_url);

            provider.resolution.cache.insert(
                "gpt-5".to_string(),
                crate::providers::openai::resolution::ResolvedModel {
                    model_id: "gpt-5.1-codex-max".to_string(),
                    endpoint: crate::providers::openai::resolution::Endpoint::ChatCompletions,
                    token_param:
                        crate::providers::openai::resolution::TokenParamStyle::MaxCompletionTokens,
                    resolved_at: tokio::time::Instant::now(),
                },
            );

            let body = json!({
                "model": "gpt-5.1-codex-max",
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 500,
                MODEL_FAMILY_BODY_KEY: "gpt-5",
            });

            let result = provider.send(body, HeaderMap::new(), false).await;
            assert!(result.is_ok());

            let outgoing = captured
                .lock()
                .unwrap()
                .clone()
                .expect("mock server must have received a request");
            assert_eq!(
                outgoing.get("max_completion_tokens"),
                Some(&json!(500)),
                "resolved MaxCompletionTokens must rename the key on the outgoing body: {outgoing:?}"
            );
            assert!(
                outgoing.get("max_tokens").is_none(),
                "no max_tokens key must remain once renamed: {outgoing:?}"
            );
        }

        #[tokio::test]
        #[allow(clippy::unwrap_used)]
        async fn send_should_leave_max_tokens_unchanged_when_resolved_token_param_is_default() {
            let (base_url, captured, _server) = start_capturing_chat_completions_server().await;
            let provider = provider_for(base_url);

            // Same cache-seeding pattern as this module's other tests, but
            // with the default `MaxTokens` style — the post-processing step
            // must be a complete no-op here.
            provider.resolution.cache.insert(
                "gpt-5".to_string(),
                crate::providers::openai::resolution::ResolvedModel {
                    model_id: "gpt-5.1-codex-max".to_string(),
                    endpoint: crate::providers::openai::resolution::Endpoint::ChatCompletions,
                    token_param: crate::providers::openai::resolution::TokenParamStyle::MaxTokens,
                    resolved_at: tokio::time::Instant::now(),
                },
            );

            let body = json!({
                "model": "gpt-5.1-codex-max",
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 500,
                MODEL_FAMILY_BODY_KEY: "gpt-5",
            });

            let result = provider.send(body, HeaderMap::new(), false).await;
            assert!(result.is_ok());

            let outgoing = captured
                .lock()
                .unwrap()
                .clone()
                .expect("mock server must have received a request");
            assert_eq!(outgoing.get("max_tokens"), Some(&json!(500)));
            assert!(outgoing.get("max_completion_tokens").is_none());
        }
    }

    // ────────────────────────────────────────────────────────────────────
    // Story 3.2.3: send()'s endpoint-choice branch
    // ────────────────────────────────────────────────────────────────────

    #[allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::items_after_statements
    )]
    mod responses_endpoint_routing {
        use super::*;
        use crate::auth::exec::ExecCredentialCache;
        use crate::auth::SystemSecretResolver;
        use crate::config::schema::{AuthMethod, SecretRef, UpstreamKind};
        use crate::providers::openai::resolution::{Endpoint, ResolvedModel, TokenParamStyle};
        use std::sync::Mutex;
        use tokio::net::TcpListener;

        /// Records which path was actually `POSTed` to (`/v1/responses` vs.
        /// `/v1/chat/completions`) plus the received body, and always
        /// returns a well-formed, minimal Responses API response.
        struct CapturedRequest {
            path: String,
            body: Value,
        }

        async fn handle_responses(
            axum::extract::State(captured): axum::extract::State<
                Arc<Mutex<Option<CapturedRequest>>>,
            >,
            axum::Json(body): axum::Json<Value>,
        ) -> axum::Json<Value> {
            *captured.lock().unwrap() = Some(CapturedRequest {
                path: "/v1/responses".to_string(),
                body,
            });
            axum::Json(json!({
                "id": "resp_test",
                "model": "gpt-5.3-codex",
                "output": [{
                    "type": "message",
                    "content": [{"type": "output_text", "text": "hello from responses"}],
                }],
                "usage": {"input_tokens": 1, "output_tokens": 1},
            }))
        }

        async fn handle_chat_completions_should_not_be_hit(
            axum::extract::State(captured): axum::extract::State<
                Arc<Mutex<Option<CapturedRequest>>>,
            >,
            axum::Json(body): axum::Json<Value>,
        ) -> axum::Json<Value> {
            *captured.lock().unwrap() = Some(CapturedRequest {
                path: "/v1/chat/completions".to_string(),
                body,
            });
            axum::Json(json!({
                "id": "chatcmpl-test",
                "model": "gpt-5.3-codex",
                "choices": [{
                    "message": {"role": "assistant", "content": "hello from chat completions"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1}
            }))
        }

        async fn start_server() -> (
            String,
            Arc<Mutex<Option<CapturedRequest>>>,
            tokio::task::JoinHandle<()>,
        ) {
            let captured: Arc<Mutex<Option<CapturedRequest>>> = Arc::new(Mutex::new(None));
            let app = axum::Router::new()
                .route("/v1/responses", axum::routing::post(handle_responses))
                .route(
                    "/v1/chat/completions",
                    axum::routing::post(handle_chat_completions_should_not_be_hit),
                )
                .with_state(captured.clone());
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("mock server bind should succeed");
            let addr = listener
                .local_addr()
                .expect("mock server local_addr should succeed");
            let handle = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            (format!("http://{addr}"), captured, handle)
        }

        fn provider_for(base_url: String) -> OpenaiProvider {
            let upstream = Arc::new(Upstream {
                name: "test-openai".to_string(),
                kind: UpstreamKind::Openai {
                    base_url: base_url.clone(),
                },
                auth: Some(AuthMethod::Bearer {
                    token: SecretRef::Inline {
                        value: "test-token".to_string(),
                    },
                }),
            });
            OpenaiProvider::new(
                upstream,
                base_url,
                Arc::new(SystemSecretResolver),
                Arc::new(ExecCredentialCache::new()),
                30,
                Arc::new(ProxyMetrics::new()),
            )
            .unwrap()
        }

        #[tokio::test]
        async fn send_should_post_to_responses_endpoint_when_resolved_model_endpoint_is_responses()
        {
            let (base_url, captured, _server) = start_server().await;
            let provider = provider_for(base_url);

            provider.resolution.cache.insert(
                "gpt-5".to_string(),
                ResolvedModel {
                    model_id: "gpt-5.3-codex".to_string(),
                    endpoint: Endpoint::Responses,
                    token_param: TokenParamStyle::MaxTokens,
                    resolved_at: tokio::time::Instant::now(),
                },
            );

            let body = json!({
                "model": "gpt-5",
                "messages": [{"role": "user", "content": "hi"}],
                MODEL_FAMILY_BODY_KEY: "gpt-5",
            });

            let result = provider.send(body, HeaderMap::new(), false).await;
            let response = result.expect("send should succeed");

            let ProviderResponse::Full(anthropic_value) = response else {
                panic!("non-streaming send must return ProviderResponse::Full");
            };
            assert_eq!(
                anthropic_value["content"],
                json!([{"type": "text", "text": "hello from responses"}])
            );

            let outgoing = captured
                .lock()
                .unwrap()
                .take()
                .expect("mock server must have received a request");
            assert_eq!(
                outgoing.path, "/v1/responses",
                "resolved Endpoint::Responses must route to /v1/responses, not /v1/chat/completions"
            );
            assert_eq!(outgoing.body["model"], json!("gpt-5.3-codex"));
        }

        #[tokio::test]
        async fn send_should_post_to_chat_completions_when_no_family_key_is_present() {
            // Regression check for the "completely unchanged" requirement: a
            // static-`model` pin (no `model_family`, no cached `Endpoint`)
            // must still route to /v1/chat/completions.
            let (base_url, captured, _server) = start_server().await;
            let provider = provider_for(base_url);

            let body = json!({
                "model": "gpt-5.3-codex",
                "messages": [{"role": "user", "content": "hi"}],
            });

            let result = provider.send(body, HeaderMap::new(), false).await;
            assert!(result.is_ok());

            let outgoing = captured
                .lock()
                .unwrap()
                .take()
                .expect("mock server must have received a request");
            assert_eq!(outgoing.path, "/v1/chat/completions");
        }
    }

    // ────────────────────────────────────────────────────────────────────
    // classify_openai_error (ADR-003 seam; Epic 1.1)
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn classify_openai_error_should_return_deprecated_when_400_names_deprecation() {
        let body = r#"{"error":{"type":"invalid_request_error","message":"The model `gpt-5.1-codex-max` has been deprecated"}}"#;
        assert_eq!(
            classify_openai_error(400, body),
            OpenaiErrorClass::Deprecated
        );
    }

    #[test]
    fn classify_openai_error_should_return_wrong_endpoint_when_404_names_v1_responses() {
        let body = r#"{"error":{"type":"invalid_request_error","message":"This model is not supported in the v1/chat/completions endpoint. Use the v1/responses endpoint instead"}}"#;
        assert_eq!(
            classify_openai_error(404, body),
            OpenaiErrorClass::WrongEndpoint
        );
    }

    #[test]
    fn classify_openai_error_should_return_transient_when_body_is_unparseable_or_5xx() {
        // Non-JSON body.
        assert_eq!(
            classify_openai_error(503, "Service Unavailable"),
            OpenaiErrorClass::Transient
        );
        // Well-formed JSON on a 5xx is still transient — status wins, the
        // message is never consulted.
        let body = r#"{"error":{"type":"server_error","message":"internal error"}}"#;
        assert_eq!(
            classify_openai_error(500, body),
            OpenaiErrorClass::Transient
        );
    }

    #[test]
    fn classify_openai_error_should_return_other_when_401_or_unmatched_4xx() {
        let auth_body =
            r#"{"error":{"type":"invalid_request_error","message":"Incorrect API key provided"}}"#;
        assert_eq!(
            classify_openai_error(401, auth_body),
            OpenaiErrorClass::Other
        );

        // A 400 that doesn't match any known deprecated/wrong-endpoint
        // substring must fall through to `Other`, not `Deprecated`.
        let unmatched_body = r#"{"error":{"type":"invalid_request_error","message":"Missing required parameter: 'messages'"}}"#;
        assert_eq!(
            classify_openai_error(400, unmatched_body),
            OpenaiErrorClass::Other
        );
    }

    #[test]
    fn classify_openai_error_should_classify_correctly_after_appending_new_table_entry() {
        // "no longer available" is a second table entry for the same
        // `Deprecated` class as "has been deprecated" — proving a new
        // phrasing is a table entry, not a `classify_openai_error` change.
        let body = r#"{"error":{"type":"invalid_request_error","message":"Model `gpt-4-vision-preview` is no longer available"}}"#;
        assert_eq!(
            classify_openai_error(400, body),
            OpenaiErrorClass::Deprecated
        );
    }

    #[test]
    fn classify_openai_error_should_return_other_not_deprecated_when_4xx_body_matches_no_table_entry(
    ) {
        // A structurally-valid 4xx error with a message matching no table
        // entry must return `Other`, never panic and never default to
        // `Deprecated`.
        let body = r#"{"error":{"type":"invalid_request_error","message":"You exceeded your current quota"}}"#;
        assert_eq!(classify_openai_error(422, body), OpenaiErrorClass::Other);
    }

    // ────────────────────────────────────────────────────────────────────
    // OpenaiToAnthropicStream
    // ────────────────────────────────────────────────────────────────────

    mod stream_translator {
        use super::*;
        use futures_util::stream;

        async fn drain<S>(s: S) -> Vec<Bytes>
        where
            S: Stream<Item = Result<Bytes, anyhow::Error>>,
        {
            s.map(|item| item.unwrap()).collect().await
        }

        fn sse(data: &str) -> Bytes {
            Bytes::from(format!("data: {data}\n\n"))
        }

        fn parse_event(frame: &Bytes) -> (String, Value) {
            let text = String::from_utf8(frame.to_vec()).unwrap();
            let mut event = String::new();
            let mut data = String::new();
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("event: ") {
                    event = rest.to_string();
                } else if let Some(rest) = line.strip_prefix("data: ") {
                    data = rest.to_string();
                }
            }
            (event, serde_json::from_str(&data).unwrap())
        }

        #[tokio::test]
        async fn content_delta_is_bracketed_by_start_and_stop_events() {
            let inner = stream::iter(vec![
                Ok(sse(
                    r#"{"choices":[{"delta":{"content":"Hi"},"finish_reason":null}]}"#,
                )),
                Ok(sse(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#)),
                Ok(sse("[DONE]")),
            ]);
            let translator = OpenaiToAnthropicStream::new(inner, "gpt-4o".to_string());
            let out = drain(translator).await;

            let events: Vec<String> = out.iter().map(|f| parse_event(f).0).collect();
            assert_eq!(
                events,
                vec![
                    "message_start",
                    "content_block_start",
                    "content_block_delta",
                    "content_block_stop",
                    "message_delta",
                    "message_stop",
                ]
            );

            let (_, delta_data) = parse_event(&out[2]);
            assert_eq!(delta_data["delta"]["text"], "Hi");

            let (_, message_delta_data) = parse_event(&out[4]);
            assert_eq!(message_delta_data["delta"]["stop_reason"], "end_turn");
        }

        #[tokio::test]
        async fn length_finish_reason_maps_to_max_tokens_stop_reason() {
            let inner = stream::iter(vec![Ok(sse(
                r#"{"choices":[{"delta":{"content":"x"},"finish_reason":"length"}]}"#,
            ))]);
            let translator = OpenaiToAnthropicStream::new(inner, "gpt-4o".to_string());
            let out = drain(translator).await;

            let (_, message_delta_data) = parse_event(&out[4]);
            assert_eq!(message_delta_data["delta"]["stop_reason"], "max_tokens");
        }

        #[tokio::test]
        async fn array_and_reasoning_deltas_produce_text() {
            // Gateway may send delta content as parts or put tokens in
            // `reasoning_content`; both must surface as text deltas rather
            // than vanishing (the empty-stream symptom).
            let inner = stream::iter(vec![
                Ok(sse(
                    r#"{"choices":[{"delta":{"content":[{"type":"text","text":"A"}]},"finish_reason":null}]}"#,
                )),
                Ok(sse(
                    r#"{"choices":[{"delta":{"reasoning_content":"th"},"finish_reason":null}]}"#,
                )),
                Ok(sse(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#)),
                Ok(sse("[DONE]")),
            ]);
            let translator = OpenaiToAnthropicStream::new(inner, "gpt-4o".to_string());
            let out = drain(translator).await;

            let texts: Vec<String> = out
                .iter()
                .map(parse_event)
                .filter(|(e, _)| e == "content_block_delta")
                .map(|(_, d)| d["delta"]["text"].as_str().unwrap_or("").to_string())
                .collect();
            assert_eq!(texts, vec!["A".to_string(), "th".to_string()]);
        }

        #[tokio::test]
        #[allow(clippy::expect_used)]
        async fn chunked_tool_call_accumulates_into_one_tool_use_block() {
            // Arguments split across chunks must reassemble: block indices
            // stable, partial_json concatenates to valid JSON, stop is
            // tool_use. This is the streaming half of the premature-stop
            // repro (Claude Code streams).
            let inner = stream::iter(vec![
                Ok(sse(
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"get_time","arguments":""}}]},"finish_reason":null}]}"#,
                )),
                Ok(sse(
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"zone\":"}}]},"finish_reason":null}]}"#,
                )),
                Ok(sse(
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"utc\"}"}}]},"finish_reason":null}]}"#,
                )),
                Ok(sse(
                    r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
                )),
                Ok(sse("[DONE]")),
            ]);
            let translator = OpenaiToAnthropicStream::new(inner, "gpt-4o".to_string());
            let out = drain(translator).await;
            let events: Vec<(String, Value)> = out.iter().map(parse_event).collect();
            let kinds: Vec<&str> = events.iter().map(|(e, _)| e.as_str()).collect();

            // Text block 0 opens (possibly empty), tool block takes index 1.
            let tool_start = events
                .iter()
                .find(|(e, d)| {
                    e == "content_block_start" && d["content_block"]["type"] == "tool_use"
                })
                .expect("tool_use content_block_start");
            assert_eq!(tool_start.1["index"], 1);
            assert_eq!(tool_start.1["content_block"]["id"], "call_1");
            assert_eq!(tool_start.1["content_block"]["name"], "get_time");

            let partials: String = events
                .iter()
                .filter(|(e, d)| {
                    e == "content_block_delta" && d["delta"]["type"] == "input_json_delta"
                })
                .map(|(_, d)| {
                    d["delta"]["partial_json"]
                        .as_str()
                        .unwrap_or("")
                        .to_string()
                })
                .collect();
            assert_eq!(partials, "{\"zone\":\"utc\"}");
            serde_json::from_str::<Value>(&partials).expect("reassembled args parse");

            // Every start has a matching stop on the same index.
            for idx in [0, 1] {
                let starts = events
                    .iter()
                    .filter(|(e, d)| e == "content_block_start" && d["index"] == idx)
                    .count();
                let stops = events
                    .iter()
                    .filter(|(e, d)| e == "content_block_stop" && d["index"] == idx)
                    .count();
                assert_eq!((starts, stops), (1, 1), "bracket mismatch at {idx}");
            }

            let (_, message_delta_data) = parse_event(&out[out.len() - 2]);
            assert_eq!(message_delta_data["delta"]["stop_reason"], "tool_use");
            assert_eq!(kinds.last(), Some(&"message_stop"));
        }

        #[tokio::test]
        async fn empty_stream_still_produces_well_formed_bracketing_events() {
            let inner: futures_util::stream::Iter<
                std::vec::IntoIter<Result<Bytes, anyhow::Error>>,
            > = stream::iter(vec![]);
            let translator = OpenaiToAnthropicStream::new(inner, "gpt-4o".to_string());
            let out = drain(translator).await;

            let events: Vec<String> = out.iter().map(|f| parse_event(f).0).collect();
            assert_eq!(
                events,
                vec![
                    "message_start",
                    "content_block_start",
                    "content_block_stop",
                    "message_delta",
                    "message_stop",
                ]
            );
        }
    }
}
