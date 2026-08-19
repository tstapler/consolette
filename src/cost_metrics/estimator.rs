//! Token estimators (Epic 1.2) — the Strategy pattern's two implementors of
//! [`TokenEstimator`]: a local/free `tiktoken`-backed guess for the
//! OpenAI-side counterfactual, and Anthropic's real `count_tokens` API for
//! the exact Anthropic-side counterfactual.
//!
//! See `project_plans/compaction-cost-metrics/implementation/plan.md` Epic
//! 1.2 for the full design and its acceptance criteria.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::auth::exec::ExecCredentialCache;
use crate::auth::SecretResolver;
use crate::config::schema::Upstream;
use crate::providers::anthropic::apply_auth_headers;

use super::types::{EstimatorKind, TokenCount, TokenSource};

/// Metadata that accompanies a [`TokenEstimator::estimate`] result: whatever
/// couldn't be folded into the token count itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EstimateMeta {
    /// `true` if `messages` contained at least one non-text content block
    /// (e.g. `image`) that was excluded from the token count rather than
    /// naively serialized and counted.
    pub truncated_content: bool,
}

/// Errors a [`TokenEstimator`] implementor can return. Distinct from
/// [`crate::providers::ProviderError`] — estimators are a hypothetical,
/// never-sent-request path, not the live provider request path, so they get
/// their own small error type rather than overloading `ProviderError`'s
/// variants (most of which don't apply here, e.g. `Validation`).
#[derive(Debug, thiserror::Error)]
pub enum EstimatorError {
    /// Non-200/429 response, or any other request-construction/transport
    /// failure that isn't a timeout.
    #[error("token estimator upstream failure: {0}")]
    UpstreamFailure(String),
    /// The request timed out.
    #[error("token estimator request timed out")]
    Timeout,
    /// The upstream responded `429`. Callers get exactly one attempt — no
    /// retry loop lives inside the estimator (see plan.md Story 1.2.2).
    #[error("token estimator was rate limited")]
    RateLimited,
}

/// Strategy interface for producing a [`TokenCount`] estimate for a
/// `messages` array against a given `model`, without ever sending the real
/// request.
#[async_trait]
pub trait TokenEstimator: Send + Sync {
    /// # Errors
    ///
    /// Returns [`EstimatorError`] if the estimate can't be produced (e.g. a
    /// remote `count_tokens` call fails) — callers must treat this as "no
    /// estimate available," never substitute a silent `0`.
    async fn estimate(
        &self,
        model: &str,
        messages: &Value,
    ) -> Result<(TokenCount, EstimateMeta), EstimatorError>;
}

// ---------------------------------------------------------------------------
// Shared text extraction (Task 1.2.1b)
// ---------------------------------------------------------------------------

/// Extract the concatenated estimable text from a `messages` array, plus
/// whether any non-text block (image, or any block type this function
/// doesn't recognize) was seen and excluded.
///
/// Per-block-type handling (not naive `to_string()`-and-tokenize):
/// - `content` as a plain string is used directly.
/// - `text` blocks contribute their `.text` string.
/// - `tool_use` blocks contribute the string leaves of `.input` (its
///   arguments), not the surrounding JSON punctuation.
/// - `tool_result` blocks recurse into their own `.content` (string or
///   array of blocks).
/// - Anything else (notably `image`) is excluded from the text and sets the
///   returned flag.
#[must_use]
pub fn extract_estimable_text(messages: &Value) -> (String, bool) {
    let mut text = String::new();
    let mut saw_non_text_block = false;

    if let Some(array) = messages.as_array() {
        for message in array {
            extract_from_message(message, &mut text, &mut saw_non_text_block);
        }
    }

    (text, saw_non_text_block)
}

fn extract_from_message(message: &Value, text: &mut String, saw_non_text_block: &mut bool) {
    if let Some(content) = message.get("content") {
        extract_from_content(content, text, saw_non_text_block);
    }
}

