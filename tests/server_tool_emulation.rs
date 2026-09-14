//! Hermetic end-to-end coverage for server-tool emulation (Story 3.1.4).
//!
//! No network, no daemon, no real `stapler-mcp` binary: scripted dispatch
//! fakes stand in for upstreams and the [`fake`](consolette::server_tools::executor::fake)
//! in-process MCP server stands in for stapler-mcp over the same
//! `serve_client` path production uses.
//!
//! The live-browser end-to-end test at the bottom is explicitly deferred
//! until stapler-mcp ships `browser_web_search`
//! (<https://github.com/tstapler/stapler-mcp/issues/46>): it is `#[ignore]`d,
//! not deleted.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use futures_core::Stream;
use serde_json::{json, Value};

use consolette::server_tools::executor::fake::{fake_pool, sample_results, FakeBehavior};
use consolette::server_tools::{
    executor::{McpSearchPool, PoolConfig},
    r#loop::run_loop,
    synthesize_sse, Dispatch, LoopLimits,
};

fn test_limits() -> LoopLimits {
    LoopLimits {
        max_iterations: 5,
        max_results: 5,
        total_timeout: std::time::Duration::from_secs(30),
    }
}

fn search_request(model: &str) -> Value {
    json!({
        "model": model,
        "max_tokens": 512,
        "messages": [{"role": "user", "content": "What is new in Rust?"}],
        "tools": [{"type": "web_search_20250305", "name": "web_search"}],
        "stream": false
    })
}

fn search_call_response() -> Value {
    json!({
        "id": "msg_up1", "type": "message", "role": "assistant", "model": "m",
        "content": [
            {"type": "text", "text": "I'll search for that."},
            {"type": "tool_use", "id": "toolu_1", "name": "web_search",
             "input": {"query": "Rust news"}}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 100, "output_tokens": 20}
    })
}

fn grounded_answer_response() -> Value {
    json!({
        "id": "msg_up2", "type": "message", "role": "assistant", "model": "m",
        "content": [{"type": "text", "text": "Rust 1.90 shipped async closures."}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 200, "output_tokens": 30}
    })
}

/// Scripted upstream: first dispatch asks for a search, every later dispatch
/// answers. Records dispatch counts and (optionally) request bodies.
struct ScriptedUpstream {
    calls: Arc<AtomicUsize>,
    seen: Option<Arc<std::sync::Mutex<Vec<Value>>>>,
}

impl ScriptedUpstream {
    fn new() -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                calls: Arc::clone(&calls),
                seen: None,
            },
            calls,
        )
    }
}

impl Dispatch for ScriptedUpstream {
    fn dispatch<'a>(
        &'a self,
        body: Value,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<Value, consolette::providers::ProviderError>>
                + Send
                + 'a,
        >,
    > {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(seen) = &self.seen {
            seen.lock().expect("seen lock").push(body.clone());
        }
        let redispatch = body["messages"].as_array().is_some_and(|m| m.len() > 1);
        Box::pin(async move {
            if redispatch {
                Ok(grounded_answer_response())
            } else {
                Ok(search_call_response())
            }
        })
    }
}

fn searching_upstream() -> (ScriptedUpstream, Arc<AtomicUsize>) {
    ScriptedUpstream::new()
}

#[tokio::test]
async fn loop_should_return_server_tool_blocks_when_upstream_calls_search() {
    let (pool, _) = fake_pool(FakeBehavior::BraveOk(sample_results()), 5_000, 5_000, 2);
    let (dispatch, calls) = searching_upstream();

    let outcome = run_loop(
        &search_request("test-model"),
        &dispatch,
        &pool,
        &test_limits(),
    )
    .await
    .expect("loop must succeed");

    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(outcome.searches_executed, 1);
    assert!(!outcome.degraded);
    let content = outcome.message["content"]
        .as_array()
        .expect("content array");
    assert!(content
        .iter()
        .any(|b| b["type"] == json!("server_tool_use")));
    let result = content
        .iter()
        .find(|b| b["type"] == json!("web_search_tool_result"))
        .expect("web_search_tool_result block");
    assert!(result
        .to_string()
        .contains("https://example.com/rust-async"));
    assert!(content.iter().any(|b| b["type"] == json!("text")));
    assert_eq!(outcome.message["stop_reason"], json!("end_turn"));
    // Usage aggregates both iterations + the search count.
    assert_eq!(outcome.message["usage"]["input_tokens"], json!(300));
    assert_eq!(outcome.message["usage"]["output_tokens"], json!(50));
    assert_eq!(
        outcome.message["usage"]["server_tool_use"]["web_search_requests"],
        json!(1)
    );
}

