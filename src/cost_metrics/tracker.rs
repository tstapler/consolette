//! `CostTracker` — the write path (Story 1.3.2) and the read path
//! (`report_for_session`, Story 1.3.3) for per-session cost accounting.
//!
//! **Deviation from plan.md Task 1.3.2a's literal struct shape**: the plan's
//! `CostTracker` struct includes `tiktoken: TiktokenEstimator` and
//! `anthropic: BoundedEstimator<AnthropicCountTokensEstimator>` fields.
//! Those two fields are omitted here: `AnthropicCountTokensEstimator::new`
//! requires an `Arc<Upstream>`/`SecretResolver`/`ExecCredentialCache` that
//! `CostTracker::new(pricing: PricingTable)`'s signature (also specified by
//! Task 1.3.2a) has no way to supply, and no Epic 1.3 method or test
//! actually calls an estimator through `CostTracker` — estimation is
//! `CostTrackingHook::post_compact`'s job (Epic 2.1), which will hold its
//! own estimator instances and call `record_pending`/`record_counterfactual`
//! with already-computed `TokenCount`s. `CostTracker` here owns only state
//! (`SessionCostStore`) and pricing.
//!
//! **`PricingTable`/`ModelPrice`**: Epic 1.4 (`src/cost_metrics/pricing.rs`)
//! owns the real implementation (`load_default`, `merge_overrides`, live
//! refresh from `LiteLLM`, `PricingSource` plumbing). Epic 1.3 originally
//! carried a minimal local stub here just far enough to make the
//! write-time fold step and its golden-value pricing test (Task 1.3.3,
//! "was arch B5.2/adv B5") compile and pass; that stub has been replaced
//! with the real type imported from `pricing.rs` below.

use std::sync::Arc;

use chrono::Utc;
use tokio::sync::watch;

use crate::cost_metrics::pricing::PricingTable;
use crate::cost_metrics::report::{CostReport, CostReportError, TierBreakdown};
use crate::cost_metrics::store::{
    tier_index, CostRecord, SessionCostState, SessionCostStore, TierTotals, ALL_TIERS,
};
use crate::cost_metrics::types::{
    cost_for_tokens, CostAmountUsd, ReconciliationStatus, RequestId, TokenCount,
};
use crate::session_compaction::session_state::SessionKey;
use crate::session_compaction::tiered::CompactionTier;

/// How long a `Pending` row may sit unreconciled before a subsequent read
/// sweeps it to `Abandoned` on its own (REQ-7, plan.md Story 2.2.2's
/// "repair iteration 1" addition). Lazy/read-triggered only — no background
/// timer task, per that story's explicit scope boundary: nothing in this
/// codebase reliably calls `record_request_failed` on every failure yet, so
/// a `Pending` row can otherwise be orphaned forever.
pub const PENDING_MAX_AGE: chrono::Duration = chrono::Duration::minutes(10);

/// Flip any `Pending` record older than [`PENDING_MAX_AGE`] to `Abandoned`.
/// Called from every read/write path that touches a session's state so a
/// stale row ages out even without a live error-handling call site ever
/// invoking `record_request_failed`. Never touches `totals_by_tier` — an
/// aged-out row was never `Reconciled`, so there is nothing to unfold.
fn sweep_stale_pending(state: &mut SessionCostState, now: chrono::DateTime<Utc>) {
    for record in &mut state.records {
        if record.status == ReconciliationStatus::Pending
            && now - record.recorded_at > PENDING_MAX_AGE
        {
            record.status = ReconciliationStatus::Abandoned;
        }
    }
}

/// Errors `CostTracker`'s write-path methods can return.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CostTrackerError {
    /// The session was never seen (via `record_pending`) or was evicted —
    /// callers must not treat this as "create a fresh empty session."
    #[error("session not found")]
    SessionNotFound,
    /// The row for this `request_id` is missing from a session that *is*
    /// known (e.g. evicted from the bounded ring before a late-arriving
    /// write reached it).
    #[error("record not found for request id")]
    RecordNotFound,
}

/// `CostTracker` — session-scoped cost-accounting state plus pricing.
///
/// `pricing` is a `watch::Receiver` rather than an owned `PricingTable` so a
/// production `CostTracker` (via [`CostTracker::new_with_pricing_receiver`])
/// can observe Story 1.4.2's background live-refresh swaps without any
/// write-path call site needing to change: every read still goes through
/// `self.pricing.borrow().clone()`, an `Arc<PricingTable>` clone.
pub struct CostTracker {
    store: SessionCostStore,
    pricing: watch::Receiver<Arc<PricingTable>>,
}

