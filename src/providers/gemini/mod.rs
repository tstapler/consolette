//! `GeminiProvider`: Cloud Code Assist upstream (ADR-001/ADR-003).
//!
//! Non-streaming text completions (Epic 1.3): `send()` translates an
//! Anthropic Messages request into the Cloud Code Assist `CloudCodeEnvelope`
//! (`translate.rs`), POSTs it to `v1internal:generateContent`, strictly
//! parses the 2xx body (ADR-002 — a parse failure becomes
//! `ProviderError::ResponseShapeMismatch`, never a lenient default), and
//! translates the response back to Anthropic shape. Streaming (`stream.rs`)
//! and tool calls (`tools.rs`'s `ThoughtSignatureCache`/`GeminiToolCallState`
//! becoming load-bearing) land in later phases.

mod error;
mod stream;
mod tools;
mod translate;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use http::HeaderMap;
use reqwest::{Client, StatusCode};
use serde_json::Value;
use tracing::debug;

use crate::auth::exec::ExecCredentialCache;
use crate::auth::SecretResolver;
use crate::config::schema::{Upstream, UpstreamKind};

use super::anthropic::apply_auth_headers;
use super::{ModelInfo, Provider, ProviderError, ProviderResponse};

pub(crate) use error::DRIFT_COOLDOWN_SECS;
use error::{classify_gemini_error, GeminiErrorBody};
use stream::GeminiToAnthropicStream;
use tools::ThoughtSignatureCache;
#[cfg(test)]
use tools::ToolUseId;
use translate::{
    translate_anthropic_request_to_gemini, translate_gemini_response_to_anthropic,
    GeminiGenerateContentResponse,
};

/// Provider for Google's Cloud Code Assist endpoint, structurally mirroring
/// `OpenaiProvider` more than `AnthropicProvider` (see Domain Glossary).
pub struct GeminiProvider {
    /// Pooled client for non-streaming requests.
    client: Client,
    /// Non-pooled client for SSE streaming (prevents pool exhaustion, ADR-004).
    /// Used by `send_streaming_request` (Story 2.1.2) — constructed eagerly
    /// (not lazily) to match `AnthropicProvider`/`OpenaiProvider`'s
    /// established two-client-at-construction-time shape.
    stream_client: Client,
    /// Base URL for the Cloud Code Assist API — hardcoded, matching
    /// `AnthropicProvider`'s precedent (`UpstreamKind::Gemini` carries no
    /// base-URL override field).
    base_url: String,
    /// The upstream this provider was constructed for — supplies `name`
    /// (for exec-cache keying/logging) and (via `UpstreamKind::Gemini::project_id`)
    /// the Cloud Code Assist envelope's `project` field.
    upstream: Arc<Upstream>,
    /// Resolves `SecretRef`s (env/keychain/inline) to plaintext.
    resolver: Arc<dyn SecretResolver + Send + Sync>,
    /// Shared cache for `exec` auth-method subprocess results.
    exec_cache: Arc<ExecCredentialCache>,
    // Scaffolded per project_plans/gemini-provider/implementation/plan.md
    // Story 1.6.1 — provider-owned so it survives across the separate
    // send() calls Story 3.3.1 needs to bridge; populated/read starting in
    // Story 3.3.1, once tool calls exist. Type defined in tools.rs.
    thought_signatures: ThoughtSignatureCache,
}

impl GeminiProvider {
    /// Construct a new `GeminiProvider` for one configured `Upstream`.
    ///
    /// Follows the ADR-004 two-client split identically to
    /// `AnthropicProvider::new`/`OpenaiProvider::new`.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError::Upstream`] if either reqwest `Client`
    /// fails to build (e.g. an invalid TLS backend configuration).
    pub fn new(
        upstream: Arc<Upstream>,
        resolver: Arc<dyn SecretResolver + Send + Sync>,
        exec_cache: Arc<ExecCredentialCache>,
        request_timeout_secs: u64,
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

        Ok(Self {
            client,
            stream_client,
            base_url: "https://cloudcode-pa.googleapis.com".to_string(),
            upstream,
            resolver,
            exec_cache,
            thought_signatures: ThoughtSignatureCache::new(),
        })
    }

