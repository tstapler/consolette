//! Hand-rolled `axum`/`tokio` local test server for HTTP-mocking in
//! `cost_metrics` tests (Task 1.2.2c).
//!
//! Deliberately **not** `wiremock`: `Cargo.toml`'s `[dev-dependencies]` has
//! no HTTP-mocking crate, and `axum`/`tokio` are already direct dependencies
//! of this crate. This is the repair-iteration-1 decision recorded in
//! `project_plans/compaction-cost-metrics/implementation/plan.md` (Blocker
//! 11) — every test in Epics 1.2 and 4 that needs an HTTP double uses this
//! module instead of adding a new supply-chain dependency.
//!
//! Only `#[cfg(test)]` code in this crate should depend on this module, but
//! it is declared as an ordinary module (not gated) per the plan's Task
//! 1.2.2c file list, so it is available to integration tests in `tests/`
//! too if a later epic needs it there.

// This is test-support scaffolding, not production request-handling code:
// a poisoned mutex or a bind failure means the test fixture itself is
// broken, so panicking immediately (with a message pointing at the cause)
// is more useful here than propagating a `Result` callers would just
// `.unwrap()` anyway.
#![allow(clippy::expect_used)]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// One canned response the mock server returns, in order, for successive
/// requests. Once the queue has exactly one entry left, that entry is
/// returned (by clone) for all further requests, so a single-entry queue
/// behaves as "always respond this way."
#[derive(Debug, Clone)]
pub struct MockResponse {
    pub status: StatusCode,
    pub body: serde_json::Value,
}

impl MockResponse {
    #[must_use]
    pub fn ok(body: serde_json::Value) -> Self {
        Self {
            status: StatusCode::OK,
            body,
        }
    }

    #[must_use]
    pub fn status(status: StatusCode) -> Self {
        Self {
            status,
            body: serde_json::json!({}),
        }
    }
}

/// What the mock server observed across all requests it has served so far,
/// for test assertions.
#[derive(Debug, Default)]
pub struct MockObservations {
    pub request_count: AtomicUsize,
    pub concurrent_high_water_mark: AtomicUsize,
    in_flight: AtomicUsize,
    pub last_headers: Mutex<Option<HeaderMap>>,
}

struct ServerState {
    responses: Mutex<VecDeque<MockResponse>>,
    observations: Arc<MockObservations>,
    /// Artificial per-request delay, giving concurrency tests a window in
    /// which overlapping in-flight requests can be observed.
    response_delay: Duration,
}

/// A running hand-rolled mock HTTP server plus a handle to shut it down.
///
/// The server is torn down when this value is dropped.
pub struct MockServer {
    pub addr: SocketAddr,
    pub observations: Arc<MockObservations>,
    handle: JoinHandle<()>,
}

impl MockServer {
    /// Start a mock server that responds to `POST /v1/messages/count_tokens`
    /// with the given canned responses, in order, with no artificial delay.
    pub async fn start(responses: Vec<MockResponse>) -> Self {
        Self::start_with_delay(responses, Duration::ZERO).await
    }

    /// Same as `start`, but holds each request open for `delay` before
    /// responding. Used by the bounded-concurrency test (Task 1.2.2d) to
    /// create a window in which the mock server's concurrent-request
    /// high-water mark can be observed.
    ///
    /// # Panics
    ///
    /// Panics if binding a local ephemeral port or reading its address
    /// fails — an environment failure that means the test fixture itself
    /// is broken, not a condition callers should handle.
    pub async fn start_with_delay(responses: Vec<MockResponse>, delay: Duration) -> Self {
        let observations = Arc::new(MockObservations::default());
        let state = Arc::new(ServerState {
            responses: Mutex::new(VecDeque::from(responses)),
            observations: observations.clone(),
            response_delay: delay,
        });

        let app = Router::new()
            .route("/v1/messages/count_tokens", post(handle_count_tokens))
            .with_state(state);

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock server bind should succeed");
        let addr = listener
            .local_addr()
            .expect("mock server local_addr should succeed");

        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            addr,
            observations,
            handle,
        }
    }

    /// Base URL (`http://127.0.0.1:<port>`) for pointing an estimator at
    /// this mock server.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn handle_count_tokens(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    _body: Bytes,
) -> impl IntoResponse {
    let obs = &state.observations;
    obs.request_count.fetch_add(1, Ordering::SeqCst);
    *obs.last_headers.lock().expect("mock server mutex poisoned") = Some(headers);

    let in_flight = obs.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    obs.concurrent_high_water_mark
        .fetch_max(in_flight, Ordering::SeqCst);

    if !state.response_delay.is_zero() {
        tokio::time::sleep(state.response_delay).await;
    }

    let response = {
        let mut queue = state.responses.lock().expect("mock server mutex poisoned");
        if queue.len() > 1 {
            queue.pop_front().expect("queue checked non-empty above")
        } else {
            queue
                .front()
                .cloned()
                .unwrap_or_else(|| MockResponse::ok(serde_json::json!({})))
        }
    };

    obs.in_flight.fetch_sub(1, Ordering::SeqCst);

    (response.status, axum::Json(response.body))
}
