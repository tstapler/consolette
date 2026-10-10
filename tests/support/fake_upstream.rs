//! In-process axum fake of an Anthropic-style upstream, scriptable per request.
//!
//! Each non-`GET /v1/models` request consumes the next [`Step`]; the last step
//! repeats once the script is exhausted. Every request is recorded.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use futures_util::stream;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// One scripted upstream behavior.
#[derive(Clone, Debug)]
pub enum Step {
    Status(u16),
    StatusWithRetryAfter(u16, u64),
    Ok(Value),
    /// Never sends headers.
    Hang,
    /// Sends 200 headers, then never a body byte.
    HangAfterHeaders,
    /// Anthropic SSE frames, one text delta per chunk, `delay` between chunks.
    Stream(Vec<String>, Duration),
    /// Wait `Duration` before sending the inner step's headers.
    Delayed(Duration, Box<Step>),
}

#[derive(Clone, Debug)]
pub struct Script {
    steps: Vec<Step>,
    models: Vec<String>,
}

/// Result of [`Script::delay_headers`]; finish with [`DelayedHeaders::then`].
pub struct DelayedHeaders(Duration);

impl DelayedHeaders {
    pub fn then(self, step: Step) -> Script {
        Script::sequence([Step::Delayed(self.0, Box::new(step))])
    }
}

impl Script {
    pub fn sequence(steps: impl IntoIterator<Item = Step>) -> Self {
        Self {
            steps: steps.into_iter().collect(),
            models: Vec::new(),
        }
    }

    pub fn hang() -> Self {
        Self::sequence([Step::Hang])
    }

    pub fn hang_after_headers() -> Self {
        Self::sequence([Step::HangAfterHeaders])
    }

    pub fn models<S: Into<String>>(ids: impl IntoIterator<Item = S>) -> Self {
        Self {
            steps: vec![Step::Ok(anthropic_message())],
            models: ids.into_iter().map(Into::into).collect(),
        }
    }

    pub fn stream_chunks<S: Into<String>>(
        chunks: impl IntoIterator<Item = S>,
        delay: Duration,
    ) -> Self {
        Self::sequence([Step::Stream(
            chunks.into_iter().map(Into::into).collect(),
            delay,
        )])
    }

    pub fn delay_headers(delay: Duration) -> DelayedHeaders {
        DelayedHeaders(delay)
    }

    pub fn status_with_retry_after(status: u16, secs: u64) -> Self {
        Self::sequence([Step::StatusWithRetryAfter(status, secs)])
    }
}

/// A minimal valid Anthropic `/v1/messages` response.
pub fn anthropic_message() -> Value {
    json!({
        "id": "msg_fake",
        "type": "message",
        "role": "assistant",
        "model": "fake-model",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
}

#[derive(Default)]
struct Shared {
    queue: VecDeque<Step>,
    last: Option<Step>,
    bodies: Vec<Value>,
    headers: Vec<Vec<(String, String)>>,
}

#[derive(Clone)]
struct AppState {
    shared: Arc<Mutex<Shared>>,
    models: Arc<Vec<String>>,
}

pub struct FakeUpstream {
    addr: std::net::SocketAddr,
    shared: Arc<Mutex<Shared>>,
    task: JoinHandle<()>,
}

impl Drop for FakeUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeUpstream {
    /// Binds `127.0.0.1:0` (OS-assigned port) and serves `script`.
    pub async fn start(script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake upstream");
        let addr = listener.local_addr().expect("local addr");
        let shared = Arc::new(Mutex::new(Shared {
            queue: script.steps.into(),
            ..Shared::default()
        }));
        let state = AppState {
            shared: shared.clone(),
            models: Arc::new(script.models),
        };
        let app = Router::new().fallback(handle).with_state(state);
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { addr, shared, task }
    }

    /// `http://127.0.0.1:<port>`, without a trailing slash or path.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Parsed JSON request bodies in arrival order (`null` if not JSON).
    pub fn received_bodies(&self) -> Vec<Value> {
        self.shared.lock().expect("lock").bodies.clone()
    }

    /// Request headers (lower-cased names) in arrival order, queryable via `get(name)`.
    pub fn received_headers(&self) -> Vec<RecordedHeaders> {
        let guard = self.shared.lock().expect("lock");
        guard.headers.iter().cloned().map(RecordedHeaders).collect()
    }
}

#[derive(Clone, Debug)]
pub struct RecordedHeaders(Vec<(String, String)>);

impl RecordedHeaders {
    /// Value of header `name` (case-insensitive); panics if absent.
    pub fn get(&self, name: &str) -> &str {
        self.0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("header {name} not recorded"))
    }
}

fn record(shared: &Mutex<Shared>, headers: &HeaderMap, body: &[u8]) {
    let parsed = serde_json::from_slice(body).unwrap_or(Value::Null);
    let hs = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string()))
        .collect();
    let mut g = shared.lock().expect("lock");
    g.bodies.push(parsed);
    g.headers.push(hs);
}

