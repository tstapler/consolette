//! Live probes: the Claude-Code contract checked against REAL pinned models
//! through a running daemon (`#[ignore]` — needs network + quota).
//!
//! Run: `CONSOLETTE_BASE_URL=http://127.0.0.1:47000 \
//!         CONSOLETTE_PROBE_MODELS=cohere/north-mini-code:free,poolside/laguna-s-2.1:free \
//!         cargo test --test claude_live_probe -- --ignored --nocapture`
//!
//! Unlike `claude_compat.rs` (mock conformance, hard asserts), live probes
//! distinguish proxy bugs from model behavior: the proxy contract is
//! asserted hard (well-formed shapes, stop/blocks consistency); model
//! behavior (did it actually call the tool?) is REPORTED, not asserted.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{json, Value};

fn base_url() -> String {
    std::env::var("CONSOLETTE_BASE_URL").unwrap_or("http://127.0.0.1:47000".to_string())
}

fn probe_models() -> Vec<String> {
    std::env::var("CONSOLETTE_PROBE_MODELS")
        .unwrap_or("cohere/north-mini-code:free".to_string())
        .split(',')
        .map(str::to_string)
        .collect()
}

fn get_time_tool() -> Value {
    json!([{
        "name": "get_time",
        "description": "Return the current UTC time. Call this when asked for the time.",
        "input_schema": {"type": "object", "properties": {}, "additionalProperties": false}
    }])
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

/// Proxy contract: well-formed message + stop/blocks consistency.
/// Returns a one-line verdict for the report.
fn check_message(model: &str, label: &str, v: &Value) -> String {
    let fail = |why: &str| format!("PROXY-FAIL {model} {label}: {why}");
    if v.get("type") != Some(&json!("message")) {
        return fail(&format!("not a message: {v}"));
    }
    if v.get("error").is_some() {
        return format!("UPSTREAM-ERROR {model} {label}: {}", v["error"]);
    }
    let Some(content) = v.get("content").and_then(Value::as_array) else {
        return fail("content not an array");
    };
    let mut saw_text = false;
    let mut saw_tool = false;
    for b in content {
        match b.get("type").and_then(Value::as_str) {
            Some("text") if b["text"].is_string() => saw_text = true,
            Some("tool_use")
                if b["id"].is_string() && b["name"].is_string() && b["input"].is_object() =>
            {
                saw_tool = true;
            }
            Some("thinking") => {}
            other => return fail(&format!("bad block: {b:?} ({other:?})")),
        }
    }
    let stop = v["stop_reason"].as_str().unwrap_or("<missing>");
    if stop == "tool_use" && !saw_tool {
        return fail("stop=tool_use with zero tool_use blocks");
    }
    if saw_tool && stop != "tool_use" {
        return fail(&format!("tool_use blocks with stop={stop}"));
    }
    let text_len: usize = content
        .iter()
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .map(str::len)
        .sum();
    format!(
        "OK {model} {label}: stop={stop} text_len={text_len} tool={saw_tool} saw_text={saw_text}"
    )
}

#[test]
#[ignore = "needs live daemon + quota"]
fn live_text_turn_returns_nonempty_text() {
    for model in probe_models() {
        let body = json!({
            "model": model,
            "messages": [{"role": "user", "content": "Reply with exactly: HI"}],
            "max_tokens": 64
        });
        let v = post("/v1/messages", &body);
        let verdict = check_message(&model, "text-turn", &v);
        println!("{verdict}");
        assert!(!verdict.starts_with("PROXY-FAIL"), "{verdict}");
    }
}

#[test]
#[ignore = "needs live daemon + quota"]
fn live_tool_turn_contract_holds() {
    // Model may answer in text instead of calling (reported, not failed);
    // the proxy must never emit stop=tool_use without blocks.
    for model in probe_models() {
        let body = json!({
            "model": model,
            "messages": [{"role": "user", "content": "What time is it? Use get_time."}],
            "max_tokens": 256,
            "tools": get_time_tool(),
            "tool_choice": {"type": "auto"}
        });
        let v = post("/v1/messages", &body);
        let verdict = check_message(&model, "tool-turn", &v);
        println!("{verdict}\n  content: {}", v["content"]);
        assert!(!verdict.starts_with("PROXY-FAIL"), "{verdict}");
    }
}

#[test]
#[ignore = "needs live daemon + quota"]
fn live_tool_round_trip_keeps_loop_alive() {
    // If the model called the tool, answer with a tool_result and confirm
    // the second turn is still well-formed (multi-turn continuity).
    for model in probe_models() {
        let first = post(
            "/v1/messages",
            &json!({
                "model": model,
                "messages": [{"role": "user", "content": "What time is it? Use get_time."}],
                "max_tokens": 256,
                "tools": get_time_tool(),
                "tool_choice": {"type": "any"}
            }),
        );
        println!("first: {}", check_message(&model, "round1", &first));
        let call = first["content"]
            .as_array()
            .and_then(|c| c.iter().find(|b| b["type"] == "tool_use"));
        let Some(call) = call else {
            println!("SKIP {model} round2: model did not call (no proxy fault)");
            continue;
        };
        let second = post(
            "/v1/messages",
            &json!({
                "model": model,
                "messages": [
                    {"role": "user", "content": "What time is it? Use get_time."},
                    {"role": "assistant", "content": first["content"]},
                    {"role": "user", "content": [{
                        "type": "tool_result",
                        "tool_use_id": call["id"],
                        "content": "12:00 UTC"
                    }]}
                ],
                "max_tokens": 256,
                "tools": get_time_tool()
            }),
        );
        let verdict = check_message(&model, "round2", &second);
        println!("{verdict}\n  content: {}", second["content"]);
        assert!(!verdict.starts_with("PROXY-FAIL"), "{verdict}");
    }
}
