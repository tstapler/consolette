//! Anthropic-native error envelope mapping (plan.md Epic 2.1, Story 2.1.3).

use axum::http::StatusCode;
use serde_json::{json, Value};

use crate::providers::ProviderError;

/// Maps a `ProviderError` to an Anthropic-shaped `(status, body)` pair,
/// plus an optional `Retry-After` header value for rate-limit responses.
///
/// # Panics
///
/// Does not panic in practice: `StatusCode::from_u16(529)` always succeeds
/// because 529 is a valid three-digit HTTP status code.
#[must_use]
pub fn map_provider_error_anthropic(err: &ProviderError) -> (StatusCode, Option<u64>, Value) {
    match err {
        ProviderError::Validation(msg, _status) => (
            StatusCode::BAD_REQUEST,
            None,
            json!({"type":"error","error":{"type":"invalid_request_error","message":msg}}),
        ),
        ProviderError::Auth(msg) => (
            StatusCode::UNAUTHORIZED,
            None,
            json!({"type":"error","error":{"type":"authentication_error","message":msg}}),
        ),
        ProviderError::RateLimited => (
            StatusCode::TOO_MANY_REQUESTS,
            None,
            json!({"type":"error","error":{"type":"rate_limit_error","message":"rate limited"}}),
        ),
        ProviderError::RateLimitedWithRetry { retry_after } => (
            StatusCode::TOO_MANY_REQUESTS,
            Some(*retry_after),
            json!({"type":"error","error":{"type":"rate_limit_error","message":"rate limited"}}),
        ),
        ProviderError::Timeout => (
            {
                #[allow(clippy::unwrap_used)]
                StatusCode::from_u16(529).unwrap()
            },
            None,
            json!({"type":"error","error":{"type":"overloaded_error","message":"request timed out"}}),
        ),
        ProviderError::Exhausted => (
            {
                #[allow(clippy::unwrap_used)]
                StatusCode::from_u16(529).unwrap()
            },
            None,
            json!({"type":"error","error":{"type":"overloaded_error","message":"all upstream candidates exhausted (check monitoring dashboard at http://127.0.0.1:47000/dashboard for rate limits and cooldown status)"}}),
        ),
        ProviderError::ModelUnsupported(model) => (
            StatusCode::NOT_FOUND,
            None,
            json!({"type":"error","error":{"type":"not_found_error","message":format!("model not found: {model}")}}),
        ),
        ProviderError::Upstream { status, body } => (
            StatusCode::from_u16(*status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            None,
            json!({"type":"error","error":{"type":"api_error","message":body}}),
        ),
        ProviderError::ResponseShapeMismatch(msg) => (
            StatusCode::BAD_GATEWAY,
            None,
            json!({"type":"error","error":{"type":"api_error","message":msg}}),
        ),
    }
}

/// Maps a `ProviderError` to an OpenAI-shaped `(status, body)` pair, plus an
/// optional `Retry-After` header value for rate-limit responses (plan.md
/// Task 3.1.2.1).
#[must_use]
pub fn map_provider_error_openai(err: &ProviderError) -> (StatusCode, Option<u64>, Value) {
    fn envelope(message: &str, error_type: &str) -> Value {
        json!({"error":{"message":message,"type":error_type,"param":null,"code":null}})
    }

    match err {
        ProviderError::Validation(msg, _status) => (
            StatusCode::BAD_REQUEST,
            None,
            envelope(msg, "invalid_request_error"),
        ),
        ProviderError::Auth(msg) => (
            StatusCode::UNAUTHORIZED,
            None,
            envelope(msg, "invalid_request_error"),
        ),
        ProviderError::RateLimited => (
            StatusCode::TOO_MANY_REQUESTS,
            None,
            envelope("rate limited", "rate_limit_error"),
        ),
        ProviderError::RateLimitedWithRetry { retry_after } => (
            StatusCode::TOO_MANY_REQUESTS,
            Some(*retry_after),
            envelope("rate limited", "rate_limit_error"),
        ),
        ProviderError::Timeout => (
            StatusCode::SERVICE_UNAVAILABLE,
            None,
            envelope("request timed out", "server_error"),
        ),
        ProviderError::Exhausted => (
            StatusCode::SERVICE_UNAVAILABLE,
            None,
            envelope("all upstream candidates exhausted (check monitoring dashboard at http://127.0.0.1:47000/dashboard for rate limits and cooldown status)", "server_error"),
        ),
        ProviderError::ModelUnsupported(model) => (
            StatusCode::NOT_FOUND,
            None,
            envelope(
                &format!("model not found: {model}"),
                "invalid_request_error",
            ),
        ),
        ProviderError::Upstream { status, body } => (
            StatusCode::from_u16(*status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            None,
            envelope(body, "server_error"),
        ),
        ProviderError::ResponseShapeMismatch(msg) => {
            (StatusCode::BAD_GATEWAY, None, envelope(msg, "server_error"))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn validation_maps_to_400_invalid_request_error() {
        let (status, retry_after, body) = map_provider_error_anthropic(&ProviderError::Validation(
            "messages: field required".to_string(),
            400,
        ));
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(retry_after, None);
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["message"], "messages: field required");
    }

    #[test]
    fn auth_maps_to_401_authentication_error() {
        let (status, _, body) =
            map_provider_error_anthropic(&ProviderError::Auth("invalid API key".to_string()));
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["type"], "authentication_error");
    }

    #[test]
    fn rate_limited_maps_to_429_no_retry_after() {
        let (status, retry_after, body) = map_provider_error_anthropic(&ProviderError::RateLimited);
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(retry_after, None);
        assert_eq!(body["error"]["type"], "rate_limit_error");
    }

    #[test]
    fn rate_limited_with_retry_carries_retry_after() {
        let (status, retry_after, body) =
            map_provider_error_anthropic(&ProviderError::RateLimitedWithRetry { retry_after: 30 });
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(retry_after, Some(30));
        assert_eq!(body["error"]["type"], "rate_limit_error");
    }

    #[test]
    fn timeout_maps_to_529_overloaded_error() {
        let (status, _, body) = map_provider_error_anthropic(&ProviderError::Timeout);
        assert_eq!(status.as_u16(), 529);
        assert_eq!(body["error"]["type"], "overloaded_error");
    }

    #[test]
    fn exhausted_maps_to_529_overloaded_error() {
        let (status, _, body) = map_provider_error_anthropic(&ProviderError::Exhausted);
        assert_eq!(status.as_u16(), 529);
        assert_eq!(body["error"]["type"], "overloaded_error");
    }

    #[test]
    fn model_unsupported_maps_to_404_not_found_error() {
        let (status, _, body) =
            map_provider_error_anthropic(&ProviderError::ModelUnsupported("gpt-9".to_string()));
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["type"], "not_found_error");
        assert_eq!(body["error"]["message"], "model not found: gpt-9");
    }

    #[test]
    fn upstream_404_passes_through_status_as_api_error() {
        let (status, _, body) = map_provider_error_anthropic(&ProviderError::Upstream {
            status: 404,
            body: "model not found".to_string(),
        });
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["type"], "api_error");
    }

    #[test]
    fn upstream_503_stays_503_distinct_from_exhausted_529() {
        let (status, _, body) = map_provider_error_anthropic(&ProviderError::Upstream {
            status: 503,
            body: "upstream returned 503".to_string(),
        });
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["type"], "api_error");
    }

    // REQ-7 (Story 1.4.1, ADR-002) — focus area.
    #[test]
    fn map_provider_error_anthropic_should_return_502_bad_gateway_for_response_shape_mismatch() {
        let (status, retry_after, body) = map_provider_error_anthropic(
            &ProviderError::ResponseShapeMismatch("missing field `candidates`".to_string()),
        );
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(retry_after, None);
        assert_eq!(
            body,
            json!({"type":"error","error":{"type":"api_error","message":"missing field `candidates`"}})
        );
    }

    // ────────────────────────────────────────────────────────────────────
    // map_provider_error_openai (plan.md Task 3.1.2.1/3.1.2.3)
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn openai_validation_maps_to_400_invalid_request_error() {
        let (status, retry_after, body) = map_provider_error_openai(&ProviderError::Validation(
            "messages: field required".to_string(),
            400,
        ));
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(retry_after, None);
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["message"], "messages: field required");
        assert_eq!(body["error"]["param"], Value::Null);
        assert_eq!(body["error"]["code"], Value::Null);
    }

    #[test]
    fn openai_rate_limited_maps_to_429_rate_limit_error() {
        let (status, retry_after, body) = map_provider_error_openai(&ProviderError::RateLimited);
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(retry_after, None);
        assert_eq!(body["error"]["type"], "rate_limit_error");
    }

    #[test]
    fn openai_rate_limited_with_retry_carries_retry_after() {
        let (status, retry_after, body) =
            map_provider_error_openai(&ProviderError::RateLimitedWithRetry { retry_after: 30 });
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(retry_after, Some(30));
        assert_eq!(body["error"]["type"], "rate_limit_error");
    }

    #[test]
    fn openai_timeout_maps_to_503_server_error() {
        let (status, _, body) = map_provider_error_openai(&ProviderError::Timeout);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["type"], "server_error");
    }

    #[test]
    fn openai_exhausted_maps_to_503_server_error() {
        let (status, _, body) = map_provider_error_openai(&ProviderError::Exhausted);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["type"], "server_error");
    }

    #[test]
    fn openai_upstream_status_passes_through_as_server_error() {
        let (status, _, body) = map_provider_error_openai(&ProviderError::Upstream {
            status: 404,
            body: "model not found".to_string(),
        });
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["type"], "server_error");
    }

    #[test]
    fn openai_upstream_invalid_status_falls_back_to_500() {
        let (status, _, _) = map_provider_error_openai(&ProviderError::Upstream {
            status: 0,
            body: "weird".to_string(),
        });
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    // REQ-7 (Story 1.4.1, ADR-002) — focus area.
    #[test]
    fn map_provider_error_openai_should_return_502_server_error_for_response_shape_mismatch() {
        let (status, retry_after, body) = map_provider_error_openai(
            &ProviderError::ResponseShapeMismatch("missing field `candidates`".to_string()),
        );
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(retry_after, None);
        assert_eq!(body["error"]["type"], "server_error");
        assert_eq!(body["error"]["message"], "missing field `candidates`");
    }
}
