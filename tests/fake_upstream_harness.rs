#![allow(clippy::unwrap_used, clippy::expect_used)] // test crate: a panic is the failure signal

mod support;

use std::time::{Duration, Instant};
use support::fake_upstream::{anthropic_message, FakeUpstream, Script, Step};

fn post(url: &str, body: serde_json::Value) -> reqwest::RequestBuilder {
    reqwest::Client::new()
        .post(format!("{url}/v1/messages"))
        .json(&body)
}

#[tokio::test]
async fn sequence_should_serve_529_then_ok() {
    let up = FakeUpstream::start(Script::sequence([
        Step::Status(529),
        Step::Ok(anthropic_message()),
    ]))
    .await;
    let first = post(&up.base_url(), serde_json::json!({"n":1}))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status().as_u16(), 529);
    let second = post(&up.base_url(), serde_json::json!({"n":2}))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status().as_u16(), 200);
    let v: serde_json::Value = second.json().await.unwrap();
    assert_eq!(v["type"], "message");
    assert_eq!(v["role"], "assistant");
}

#[tokio::test]
async fn sequence_should_repeat_last_step_when_exhausted() {
    let up = FakeUpstream::start(Script::sequence([Step::Status(429)])).await;
    for _ in 0..3 {
        let r = post(&up.base_url(), serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 429);
    }
}

#[tokio::test]
async fn base_url_should_use_os_assigned_loopback_port() {
    let a = FakeUpstream::start(Script::sequence([Step::Ok(anthropic_message())])).await;
    let b = FakeUpstream::start(Script::sequence([Step::Ok(anthropic_message())])).await;
    assert!(a.base_url().starts_with("http://127.0.0.1:"));
    assert_ne!(a.base_url(), b.base_url());
}

#[tokio::test]
async fn models_should_serve_listed_ids_on_v1_models() {
    let up = FakeUpstream::start(Script::models(["m1", "m2"])).await;
    let v: serde_json::Value = reqwest::get(format!("{}/v1/models", up.base_url()))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["m1", "m2"]);
}

#[tokio::test]
async fn hang_should_never_send_headers() {
    let up = FakeUpstream::start(Script::hang()).await;
    let res = tokio::time::timeout(
        Duration::from_millis(300),
        post(&up.base_url(), serde_json::json!({})).send(),
    )
    .await;
    assert!(res.is_err(), "hang() must not respond");
}

#[tokio::test]
async fn hang_after_headers_should_send_headers_but_no_body_bytes() {
    let up = FakeUpstream::start(Script::hang_after_headers()).await;
    let resp = tokio::time::timeout(
        Duration::from_secs(2),
        post(&up.base_url(), serde_json::json!({})).send(),
    )
    .await
    .expect("headers should arrive")
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = tokio::time::timeout(Duration::from_millis(300), resp.bytes()).await;
    assert!(body.is_err(), "no body bytes may arrive");
}

#[tokio::test]
async fn delay_headers_should_hold_headers_for_at_least_the_delay() {
    let up = FakeUpstream::start(
        Script::delay_headers(Duration::from_millis(150)).then(Step::Ok(anthropic_message())),
    )
    .await;
    let start = Instant::now();
    let resp = post(&up.base_url(), serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert!(start.elapsed() >= Duration::from_millis(150));
    assert_eq!(resp.status().as_u16(), 200);
}

#[tokio::test]
async fn status_with_retry_after_should_carry_header() {
    let up = FakeUpstream::start(Script::status_with_retry_after(429, 3)).await;
    let resp = post(&up.base_url(), serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 429);
    assert_eq!(resp.headers()["retry-after"].to_str().unwrap(), "3");
}

#[tokio::test]
async fn stream_chunks_should_emit_sse_frames_with_inter_chunk_delay() {
    let up = FakeUpstream::start(Script::stream_chunks(
        ["a", "b"],
        Duration::from_millis(100),
    ))
    .await;
    let start = Instant::now();
    let resp = post(&up.base_url(), serde_json::json!({"stream":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.headers()["content-type"].to_str().unwrap(),
        "text/event-stream"
    );
    let text = resp.text().await.unwrap();
    assert!(start.elapsed() >= Duration::from_millis(100));
    let events: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix("event: "))
        .collect();
    assert_eq!(events.first(), Some(&"message_start"));
    assert_eq!(events.last(), Some(&"message_stop"));
    assert_eq!(
        events
            .iter()
            .filter(|e| **e == "content_block_delta")
            .count(),
        2
    );
    assert!(text.contains(r#""text":"a""#) && text.contains(r#""text":"b""#));
}

#[tokio::test]
async fn recorder_should_return_bodies_and_headers_in_arrival_order() {
    let up = FakeUpstream::start(Script::sequence([Step::Ok(anthropic_message())])).await;
    let c = reqwest::Client::new();
    for n in 1..=2 {
        c.post(format!("{}/v1/messages", up.base_url()))
            .header("x-probe", format!("p{n}"))
            .json(&serde_json::json!({"n": n}))
            .send()
            .await
            .unwrap();
    }
    let bodies = up.received_bodies();
    assert_eq!(
        bodies,
        vec![serde_json::json!({"n":1}), serde_json::json!({"n":2})]
    );
    let headers = up.received_headers();
    assert_eq!(headers.len(), 2);
    assert_eq!(headers[0].get("x-probe"), "p1");
    assert_eq!(headers[1].get("x-probe"), "p2");
}

#[tokio::test]
async fn recorder_should_record_requests_that_never_get_a_response() {
    let up = FakeUpstream::start(Script::hang()).await;
    let _ = tokio::time::timeout(
        Duration::from_millis(200),
        post(&up.base_url(), serde_json::json!({"k":"v"})).send(),
    )
    .await;
    assert_eq!(up.received_bodies(), vec![serde_json::json!({"k":"v"})]);
}

#[tokio::test]
#[ignore = "manual: prints a base URL and serves until killed"]
async fn fake_upstream_serve() {
    let up = FakeUpstream::start(Script::sequence([Step::Ok(anthropic_message())])).await;
    println!("fake upstream listening at {}", up.base_url());
    std::future::pending::<()>().await;
}
