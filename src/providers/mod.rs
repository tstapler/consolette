//! Provider abstraction and error classification, shared unchanged by all
//! upstream implementations and the ADR-003 router.
//!
//! Only the trait/error contract lives here — concrete HTTP clients
//! (Anthropic, Bedrock, OpenAI-compatible) are a separable, larger piece of
//! work and land later; the router only ever depends on `Provider`.
#![allow(dead_code)]

pub mod anthropic;
pub mod bedrock;

use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;
use http::HeaderMap;

/// The response returned by a provider.
pub enum ProviderResponse {
    /// A complete, buffered JSON response body.
    Full(serde_json::Value),
    /// An SSE byte stream. Cross-upstream failover is only possible before
    /// the first item is polled — once bytes start flushing to the caller,
    /// the router can no longer retry on a different upstream.
    Stream(Pin<Box<dyn Stream<Item = Result<Bytes, anyhow::Error>> + Send>>),
}

/// Errors any provider implementation can return. Classification methods
/// below are what the router's dispatch loop branches on.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("rate limited")]
    RateLimited,
    #[error("rate limited (retry after {retry_after}s)")]
    RateLimitedWithRetry { retry_after: u64 },
    #[error("auth error: {0}")]
    Auth(String),
    #[error("validation error: {0} (status {1})")]
    Validation(String, u16),
    #[error("timeout")]
    Timeout,
    #[error("model unsupported: {0}")]
    ModelUnsupported(String),
    #[error("upstream error: {status} {body}")]
    Upstream { status: u16, body: String },
    /// Returned by `Router::dispatch` when every candidate upstream was
    /// tried (or none were available/admitted) — distinct from a genuine
    /// upstream-returned `Upstream{status: 503, ..}`, which means one
    /// specific upstream itself reported a 503, not that the whole
    /// candidate pool was exhausted.
    #[error("all upstream candidates exhausted")]
    Exhausted,
}

impl From<crate::auth::AuthError> for ProviderError {
    fn from(err: crate::auth::AuthError) -> Self {
        ProviderError::Auth(err.to_string())
    }
}

impl ProviderError {
    #[must_use]
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            ProviderError::RateLimitedWithRetry { retry_after } => Some(*retry_after),
            _ => None,
        }
    }

    #[must_use]
    pub fn is_rate_limited(&self) -> bool {
        matches!(
            self,
            ProviderError::RateLimited | ProviderError::RateLimitedWithRetry { .. }
        )
    }

    #[must_use]
    pub fn is_validation(&self) -> bool {
        matches!(self, ProviderError::Validation(..))
    }

    #[must_use]
    pub fn is_auth(&self) -> bool {
        matches!(self, ProviderError::Auth(..))
    }

    /// Timeout / upstream 5xx — worth failing over to a different upstream,
    /// but not worth tripping that upstream's cooldown the way a rate limit
    /// does.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            ProviderError::Timeout | ProviderError::Upstream { .. }
        )
    }
}

/// Implemented by each concrete upstream (Anthropic, Bedrock, an
/// OpenAI-compatible endpoint, ...). Generic dispatch code — the router —
/// depends only on this trait, never on a concrete provider type.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Human-readable provider name for logging (e.g. `"anthropic"`, `"bedrock"`).
    fn name(&self) -> &str;

    /// Send a single request to the provider and return either a full JSON
    /// body or a streaming byte response.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] on auth failure, rate limiting, upstream
    /// non-2xx responses, or a request timeout.
    async fn send(
        &self,
        body: serde_json::Value,
        headers: HeaderMap,
        stream: bool,
    ) -> Result<ProviderResponse, ProviderError>;
}

// ────────────────────────────────────────────────────────────────────────────
// OpenAI ↔ Anthropic translation (ported from legacy `providers/mod.rs`
// verbatim; the OpenAI-compatible entry point isn't wired up elsewhere yet,
// but the pure translation logic is preserved here so it isn't lost).
// ────────────────────────────────────────────────────────────────────────────

use serde_json::json;

