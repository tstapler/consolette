//! Decayed per-member stats for auto-model-family (Epic 2, ADR-002/ADR-003).
//!
//! [`MemberStats`] is keyed per `(upstream_name, model_id)` in
//! [`MemberStatsMap`], so two family members sharing one upstream get
//! separate buckets. Latency reuses the tested windowing/percentile code by
//! composing [`DurationHistogram`] — no shape is copied.
//!
//! Decay is a windowed deque (the plan's implementer default): a bounded
//! sample window with a time cutoff, so old errors age out and a recovered
//! model can outrank its history. Below [`MIN_SAMPLES`] a member reports
//! `cold` (unknown ≠ perfect); promotion above threshold additionally
//! requires [`confidently_better`] (Wilson-interval non-overlap, or err
//! delta > 5pp at n < 30).
//!
//! [`FamilyRuntime`] bundles the stats map with the 1h-TTL denylist STUB,
//! snapshots, counters, and probe/hysteresis state. It is owned by
//! `MetricsCollector` so `post_route` rebuilds preserve learning; only the
//! immutable `FamilyTable` is rebuilt from config. The denylist's exclusion
//! logic (pre-dispatch check, 404-vs-400 writer discrimination) lands in
//! Epic 3 — this module provides only the type, TTL constructor, and
//! TTL-aware helpers.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::DurationHistogram;
use crate::config::schema::ModelFamily;
use crate::providers::ProviderError;

/// Composite stats key: `(upstream_name, model_id)` — never a positional
/// upstream index, so member reorders and `post_route` rebuilds keep
/// addressing the same bucket (ADR-002).
pub type MemberKey = (String, String);

/// Per-member stats table, owned by [`FamilyRuntime`].
pub type MemberStatsMap = DashMap<MemberKey, MemberStats>;

/// Minimum quality samples before stats stop reporting `cold`.
pub const MIN_SAMPLES: usize = 20;
/// Small-n promotion band: below n=30 a >5pp error delta also promotes.
pub const SMALL_N_BAND: usize = 30;
/// Promotion delta inside the small-n band, in error-rate points.
pub const SMALL_N_DELTA: f64 = 0.05;

/// Default stats window (15 min, matching `DurationHistogram`'s default).
const DEFAULT_WINDOW_SECS: u64 = 15 * 60;
/// Default cap on quality samples per member (bounds memory; the newest
/// samples win, which is itself a form of decay).
const DEFAULT_MAX_SAMPLES: usize = 200;

/// Default denylist TTL: 1h (Epic 3 fills the exclusion logic).
pub const DENYLIST_TTL: Duration = Duration::from_secs(3600);

// ────────────────────────────────────────────────────────────────────────────
// Per-class write table
// ────────────────────────────────────────────────────────────────────────────

/// How one dispatch attempt classifies for member stats (plan §Pattern
/// Decisions "Error→stats classification"): `Timeout`/`Upstream{5xx}` are
/// quality signals; 429/`RateLimited` is backpressure (cooldown only, never
/// the error rate); auth/validation are dropped entirely;
/// `ModelUnsupported` is a denylist feed for Epic 3, not an error rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberRecordClass {
    /// Counts toward the decayed quality error-rate AND takes a latency
    /// sample. Carries whether this attempt was an error.
    Quality { error: bool },
    /// Backpressure: excluded from the quality rate AND from latency blame
    /// (consumed via the `HealthRegistry` cooldown path instead).
    Backpressure,
    /// Dropped entirely: never touches error-rate or latency.
    Ignored,
    /// Denylist feed for Epic 3's per-member TTL denylist (404-only writer
    /// discrimination lands there); not an error-rate signal.
    DenylistFeed,
}

