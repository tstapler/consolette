//! Session-scoped cost-tracking store (Epic 1.3, Story 1.3.1).
//!
//! Mirrors [`crate::session_compaction::session_state::SessionStateStore`]'s
//! `moka`-backed shape, but with one deliberate difference: `get_or_init`
//! uses `moka`'s atomic `Cache::get_with` instead of that store's
//! `get`-then-`insert` pattern, which is a real TOCTOU race under concurrent
//! first access (see `project_plans/compaction-cost-metrics/implementation/plan.md`
//! Epic 1.3, Story 1.3.1, "repair iteration 1 (was adv B3)"). A non-creating
//! [`SessionCostStore::get`] is also exposed so read/upsert call sites never
//! resurrect an evicted or never-seen session as a side effect of reading it.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use moka::future::Cache;
use tokio::sync::RwLock;

use crate::cost_metrics::types::{CostAmountUsd, ReconciliationStatus, RequestId, TokenCount};
use crate::session_compaction::session_state::SessionKey;
use crate::session_compaction::tiered::CompactionTier;

/// Bounded ring capacity per session — old records are evicted, never
/// unbounded growth. `totals_by_tier` (folded only on `Reconciled`) is the
/// durable aggregate; the ring is a bounded history/debugging aid.
pub const MAX_RECORDS_PER_SESSION: usize = 200;

/// Number of [`CompactionTier`] variants, used to size the fixed `totals_by_tier`
/// array. `CompactionTier` does not derive `Hash`, so a `HashMap` keyed by tier
/// is not an option here — see plan.md Task 1.3.1a.
const TIER_COUNT: usize = 4;

/// Map a [`CompactionTier`] to its index into a 4-element `totals_by_tier` array.
#[must_use]
pub fn tier_index(tier: CompactionTier) -> usize {
    match tier {
        CompactionTier::Off => 0,
        CompactionTier::Micro => 1,
        CompactionTier::Auto => 2,
        CompactionTier::Full => 3,
    }
}

/// All four [`CompactionTier`] variants in `tier_index` order, for callers
/// that need to zip `totals_by_tier` back up with its tier.
pub const ALL_TIERS: [CompactionTier; TIER_COUNT] = [
    CompactionTier::Off,
    CompactionTier::Micro,
    CompactionTier::Auto,
    CompactionTier::Full,
];

/// One request's cost-tracking row.
#[derive(Debug, Clone, PartialEq)]
pub struct CostRecord {
    pub request_id: RequestId,
    pub tier: Option<CompactionTier>,
    pub counterfactual_est: Option<TokenCount>,
    pub compacted_est: Option<TokenCount>,
    pub actual_tokens: Option<TokenCount>,
    pub model: Option<String>,
    pub status: ReconciliationStatus,
    pub recorded_at: DateTime<Utc>,
    pub cost: Option<CostAmountUsd>,
}

/// Running totals for one [`CompactionTier`], folded in exactly once per
/// record — only when that record transitions to `Reconciled` (never while
/// `Pending`/`Abandoned`). See plan.md Task 1.3.1a ("repair iteration 1, was
/// arch B5.1/B5.2").
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TierTotals {
    pub counterfactual_tokens: u64,
    pub compacted_tokens: u64,
    pub actual_tokens: u64,
    pub cost_counterfactual: Option<CostAmountUsd>,
    pub cost_actual: Option<CostAmountUsd>,
    /// How many records have been folded into this tier's totals. Not in
    /// plan.md's original field list, but needed to distinguish "no
    /// reconciled records yet" (report as `None`) from "reconciled records
    /// exist and legitimately sum to zero savings" (report as `Some(0)`,
    /// e.g. `CompactionTier::Off`) — see Story 1.3.3's acceptance criteria.
    pub reconciled_count: u64,
}

/// Per-session cost-tracking state: a bounded ring of individual records plus
/// running per-tier totals.
#[derive(Debug, Clone)]
pub struct SessionCostState {
    pub records: VecDeque<CostRecord>,
    pub totals_by_tier: [TierTotals; TIER_COUNT],
}

impl Default for SessionCostState {
    fn default() -> Self {
        SessionCostState {
            records: VecDeque::new(),
            totals_by_tier: [TierTotals::default(); TIER_COUNT],
        }
    }
}

impl SessionCostState {
    /// Push a record into the bounded ring, evicting the oldest if at
    /// capacity. Only manages the ring — never touches `totals_by_tier`
    /// (that only happens on the `Reconciled` transition, in `CostTracker`).
    pub fn push_record(&mut self, record: CostRecord) {
        if self.records.len() >= MAX_RECORDS_PER_SESSION {
            self.records.pop_front();
        }
        self.records.push_back(record);
    }

    /// Find a mutable reference to the record with this `request_id`, if the
    /// ring still holds it (it may have been evicted).
    pub fn find_mut(&mut self, request_id: RequestId) -> Option<&mut CostRecord> {
        self.records.iter_mut().find(|r| r.request_id == request_id)
    }
}

/// TTL-evicted, atomically-initialized store of [`SessionCostState`], one
/// entry per [`SessionKey`].
pub struct SessionCostStore {
    cache: Cache<SessionKey, Arc<RwLock<SessionCostState>>>,
}

