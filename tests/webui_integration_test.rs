//! Integration tests for embedded WebUI static assets, SSE streaming,
//! and REST API endpoints (Epic 7, Tasks 7.2.1 & 7.2.2).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

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
    assert!(
        config_json.get("fallbackCascade").is_some()
            || config_json.get("fallback_cascade").is_some()
    );

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

#[test]
fn fixture_files_should_be_reachable_via_include_str_when_cargo_test_runs() {
    let fixtures = [
        include_str!("../ui/src/testing/fixtures/sse/request-trace-success.json"),
        include_str!("../ui/src/testing/fixtures/sse/request-trace-failure.json"),
        include_str!("../ui/src/testing/fixtures/sse/metrics-tick.json"),
        include_str!("../ui/src/testing/fixtures/sse/error-logged.json"),
        include_str!("../ui/src/testing/fixtures/sse/config-changed.json"),
    ];
    for raw in fixtures {
        let value: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert!(value["type"].is_string());
    }
}

#[tokio::test]
async fn metrics_recent_requests_should_keep_legacy_keys_and_add_attempts_and_route_reason() {
    use std::time::Duration;
    use support::fake_upstream::{FakeUpstream, Script};
    use support::live_server::LiveServer;
    let upstream = FakeUpstream::start(Script::models(Vec::<String>::new())).await;
    let server = LiveServer::start(&upstream.base_url()).await;
    let mut sse = server.open_sse().await;
    let _client = server
        .post_messages(&serde_json::json!({
            "model": "claude-sonnet-4",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .await;
    let traces = sse
        .request_traces(Duration::from_secs(5), Duration::from_millis(200))
        .await;
    assert_eq!(traces.len(), 1, "{traces:?}");

    let metrics = server.metrics().await;
    let recent = &metrics["recent_requests"][0];
    assert!(recent["request_id"].is_string(), "{recent}");
    assert!(recent["duration_ms"].is_number(), "{recent}");
    assert_eq!(recent["model"], "claude-sonnet-4");
    assert_eq!(
        recent["attempts"].as_array().map(Vec::len),
        Some(1),
        "{recent}"
    );
    assert_eq!(recent["route_reason"], "default", "{recent}");
}

// ---- Epic 3: model catalog endpoints (Story 3.1.4) ----

use support::fake_upstream::{FakeUpstream, Script, Step};

/// Real `EntrypointState` over one anthropic-kind upstream per fake, all on the primary route.
/// `pin` pins a model on the named upstream's route entry.
async fn catalog_state(
    upstreams: &[(&str, &FakeUpstream)],
    pin: Option<(&str, &str)>,
) -> (EntrypointState, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let conf_d = dir.path().join("conf.d");
    std::fs::create_dir_all(&conf_d).unwrap();
    let mut toml = String::from("local_discovery = false\nhealth_check_interval_secs = 0\n");
    for (name, fake) in upstreams {
        toml.push_str(&format!(
            "\n[[upstreams]]\nname = \"{name}\"\nkind = \"anthropic\"\nbase_url = \"{}\"\n\n[upstreams.auth]\ntype = \"apikey\"\n\n[upstreams.auth.key]\nsource = \"inline\"\nvalue = \"sk-test\"\n",
            fake.base_url()
        ));
    }
    toml.push_str("\n[[routes]]\nname = \"main\"\nstrategy = \"fallback\"\n");
    for (name, _) in upstreams {
        toml.push_str(&format!("\n[[routes.upstreams]]\nname = \"{name}\"\n"));
        if let Some((pinned, model)) = pin {
            if pinned == *name {
                toml.push_str(&format!("model = \"{model}\"\n"));
            }
        }
    }
    std::fs::write(conf_d.join("00-catalog.toml"), toml).unwrap();
    let config = consolette::config::load(dir.path()).unwrap();
    let state = EntrypointState::build(&config, dir.path()).await.unwrap();
    (state, dir)
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
) -> (StatusCode, serde_json::Value, std::time::Duration) {
    let started = std::time::Instant::now();
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let elapsed = started.elapsed();
    let status = res.status();
    let body = to_bytes(res.into_body(), 4 * 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&body).unwrap_or_default(),
        elapsed,
    )
}

/// Polls `GET /api/model-catalog` until no entry is refreshing and each has been attempted.
async fn wait_settled(app: &axum::Router) -> serde_json::Value {
    for _ in 0..100 {
        let (_, body, _) = call(app, "GET", "/api/model-catalog").await;
        let settled = body["upstreams"].as_object().is_some_and(|m| {
            m.values()
                .all(|e| e["refreshing"] == false && !e["lastAttemptAt"].is_null())
        });
        if settled {
            return body;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("catalog never settled");
}

async fn wait_for_calls(fake: &FakeUpstream, expected: usize) {
    for _ in 0..100 {
        if fake.received_bodies().len() == expected {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!(
        "fake saw {} calls, wanted {expected}",
        fake.received_bodies().len()
    );
}

#[tokio::test]
async fn get_model_catalog_should_return_typed_envelope_with_states_and_ages() {
    let ok = FakeUpstream::start(Script::models(["claude-a", "claude-b"])).await;
    let denied = FakeUpstream::start(
        Script::models(Vec::<String>::new())
            .with_models_step(Step::StatusBody(403, "denied".to_string())),
    )
    .await;
    let (state, _dir) = catalog_state(
        &[("anthropic", &ok), ("bedrock", &denied)],
        Some(("anthropic", "claude-missing")),
    )
    .await;
    let app = entrypoint_router(state);

    let (_, before, _) = call(&app, "GET", "/api/model-catalog").await;
    assert_eq!(before["upstreams"]["anthropic"]["state"], "loading");
    assert_eq!(
        before["upstreams"]["anthropic"]["refreshedAt"],
        serde_json::Value::Null
    );
    assert_eq!(before["configWarnings"], serde_json::json!([]));

    let (status, ack, _) = call(&app, "POST", "/api/model-catalog/refresh").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ack["refreshing"], true);
    let body = wait_settled(&app).await;

    let anthropic = &body["upstreams"]["anthropic"];
    assert_eq!(anthropic["state"], "ok");
    assert_eq!(anthropic["totalCount"], 2);
    assert_eq!(anthropic["truncated"], false);
    assert_eq!(
        anthropic["models"][0],
        serde_json::json!({"id": "claude-a", "ownedBy": "anthropic"})
    );
    assert!(
        anthropic["refreshedAt"]
            .as_str()
            .is_some_and(|t| t.ends_with('Z')),
        "{anthropic}"
    );
    assert_eq!(anthropic["error"], serde_json::Value::Null);
    let bedrock = &body["upstreams"]["bedrock"];
    assert_eq!(bedrock["state"], "error");
    assert_eq!(bedrock["models"], serde_json::json!([]));
    assert_eq!(bedrock["error"]["kind"], "auth");
    assert_eq!(bedrock["error"]["status"], 403);
    assert_eq!(
        body["configWarnings"],
        serde_json::json!([{
            "upstream": "anthropic",
            "model": "claude-missing",
            "referencedBy": "route main upstream 0"
        }])
    );
}

#[tokio::test]
#[allow(non_snake_case)]
async fn get_and_ifStale_post_should_return_within_100ms_when_upstream_hangs() {
    let hanging = FakeUpstream::start(Script::hang().with_models_step(Step::Hang)).await;
    let (state, _dir) = catalog_state(&[("slow", &hanging)], None).await;
    let app = entrypoint_router(state);

    let (status, get_body, get_elapsed) = call(&app, "GET", "/api/model-catalog").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(get_body["upstreams"]["slow"]["state"], "loading");
    assert!(get_elapsed.as_millis() < 100, "{get_elapsed:?}");
    assert_eq!(
        hanging.received_bodies().len(),
        0,
        "GET must not call upstream"
    );

    let (status, ack, post_elapsed) =
        call(&app, "POST", "/api/model-catalog/refresh?ifStale=true").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ack["refreshing"], true);
    assert!(post_elapsed.as_millis() < 100, "{post_elapsed:?}");
    wait_for_calls(&hanging, 1).await;
    let (_, after, _) = call(&app, "GET", "/api/model-catalog").await;
    assert_eq!(after["upstreams"]["slow"]["refreshing"], true);
}

#[tokio::test]
async fn post_refresh_upstream_should_call_only_that_fake_404_unknown_and_coalesce_when_in_flight()
{
    let anthropic = FakeUpstream::start(Script::models(["claude-a"])).await;
    let bedrock = FakeUpstream::start(Script::hang().with_models_step(Step::Hang)).await;
    let (state, _dir) =
        catalog_state(&[("anthropic", &anthropic), ("bedrock", &bedrock)], None).await;
    let app = entrypoint_router(state);

    let (status, first, elapsed) =
        call(&app, "POST", "/api/model-catalog/refresh?upstream=bedrock").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["refreshing"], true);
    assert_eq!(first["coalesced"], false);
    assert_eq!(first["retryAfterSecs"], serde_json::Value::Null);
    assert!(first["upstreams"]["bedrock"].is_object());
    assert!(elapsed.as_millis() < 100, "{elapsed:?}");
    wait_for_calls(&bedrock, 1).await;

    let (_, second, _) = call(&app, "POST", "/api/model-catalog/refresh?upstream=bedrock").await;
    assert_eq!(second["coalesced"], true);
    assert_eq!(second["upstreams"]["bedrock"]["refreshing"], true);

    let (status, unknown, _) = call(&app, "POST", "/api/model-catalog/refresh?upstream=nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(unknown, serde_json::json!({"error": "unknown upstream"}));

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        bedrock.received_bodies().len(),
        1,
        "coalesced call must not hit upstream"
    );
    assert_eq!(
        anthropic.received_bodies().len(),
        0,
        "other upstream must not be called"
    );
}

#[tokio::test]
async fn legacy_get_models_should_remain_unchanged() {
    let fake = FakeUpstream::start(Script::models(["claude-a"])).await;
    let (state, _dir) = catalog_state(&[("anthropic", &fake)], None).await;
    let app = entrypoint_router(state);

    let (status, body, _) = call(&app, "GET", "/api/models").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        serde_json::json!({"anthropic": [{"id": "claude-a", "owned_by": "anthropic"}]})
    );
}

#[tokio::test]
#[allow(non_snake_case)]
async fn catalog_responses_and_sse_frames_should_not_contain_SENTINEL() {
    let leaky = FakeUpstream::start(Script::models(Vec::<String>::new()).with_models_step(
        Step::StatusBody(
            401,
            "bad key sk-ant-SENTINEL123 at https://x/v1/models?key=AIzaSENTINEL".to_string(),
        ),
    ))
    .await;
    let live = support::live_server::LiveServer::start(&leaky.base_url()).await;
    let mut sse = live.open_sse().await;
    let app = entrypoint_router(live.state.clone());

    let (_, post_body, _) = call(&app, "POST", "/api/model-catalog/refresh").await;
    let settled = wait_settled(&app).await;
    let frames = sse
        .collect_until(std::time::Duration::from_secs(5), "ModelCatalogChanged")
        .await;

    assert!(frames.contains("ModelCatalogChanged"), "{frames}");
    assert_eq!(settled["upstreams"]["fake"]["error"]["kind"], "auth");
    for text in [post_body.to_string(), settled.to_string(), frames] {
        assert!(!text.contains("SENTINEL"), "{text}");
    }
}

#[tokio::test]
async fn metrics_should_report_model_catalog_section_with_coalesced_counter_and_upstream_state() {
    let fake = FakeUpstream::start(Script::models(["claude-a"])).await;
    let (state, _dir) = catalog_state(&[("anthropic", &fake)], None).await;
    let app = entrypoint_router(state);

    let (status, body, _) = call(&app, "GET", "/metrics").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["model_catalog"]["catalog_refresh_coalesced_total"], 0);
    assert_eq!(
        body["model_catalog"]["upstreams"]["anthropic"]["state"],
        "loading"
    );
}