    /// Test-only accessor proving `thought_signatures` is a field constructed
    /// once in `new()` — surviving across separate `send()` calls on the same
    /// instance — rather than being (re)constructed per call (Story 1.6.1).
    #[cfg(test)]
    fn thought_signatures(&self) -> &ThoughtSignatureCache {
        &self.thought_signatures
    }

    /// The configured Cloud Code Assist project id (ADR-003), used verbatim
    /// as `CloudCodeEnvelope.project` on every outgoing request.
    #[must_use]
    pub fn project_id(&self) -> &str {
        match &self.upstream.kind {
            UpstreamKind::Gemini { project_id } => project_id,
            // Can't happen — GeminiProvider is only ever constructed for a
            // Gemini-kind upstream, per build_providers's match arm.
            other => unreachable!("GeminiProvider constructed for non-Gemini upstream: {other:?}"),
        }
    }

    /// Build the outgoing request headers: `Content-Type` plus auth per the
    /// upstream's configured `AuthMethod`.
    ///
    /// On an auth failure, logs a `gemini`-specific, actionable message
    /// (Task 1.2.2b, folded into this story) distinct from `exec.rs`'s
    /// generic `tracing::warn!` — this one names the actual remediation
    /// command for a locally-detected expired/missing Antigravity token.
    async fn build_headers(&self, url: &str) -> Result<HeaderMap, ProviderError> {
        let mut out = HeaderMap::new();
        out.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );

        match apply_auth_headers(
            &self.upstream,
            self.resolver.as_ref(),
            &self.exec_cache,
            &mut out,
            url,
        )
        .await
        {
            Ok(()) => Ok(out),
            Err(ProviderError::Auth(msg)) => {
                tracing::error!(
                    upstream = "gemini",
                    %msg,
                    "gemini upstream: token refresh failed — run 'antigravity-cli login' or reopen the Antigravity IDE to mint a fresh token"
                );
                Err(ProviderError::Auth(msg))
            }
            Err(e) => Err(e),
        }
    }

    /// Send a non-streaming request to `POST /v1internal:generateContent`.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if header construction/auth fails, the
    /// request times out, the upstream responds with a non-2xx status, or
    /// the 2xx body doesn't match the documented shape
    /// (`ProviderError::ResponseShapeMismatch`, ADR-002).
    pub async fn send_request(&self, body: Value) -> Result<Value, ProviderError> {
        let url = format!("{}/v1internal:generateContent", self.base_url);
        let headers = self.build_headers(&url).await?;

        let model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("gemini-3-pro")
            .to_string();

        let envelope = translate_anthropic_request_to_gemini(&body, self.project_id())?;
        let body_bytes = serde_json::to_vec(&envelope).map_err(|e| ProviderError::Upstream {
            status: 0,
            body: e.to_string(),
        })?;

        debug!("Gemini non-stream POST {url}");

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
        let bytes = response
            .bytes()
            .await
            .map_err(|e| ProviderError::Upstream {
                status: status.as_u16(),
                body: e.to_string(),
            })?;

        if !status.is_success() {
            let err = classify_error_response(status, &bytes);
            // Task 1.3.4c: a real HTTP 401/403 from Gemini itself (meaning
            // apply_auth_headers already succeeded — the local token was NOT
            // expired) is a distinct signal from build_headers's
            // locally-detected-expiry log line above: possible account
            // suspension/revocation, not routine expiry.
            if err.is_auth() {
                tracing::error!(
                    "gemini upstream: request rejected with 401/403 despite a non-expired local token — this may indicate account suspension/revocation (see requirements.md's accepted ToS risk), not routine expiry; running 'antigravity-cli login' will not fix a suspension"
                );
            }
            return Err(err);
        }

        translate_success_bytes(&bytes, &model)
    }

    /// Constructs the (unsent) outgoing `streamGenerateContent` request —
    /// split out from [`Self::send_streaming_request`] so Task 2.1.2b's test
    /// can assert the URL/method/headers directly against a captured
    /// `reqwest::Request`, without a live network call (no HTTP-mock crate
    /// in this repo — see validation.md's Test Stack Notes). Always issued
    /// via `self.stream_client`, never `self.client` (ADR-004).
    fn build_stream_request(
        &self,
        headers: HeaderMap,
        body_bytes: Vec<u8>,
    ) -> Result<reqwest::Request, ProviderError> {
        let url = format!("{}/v1internal:streamGenerateContent?alt=sse", self.base_url);
        self.stream_client
            .post(&url)
            .headers(headers)
            .body(body_bytes)
            .build()
            .map_err(|e| ProviderError::Upstream {
                status: 0,
                body: e.to_string(),
            })
    }

    /// Send a streaming request to `POST /v1internal:streamGenerateContent?alt=sse`
    /// via `self.stream_client` (the `pool_max_idle_per_host(0)` client,
    /// ADR-004 — not `self.client`), returning the raw upstream
    /// `reqwest::Response` for the caller to wrap in a
    /// [`stream::GeminiToAnthropicStream`].
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if header construction/auth fails, the
    /// request times out, or the upstream responds with a non-2xx status.
    pub async fn send_streaming_request(&self, body: &Value) -> Result<reqwest::Response, ProviderError> {
        let url = format!("{}/v1internal:streamGenerateContent?alt=sse", self.base_url);
        let mut headers = self.build_headers(&url).await?;
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("text/event-stream"),
        );

        let envelope = translate_anthropic_request_to_gemini(body, self.project_id())?;
        let body_bytes = serde_json::to_vec(&envelope).map_err(|e| ProviderError::Upstream {
            status: 0,
            body: e.to_string(),
        })?;

        let request = self.build_stream_request(headers, body_bytes)?;
        debug!("Gemini stream {} {}", request.method(), request.url());

        let response = self
            .stream_client
            .execute(request)
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
            let bytes = response
                .bytes()
                .await
                .map_err(|e| ProviderError::Upstream {
                    status: status.as_u16(),
                    body: e.to_string(),
                })?;
            return Err(classify_error_response(status, &bytes));
        }

        Ok(response)
    }

    /// Fetch the list of available models from
    /// `POST /v1internal:fetchAvailableModels`.
    ///
    /// Uses POST (per plan.md's Unresolved Questions note: "try POST first,
    /// matching `generateContent`'s method" — no live token was available to
    /// verify this against the real endpoint; adjust if a live call 405s).
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if header construction/auth fails, the
    /// request times out, or the upstream responds with a non-2xx status.
    pub async fn fetch_models(&self) -> Result<Value, ProviderError> {
        let url = format!("{}/v1internal:fetchAvailableModels", self.base_url);
        let headers = self.build_headers(&url).await?;

        debug!("Gemini POST {url}");

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .body("{}")
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
            let body_str = response.text().await.unwrap_or_default();
            return Err(ProviderError::Upstream {
                status: status.as_u16(),
                body: body_str,
            });
        }

        response.json().await.map_err(|e| ProviderError::Upstream {
            status: status.as_u16(),
            body: e.to_string(),
        })
    }
}

