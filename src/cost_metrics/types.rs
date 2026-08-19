//! Domain vocabulary for `cost_metrics` (Epic 1.1).
//!
//! Every type here comes straight from the Domain Glossary in
//! `project_plans/compaction-cost-metrics/implementation/plan.md`. This
//! module defines only the shared vocabulary — no logic, no I/O — so the
//! rest of the feature (estimators, tracker, pricing) has one consistent
//! set of newtypes/sum types instead of ad hoc `u64`/`bool`/`String` fields
//! invented per-file.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unique identifier for one `SessionCompactionPipeline::apply()` invocation
/// / one upstream provider round trip.
///
/// `Default` is deliberately `Uuid::nil()`, not a fresh random id, so that
/// `CompactionReport::default() == CompactionReport::default()` stays true
/// once `CompactionReport` gains a `request_id: RequestId` field (Epic 2.1).
/// Real ids are always constructed via [`RequestId::new`], never via
/// `Default`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestId(pub Uuid);

impl RequestId {
    /// Construct a fresh, real request id (`Uuid::new_v4()`-backed).
    #[must_use]
    pub fn new() -> Self {
        RequestId(Uuid::new_v4())
    }
}

impl Default for RequestId {
    fn default() -> Self {
        RequestId(Uuid::nil())
    }
}

/// A count of LLM tokens, always paired with its [`TokenSource`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCount {
    pub value: u64,
    pub source: TokenSource,
}

/// Where a [`TokenCount`] came from: a real provider `usage.*` field, or an
/// estimator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TokenSource {
    /// From real provider `usage.*`.
    Exact,
    /// From a `TokenEstimator` implementor.
    Estimated { via: EstimatorKind },
}

/// Which estimator produced an `Estimated` `TokenCount`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EstimatorKind {
    TiktokenCl100k,
    TiktokenO200k,
    AnthropicCountTokensApi,
}

/// A dollar amount. Callers use `Option<CostAmountUsd>` when the model's
/// price is unknown — this type never defaults to `0.0` for a missing
/// price.
///
/// Note: derives `PartialEq` but not `Eq` — the wrapped `f64` has no `Eq`
/// impl in `std`, so `Eq` cannot be derived here despite the Domain
/// Glossary's blanket "derive ... Eq" note for `cost_metrics` types (see
/// this task's return notes).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CostAmountUsd(pub f64);

/// Price a token count at a given per-token USD rate.
///
/// `usd_per_token` is a **per-token** rate (matching `LiteLLM`'s
/// `input_cost_per_token` source data), never a per-million rate — no
/// x1,000,000 conversion belongs anywhere in this pipeline.
#[must_use]
#[allow(clippy::cast_precision_loss)]
// A session would need > 2^52 tokens (far beyond any real context window)
// before this f64 conversion lost precision, so the loss is not reachable.
pub fn cost_for_tokens(tokens: &TokenCount, usd_per_token: f64) -> CostAmountUsd {
    CostAmountUsd(tokens.value as f64 * usd_per_token)
}

/// Per-`(SessionKey, RequestId)` row state, distinguishing "no data yet"
/// from "compaction ran but saved nothing."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReconciliationStatus {
    /// Counterfactual recorded, actual not yet arrived.
    Pending,
    /// Both sides recorded.
    Reconciled,
    /// Request failed/timed out; counterfactual will never be joined.
    Abandoned,
}

/// Where a `PricingTable` entry's numbers came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PricingSource {
    #[default]
    Static,
    Live,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn token_source_should_round_trip_serde_when_estimated_variant() {
        let source = TokenSource::Estimated {
            via: EstimatorKind::TiktokenCl100k,
        };
        let json = serde_json::to_string(&source).unwrap();
        assert_eq!(json, r#"{"Estimated":{"via":"TiktokenCl100k"}}"#);
        let round_tripped: TokenSource = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped, source);
    }

    #[test]
    fn token_source_should_round_trip_serde_when_exact_variant() {
        let source = TokenSource::Exact;
        let json = serde_json::to_string(&source).unwrap();
        let round_tripped: TokenSource = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped, source);
    }

    #[test]
    fn request_id_default_should_equal_nil_uuid_when_not_explicitly_constructed() {
        assert_eq!(RequestId::default(), RequestId(Uuid::nil()));
        assert_eq!(RequestId::default(), RequestId::default());
        assert_ne!(RequestId::default(), RequestId::new());
    }

    #[test]
    fn estimator_kind_should_round_trip_serde_when_any_variant() {
        for kind in [
            EstimatorKind::TiktokenCl100k,
            EstimatorKind::TiktokenO200k,
            EstimatorKind::AnthropicCountTokensApi,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let round_tripped: EstimatorKind = serde_json::from_str(&json).unwrap();
            assert_eq!(round_tripped, kind);
        }
    }

    #[test]
    fn reconciliation_status_should_round_trip_serde_when_any_variant() {
        for status in [
            ReconciliationStatus::Pending,
            ReconciliationStatus::Reconciled,
            ReconciliationStatus::Abandoned,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let round_tripped: ReconciliationStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(round_tripped, status);
        }
    }

    #[test]
    fn pricing_source_should_round_trip_serde_when_any_variant() {
        for source in [PricingSource::Static, PricingSource::Live] {
            let json = serde_json::to_string(&source).unwrap();
            let round_tripped: PricingSource = serde_json::from_str(&json).unwrap();
            assert_eq!(round_tripped, source);
        }
    }

    #[test]
    fn cost_for_tokens_should_multiply_value_by_per_token_rate() {
        let tokens = TokenCount {
            value: 1_000,
            source: TokenSource::Exact,
        };
        let cost = cost_for_tokens(&tokens, 0.000_002);
        assert_eq!(cost, CostAmountUsd(0.002));
    }
}