impl CostTracker {
    /// Build a tracker over a fixed, never-refreshed pricing table — the
    /// common case for tests and for a deployment with no live-refresh
    /// configured. Internally wraps `pricing` in a throwaway `watch`
    /// channel; dropping the sender is harmless since nothing ever calls
    /// `send`/`send_replace` on it.
    pub async fn new(pricing: PricingTable) -> Self {
        let (_tx, rx) = watch::channel(Arc::new(pricing));
        CostTracker {
            store: SessionCostStore::new().await,
            pricing: rx,
        }
    }

    /// Build a tracker over an externally-owned, potentially live-refreshing
    /// pricing receiver (Story 1.4.2) — the sender side is held by
    /// [`crate::cost_metrics::pricing::spawn_pricing_refresh_task`].
    pub async fn new_with_pricing_receiver(pricing: watch::Receiver<Arc<PricingTable>>) -> Self {
        CostTracker {
            store: SessionCostStore::new().await,
            pricing,
        }
    }

    /// Test-only accessor to force-evict a session, simulating TTL expiry.
    #[cfg(test)]
    async fn invalidate(&self, key: &SessionKey) {
        self.store.invalidate(key).await;
    }

    /// Test-only constructor taking an already-built `SessionCostStore` —
    /// lets a test supply a store with a non-default TTL (e.g. via
    /// [`SessionCostStore::new_with_ttl`]) without `CostTracker::new`
    /// growing a TTL parameter it has no other use for.
    #[cfg(test)]
    fn new_with_store(store: SessionCostStore, pricing: PricingTable) -> Self {
        let (_tx, rx) = watch::channel(Arc::new(pricing));
        CostTracker { store, pricing: rx }
    }

    /// Insert a `Pending` row synchronously — no network call, no `.await`
    /// beyond acquiring the write lock. Uses `get_or_init`: this is the one
    /// call site allowed to create a session's state from nothing.
    pub async fn record_pending(
        &self,
        session_key: &SessionKey,
        request_id: RequestId,
        tier: CompactionTier,
    ) {
        let state = self.store.get_or_init(session_key).await;
        let mut state = state.write().await;
        state.push_record(CostRecord {
            request_id,
            tier: Some(tier),
            counterfactual_est: None,
            compacted_est: None,
            actual_tokens: None,
            model: None,
            status: ReconciliationStatus::Pending,
            recorded_at: Utc::now(),
            cost: None,
        });
    }

    /// Fill in the counterfactual/compacted estimates for an existing row.
    /// Non-creating: a missing session or a missing (evicted) record is
    /// reported, never fabricated.
    ///
    /// # Errors
    ///
    /// Returns `Err(SessionNotFound)` if `session_key` is unknown/evicted,
    /// or `Err(RecordNotFound)` if the session exists but `request_id`'s row
    /// was evicted before this call arrived.
    pub async fn record_counterfactual(
        &self,
        session_key: &SessionKey,
        request_id: RequestId,
        counterfactual_est: TokenCount,
        compacted_est: TokenCount,
    ) -> Result<(), CostTrackerError> {
        let state = self
            .store
            .get(session_key)
            .await
            .ok_or(CostTrackerError::SessionNotFound)?;
        let mut state = state.write().await;
        let record = state
            .find_mut(request_id)
            .ok_or(CostTrackerError::RecordNotFound)
            .inspect_err(|_| {
                tracing::warn!(
                    ?request_id,
                    "record_counterfactual: row missing (evicted before estimator returned)"
                );
            })?;
        record.counterfactual_est = Some(counterfactual_est);
        record.compacted_est = Some(compacted_est);
        let pricing = self.pricing.borrow().clone();
        try_fold_if_ready(&mut state, request_id, &pricing);
        Ok(())
    }

