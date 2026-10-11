//! Request -> trace -> catalog end to end through the real server and fake upstreams
//! (Story 5.1.1), plus the Story 1.1.3 live check (Task 1.1.3g): a request to an
//! unreachable upstream produces a trace row with a real 502/503 status, not an always-200.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)] // test crate: a panic is the failure signal

mod support;

use std::time::Duration;

use serde_json::{json, Value};
use support::fake_upstream::{anthropic_message, FakeUpstream, Script, Step};
use support::live_server::{read_response, response_json, LiveServer};
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

const REPLY_WITHIN: Duration = Duration::from_secs(10);

fn haiku_request() -> Value {
    json!({
        "model": "claude-haiku-4-5",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}]
    })
}

/// Sends `body` to `/v1/messages` and returns `(status, parsed JSON body)`.
async fn send(server: &LiveServer, body: &Value) -> (u16, Value) {
    let mut client = server.post_messages(body).await;
    let (status, raw) = read_response(&mut client, REPLY_WITHIN).await;
    (status, response_json(&raw))
}

/// `/metrics` `summary.total_requests`, which is the `requests_total` counter.
async fn requests_total(server: &LiveServer) -> u64 {
    server.metrics().await["summary"]["total_requests"]
        .as_u64()
        .unwrap()
}

fn two_upstream_route(a: &FakeUpstream, b: &FakeUpstream) -> String {
    format!(
        r#"{}{}
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
    )
}