#[tokio::test]
async fn stream_should_emit_wellformed_sse_when_server_tool_present() {
    let (pool, _) = fake_pool(FakeBehavior::BraveOk(sample_results()), 5_000, 5_000, 2);
    let (dispatch, _) = searching_upstream();
    let outcome = run_loop(
        &search_request("test-model"),
        &dispatch,
        &pool,
        &test_limits(),
    )
    .await
    .expect("loop must succeed");

    let frames = synthesize_sse(&outcome.message);

    assert!(frames.len() >= 4);
    assert!(frames[0].starts_with("event: message_start\n"));
    assert!(frames[frames.len() - 1].starts_with("event: message_stop\n"));
    // Every frame is a complete `event:`/`data:` pair with valid JSON data.
    for frame in &frames {
        let (_, data) = frame.split_once("data: ").expect("data line");
        let parsed: Value = serde_json::from_str(data.trim()).expect("frame data is JSON");
        assert!(parsed.get("type").is_some(), "frame has a type: {frame}");
    }
    // The usage-carrying message_delta matches the loop total exactly
    // (drives single-cost-counting downstream).
    let delta: Value = serde_json::from_str(
        frames[frames.len() - 2]
            .split_once("data: ")
            .expect("delta data")
            .1
            .trim(),
    )
    .expect("delta JSON");
    assert_eq!(delta["usage"], outcome.message["usage"]);
}

#[tokio::test]
async fn e2e_should_succeed_with_results_when_cohere_model_requests_search() {
    // A Cohere-backed model id: the rewritten outbound body must carry a
    // described, non-empty function (the old 400 class) and no server defs.
    let (pool, _) = fake_pool(FakeBehavior::BraveOk(sample_results()), 5_000, 5_000, 2);
    let seen: Arc<std::sync::Mutex<Vec<Value>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (dispatch, _) = searching_upstream();
    let recording = ScriptedUpstream {
        calls: Arc::new(AtomicUsize::new(0)),
        seen: Some(Arc::clone(&seen)),
    };

    let outcome = run_loop(
        &search_request("cohere/north-mini-code:free"),
        &recording,
        &pool,
        &test_limits(),
    )
    .await
    .expect("cohere-model search must succeed");
    let _ = dispatch;

    assert!(!outcome.degraded);
    assert!(outcome.message.to_string().contains("async closures"));
    let bodies = seen.lock().expect("seen lock");
    assert!(!bodies.is_empty());
    let first = &bodies[0];
    assert!(
        !first.to_string().contains("web_search_20250305"),
        "server defs must not leak upstream: {first}"
    );
    let def = &first["tools"][0];
    assert!(!def["description"].as_str().unwrap_or("").is_empty());
    assert!(def["input_schema"]
        .as_object()
        .is_some_and(|s| !s.is_empty()));
}

/// Fixed canned upstream for the no-regression entrypoint test.
struct FixedProvider {
    name: String,
    response: std::sync::Mutex<
        Option<
            Result<consolette::providers::ProviderResponse, consolette::providers::ProviderError>,
        >,
    >,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl consolette::providers::Provider for FixedProvider {
    fn name(&self) -> &str {
        &self.name
    }
    async fn send(
        &self,
        _body: Value,
        _: axum::http::HeaderMap,
        _: bool,
    ) -> Result<consolette::providers::ProviderResponse, consolette::providers::ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.response
            .lock()
            .expect("response lock")
            .take()
            .expect("called more than once")
    }
    async fn list_models(
        &self,
    ) -> Result<Vec<consolette::providers::ModelInfo>, consolette::providers::ProviderError> {
        Ok(Vec::new())
    }
}