fn next_step(shared: &Mutex<Shared>) -> Step {
    let mut g = shared.lock().expect("lock");
    if let Some(step) = g.queue.pop_front() {
        g.last = Some(step.clone());
        step
    } else {
        g.last.clone().unwrap_or(Step::Status(500))
    }
}

async fn handle(State(state): State<AppState>, req: Request<Body>) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    record(&state.shared, &parts.headers, &bytes);

    if parts.method == Method::GET && parts.uri.path() == "/v1/models" {
        let data: Vec<Value> = state
            .models
            .iter()
            .map(|id| json!({"type": "model", "id": id, "display_name": id}))
            .collect();
        return json_response(
            StatusCode::OK,
            &json!({"data": data, "has_more": false}),
            None,
        );
    }
    render(next_step(&state.shared)).await
}

fn json_response(status: StatusCode, v: &Value, retry_after: Option<u64>) -> Response {
    let mut b = Response::builder()
        .status(status)
        .header("content-type", "application/json");
    if let Some(s) = retry_after {
        b = b.header("retry-after", s.to_string());
    }
    b.body(Body::from(v.to_string())).expect("response")
}

fn error_body(status: u16) -> Value {
    json!({"type": "error", "error": {"type": "api_error", "message": format!("fake upstream status {status}")}})
}

fn sse_frame(event: &str, data: Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

async fn render(step: Step) -> Response {
    match step {
        Step::Status(code) => {
            let s = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            json_response(s, &error_body(code), None)
        }
        Step::StatusWithRetryAfter(code, secs) => {
            let s = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            json_response(s, &error_body(code), Some(secs))
        }
        Step::Ok(v) => json_response(StatusCode::OK, &v, None),
        Step::Hang => std::future::pending().await,
        Step::HangAfterHeaders => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(stream::pending::<
                Result<Bytes, Infallible>,
            >()))
            .expect("response"),
        Step::Delayed(d, inner) => {
            tokio::time::sleep(d).await;
            Box::pin(render(*inner)).await
        }
        Step::Stream(chunks, delay) => {
            let mut frames = vec![
                sse_frame(
                    "message_start",
                    json!({"type": "message_start", "message": {
                    "id": "msg_fake", "type": "message", "role": "assistant", "model": "fake-model",
                    "content": [], "stop_reason": null, "usage": {"input_tokens": 1, "output_tokens": 0}}}),
                ),
                sse_frame(
                    "content_block_start",
                    json!({"type": "content_block_start", "index": 0,
                    "content_block": {"type": "text", "text": ""}}),
                ),
            ];
            let mut out: Vec<(Duration, Bytes)> =
                frames.drain(..).map(|f| (Duration::ZERO, f)).collect();
            for (i, c) in chunks.iter().enumerate() {
                let wait = if i == 0 { Duration::ZERO } else { delay };
                out.push((
                    wait,
                    sse_frame(
                        "content_block_delta",
                        json!({
                    "type": "content_block_delta", "index": 0,
                    "delta": {"type": "text_delta", "text": c}}),
                    ),
                ));
            }
            out.push((
                delay,
                sse_frame(
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": 0}),
                ),
            ));
            out.push((Duration::ZERO, sse_frame("message_delta", json!({"type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": null}, "usage": {"output_tokens": 1}}))));
            out.push((
                Duration::ZERO,
                sse_frame("message_stop", json!({"type": "message_stop"})),
            ));
            let body = Body::from_stream(stream::unfold(out.into_iter(), |mut it| async move {
                let (wait, frame) = it.next()?;
                if !wait.is_zero() {
                    tokio::time::sleep(wait).await;
                }
                Some((Ok::<Bytes, Infallible>(frame), it))
            }));
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(body)
                .expect("response")
        }
    }
}
