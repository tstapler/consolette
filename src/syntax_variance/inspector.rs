//! Syntax variance inspector: flags unknown or experimental HTTP headers,
//! top-level Anthropic JSON payload keys, and response shapes.

use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntaxVariance {
    pub variance_type: String, // "header", "request_body", "response_shape"
    pub key_name: String,      // e.g. "anthropic-beta:thinking", "body:thinking"
    pub sample_json: String,   // Sanitized excerpt
}

/// Known standard Anthropic request headers
const KNOWN_HEADERS: &[&str] = &[
    "content-type",
    "authorization",
    "x-api-key",
    "anthropic-version",
    "user-agent",
    "host",
    "accept",
    "accept-encoding",
    "connection",
    "content-length",
];

/// Known standard Anthropic top-level request body keys
const KNOWN_BODY_KEYS: &[&str] = &[
    "model",
    "messages",
    "system",
    "max_tokens",
    "temperature",
    "top_p",
    "top_k",
    "tools",
    "tool_choice",
    "stream",
    "metadata",
];

/// Inspect HTTP request headers for unexpected flags (e.g. `anthropic-beta`) or non-standard `x-` / `anthropic-` headers.
#[must_use]
pub fn inspect_headers(headers: &HeaderMap) -> Vec<SyntaxVariance> {
    let mut variances = Vec::new();

    for (name, value) in headers {
        let name_str = name.as_str().to_lowercase();
        let val_str = value.to_str().unwrap_or("<binary>");

        if name_str == "anthropic-beta" {
            for beta_flag in val_str.split(',') {
                let flag = beta_flag.trim();
                if !flag.is_empty() {
                    variances.push(SyntaxVariance {
                        variance_type: "header".to_string(),
                        key_name: format!("anthropic-beta:{flag}"),
                        sample_json:
                            serde_json::json!({ "header": "anthropic-beta", "value": flag })
                                .to_string(),
                    });
                }
            }
        } else if !KNOWN_HEADERS.contains(&name_str.as_str())
            && (name_str.starts_with("x-") || name_str.starts_with("anthropic-"))
        {
            variances.push(SyntaxVariance {
                variance_type: "header".to_string(),
                key_name: name_str.clone(),
                sample_json: serde_json::json!({ "header": name_str, "value": val_str })
                    .to_string(),
            });
        }
    }

    variances
}

/// Inspect incoming Anthropic JSON body for unmapped or experimental fields.
#[must_use]
pub fn inspect_request_body(body: &Value) -> Vec<SyntaxVariance> {
    let mut variances = Vec::new();

    let Some(obj) = body.as_object() else {
        return variances;
    };

    for (key, val) in obj {
        if !KNOWN_BODY_KEYS.contains(&key.as_str()) {
            let sample = if val.to_string().len() > 200 {
                format!("{}...", &val.to_string()[..200])
            } else {
                val.to_string()
            };

            variances.push(SyntaxVariance {
                variance_type: "request_body".to_string(),
                key_name: format!("body:{key}"),
                sample_json: sample,
            });
        }
    }

    variances
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn inspect_headers_should_flag_anthropic_beta_flags() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("prompt-caching-2024-07-16, thinking-2025-02-19"),
        );

        let variances = inspect_headers(&headers);
        assert_eq!(variances.len(), 2);
        assert_eq!(
            variances[0].key_name,
            "anthropic-beta:prompt-caching-2024-07-16"
        );
        assert_eq!(variances[1].key_name, "anthropic-beta:thinking-2025-02-19");
    }

    #[test]
    fn inspect_request_body_should_flag_unrecognized_keys() {
        let body = serde_json::json!({
            "model": "claude-sonnet-4-5",
            "messages": [],
            "thinking": {"type": "enabled", "budget_tokens": 1024}
        });

        let variances = inspect_request_body(&body);
        assert_eq!(variances.len(), 1);
        assert_eq!(variances[0].key_name, "body:thinking");
    }
}
