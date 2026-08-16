//! Per-session actual-vs-counterfactual token/cost accounting for
//! [`crate::session_compaction::SessionCompactionPipeline`].
//!
//! See `project_plans/compaction-cost-metrics/implementation/plan.md` for
//! the full design. This module is being built up epic-by-epic; Epic 1.1
//! introduces only the shared vocabulary in [`types`] — no behavior yet.

pub mod cli_format;
pub mod client;
pub mod estimator;
pub mod hook;
pub mod pricing;
pub mod report;
pub mod server;
pub mod store;
pub mod test_support;
pub mod tracker;
pub mod types;

use crate::cost_metrics::tracker::{CostTracker, CostTrackerError};
use crate::cost_metrics::types::{RequestId, TokenCount, TokenSource};
use crate::session_compaction::SessionKey;

/// Extract real `usage.*` token counts from an Anthropic Messages API
/// response and record them against `(session_key, request_id)` via
/// [`CostTracker::record_actual_usage`], combined into one billed-token
/// figure (`input_tokens + output_tokens`) with [`TokenSource::Exact`]
/// (plan.md Epic 2.2, Story 2.2.1).
///
/// `model` is accepted for signature parity with plan.md's Task 2.2.1b —
/// `CostTracker::record_actual_usage` itself has no `model` parameter
/// (a record's `model` is set once, at `record_pending` time), so this
/// wrapper does not currently use it.
///
/// Returns `None`, without touching `tracker`, when `anthropic_response` has
/// no parseable `usage` object (see `crate::providers::extract_usage`).
/// Otherwise returns `Some` of whatever `record_actual_usage` returns —
/// `Err(CostTrackerError::SessionNotFound)` only when the *session* itself
/// was never seen or was evicted, never as a stand-in for a missing `usage`
/// field.
///
/// **Reachability (Story 2.2.1)**: this function is unit-tested directly in
/// `src/providers/mod.rs`. It has no caller in this codebase's live request
/// path today — see `crate::providers::translate_and_record`'s doc comment
/// and plan.md Epic 2.2's reachability statement.
pub async fn record_actual_usage_from_anthropic_response(
    tracker: &CostTracker,
    session_key: &SessionKey,
    request_id: RequestId,
    _model: &str,
    anthropic_response: &serde_json::Value,
) -> Option<Result<(), CostTrackerError>> {
    let (input_tokens, output_tokens) = crate::providers::extract_usage(anthropic_response)?;
    let actual = TokenCount {
        value: input_tokens + output_tokens,
        source: TokenSource::Exact,
    };
    Some(
        tracker
            .record_actual_usage(session_key, request_id, actual)
            .await,
    )
}
