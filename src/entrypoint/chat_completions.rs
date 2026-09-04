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
use crate::entrypoint::openai_stream::OpenAiStreamTranslator;
use crate::entrypoint::EntrypointState;
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

    let anthropic_body = translate_openai_to_anthropic(&body);

    let (session_key, request_id) = begin_cost_tracking(&state.cost_tracker).await;

    match state
        .dispatch_router
        .load()
        .dispatch(anthropic_body, headers, stream, est_tokens)
        .await
    {
        Ok(ProviderResponse::Full(json)) => {
            let openai_json =
                translate_and_record(&state.cost_tracker, &session_key, request_id, &json).await;
            (StatusCode::OK, Json(openai_json)).into_response()
        }
        Ok(ProviderResponse::Stream(s)) => {
            let tee = CostTrackingStream::new(
                s,
                std::sync::Arc::clone(&state.cost_tracker),
                session_key,
                request_id,
                model.clone(),
            );
            let translated = OpenAiStreamTranslator::new(tee, model);
            Response::builder()
                .status(StatusCode::OK)
                .header(CONTENT_TYPE, "text/event-stream")
                .header(CACHE_CONTROL, "no-cache")
                .header(CONNECTION, "keep-alive")
                .body(Body::from_stream(translated))
                .unwrap()
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
            response
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
        use crate::routing::router::Router as DispatchRouter;
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
}