fn extract_from_content(content: &Value, text: &mut String, saw_non_text_block: &mut bool) {
    match content {
        Value::String(s) => push_text(text, s),
        Value::Array(blocks) => {
            for block in blocks {
                extract_block(block, text, saw_non_text_block);
            }
        }
        _ => {}
    }
}

fn extract_block(block: &Value, text: &mut String, saw_non_text_block: &mut bool) {
    let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
    match block_type {
        "text" => {
            if let Some(s) = block.get("text").and_then(Value::as_str) {
                push_text(text, s);
            }
        }
        "tool_use" => {
            if let Some(input) = block.get("input") {
                collect_string_leaves(input, text);
            }
        }
        "tool_result" => {
            if let Some(inner_content) = block.get("content") {
                extract_from_content(inner_content, text, saw_non_text_block);
            }
        }
        // Includes "image" and any block type this function doesn't
        // recognize — excluded from the token text, not naively stringified.
        _ => {
            *saw_non_text_block = true;
        }
    }
}

fn push_text(text: &mut String, s: &str) {
    if !text.is_empty() {
        text.push(' ');
    }
    text.push_str(s);
}

/// Collect every string leaf in a JSON value, space-joined, ignoring keys
/// and structural punctuation. Used for `tool_use.input`, whose argument
/// values are the estimable content, not the JSON shape around them.
fn collect_string_leaves(value: &Value, text: &mut String) {
    match value {
        Value::String(s) => push_text(text, s),
        Value::Array(items) => {
            for item in items {
                collect_string_leaves(item, text);
            }
        }
        Value::Object(map) => {
            for v in map.values() {
                collect_string_leaves(v, text);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// TiktokenEstimator (Story 1.2.1)
// ---------------------------------------------------------------------------

/// Which `tiktoken-rs` BPE encoding to use for a given model name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TiktokenEncoding {
    Cl100k,
    O200k,
}

/// Pick an encoding for `model`. Defaults to `o200k_base` for unknown
/// models, matching `tiktoken-rs`'s own `get_bpe_from_model` fallback
/// behavior (Task 1.2.1c).
fn encoding_for_model(model: &str) -> TiktokenEncoding {
    let model = model.to_ascii_lowercase();
    if model.starts_with("gpt-3.5")
        || model.starts_with("gpt-4-")
        || model == "gpt-4"
        || model.starts_with("text-embedding")
    {
        TiktokenEncoding::Cl100k
    } else {
        TiktokenEncoding::O200k
    }
}

/// Local, free, synchronous token estimate backed by `tiktoken-rs`, for
/// OpenAI-routed counterfactuals — never makes a network call.
#[derive(Debug, Default, Clone, Copy)]
pub struct TiktokenEstimator;

impl TiktokenEstimator {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl TokenEstimator for TiktokenEstimator {
    async fn estimate(
        &self,
        model: &str,
        messages: &Value,
    ) -> Result<(TokenCount, EstimateMeta), EstimatorError> {
        let (text, saw_non_text_block) = extract_estimable_text(messages);

        let (count, kind) = match encoding_for_model(model) {
            TiktokenEncoding::Cl100k => {
                let bpe = tiktoken_rs::cl100k_base_singleton();
                let bpe = bpe.lock();
                (
                    bpe.encode_with_special_tokens(&text).len(),
                    EstimatorKind::TiktokenCl100k,
                )
            }
            TiktokenEncoding::O200k => {
                let bpe = tiktoken_rs::o200k_base_singleton();
                let bpe = bpe.lock();
                (
                    bpe.encode_with_special_tokens(&text).len(),
                    EstimatorKind::TiktokenO200k,
                )
            }
        };

        Ok((
            TokenCount {
                value: count as u64,
                source: TokenSource::Estimated { via: kind },
            },
            EstimateMeta {
                truncated_content: saw_non_text_block,
            },
        ))
    }
}

// ---------------------------------------------------------------------------
// AnthropicCountTokensEstimator (Story 1.2.2)
// ---------------------------------------------------------------------------

/// Calls Anthropic's real `POST /v1/messages/count_tokens` for an exact
/// Anthropic-side counterfactual (tiktoken undercounts Claude tokens
/// 15-30%+ per Anthropic's own guidance, so a guess isn't good enough here).
///
/// Reuses the exact credential-resolution path `src/providers/anthropic.rs`
/// uses for live calls (via [`apply_auth_headers`]) rather than duplicating
/// secret lookup (Task 1.2.2a).
pub struct AnthropicCountTokensEstimator {
    client: Client,
    base_url: String,
    upstream: Arc<Upstream>,
    resolver: Arc<dyn SecretResolver + Send + Sync>,
    exec_cache: Arc<ExecCredentialCache>,
}

impl AnthropicCountTokensEstimator {
    pub const DEFAULT_BASE_URL: &'static str = "https://api.anthropic.com";

    /// Construct an estimator that calls the real Anthropic API.
    #[must_use]
    pub fn new(
        upstream: Arc<Upstream>,
        resolver: Arc<dyn SecretResolver + Send + Sync>,
        exec_cache: Arc<ExecCredentialCache>,
    ) -> Self {
        Self::with_base_url(
            upstream,
            resolver,
            exec_cache,
            Self::DEFAULT_BASE_URL.to_string(),
        )
    }

    /// Construct an estimator pointed at `base_url` instead of the real
    /// Anthropic API — used by tests to point at the hand-rolled mock
    /// server (Task 1.2.2c).
    ///
    /// # Panics
    ///
    /// Panics if the underlying `reqwest::Client` fails to build. In
    /// practice this can't happen: the only configuration applied is a
    /// timeout, and this crate pins a single fixed TLS backend
    /// (`rustls-tls` in `Cargo.toml`), which is the only thing that can
    /// make `build()` fail.
    #[must_use]
    pub fn with_base_url(
        upstream: Arc<Upstream>,
        resolver: Arc<dyn SecretResolver + Send + Sync>,
        exec_cache: Arc<ExecCredentialCache>,
        base_url: String,
    ) -> Self {
        // Timeout-only client config; `build()` only fails on TLS backend
        // misconfiguration, which can't happen with this crate's fixed
        // `rustls-tls` feature selection (see `Cargo.toml`).
        #[allow(clippy::expect_used)]
        let client = Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("reqwest client with only a timeout set should always build");

        Self {
            client,
            base_url,
            upstream,
            resolver,
            exec_cache,
        }
    }
}

#[async_trait]
impl TokenEstimator for AnthropicCountTokensEstimator {
    async fn estimate(
        &self,
        model: &str,
        messages: &Value,
    ) -> Result<(TokenCount, EstimateMeta), EstimatorError> {
        let (text, saw_non_text_block) = extract_estimable_text(messages);
        let url = format!("{}/v1/messages/count_tokens", self.base_url);

        let mut headers = http::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            "anthropic-version",
            reqwest::header::HeaderValue::from_static("2023-06-01"),
        );
        apply_auth_headers(
            &self.upstream,
            self.resolver.as_ref(),
            &self.exec_cache,
            &mut headers,
            &url,
        )
        .await
        .map_err(|e| EstimatorError::UpstreamFailure(e.to_string()))?;

        let body = serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": text}],
        });

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    EstimatorError::Timeout
                } else {
                    EstimatorError::UpstreamFailure(e.to_string())
                }
            })?;

        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            // No retry loop here by design (Story 1.2.2 acceptance
            // criteria) — retries, if any, are the caller's decision.
            return Err(EstimatorError::RateLimited);
        }
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            return Err(EstimatorError::UpstreamFailure(format!(
                "status {status}: {body_text}"
            )));
        }

        let parsed: Value = response
            .json()
            .await
            .map_err(|e| EstimatorError::UpstreamFailure(e.to_string()))?;
        let input_tokens = parsed
            .get("input_tokens")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                EstimatorError::UpstreamFailure(
                    "count_tokens response missing input_tokens".to_string(),
                )
            })?;

        Ok((
            TokenCount {
                value: input_tokens,
                source: TokenSource::Estimated {
                    via: EstimatorKind::AnthropicCountTokensApi,
                },
            },
            EstimateMeta {
                truncated_content: saw_non_text_block,
            },
        ))
    }
}