impl SessionCostStore {
    #[allow(clippy::new_without_default)]
    pub fn new() -> impl Future<Output = Self> {
        let cache = Cache::builder()
            .max_capacity(1000)
            .time_to_live(Duration::from_hours(1))
            .build();
        std::future::ready(SessionCostStore { cache })
    }

    /// Fetch this session's state, atomically creating an empty one if
    /// absent. Safe under concurrent first access: `moka`'s `get_with` runs
    /// the initializer exactly once per key even when many callers race, and
    /// every caller receives the same `Arc`.
    pub async fn get_or_init(&self, key: &SessionKey) -> Arc<RwLock<SessionCostState>> {
        self.cache
            .get_with(key.clone(), async {
                Arc::new(RwLock::new(SessionCostState::default()))
            })
            .await
    }

    /// Fetch this session's state without creating one — returns `None` for
    /// an evicted or never-seen key. Used by every call site that must not
    /// resurrect a session as a side effect of reading it.
    pub async fn get(&self, key: &SessionKey) -> Option<Arc<RwLock<SessionCostState>>> {
        self.cache.get(key).await
    }

    /// Force-evict a key. Test-only hook for simulating TTL expiry without
    /// waiting out a real `time_to_live`.
    #[cfg(test)]
    pub async fn invalidate(&self, key: &SessionKey) {
        self.cache.invalidate(key).await;
    }

    /// Like [`SessionCostStore::new`], but with a caller-supplied TTL so a
    /// TTL-eviction test can wait out a near-zero `time_to_live` instead of
    /// the real one-hour default (plan.md Task 4.2.1a).
    #[cfg(test)]
    pub fn new_with_ttl(ttl: Duration) -> impl Future<Output = Self> {
        let cache = Cache::builder()
            .max_capacity(1000)
            .time_to_live(ttl)
            .build();
        std::future::ready(SessionCostStore { cache })
    }

    /// Number of entries currently in the cache. Test-only, used to assert a
    /// non-creating `get()` did not create an entry as a side effect.
    #[cfg(test)]
    pub async fn entry_count(&self) -> u64 {
        self.cache.run_pending_tasks().await;
        self.cache.entry_count()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use uuid::Uuid;

    fn sample_record(request_id: RequestId) -> CostRecord {
        CostRecord {
            request_id,
            tier: Some(CompactionTier::Full),
            counterfactual_est: None,
            compacted_est: None,
            actual_tokens: None,
            model: None,
            status: ReconciliationStatus::Pending,
            recorded_at: Utc::now(),
            cost: None,
        }
    }

    #[tokio::test]
    async fn get_or_init_should_return_same_arc_when_twenty_callers_race_on_fresh_key() {
        let store = Arc::new(SessionCostStore::new().await);
        let key = SessionKey::new("s1");
        let init_calls = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..20 {
            let store = Arc::clone(&store);
            let key = key.clone();
            let init_calls = Arc::clone(&init_calls);
            handles.push(tokio::spawn(async move {
                // Exercise the same atomic-init code path as `get_or_init`,
                // but count how many times the initializer body actually
                // runs, which `get_or_init` itself doesn't expose.
                store
                    .cache
                    .get_with(key.clone(), async {
                        init_calls.fetch_add(1, Ordering::SeqCst);
                        Arc::new(RwLock::new(SessionCostState::default()))
                    })
                    .await
            }));
        }

        let mut arcs = Vec::new();
        for handle in handles {
            arcs.push(handle.await.unwrap());
        }

        let first = &arcs[0];
        for arc in &arcs {
            assert!(Arc::ptr_eq(first, arc));
        }
        assert_eq!(init_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn get_or_init_should_give_independent_state_when_keys_differ() {
        let store = SessionCostStore::new().await;
        let key_a = SessionKey::new("s1");
        let key_b = SessionKey::new("s2");

        let state_a = store.get_or_init(&key_a).await;
        state_a
            .write()
            .await
            .push_record(sample_record(RequestId::new()));

        let state_b = store.get_or_init(&key_b).await;
        assert!(state_b.read().await.records.is_empty());
    }

    #[tokio::test]
    async fn get_should_return_none_when_key_never_seen_and_must_not_create_entry() {
        let store = SessionCostStore::new().await;
        let key = SessionKey::new("ghost");

        let before = store.entry_count().await;
        let result = store.get(&key).await;
        let after = store.entry_count().await;

        assert!(result.is_none());
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn push_record_should_evict_oldest_when_capacity_exceeded() {
        let mut state = SessionCostState::default();
        let mut request_ids = Vec::new();
        for _ in 0..=MAX_RECORDS_PER_SESSION {
            let request_id = RequestId(Uuid::new_v4());
            request_ids.push(request_id);
            state.push_record(sample_record(request_id));
        }

        assert_eq!(state.records.len(), MAX_RECORDS_PER_SESSION);
        // The very first inserted record was evicted.
        assert!(!state.records.iter().any(|r| r.request_id == request_ids[0]));
        // The last inserted record survives.
        assert!(state
            .records
            .iter()
            .any(|r| r.request_id == *request_ids.last().unwrap()));
        // No record was reconciled, so totals reflect none of the inserts —
        // eviction of un-folded rows cannot desync totals from history.
        assert_eq!(state.totals_by_tier, [TierTotals::default(); TIER_COUNT]);
    }
}