    /// Upsert the actual token usage for a request. If the row already
    /// exists (the common case), updates it in place and — if the
    /// counterfactual side is already populated — reconciles and folds into
    /// `totals_by_tier` exactly once. If no row exists yet (adverse
    /// ordering: the actual arrived before `record_pending`, within a
    /// session the store already knows about — via `get_or_init` here, the
    /// one specified exception beyond `record_pending`), creates a
    /// `Pending` row with `actual_tokens` populated.
    ///
    /// Idempotent per `request_id`: calling this twice replaces the stored
    /// `actual_tokens`/fold contribution rather than accumulating it.
    ///
    /// Returns `Err(SessionNotFound)` only when the *session* itself was
    /// never seen or was evicted — never as a stand-in for "row missing
    /// within a known session," which is the adverse-ordering create path.
    ///
    /// # Errors
    ///
    /// Returns `Err(SessionNotFound)` if `session_key` is unknown/evicted.
    pub async fn record_actual_usage(
        &self,
        session_key: &SessionKey,
        request_id: RequestId,
        actual: TokenCount,
    ) -> Result<(), CostTrackerError> {
        // Non-creating get first: a session that was never seen (or was
        // evicted) must not be silently resurrected.
        let Some(state) = self.store.get(session_key).await else {
            return Err(CostTrackerError::SessionNotFound);
        };
        let mut state_guard = state.write().await;
        let pricing = self.pricing.borrow().clone();

        let already_reconciled_tier = state_guard.find_mut(request_id).and_then(|record| {
            (record.status == ReconciliationStatus::Reconciled)
                .then(|| record.tier.unwrap_or(CompactionTier::Off))
        });

        if state_guard.find_mut(request_id).is_none() {
            // Adverse ordering: actual arrived before `record_pending`.
            // Create the row now; the counterfactual side will reconcile it
            // later.
            state_guard.push_record(CostRecord {
                request_id,
                tier: None,
                counterfactual_est: None,
                compacted_est: None,
                actual_tokens: Some(actual),
                model: None,
                status: ReconciliationStatus::Pending,
                recorded_at: Utc::now(),
                cost: None,
            });
        } else if let Some(tier) = already_reconciled_tier {
            // Was already reconciled and folded once; unfold its *old*
            // contribution — record.actual_tokens still holds the value
            // that was actually folded in — before overwriting it with the
            // replacement, so a decreasing retry can't corrupt totals via
            // `saturating_sub` against the wrong operand.
            unfold(&mut state_guard, tier, request_id, &pricing);
            if let Some(record) = state_guard.find_mut(request_id) {
                record.actual_tokens = Some(actual);
                record.status = ReconciliationStatus::Pending;
            }
            try_fold_if_ready(&mut state_guard, request_id, &pricing);
        } else {
            if let Some(record) = state_guard.find_mut(request_id) {
                record.actual_tokens = Some(actual);
            }
            try_fold_if_ready(&mut state_guard, request_id, &pricing);
        }

        Ok(())
    }

    /// Mark a `Pending` row `Abandoned`. Contributes nothing to
    /// `totals_by_tier`, and never un-folds anything already folded
    /// (`Abandoned` rows by construction never reached `Reconciled`).
    pub async fn record_request_failed(&self, session_key: &SessionKey, request_id: RequestId) {
        let Some(state) = self.store.get(session_key).await else {
            return;
        };
        let mut state = state.write().await;
        if let Some(record) = state.find_mut(request_id) {
            if record.status == ReconciliationStatus::Pending {
                record.status = ReconciliationStatus::Abandoned;
            }
        }
    }

    /// Pure read over `totals_by_tier` — performs zero `PricingTable`
    /// lookups; every `CostAmountUsd` here was already computed and folded
    /// in at write time.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `session_key` is unknown/evicted.
    pub async fn report_for_session(
        &self,
        session_key: &SessionKey,
    ) -> Result<CostReport, CostReportError> {
        let state = self
            .store
            .get(session_key)
            .await
            .ok_or(CostReportError::SessionNotFound)?;
        {
            // Lazy read-time age-out sweep (REQ-7): flip any long-stale
            // `Pending` row to `Abandoned` before computing the report, so
            // `abandoned_count`/`pending_count` reflect it on this very call.
            let mut state = state.write().await;
            sweep_stale_pending(&mut state, Utc::now());
        }
        let state = state.read().await;

        let mut by_tier = Vec::with_capacity(ALL_TIERS.len());
        let mut total_counterfactual = 0u64;
        let mut total_compacted = 0u64;
        let mut total_actual = 0u64;
        let mut total_reconciled = 0u64;
        let mut cost_counterfactual_sum: Option<CostAmountUsd> = None;
        let mut cost_actual_sum: Option<CostAmountUsd> = None;

        for tier in ALL_TIERS {
            let totals = state.totals_by_tier[tier_index(tier)];
            let has_data = totals.reconciled_count > 0;
            let tokens_saved = has_data.then(|| {
                totals
                    .counterfactual_tokens
                    .saturating_sub(totals.compacted_tokens)
            });
            by_tier.push(TierBreakdown {
                tier,
                counterfactual_tokens: has_data.then_some(totals.counterfactual_tokens),
                compacted_tokens: has_data.then_some(totals.compacted_tokens),
                actual_tokens: has_data.then_some(totals.actual_tokens),
                tokens_saved,
                cost_counterfactual_usd: totals.cost_counterfactual.map(|c| c.0),
                cost_actual_usd: totals.cost_actual.map(|c| c.0),
            });

            if has_data {
                total_counterfactual += totals.counterfactual_tokens;
                total_compacted += totals.compacted_tokens;
                total_actual += totals.actual_tokens;
                total_reconciled += totals.reconciled_count;
                cost_counterfactual_sum =
                    sum_opt(cost_counterfactual_sum, totals.cost_counterfactual);
                cost_actual_sum = sum_opt(cost_actual_sum, totals.cost_actual);
            }
        }

        let pending_count = state
            .records
            .iter()
            .filter(|r| r.status == ReconciliationStatus::Pending)
            .count();
        let abandoned_count = state
            .records
            .iter()
            .filter(|r| r.status == ReconciliationStatus::Abandoned)
            .count();

        let has_any_reconciled = total_reconciled > 0;

        Ok(CostReport {
            session_key: session_key.0.clone(),
            actual_tokens: has_any_reconciled.then_some(total_actual),
            actual_source: has_any_reconciled
                .then_some(crate::cost_metrics::types::TokenSource::Exact),
            counterfactual_tokens: has_any_reconciled.then_some(total_counterfactual),
            counterfactual_source: has_any_reconciled.then_some(
                crate::cost_metrics::types::TokenSource::Estimated {
                    via: crate::cost_metrics::types::EstimatorKind::TiktokenO200k,
                },
            ),
            compacted_tokens: has_any_reconciled.then_some(total_compacted),
            tokens_saved: has_any_reconciled
                .then_some(total_counterfactual.saturating_sub(total_compacted)),
            estimated_cost_saved_usd: cost_counterfactual_sum.map(|c| c.0),
            actual_cost_usd: cost_actual_sum.map(|c| c.0),
            pricing_source: self.pricing.borrow().source(),
            pending_count,
            abandoned_count,
            by_tier,
        })
    }
}

