//! Integration tests for embedded WebUI static assets, SSE streaming,
//! and REST API endpoints (Epic 7, Tasks 7.2.1 & 7.2.2).

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt;

use consolette::config::schema::{Config, WebUiMode};
use consolette::entrypoint::{entrypoint_router, EntrypointState};

fn sample_config() -> Config {
    Config::default()
}

async fn create_test_state(config_dir: &std::path::Path) -> EntrypointState {
    let mut config = sample_config();
    config.web_ui = WebUiMode::Angular;
    EntrypointState::build(&config, config_dir)
        .await
        .expect("Failed to build EntrypointState for test")
}

async fn create_legacy_test_state(config_dir: &std::path::Path) -> EntrypointState {
    let mut config = sample_config();
    config.web_ui = WebUiMode::Legacy;
    EntrypointState::build(&config, config_dir)
        .await
        .expect("Failed to build EntrypointState for test")
}

#[tokio::test]
async fn test_embedded_asset_and_spa_fallback() {
    let dir = tempfile::tempdir().expect("Failed to create tempdir");
    let state = create_test_state(dir.path()).await;
    let app = entrypoint_router(state);

    // 1. Root /dashboard request should return index.html
    let req = Request::builder()
        .uri("/dashboard")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(content_type.contains("text/html"));

    // 2. SPA client-side route without extension (e.g. /dashboard/sessions) should fall back to index.html with 200 OK
    let req = Request::builder()
        .uri("/dashboard/sessions")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(content_type.contains("text/html"));

    // 3. Requesting a non-existent static file with an extension should return 404 Not Found
    let req = Request::builder()
        .uri("/dashboard/missing_file.xyz")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_legacy_web_ui_fallback() {
    let dir = tempfile::tempdir().expect("Failed to create tempdir");
    let state = create_legacy_test_state(dir.path()).await;
    let app = entrypoint_router(state);

    let req = Request::builder()
        .uri("/dashboard")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(content_type.contains("text/html"));
}

#[tokio::test]
async fn test_sse_telemetry_events_endpoint() {
    let dir = tempfile::tempdir().expect("Failed to create tempdir");
    let state = create_test_state(dir.path()).await;
    let app = entrypoint_router(state);

    let req = Request::builder()
        .uri("/v1/dashboard/events")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(content_type.contains("text/event-stream"));
}

#[tokio::test]
async fn test_dashboard_sessions_api() {
    let dir = tempfile::tempdir().expect("Failed to create tempdir");
    let state = create_test_state(dir.path()).await;
    let app = entrypoint_router(state);

    let req = Request::builder()
        .uri("/v1/dashboard/sessions?limit=10")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let body = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json.is_array());
}

#[tokio::test]
async fn test_dashboard_benchmark_api() {
    let dir = tempfile::tempdir().expect("Failed to create tempdir");
    let state = create_test_state(dir.path()).await;
    let app = entrypoint_router(state);

    let req = Request::builder()
        .uri("/v1/dashboard/benchmark")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let body = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json.is_array());
}

#[tokio::test]
async fn test_dashboard_config_api_get_and_put() {
    let dir = tempfile::tempdir().expect("Failed to create tempdir");
    let state = create_test_state(dir.path()).await;
    let app = entrypoint_router(state);

    // GET /v1/dashboard/config
    let req = Request::builder()
        .uri("/v1/dashboard/config")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let body = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    let config_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(config_json.get("providers").is_some());
    assert!(config_json.get("fallbackCascade").is_some() || config_json.get("fallback_cascade").is_some());

    // PUT /v1/dashboard/config
    let put_body = serde_json::to_vec(&config_json).unwrap();
    let req = Request::builder()
        .method("PUT")
        .uri("/v1/dashboard/config")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(put_body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}