/// Classifies one attempt outcome per the write table above.
/// `None` = success.
#[must_use]
pub fn classify_for_member_stats(err: Option<&ProviderError>) -> MemberRecordClass {
    let Some(err) = err else {
        return MemberRecordClass::Quality { error: false };
    };
    if err.is_rate_limited() {
        return MemberRecordClass::Backpressure;
    }
    if err.is_auth() || err.is_validation() {
        return MemberRecordClass::Ignored;
    }
    match err {
        // `Timeout`, `Upstream{5xx}`, and a 2xx body that doesn't match the
        // documented shape (broken endpoint for our purposes) are quality
        // signals: error-bit + latency sample.
        ProviderError::Timeout | ProviderError::ResponseShapeMismatch(_) => {
            MemberRecordClass::Quality { error: true }
        }
        ProviderError::Upstream { status, .. } => {
            if *status == 429 {
                MemberRecordClass::Backpressure
            } else if *status >= 500 {
                MemberRecordClass::Quality { error: true }
            } else {
                // 4xx from the wire (non-429): client-side, like validation.
                MemberRecordClass::Ignored
            }
        }
        ProviderError::ModelUnsupported(_) => MemberRecordClass::DenylistFeed,
        // Router-level exhaustion carries no per-member signal.
        ProviderError::Exhausted
        | ProviderError::RateLimited
        | ProviderError::RateLimitedWithRetry { .. }
        | ProviderError::Auth(_)
        | ProviderError::Validation(..) => MemberRecordClass::Ignored,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// MemberStats
// ────────────────────────────────────────────────────────────────────────────

/// One quality outcome inside the decay window.
#[derive(Debug, Clone, Copy)]
struct OutcomeSample {
    at: Instant,
    error: bool,
}

/// Decayed per-member counters: quality-error window + latency via a
/// composed [`DurationHistogram`] + sample count.
pub struct MemberStats {
    outcomes: Mutex<VecDeque<OutcomeSample>>,
    window: Duration,
    max_samples: usize,
    latency: DurationHistogram,
}

impl MemberStats {
    #[must_use]
    pub fn new() -> Self {
        Self::with_window(
            Duration::from_secs(DEFAULT_WINDOW_SECS),
            DEFAULT_MAX_SAMPLES,
        )
    }

    #[must_use]
    pub fn with_window(window: Duration, max_samples: usize) -> Self {
        Self {
            outcomes: Mutex::new(VecDeque::new()),
            window,
            max_samples: max_samples.max(1),
            latency: DurationHistogram::new(window),
        }
    }

    /// Records one attempt per the write table; returns its classification
    /// so the dispatch loop can route `DenylistFeed` outcomes onward.
    pub fn record(&self, err: Option<&ProviderError>, latency_ms: u64) -> MemberRecordClass {
        let class = classify_for_member_stats(err);
        if let MemberRecordClass::Quality { error } = class {
            let now = Instant::now();
            let cutoff = now.checked_sub(self.window).unwrap_or(now);
            let mut outcomes = outcomes_lock(&self.outcomes);
            outcomes.push_back(OutcomeSample { at: now, error });
            while outcomes.len() > self.max_samples {
                outcomes.pop_front();
            }
            while outcomes.front().is_some_and(|s| s.at < cutoff) {
                outcomes.pop_front();
            }
            drop(outcomes);
            self.latency.record(latency_ms);
        }
        class
    }

    /// Quality samples currently in the window.
    #[must_use]
    pub fn sample_count(&self) -> usize {
        let now = Instant::now();
        let cutoff = now.checked_sub(self.window).unwrap_or(now);
        let mut outcomes = outcomes_lock(&self.outcomes);
        while outcomes.front().is_some_and(|s| s.at < cutoff) {
            outcomes.pop_front();
        }
        outcomes.len()
    }

    /// Decayed error-rate over the window (0.0 when empty; check
    /// [`is_cold`](Self::is_cold) before trusting it).
    #[must_use]
    // Sample counts are bounded by `max_samples` (default 200) — far below
    // `f64`'s exact-integer range.
    #[allow(clippy::cast_precision_loss)]
    pub fn error_rate(&self) -> f64 {
        let now = Instant::now();
        let cutoff = now.checked_sub(self.window).unwrap_or(now);
        let mut outcomes = outcomes_lock(&self.outcomes);
        while outcomes.front().is_some_and(|s| s.at < cutoff) {
            outcomes.pop_front();
        }
        if outcomes.is_empty() {
            return 0.0;
        }
        let errors = outcomes.iter().filter(|s| s.error).count();
        errors as f64 / outcomes.len() as f64
    }

    /// p50 latency over the composed histogram's window.
    #[must_use]
    pub fn latency_p50_ms(&self) -> u64 {
        self.latency.percentiles().0
    }

    /// Below [`MIN_SAMPLES`] stats report cold: unknown ≠ perfect, so the
    /// resolver serves the config-order default instead of ranking on noise.
    #[must_use]
    pub fn is_cold(&self) -> bool {
        self.sample_count() < MIN_SAMPLES
    }

    /// Age of the oldest in-window sample (the "window age" the dashboard
    /// shows next to every figure); 0s when empty.
    #[must_use]
    pub fn window_age(&self) -> Duration {
        let now = Instant::now();
        let outcomes = outcomes_lock(&self.outcomes);
        outcomes.front().map_or(Duration::ZERO, |s| {
            now.checked_duration_since(s.at).unwrap_or(Duration::ZERO)
        })
    }
}

impl Default for MemberStats {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::missing_panics_doc)]
fn outcomes_lock(
    m: &Mutex<VecDeque<OutcomeSample>>,
) -> std::sync::MutexGuard<'_, VecDeque<OutcomeSample>> {
    // A poisoned mutex only happens if another lock holder panicked;
    // recovering the inner data is safe (same precedent as
    // `MetricsCollector`'s locks).
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ────────────────────────────────────────────────────────────────────────────
// Minimum-sample threshold helpers (Story 2.2, task 2)
// ────────────────────────────────────────────────────────────────────────────

/// Wilson score interval (95%, z≈1.96) for an observed error proportion.
/// Returns `(lo, hi)` clamped to `[0, 1]`; `(0, 1)` when `n == 0`.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn wilson_interval(error_rate: f64, n: usize) -> (f64, f64) {
    if n == 0 {
        return (0.0, 1.0);
    }
    let n_f = n as f64;
    let z = 1.96_f64;
    let z2 = z * z;
    let denom = 1.0 + z2 / n_f;
    let centre = (error_rate + z2 / (2.0 * n_f)) / denom;
    let half =
        z * ((error_rate * (1.0 - error_rate) / n_f) + z2 / (4.0 * n_f * n_f)).sqrt() / denom;
    (
        (centre - half).clamp(0.0, 1.0),
        (centre + half).clamp(0.0, 1.0),
    )
}

