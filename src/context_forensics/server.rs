//! JSON query routes for the context-forensics dashboard (context-analyzer
//! plan.md Story 1.4.1) and the dashboard's own HTML shell (Stories 1.4.2/
//! 1.4.3).
//!
//! Mounted onto the existing `serve-cost` router (Story 1.4.4) rather than
//! served from a second process/port — Pattern Decision: "Success Metric
//! requires 'one consolette-served dashboard... without leaving
//! consolette.'"

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::context_forensics::store::{ContextForensicsStore, Source};

/// Self-contained dashboard HTML/JS/CSS, compiled into the binary (matches
/// `cost_metrics/dashboard.html`'s `include_str!` precedent — no build
/// step, no CDN dependency, ADR-015).
const CONTEXT_DASHBOARD_HTML: &str = include_str!("dashboard.html");

/// One row of `GET /v1/context/sessions` (Story 1.4.1 AC1).
#[derive(Debug, Clone, Serialize)]
struct SessionListEntry {
    id: String,
    source: Source,
    project: Option<String>,
    started_at: Option<String>,
    peak_context_tokens: u64,
    chain_coverage_ratio: Option<f64>,
    parse_failure_count: u64,
}

/// `{"error": "...", ...}` body for a non-2xx response.
fn error_body(error: &str, extra: &serde_json::Value) -> serde_json::Value {
    let mut body = serde_json::json!({ "error": error });
    if let (Some(body_map), Some(extra_map)) = (body.as_object_mut(), extra.as_object()) {
        for (key, value) in extra_map {
            body_map.insert(key.clone(), value.clone());
        }
    }
    body
}

/// `GET /v1/context/sessions`: every stored session, with `peak_context_tokens`
/// resolved via [`ContextForensicsStore::peak_context_tokens`] (Story 1.4.1
/// AC1). Always `200` — an empty corpus is a valid (empty) response, not an
/// error.
async fn handler_list_sessions(
    State(store): State<Arc<ContextForensicsStore>>,
) -> impl IntoResponse {
    let sessions = match store.list_sessions() {
        Ok(sessions) => sessions,
        Err(error) => {
            tracing::error!(%error, "context-forensics: failed to list sessions");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(error_body("store_query_failed", &serde_json::json!({}))),
            )
                .into_response();
        }
    };

    let mut entries = Vec::with_capacity(sessions.len());
    for session in sessions {
        let peak_context_tokens = store.peak_context_tokens(&session.id).unwrap_or(0);
        entries.push(SessionListEntry {
            id: session.id,
            source: session.source,
            project: session.project,
            started_at: session.started_at,
            peak_context_tokens,
            chain_coverage_ratio: session.chain_coverage_ratio,
            parse_failure_count: session.parse_failure_count,
        });
    }

    (StatusCode::OK, Json(entries)).into_response()
}

/// `GET /v1/context/sessions/{id}/composition`: per-turn [`TurnComposition`]
/// for `id`. `404` with a JSON error body when `id` isn't a known session
/// (Story 1.4.1 AC3) — never a `500` for an unknown id.
async fn handler_session_composition(
    State(store): State<Arc<ContextForensicsStore>>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    match store.get_session(&session_id) {
        Ok(None) => {
            return session_not_found(&session_id);
        }
        Ok(Some(_)) => {}
        Err(error) => {
            tracing::error!(%error, session_id, "context-forensics: failed to look up session");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(error_body("store_query_failed", &serde_json::json!({}))),
            )
                .into_response();
        }
    }

    match store.composition_for_session(&session_id) {
        Ok(turns) => (StatusCode::OK, Json(serde_json::json!({ "turns": turns }))).into_response(),
        Err(error) => {
            tracing::error!(%error, session_id, "context-forensics: failed to compute composition");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(error_body("store_query_failed", &serde_json::json!({}))),
            )
                .into_response()
        }
    }
}

/// `GET /v1/context/sessions/{id}/growth`: `{turns, native_compaction_events}`
/// for `id`. `404` with a JSON error body when `id` isn't known (Story
/// 1.4.1 AC3, Story 1.4.3 AC2).
async fn handler_session_growth(
    State(store): State<Arc<ContextForensicsStore>>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    match store.get_session(&session_id) {
        Ok(None) => {
            return session_not_found(&session_id);
        }
        Ok(Some(_)) => {}
        Err(error) => {
            tracing::error!(%error, session_id, "context-forensics: failed to look up session");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(error_body("store_query_failed", &serde_json::json!({}))),
            )
                .into_response();
        }
    }

    let turns = match store.growth_for_session(&session_id) {
        Ok(turns) => turns,
        Err(error) => {
            tracing::error!(%error, session_id, "context-forensics: failed to compute growth series");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(error_body("store_query_failed", &serde_json::json!({}))),
            )
                .into_response();
        }
    };
    let native_compaction_events = store
        .native_compaction_events_for_session(&session_id)
        .unwrap_or_default();

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "turns": turns,
            "native_compaction_events": native_compaction_events,
        })),
    )
        .into_response()
}

fn session_not_found(session_id: &str) -> axum::response::Response {
    (
        StatusCode::NOT_FOUND,
        Json(error_body(
            "session_not_found",
            &serde_json::json!({ "session_id": session_id }),
        )),
    )
        .into_response()
}

/// `GET /dashboard/context`: the context-forensics dashboard HTML shell.
async fn handler_context_dashboard() -> impl IntoResponse {
    Html(CONTEXT_DASHBOARD_HTML)
}

