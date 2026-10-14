//! `X-Consolette-Upstream` end to end through the real server and fake upstreams: the header
//! forces one upstream, never falls back, bypasses the cache, and never reaches an upstream.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)] // test crate: a panic is the failure signal

mod support;

use std::time::Duration;

use serde_json::{json, Value};
use support::fake_upstream::{anthropic_message, FakeUpstream, Script, Step};
use support::live_server::{read_response, response_json, LiveServer};

const REPLY_WITHIN: Duration = Duration::from_secs(10);
const OVERRIDE: &str = "X-Consolette-Upstream";

fn request(text: &str) -> Value {
    json!({
        "model": "claude-haiku-4-5",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": text}]
    })
}

/// A nonce keeps the on-disk cache and other tests from turning a send into a hit.
fn unique_request() -> Value {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    request(&format!("override probe {nonce}"))
}

async fn post(
    server: &LiveServer,
    path: &str,
    headers: &[(&str, &str)],
    body: &Value,
) -> (u16, Value) {
    let mut client = server.post_with_headers(path, headers, body).await;
    let (status, raw) = read_response(&mut client, REPLY_WITHIN).await;
    (status, response_json(&raw))
}

fn ok_upstream() -> Script {
    Script::sequence([Step::Ok(anthropic_message())])
}

/// Route `default` = [A, B]; `C` is configured but not routed.
fn route_ab_plus_c(a: &FakeUpstream, b: &FakeUpstream, c: &FakeUpstream, extra: &str) -> String {
    format!(
        r#"{}{}{}{extra}
[[routes]]
name = "default"
strategy = "fallback"

[[routes.upstreams]]
name = "A"

[[routes.upstreams]]
name = "B"
"#,
        LiveServer::anthropic_upstream("A", &a.base_url()),
        LiveServer::anthropic_upstream("B", &b.base_url()),
        LiveServer::anthropic_upstream("C", &c.base_url()),
    )
}

async fn three(script_a: Script, script_b: Script) -> (FakeUpstream, FakeUpstream, FakeUpstream) {
    (
        FakeUpstream::start(script_a).await,
        FakeUpstream::start(script_b).await,
        FakeUpstream::start(ok_upstream()).await,
    )
}

#[tokio::test]
async fn forced_upstream_should_be_used_even_when_the_first_route_entry_would_succeed() {
    let (a, b, c) = three(ok_upstream(), ok_upstream()).await;
    let server = LiveServer::start_config(&route_ab_plus_c(&a, &b, &c, "")).await;
    let mut sse = server.open_sse().await;

    let (status, _) = post(
        &server,
        "/v1/messages",
        &[(OVERRIDE, "B")],
        &request("hello"),
    )
    .await;

    assert_eq!(status, 200);
    assert_eq!(a.received_bodies().len(), 0, "A must not be tried");
    assert_eq!(b.received_bodies().len(), 1);
    let traces = sse
        .request_traces(Duration::from_secs(5), Duration::from_millis(300))
        .await;
    assert_eq!(traces.len(), 1, "{traces:?}");
    let t = &traces[0]["data"];
    assert_eq!(t["routedProvider"], "B", "{t}");
    assert_eq!(t["routeReason"], "override", "{t}");
    assert_eq!(t["attempts"].as_array().unwrap().len(), 1, "{t}");
}

#[tokio::test]
async fn forced_upstream_outside_the_active_route_should_still_be_used() {
    let (a, b, c) = three(ok_upstream(), ok_upstream()).await;
    let server = LiveServer::start_config(&route_ab_plus_c(&a, &b, &c, "")).await;

    let (status, _) = post(
        &server,
        "/v1/messages",
        &[(OVERRIDE, "C")],
        &request("hello"),
    )
    .await;

    assert_eq!(status, 200);
    assert_eq!(c.received_bodies().len(), 1);
    assert_eq!(a.received_bodies().len() + b.received_bodies().len(), 0);
}