// ---------------------------------------------------------------------------
// BoundedEstimator (Task 1.2.2b)
// ---------------------------------------------------------------------------

/// Wraps any [`TokenEstimator`] with a concurrency cap, so a burst of
/// compactions cannot open unbounded outbound connections to a remote
/// estimator (namely [`AnthropicCountTokensEstimator`]).
pub struct BoundedEstimator<E: TokenEstimator> {
    inner: E,
    permits: Arc<Semaphore>,
}

impl<E: TokenEstimator> BoundedEstimator<E> {
    #[must_use]
    pub fn new(inner: E, max_concurrent: usize) -> Self {
        Self {
            inner,
            permits: Arc::new(Semaphore::new(max_concurrent)),
        }
    }
}

#[async_trait]
impl<E: TokenEstimator> TokenEstimator for BoundedEstimator<E> {
    async fn estimate(
        &self,
        model: &str,
        messages: &Value,
    ) -> Result<(TokenCount, EstimateMeta), EstimatorError> {
        let _permit = self.permits.acquire().await.map_err(|_| {
            EstimatorError::UpstreamFailure("estimator semaphore was closed".to_string())
        })?;
        self.inner.estimate(model, messages).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // test assertions on values we just constructed/mocked
mod tests {
    use super::*;
    use crate::auth::SystemSecretResolver;
    use crate::config::schema::{AuthMethod, SecretRef, UpstreamKind};
    use crate::cost_metrics::test_support::{MockResponse, MockServer};
    use serde_json::json;
    use std::sync::atomic::Ordering;

    fn test_upstream() -> Arc<Upstream> {
        Arc::new(Upstream {
            name: "anthropic-test".to_string(),
            kind: UpstreamKind::Anthropic,
            auth: Some(AuthMethod::Apikey {
                key: SecretRef::Inline {
                    value: "test-api-key".to_string(),
                },
                header: "x-api-key".to_string(),
            }),
        })
    }

    fn test_estimator(base_url: String) -> AnthropicCountTokensEstimator {
        AnthropicCountTokensEstimator::with_base_url(
            test_upstream(),
            Arc::new(SystemSecretResolver),
            Arc::new(ExecCredentialCache::new()),
            base_url,
        )
    }

    #[tokio::test]
    async fn tiktoken_estimator_should_return_exact_o200k_count_when_messages_are_text_only() {
        let messages = json!([{"role": "user", "content": "hello world"}]);
        let estimator = TiktokenEstimator::new();

        let (tokens, meta) = estimator.estimate("gpt-4o", &messages).await.unwrap();

        let expected = {
            let bpe = tiktoken_rs::o200k_base_singleton();
            let bpe = bpe.lock();
            bpe.encode_with_special_tokens("hello world").len() as u64
        };
        assert_eq!(
            tokens,
            TokenCount {
                value: expected,
                source: TokenSource::Estimated {
                    via: EstimatorKind::TiktokenO200k
                },
            }
        );
        assert!(!meta.truncated_content);
    }

    #[tokio::test]
    async fn tiktoken_estimator_should_set_truncated_content_flag_when_image_block_present() {
        let messages = json!([{
            "role": "user",
            "content": [
                {"type": "text", "text": "look at this"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
            ]
        }]);
        let estimator = TiktokenEstimator::new();

        let (tokens, meta) = estimator.estimate("gpt-4o", &messages).await.unwrap();

        assert!(meta.truncated_content);
        let expected = {
            let bpe = tiktoken_rs::o200k_base_singleton();
            let bpe = bpe.lock();
            bpe.encode_with_special_tokens("look at this").len() as u64
        };
        assert_eq!(tokens.value, expected);
    }

    #[tokio::test]
    async fn tiktoken_estimator_should_count_tool_result_text_payload_not_json_structure() {
        let messages = json!([{
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "toolu_01",
                    "content": "search returned three matches",
                },
            ]
        }]);
        let estimator = TiktokenEstimator::new();

        let (tokens, meta) = estimator.estimate("gpt-4o", &messages).await.unwrap();

        assert!(!meta.truncated_content);
        let expected = {
            let bpe = tiktoken_rs::o200k_base_singleton();
            let bpe = bpe.lock();
            bpe.encode_with_special_tokens("search returned three matches")
                .len() as u64
        };
        assert_eq!(tokens.value, expected);
    }

    #[tokio::test]
    async fn anthropic_count_tokens_estimator_should_return_estimated_source_when_mock_returns_200()
    {
        let mock = MockServer::start(vec![MockResponse::ok(json!({"input_tokens": 512}))]).await;
        let estimator = test_estimator(mock.base_url());
        let messages = json!([{"role": "user", "content": "hello"}]);

        let (tokens, _meta) = estimator
            .estimate("claude-sonnet-5", &messages)
            .await
            .unwrap();

        assert_eq!(
            tokens,
            TokenCount {
                value: 512,
                source: TokenSource::Estimated {
                    via: EstimatorKind::AnthropicCountTokensApi
                },
            }
        );
    }

    #[tokio::test]
    async fn anthropic_count_tokens_estimator_should_return_rate_limited_error_when_mock_returns_429_without_retry(
    ) {
        let mock =
            MockServer::start(vec![MockResponse::status(StatusCode::TOO_MANY_REQUESTS)]).await;
        let estimator = test_estimator(mock.base_url());
        let messages = json!([{"role": "user", "content": "hello"}]);

        let result = estimator.estimate("claude-sonnet-5", &messages).await;

        assert!(matches!(result, Err(EstimatorError::RateLimited)));
        assert_eq!(mock.observations.request_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn anthropic_count_tokens_estimator_should_return_upstream_failure_error_when_mock_returns_500(
    ) {
        let mock = MockServer::start(vec![MockResponse::status(
            StatusCode::INTERNAL_SERVER_ERROR,
        )])
        .await;
        let estimator = test_estimator(mock.base_url());
        let messages = json!([{"role": "user", "content": "hello"}]);

        let result = estimator.estimate("claude-sonnet-5", &messages).await;

        assert!(matches!(result, Err(EstimatorError::UpstreamFailure(_))));
    }

    #[tokio::test]
    async fn bounded_estimator_should_cap_concurrent_requests_when_burst_of_twenty_calls_issued() {
        const MAX_CONCURRENT: usize = 4;
        let mock = MockServer::start_with_delay(
            vec![MockResponse::ok(json!({"input_tokens": 10}))],
            Duration::from_millis(50),
        )
        .await;
        let estimator = test_estimator(mock.base_url());
        let bounded = Arc::new(BoundedEstimator::new(estimator, MAX_CONCURRENT));

        let mut set = tokio::task::JoinSet::new();
        for _ in 0..20 {
            let bounded = Arc::clone(&bounded);
            set.spawn(async move {
                let messages = json!([{"role": "user", "content": "hello"}]);
                bounded.estimate("claude-sonnet-5", &messages).await
            });
        }
        while let Some(result) = set.join_next().await {
            result.unwrap().unwrap();
        }

        assert_eq!(mock.observations.request_count.load(Ordering::SeqCst), 20);
        assert!(
            mock.observations
                .concurrent_high_water_mark
                .load(Ordering::SeqCst)
                <= MAX_CONCURRENT
        );

        let headers = mock
            .observations
            .last_headers
            .lock()
            .unwrap()
            .clone()
            .expect("mock server should have observed at least one request's headers");
        let api_key = headers
            .get("x-api-key")
            .expect("x-api-key header should be present");
        assert!(!api_key.is_empty());
        assert!(headers.get("anthropic-version").is_some());
    }
}
