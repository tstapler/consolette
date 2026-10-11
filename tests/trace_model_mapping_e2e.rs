//! Live check for Story 1.1.3 (Task 1.1.3g): a request to an unreachable
//! upstream produces a trace row with a real 502/503 status, not an always-200.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)] // test crate: a panic is the failure signal

mod support;

use std::time::Duration;

use support::fake_upstream::{FakeUpstream, Script};
use support::live_server::LiveServer;
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn unreachable_upstream_should_show_502_or_503_row() {
    // Bind then drop: the port is OS-assigned and nothing listens on it.
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);

    let server = LiveServer::start(&url).await;
    let mut sse = server.open_sse().await;
    let _client = server
        .post_messages(&serde_json::json!({
            "model": "claude-sonnet-4",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .await;

    let traces = sse
        .request_traces(Duration::from_secs(5), Duration::from_millis(300))
        .await;
    assert_eq!(traces.len(), 1, "exactly one trace: {traces:?}");
    let status = traces[0]["data"]["statusCode"].as_u64().unwrap();
    assert!(matches!(status, 502 | 503), "status {status}: {traces:?}");

    let ring = server.state.metrics.get_recent_requests(10);
    assert_eq!(ring.len(), 1);
    assert_eq!(u64::from(ring[0].status_code), status);
}

/// REQ-9: "SSE push < 100 ms", with a wide tolerance because this runs over real sockets.
#[tokio::test]
async fn sse_trace_should_arrive_within_100ms_of_response_completion_on_localhost() {
    let upstream = FakeUpstream::start(Script::models(Vec::<String>::new())).await;
    let server = LiveServer::start(&upstream.base_url()).await;
    let mut sse = server.open_sse().await;
    let mut client = server
        .post_messages(&serde_json::json!({
            "model": "claude-sonnet-4",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .await;
    let mut response = [0u8; 4096];
    let read = tokio::time::timeout(Duration::from_secs(5), client.read(&mut response)).await;
    assert!(matches!(read, Ok(Ok(n)) if n > 0), "response: {read:?}");

    let traces = sse
        .request_traces(Duration::from_millis(100), Duration::from_millis(1))
        .await;

    assert_eq!(traces.len(), 1, "{traces:?}");
    assert_eq!(traces[0]["data"]["statusCode"], 200);
    assert_eq!(traces[0]["data"]["requestedModel"], "claude-sonnet-4");
    assert_eq!(traces[0]["data"]["routedProvider"], "fake");
    assert_eq!(traces[0]["data"]["attempts"][0]["outcome"], "ok");
}
