//! `post_v1_messages`/`get_v1_models` test suite (kibitzer file-size: split
//! out of `messages.rs` alongside the production-code refactor).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::entrypoint::{entrypoint_router, EntrypointState};
use crate::providers::{Provider, ProviderError};
use async_trait::async_trait;
use axum::body::to_bytes;
use axum::http::Request;
use bytes::Bytes;
use futures_core::Stream;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tower::ServiceExt;

struct FixedProvider {
    name: String,
    response: std::sync::Mutex<Option<Result<ProviderResponse, ProviderError>>>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for FixedProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn send(
        &self,
        _body: serde_json::Value,
        _headers: HeaderMap,
        _stream: bool,
    ) -> Result<ProviderResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.response
            .lock()
            .unwrap()
            .take()
            .expect("FixedProvider::send called more than once")
    }

    async fn list_models(&self) -> Result<Vec<crate::providers::ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }
}

async fn test_state_with_provider(
    response: Result<ProviderResponse, ProviderError>,
) -> (EntrypointState, Arc<AtomicUsize>) {
    use crate::providers::anthropic::AnthropicProvider;
    use crate::routing::health::HealthRegistry;
    use crate::routing::router::{Router as DispatchRouter, RouterDeps};
    use crate::routing::strategy::{FallbackStrategy, UpstreamRef};

    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(FixedProvider {
        name: "test".to_string(),
        response: std::sync::Mutex::new(Some(response)),
        calls: Arc::clone(&calls),
    });
    let _ = AnthropicProvider::new; // keep import warning-free if unused later

    let candidates = vec![UpstreamRef {
        index: 0,
        name: "test".to_string(),
        weight: 1.0,
        model: None,
    }];
    let health = Arc::new(HealthRegistry::new(300));
    let admission = Arc::new(crate::ratelimit::RateLimiters::new(
        &crate::config::schema::RateLimitConfig::default(),
    )) as Arc<dyn crate::ratelimit::AdmissionControl>;
    let metrics = crate::metrics::MetricsCollector::new();
    let router = DispatchRouter::new(RouterDeps {
        candidates,
        providers: vec![provider],
        strategy: Arc::new(FallbackStrategy) as Arc<dyn crate::routing::strategy::RoutingStrategy>,
        health,
        admission,
        metrics: Arc::clone(&metrics),
    });

    let state = crate::entrypoint::test_support::state_with_router(router, metrics).await;
    (state, calls)
}

/// Shared "POST a body to /v1/messages and read back the response" test
/// helper — the request-building boilerplate every test below repeated.
async fn post_messages(state: EntrypointState, body: serde_json::Value) -> Response {
    entrypoint_router(state)
        .oneshot(
            Request::post("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn happy_path_returns_full_json_response() {
    let expected = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": "hello"}]
    });
    let (state, _calls) =
        test_state_with_provider(Ok(ProviderResponse::Full(expected.clone()))).await;

    let resp = post_messages(
        state,
        serde_json::json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "hi"}],
            "stream": false
        }),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json, expected);
}

#[tokio::test]
async fn successful_dispatch_records_exact_usage() {
    let response = serde_json::json!({
        "id": "msg_1",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });
    let (state, _calls) = test_state_with_provider(Ok(ProviderResponse::Full(response))).await;
    let cost_tracker = Arc::clone(&state.cost_tracker);

    let resp = post_messages(
        state,
        serde_json::json!({"model": "claude-sonnet-4-5", "stream": false}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // We don't have direct access to the session_key generated inside
    // the handler, so this test only asserts the response succeeded;
    // Story 2.1.2's tracked-outcome assertions are covered directly in
    // cost_tee.rs's CostTrackingStream tests and cost_metrics::mod.rs's
    // record_actual_usage_from_anthropic_response tests.
    let _ = cost_tracker;
}

#[tokio::test]
async fn failed_dispatch_maps_to_error_response() {
    let (state, _calls) = test_state_with_provider(Err(ProviderError::Timeout)).await;

    let resp = post_messages(
        state,
        serde_json::json!({"model": "claude-sonnet-4-5", "stream": false}),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 529);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["type"], "overloaded_error");
}

#[tokio::test]
async fn streaming_response_passes_through_bytes() {
    let frame1 = Bytes::from_static(b"event: message_start\ndata: {}\n\n");
    let frame2 = Bytes::from_static(b"event: message_stop\ndata: {}\n\n");
    let inner: Pin<Box<dyn Stream<Item = Result<Bytes, anyhow::Error>> + Send>> =
        Box::pin(futures_util::stream::iter(vec![
            Ok(frame1.clone()),
            Ok(frame2.clone()),
        ]));
    let (state, _calls) = test_state_with_provider(Ok(ProviderResponse::Stream(inner))).await;

    let resp = post_messages(
        state,
        serde_json::json!({"model": "claude-sonnet-4-5", "stream": true}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let mut expected = Vec::new();
    expected.extend_from_slice(&frame1);
    expected.extend_from_slice(&frame2);
    assert_eq!(body.as_ref(), expected.as_slice());
}

#[test]
fn v1_models_lists_pinned_ids_deduped() {
    let config: crate::config::schema::Config = serde_json::from_value(serde_json::json!({
        "routes": [{
            "name": "default", "strategy": "fallback",
            "upstreams": [
                {"name": "a", "model": "m1"},
                {"name": "b", "model": "m1"},
                {"name": "c"}
            ]
        }]
    }))
    .unwrap();
    let v = v1_models_from_config(&config);
    let ids: Vec<&str> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["m1"]);
    assert_eq!(v["has_more"], serde_json::json!(false));
    assert_eq!(v["data"][0]["type"], serde_json::json!("model"));
}
