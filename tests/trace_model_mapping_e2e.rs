//! Live check for Story 1.1.3 (Task 1.1.3g): a request to an unreachable
//! upstream produces a trace row with a real 502/503 status, not an always-200.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)] // test crate: a panic is the failure signal

mod support;

use std::time::Duration;

use support::live_server::LiveServer;

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
