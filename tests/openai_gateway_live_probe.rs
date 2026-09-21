//! Live probe: `model_family` resolution (Epic 2/4 of the
//! `openai-model-resolution` project) against a REAL ExampleCorp Model Gateway
//! upstream, through a running daemon (`#[ignore]` — needs VPN/SBN Dev
//! Agent network access).
//!
//! Every other test for the resolution walk (`src/providers/openai/
//! resolution.rs`) and Responses API streaming (`src/providers/openai/
//! responses.rs`) mocks the upstream — this is the one test in the project
//! that hits a real gateway end to end: fetch a real `/v1/models` list,
//! resolve a real family via the actual candidate-ranking/probe walk, and
//! confirm a real, well-formed response comes back and gets cached.
//!
//! This does NOT run in CI and is not required for `cargo test` to pass —
//! per pitfalls.md §5's "silent test skip masquerading as coverage"
//! warning, it exists specifically so that gap doesn't go unnoticed: it
//! compiles and is discoverable via `cargo test -- --ignored`, even though
//! nobody in this sandboxed environment can actually run it (no SBN Dev
//! Agent/VPN access here — same access gap already flagged for Task
//! 1.1.2c and Task 6.1.2a).
//!
//! Cross-referenced from Story 6.2.1's operator docs (TODO once Epic 6.2
//! lands: link this file from wherever `model_family` gets documented).
//!
//! To run manually, with VPN/SBN Dev Agent active:
//! 1. Point a `consolette` daemon's config at a real ExampleCorp Model Gateway
//!    OpenAI-compatible upstream with `model_family` set (see
//!    `references/conf.d/00-providers.toml`'s `model_family` example) under
//!    some route name, e.g. `openai-gateway-family-probe`.
//! 2. Start that daemon.
//! 3. Run:
//!    ```sh
//!    CONSOLETTE_BASE_URL=http://127.0.0.1:47000 \
//!      CONSOLETTE_GATEWAY_FAMILY_MODEL=openai-gateway-family-probe \
//!      cargo test --test openai_gateway_live_probe -- --ignored --nocapture
//!    ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{json, Value};

fn base_url() -> String {
    std::env::var("CONSOLETTE_BASE_URL").unwrap_or("http://127.0.0.1:47000".to_string())
}

/// The route name (not a raw `OpenAI` model id) configured in the daemon with
/// `model_family` set — resolution happens server-side against that route's
/// upstream, not something this test can pass directly.
fn family_route_model() -> String {
    std::env::var("CONSOLETTE_GATEWAY_FAMILY_MODEL")
        .unwrap_or("openai-gateway-family-probe".to_string())
}

fn post(path: &str, body: &Value) -> Value {
    let out = std::process::Command::new("curl")
        .args([
            "-s",
            "--max-time",
            "120",
            "-X",
            "POST",
            &format!("{base}{path}", base = base_url()),
            "-H",
            "Content-Type: application/json",
            "-d",
            &body.to_string(),
        ])
        .output()
        .expect("curl failed");
    serde_json::from_str::<Value>(String::from_utf8_lossy(&out.stdout).as_ref())
        .expect("non-JSON response")
}

/// First request against a `model_family`-configured route: triggers the
/// cold-cache resolution walk (fetch real `/v1/models`, rank candidates,
/// probe) against the real gateway, then a real completion through whatever
/// candidate resolution picked.
#[test]
#[ignore = "needs VPN/SBN Dev Agent access to a real ExampleCorp Model Gateway upstream"]
fn live_model_family_resolves_and_completes_against_real_gateway() {
    let model = family_route_model();
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": "Reply with exactly: HI"}],
        "max_tokens": 64
    });
    let v = post("/v1/messages", &body);

    assert!(
        v.get("error").is_none(),
        "resolution or completion against the real gateway failed: {v}"
    );
    assert_eq!(
        v.get("type"),
        Some(&json!("message")),
        "expected a well-formed Anthropic-shaped message, got: {v}"
    );
    let content = v
        .get("content")
        .and_then(Value::as_array)
        .expect("content must be an array");
    assert!(
        content
            .iter()
            .any(|b| b.get("type").and_then(Value::as_str) == Some("text")),
        "expected at least one text block, got: {content:?}"
    );
}

/// A second request against the same route should hit the warm resolution
/// cache (no repeated `/v1/models` fetch or probe walk) — confirmed
/// indirectly here by simply completing quickly and successfully again;
/// the cache-hit-vs-miss distinction itself is covered by
/// `src/providers/openai/resolution.rs`'s mocked unit tests, not this live
/// probe.
#[test]
#[ignore = "needs VPN/SBN Dev Agent access to a real ExampleCorp Model Gateway upstream"]
fn live_model_family_second_request_hits_warm_cache() {
    let model = family_route_model();
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": "Reply with exactly: HI AGAIN"}],
        "max_tokens": 64
    });
    // Prime the cache.
    let _ = post("/v1/messages", &body);

    let v = post("/v1/messages", &body);
    assert!(
        v.get("error").is_none(),
        "warm-cache request against the real gateway failed: {v}"
    );
    assert_eq!(v.get("type"), Some(&json!("message")));
}