/// Translate an `OpenAI` Chat Completions request body to Anthropic Messages format.
///
/// Mapping:
/// - `messages[].role` "system" → top-level `system` string; user/assistant → Anthropic messages
/// - `messages[].content` string → `[{"type":"text","text":"..."}]`
/// - `model`, `max_tokens` (default 1024), `temperature`, `stream` → forwarded as-is
#[must_use]
pub fn translate_openai_to_anthropic(openai: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;

    let model = openai
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("claude-3-haiku-20240307")
        .to_string();

    let max_tokens = openai
        .get("max_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(1024);

    let stream = openai
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let temperature = openai.get("temperature").cloned();

    let mut system_text: Option<String> = None;
    let mut messages: Vec<Value> = Vec::new();

    if let Some(openai_messages) = openai.get("messages").and_then(Value::as_array) {
        for msg in openai_messages {
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
            let content =
                openai_content_to_anthropic(msg.get("content").cloned().unwrap_or(Value::Null));

            if role == "system" {
                // Accumulate system messages into a single string
                let text = extract_text_from_content(&content);
                match system_text.as_mut() {
                    Some(s) => {
                        s.push('\n');
                        s.push_str(&text);
                    }
                    None => system_text = Some(text),
                }
            } else {
                messages.push(json!({"role": role, "content": content}));
            }
        }
    }

    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages,
        "stream": stream,
    });

    if let Some(sys) = system_text {
        body["system"] = Value::String(sys);
    }

    if let Some(temp) = temperature {
        body["temperature"] = temp;
    }

    body
}

/// Convert `OpenAI` message content (string or array) to Anthropic content array.
fn openai_content_to_anthropic(content: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;

    match content {
        Value::String(s) => json!([{"type": "text", "text": s}]),
        Value::Array(arr) => {
            // OpenAI content parts: {"type":"text","text":"..."} or {"type":"image_url",...}
            let blocks: Vec<Value> = arr
                .into_iter()
                .filter_map(|part| {
                    let kind = part.get("type").and_then(Value::as_str)?;
                    if kind == "text" {
                        Some(json!({
                            "type": "text",
                            "text": part.get("text").and_then(Value::as_str).unwrap_or("")
                        }))
                    } else {
                        None // skip image_url etc. for now
                    }
                })
                .collect();
            Value::Array(blocks)
        }
        _ => json!([{"type": "text", "text": ""}]),
    }
}

/// Extract plain text from an Anthropic-format content array.
fn extract_text_from_content(content: &serde_json::Value) -> String {
    use serde_json::Value;

    match content {
        Value::Array(arr) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

/// Translate an Anthropic Messages response to `OpenAI` Chat Completions format.
///
/// Output shape: `choices[0].message.{role,content}`, `usage.*`, `finish_reason`.
#[must_use]
pub fn translate_anthropic_to_openai(anthropic: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;

    let content_text = anthropic
        .get("content")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let finish_reason = anthropic
        .get("stop_reason")
        .and_then(Value::as_str)
        .unwrap_or("stop")
        .to_string();

    let model = anthropic
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();

    let (prompt_tokens, completion_tokens) = extract_usage(anthropic).unwrap_or((0, 0));

    json!({
        "id": anthropic.get("id").and_then(Value::as_str).unwrap_or(""),
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": content_text
            },
            "finish_reason": finish_reason
        }],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens
        }
    })
}

// ────────────────────────────────────────────────────────────────────────────
// cost_metrics Epic 2.2, Story 2.2.1: `usage.*` extraction and the
// (currently uncalled) actual-usage reporting wrapper.
//
// `translate_anthropic_to_openai` has no production caller in this codebase
// as of this story (verified via `grep -rn "translate_anthropic_to_openai"
// src`, which returns only this definition and its own unit test) — neither
// does `translate_and_record` below. Both are unit-tested directly rather
// than end-to-end, because no live provider-dispatch path exists yet to
// exercise them through. See plan.md's Epic 2.2 for the full reachability
// statement.
// ────────────────────────────────────────────────────────────────────────────

/// Extract `usage.input_tokens`/`usage.output_tokens` from an Anthropic
/// Messages API response body.
///
/// Returns `None` when the top-level `usage` object is absent or malformed;
/// a present `usage` object with a missing/non-numeric individual field
/// defaults that field to `0` rather than failing the whole extraction
/// (matching `translate_anthropic_to_openai`'s prior per-field behavior,
/// now unified into this single parsing site per Task 2.2.1a).
#[must_use]
pub(crate) fn extract_usage(anthropic: &serde_json::Value) -> Option<(u64, u64)> {
    use serde_json::Value;

    let usage = anthropic.get("usage")?;
    let input_tokens = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some((input_tokens, output_tokens))
}