#[tokio::test]
#[allow(non_snake_case)]
async fn fallback_chain_should_emit_one_trace_routed_provider_B_reason_fallback_two_attempts_requests_total_plus_2(
) {
    let a = FakeUpstream::start(Script::sequence([Step::Status(529)])).await;
    let b = FakeUpstream::start(Script::sequence([Step::Ok(anthropic_message())])).await;
    let server = LiveServer::start_config(&two_upstream_route(&a, &b)).await;
    let mut sse = server.open_sse().await;
    let before = requests_total(&server).await;

    let (status, _) = send(&server, &haiku_request()).await;

    assert_eq!(status, 200);
    let traces = sse
        .request_traces(Duration::from_secs(5), Duration::from_millis(300))
        .await;
    assert_eq!(traces.len(), 1, "exactly one trace: {traces:?}");
    let t = &traces[0]["data"];
    assert_eq!(t["statusCode"], 200, "{t}");
    assert_eq!(t["routedProvider"], "B", "{t}");
    assert_eq!(t["routeReason"], "fallback", "{t}");
    let attempts = t["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2, "{attempts:?}");
    assert_eq!(attempts[0]["upstream"], "A");
    assert_eq!(
        attempts[0]["outcome"], "rate_limited",
        "529 maps to rate_limited"
    );
    // The provider folds 529 into its rate-limit error, so the attempt records 429, not 529.
    assert_eq!(attempts[0]["status"], 429);
    assert_eq!(attempts[0]["errorClass"], "rate_limited");
    assert_eq!(
        attempts[0]["errorMessage"],
        "Upstream rate limit (HTTP 429)"
    );
    assert_eq!(attempts[1]["upstream"], "B");
    assert_eq!(attempts[1]["outcome"], "ok");

    let after = requests_total(&server).await;
    assert_eq!(after - before, 2, "requests_total counts per attempt");
    assert_eq!(server.state.metrics.get_recent_requests(10).len(), 1);
}

#[tokio::test]
async fn total_failure_should_emit_one_5xx_trace_with_two_attempts_and_return_error_to_client() {
    let a = FakeUpstream::start(Script::sequence([Step::Status(500)])).await;
    let b = FakeUpstream::start(Script::sequence([Step::Status(500)])).await;
    let server = LiveServer::start_config(&two_upstream_route(&a, &b)).await;
    let mut sse = server.open_sse().await;

    let (status, _) = send(&server, &haiku_request()).await;

    assert!(status >= 500, "client must get the error, got {status}");
    let traces = sse
        .request_traces(Duration::from_secs(5), Duration::from_millis(300))
        .await;
    assert_eq!(traces.len(), 1, "one trace per client request: {traces:?}");
    let t = &traces[0]["data"];
    let trace_status = t["statusCode"].as_u64().unwrap();
    assert!((500..600).contains(&trace_status), "{t}");
    assert_eq!(u64::from(status), trace_status, "client and trace agree");
    assert_eq!(t["attempts"].as_array().unwrap().len(), 2, "{t}");
}

#[tokio::test]
async fn refresh_then_get_should_show_state_ok_with_fake_upstream_ids() {
    let upstream = FakeUpstream::start(Script::models(["claude-fake-a", "claude-fake-b"])).await;
    let server = LiveServer::start(&upstream.base_url()).await;

    let ack = server
        .request_json("POST", "/api/model-catalog/refresh")
        .await;
    assert_eq!(ack["upstreams"]["fake"]["state"], "loading", "{ack}");

    // The refresh runs in the background; poll rather than sleep a fixed time.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let entry = loop {
        let catalog = server.request_json("GET", "/api/model-catalog").await;
        let entry = catalog["upstreams"]["fake"].clone();
        if entry["state"] == "ok" || std::time::Instant::now() > deadline {
            break entry;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    assert_eq!(entry["state"], "ok", "{entry}");
    let ids: Vec<&str> = entry["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["claude-fake-a", "claude-fake-b"]);
}

/// REQ-1: the raw SSE frame carries camelCase keys and an `id`, never snake_case.
#[tokio::test]
async fn sse_frame_should_use_camel_case_keys_and_id_when_request_sent_through_running_server() {
    let upstream = FakeUpstream::start(Script::models(Vec::<String>::new())).await;
    let server = LiveServer::start(&upstream.base_url()).await;
    let mut sse = server.open_sse().await;

    let (status, _) = send(&server, &haiku_request()).await;
    assert_eq!(status, 200);

    let raw = sse
        .collect_until(Duration::from_secs(5), "\"type\":\"RequestTrace\"")
        .await;
    let frame = raw
        .lines()
        .find(|l| l.starts_with("data:") && l.contains("RequestTrace"))
        .unwrap_or_else(|| panic!("no RequestTrace frame in: {raw}"));
    let data: Value = serde_json::from_str(frame.trim_start_matches("data:").trim()).unwrap();
    assert!(data["data"]["requestId"].is_string(), "{frame}");
    assert_eq!(data["data"]["id"], data["data"]["requestId"], "{frame}");
    assert!(!frame.contains("request_id"), "{frame}");
    assert!(!frame.contains("status_code"), "{frame}");
}

/// Minimal stdio MCP server offering `brave_web_search`, so the emulation loop's search
/// backend is real child-process I/O without needing the `stapler-mcp` binary.
const FAKE_SEARCH_BACKEND: &str = r#"#!/usr/bin/env python3
import json, sys

def reply(msg_id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": msg_id, "result": result}) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    msg = json.loads(line)
    method, msg_id = msg.get("method"), msg.get("id")
    if msg_id is None:
        continue
    if method == "initialize":
        reply(msg_id, {"protocolVersion": msg["params"]["protocolVersion"],
                       "capabilities": {"tools": {}},
                       "serverInfo": {"name": "fake-search", "version": "0"}})
    elif method == "tools/list":
        reply(msg_id, {"tools": [{"name": "brave_web_search",
                                  "inputSchema": {"type": "object"}}]})
    elif method == "tools/call":
        hits = {"results": [{"title": "Rust news", "url": "https://example.com/rust",
                             "description": "async closures shipped"}]}
        reply(msg_id, {"content": [{"type": "text", "text": json.dumps(hits)}],
                       "isError": False})
    else:
        reply(msg_id, {})
"#;

fn openai_completion(message: Value, finish_reason: &str) -> Value {
    json!({
        "id": "chatcmpl-fake", "object": "chat.completion", "model": "fake-model",
        "choices": [{"index": 0, "finish_reason": finish_reason, "message": message}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
}

fn search_request(model: &str) -> Value {
    json!({
        "model": model,
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "What is new in Rust?"}],
        "tools": [{"type": "web_search_20250305", "name": "web_search"}]
    })
}

/// Cacheable request unique per run, so a stale row in the on-disk response cache
/// (`~/.claude/consolette/response-cache.sqlite`) cannot turn the first send into a hit.
fn cacheable_request() -> Value {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    json!({
        "model": "claude-sonnet-4",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": format!("cache probe {nonce}")}]
    })
}

#[tokio::test]
async fn four_client_requests_should_emit_exactly_four_traces_with_expected_kinds() {
    use std::os::unix::fs::PermissionsExt;

    let tools = tempfile::tempdir().unwrap();
    let backend = tools.path().join("fake-search-backend");
    std::fs::write(&backend, FAKE_SEARCH_BACKEND).unwrap();
    std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o755)).unwrap();

    // Dispatch 1 asks for a search, dispatch 2 answers; every later dispatch answers plainly.
    let tool_call = openai_completion(
        json!({"role": "assistant", "content": null, "tool_calls": [{
            "id": "call_1", "type": "function",
            "function": {"name": "web_search", "arguments": "{\"query\":\"Rust news\"}"}}]}),
        "tool_calls",
    );
    let answer = openai_completion(json!({"role": "assistant", "content": "answer"}), "stop");
    let upstream = FakeUpstream::start(Script::sequence([
        Step::Ok(tool_call),
        Step::Ok(answer.clone()),
        Step::Ok(answer.clone()),
    ]))
    .await;
    let local = FakeUpstream::start(Script::sequence([Step::Ok(answer)])).await;

    let server = LiveServer::start_config(&format!(
        r#"{}
[server_tools]
backend_path = "{}"

[response_cache]
enabled = true

[[routes]]
name = "default"
strategy = "fallback"

[[routes.upstreams]]
name = "oa"
model = "gpt-5.1"
"#,
        LiveServer::openai_upstream("oa", &upstream.base_url()),
        backend.display()
    ))
    .await;
    {
        use consolette::providers::local_discovery::{Endpoint, LocalBackend};
        server.state.dispatch_router.load().local_catalog().set(
            LocalBackend::Ollama,
            Some(Endpoint {
                base_url: local.base_url(),
                models: vec!["llama3.2".to_string()],
            }),
        );
    }
    let mut sse = server.open_sse().await;

    let cacheable = cacheable_request();
    let requests = [
        search_request("claude-sonnet-4"),
        cacheable.clone(),
        cacheable,
        json!({
            "model": "local/ollama/llama3.2",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hello"}]
        }),
    ];
    for request in &requests {
        let (status, body) = send(&server, request).await;
        assert_eq!(status, 200, "{body}");
    }

    let traces = sse
        .request_traces(Duration::from_secs(5), Duration::from_millis(500))
        .await;
    assert_eq!(traces.len(), 4, "one trace per client request: {traces:?}");
    let data: Vec<&Value> = traces.iter().map(|t| &t["data"]).collect();

    assert_eq!(data[0]["traceKind"], "routed", "{}", data[0]);
    assert_eq!(data[0]["iterations"], 2, "{}", data[0]);
    assert_eq!(
        data[0]["attempts"].as_array().unwrap().len(),
        2,
        "{}",
        data[0]
    );
    assert_eq!(data[1]["traceKind"], "routed", "cache miss: {}", data[1]);
    assert_eq!(data[1]["iterations"], 1, "{}", data[1]);
    assert_eq!(data[2]["traceKind"], "cache_hit", "{}", data[2]);
    assert!(
        data[2]["attempts"].as_array().unwrap().is_empty(),
        "{}",
        data[2]
    );
    assert_eq!(data[3]["traceKind"], "local", "{}", data[3]);
    assert_eq!(data[3]["routedProvider"], "local", "{}", data[3]);
    assert_eq!(server.state.metrics.get_recent_requests(10).len(), 4);
}