#[tokio::test]
async fn forced_upstream_failure_should_be_returned_without_falling_back() {
    let (a, b, c) = three(ok_upstream(), Script::sequence([Step::Status(500)])).await;
    let server = LiveServer::start_config(&route_ab_plus_c(&a, &b, &c, "")).await;
    let mut sse = server.open_sse().await;

    let (status, _) = post(
        &server,
        "/v1/messages",
        &[(OVERRIDE, "B")],
        &request("hello"),
    )
    .await;

    assert!(
        status >= 500,
        "forced failure must reach the client: {status}"
    );
    assert!(!b.received_bodies().is_empty());
    assert_eq!(a.received_bodies().len(), 0, "no fallback to A");
    let traces = sse
        .request_traces(Duration::from_secs(5), Duration::from_millis(300))
        .await;
    let t = &traces[0]["data"];
    assert_eq!(t["routeReason"], "override", "{t}");
    assert_eq!(t["routedProvider"], "B", "{t}");
    assert!(t["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .all(|attempt| attempt["upstream"] == "B"));
}

#[tokio::test]
async fn unknown_upstream_should_be_a_400_listing_configured_names_and_a_400_trace() {
    let (a, b, c) = three(ok_upstream(), ok_upstream()).await;
    let server = LiveServer::start_config(&route_ab_plus_c(&a, &b, &c, "")).await;
    let mut sse = server.open_sse().await;

    for path in ["/v1/messages", "/v1/chat/completions"] {
        let (status, body) = post(&server, path, &[(OVERRIDE, "nope")], &request("hi")).await;
        assert_eq!(status, 400, "{path}: {body}");
        let text = body.to_string();
        assert!(text.contains("A, B, C"), "{path} lists names: {text}");
        assert!(!text.contains("sk-test"), "names only: {text}");
    }
    let traces = sse
        .request_traces(Duration::from_secs(5), Duration::from_millis(300))
        .await;
    assert_eq!(traces.len(), 2, "{traces:?}");
    assert!(traces.iter().all(|t| t["data"]["statusCode"] == 400));
    let total: usize = [&a, &b, &c].iter().map(|u| u.received_bodies().len()).sum();
    assert_eq!(total, 0, "nothing may reach an upstream");
}

#[tokio::test]
async fn empty_override_header_should_be_a_400() {
    let (a, b, c) = three(ok_upstream(), ok_upstream()).await;
    let server = LiveServer::start_config(&route_ab_plus_c(&a, &b, &c, "")).await;

    let (status, body) = post(&server, "/v1/messages", &[(OVERRIDE, " ")], &request("hi")).await;

    assert_eq!(status, 400, "{body}");
    assert_eq!(a.received_bodies().len(), 0);
}

#[tokio::test]
async fn absent_header_should_route_as_before() {
    let (a, b, c) = three(ok_upstream(), ok_upstream()).await;
    let server = LiveServer::start_config(&route_ab_plus_c(&a, &b, &c, "")).await;
    let mut sse = server.open_sse().await;

    let (status, _) = post(&server, "/v1/messages", &[], &request("hello")).await;

    assert_eq!(status, 200);
    assert_eq!(a.received_bodies().len(), 1);
    let traces = sse
        .request_traces(Duration::from_secs(5), Duration::from_millis(300))
        .await;
    assert_eq!(traces[0]["data"]["routeReason"], "default");
}

#[tokio::test]
async fn forced_request_should_neither_read_nor_write_the_response_cache() {
    let (a, b, c) = three(ok_upstream(), ok_upstream()).await;
    let server = LiveServer::start_config(&route_ab_plus_c(
        &a,
        &b,
        &c,
        "\n[response_cache]\nenabled = true\n",
    ))
    .await;
    let body = unique_request();
    let forced = [(OVERRIDE, "A")];
    let hits = || a.received_bodies().len();

    post(&server, "/v1/messages", &forced, &body).await;
    assert_eq!(hits(), 1);
    // The forced answer was not stored, so the first plain send misses...
    post(&server, "/v1/messages", &[], &body).await;
    assert_eq!(hits(), 2, "forced send must not write the cache");
    // ...and the second plain send hits, proving the cache is live.
    post(&server, "/v1/messages", &[], &body).await;
    assert_eq!(hits(), 2, "cache must be active for plain sends");
    // A cached answer exists now, but a forced send must not read it.
    post(&server, "/v1/messages", &forced, &body).await;
    assert_eq!(hits(), 3, "forced send must not read the cache");
}

fn openai_completion() -> Value {
    json!({
        "id": "chatcmpl-fake", "object": "chat.completion", "model": "fake-model",
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": "ok"}}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
}

// Provider header allowlists already drop it, so this guards the wire only; the strip itself
// is covered by the `upstream_override` unit tests.
#[tokio::test]
async fn consolette_headers_should_never_reach_an_anthropic_or_openai_upstream() {
    let a = FakeUpstream::start(ok_upstream()).await;
    let oa = FakeUpstream::start(Script::sequence([Step::Ok(openai_completion())])).await;
    let server = LiveServer::start_config(&format!(
        r#"{}{}
[[routes]]
name = "default"
strategy = "fallback"

[[routes.upstreams]]
name = "A"
"#,
        LiveServer::anthropic_upstream("A", &a.base_url()),
        LiveServer::openai_upstream("OA", &oa.base_url()),
    ))
    .await;
    let headers = [
        (OVERRIDE, "A"),
        ("X-Consolette-Cache-Bypass", "true"),
        ("X-Consolette-Debug", "1"),
    ];

    for (upstream, path) in [
        ("A", "/v1/messages"),
        ("A", "/v1/chat/completions"),
        ("OA", "/v1/messages"),
        ("OA", "/v1/chat/completions"),
    ] {
        let mut h = headers;
        h[0] = (OVERRIDE, upstream);
        let (status, body) = post(&server, path, &h, &unique_request()).await;
        assert_eq!(status, 200, "{upstream} {path}: {body}");
    }

    for fake in [&a, &oa] {
        let received = fake.received_headers();
        assert_eq!(received.len(), 2, "each fake is forced twice");
        for recorded in received {
            for name in ["x-consolette-upstream", "x-consolette-cache-bypass"] {
                assert!(!recorded.has(name), "{name} leaked: {recorded:?}");
            }
            assert!(!format!("{recorded:?}")
                .to_lowercase()
                .contains("x-consolette"));
        }
    }
}
