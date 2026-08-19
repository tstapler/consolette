//! `CostReport` — the single aggregation type shared by the CLI (`cost-report`)
//! and the HTTP surface (`GET /v1/cost/<key>`), Epic 1.3 Story 1.3.3.
//!
//! `report_for_session` (in `tracker.rs`) is a pure read over
//! `SessionCostState::totals_by_tier`: every `CostAmountUsd` here was already
//! computed and folded in at write time (Story 1.3.2's fold step), so this
//! module never touches a pricing table.

use serde::{Deserialize, Serialize};

use crate::cost_metrics::types::{PricingSource, TokenSource};
use crate::session_compaction::tiered::CompactionTier;

/// Per-tier slice of a [`CostReport`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TierBreakdown {
    pub tier: CompactionTier,
    pub counterfactual_tokens: Option<u64>,
    pub compacted_tokens: Option<u64>,
    pub actual_tokens: Option<u64>,
    pub tokens_saved: Option<u64>,
    pub cost_counterfactual_usd: Option<f64>,
    pub cost_actual_usd: Option<f64>,
}

/// One session's cost-accounting summary — the one shared type the CLI and
/// HTTP surfaces both compute from, so they cannot structurally disagree.
///
/// Derives `Deserialize` (Epic 3.1, Task 3.1.1b) so `cost-report`'s
/// `reqwest` client can parse the exact bytes `GET /v1/cost/{session_key}`
/// serializes, rather than re-deriving an equivalent shape independently.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostReport {
    pub session_key: String,
    pub actual_tokens: Option<u64>,
    pub actual_source: Option<TokenSource>,
    pub counterfactual_tokens: Option<u64>,
    pub counterfactual_source: Option<TokenSource>,
    pub compacted_tokens: Option<u64>,
    /// `counterfactual_est - compacted_est`, both sides from the same
    /// `TokenEstimator` call — never `counterfactual - actual` (see plan.md
    /// Epic 1.3, Story 1.3.3, "repair iteration 1 (was arch B4)").
    pub tokens_saved: Option<u64>,
    pub estimated_cost_saved_usd: Option<f64>,
    pub actual_cost_usd: Option<f64>,
    pub pricing_source: PricingSource,
    pub pending_count: usize,
    pub abandoned_count: usize,
    pub by_tier: Vec<TierBreakdown>,
}

/// Errors `report_for_session` can return.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CostReportError {
    /// The session was never seen by `record_pending` (or was evicted) —
    /// distinct from a zeroed report. `report_for_session` uses the store's
    /// non-creating `get`, so a report read never creates session state as a
    /// side effect.
    #[error("session not found")]
    SessionNotFound,
}
