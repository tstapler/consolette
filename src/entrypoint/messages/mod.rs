//! Anthropic-native `POST /v1/messages` (plan.md Phase 2).

use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, CONNECTION, CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures_core::Stream;

use crate::cost_metrics::record_actual_usage_from_anthropic_response;
use crate::cost_metrics::types::RequestId;
use crate::entrypoint::cost_tee::{begin_cost_tracking, CostTrackingStream};
use crate::entrypoint::errors::map_provider_error_anthropic;
use crate::entrypoint::EntrypointState;
use crate::providers::{ProviderError, ProviderResponse};
use crate::server_tools::{run_loop, synthesize_sse, LoopOutcome};
use crate::session_compaction::SessionKey;

/// Rough char-count heuristic for estimating input tokens ahead of
/// dispatch, since no tokenizer dependency is in scope for this feature.
fn estimate_tokens(body: &serde_json::Value) -> u32 {
    u32::try_from(body.to_string().len() / 4).unwrap_or(u32::MAX)
}

fn request_stream_flag(body: &serde_json::Value) -> bool {
    body.get("stream")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn request_model_name(body: &serde_json::Value) -> String {
    body.get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string()
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
    let stream = request_stream_flag(&body);
    let model = request_model_name(&body);
    let est_tokens = estimate_tokens(&body);

    let (session_key, request_id) = begin_cost_tracking(&state.cost_tracker).await;

    // Server-tool emulation (D1): requests carrying a server web_search def
    // on an eligible route run the proxy-internal loop instead of the
    // single-shot path. Requests without server defs fall through to
    // today's path byte-identically (S-3).
    if state.server_tools.should_emulate(&body) {
        return handle_emulated_search(EmulatedSearchParams {
            state,
            body,
            headers,
            model,
            session_key,
            request_id,
            est_tokens,
            stream,
        })
        .await;
    }

    let result = state
        .dispatch_router
        .load()
        .dispatch(body, headers, stream, est_tokens)
        .await;
    handle_direct_dispatch(&state, model, session_key, request_id, result).await
}

/// Handles the direct-dispatch (non-emulated) outcome of `post_v1_messages`:
/// full JSON, tee'd stream, or a mapped provider error.
async fn handle_direct_dispatch(
    state: &EntrypointState,
    model: String,
    session_key: SessionKey,
    request_id: RequestId,
    result: Result<ProviderResponse, ProviderError>,
) -> Response {
    match result {
        Ok(ProviderResponse::Full(json)) => {
            direct_full_response(state, &model, session_key, request_id, json).await
        }
        Ok(ProviderResponse::Stream(s)) => {
            direct_stream_response(state, model, session_key, request_id, s)
        }
        Err(e) => direct_error_response(state, &session_key, request_id, &e).await,
    }
}

async fn direct_full_response(
    state: &EntrypointState,
    model: &str,
    session_key: SessionKey,
    request_id: RequestId,
    json: serde_json::Value,
) -> Response {
    record_model_token_usage(state, model, &json);
    record_actual_usage_from_anthropic_response(
        &state.cost_tracker,
        &session_key,
        request_id,
        model,
        &json,
    )
    .await;
    (StatusCode::OK, Json(json)).into_response()
}

// Does not panic in practice — see `post_v1_messages`' panic documentation.
#[allow(clippy::unwrap_used)]
fn direct_stream_response(
    state: &EntrypointState,
    model: String,
    session_key: SessionKey,
    request_id: RequestId,
    s: Pin<Box<dyn Stream<Item = Result<Bytes, anyhow::Error>> + Send>>,
) -> Response {
    let tee = CostTrackingStream::new(
        s,
        Arc::clone(&state.cost_tracker),
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

// Does not panic in practice — see `post_v1_messages`' panic documentation.
#[allow(clippy::unwrap_used)]
async fn direct_error_response(
    state: &EntrypointState,
    session_key: &SessionKey,
    request_id: RequestId,
    e: &ProviderError,
) -> Response {
    state
        .cost_tracker
        .record_request_failed(session_key, request_id)
        .await;
    let (status, retry_after, error_body) = map_provider_error_anthropic(e);
    let mut response = (status, Json(error_body)).into_response();
    if let Some(retry_after) = retry_after {
        response
            .headers_mut()
            .insert(RETRY_AFTER, retry_after.to_string().parse().unwrap());
    }
    response
}

fn record_model_token_usage(state: &EntrypointState, model: &str, json: &serde_json::Value) {
    if let Some(usage) = crate::providers::extract_usage(json) {
        state
            .metrics
            .counters
            .record_model_tokens(model, usage.input_tokens, usage.output_tokens);
    }
}

/// Config/context bundle for [`handle_emulated_search`] (Fowler's
/// introduce-parameter-object; same pattern as
/// `routing::router::RouterDeps`) — replaces the 8-parameter list this
/// function used to take.
struct EmulatedSearchParams {
    state: EntrypointState,
    body: serde_json::Value,
    headers: HeaderMap,
    model: String,
    session_key: SessionKey,
    request_id: RequestId,
    est_tokens: u32,
    stream: bool,
}

/// Emulated `POST /v1/messages` path: rewrite → bounded loop over the
/// snapshotted router → faithful server-shape turn (Full), or the same turn
/// re-serialized as buffered SSE (Stream, V1).
///
/// Cost is recorded once under the same `(session_key, request_id)` created
/// above (idempotent replace; final == sum of parts). On the Stream path the
/// `CostTrackingStream` tee re-records the identical total parsed from the
/// synthesized `message_delta` usage — single-count by construction (C4).
async fn handle_emulated_search(params: EmulatedSearchParams) -> Response {
    let EmulatedSearchParams {
        state,
        body,
        headers,
        model,
        session_key,
        request_id,
        est_tokens,
        stream,
    } = params;

    let loop_request = EmulationLoopRequest {
        body: &body,
        headers: &headers,
        est_tokens,
    };
    let outcome = match run_emulation_loop(&state, loop_request, &session_key, request_id).await {
        Ok(outcome) => outcome,
        Err(response) => return *response,
    };

    record_emulation_outcome(&state, &session_key, request_id, &model, &outcome).await;

    if stream {
        // Buffered V1 (pre-mortem failure 3): time-to-first-byte degrades to
        // time-to-final-answer for search turns only. Non-search streams are
        // untouched (they never enter this branch).
        stream_response(&state, model, session_key, request_id, &outcome.message)
    } else {
        full_response(outcome.message)
    }
}

/// Records the emulation loop's cost usage and tool-call metrics, and logs
/// its V1 caveats — everything `handle_emulated_search` does with a
/// successful [`LoopOutcome`] before choosing its response framing.
async fn record_emulation_outcome(
    state: &EntrypointState,
    session_key: &SessionKey,
    request_id: RequestId,
    model: &str,
    outcome: &LoopOutcome,
) {
    record_actual_usage_from_anthropic_response(
        &state.cost_tracker,
        session_key,
        request_id,
        model,
        &outcome.message,
    )
    .await;

    record_search_tool_metrics(state, outcome);
    log_emulation_caveats(outcome);
}

/// The per-request inputs `run_emulation_loop` needs to build its dispatch
/// closure — bundled (Fowler's introduce-parameter-object) so the function
/// stays under the project's 5-parameter guideline.
struct EmulationLoopRequest<'a> {
    body: &'a serde_json::Value,
    headers: &'a HeaderMap,
    est_tokens: u32,
}

/// Snapshots the dispatch router and runs the bounded server-tool emulation
/// loop (C1/C3, see module docs above), mapping a genuine dispatch failure
/// straight to its already-recorded-failed error response.
async fn run_emulation_loop(
    state: &EntrypointState,
    request: EmulationLoopRequest<'_>,
    session_key: &SessionKey,
    request_id: RequestId,
) -> Result<LoopOutcome, Box<Response>> {
    let EmulationLoopRequest {
        body,
        headers,
        est_tokens,
    } = request;

    // C1: snapshot the router Arc once — every loop iteration dispatches
    // under the same route view even if `POST /api/route` hot-swaps mid-loop.
    // C3 (known limitation): `est_tokens` is measured pre-loop and ignores
    // search-augmented turns, so admission control may under-count long
    // search sessions. Documented, not gated in V1.
    let router = state.dispatch_router.load_full();
    let dispatch = |next: serde_json::Value| {
        let router = Arc::clone(&router);
        let headers = headers.clone();
        async move {
            match router.dispatch(next, headers, false, est_tokens).await {
                Ok(ProviderResponse::Full(json)) => Ok(json),
                Ok(ProviderResponse::Stream(_)) => Err(ProviderError::ResponseShapeMismatch(
                    "emulation loop dispatched with stream=false but got a stream".to_string(),
                )),
                Err(e) => Err(e),
            }
        }
    };

    let limits = state.server_tools.config.loop_limits();
    match run_loop(body, &dispatch, &*state.search_pool, &limits).await {
        Ok(outcome) => Ok(outcome),
        Err(e) => Err(Box::new(
            record_failure_and_error_response(state, session_key, request_id, &e).await,
        )),
    }
}

/// Records the emulation loop's search/iteration counters onto the shared
/// metrics collector.
fn record_search_tool_metrics(state: &EntrypointState, outcome: &LoopOutcome) {
    for _ in 0..outcome.searches_brave {
        state.metrics.counters.record_server_tool_search_ok("brave");
    }
    for _ in 0..outcome.searches_browser {
        state
            .metrics
            .counters
            .record_server_tool_search_ok("browser");
    }
    for _ in 0..outcome.searches_failed {
        state
            .metrics
            .counters
            .record_server_tool_search_failed("unserved");
    }
    state
        .metrics
        .counters
        .record_server_tool_iterations(u64::from(outcome.iterations));
}

/// Logs the V1 emulation caveats (pre-mortem failures 3/5), once per
/// affected request.
fn log_emulation_caveats(outcome: &LoopOutcome) {
    if outcome.filters_ignored {
        tracing::info!(
            "server-tool emulation ignored domain/location filters (V1 logs-and-ignores)"
        );
    }
    if outcome.degraded {
        tracing::debug!(
            searches = outcome.searches_executed,
            iterations = outcome.iterations,
            "server-tool emulation degraded to best answer"
        );
    }
}

/// Records the request as failed, then maps the provider error to a
/// response via the fallible header-building `error_response` (kept
/// distinct from `handle_direct_dispatch`'s panic-on-malformed-header
/// variant, which that function's `# Panics` doc proves infallible for its
/// own call site — the two were already independent before this refactor).
async fn record_failure_and_error_response(
    state: &EntrypointState,
    session_key: &SessionKey,
    request_id: RequestId,
    e: &ProviderError,
) -> Response {
    state
        .cost_tracker
        .record_request_failed(session_key, request_id)
        .await;
    let (status, retry_after, error_body) = map_provider_error_anthropic(e);
    error_response(retry_after, status, error_body)
}

/// Final SSE framing for the emulated-search stream path: synthesizes SSE
/// frames from the turn, tees them for cost tracking, and builds the
/// `text/event-stream` response.
fn stream_response(
    state: &EntrypointState,
    model: String,
    session_key: SessionKey,
    request_id: RequestId,
    message: &serde_json::Value,
) -> Response {
    let frames = synthesize_sse(message);
    let iter = frames
        .into_iter()
        .map(|frame| Ok::<_, anyhow::Error>(Bytes::from(frame)));
    let boxed: Pin<Box<dyn Stream<Item = Result<Bytes, anyhow::Error>> + Send>> =
        Box::pin(futures_util::stream::iter(iter));
    let tee = CostTrackingStream::new(
        boxed,
        Arc::clone(&state.cost_tracker),
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
        // Static valid literals + a stream body: infallible in practice;
        // see `post_v1_messages`' panic documentation.
        .unwrap_or_else(|_| (StatusCode::OK, Json(serde_json::json!({}))).into_response())
}

/// Final JSON framing for the emulated-search full-response path.
fn full_response(message: serde_json::Value) -> Response {
    (StatusCode::OK, Json(message)).into_response()
}

/// Shared provider-error mapping for the Full paths.
fn error_response(
    retry_after: Option<u64>,
    status: StatusCode,
    error_body: serde_json::Value,
) -> Response {
    let mut response = (status, Json(error_body)).into_response();
    if let Some(retry_after) = retry_after {
        if let Ok(value) = retry_after.to_string().parse() {
            response.headers_mut().insert(RETRY_AFTER, value);
        }
    }
    response
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

    // The list is sorted immediately below, so dedup-after-sort (which
    // removes every duplicate, not just adjacent ones) is equivalent to the
    // original insertion-order "push if not already present" loop.
    let mut ids: Vec<String> = config
        .routes
        .first()
        .into_iter()
        .flat_map(|route| route.upstreams.iter())
        .filter_map(|u| u.model.clone())
        .collect();
    ids.sort();
    ids.dedup();

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
mod tests;