/// Parses a non-2xx Gemini response body into a `ProviderError`. Falls back
/// to a plain `Upstream` error if the body itself isn't the expected
/// `GeminiErrorBody` shape (e.g. an upstream proxy's own HTML error page).
fn classify_error_response(status: StatusCode, bytes: &[u8]) -> ProviderError {
    match serde_json::from_slice::<GeminiErrorBody>(bytes) {
        Ok(error_body) => classify_gemini_error(status, &error_body),
        Err(_) => ProviderError::Upstream {
            status: status.as_u16(),
            body: String::from_utf8_lossy(bytes).into_owned(),
        },
    }
}

/// Strictly parses a 2xx `generateContent` response body and translates it
/// to Anthropic shape. Extracted as a pure function (no network I/O) so it's
/// directly fixture-testable per Task 1.3.4f's rescoped scope (validation.md
/// Test Stack Notes) — no HTTP-mocking crate needed.
///
/// # Errors
///
/// Returns [`ProviderError::ResponseShapeMismatch`] if `bytes` doesn't
/// strictly deserialize into `GeminiGenerateContentResponse` (ADR-002 — no
/// `.unwrap_or_default()` fallback).
fn translate_success_bytes(bytes: &[u8], model: &str) -> Result<Value, ProviderError> {
    let parsed: GeminiGenerateContentResponse = serde_json::from_slice(bytes)
        .map_err(|e| ProviderError::ResponseShapeMismatch(e.to_string()))?;
    Ok(translate_gemini_response_to_anthropic(&parsed, model))
}

