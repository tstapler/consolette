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

    let prompt_tokens = anthropic
        .get("usage")
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion_tokens = anthropic
        .get("usage")
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