fn sum_opt(acc: Option<CostAmountUsd>, next: Option<CostAmountUsd>) -> Option<CostAmountUsd> {
    match (acc, next) {
        (None, None) => None,
        (None, Some(c)) | (Some(c), None) => Some(c),
        (Some(a), Some(b)) => Some(CostAmountUsd(a.0 + b.0)),
    }
}

/// Fold a record into `totals_by_tier` exactly once, the moment it has
/// enough data to reconcile and the row isn't already `Reconciled`. Shared by
/// `record_counterfactual` and `record_actual_usage` so there is exactly one
/// place that appends to `totals_by_tier`.
///
/// Two shapes are considered "ready":
/// - The session-compaction shape: all three of `counterfactual_est`,
///   `compacted_est`, and `actual_tokens` are present (there was a real
///   compaction decision to compare against).
/// - The live HTTP-proxy shape (`tier == CompactionTier::Off`, entrypoint's
///   `begin_cost_tracking`/`CostTrackingStream`, ADR-016): a proxied request
///   has no counterfactual to compare against by construction, so `actual_tokens`
///   alone is enough — the record folds with a zero counterfactual/compacted
///   baseline, meaning "no savings computation applies," not "zero tokens saved."
fn try_fold_if_ready(state: &mut SessionCostState, request_id: RequestId, pricing: &PricingTable) {
    let Some(record) = state.find_mut(request_id) else {
        return;
    };
    if record.status == ReconciliationStatus::Reconciled {
        return;
    }
    let Some(actual) = record.actual_tokens else {
        return;
    };
    let tier = record.tier.unwrap_or(CompactionTier::Off);
    let (counterfactual, compacted) = match (record.counterfactual_est, record.compacted_est) {
        (Some(c), Some(k)) => (c, k),
        (None, None) if tier == CompactionTier::Off => {
            let zero = TokenCount {
                value: 0,
                source: actual.source,
            };
            (zero, zero)
        }
        _ => return,
    };

    let price = record.model.as_deref().and_then(|m| pricing.price_for(m));
    let cost_counterfactual =
        price.map(|p| cost_for_tokens(&counterfactual, p.input_usd_per_token));
    let cost_actual = price.map(|p| cost_for_tokens(&actual, p.input_usd_per_token));

    record.status = ReconciliationStatus::Reconciled;
    record.cost = cost_actual;

    let totals: &mut TierTotals = &mut state.totals_by_tier[tier_index(tier)];
    totals.counterfactual_tokens += counterfactual.value;
    totals.compacted_tokens += compacted.value;
    totals.actual_tokens += actual.value;
    totals.reconciled_count += 1;
    totals.cost_counterfactual = sum_opt(totals.cost_counterfactual, cost_counterfactual);
    totals.cost_actual = sum_opt(totals.cost_actual, cost_actual);
}

