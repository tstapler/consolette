//! `GET /metrics`, `GET /errors/summary`, `GET /dashboard` — the legacy
//! monitoring surface (Story 6.2 Task 6.2.5), wired against the new
//! `EntrypointState`/`Router` instead of the old ad hoc proxy state.
//!
//! Known gap: the dashboard's request-body inspector (`GET
//! /requests/{id}?stage=`) isn't wired up — it needs a raw/compressed body
//! cache that doesn't exist yet (that's Story 6.2's `compression`/`memory`
//! module port, not this one). The dashboard already degrades gracefully
//! when that route 404s ("Not found or evicted from ring buffer").

use axum::extract::State;
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
