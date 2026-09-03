//! `GET /metrics`, `GET /errors/summary`, `GET /dashboard`, `GET
//! /requests/{id}` — the legacy monitoring surface (Story 6.2 Task 6.2.5),
//! wired against the new `EntrypointState`/`Router` instead of the old ad
//! hoc proxy state.
//!
//! Known gap: `GET /requests/{id}?stage=compressed` always 404s — that
//! stage needs the `compression` module wired into dispatch, which isn't in
//! scope here. The dashboard already degrades gracefully when it 404s
//! ("no compressed snapshot — compression may have been skipped").

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};

use super::EntrypointState;

/// `GET /metrics` — full counters/histogram/error snapshot.
// Kept `async` for signature symmetry with the other Axum handlers.
#[allow(clippy::unused_async)]
pub async fn get_metrics(State(state): State<EntrypointState>) -> impl IntoResponse {
    Json(state.metrics.to_metrics_json())
}

/// `GET /errors/summary` — deduplicated error types, most recently seen first.
#[allow(clippy::unused_async)]
pub async fn get_errors_summary(State(state): State<EntrypointState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "errors": state.metrics.error_tracker.get_summary(20),
    }))
}

/// `GET /requests/{id}?stage=original|compressed` — the dashboard's
/// request-body inspector. `original` serves the cached pre-dispatch body;
/// `compressed` always 404s (see module docs) until the `compression`
/// module is wired into dispatch. Any other `stage` value, or the request
/// having already fallen off the 100-entry ring buffer, also 404s.
///
/// # Errors
///
/// Returns [`StatusCode::NOT_FOUND`] for `stage=compressed`, an unknown
/// request id, or one evicted from the ring buffer.
#[allow(clippy::unused_async, clippy::implicit_hasher)]
pub async fn get_request_body(
    State(state): State<EntrypointState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    match params.get("stage").map(String::as_str) {
        Some("compressed") => Err(StatusCode::NOT_FOUND),
        _ => state
            .metrics
            .get_original_body(&id)
            .map(Json)
            .ok_or(StatusCode::NOT_FOUND),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::Config;

    #[allow(clippy::unwrap_used)]
    async fn state_with_cached_body(request_id: &str, body: serde_json::Value) -> EntrypointState {
        let state = EntrypointState::build(
            &Config::default(),
            std::path::Path::new("/tmp/consolette-test"),
        )
        .await
        .unwrap();
        state
            .metrics
            .push_original_body(request_id.to_string(), body);
        state
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn original_stage_returns_the_cached_body() {
        let body = serde_json::json!({"model": "claude-sonnet-4-5", "messages": []});
        let state = state_with_cached_body("req-1", body.clone()).await;

        let result = get_request_body(
            State(state),
            Path("req-1".to_string()),
            Query(HashMap::from([(
                "stage".to_string(),
                "original".to_string(),
            )])),
        )
        .await;

        let Json(returned) = result.unwrap();
        assert_eq!(returned, body);
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn unknown_request_id_404s() {
        let state = state_with_cached_body("req-1", serde_json::json!({})).await;

        let result = get_request_body(
            State(state),
            Path("does-not-exist".to_string()),
            Query(HashMap::new()),
        )
        .await;

        assert_eq!(result.unwrap_err(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn compressed_stage_always_404s() {
        let state = state_with_cached_body("req-1", serde_json::json!({})).await;

        let result = get_request_body(
            State(state),
            Path("req-1".to_string()),
            Query(HashMap::from([(
                "stage".to_string(),
                "compressed".to_string(),
            )])),
        )
        .await;

        assert_eq!(
            result.unwrap_err(),
            StatusCode::NOT_FOUND,
            "compression isn't wired into dispatch yet"
        );
    }
}
