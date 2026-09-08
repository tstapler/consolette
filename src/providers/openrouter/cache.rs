//! `ModelListCache` (Epic 2.1, plan.md Stories 2.1.1/2.1.2/2.1.3, ADR-001):
//! a shared, TTL-bounded, background-refreshed, sync-readable snapshot of
//! `OpenRouter`'s current free-model list.
//!
//! Backed by `moka::sync::Cache`, not `moka::future::Cache` (the other 5
//! in-repo `moka` usages, e.g. `src/cost_metrics/store.rs`) — this is a
//! deliberate inconsistency, not an oversight: `RoutingStrategy::expand_candidates`/
//! `record_outcome` (ADR-003 of the base consolette ADR set) must stay
//! synchronous, and only `moka::sync::Cache` exposes non-async
//! `get`/`insert`/`invalidate`. See ADR-001
//! (`project_plans/openrouter-routing/decisions/ADR-001-model-list-cache-moka-sync.md`)
//! for the full rationale, including the money-safety-motivated 15-minute
//! TTL and the per-dispatch price-recheck backstop this cache's
//! `FreeModelEntry` shape exists to support (see `mod.rs`'s
//! `check_cached_price`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use tracing::warn;

use super::OpenrouterProvider;

/// Money-safety-bounded TTL (ADR-001): backstop-of-last-resort on how long
/// a stale free-model list can keep serving if the background refresh task
/// (`MODEL_LIST_REFRESH_INTERVAL`, `mod.rs`) ever stops running. In normal
/// operation the 5-minute background refresh keeps entries far fresher
/// than this; the TTL only matters once that stops happening.
pub(crate) const MODEL_LIST_TTL: Duration = Duration::from_mins(15);

/// Sliding window (Story 2.1.3) used to distinguish a minority of cached
/// models genuinely 404ing (staleness) from every cached model 404ing at
/// once (an account-wide data-policy toggle) — see
/// `record_not_found_and_maybe_invalidate`.
const NOT_FOUND_WINDOW: Duration = Duration::from_secs(60);

/// One entry of the cached free-model list — carries price forward (not
/// just the bare id) so `OpenrouterProvider::send()`'s per-dispatch recheck
/// (money-safety backstop mechanism 2, plan.md Risk Control) can verify the
/// *specific selected model's* price, not just its membership.
#[derive(Debug, Clone, PartialEq)]
pub struct FreeModelEntry {
    pub id: String,
    pub price_prompt: f64,
    pub price_completion: f64,
}

/// Shared, TTL-bounded, background-refreshed, sync-readable free-model-list
/// cache. See the module doc comment and ADR-001 for the design rationale.
pub struct ModelListCache {
    /// Single-entry cache (key is the unit type — there is exactly one
    /// free-model list to cache) so TTL eviction and `invalidate()` come
    /// from `moka` for free instead of being hand-rolled.
    cache: moka::sync::Cache<(), Arc<Vec<FreeModelEntry>>>,
    /// Wall-clock timestamp of the last *successful* refresh — `None` if no
    /// refresh has ever succeeded. `DateTime<Utc>` (not `Instant`), since
    /// Epic 5.1's `observability_snapshot()` surfaces this as an RFC3339
    /// string (`design/ux.md`'s `openrouter_scoring.cache.last_refresh`
    /// example) — a wall clock, not a monotonic clock, is the only thing
    /// that can produce a calendar timestamp.
    last_refresh: Mutex<Option<DateTime<Utc>>>,
    /// The reason the cache entry was last cleared, surfaced via
    /// `/metrics` (Observability Plan) — e.g. `"model_not_found:<id>"` or
    /// `"suppressed_systemic_404"`.
    last_invalidation_reason: Mutex<Option<String>>,
    /// Distinct model ids that 404'd recently, pruned to `NOT_FOUND_WINDOW`
    /// on every call — the minority-vs-systemic signal (Story 2.1.3).
    recent_not_found: DashMap<String, Instant>,
    /// Back-reference to the owning provider, set via `Arc::new_cyclic` in
    /// `OpenrouterProvider::new()` — lets a real (non-systemic) 404
    /// invalidation trigger its own on-demand refetch (Story 2.1.3)
    /// without the `RoutingStrategy`/`Router` orchestrating it. `Weak`, not
    /// `Arc`, so the cache doesn't keep its own provider alive after a
    /// route hot-swap orphans it (mirrors the background refresh task's
    /// lifecycle, Story 2.1.2).
    provider: Weak<OpenrouterProvider>,
    /// Single-flight guard for `trigger_immediate_refresh` (Story 2.1.3,
    /// adversarial-review Blocker 4). `Arc`-wrapped rather than a bare
    /// `AtomicBool` (a small, deliberate refinement of plan.md Task
    /// 2.1.1b's literal field type): the task spawned by
    /// `trigger_immediate_refresh` must reset this flag on *every* exit
    /// path, including a `Weak<OpenrouterProvider>` upgrade failure — and
    /// on that path there is no other way back to `self` from inside a
    /// `'static` spawned future. Cloning the `Arc<AtomicBool>` into the
    /// spawned task solves that without a self-referential
    /// `Weak<ModelListCache>`.
    refresh_in_flight: Arc<AtomicBool>,
}

