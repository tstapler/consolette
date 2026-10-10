//! Real-socket client abort (Task 1.1.3k). ADR-002's load-bearing assumption:
//! axum drops the handler future when the client disconnects, so the
//! `TraceScope` inside `Router::dispatch` emits one 499 trace.
//!
//! Wide tolerances, real time (no paused clock).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)] // test crate: a panic is the failure signal

mod support;

use std::time::Duration;

use support::fake_upstream::{FakeUpstream, Script};
use support::live_server::LiveServer;

#[tokio::test]
async fn client_abort_should_emit_one_499_trace_with_ring_buffer_entry_and_counters_cancelled_1_unfinished_0_through_real_socket_and_compat_wrapper(
) {
    let upstream = FakeUpstream::start(Script::hang()).await;
    let server = LiveServer::start(&upstream.base_url()).await;
    let mut sse = server.open_sse().await;

    let client = server
        .post_messages(&serde_json::json!({
            "model": "claude-sonnet-4",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(client);

    let traces = sse
        .request_traces(Duration::from_secs(2), Duration::from_millis(300))
        .await;
    assert_eq!(traces.len(), 1, "exactly one trace: {traces:?}");
    assert_eq!(traces[0]["data"]["statusCode"], 499, "{traces:?}");

    let ring = server.state.metrics.get_recent_requests(10);
    assert_eq!(ring.len(), 1, "one ring-buffer entry");
    assert_eq!(ring[0].status_code, 499);
    assert_eq!(ring[0].request_id, traces[0]["data"]["requestId"]);

    let metrics = server.metrics().await;
    assert_eq!(metrics["trace_cancelled_total"], 1, "{metrics}");
    assert_eq!(metrics["trace_unfinished_total"], 0, "{metrics}");
}