/// Parses a `fetchAvailableModels` response body into `Vec<ModelInfo>`.
/// Extracted as a pure function for the same fixture-testability reason as
/// `translate_success_bytes`. Exact response shape is unverified (plan.md's
/// Unresolved Questions) — this accepts `{"models":[{"name": "..."}]}`,
/// stripping a `"models/"` resource-name prefix if present (the shape
/// Google's public Generative Language API uses), on the theory that Cloud
/// Code Assist's internal endpoint likely follows the same convention;
/// confirm/adjust against the real response during Task 1.3.4h.
fn parse_available_models(value: &Value) -> Vec<ModelInfo> {
    value
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| {
            let raw_name = entry.get("name").and_then(Value::as_str)?;
            let id = raw_name
                .strip_prefix("models/")
                .unwrap_or(raw_name)
                .to_string();
            Some(ModelInfo {
                id,
                owned_by: Some("google".to_string()),
            })
        })
        .collect()
}

#[async_trait]
impl Provider for GeminiProvider {
    fn name(&self) -> &'static str {
        "gemini"
    }

    async fn send(
        &self,
        body: Value,
        _headers: HeaderMap,
        stream: bool,
    ) -> Result<ProviderResponse, ProviderError> {
        if stream {
            let model = body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("gemini-3-pro")
                .to_string();
            let response = self.send_streaming_request(&body).await?;
            let byte_stream = response
                .bytes_stream()
                .map(|r| r.map_err(anyhow::Error::from));
            let translated = GeminiToAnthropicStream::new(byte_stream, model);
            return Ok(ProviderResponse::Stream(Box::pin(translated)));
        }
        let value = self.send_request(body).await?;
        Ok(ProviderResponse::Full(value))
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let value = self.fetch_models().await?;
        Ok(parse_available_models(&value))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_upstream(project_id: &str) -> Arc<Upstream> {
        Arc::new(Upstream {
            name: "gemini".to_string(),
            kind: UpstreamKind::Gemini {
                project_id: project_id.to_string(),
            },
            auth: None,
        })
    }

    fn test_provider() -> GeminiProvider {
        GeminiProvider::new(
            test_upstream("my-gcp-project"),
            Arc::new(crate::auth::SystemSecretResolver),
            Arc::new(ExecCredentialCache::new()),
            60,
        )
        .unwrap()
    }

    // REQ-14 (Story 1.7.1): `project_id()` returns the exact configured
    // string from `UpstreamKind::Gemini`, never a default/guess.
    #[test]
    fn project_id_accessor_should_return_configured_project_id_from_upstream_kind_gemini() {
        let provider = test_provider();
        assert_eq!(provider.project_id(), "my-gcp-project");
    }

    // REQ-6 (Story 1.3.4)
    #[test]
    fn gemini_provider_new_should_construct_two_distinct_reqwest_clients_matching_adr_004() {
        let provider = test_provider();
        assert_eq!(provider.base_url, "https://cloudcode-pa.googleapis.com");
        // `reqwest::Client` exposes no public identity check; distinctness
        // is enforced structurally by the two separate `Client::builder()`
        // calls in `GeminiProvider::new` (one pooled, one
        // `pool_max_idle_per_host(0)`), matching `AnthropicProvider::new`'s
        // ADR-004 pattern exactly (both fields exist and both builds
        // succeeded, asserted by `test_provider()`'s `.unwrap()` above).
    }

    // REQ-6 — pure-function fixture test (validation.md Test Stack Notes):
    // no live HTTP mock, feeds the STOP-finish-reason fixture directly
    // through the post-parse translation path `send()` uses internally.
    #[test]
    fn send_should_return_anthropic_shaped_full_response_when_upstream_returns_stop_finish_reason()
    {
        let body = br#"{
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "hello"}]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5, "totalTokenCount": 15}
        }"#;

        let result = translate_success_bytes(body, "gemini-3-pro").unwrap();

        assert_eq!(result["content"][0]["text"], "hello");
        assert_eq!(result["stop_reason"], "end_turn");
    }

    // REQ-8 (Story 1.4.2, ADR-002) — focus area.
    #[test]
    fn send_should_return_response_shape_mismatch_when_candidates_field_is_missing() {
        let body =
            br#"{"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":0,"totalTokenCount":1}}"#;

        let err = translate_success_bytes(body, "gemini-3-pro").unwrap_err();

        match err {
            ProviderError::ResponseShapeMismatch(msg) => {
                assert!(
                    msg.contains("candidates"),
                    "expected the serde error to mention `candidates`, got: {msg}"
                );
            }
            other => panic!("expected ResponseShapeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn send_should_return_response_shape_mismatch_when_body_is_truncated_json() {
        let body = br#"{"candidates":[{"content":"#;

        let err = translate_success_bytes(body, "gemini-3-pro").unwrap_err();

        assert!(matches!(err, ProviderError::ResponseShapeMismatch(_)));
    }

    #[test]
    fn send_should_return_ok_when_candidates_present_and_well_formed() {
        let body = br#"{
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "hi"}]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2}
        }"#;

        assert!(translate_success_bytes(body, "gemini-3-pro").is_ok());
    }

    // REQ-6 (Story 1.3.4d/f) — pure-function fixture test, exact
    // `fetchAvailableModels` shape unverified (see `parse_available_models`'s
    // doc comment).
    #[test]
    fn list_models_should_return_gemini_3_pro_from_fetch_available_models() {
        let value = json!({
            "models": [
                {"name": "models/gemini-3-pro", "displayName": "Gemini 3 Pro"}
            ]
        });

        let models = parse_available_models(&value);

        assert_eq!(
            models,
            vec![ModelInfo {
                id: "gemini-3-pro".to_string(),
                owned_by: Some("google".to_string()),
            }]
        );
    }

    // REQ-19 (Story 2.1.2) — rescoped per validation.md's Test Stack Notes:
    // no HTTP-mock crate exists in this repo, so this asserts the outgoing
    // request's construction (client selection, URL, headers) directly
    // against a captured `reqwest::Request`, built via the pure
    // `build_stream_request` helper (no network I/O), rather than driving a
    // live/mocked HTTP round trip.
    #[test]
    fn send_should_use_stream_client_and_stream_generate_content_endpoint_when_stream_true() {
        let provider = test_provider();
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("text/event-stream"),
        );

        let request = provider
            .build_stream_request(headers, b"{}".to_vec())
            .unwrap();

        assert_eq!(request.method(), reqwest::Method::POST);
        assert_eq!(
            request.url().as_str(),
            "https://cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            request
                .headers()
                .get(reqwest::header::ACCEPT)
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );
        // `stream_client` selection (never `self.client`) is structurally
        // guaranteed by `build_stream_request`'s body — `reqwest::Request`
        // carries no runtime client identity to assert against directly,
        // the same fixture-testing constraint documented on
        // `gemini_provider_new_should_construct_two_distinct_reqwest_clients_matching_adr_004`
        // above.
    }

    #[test]
    fn classify_error_response_should_fall_back_to_upstream_when_body_is_not_gemini_error_shape() {
        let err = classify_error_response(StatusCode::BAD_GATEWAY, b"<html>502</html>");
        assert!(matches!(err, ProviderError::Upstream { status: 502, .. }));
    }

    // REQ-13 (Story 1.6.1) — the thought_signatures field is constructed
    // once in `new()`, not per `send()` call: a value inserted before either
    // call is still readable via the same cache instance after both
    // complete. `test_upstream`'s `auth: None` makes both `send()` calls
    // fail fast on `ProviderError::Auth` before any network I/O — this test
    // only cares about the cache's lifetime, not `send()`'s outcome, so
    // that's fine and keeps the test hermetic.
    #[tokio::test]
    async fn thought_signature_cache_should_persist_across_two_sequential_send_calls_on_same_provider_instance(
    ) {
        let provider = test_provider();
        let id = ToolUseId::from("toolu_01".to_string());
        provider
            .thought_signatures()
            .insert("session-x", id.clone(), "sig-1".to_string());

        let body = json!({
            "model": "gemini-3-pro",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 16
        });

        let _ = provider.send(body.clone(), HeaderMap::new(), false).await;
        let _ = provider.send(body, HeaderMap::new(), false).await;

        assert_eq!(
            provider.thought_signatures().get("session-x", &id),
            Some("sig-1".to_string()),
            "same ThoughtSignatureCache instance must persist across sequential send() calls"
        );
    }
}