async fn state_with_response(
    response: Result<consolette::providers::ProviderResponse, consolette::providers::ProviderError>,
) -> (consolette::entrypoint::EntrypointState, Arc<AtomicUsize>) {
    use consolette::providers::Provider;

    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(FixedProvider {
        name: "test".to_string(),
        response: std::sync::Mutex::new(Some(response)),
        calls: Arc::clone(&calls),
    });
    let candidates = vec![consolette::routing::strategy::UpstreamRef {
        index: 0,
        name: "test".to_string(),
        weight: 1.0,
        model: None,
    }];
    let router = consolette::routing::router::Router::new(
        candidates,
        vec![provider],
        Arc::new(consolette::routing::strategy::FallbackStrategy)
            as Arc<dyn consolette::routing::strategy::RoutingStrategy>,
        Arc::new(consolette::routing::health::HealthRegistry::new(300)),
        Arc::new(consolette::ratelimit::RateLimiters::new(
            &consolette::config::schema::RateLimitConfig::default(),
        )) as Arc<dyn consolette::ratelimit::AdmissionControl>,
        consolette::metrics::MetricsCollector::new(),
    );
    let state = consolette::entrypoint::EntrypointState {
        dispatch_router: Arc::new(arc_swap::ArcSwap::from_pointee(router)),
        cost_tracker: Arc::new(
            consolette::cost_metrics::tracker::CostTracker::new(
                consolette::cost_metrics::pricing::PricingTable::load_default(),
            )
            .await,
        ),
        metrics: consolette::metrics::MetricsCollector::new(),
        server_info: Arc::new(consolette::entrypoint::ServerInfo {
            port: 0,
            route_name: "test".to_string(),
            strategy: "Fallback".to_string(),
            upstreams: vec![],
        }),
        config_dir: Arc::new(std::path::PathBuf::from("/tmp/consolette-test")),
        session_overrides: Arc::new(
            consolette::routing::session_overrides::SessionOverrideStore::new(),
        ),
        capability: consolette::routing::capability::CapabilityCache::new(
            std::time::Duration::from_secs(consolette::routing::capability::EVAL_TTL_SECS),
        ),
        server_tools: Arc::new(consolette::server_tools::ServerToolsRuntime::default()),
        search_pool: Arc::new(McpSearchPool::new(
            consolette::server_tools::ServerToolsConfig::default().pool_config(),
        )),
    };
    (state, calls)
}

/// No server tools ⇒ today's single-dispatch path, byte-identical response.
#[tokio::test]
async fn entrypoint_should_dispatch_once_unchanged_when_no_server_def() {
    use consolette::entrypoint::entrypoint_router;
    use consolette::providers::ProviderResponse;

    let expected = json!({
        "id": "msg_1", "type": "message", "role": "assistant",
        "content": [{"type": "text", "text": "hello"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 3, "output_tokens": 2}
    });
    let (state, calls) = state_with_response(Ok(ProviderResponse::Full(expected.clone()))).await;

    let request_body = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 100,
        "messages": [{"role": "user", "content": "hi"}],
        "stream": false
    });
    let resp = tower::ServiceExt::oneshot(
        entrypoint_router(state),
        axum::http::Request::post("/v1/messages")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(request_body.to_string()))
            .expect("request build"),
    )
    .await
    .expect("router responds");

    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body bytes");
    let json: Value = serde_json::from_slice(&body).expect("json body");
    assert_eq!(json, expected, "no-server-def path must be byte-identical");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "exactly one dispatch");
}

/// Backend down at loop start (binary missing) ⇒ drop-degrade: defs
/// stripped, single dispatch, verbatim answer — never an error.
#[tokio::test]
async fn loop_should_degrade_to_drop_when_executor_fails() {
    let pool = McpSearchPool::new(PoolConfig {
        binary_path: "/nonexistent/consolette-test-no-binary".to_string(),
        pool_size: 1,
        per_search_timeout_ms: 1_000,
        browser_timeout_ms: 1_000,
    });
    let (dispatch, calls) = searching_upstream();

    let outcome = run_loop(
        &search_request("test-model"),
        &dispatch,
        &pool,
        &test_limits(),
    )
    .await
    .expect("degrade path must still succeed");

    assert!(outcome.degraded);
    assert_eq!(outcome.searches_executed, 0);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "drop ⇒ single dispatch");
}