/// Reverse a previous fold's contribution for `request_id` from `tier`'s
/// totals, used by `record_actual_usage`'s idempotent-replace path so a
/// second call for the same `request_id` never double-counts.
fn unfold(
    state: &mut SessionCostState,
    tier: CompactionTier,
    request_id: RequestId,
    pricing: &PricingTable,
) {
    let Some(record) = state.find_mut(request_id) else {
        return;
    };
    let Some(actual) = record.actual_tokens else {
        return;
    };
    let (counterfactual, compacted) = match (record.counterfactual_est, record.compacted_est) {
        (Some(c), Some(k)) => (c, k),
        (None, None) if tier == CompactionTier::Off => {
            let zero = TokenCount {
                value: 0,
                source: actual.source,
            };
            (zero, zero)
        }
        _ => return,
    };
    let price = record.model.as_deref().and_then(|m| pricing.price_for(m));
    let cost_counterfactual =
        price.map(|p| cost_for_tokens(&counterfactual, p.input_usd_per_token));
    let old_cost_actual = record.cost;

    let totals: &mut TierTotals = &mut state.totals_by_tier[tier_index(tier)];
    totals.counterfactual_tokens = totals
        .counterfactual_tokens
        .saturating_sub(counterfactual.value);
    totals.compacted_tokens = totals.compacted_tokens.saturating_sub(compacted.value);
    totals.actual_tokens = totals.actual_tokens.saturating_sub(actual.value);
    totals.reconciled_count = totals.reconciled_count.saturating_sub(1);
    if let (Some(total), Some(old)) = (totals.cost_actual, old_cost_actual) {
        totals.cost_actual = Some(CostAmountUsd(total.0 - old.0));
    }
    if let (Some(total), Some(old)) = (totals.cost_counterfactual, cost_counterfactual) {
        totals.cost_counterfactual = Some(CostAmountUsd(total.0 - old.0));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::cost_metrics::pricing::ModelPrice;
    use crate::cost_metrics::types::TokenSource;
    use std::sync::Arc;

    fn tc(value: u64) -> TokenCount {
        TokenCount {
            value,
            source: TokenSource::Exact,
        }
    }

    fn est(value: u64) -> TokenCount {
        TokenCount {
            value,
            source: TokenSource::Estimated {
                via: crate::cost_metrics::types::EstimatorKind::TiktokenO200k,
            },
        }
    }

    #[tokio::test]
    async fn record_pending_should_insert_pending_row_synchronously_when_called() {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let request_id = RequestId::new();

        tracker
            .record_pending(&key, request_id, CompactionTier::Full)
            .await;

        let state = tracker.store.get(&key).await.expect("session should exist");
        let state = state.read().await;
        assert_eq!(state.records.len(), 1);
        let record = &state.records[0];
        assert_eq!(record.status, ReconciliationStatus::Pending);
        assert!(record.counterfactual_est.is_none());
    }

    #[tokio::test]
    async fn record_counterfactual_then_actual_should_reconcile_and_fold_totals_once() {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let request_id = RequestId::new();

        tracker
            .record_pending(&key, request_id, CompactionTier::Full)
            .await;
        tracker
            .record_counterfactual(&key, request_id, est(41200), est(9000))
            .await
            .unwrap();
        tracker
            .record_actual_usage(&key, request_id, tc(8600))
            .await
            .unwrap();

        let state = tracker.store.get(&key).await.unwrap();
        let state = state.read().await;
        let record = &state.records[0];
        assert_eq!(record.status, ReconciliationStatus::Reconciled);
        let totals = state.totals_by_tier[tier_index(CompactionTier::Full)];
        assert_eq!(totals.counterfactual_tokens, 41200);
        assert_eq!(totals.compacted_tokens, 9000);
        assert_eq!(totals.actual_tokens, 8600);
        assert_eq!(totals.reconciled_count, 1);
    }

    #[tokio::test]
    async fn record_actual_usage_should_create_pending_row_when_it_arrives_before_record_pending() {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let request_id = RequestId::new();

        // Session must already be known to the store (a real request round
        // trip happened) even though `record_pending`'s row hasn't landed
        // yet — simulate that by initializing the session directly.
        tracker.store.get_or_init(&key).await;

        tracker
            .record_actual_usage(&key, request_id, tc(8600))
            .await
            .unwrap();

        let state = tracker.store.get(&key).await.unwrap();
        {
            let state = state.read().await;
            let record = &state.records[0];
            assert_eq!(record.status, ReconciliationStatus::Pending);
            assert_eq!(record.actual_tokens, Some(tc(8600)));
        }

        tracker
            .record_counterfactual(&key, request_id, est(41200), est(9000))
            .await
            .unwrap();

        let state = state.read().await;
        let record = &state.records[0];
        assert_eq!(record.status, ReconciliationStatus::Reconciled);
        let totals = state.totals_by_tier[tier_index(record.tier.unwrap_or(CompactionTier::Off))];
        assert_eq!(totals.reconciled_count, 1);
    }

    #[tokio::test]
    async fn record_actual_usage_should_replace_not_accumulate_totals_when_called_twice_for_same_request_id(
    ) {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let request_id = RequestId::new();

        tracker
            .record_pending(&key, request_id, CompactionTier::Full)
            .await;
        tracker
            .record_counterfactual(&key, request_id, est(41200), est(9000))
            .await
            .unwrap();
        tracker
            .record_actual_usage(&key, request_id, tc(8600))
            .await
            .unwrap();
        // Retry: same request_id, different actual value.
        tracker
            .record_actual_usage(&key, request_id, tc(9200))
            .await
            .unwrap();

        let state = tracker.store.get(&key).await.unwrap();
        let state = state.read().await;
        let totals = state.totals_by_tier[tier_index(CompactionTier::Full)];
        assert_eq!(totals.actual_tokens, 9200);
        assert_eq!(totals.reconciled_count, 1);
    }

    /// Regression test for a fold/unfold ordering bug: `unfold` used to read
    /// `record.actual_tokens` *after* `record_actual_usage` had already
    /// overwritten it with the new value, so it subtracted the new value
    /// from `totals.actual_tokens` instead of the old one. A rising retry
    /// (8600 -> 9200) masked this via `saturating_sub` flooring at zero
    /// either way, but a second reconciled record sharing the tier exposes
    /// it: the wrong subtrahend eats into the *other* record's contribution.
    #[tokio::test]
    async fn record_actual_usage_should_not_corrupt_other_records_totals_when_retried_with_decreasing_value(
    ) {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let request_id_a = RequestId::new();
        let request_id_b = RequestId::new();

        // Record A: reconciled once at 9200, never retried.
        tracker
            .record_pending(&key, request_id_a, CompactionTier::Full)
            .await;
        tracker
            .record_counterfactual(&key, request_id_a, est(41200), est(9000))
            .await
            .unwrap();
        tracker
            .record_actual_usage(&key, request_id_a, tc(9200))
            .await
            .unwrap();

        // Record B: reconciled at 9200, then retried down to 8600.
        tracker
            .record_pending(&key, request_id_b, CompactionTier::Full)
            .await;
        tracker
            .record_counterfactual(&key, request_id_b, est(41200), est(9000))
            .await
            .unwrap();
        tracker
            .record_actual_usage(&key, request_id_b, tc(9200))
            .await
            .unwrap();
        tracker
            .record_actual_usage(&key, request_id_b, tc(8600))
            .await
            .unwrap();

        let state = tracker.store.get(&key).await.unwrap();
        let state = state.read().await;
        let totals = state.totals_by_tier[tier_index(CompactionTier::Full)];
        // Correct total: A's 9200 + B's replaced 8600 = 17800. The buggy
        // implementation subtracted B's *new* value (8600) from the shared
        // tier total instead of B's old value (9200), corrupting A's
        // contribution in the process.
        assert_eq!(totals.actual_tokens, 17800);
        assert_eq!(totals.reconciled_count, 2);
    }

    #[tokio::test]
    async fn record_actual_usage_should_return_session_not_found_when_entry_evicted_before_write() {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let request_id = RequestId::new();

        tracker
            .record_pending(&key, request_id, CompactionTier::Full)
            .await;
        tracker.invalidate(&key).await;

        let result = tracker
            .record_actual_usage(&key, request_id, tc(8600))
            .await;
        assert_eq!(result, Err(CostTrackerError::SessionNotFound));
    }

    #[tokio::test]
    async fn record_request_failed_should_mark_abandoned_and_leave_totals_unchanged_when_pending_row_exists(
    ) {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let request_id = RequestId::new();

        tracker
            .record_pending(&key, request_id, CompactionTier::Full)
            .await;
        tracker.record_request_failed(&key, request_id).await;

        let state = tracker.store.get(&key).await.unwrap();
        let state = state.read().await;
        let record = &state.records[0];
        assert_eq!(record.status, ReconciliationStatus::Abandoned);
        assert_eq!(
            state.totals_by_tier[tier_index(CompactionTier::Full)],
            TierTotals::default()
        );
    }

    #[tokio::test]
    async fn report_for_session_should_return_session_not_found_when_session_never_recorded() {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("ghost");

        let result = tracker.report_for_session(&key).await;
        assert_eq!(result, Err(CostReportError::SessionNotFound));
    }

    #[tokio::test]
    async fn report_for_session_should_return_none_tokens_saved_when_only_pending_records_exist() {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let request_id = RequestId::new();

        tracker
            .record_pending(&key, request_id, CompactionTier::Full)
            .await;
        tracker
            .record_counterfactual(&key, request_id, est(41200), est(9000))
            .await
            .unwrap();

        let report = tracker.report_for_session(&key).await.unwrap();
        assert_eq!(report.actual_tokens, None);
        assert_eq!(report.tokens_saved, None);
        assert_eq!(report.pending_count, 1);
    }

    #[tokio::test]
    async fn report_for_session_should_compute_tokens_saved_as_counterfactual_minus_compacted_when_both_estimated_by_same_estimator(
    ) {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let request_id = RequestId::new();

        tracker
            .record_pending(&key, request_id, CompactionTier::Off)
            .await;
        tracker
            .record_counterfactual(&key, request_id, est(10000), est(10000))
            .await
            .unwrap();
        tracker
            .record_actual_usage(&key, request_id, tc(10000))
            .await
            .unwrap();

        let report = tracker.report_for_session(&key).await.unwrap();
        assert_eq!(report.tokens_saved, Some(0));
    }

    #[tokio::test]
    async fn report_for_session_should_show_per_tier_breakdown_when_multiple_tiers_reconciled() {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let auto_request = RequestId::new();
        let full_request = RequestId::new();

        tracker
            .record_pending(&key, auto_request, CompactionTier::Auto)
            .await;
        tracker
            .record_counterfactual(&key, auto_request, est(10000), est(8000))
            .await
            .unwrap();
        tracker
            .record_actual_usage(&key, auto_request, tc(8000))
            .await
            .unwrap();

        tracker
            .record_pending(&key, full_request, CompactionTier::Full)
            .await;
        tracker
            .record_counterfactual(&key, full_request, est(12000), est(4000))
            .await
            .unwrap();
        tracker
            .record_actual_usage(&key, full_request, tc(4000))
            .await
            .unwrap();

        let report = tracker.report_for_session(&key).await.unwrap();
        let auto = report
            .by_tier
            .iter()
            .find(|t| t.tier == CompactionTier::Auto)
            .unwrap();
        assert_eq!(auto.tokens_saved, Some(2000));
        let full = report
            .by_tier
            .iter()
            .find(|t| t.tier == CompactionTier::Full)
            .unwrap();
        assert_eq!(full.tokens_saved, Some(8000));
        assert_eq!(report.tokens_saved, Some(10000));
    }

    #[tokio::test]
    async fn report_for_session_should_sum_per_record_priced_costs_when_session_spans_two_models() {
        let mut pricing = PricingTable::new();
        pricing.insert(
            "claude-sonnet-5",
            ModelPrice {
                input_usd_per_token: 0.000_003,
                output_usd_per_token: 0.000_015,
                ..Default::default()
            },
        );
        pricing.insert(
            "gpt-4o",
            ModelPrice {
                input_usd_per_token: 0.000_002_5,
                output_usd_per_token: 0.000_01,
                ..Default::default()
            },
        );
        let tracker = CostTracker::new(pricing).await;
        let key = SessionKey::new("s1");

        let claude_request = RequestId::new();
        tracker
            .record_pending(&key, claude_request, CompactionTier::Full)
            .await;
        {
            let state = tracker.store.get(&key).await.unwrap();
            state.write().await.find_mut(claude_request).unwrap().model =
                Some("claude-sonnet-5".to_string());
        }
        tracker
            .record_counterfactual(&key, claude_request, est(10000), est(8000))
            .await
            .unwrap();
        tracker
            .record_actual_usage(&key, claude_request, tc(8000))
            .await
            .unwrap();

        let gpt_request = RequestId::new();
        tracker
            .record_pending(&key, gpt_request, CompactionTier::Full)
            .await;
        {
            let state = tracker.store.get(&key).await.unwrap();
            state.write().await.find_mut(gpt_request).unwrap().model = Some("gpt-4o".to_string());
        }
        tracker
            .record_counterfactual(&key, gpt_request, est(5000), est(5000))
            .await
            .unwrap();
        tracker
            .record_actual_usage(&key, gpt_request, tc(5000))
            .await
            .unwrap();

        let report = tracker.report_for_session(&key).await.unwrap();
        let expected = 8000.0 * 0.000_003 + 5000.0 * 0.000_002_5;
        assert!((report.actual_cost_usd.unwrap() - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn cost_for_tokens_should_equal_exact_per_token_product_when_priced_at_write_time() {
        let tokens = tc(8000);
        let cost = cost_for_tokens(&tokens, 0.000_003);
        assert_eq!(cost, CostAmountUsd(0.024));
    }

    #[tokio::test]
    async fn record_pending_should_insert_twenty_distinct_rows_when_concurrent_on_same_session() {
        let tracker = Arc::new(CostTracker::new(PricingTable::new()).await);
        let key = SessionKey::new("s1");

        let mut handles = Vec::new();
        let mut request_ids = Vec::new();
        for _ in 0..20 {
            let request_id = RequestId::new();
            request_ids.push(request_id);
            let tracker = Arc::clone(&tracker);
            let key = key.clone();
            handles.push(tokio::spawn(async move {
                tracker
                    .record_pending(&key, request_id, CompactionTier::Full)
                    .await;
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }

        let state = tracker.store.get(&key).await.unwrap();
        let state = state.read().await;
        assert_eq!(state.records.len(), 20);
        for request_id in request_ids {
            assert!(state.records.iter().any(|r| r.request_id == request_id));
        }
    }

    // -- Epic 4.2: concurrency and reconciliation edge cases --

    #[tokio::test]
    async fn report_for_session_should_return_session_not_found_when_cache_entry_ttl_expired() {
        use std::time::Duration;

        let store = SessionCostStore::new_with_ttl(Duration::from_millis(50)).await;
        let tracker = CostTracker::new_with_store(store, PricingTable::new());
        let key = SessionKey::new("s1");
        let request_id = RequestId::new();

        tracker
            .record_pending(&key, request_id, CompactionTier::Full)
            .await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let result = tracker.report_for_session(&key).await;
        assert_eq!(result, Err(CostReportError::SessionNotFound));

        // Same error, same shape, as a session that was never seen at all.
        let never_seen = tracker.report_for_session(&SessionKey::new("ghost")).await;
        assert_eq!(result, never_seen);
    }

    #[tokio::test]
    async fn report_for_session_should_return_ok_with_zero_tokens_saved_when_off_tier_has_zero_savings_not_err(
    ) {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let request_id = RequestId::new();

        tracker
            .record_pending(&key, request_id, CompactionTier::Off)
            .await;
        tracker
            .record_counterfactual(&key, request_id, est(10000), est(10000))
            .await
            .unwrap();
        tracker
            .record_actual_usage(&key, request_id, tc(10000))
            .await
            .unwrap();

        let report = tracker.report_for_session(&key).await;
        assert!(
            report.is_ok(),
            "zero savings must not be reported as an error"
        );
        let report = report.unwrap();
        assert_eq!(report.tokens_saved, Some(0));
    }

    #[tokio::test]
    async fn report_for_session_should_age_out_stale_pending_row_to_abandoned_when_older_than_pending_max_age(
    ) {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let request_id = RequestId::new();

        tracker
            .record_pending(&key, request_id, CompactionTier::Full)
            .await;
        {
            let state = tracker.store.get(&key).await.unwrap();
            let mut state = state.write().await;
            let record = state.find_mut(request_id).unwrap();
            // Backdate `recorded_at` past `PENDING_MAX_AGE` — the minimal
            // test-only seam needed, using the already-`pub` `CostRecord`
            // field rather than adding new production API surface.
            record.recorded_at = Utc::now() - PENDING_MAX_AGE - chrono::Duration::seconds(1);
        }

        let report = tracker.report_for_session(&key).await.unwrap();
        assert_eq!(report.abandoned_count, 1);
        assert_eq!(report.pending_count, 0);

        let state = tracker.store.get(&key).await.unwrap();
        let state = state.read().await;
        assert_eq!(state.records[0].status, ReconciliationStatus::Abandoned);
    }

    // ────────────────────────────────────────────────────────────────────
    // Entrypoint (HTTP-proxy) shape: record_pending + record_actual_usage
    // only, never record_counterfactual (ADR-016; plan.md Stories 2.1.2/
    // 2.2.1). A live proxied request has nothing to compare against, so
    // report_for_session must still surface actual usage.
    // ────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn report_for_session_should_reflect_actual_usage_when_no_counterfactual_was_ever_recorded(
    ) {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let request_id = RequestId::new();

        tracker
            .record_pending(&key, request_id, CompactionTier::Off)
            .await;
        tracker
            .record_actual_usage(&key, request_id, tc(42))
            .await
            .unwrap();

        let report = tracker.report_for_session(&key).await.unwrap();
        assert_eq!(
            report.actual_tokens,
            Some(42),
            "actual usage from a no-counterfactual (HTTP-proxy) record must be reported"
        );

        let state = tracker.store.get(&key).await.unwrap();
        let state = state.read().await;
        assert_eq!(state.records[0].status, ReconciliationStatus::Reconciled);
        let totals = state.totals_by_tier[tier_index(CompactionTier::Off)];
        assert_eq!(totals.actual_tokens, 42);
        assert_eq!(totals.counterfactual_tokens, 0);
        assert_eq!(totals.compacted_tokens, 0);
        assert_eq!(totals.reconciled_count, 1);
    }

    #[tokio::test]
    async fn record_actual_usage_retry_should_not_corrupt_totals_for_no_counterfactual_record() {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let request_id = RequestId::new();

        tracker
            .record_pending(&key, request_id, CompactionTier::Off)
            .await;
        tracker
            .record_actual_usage(&key, request_id, tc(42))
            .await
            .unwrap();
        // Retry with a corrected value, mirroring a mid-stream-cut estimate
        // later replaced by an exact count (unfold must reverse the first
        // fold before try_fold_if_ready re-applies the new one).
        tracker
            .record_actual_usage(&key, request_id, tc(50))
            .await
            .unwrap();

        let report = tracker.report_for_session(&key).await.unwrap();
        assert_eq!(report.actual_tokens, Some(50));

        let state = tracker.store.get(&key).await.unwrap();
        let state = state.read().await;
        let totals = state.totals_by_tier[tier_index(CompactionTier::Off)];
        assert_eq!(totals.actual_tokens, 50);
        assert_eq!(totals.reconciled_count, 1);
    }
}
