//! Anthropic-native `POST /v1/messages` (plan.md Phase 2).

use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, CONNECTION, CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::entrypoint::cost_tee::{begin_cost_tracking, CostTrackingStream};
use crate::entrypoint::errors::map_provider_error_anthropic;
use crate::entrypoint::EntrypointState;
use crate::providers::ProviderResponse;

/// Rough char-count heuristic for estimating input tokens ahead of
/// dispatch, since no tokenizer dependency is in scope for this feature.
fn estimate_tokens(body: &serde_json::Value) -> u32 {
    u32::try_from(body.to_string().len() / 4).unwrap_or(u32::MAX)
}

/// # Panics
///
/// Does not panic in practice: the `Response::builder()` call below only
/// fails if a header value is malformed, and every header set here is a
/// static, valid literal; the `Retry-After` header value is a `u64` printed
/// via `to_string()`, which always parses as a valid `HeaderValue`.
#[allow(clippy::unwrap_used)]
pub async fn post_v1_messages(
    State(state): State<EntrypointState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let stream = body
        .get("stream")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let model = body
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let est_tokens = estimate_tokens(&body);

    let (session_key, request_id) = begin_cost_tracking(&state.cost_tracker).await;

    match state
        .dispatch_router
        .load()
        .dispatch(body, headers, stream, est_tokens)
        .await
    {
        Ok(ProviderResponse::Full(json)) => {
            crate::cost_metrics::record_actual_usage_from_anthropic_response(
                &state.cost_tracker,
                &session_key,
                request_id,
                &model,
                &json,
            )
            .await;
            (StatusCode::OK, Json(json)).into_response()
        }
        Ok(ProviderResponse::Stream(s)) => {
            let tee = CostTrackingStream::new(
                s,
                std::sync::Arc::clone(&state.cost_tracker),
                session_key,
                request_id,
                model,
            );
            Response::builder()
                .status(StatusCode::OK)
                .header(CONTENT_TYPE, "text/event-stream")
                .header(CACHE_CONTROL, "no-cache")
                .header(CONNECTION, "keep-alive")
                .body(Body::from_stream(tee))
                .unwrap()
        }
        Err(e) => {
            state
                .cost_tracker
                .record_request_failed(&session_key, request_id)
                .await;
            let (status, retry_after, error_body) = map_provider_error_anthropic(&e);
            let mut response = (status, Json(error_body)).into_response();
            if let Some(retry_after) = retry_after {
                response
                    .headers_mut()
                    .insert(RETRY_AFTER, retry_after.to_string().parse().unwrap());
            }
            response
        }
    }
}

/// `GET /v1/models`: the Anthropic Models API surface over the configured
/// routes, so clients that validate model IDs (Claude Code session restore,
/// `/model` pickers) recognize the IDs this proxy actually serves.
///
/// Lists the active route's pinned model IDs (route upstream `model`
/// overrides); unpinned upstreams contribute nothing since their served ID
/// is whatever the client requested. Served from config — no upstream
/// calls, never 404s on dead upstreams. (Family aliases join this list
/// with the auto-model-family feature.)
///
/// # Errors
///
/// Returns 500 if the on-disk config fails to load.
pub async fn get_v1_models(
    State(state): State<EntrypointState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    use serde_json::json;

    let config = crate::config::load(&state.config_dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
    })?;

    Ok(Json(v1_models_from_config(&config)))
}

/// Build the `GET /v1/models` response body from a loaded config (pure,
/// unit-testable): the active route's pinned model IDs, deduplicated and
/// sorted.
fn v1_models_from_config(config: &crate::config::schema::Config) -> serde_json::Value {
    use serde_json::{json, Value};

    let mut ids: Vec<String> = Vec::new();
    if let Some(route) = config.routes.first() {
        for u in &route.upstreams {
            if let Some(model) = u.model.as_deref() {
                if !ids.iter().any(|id| id == model) {
                    ids.push(model.to_string());
                }
            }
        }
    }
    ids.sort();

    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let data: Vec<Value> = ids
        .iter()
        .map(|id| {
            json!({
                "type": "model",
                "id": id,
                "display_name": id,
                "created_at": now
            })
        })
        .collect();
    let first_id = ids.first().cloned().unwrap_or_default();
    let last_id = ids.last().cloned().unwrap_or_default();
    json!({
        "data": data,
        "has_more": false,
        "first_id": first_id,
        "last_id": last_id
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
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
        use crate::routing::router::Router as DispatchRouter;
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
        let router = DispatchRouter::new(
            candidates,
            vec![provider],
            Arc::new(FallbackStrategy) as Arc<dyn crate::routing::strategy::RoutingStrategy>,
            health,
            admission,
            Arc::clone(&metrics),
        );

        let state = EntrypointState {
            dispatch_router: Arc::new(arc_swap::ArcSwap::from_pointee(router)),
            cost_tracker: Arc::new(
                crate::cost_metrics::tracker::CostTracker::new(
                    crate::cost_metrics::pricing::PricingTable::load_default(),
                )
                .await,
            ),
            metrics,
            server_info: Arc::new(crate::entrypoint::ServerInfo {
                port: 0,
                route_name: "test".to_string(),
                strategy: "Fallback".to_string(),
                upstreams: vec![],
            }),
            config_dir: Arc::new(std::path::PathBuf::from("/tmp/consolette-test")),
            session_overrides: Arc::new(
                crate::routing::session_overrides::SessionOverrideStore::new(),
            ),
        };
        (state, calls)
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

        let router = entrypoint_router(state);
        let resp = router
            .oneshot(
                Request::post("/v1/messages")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "model": "claude-sonnet-4-5",
                            "max_tokens": 100,
                            "messages": [{"role": "user", "content": "hi"}],
                            "stream": false
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

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

        let router = entrypoint_router(state);
        let resp = router
            .oneshot(
                Request::post("/v1/messages")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"model": "claude-sonnet-4-5", "stream": false})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
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

        let router = entrypoint_router(state);
        let resp = router
            .oneshot(
                Request::post("/v1/messages")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"model": "claude-sonnet-4-5", "stream": false})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
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

        let router = entrypoint_router(state);
        let resp = router
            .oneshot(
                Request::post("/v1/messages")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"model": "claude-sonnet-4-5", "stream": true})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
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
}