/// Build the `/dashboard/context` + `/v1/context/*` route group,
/// `State`-sharing `store`.
#[must_use = "dropping the router without serving it drops the route registration"]
pub fn context_router(store: Arc<ContextForensicsStore>) -> Router {
    Router::new()
        .route("/dashboard/context", get(handler_context_dashboard))
        .route("/v1/context/sessions", get(handler_list_sessions))
        .route(
            "/v1/context/sessions/{id}/composition",
            get(handler_session_composition),
        )
        .route(
            "/v1/context/sessions/{id}/growth",
            get(handler_session_growth),
        )
        .with_state(store)
}

/// Fallback route group mounted in place of [`context_router`] when
/// [`ContextForensicsStore::open`] failed at `serve_cost` startup (Story
/// 1.4.4's fail-open contract): every context-forensics path returns `503`
/// rather than a `404` (which would misleadingly imply the feature doesn't
/// exist) or, worse, taking the rest of `serve_cost` down with it.
#[must_use = "dropping the router without serving it drops the route registration"]
pub fn context_unavailable_router() -> Router {
    async fn unavailable() -> impl IntoResponse {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(error_body(
                "context_forensics_store_unavailable",
                &serde_json::json!({}),
            )),
        )
    }

    Router::new()
        .route("/dashboard/context", get(unavailable))
        .route("/v1/context/sessions", get(unavailable))
        .route("/v1/context/sessions/{id}/composition", get(unavailable))
        .route("/v1/context/sessions/{id}/growth", get(unavailable))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::context_forensics::ingest_claude_code::ingest_claude_code_session;
    use axum::body::Body;
    use axum::http::Request;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};
    use tower::ServiceExt;

    fn seeded_store() -> (TempDir, Arc<ContextForensicsStore>, String) {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(ContextForensicsStore::open(&dir.path().join("cf.sqlite")).unwrap());

        let mut fixture = NamedTempFile::with_suffix(".jsonl").unwrap();
        writeln!(
            fixture,
            r#"{{"type":"user","uuid":"u1","parentUuid":null,"isSidechain":false,"isMeta":false,"message":{{"role":"user","content":"hi"}}}}"#
        )
        .unwrap();
        writeln!(
            fixture,
            r#"{{"type":"assistant","uuid":"a1","parentUuid":"u1","isSidechain":false,"isMeta":false,"message":{{"role":"assistant","content":[{{"type":"text","text":"reply"}}],"usage":{{"input_tokens":1000,"output_tokens":20,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}}}}"#
        )
        .unwrap();

        ingest_claude_code_session(&store, fixture.path()).unwrap();
        let session_id = fixture
            .path()
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        (dir, store, session_id)
    }

    #[tokio::test]
    async fn get_sessions_route_should_return_all_sessions_when_store_seeded() {
        let (_dir, store, _session_id) = seeded_store();
        let router = context_router(store);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/v1/context/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json.as_array().unwrap().len(), 1);
        assert_eq!(json[0]["peak_context_tokens"], 1000);
    }

    #[tokio::test]
    async fn get_session_composition_route_should_return_per_turn_breakdown_when_session_exists() {
        let (_dir, store, session_id) = seeded_store();
        let router = context_router(store);

        let response = router
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/context/sessions/{session_id}/composition"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let turns = json["turns"].as_array().unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0]["input_tokens"], 1000);
    }

    #[tokio::test]
    async fn get_session_growth_route_should_return_404_when_session_id_unknown() {
        let (_dir, store, _session_id) = seeded_store();
        let router = context_router(store);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/v1/context/sessions/does-not-exist/growth")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "session_not_found");
    }

    #[tokio::test]
    async fn get_session_composition_route_should_return_404_when_session_id_unknown() {
        let (_dir, store, _session_id) = seeded_store();
        let router = context_router(store);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/v1/context/sessions/does-not-exist/composition")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_session_growth_route_should_include_native_compaction_events_when_session_has_compaction_event(
    ) {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(ContextForensicsStore::open(&dir.path().join("cf.sqlite")).unwrap());

        let mut fixture = NamedTempFile::with_suffix(".jsonl").unwrap();
        writeln!(
            fixture,
            r#"{{"type":"user","uuid":"u1","parentUuid":null,"isSidechain":false,"isMeta":false,"message":{{"role":"user","content":"hi"}}}}"#
        )
        .unwrap();
        writeln!(
            fixture,
            r#"{{"type":"assistant","uuid":"a1","parentUuid":"u1","isSidechain":false,"isMeta":false,"message":{{"role":"assistant","content":[{{"type":"text","text":"reply"}}],"usage":{{"input_tokens":1000,"output_tokens":20,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}}}}"#
        )
        .unwrap();
        writeln!(
            fixture,
            r#"{{"type":"system","uuid":"b1","parentUuid":"a1","isSidechain":false,"isMeta":false,"subtype":"compact_boundary","message":null,"compactMetadata":{{"trigger":"auto","preTokens":9000,"postTokens":1200}}}}"#
        )
        .unwrap();

        ingest_claude_code_session(&store, fixture.path()).unwrap();
        let session_id = fixture
            .path()
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let router = context_router(store);
        let response = router
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/context/sessions/{session_id}/growth"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let events = json["native_compaction_events"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["row_uuid"], "b1");
        assert_eq!(events[0]["turn_index"], 0);
    }

    #[tokio::test]
    async fn get_context_dashboard_route_should_return_200() {
        let (_dir, store, _session_id) = seeded_store();
        let router = context_router(store);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/dashboard/context")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}
