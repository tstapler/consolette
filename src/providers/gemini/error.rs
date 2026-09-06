//! Gemini error classification: `classify_gemini_error`, `GeminiErrorBody`
//! (Story 1.3.3).
//!
//! Mirrors `anthropic::map_error_status`'s status-code branching, but Gemini
//! carries its own `{"error":{code,message,status,details}}` body shape
//! (including a 429 `retryDelay` in `details[]`) that has no Anthropic
//! equivalent.

use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::Value;

use crate::providers::ProviderError;

/// The Cloud Code Assist / Gemini error envelope:
/// `{"error":{"code":...,"message":...,"status":...,"details":[...]}}`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GeminiErrorBody {
    pub error: GeminiErrorDetail,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GeminiErrorDetail {
    #[allow(dead_code)] // carried for completeness/future use; not read today
    pub code: u16,
    pub message: String,
    #[allow(dead_code)] // carried for completeness/future use; not read today
    pub status: String,
    #[serde(default)]
    pub details: Vec<Value>,
}

/// Maps a Gemini HTTP status + parsed error body onto the shared
/// `ProviderError` vocabulary, mirroring `anthropic::map_error_status`'s
/// branching style:
/// - 429 with a `retryDelay` in `details[]` -> `RateLimitedWithRetry` (ceiling
///   of the delay in seconds); 429 without one falls back to 60s, matching
///   the other providers' `retry-after`-header fallback.
/// - 401/403 -> `Auth`.
/// - other 4xx -> `Validation`.
/// - 5xx -> `Upstream`.
#[must_use]
pub(crate) fn classify_gemini_error(status: StatusCode, body: &GeminiErrorBody) -> ProviderError {
    if status == StatusCode::TOO_MANY_REQUESTS {
        let retry_after = extract_retry_delay_secs(&body.error.details).unwrap_or(60);
        return ProviderError::RateLimitedWithRetry { retry_after };
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

/// Extracts a `google.rpc.RetryInfo`-shaped `retryDelay` (e.g.
/// `"3.957525076s"`) from `details[]`, returning the ceiling in whole
/// seconds.
fn extract_retry_delay_secs(details: &[Value]) -> Option<u64> {
    details
        .iter()
        .find_map(|d| d.get("retryDelay").and_then(Value::as_str))
        .and_then(parse_retry_delay_seconds)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn parse_retry_delay_seconds(s: &str) -> Option<u64> {
    let trimmed = s.strip_suffix('s')?;
    let secs: f64 = trimmed.parse().ok()?;
    if !secs.is_finite() || secs.is_sign_negative() {
        return None;
    }
    Some(secs.ceil() as u64)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn error_body(value: Value) -> GeminiErrorBody {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn classify_gemini_error_should_return_rate_limited_with_retry_when_429_carries_retry_delay() {
        let body = error_body(json!({
            "error": {
                "code": 429,
                "message": "Resource exhausted",
                "status": "RESOURCE_EXHAUSTED",
                "details": [{
                    "@type": "type.googleapis.com/google.rpc.RetryInfo",
                    "retryDelay": "3.957525076s",
                }],
            }
        }));

        let err = classify_gemini_error(StatusCode::TOO_MANY_REQUESTS, &body);

        assert!(matches!(
            err,
            ProviderError::RateLimitedWithRetry { retry_after: 4 }
        ));
    }

    #[test]
    fn classify_gemini_error_should_default_retry_after_when_429_has_no_retry_delay() {
        let body = error_body(json!({
            "error": {
                "code": 429,
                "message": "Resource exhausted",
                "status": "RESOURCE_EXHAUSTED",
            }
        }));

        let err = classify_gemini_error(StatusCode::TOO_MANY_REQUESTS, &body);

        assert!(matches!(
            err,
            ProviderError::RateLimitedWithRetry { retry_after: 60 }
        ));
    }

    #[test]
    fn classify_gemini_error_should_return_auth_when_401_unauthenticated() {
        let body = error_body(json!({
            "error": {
                "code": 401,
                "message": "Request had invalid authentication credentials.",
                "status": "UNAUTHENTICATED",
            }
        }));

        let err = classify_gemini_error(StatusCode::UNAUTHORIZED, &body);

        match err {
            ProviderError::Auth(msg) => {
                assert_eq!(msg, "Request had invalid authentication credentials.");
            }
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn classify_gemini_error_should_return_auth_when_403_forbidden() {
        let body = error_body(json!({
            "error": {"code": 403, "message": "forbidden", "status": "PERMISSION_DENIED"}
        }));

        assert!(classify_gemini_error(StatusCode::FORBIDDEN, &body).is_auth());
    }

    #[test]
    fn classify_gemini_error_should_return_validation_when_400_invalid_argument() {
        let body = error_body(json!({
            "error": {
                "code": 400,
                "message": "Invalid value at 'request.contents[0].role'",
                "status": "INVALID_ARGUMENT",
            }
        }));

        let err = classify_gemini_error(StatusCode::BAD_REQUEST, &body);

        match err {
            ProviderError::Validation(msg, status) => {
                assert_eq!(msg, "Invalid value at 'request.contents[0].role'");
                assert_eq!(status, 400);
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    // Rust idioms review, Fix 9 — malformed/negative retryDelay input must
    // fall back to None (the caller then applies its own 60s default), never
    // panic. Covers: not-numeric, negative, missing "s" suffix, NaN, empty.
    #[test]
    fn parse_retry_delay_seconds_should_return_none_for_malformed_or_negative_input() {
        for input in ["soon", "-3s", "3", "NaNs", ""] {
            assert_eq!(
                parse_retry_delay_seconds(input),
                None,
                "expected None for malformed input {input:?}"
            );
        }
    }

    #[test]
    fn classify_gemini_error_should_return_upstream_when_5xx() {
        let body = error_body(json!({
            "error": {"code": 503, "message": "backend unavailable", "status": "UNAVAILABLE"}
        }));

        let err = classify_gemini_error(StatusCode::SERVICE_UNAVAILABLE, &body);

        assert!(matches!(err, ProviderError::Upstream { status: 503, .. }));
    }
}