/// Executor failure must not trip routing cooldowns: the loop returns `Ok`
/// (never `Err(ProviderError)`), so the router's health/admission paths are
/// never engaged for backend outages.
#[tokio::test]
async fn executor_failure_should_not_trip_cooldown_when_backend_down() {
    let pool = McpSearchPool::new(PoolConfig {
        binary_path: "/nonexistent/consolette-test-no-binary".to_string(),
        pool_size: 1,
        per_search_timeout_ms: 1_000,
        browser_timeout_ms: 1_000,
    });
    let (dispatch, _) = searching_upstream();

    let result = run_loop(
        &search_request("test-model"),
        &dispatch,
        &pool,
        &test_limits(),
    )
    .await;

    assert!(
        result.is_ok(),
        "executor failure must degrade, not error: {result:?}"
    );
    assert!(result.expect("ok").degraded);
}

/// V-STREAM-03: the loop total recorded once, then the synthesized SSE tee
/// re-records the identical total — the row must equal the total, never 2×.
#[tokio::test]
async fn stream_cost_should_count_once_when_loop_recorded() {
    use consolette::cost_metrics::tracker::CostTracker;
    use consolette::entrypoint::cost_tee::CostTrackingStream;
    use consolette::session_compaction::CompactionTier;

    let (pool, _) = fake_pool(FakeBehavior::BraveOk(sample_results()), 5_000, 5_000, 2);
    let (dispatch, _) = searching_upstream();
    let outcome = run_loop(
        &search_request("test-model"),
        &dispatch,
        &pool,
        &test_limits(),
    )
    .await
    .expect("loop must succeed");
    let expected_total = outcome.message["usage"]["input_tokens"]
        .as_u64()
        .expect("input tokens")
        + outcome.message["usage"]["output_tokens"]
            .as_u64()
            .expect("output tokens");

    let tracker =
        CostTracker::new(consolette::cost_metrics::pricing::PricingTable::load_default()).await;
    let session = consolette::session_compaction::SessionKey::new("stream-single-count");
    let request_id = consolette::cost_metrics::types::RequestId::new();
    tracker
        .record_pending(&session, request_id, CompactionTier::Off)
        .await;
    consolette::cost_metrics::record_actual_usage_from_anthropic_response(
        &tracker,
        &session,
        request_id,
        "test-model",
        &outcome.message,
    )
    .await;

    // Drive the synthesized frames through the same tee the Stream path uses.
    let tracker = Arc::new(tracker);
    let frames = synthesize_sse(&outcome.message);
    let iter = frames
        .into_iter()
        .map(|frame| Ok::<_, anyhow::Error>(Bytes::from(frame)));
    let boxed: Pin<Box<dyn Stream<Item = Result<Bytes, anyhow::Error>> + Send>> =
        Box::pin(futures_util::stream::iter(iter));
    let mut tee = CostTrackingStream::new(
        boxed,
        Arc::clone(&tracker),
        session.clone(),
        request_id,
        "test-model".to_string(),
    );
    while futures_util::StreamExt::next(&mut tee).await.is_some() {}

    let report = tracker
        .report_for_session(&session)
        .await
        .expect("session report");
    assert_eq!(
        report.actual_tokens,
        Some(expected_total),
        "single-count: row must equal the loop total, not 2×"
    );
}

/// Deferred live end-to-end: needs a real `stapler-mcp` binary WITH the new
/// `browser_web_search` tool, which has not shipped yet. Tracked at
/// <https://github.com/tstapler/stapler-mcp/issues/46>. Kept (not deleted)
/// so going live is one `-- --ignored` run away.
#[tokio::test]
#[ignore = "DEFERRED on stapler-mcp#46: live fallback needs stapler-mcp to ship browser_web_search"]
async fn live_browser_fallback_should_work_once_stapler_mcp_ships_browser_tool() {
    let pool = McpSearchPool::new(PoolConfig {
        binary_path: "stapler-mcp".to_string(),
        pool_size: 1,
        per_search_timeout_ms: 15_000,
        browser_timeout_ms: 30_000,
    });
    let has_browser_tool = pool.live_probe_browser_tool().await.unwrap_or(false);
    assert!(
        has_browser_tool,
        "DEFERRED on https://github.com/tstapler/stapler-mcp/issues/46: \
         live stapler-mcp does not offer browser_web_search yet"
    );
}
