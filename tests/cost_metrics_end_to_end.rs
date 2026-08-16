//! Epic 4.1, Story 4.1.1: end-to-end proof of requirements.md's stated
//! success metric — a `Full`-tier session's `CostReport` shows materially
//! more token savings (and materially lower actual/billed tokens) than an
//! `Off`-tier session driven over the identical growing message history.
//!
//! Unlike the unit tests scattered across `src/session_compaction/mod.rs`
//! and `src/cost_metrics/{tracker,hook}.rs`, this test wires the real
//! pieces together end to end: `SessionCompactionPipeline::apply` (real
//! tiering/budgeting/summarization), `CostTrackingHook<TiktokenEstimator>`
//! (real, synchronous, network-free tokenizer — no mock estimator), and
//! `CostTracker::report_for_session` (the same read path the CLI/HTTP
//! surfaces use). Only the provider's `usage.*` response is faked, since a
//! real Anthropic round trip is out of scope for this test.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_precision_loss
)]

use std::sync::Arc;
use std::time::Duration;

use consolette::cost_metrics::estimator::TiktokenEstimator;
use consolette::cost_metrics::hook::CostTrackingHook;
use consolette::cost_metrics::pricing::PricingTable;
use consolette::cost_metrics::record_actual_usage_from_anthropic_response;
use consolette::cost_metrics::report::CostReport;
use consolette::cost_metrics::tracker::CostTracker;
use consolette::session_compaction::{SessionCompactionPipeline, SessionKey, TierThresholds};
use serde_json::{json, Value};

const MODEL: &str = "claude-sonnet-5";

/// Task 4.1.1a: a 20-turn message history of escalating size, alternating
/// `user`/`assistant` roles (so `ConversationSummarizer`'s turn-based
/// keep-window has turns to drop) and carrying a growing `tool_result`
/// block per turn (so `ToolResultBudget`'s age-based eliding has something
/// to elide). No equivalent generator exists in
/// `src/session_compaction/mod.rs`'s test module — its fixtures are either
/// uniform-size `tool_result` runs or a single small plan-reinjection case
/// — so this is a new fixture built for this test's specific "escalating
/// size" requirement.
fn escalating_messages(n: usize) -> Vec<Value> {
    (0..n)
        .map(|i| {
            // Escalating filler: turn 0 is small, turn 19 is >10x larger,
            // so the cumulative history driven turn-by-turn below grows
            // superlinearly, giving `Full` tier's budgeting/summarization
            // an increasingly large gap to close against `Off`.
            let filler = "lorem ipsum dolor sit amet ".repeat(20 + i * 15);
            json!({
                "role": if i % 2 == 0 { "user" } else { "assistant" },
                "content": [
                    {"type": "text", "text": format!("turn {i} notes: {filler}")},
                    {
                        "type": "tool_result",
                        "tool_use_id": format!("tool-{i}"),
                        "content": filler,
                    },
                ]
            })
        })
        .collect()
}

/// Deterministic fake `usage.input_tokens`/`usage.output_tokens`,
/// proportional to the size of the message payload actually sent for this
/// turn (`out` — `apply()`'s possibly-compacted result), standing in for a
/// real provider round trip. `output_tokens` is a small constant (a
/// generated reply is roughly turn-independent in size here) — only the
/// input side should track the compacted-vs-uncompacted history size.
fn fake_usage_for(out: &Value) -> (u64, u64) {
    let approx_input_tokens = (serde_json::to_string(out).unwrap().len() as u64 / 4).max(1);
    (approx_input_tokens, 25)
}