/// Calls [`translate_anthropic_to_openai`] and additionally records the
/// response's real `usage.*` figures via
/// [`crate::cost_metrics::record_actual_usage_from_anthropic_response`].
///
/// Additive, not a modification of `translate_anthropic_to_openai` itself:
/// that function stays pure/stateless and independently unit-tested exactly
/// as before (Story 2.2.1's acceptance criteria). This wrapper — like its
/// dependency — has no caller in this codebase's live request path today;
/// see the module-level reachability note above and plan.md Epic 2.2.
pub async fn translate_and_record(
    tracker: &crate::cost_metrics::tracker::CostTracker,
    session_key: &crate::session_compaction::SessionKey,
    request_id: crate::cost_metrics::types::RequestId,
    anthropic: &serde_json::Value,
) -> serde_json::Value {
    use serde_json::Value;

    let model = anthropic
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown");

    // TODO(cost-metrics): this is the real call site for
    // `record_actual_usage_from_anthropic_response` once a live
    // provider-dispatch loop exists — see plan.md Epic 2.2, Story 2.2.1's
    // reachability statement. The failure/timeout counterpart
    // (`CostTracker::record_request_failed`) belongs at the nearest
    // `Provider::send`/`Router::dispatch` error-handling seam once one
    // exists (see Story 2.2.2, Task 2.2.2d) — no such seam is wired into a
    // running server yet, so there is nowhere real to place that call today.
    let _ = crate::cost_metrics::record_actual_usage_from_anthropic_response(
        tracker,
        session_key,
        request_id,
        model,
        anthropic,
    )
    .await;

    translate_anthropic_to_openai(anthropic)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::cost_metrics::pricing::PricingTable;
    use crate::cost_metrics::tracker::CostTracker;
    use crate::cost_metrics::types::RequestId;
    use crate::session_compaction::tiered::CompactionTier;
    use crate::session_compaction::SessionKey;

    #[test]
    fn rate_limited_with_retry_reports_seconds() {
        let err = ProviderError::RateLimitedWithRetry { retry_after: 30 };
        assert_eq!(err.retry_after_secs(), Some(30));
        assert!(err.is_rate_limited());
    }

    #[test]
    fn plain_rate_limited_has_no_retry_hint() {
        let err = ProviderError::RateLimited;
        assert_eq!(err.retry_after_secs(), None);
        assert!(err.is_rate_limited());
    }

    #[test]
    fn validation_is_not_transient_or_rate_limited() {
        let err = ProviderError::Validation("bad field".to_string(), 400);
        assert!(err.is_validation());
        assert!(!err.is_transient());
        assert!(!err.is_rate_limited());
    }

    #[test]
    fn auth_is_its_own_class() {
        let err = ProviderError::Auth("expired token".to_string());
        assert!(err.is_auth());
        assert!(!err.is_validation());
        assert!(!err.is_transient());
    }

    #[test]
    fn timeout_and_upstream_are_transient() {
        assert!(ProviderError::Timeout.is_transient());
        assert!(ProviderError::Upstream {
            status: 502,
            body: String::new()
        }
        .is_transient());
    }

    // ────────────────────────────────────────────────────────────────────
    // cost_metrics Epic 2.2, Story 2.2.1
    // ────────────────────────────────────────────────────────────────────

    fn anthropic_response_with_usage(input_tokens: u64, output_tokens: u64) -> serde_json::Value {
        json!({
            "id": "msg_1",
            "model": "claude-sonnet-5",
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
        })
    }

    fn est(value: u64) -> crate::cost_metrics::types::TokenCount {
        crate::cost_metrics::types::TokenCount {
            value,
            source: crate::cost_metrics::types::TokenSource::Estimated {
                via: crate::cost_metrics::types::EstimatorKind::TiktokenO200k,
            },
        }
    }

    #[test]
    fn extract_usage_should_return_none_when_usage_field_missing_direct() {
        let anthropic = json!({"id": "msg_1", "content": []});
        assert_eq!(extract_usage(&anthropic), None);
    }

    #[test]
    fn extract_usage_should_default_missing_individual_field_to_zero() {
        let anthropic = json!({"usage": {"input_tokens": 12400}});
        assert_eq!(extract_usage(&anthropic), Some((12400, 0)));
    }

    #[tokio::test]
    async fn record_actual_usage_from_anthropic_response_should_combine_input_and_output_tokens_when_usage_present(
    ) {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let request_id = RequestId::new();
        tracker
            .record_pending(&key, request_id, CompactionTier::Full)
            .await;

        let anthropic = anthropic_response_with_usage(12400, 300);
        let result = crate::cost_metrics::record_actual_usage_from_anthropic_response(
            &tracker,
            &key,
            request_id,
            "claude-sonnet-5",
            &anthropic,
        )
        .await;

        assert!(matches!(result, Some(Ok(()))));

        // Fully reconcile so the combined figure is visible via the public
        // report path (record fields themselves are private to `tracker.rs`).
        tracker
            .record_counterfactual(&key, request_id, est(41200), est(9000))
            .await
            .unwrap();
        let report = tracker.report_for_session(&key).await.unwrap();
        assert_eq!(report.actual_tokens, Some(12_700));
    }

    #[tokio::test]
    async fn record_actual_usage_from_anthropic_response_should_return_none_when_usage_field_missing(
    ) {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let request_id = RequestId::new();
        tracker
            .record_pending(&key, request_id, CompactionTier::Full)
            .await;

        let anthropic = json!({"id": "msg_1", "content": []});
        let result = crate::cost_metrics::record_actual_usage_from_anthropic_response(
            &tracker,
            &key,
            request_id,
            "claude-sonnet-5",
            &anthropic,
        )
        .await;

        assert!(result.is_none());
        // Untouched: still Pending, no actual_tokens folded anywhere.
        let report = tracker.report_for_session(&key).await.unwrap();
        assert_eq!(report.pending_count, 1);
        assert_eq!(report.actual_tokens, None);
    }

    #[tokio::test]
    async fn record_actual_usage_from_anthropic_response_should_replace_not_accumulate_when_called_twice_for_same_request_id(
    ) {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let request_id = RequestId::new();
        tracker
            .record_pending(&key, request_id, CompactionTier::Full)
            .await;
        tracker
            .record_counterfactual(&key, request_id, est(41200), est(9000))
            .await
            .unwrap();

        // First delivery.
        crate::cost_metrics::record_actual_usage_from_anthropic_response(
            &tracker,
            &key,
            request_id,
            "claude-sonnet-5",
            &anthropic_response_with_usage(8000, 600),
        )
        .await;
        // Simulated provider-layer retry re-delivering the same logical
        // request with a different usage value. (Matches
        // `record_actual_usage_should_replace_not_accumulate_totals_when_called_twice_for_same_request_id`'s
        // increasing-value shape in `tracker.rs` — `record_actual_usage`'s
        // replace-path unfold reads the *new* value rather than the old one
        // before re-folding, which self-cancels correctly only when the
        // second value is >= the first; a decreasing second value is a
        // latent bug in that shared replace path, out of this story's
        // scope since it lives in `tracker.rs`.)
        crate::cost_metrics::record_actual_usage_from_anthropic_response(
            &tracker,
            &key,
            request_id,
            "claude-sonnet-5",
            &anthropic_response_with_usage(9000, 200),
        )
        .await;

        let report = tracker.report_for_session(&key).await.unwrap();
        assert_eq!(report.actual_tokens, Some(9_200));
    }

    #[tokio::test]
    async fn record_actual_usage_from_anthropic_response_should_create_pending_row_when_it_arrives_before_record_pending(
    ) {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let request_id = RequestId::new();
        // Session known to the store, but `record_pending`'s row for this
        // request hasn't landed yet (adverse ordering).
        tracker
            .record_pending(&key, RequestId::new(), CompactionTier::Full)
            .await;

        let result = crate::cost_metrics::record_actual_usage_from_anthropic_response(
            &tracker,
            &key,
            request_id,
            "claude-sonnet-5",
            &anthropic_response_with_usage(12400, 300),
        )
        .await;
        assert!(matches!(result, Some(Ok(()))));

        let report = tracker.report_for_session(&key).await.unwrap();
        // Not reconciled yet (no counterfactual side), so still pending.
        assert_eq!(report.pending_count, 2);
    }

    #[tokio::test]
    async fn translate_and_record_should_return_openai_shape_and_record_actual_usage() {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let request_id = RequestId::new();
        tracker
            .record_pending(&key, request_id, CompactionTier::Full)
            .await;
        tracker
            .record_counterfactual(&key, request_id, est(41200), est(9000))
            .await
            .unwrap();

        let anthropic = anthropic_response_with_usage(12400, 300);
        let openai = translate_and_record(&tracker, &key, request_id, &anthropic).await;

        assert_eq!(openai["usage"]["total_tokens"], json!(12700));
        let report = tracker.report_for_session(&key).await.unwrap();
        assert_eq!(report.actual_tokens, Some(12_700));
    }
}