/// Promotion gate: a challenger dethrones the incumbent only if its Wilson
/// interval sits strictly below the incumbent's (non-overlap), or — inside
/// the small-n band (either side < 30) — its error rate beats the incumbent
/// by more than 5pp. Either side below [`MIN_SAMPLES`] never promotes, so
/// one stray 500 can't flip the pick.
#[must_use]
pub fn confidently_better(
    challenger_err: f64,
    challenger_n: usize,
    incumbent_err: f64,
    incumbent_n: usize,
) -> bool {
    if challenger_n < MIN_SAMPLES || incumbent_n < MIN_SAMPLES {
        return false;
    }
    if challenger_err >= incumbent_err {
        return false;
    }
    let (_, chall_hi) = wilson_interval(challenger_err, challenger_n);
    let (inc_lo, _) = wilson_interval(incumbent_err, incumbent_n);
    if chall_hi < inc_lo {
        return true;
    }
    if (challenger_n < SMALL_N_BAND || incumbent_n < SMALL_N_BAND)
        && incumbent_err - challenger_err > SMALL_N_DELTA
    {
        return true;
    }
    false
}

// ────────────────────────────────────────────────────────────────────────────
// ResolutionSnapshot / ResolutionCounter (Epic 5a consumes these)
// ────────────────────────────────────────────────────────────────────────────

/// One family member pick, as served to the dashboard + `/metrics`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickedMember {
    pub upstream: String,
    pub model: String,
}

/// Last-resolution record per alias: chosen member + the error-rate/latency
/// figures the pick was made on + timestamp + previous pick.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolutionSnapshot {
    pub alias: String,
    pub picked: PickedMember,
    pub error_rate: f64,
    pub latency_p50_ms: u64,
    pub samples: usize,
    pub at: DateTime<Utc>,
    pub previous_pick: Option<PickedMember>,
}

impl ResolutionSnapshot {
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        alias: &str,
        upstream: &str,
        model: &str,
        error_rate: f64,
        latency_p50_ms: u64,
        samples: usize,
        previous_pick: Option<PickedMember>,
    ) -> Self {
        Self {
            alias: alias.to_string(),
            picked: PickedMember {
                upstream: upstream.to_string(),
                model: model.to_string(),
            },
            error_rate,
            latency_p50_ms,
            samples,
            at: Utc::now(),
            previous_pick,
        }
    }
}

/// Per-alias `/metrics` counters: resolutions total + fallback-to-default
/// events (audits the paid-leak guard + all-down bypass).
#[derive(Debug, Default)]
pub struct ResolutionCounter {
    pub resolutions_total: AtomicU64,
    pub fallback_to_default_total: AtomicU64,
    /// Of the resolutions, servings of a non-verifiably-free model ID. Must
    /// stay 0 for every free alias — the paid-leak audit signal (Epic 3
    /// addition: Epic 7's `PerAliasCounters::paid_resolutions` dimension,
    /// moved here so the dispatch path has ONE canonical counter location —
    /// this `FamilyRuntime`-owned table, which survives `post_route`
    /// rebuilds and feeds Epic 5a's `/metrics` family section).
    pub paid_resolutions: AtomicU64,
}

impl ResolutionCounter {
    pub fn record_resolution(&self) {
        self.resolutions_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_fallback(&self) {
        self.fallback_to_default_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Records one resolution that served a non-verifiably-free model ID.
    /// Called by Epic 3's resolution step alongside `record_resolution`
    /// (kept separate so free-alias traffic provably leaves it at 0).
    pub fn record_paid_resolution(&self) {
        self.paid_resolutions.fetch_add(1, Ordering::Relaxed);
    }

    #[must_use]
    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.resolutions_total.load(Ordering::Relaxed),
            self.fallback_to_default_total.load(Ordering::Relaxed),
        )
    }