/// Tasks 4.1.1b/c: drive `apply()` + `record_actual_usage` for all 20 turns
/// at a fixed `pressure_pct` (so every turn resolves to the same tier),
/// wiring a real `CostTrackingHook<TiktokenEstimator>` so both the
/// counterfactual/compacted estimates and the reconciled actual figures
/// come from genuine (if synthetic-usage) accounting rather than mocks.
async fn run_session(session_name: &str, pressure_pct: f32, all_messages: &[Value]) -> CostReport {
    let tracker = Arc::new(CostTracker::new(PricingTable::new()).await);
    let estimator = Arc::new(TiktokenEstimator::new());
    let hook = Arc::new(CostTrackingHook::new(tracker.clone(), estimator, MODEL));

    let mut pipeline = SessionCompactionPipeline::new(TierThresholds::default()).await;
    pipeline.register_hook(hook);

    let key = SessionKey::new(session_name);

    for turn in 1..=all_messages.len() {
        let history = Value::Array(all_messages[..turn].to_vec());
        let (out, report) = pipeline.apply(&key, &history, pressure_pct).await;

        let (input_tokens, output_tokens) = fake_usage_for(&out);
        let anthropic_response = json!({
            "usage": {
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
            }
        });
        record_actual_usage_from_anthropic_response(
            &tracker,
            &key,
            report.request_id,
            MODEL,
            &anthropic_response,
        )
        .await
        .expect("usage object is well-formed, so extract_usage must succeed")
        .expect("session is known (record_pending already ran inside apply())");
    }

    // Task 4.1.1d groundwork: the counterfactual/compacted estimates are
    // filled in by a `tokio::spawn`ed task inside `CostTrackingHook`
    // (Epic 2.1's deliberate hot-path/estimator-latency split), so bound-poll
    // for full reconciliation instead of asserting immediately or sleeping a
    // fixed duration — mirrors the poll loop in
    // `src/cost_metrics/hook.rs`'s own reconciliation tests.
    let mut report = tracker
        .report_for_session(&key)
        .await
        .expect("session was recorded via record_pending inside apply()");
    for _ in 0..200 {
        if report.pending_count == 0 && report.abandoned_count == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        report = tracker
            .report_for_session(&key)
            .await
            .expect("session was recorded via record_pending inside apply()");
    }

    assert_eq!(
        report.pending_count, 0,
        "session {session_name} did not fully reconcile within the poll budget: {report:#?}"
    );
    assert_eq!(
        report.abandoned_count, 0,
        "session {session_name} had abandoned records (estimator failure?): {report:#?}"
    );

    report
}

/// Story 4.1.1: proves requirements.md's success metric end to end —
/// `Full` tier saves a material fraction of counterfactual tokens over 20
/// escalating turns, `Off` tier saves exactly nothing (by construction:
/// `out == messages` at `Off`), and the two same-estimator-based
/// `tokens_saved` figures agree in direction with the independently-derived
/// "actual" (fake-`usage.*`) billed-token figures.
#[tokio::test]
async fn full_tier_saves_materially_more_than_off_tier_across_twenty_escalating_turns() {
    let all_messages = escalating_messages(20);

    let session_full = run_session("e2e-full", 0.95, &all_messages).await;
    let session_off = run_session("e2e-off", 0.10, &all_messages).await;

    assert_eq!(
        session_off.tokens_saved,
        Some(0),
        "Off tier is a no-op (out == messages by construction), so tokens_saved must be exactly \
         zero.\nsession_full={session_full:#?}\nsession_off={session_off:#?}"
    );

    let full_tokens_saved = session_full
        .tokens_saved
        .expect("Full session reconciled at least one record, so tokens_saved must be Some");
    let full_counterfactual = session_full.counterfactual_tokens.expect(
        "Full session reconciled at least one record, so counterfactual_tokens must be Some",
    );
    assert!(
        full_tokens_saved as f64 >= full_counterfactual as f64 * 0.3,
        "Full session's tokens_saved ({full_tokens_saved}) is not at least 30% of its \
         counterfactual_tokens ({full_counterfactual}).\nsession_full={session_full:#?}\n\
         session_off={session_off:#?}"
    );

    let full_actual = session_full
        .actual_tokens
        .expect("Full session reconciled at least one record, so actual_tokens must be Some");
    let off_actual = session_off
        .actual_tokens
        .expect("Off session reconciled at least one record, so actual_tokens must be Some");
    assert!(
        full_actual < off_actual,
        "Full session's actual (billed) tokens ({full_actual}) should be materially lower than \
         Off's ({off_actual}) — same-estimator tokens_saved and real billed tokens must agree in \
         direction.\nsession_full={session_full:#?}\nsession_off={session_off:#?}"
    );
}
