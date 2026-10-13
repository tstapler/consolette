//! `GET /v1/dashboard/traces` — trace backfill for a page load or an SSE reconnect.

use axum::extract::{Query, State};
use axum::response::Json;
use serde::Deserialize;

use super::events::{event_json, DashboardEvent, RequestTraceData};
use super::EntrypointState;

const DEFAULT_LIMIT: usize = 50;
/// The ring buffer holds at most 100 entries.
const MAX_LIMIT: usize = 100;

#[derive(Debug, Deserialize)]
pub struct TracesQuery {
    limit: Option<usize>,
}

/// Newest-first array whose elements equal the `data` of the matching `RequestTrace` SSE
/// frames, so the UI merges both sources with one upsert rule (ADR-001).
// Kept `async` for signature symmetry with the other Axum handlers.
#[allow(clippy::unused_async)]
pub async fn get_dashboard_traces(
    State(state): State<EntrypointState>,
    Query(params): Query<TracesQuery>,
) -> Json<Vec<serde_json::Value>> {
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let rows = state
        .metrics
        .get_recent_requests(limit)
        .iter()
        .filter_map(|detail| {
            let event = DashboardEvent::RequestTrace(RequestTraceData::from_detail(detail));
            let mut frame: serde_json::Value =
                serde_json::from_str(&event_json(&event, state.compat_snake_case)).ok()?;
            Some(frame.get_mut("data")?.take())
        })
        .collect();
    Json(rows)
}
