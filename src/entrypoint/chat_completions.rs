//! OpenAI-compatible `POST /v1/chat/completions` (plan.md Phase 3).
//!
//! Thin adapter over the same `Router::dispatch` seam `messages.rs` uses:
//! translate the OpenAI-shaped request body to Anthropic's wire format,
//! dispatch, then translate the (Anthropic-shaped) response back.

use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, CONNECTION, CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::entrypoint::cost_tee::{begin_cost_tracking, CostTrackingStream};
use crate::entrypoint::errors::map_provider_error_openai;
use crate::entrypoint::messages::HandlerResult;
use crate::entrypoint::openai_stream::OpenAiStreamTranslator;
use crate::entrypoint::upstream_override::take_upstream_override;
use crate::entrypoint::EntrypointState;
use crate::metrics::trace::TraceScope;
use crate::providers::{translate_and_record, translate_openai_to_anthropic, ProviderResponse};

/// Rough char-count heuristic for estimating input tokens ahead of dispatch,
/// matching `messages.rs::estimate_tokens` — no tokenizer dependency is in
/// scope for this feature.
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
pub async fn post_v1_chat_completions(
    State(state): State<EntrypointState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let est_tokens = estimate_tokens(&body);
    let anthropic_body = translate_openai_to_anthropic(&body);
    let mut scope = TraceScope::begin(
        &state.metrics,
        &anthropic_body,
        request_stream_flag(&body),
        u64::from(est_tokens),
    );
    let result = chat_completions_inner(&mut scope, state, headers, body, anthropic_body).await;
    scope.finish(result.outcome);
    scope.mark_handler_returned();
    result.response
}