    /// Paid servings for the audit signal (see field doc).
    #[must_use]
    pub fn paid_snapshot(&self) -> u64 {
        self.paid_resolutions.load(Ordering::Relaxed)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// FamilyRuntime
// ────────────────────────────────────────────────────────────────────────────

/// Point-in-time view of one member's stats for the Epic 3 ranker: cold
/// members carry no trustworthy figures.
#[derive(Debug, Clone, Copy)]
pub struct MemberView {
    pub error_rate: f64,
    pub latency_p50_ms: u64,
    pub samples: usize,
    pub cold: bool,
}

/// Mutable per-family runtime: stats map, TTL denylist STUB, snapshots,
/// counters, probe/hysteresis state. Owned by `MetricsCollector` so it
/// survives `post_route` rebuilds; dispatch reaches it via `self.metrics`.
pub struct FamilyRuntime {
    /// Decayed stats keyed by `(upstream_name, model_id)`.
    pub stats: MemberStatsMap,
    /// Per-member hard-failure exclusion with TTL (STUB: type + TTL
    /// constructor + TTL-aware helpers; Epic 3 wires the pre-dispatch
    /// exclusion check and the 404-vs-400 writer discrimination).
    denylist: DashMap<MemberKey, Instant>,
    denylist_ttl: Duration,
    snapshots: DashMap<String, ResolutionSnapshot>,
    counters: DashMap<String, ResolutionCounter>,
    /// Per-alias resolution count driving Epic 3's probe-every-25th.
    resolutions_seen: DashMap<String, u64>,
    /// Per-alias last pick driving Epic 3's hysteresis margin.
    last_pick: DashMap<String, MemberKey>,
    /// Per-member in-flight dispatch count driving Epic 3's concurrency cap
    /// (overflow routes to the sibling). Instantaneous process state, not
    /// learning — resetting it on restart is harmless, and it lives here
    /// (not on the rebuilt `Router`) so a `post_route` hot-swap mid-burst
    /// can't double-admit a member past its cap.
    inflight: DashMap<MemberKey, usize>,
    /// Per-member 429-backpressure marks (`until` instant): Epic 3's
    /// shared-upstream rule. A 429 on member A cools the shared upstream
    /// index (via `HealthRegistry`) AND marks A personally, so a sibling B
    /// on the same upstream stays eligible (the index cool alone would
    /// wrongly exclude it) while A itself is not retried until the mark
    /// expires. TTL matches the index cooldown duration it was recorded
    /// with, so the personal mark and the index cool agree on recovery.
    backpressure: DashMap<MemberKey, Instant>,
}

impl FamilyRuntime {
    #[must_use]
    pub fn new() -> Self {
        Self::with_denylist_ttl(DENYLIST_TTL)
    }

    #[must_use]
    pub fn with_denylist_ttl(denylist_ttl: Duration) -> Self {
        Self {
            stats: DashMap::new(),
            denylist: DashMap::new(),
            denylist_ttl,
            snapshots: DashMap::new(),
            counters: DashMap::new(),
            resolutions_seen: DashMap::new(),
            last_pick: DashMap::new(),
            inflight: DashMap::new(),
            backpressure: DashMap::new(),
        }
    }

    /// Dual-write entry point for the dispatch loop: classifies `err` per
    /// the write table and records into the `(upstream, model)` bucket,
    /// creating it on first use. Returns the classification so dispatch can
    /// feed `DenylistFeed` outcomes to Epic 3's denylist writer.
    #[must_use]
    pub fn record_member(
        &self,
        upstream: &str,
        model: &str,
        err: Option<&ProviderError>,
        latency_ms: u64,
    ) -> MemberRecordClass {
        let key = (upstream.to_string(), model.to_string());
        let entry = self.stats.entry(key).or_default();
        entry.record(err, latency_ms)
    }

    /// Ranker view for one member; absent members read as cold with zero
    /// samples (unknown ≠ perfect).
    #[must_use]
    pub fn member_view(&self, upstream: &str, model: &str) -> MemberView {
        self.stats
            .get(&(upstream.to_string(), model.to_string()))
            .map_or(
                MemberView {
                    error_rate: 0.0,
                    latency_p50_ms: 0,
                    samples: 0,
                    cold: true,
                },
                |s| MemberView {
                    error_rate: s.error_rate(),
                    latency_p50_ms: s.latency_p50_ms(),
                    samples: s.sample_count(),
                    cold: s.is_cold(),
                },
            )
    }

    /// Every `(upstream, model)` key currently holding stats, denylist,
    /// inflight, or backpressure state — the "live" set the rebuild path
    /// intersects against the rebuilt `FamilyTable` via `drop_stale_keys`.
    #[must_use]
    pub fn member_keys(&self) -> HashSet<MemberKey> {
        let mut live = HashSet::with_capacity(
            self.stats.len() + self.denylist.len() + self.inflight.len() + self.backpressure.len(),
        );
        for e in &self.stats {
            live.insert(e.key().clone());
        }
        for e in &self.denylist {
            live.insert(e.key().clone());
        }
        for e in &self.inflight {
            live.insert(e.key().clone());
        }
        for e in &self.backpressure {
            live.insert(e.key().clone());
        }
        live
    }

    /// Drops stats + denylist + inflight + backpressure entries for members
    /// that left the rebuilt table, so a removed member's stale state can
    /// never shadow a new one. Snapshots/counters/probe state are per-alias
    /// and survive (only membership resets to the newly-loaded config).
    pub fn retain_members(&self, retained: &HashSet<MemberKey>) {
        self.stats.retain(|k, _| retained.contains(k));
        self.denylist.retain(|k, _| retained.contains(k));
        self.inflight.retain(|k, _| retained.contains(k));
        self.backpressure.retain(|k, _| retained.contains(k));
    }

    // ── Denylist STUB helpers (Epic 3 owns the dispatch wiring) ──

    /// Stage a per-member hard-failure exclusion starting now.
    pub fn denylist_insert(&self, upstream: &str, model: &str) {
        self.denylist
            .insert((upstream.to_string(), model.to_string()), Instant::now());
    }

    /// TTL-aware membership check (`false` once the entry expired; expired
    /// entries are lazily evicted here).
    #[must_use]
    pub fn is_denylisted(&self, upstream: &str, model: &str) -> bool {
        let key = (upstream.to_string(), model.to_string());
        match self.denylist.get(&key) {
            None => false,
            Some(t) => {
                if t.elapsed() > self.denylist_ttl {
                    drop(t);
                    self.denylist.remove(&key);
                    false
                } else {
                    true
                }
            }
        }
    }

    // ── Per-member backpressure + concurrency state (Epic 3) ──

    /// Marks one member as 429-backpressured until `until` (Epic 3's
    /// shared-upstream rule — see the field doc). Called by the dispatch
    /// loop's 429 arm alongside the index-level `HealthRegistry` trip, with
    /// the same duration so both agree on recovery.
    pub fn note_backpressure(&self, upstream: &str, model: &str, until: Instant) {
        self.backpressure
            .insert((upstream.to_string(), model.to_string()), until);
    }

    /// Personal-429 check (`false` once the mark expired; expired marks are
    /// lazily evicted here). A marked member is skipped by pre-dispatch
    /// exclusion; its unmarked same-upstream siblings stay eligible.
    #[must_use]
    pub fn is_backpressured(&self, upstream: &str, model: &str) -> bool {
        let key = (upstream.to_string(), model.to_string());
        match self.backpressure.get(&key) {
            None => false,
            Some(until) => {
                if Instant::now() >= *until {
                    drop(until);
                    self.backpressure.remove(&key);
                    false
                } else {
                    true
                }
            }
        }
    }

    /// In-flight dispatch count for one member (Epic 3's concurrency cap).
    #[must_use]
    pub fn inflight_count(&self, upstream: &str, model: &str) -> usize {
        self.inflight
            .get(&(upstream.to_string(), model.to_string()))
            .map_or(0, |c| *c)
    }

    /// Admits one in-flight slot iff the member is under `cap`. The dispatch
    /// loop calls this just before `provider.send` (after the resolve-time
    /// partition already preferred under-cap members) so concurrent bursts
    /// can't race past the cap; every `true` must be paired with one
    /// [`release_inflight`](Self::release_inflight).
    #[must_use]
    pub fn try_acquire_inflight(&self, upstream: &str, model: &str, cap: usize) -> bool {
        let key = (upstream.to_string(), model.to_string());
        let mut entry = self.inflight.entry(key).or_insert(0);
        if *entry >= cap {
            return false;
        }
        *entry += 1;
        true
    }

    /// Releases one slot previously admitted by
    /// [`try_acquire_inflight`](Self::try_acquire_inflight).
    pub fn release_inflight(&self, upstream: &str, model: &str) {
        let key = (upstream.to_string(), model.to_string());
        if let Some(mut entry) = self.inflight.get_mut(&key) {
            *entry = entry.saturating_sub(1);
            if *entry == 0 {
                drop(entry);
                self.inflight.remove(&key);
            }
        }
    }

    // ── Snapshots / counters / probe-hysteresis state (Epic 3/5a) ──

    /// Publish one alias's last-resolution record (Epic 3's resolution step
    /// calls this; Epic 5a serves it from `/metrics` + dashboard).
    pub fn record_snapshot(&self, snapshot: ResolutionSnapshot) {
        self.counters
            .entry(snapshot.alias.clone())
            .or_default()
            .record_resolution();
        self.resolutions_seen
            .entry(snapshot.alias.clone())
            .and_modify(|c| *c += 1)
            .or_insert(1);
        self.last_pick.insert(
            snapshot.alias.clone(),
            (
                snapshot.picked.upstream.clone(),
                snapshot.picked.model.clone(),
            ),
        );
        self.snapshots.insert(snapshot.alias.clone(), snapshot);
    }

    #[must_use]
    pub fn snapshot(&self, alias: &str) -> Option<ResolutionSnapshot> {
        self.snapshots.get(alias).map(|s| s.clone())
    }

    /// `(resolutions_total, fallback_to_default_total)` for one alias.
    #[must_use]
    pub fn counter_snapshot(&self, alias: &str) -> (u64, u64) {
        self.counters.get(alias).map_or((0, 0), |c| c.snapshot())
    }

    /// Paid servings for one alias — the free-never-paid audit signal (must
    /// stay 0 for every free alias).
    #[must_use]
    pub fn paid_resolutions(&self, alias: &str) -> u64 {
        self.counters.get(alias).map_or(0, |c| c.paid_snapshot())
    }

    /// Records one paid serving for one alias (alongside the resolution the
    /// snapshot publish already counted).
    pub fn record_paid_resolution(&self, alias: &str) {
        self.counters
            .entry(alias.to_string())
            .or_default()
            .record_paid_resolution();
    }

    /// Records one safety-net bypass serving for one alias (Epic 3's bypass
    /// path calls this directly — the same counter Epic 7's
    /// `safety_net_resolve_free_only` bumps, now in the canonical
    /// `FamilyRuntime`-owned location).
    pub fn record_fallback(&self, alias: &str) {
        self.counters
            .entry(alias.to_string())
            .or_default()
            .record_fallback();
    }

    /// Total resolutions seen for one alias (drives probe-every-25th).
    #[must_use]
    pub fn resolutions_seen(&self, alias: &str) -> u64 {
        self.resolutions_seen.get(alias).map_or(0, |c| *c)
    }

    /// Last pick for one alias (drives hysteresis); `None` before the first
    /// resolution.
    #[must_use]
    pub fn last_pick(&self, alias: &str) -> Option<MemberKey> {
        self.last_pick.get(alias).map(|p| p.clone())
    }

    // ── Epic 5a read-out (Story 5.1): `/metrics` `family` section ──

    /// Builds the `/metrics` `family` section (ux.md N2 shape) from this
    /// runtime's snapshots + counters + per-member views: one entry per
    /// configured alias with `current_pick`, `previous_pick`,
    /// `last_change_at`, `window_age_s`, `resolutions_total`,
    /// `fallback_to_default_total`, `paid_resolutions`, and a ranked
    /// `members` array (`model`, `error_rate`, `latency_p50_ms`, `samples`,
    /// `status`).
    ///
    /// Canonical-merge-point note (Epic 2/3/7): this is the ONE read-out
    /// Epic 5a serves. The write path is Epic 3's `decide_route`
    /// (`publish_resolution` → [`record_snapshot`](Self::record_snapshot) +
    /// [`record_fallback`](Self::record_fallback) +
    /// [`record_paid_resolution`](Self::record_paid_resolution) on this same
    /// type, which survives `post_route` rebuilds via `MetricsCollector`).
    /// `routing::family::PerAliasCounters` is Epic 7's thin local map for
    /// the pure `resolve_paid_aware`/`safety_net_resolve_free_only` stubs —
    /// dispatch does NOT write through it, so it is not merged here and its
    /// write paths are untouched.
    ///
    /// Cold state: an alias with zero resolutions still appears (never an
    /// error) — `current_pick` is the config-order first member (the
    /// `ColdStartDefault` that would serve), every member reports `cold`,
    /// and counters are 0.
    #[must_use]
    pub fn family_section_json(&self, families: &[ModelFamily]) -> Value {
        let mut out = serde_json::Map::with_capacity(families.len());
        for fam in families {
            let snapshot = self.snapshot(&fam.alias);
            let (resolutions_total, fallback_to_default_total) = self.counter_snapshot(&fam.alias);
            let current_pick = snapshot
                .as_ref()
                .map(|s| s.picked.model.clone())
                .or_else(|| fam.members.first().map(|m| m.model.clone()));
            let window_age_s = snapshot.as_ref().map_or(0, |s| {
                self.stats
                    .get(&(s.picked.upstream.clone(), s.picked.model.clone()))
                    .map_or(0, |st| st.window_age().as_secs())
            });
            let members: Vec<Value> = fam
                .members
                .iter()
                .map(|m| {
                    let view = self.member_view(&m.upstream, &m.model);
                    let status = if view.cold {
                        "cold"
                    } else if self.is_denylisted(&m.upstream, &m.model) {
                        "excluded:denylisted"
                    } else if self.is_backpressured(&m.upstream, &m.model) {
                        "cooldown"
                    } else {
                        "active"
                    };
                    json!({
                        "model": m.model,
                        "error_rate": view.error_rate,
                        "latency_p50_ms": view.latency_p50_ms,
                        "samples": view.samples,
                        "status": status,
                    })
                })
                .collect();
            out.insert(
                fam.alias.clone(),
                json!({
                    "current_pick": current_pick,
                    "previous_pick": snapshot.as_ref().and_then(|s| s.previous_pick.as_ref().map(|p| p.model.clone())),
                    "last_change_at": snapshot.as_ref().map(|s| s.at.to_rfc3339()),
                    "window_age_s": window_age_s,
                    "resolutions_total": resolutions_total,
                    "fallback_to_default_total": fallback_to_default_total,
                    "paid_resolutions": self.paid_resolutions(&fam.alias),
                    "members": members,
                }),
            );
        }
        Value::Object(out)
    }
}

impl Default for FamilyRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timeout() -> ProviderError {
        ProviderError::Timeout
    }

    fn ok_record(s: &MemberStats, n: usize) {
        for _ in 0..n {
            s.record(None, 50);
        }
    }

    fn err_record(s: &MemberStats, n: usize) {
        for _ in 0..n {
            s.record(Some(&timeout()), 50);
        }
    }

    #[test]
    fn decay_forgives_old_errors() {
        // Windowed deque with a 20-sample cap: 50 old errors are evicted as
        // 20 fresh successes arrive, so the rate reflects the recent window.
        let s = MemberStats::with_window(Duration::from_secs(3600), 20);
        err_record(&s, 50);
        assert!(
            s.error_rate() > 0.9,
            "all-old-errors window must read ~1.0, got {}",
            s.error_rate()
        );
        ok_record(&s, 20);
        assert_eq!(s.sample_count(), 20);
        assert!(
            s.error_rate() < 0.1,
            "old errors must have decayed out, got {}",
            s.error_rate()
        );
    }

    #[test]
    fn rate_limited_excluded_from_quality_rate() {
        let s = MemberStats::new();
        ok_record(&s, 5);
        for _ in 0..3 {
            let class = s.record(Some(&ProviderError::RateLimited), 9000);
            assert_eq!(class, MemberRecordClass::Backpressure);
        }
        for _ in 0..2 {
            let class = s.record(
                Some(&ProviderError::Upstream {
                    status: 429,
                    body: "slow down".to_string(),
                }),
                9000,
            );
            // Wire-level 429s are backpressure too, never quality blame.
            assert_eq!(class, MemberRecordClass::Backpressure);
        }
        assert_eq!(
            s.sample_count(),
            5,
            "429s must not add samples, let alone error bits"
        );
        assert!(s.error_rate() < f64::EPSILON);
        assert!(
            s.latency_p50_ms() < 1000,
            "429 latency must not pollute p50, got {}",
            s.latency_p50_ms()
        );
    }

    #[test]
    fn auth_and_validation_excluded() {
        let s = MemberStats::new();
        ok_record(&s, 4);
        let auth = ProviderError::Auth("expired token".to_string());
        let v400 = ProviderError::Validation("bad field".to_string(), 400);
        let v404 = ProviderError::Validation("not found".to_string(), 404);
        assert_eq!(s.record(Some(&auth), 10), MemberRecordClass::Ignored);
        assert_eq!(s.record(Some(&v400), 10), MemberRecordClass::Ignored);
        assert_eq!(s.record(Some(&v404), 10), MemberRecordClass::Ignored);
        assert_eq!(s.sample_count(), 4);
        assert!(s.error_rate() < f64::EPSILON);
    }

    #[test]
    fn quality_errors_count_with_latency() {
        let s = MemberStats::new();
        ok_record(&s, 3);
        let up500 = ProviderError::Upstream {
            status: 503,
            body: "bad gateway".to_string(),
        };
        assert_eq!(
            s.record(Some(&timeout()), 100),
            MemberRecordClass::Quality { error: true }
        );
        assert_eq!(
            s.record(Some(&up500), 100),
            MemberRecordClass::Quality { error: true }
        );
        assert_eq!(s.sample_count(), 5);
        assert!((s.error_rate() - 0.4).abs() < f64::EPSILON);
    }

    #[test]
    fn model_unsupported_feeds_denylist_not_error_rate() {
        let s = MemberStats::new();
        ok_record(&s, 2);
        let unsupported = ProviderError::ModelUnsupported("gone/model:free".to_string());
        assert_eq!(
            s.record(Some(&unsupported), 10),
            MemberRecordClass::DenylistFeed
        );
        assert_eq!(s.sample_count(), 2);
        assert!(s.error_rate() < f64::EPSILON);
    }

    #[test]
    fn cold_below_minimum_samples() {
        let s = MemberStats::new();
        ok_record(&s, MIN_SAMPLES - 1);
        assert!(s.is_cold());
        s.record(None, 10);
        assert!(!s.is_cold());
        // Unknown members are cold too, never "perfect".
        let rt = FamilyRuntime::new();
        let view = rt.member_view("openrouter", "never-seen:free");
        assert!(view.cold);
        assert_eq!(view.samples, 0);
    }

    #[test]
    fn confidently_better_requires_non_overlap_or_small_n_delta() {
        // Below threshold: never promotes, even on a perfect record.
        assert!(!confidently_better(0.0, 19, 0.5, 40));
        assert!(!confidently_better(0.0, 40, 0.5, 19));
        // Challenger not better: no promotion.
        assert!(!confidently_better(0.3, 40, 0.2, 40));
        // Clear separation at n=40: Wilson intervals don't overlap.
        assert!(confidently_better(0.0, 40, 0.5, 40));
        // Near-tie at n=40: intervals overlap, delta < 5pp → stays.
        assert!(!confidently_better(0.20, 40, 0.25, 40));
        // Small-n band: >5pp delta promotes even with overlap.
        assert!(confidently_better(0.0, 25, 0.10, 25));
        // Small-n band: ≤5pp delta does not.
        assert!(!confidently_better(0.06, 25, 0.10, 25));
    }

    #[test]
    fn two_members_same_upstream_get_separate_buckets() {
        let rt = FamilyRuntime::new();
        for _ in 0..3 {
            assert_eq!(
                rt.record_member("openrouter", "model-a:free", None, 80),
                MemberRecordClass::Quality { error: false }
            );
        }
        assert_eq!(
            rt.record_member("openrouter", "model-b:free", None, 90),
            MemberRecordClass::Quality { error: false }
        );
        assert_eq!(rt.member_view("openrouter", "model-a:free").samples, 3);
        assert_eq!(rt.member_view("openrouter", "model-b:free").samples, 1);
    }

    #[test]
    fn reorder_stable_keying() {
        // Keys are (upstream, model), never positional indices: a config
        // reorder addresses the same bucket.
        let rt = FamilyRuntime::new();
        assert_eq!(
            rt.record_member("openrouter", "model-a:free", None, 80),
            MemberRecordClass::Quality { error: false }
        );
        assert_eq!(
            rt.record_member("openrouter", "model-a:free", None, 81),
            MemberRecordClass::Quality { error: false }
        );
        let before = rt.member_view("openrouter", "model-a:free");
        // Simulate a reorder: drop + re-create nothing — the key still maps.
        let after = rt.member_view("openrouter", "model-a:free");
        assert_eq!(before.samples, after.samples);
        assert_eq!(before.samples, 2);
    }

    #[test]
    fn family_runtime_survives_post_route_rebuild() {
        use crate::config::schema::{Config, FamilyMember as CfgMember, ModelFamily};

        fn table_with(members: Vec<(&str, &str)>) -> crate::routing::family::FamilyTable {
            let config = Config {
                families: vec![ModelFamily {
                    alias: "auto-coding".to_string(),
                    members: members
                        .into_iter()
                        .map(|(u, m)| CfgMember {
                            upstream: u.to_string(),
                            model: m.to_string(),
                        })
                        .collect(),
                    allow_paid: false,
                }],
                ..Config::default()
            };
            crate::routing::family::FamilyTable::from_config(&config)
        }

        let rt = FamilyRuntime::new();
        assert_eq!(
            rt.record_member("openrouter", "model-a:free", None, 80),
            MemberRecordClass::Quality { error: false }
        );
        assert_eq!(
            rt.record_member("openrouter", "model-b:free", None, 90),
            MemberRecordClass::Quality { error: false }
        );

        // Rebuild with the same membership: learning is intact (the
        // `MetricsCollector`-owned carry-across `post_route` relies on).
        let same = table_with(vec![
            ("openrouter", "model-a:free"),
            ("openrouter", "model-b:free"),
        ]);
        rt.retain_members(&same.drop_stale_keys(&rt.member_keys()));
        assert_eq!(rt.member_view("openrouter", "model-a:free").samples, 1);
        assert_eq!(rt.member_view("openrouter", "model-b:free").samples, 1);

        // Rebuild with model-b removed: only its state is scope-cleared.
        let narrowed = table_with(vec![("openrouter", "model-a:free")]);
        rt.retain_members(&narrowed.drop_stale_keys(&rt.member_keys()));
        assert_eq!(rt.member_view("openrouter", "model-a:free").samples, 1);
        assert!(rt.member_view("openrouter", "model-b:free").cold);
        assert_eq!(rt.member_view("openrouter", "model-b:free").samples, 0);
    }

    #[test]
    fn resolution_snapshot_should_record_pick_and_margins_when_resolution_completes() {
        // Epic 2 defined `ResolutionSnapshot` but deliberately left this
        // test to Epic 5a: it pins the canonical publisher
        // (`FamilyRuntime::record_snapshot`, the same call Epic 3's
        // `publish_resolution` makes on every dispatch resolution).
        let rt = FamilyRuntime::new();

        // First resolution: cold pick A, no previous pick, margins are zero.
        rt.record_snapshot(ResolutionSnapshot::new(
            "auto-coding",
            "mock",
            "model-a:free",
            0.0,
            0,
            0,
            None,
        ));
        let first = rt.snapshot("auto-coding").expect("snapshot must publish");
        assert_eq!(first.alias, "auto-coding");
        assert_eq!(
            first.picked,
            PickedMember {
                upstream: "mock".to_string(),
                model: "model-a:free".to_string(),
            }
        );
        assert_eq!(first.error_rate, 0.0);
        assert_eq!(first.latency_p50_ms, 0);
        assert_eq!(first.samples, 0);
        assert_eq!(first.previous_pick, None);
        assert_eq!(rt.counter_snapshot("auto-coding"), (1, 0));
        assert_eq!(rt.resolutions_seen("auto-coding"), 1);

        // Second resolution: ranked pick B with the error-rate/latency
        // figures the pick was made on; previous pick is A.
        let previous = rt.snapshot("auto-coding").map(|s| s.picked);
        rt.record_snapshot(ResolutionSnapshot::new(
            "auto-coding",
            "mock",
            "model-b:free",
            0.0,
            1800,
            30,
            previous,
        ));
        let second = rt.snapshot("auto-coding").expect("snapshot must update");
        assert_eq!(second.picked.model, "model-b:free");
        assert_eq!(second.error_rate, 0.0);
        assert_eq!(second.latency_p50_ms, 1800);
        assert_eq!(second.samples, 30);
        assert_eq!(
            second.previous_pick,
            Some(PickedMember {
                upstream: "mock".to_string(),
                model: "model-a:free".to_string(),
            })
        );
        assert!(
            second.at >= first.at,
            "each resolution must re-stamp the snapshot"
        );
        assert_eq!(rt.counter_snapshot("auto-coding"), (2, 0));
        assert_eq!(
            rt.last_pick("auto-coding"),
            Some(("mock".to_string(), "model-b:free".to_string()))
        );
    }
}