impl ModelListCache {
    /// Construct a new, empty cache for `provider` — called from inside
    /// `OpenrouterProvider::new()`'s `Arc::new_cyclic` closure (Story
    /// 2.1.2), so `provider` is that closure's `Weak<Self>`.
    pub(crate) fn new(provider: Weak<OpenrouterProvider>) -> Self {
        Self {
            cache: moka::sync::Cache::builder()
                .max_capacity(1)
                .time_to_live(MODEL_LIST_TTL)
                .build(),
            last_refresh: Mutex::new(None),
            last_invalidation_reason: Mutex::new(None),
            recent_not_found: DashMap::new(),
            provider,
            refresh_in_flight: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Test-only constructor with a caller-supplied TTL, mirroring
    /// `SessionCostStore::new_with_ttl`'s precedent
    /// (`src/cost_metrics/store.rs:165-175`) — lets a TTL-expiry test use a
    /// near-zero duration instead of waiting out the real 15-minute
    /// default. Has no real `OpenrouterProvider` to hold a `Weak` to
    /// (`Weak::new()` never upgrades) — fine for every test that doesn't
    /// exercise `trigger_immediate_refresh`'s success path.
    #[cfg(test)]
    pub(crate) fn new_with_ttl(ttl: Duration) -> Self {
        Self {
            cache: moka::sync::Cache::builder()
                .max_capacity(1)
                .time_to_live(ttl)
                .build(),
            last_refresh: Mutex::new(None),
            last_invalidation_reason: Mutex::new(None),
            recent_not_found: DashMap::new(),
            provider: Weak::new(),
            refresh_in_flight: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The last successfully fetched free-model list, or `None` if no
    /// refresh has ever succeeded or the entry has expired past
    /// `MODEL_LIST_TTL` (Story 2.1.1).
    #[must_use]
    pub fn snapshot(&self) -> Option<Arc<Vec<FreeModelEntry>>> {
        self.cache.get(&())
    }

    /// The wall-clock timestamp of the last successful refresh, or `None` if
    /// none has ever succeeded — surfaced via `/metrics` (Observability
    /// Plan) as an RFC3339 string.
    #[must_use]
    pub fn last_refresh(&self) -> Option<DateTime<Utc>> {
        *self
            .last_refresh
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The reason the cache was last cleared, or `None` if it has never
    /// been invalidated — surfaced via `/metrics` (Observability Plan).
    #[must_use]
    pub fn last_invalidation_reason(&self) -> Option<String> {
        self.last_invalidation_reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_last_invalidation_reason(&self, reason: String) {
        *self
            .last_invalidation_reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reason);
    }

    /// Re-fetches `provider.list_free_models()` and replaces the cached
    /// snapshot on success, recording `last_refresh`.
    ///
    /// On `Err`, leaves the existing cache entry (if any) completely
    /// untouched — it neither clears nor replaces it, so a
    /// previously-populated, not-yet-expired entry keeps serving stale
    /// data until the TTL naturally expires it, rather than being cleared
    /// by a transient fetch error. This is Story 2.1.1's explicit, tested
    /// acceptance criterion (adversarial-review Concern: "serve stale
    /// until TTL" was previously implicit, not a stated criterion or
    /// test) — the early `?` below is itself the guard: no cache mutation
    /// happens before it.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `provider.list_free_models()` fails.
    pub async fn refresh(&self, provider: &OpenrouterProvider) -> anyhow::Result<()> {
        let entries = provider.list_free_models().await?;
        self.cache.insert((), Arc::new(entries));
        *self
            .last_refresh
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Utc::now());
        Ok(())
    }

    /// Clears the cached snapshot immediately, without waiting for the next
    /// scheduled refresh.
    pub fn invalidate(&self) {
        self.cache.invalidate(&());
    }

    /// Records a model-not-found (404) signal and, unless it looks
    /// systemic, invalidates the cache and triggers an immediate on-demand
    /// refetch so a genuinely stale entry self-heals well within the
    /// 5-minute periodic refresh interval (Story 2.1.3).
    ///
    /// Distinguishes:
    /// - a minority of the cached list's models 404ing within the last
    ///   `NOT_FOUND_WINDOW` (60s) — genuine staleness: invalidate + refetch.
    /// - *every* cached model 404ing within that window — the account-wide
    ///   data-policy toggle (`research/pitfalls.md`) — suppressed, not
    ///   invalidated, since refetching would not fix it.
    /// - a cached list of exactly 1 model (adversarial-review Blocker 3):
    ///   the general minority-vs-systemic comparison (`distinct_failed <
    ///   cached_count`) is mathematically unsatisfiable at `cached_count ==
    ///   1` (`1 < 1` is always false), so this size is always treated as
    ///   genuine staleness — the only way a single-model pool can ever
    ///   self-heal.
    /// - an empty/never-populated cache (`cached_count == 0`): nothing to
    ///   compare against, so this is a safe no-op — no invalidation, no
    ///   systemic-suppression log, no refetch trigger.
    pub fn record_not_found_and_maybe_invalidate(&self, model: &str) {
        let now = Instant::now();
        self.recent_not_found.insert(model.to_string(), now);
        self.recent_not_found
            .retain(|_, seen_at| now.duration_since(*seen_at) < NOT_FOUND_WINDOW);

        let cached_count = self.snapshot().map_or(0, |list| list.len());
        if cached_count == 0 {
            return;
        }

        let distinct_failed = self.recent_not_found.len();
        let is_genuine_staleness = cached_count == 1 || distinct_failed < cached_count;

        if is_genuine_staleness {
            self.cache.invalidate(&());
            self.set_last_invalidation_reason(format!("model_not_found:{model}"));
            self.trigger_immediate_refresh();
        } else {
            self.set_last_invalidation_reason("suppressed_systemic_404".to_string());
            warn!(
                model,
                distinct_failed,
                cached_count,
                "openrouter: suppressing model-list cache invalidation — every cached model \
                 404ing within the last 60s looks like an account-wide data-policy toggle, \
                 not genuine catalog staleness (refetching would not fix it)"
            );
        }
    }

    /// One-shot, fire-and-forget on-demand refresh triggered by a real
    /// (non-systemic) invalidation (Story 2.1.3, adversarial-review Blocker
    /// 4) — without this, a stale single model would sit at `Exhausted`
    /// until the next periodic 5-minute tick instead of self-healing
    /// immediately.
    ///
    /// Single-flight: if a refresh is already in flight, a second trigger
    /// is a no-op (adversarial-review Blocker 4's refetch-stampede
    /// concern, `research/pitfalls.md` §4). This doesn't reuse `moka`'s
    /// own single-flight machinery (`sync::Cache::get_with`'s blocking
    /// single-flight doesn't compose with an async HTTP call from a sync
    /// context — the same reason ADR-001 rejected `future::Cache` for this
    /// type); the `AtomicBool` compare-exchange guard is the
    /// sync-compatible equivalent for this one call site.
    fn trigger_immediate_refresh(&self) {
        if self
            .refresh_in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        let weak_provider = self.provider.clone();
        let in_flight = Arc::clone(&self.refresh_in_flight);
        tokio::spawn(async move {
            if let Some(provider) = weak_provider.upgrade() {
                let cache = provider.model_cache();
                if let Err(e) = cache.refresh(&provider).await {
                    warn!(error = %e, "openrouter: on-demand model-list refresh failed");
                }
            }
            in_flight.store(false, Ordering::Release);
        });
    }
}

#[cfg(test)]
impl ModelListCache {
    /// Test-only helper: seeds the cache with a fixed snapshot without
    /// going through `refresh()`, which requires a live `OpenrouterProvider`
    /// and network access.
    pub(crate) fn seed_for_test(&self, entries: Vec<FreeModelEntry>) {
        self.cache.insert((), Arc::new(entries));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::atomic::Ordering;

    use crate::config::schema::{AuthMethod, Upstream, UpstreamKind};

    use super::*;

    fn entry(id: &str) -> FreeModelEntry {
        FreeModelEntry {
            id: id.to_string(),
            price_prompt: 0.0,
            price_completion: 0.0,
        }
    }

    // REQ-3 (Story 2.1.1, Task 2.1.1c) — `snapshot()`'s None/Some contract.

    #[test]
    fn snapshot_should_return_none_before_first_refresh_and_some_after() {
        let cache = ModelListCache::new_with_ttl(MODEL_LIST_TTL);
        assert!(cache.snapshot().is_none());

        cache.seed_for_test(vec![entry("a/b:free"), entry("c/d:free")]);

        let snapshot = cache.snapshot().expect("snapshot should be populated");
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0].id, "a/b:free");
        assert_eq!(snapshot[1].id, "c/d:free");
    }

    #[test]
    fn snapshot_should_return_none_after_ttl_expires() {
        let cache = ModelListCache::new_with_ttl(Duration::from_millis(1));
        cache.seed_for_test(vec![entry("a/b:free")]);
        assert!(cache.snapshot().is_some());

        std::thread::sleep(Duration::from_millis(20));

        assert!(
            cache.snapshot().is_none(),
            "entry should have expired past its 1ms TTL"
        );
    }

    #[test]
    fn invalidate_should_clear_entry_immediately() {
        let cache = ModelListCache::new_with_ttl(MODEL_LIST_TTL);
        cache.seed_for_test(vec![entry("a/b:free")]);
        assert!(cache.snapshot().is_some());

        cache.invalidate();

        assert!(cache.snapshot().is_none());
    }

    // REQ-3 (Story 2.1.1, Task 2.1.1d) — `refresh()`'s failure-leaves-cache-
    // untouched behavior. No wiremock/mockito in this repo (established
    // deviation, see `mod.rs`'s test module doc comment) and `list_free_models()`
    // hits a hardcoded `BASE_URL`, so failure is induced hermetically via a
    // broken `AuthMethod::Exec` (a subprocess spawn for a nonexistent binary
    // fails synchronously in `build_headers`, before any network I/O is
    // attempted) rather than a mock HTTP server.

    fn broken_auth_provider() -> OpenrouterProvider {
        OpenrouterProvider::test_provider_with_upstream(Upstream {
            name: "test-openrouter-broken-auth".to_string(),
            kind: UpstreamKind::Openrouter {},
            auth: Some(AuthMethod::Exec {
                command: "/nonexistent-binary-xyz-consolette-test".to_string(),
                args: vec![],
                cache_ttl_secs: 0,
                timeout_secs: 1,
            }),
        })
    }

    #[tokio::test]
    async fn refresh_should_leave_populated_cache_untouched_on_failure() {
        let cache = ModelListCache::new_with_ttl(MODEL_LIST_TTL);
        cache.seed_for_test(vec![entry("a/b:free")]);
        let provider = broken_auth_provider();

        let result = cache.refresh(&provider).await;

        assert!(result.is_err());
        assert_eq!(
            cache.snapshot().expect("cache should be untouched"),
            Arc::new(vec![entry("a/b:free")])
        );
    }

    #[tokio::test]
    async fn refresh_should_leave_empty_cache_as_none_on_failure() {
        let cache = ModelListCache::new_with_ttl(MODEL_LIST_TTL);
        let provider = broken_auth_provider();

        let result = cache.refresh(&provider).await;

        assert!(result.is_err());
        assert!(cache.snapshot().is_none());
    }

    // REQ-3 (Story 2.1.3, Task 2.1.3c) — minority-vs-systemic invalidation.
    // The real-invalidation branch spawns an on-demand refresh (Blocker 4),
    // so these tests run inside a Tokio runtime.

    #[tokio::test]
    async fn record_not_found_and_maybe_invalidate_should_invalidate_on_minority_404() {
        let cache = ModelListCache::new_with_ttl(MODEL_LIST_TTL);
        cache.seed_for_test(vec![
            entry("a/b:free"),
            entry("c/d:free"),
            entry("e/f:free"),
            entry("g/h:free"),
            entry("i/j:free"),
        ]);

        cache.record_not_found_and_maybe_invalidate("a/b:free");

        assert!(cache.snapshot().is_none(), "cache should be invalidated");
        assert_eq!(
            cache.last_invalidation_reason(),
            Some("model_not_found:a/b:free".to_string())
        );
    }

    #[tokio::test]
    async fn record_not_found_and_maybe_invalidate_should_suppress_systemic_404() {
        // Simulates two 404s already inside the 60s window by the time the
        // decision is evaluated (matching the production scenario: two
        // concurrent dispatches to different models both landing in
        // `recent_not_found` before either's check runs to conclusion) by
        // directly seeding the first failure into `recent_not_found`
        // (same-module test access), then driving the single call whose
        // decision matters. Calling the public method twice in strict
        // sequence instead would have the first call's own real-invalidation
        // branch already clear the cache before the second call runs,
        // never reaching the systemic case at all.
        let cache = ModelListCache::new_with_ttl(MODEL_LIST_TTL);
        cache.seed_for_test(vec![entry("a/b:free"), entry("c/d:free")]);
        cache
            .recent_not_found
            .insert("a/b:free".to_string(), Instant::now());

        cache.record_not_found_and_maybe_invalidate("c/d:free");

        assert!(
            cache.snapshot().is_some(),
            "systemic 404s must not invalidate"
        );
        assert_eq!(
            cache.last_invalidation_reason(),
            Some("suppressed_systemic_404".to_string())
        );
    }

    // Blocker 3 (adversarial-review): `cached_count == 1` always invalidates.

    #[tokio::test]
    async fn record_not_found_and_maybe_invalidate_should_always_invalidate_when_cached_count_is_one(
    ) {
        let cache = ModelListCache::new_with_ttl(MODEL_LIST_TTL);
        cache.seed_for_test(vec![entry("only/model:free")]);

        cache.record_not_found_and_maybe_invalidate("only/model:free");

        assert!(cache.snapshot().is_none());
        assert_eq!(
            cache.last_invalidation_reason(),
            Some("model_not_found:only/model:free".to_string()),
            "a single-model pool's only failure must be genuine staleness, never suppressed_systemic_404"
        );
    }

    #[tokio::test]
    async fn record_not_found_and_maybe_invalidate_should_prune_stale_404_entries() {
        let cache = ModelListCache::new_with_ttl(MODEL_LIST_TTL);
        cache.seed_for_test(vec![
            entry("a/b:free"),
            entry("c/d:free"),
            entry("e/f:free"),
        ]);
        cache.recent_not_found.insert(
            "stale/model:free".to_string(),
            Instant::now()
                .checked_sub(Duration::from_secs(90))
                .expect("subtracting 90s from now should not underflow"),
        );

        cache.record_not_found_and_maybe_invalidate("a/b:free");

        // Only the current failure counts (1 < 3) once the 90s-old entry is
        // pruned — if it weren't pruned, distinct_failed would be 2, still
        // < 3, so this assertion alone wouldn't distinguish the two cases;
        // the pruning itself is asserted directly below.
        assert!(cache.snapshot().is_none());
        assert!(
            !cache.recent_not_found.contains_key("stale/model:free"),
            "entries older than NOT_FOUND_WINDOW must be pruned"
        );
    }

    #[test]
    fn record_not_found_and_maybe_invalidate_should_noop_when_cache_is_empty() {
        let cache = ModelListCache::new_with_ttl(MODEL_LIST_TTL);

        cache.record_not_found_and_maybe_invalidate("a/b:free");

        assert!(cache.snapshot().is_none());
        assert_eq!(
            cache.last_invalidation_reason(),
            None,
            "an empty cache has nothing to invalidate or suppress"
        );
    }

    // Blocker 4 (adversarial-review): real invalidation triggers an
    // immediate on-demand refetch; the single-flight guard resets on every
    // exit path, including a `Weak<OpenrouterProvider>` upgrade failure.

    #[tokio::test]
    async fn trigger_immediate_refresh_should_reset_flag_when_provider_unavailable() {
        // `new_with_ttl` holds `Weak::new()` (never upgrades), so the
        // spawned task's `weak_provider.upgrade()` deterministically fails
        // — exercising the "upgrade failure" exit path hermetically.
        let cache = ModelListCache::new_with_ttl(MODEL_LIST_TTL);
        cache.seed_for_test(vec![entry("only/model:free")]);

        cache.record_not_found_and_maybe_invalidate("only/model:free");
        assert!(
            cache.refresh_in_flight.load(Ordering::Acquire),
            "flag should be set synchronously before the spawned task runs"
        );

        // Let the spawned task run to completion.
        for _ in 0..100 {
            if !cache.refresh_in_flight.load(Ordering::Acquire) {
                break;
            }
            tokio::task::yield_now().await;
        }

        assert!(
            !cache.refresh_in_flight.load(Ordering::Acquire),
            "flag must be reset even when the provider Weak fails to upgrade"
        );
    }

    #[tokio::test]
    async fn trigger_immediate_refresh_should_be_a_noop_when_already_in_flight() {
        let cache = ModelListCache::new_with_ttl(MODEL_LIST_TTL);
        cache.refresh_in_flight.store(true, Ordering::Release);

        // Calling the private method directly (same-module test access) —
        // a second trigger while one is already in flight must not panic
        // and must leave the flag untouched (a real second spawn would
        // eventually flip it back to false itself).
        cache.trigger_immediate_refresh();

        assert!(
            cache.refresh_in_flight.load(Ordering::Acquire),
            "a no-op trigger must not have spawned a task that could reset the flag"
        );
    }
}