fn request_stream_flag(body: &serde_json::Value) -> bool {
    body.get("stream")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

// Does not panic in practice — see `post_v1_chat_completions`' panic documentation.
#[allow(clippy::unwrap_used)]
async fn chat_completions_inner(
    scope: &mut TraceScope,
    state: EntrypointState,
    mut headers: HeaderMap,
    body: serde_json::Value,
    anthropic_body: serde_json::Value,
) -> HandlerResult {
    match take_upstream_override(&mut headers) {
        Ok(Some(upstream)) => scope.force_upstream(upstream),
        Ok(None) => {}
        Err(e) => {
            let (status, _, error_body) = map_provider_error_openai(&e);
            return HandlerResult::failed((status, Json(error_body)).into_response(), &e);
        }
    }
    let stream = request_stream_flag(&body);
    let model = body
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let est_tokens = estimate_tokens(&body);

    let (session_key, request_id) = begin_cost_tracking(&state.cost_tracker).await;

    let result = state
        .dispatch_router
        .load()
        .dispatch_traced(scope, anthropic_body, headers, stream, est_tokens)
        .await;
    let cost_model = scope
        .priced_model()
        .map_or_else(|| model.clone(), str::to_string);
    match result {
        Ok(ProviderResponse::Full(json)) => {
            if let Some(usage) = crate::providers::extract_usage(&json) {
                state.metrics.counters.record_model_tokens(
                    &model,
                    usage.input_tokens,
                    usage.output_tokens,
                );
            }
            let openai_json = translate_and_record(
                &state.cost_tracker,
                &session_key,
                request_id,
                &cost_model,
                &json,
            )
            .await;
            HandlerResult::ok((StatusCode::OK, Json(openai_json)).into_response())
        }
        Ok(ProviderResponse::Stream(s)) => {
            let tee = CostTrackingStream::new(
                s,
                std::sync::Arc::clone(&state.cost_tracker),
                session_key,
                request_id,
                cost_model,
            );
            let translated = OpenAiStreamTranslator::new(tee, model);
            HandlerResult::ok(
                Response::builder()
                    .status(StatusCode::OK)
                    .header(CONTENT_TYPE, "text/event-stream")
                    .header(CACHE_CONTROL, "no-cache")
                    .header(CONNECTION, "keep-alive")
                    .body(Body::from_stream(translated))
                    .unwrap(),
            )
        }
        Err(e) => {
            state
                .cost_tracker
                .record_request_failed(&session_key, request_id)
                .await;
            let (status, retry_after, error_body) = map_provider_error_openai(&e);
            let mut response = (status, Json(error_body)).into_response();
            if let Some(retry_after) = retry_after {
                response
                    .headers_mut()
                    .insert(RETRY_AFTER, retry_after.to_string().parse().unwrap());
            }
            HandlerResult::failed(response, &e)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::config::schema::Config;
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
        use crate::routing::health::HealthRegistry;
        use crate::routing::router::{Router as DispatchRouter, RouterDeps};
        use crate::routing::strategy::{FallbackStrategy, UpstreamRef};

        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(FixedProvider {
            name: "test".to_string(),
            response: std::sync::Mutex::new(Some(response)),
            calls: Arc::clone(&calls),
        });

        let candidates = vec![UpstreamRef {
            index: 0,
            name: "test".to_string(),
            weight: 1.0,
            model: None,
            model_family: None,
        }];
        let health = Arc::new(HealthRegistry::new(300));
        let admission = Arc::new(crate::ratelimit::RateLimiters::new(
            &crate::config::schema::RateLimitConfig::default(),
        )) as Arc<dyn crate::ratelimit::AdmissionControl>;
        let metrics = crate::metrics::MetricsCollector::new();
        let router = DispatchRouter::new(RouterDeps {
            candidates,
            providers: vec![provider],
            strategy: Arc::new(FallbackStrategy)
                as Arc<dyn crate::routing::strategy::RoutingStrategy>,
            health,
            admission,
            metrics: Arc::clone(&metrics),
        });

        let state = crate::entrypoint::test_support::state_with_router(router, metrics).await;
        let _ = Config::default;
        (state, calls)
    }

    #[tokio::test]
    async fn happy_path_returns_openai_shaped_response() {
        let anthropic_response = serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let (state, _calls) =
            test_state_with_provider(Ok(ProviderResponse::Full(anthropic_response))).await;

        let router = entrypoint_router(state);
        let resp = router
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "model": "claude-sonnet-4-5",
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
        assert_eq!(json["object"], "chat.completion");
        assert_eq!(json["choices"][0]["message"]["content"], "hello");
    }

    #[tokio::test]
    async fn failed_dispatch_maps_to_openai_error_response() {
        let (state, _calls) = test_state_with_provider(Err(ProviderError::Timeout)).await;

        let router = entrypoint_router(state);
        let resp = router
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"model": "claude-sonnet-4-5", "stream": false})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["type"], "server_error");
    }

    #[tokio::test]
    async fn post_v1_chat_completions_should_emit_trace_status_when_translation_error() {
        let (state, _calls) =
            test_state_with_provider(Err(ProviderError::Validation("bad".to_string(), 400))).await;
        let metrics = Arc::clone(&state.metrics);

        let resp = entrypoint_router(state)
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"model": "claude-sonnet-4-5", "stream": false})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let traces = metrics.get_recent_requests(10);
        assert_eq!(traces.len(), 1, "one trace per client request");
        assert_eq!(traces[0].status_code, 400);
    }

    #[tokio::test]
    async fn streaming_response_is_translated_to_openai_chunks() {
        let frame1 = Bytes::from_static(
            b"event: content_block_delta\ndata: {\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
        );
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
                Request::post("/v1/chat/completions")
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
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("chat.completion.chunk"));
        assert!(text.contains("\"content\":\"Hi\""));
        assert!(text.contains("\"finish_reason\":\"stop\""));
        assert!(text.ends_with("data: [DONE]\n\n"));
    }

    async fn post_unpriced_chat(state: &EntrypointState, stream: bool) {
        let resp = entrypoint_router(state.clone())
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "model": "unpriced-requested-model",
                            "messages": [{"role": "user", "content": "hi"}],
                            "stream": stream
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    }

    #[tokio::test]
    async fn full_response_prices_cost_by_resolved_family_model() {
        let state = crate::entrypoint::test_support::cost_probe_state(false).await;
        post_unpriced_chat(&state, false).await;
        crate::entrypoint::test_support::assert_cost_priced_by_routed_model(&state).await;
    }

    #[tokio::test]
    async fn stream_response_prices_cost_by_resolved_family_model() {
        let state = crate::entrypoint::test_support::cost_probe_state(false).await;
        post_unpriced_chat(&state, true).await;
        crate::entrypoint::test_support::assert_cost_priced_by_routed_model(&state).await;
    }
}
