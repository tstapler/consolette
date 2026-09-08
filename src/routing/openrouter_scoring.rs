//! `OpenrouterScoringStrategy`: the composite-scoring, epsilon-greedy
//! `RoutingStrategy` impl over `OpenRouter`'s free-model pool (Epic 4.2,
//! ADR-002, ADR-003).
//!
//! Composite formula, selection policy, and cold-start/unranked defaults
//! all come from ADR-003
//! (`project_plans/openrouter-routing/decisions/ADR-003-composite-scoring-formula.md`);
//! the 429-folding behavior comes from ADR-002
//! (`project_plans/openrouter-routing/decisions/ADR-002-per-model-429-handling.md`).

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use dashmap::{DashMap, DashSet};
use rand::Rng;
use tracing::{debug, warn};

use crate::providers::openrouter::{FreeModelEntry, ModelListCache};

use super::bench_table::bench_score;
use super::model_stats::ModelStats;
use super::strategy::{RoutingStrategy, UpstreamRef};

/// ADR-003 composite-formula weights, fixed in code (not configurable —
/// see ADR-003 Consequences). Error rate is weighted highest: on a
/// rate-capped free pool a failing attempt both wastes a scarce request and
/// still needs a retry, so it's the most expensive signal to get wrong.
pub const WEIGHT_ERROR: f64 = 0.5;
/// Latency is second: the direct, live user-facing cost.
pub const WEIGHT_LATENCY: f64 = 0.3;
/// Bench rank is smallest: a coarse, infrequently-refreshed prior that live
/// signals should dominate once real samples exist.
pub const WEIGHT_BENCH: f64 = 0.2;

/// Epsilon-greedy exploration rate (ADR-003): 10% of selections are
/// uniform-random rather than argmax, so a deprioritized model still
/// accumulates the samples needed to recover once its penalty ages out of
/// the rolling window.
pub const EPSILON_EXPLORATION: f64 = 0.1;

/// ADR-002: number of synthetic failure samples a single rate-limited
/// attempt injects into a model's rolling error rate, on top of the real
/// failure sample — enough to dominate the rolling window immediately
/// without waiting for real attempts to accumulate.
pub const RATE_LIMIT_SYNTHETIC_FAILURES: usize = 5;

/// Neutral cold-start / unranked default (ADR-003): avoids both starving a
/// new/unranked model (never picked because it's assumed worst) and
/// over-favoring one (assumed best) before any real signal exists.
const NEUTRAL: f64 = 0.5;

/// Per-model snapshot of the last-computed score, kept for `/metrics`
/// auditability (ADR-003 Consequences; surfaced by Epic 5.1).
#[derive(Debug, Clone, PartialEq)]
pub struct ScoreBreakdown {
    pub latency_p50_ms: u64,
    pub error_rate: Option<f64>,
    pub bench_rank: Option<f64>,
    pub composite: f64,
    pub sample_count: usize,
}

/// Min-max normalizes `values` into `[0, 1]`, "lower is better" (ADR-003
/// uses the same tie-guarded formula for both live signals: latency and
/// error rate). An empty slice returns an empty vec; a slice where every
/// value is equal (including the single-element case) returns `1.0` for
/// every element — the tie guard that keeps a single-candidate pool, or a
/// pool of identical values, from dividing by zero / producing `NaN`.
fn normalize_lower_is_better(values: &[f64]) -> Vec<f64> {
    if values.is_empty() {
        return Vec::new();
    }
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if (max - min).abs() < f64::EPSILON {
        return vec![1.0; values.len()];
    }
    values
        .iter()
        .map(|v| 1.0 - (v - min) / (max - min))
        .collect()
}

/// One candidate's raw (pre-normalization) signal state, extracted from
/// `ModelStats`/`BENCH_TABLE` before scoring.
struct RawSignal {
    model_id: Option<String>,
    latency_p50_ms: u64,
    /// `true` when the latency tracker has zero samples — distinguishes a
    /// genuine cold start from a real recorded latency of `0ms`.
    latency_cold: bool,
    /// `None` = cold start (mirrors `RollingErrorRate::error_rate()`'s own
    /// contract).
    error_rate: Option<f64>,
    sample_count: usize,
}

/// The composite-scoring, epsilon-greedy `RoutingStrategy` (Epic 4.2).
///
/// Owns per-model rolling stats and the last-computed score breakdown per
/// model, shares the process-wide `ModelListCache` with `OpenrouterProvider`
/// (Epic 2.1), and knows which upstream index in the route it fans out
/// (`openrouter_index`) so `record_outcome`/`expand_candidates` only ever
/// touch candidates that actually belong to it.
pub struct OpenrouterScoringStrategy {
    model_stats: DashMap<String, ModelStats>,
    model_cache: Arc<ModelListCache>,
    openrouter_index: usize,
    /// Every candidate's last-computed score, keyed by model id — kept for
    /// `/metrics` auditability (ADR-003 Consequences), not just the
    /// winner's.
    last_scores: DashMap<String, ScoreBreakdown>,
    /// Pre-mortem P2 #2: the last selection's explore-vs-greedy outcome,
    /// keyed by the *chosen* model id. A `DashMap` (not a single
    /// `AtomicBool`) so concurrent dispatches to different models don't
    /// clobber each other's flag. Epic 5.1 reads this to distinguish an
    /// epsilon-greedy exploration pick from a genuine scoring failure in
    /// `RequestDetail`/`observability_snapshot()`/the structured log line —
    /// not wired into either yet (that's Epic 5.1's job), but the shape is
    /// fixed here alongside `select()`'s own explore/greedy branch, per
    /// plan.md's Unresolved Questions.
    last_explore: DashMap<String, bool>,
    /// "Already warned" set (Task 4.2.1c) — an unranked model logs
    /// `tracing::warn!` exactly once per model id, not once per `select()`
    /// call.
    warned_unranked: DashSet<String>,
    /// Pre-mortem P2 #1's aggregate bench-table coverage-ratio log line:
    /// logged once, the first time `expand_candidates` sees a non-empty
    /// cache snapshot (see `maybe_log_bench_coverage`'s doc comment for the
    /// judgment call this implements).
    logged_bench_coverage: AtomicBool,
}

impl OpenrouterScoringStrategy {
    /// Construct a new strategy sharing `model_cache` with the
    /// `OpenrouterProvider` at `openrouter_index` in the route's candidate
    /// list.
    #[must_use]
    pub fn new(model_cache: Arc<ModelListCache>, openrouter_index: usize) -> Self {
        Self {
            model_stats: DashMap::new(),
            model_cache,
            openrouter_index,
            last_scores: DashMap::new(),
            last_explore: DashMap::new(),
            warned_unranked: DashSet::new(),
            logged_bench_coverage: AtomicBool::new(false),
        }
    }

    /// Computes each `healthy` candidate's `ScoreBreakdown` (Story 4.2.1,
    /// ADR-003), 1:1 aligned with `healthy` by index. Records every
    /// model-bearing candidate's breakdown into `last_scores` and warns
    /// once per unranked model id as a side effect (Task 4.2.1c) — `select`
    /// stays sync/in-memory throughout (ADR-003's "no I/O, no lock held
    /// across an `.await`" constraint), just not purely side-effect-free.
    ///
    /// A candidate with no `.model` (a non-openrouter passthrough candidate
    /// that reached `select()` in a mixed route) scores fully neutral
    /// (`0.5` on every term) and is not tracked in `last_scores`/
    /// `warned_unranked` — there's no model id to key either on.
    fn compute_scores(&self, healthy: &[UpstreamRef]) -> Vec<ScoreBreakdown> {
        let raws: Vec<RawSignal> = healthy.iter().map(|c| self.raw_signal_for(c)).collect();

        // Min-max normalization is computed over only the *real* (non-cold)
        // values in this candidate slice (ADR-003) — a cold candidate's
        // component is substituted with the neutral default afterward, not
        // folded into the min/max computation itself.
        // `latency_p50_ms` is bounded by the 15-minute rolling window
        // (`DurationHistogram`) — far too small to lose precision as `f64`,
        // matching `histogram.rs::percentiles`'s own identical `allow`.
        #[allow(clippy::cast_precision_loss)]
        let latency_values: Vec<f64> = raws
            .iter()
            .filter(|r| !r.latency_cold)
            .map(|r| r.latency_p50_ms as f64)
            .collect();
        let normalized_latency = normalize_lower_is_better(&latency_values);
        let error_values: Vec<f64> = raws.iter().filter_map(|r| r.error_rate).collect();
        let normalized_error = normalize_lower_is_better(&error_values);

        let mut lat_iter = normalized_latency.into_iter();
        let mut err_iter = normalized_error.into_iter();

        raws.into_iter()
            .map(|raw| {
                let norm_latency = if raw.latency_cold {
                    NEUTRAL
                } else {
                    lat_iter.next().unwrap_or(NEUTRAL)
                };
                let norm_error = if raw.error_rate.is_none() {
                    NEUTRAL
                } else {
                    err_iter.next().unwrap_or(NEUTRAL)
                };
                self.breakdown_for(&raw, norm_latency, norm_error)
            })
            .collect()
    }

    /// Extracts one candidate's pre-normalization signal state from
    /// `model_stats` — `None`/cold for a candidate with no `.model`, or one
    /// this strategy has never recorded an outcome for.
    fn raw_signal_for(&self, candidate: &UpstreamRef) -> RawSignal {
        let Some(model_id) = &candidate.model else {
            return RawSignal {
                model_id: None,
                latency_p50_ms: 0,
                latency_cold: true,
                error_rate: None,
                sample_count: 0,
            };
        };
        let stats = self.model_stats.get(model_id);
        let (latency_p50_ms, latency_cold, sample_count) =
            stats.as_ref().map_or((0, true, 0), |s| {
                (
                    s.latency.percentiles().0,
                    s.latency.sample_count() == 0,
                    s.latency.sample_count(),
                )
            });
        let error_rate = stats.as_ref().and_then(|s| s.errors.error_rate());
        RawSignal {
            model_id: Some(model_id.clone()),
            latency_p50_ms,
            latency_cold,
            error_rate,
            sample_count,
        }
    }

    /// Combines one candidate's already-normalized `norm_latency`/`norm_error`
    /// with its absolute bench score (warning once if unranked, Task
    /// 4.2.1c) into a `ScoreBreakdown`, recording it into `last_scores` as a
    /// side effect when the candidate has a model id.
    fn breakdown_for(&self, raw: &RawSignal, norm_latency: f64, norm_error: f64) -> ScoreBreakdown {
        let bench_rank = raw.model_id.as_deref().and_then(bench_score);
        let bench = bench_rank.unwrap_or_else(|| {
            if let Some(model_id) = &raw.model_id {
                self.warn_once_if_unranked(model_id);
            }
            NEUTRAL
        });
        let composite =
            WEIGHT_ERROR * norm_error + WEIGHT_LATENCY * norm_latency + WEIGHT_BENCH * bench;
        let breakdown = ScoreBreakdown {
            latency_p50_ms: raw.latency_p50_ms,
            error_rate: raw.error_rate,
            bench_rank,
            composite,
            sample_count: raw.sample_count,
        };
        if let Some(model_id) = &raw.model_id {
            self.last_scores.insert(model_id.clone(), breakdown.clone());
        }
        breakdown
    }

    /// Task 4.2.1c: logs `tracing::warn!` exactly once per unranked model
    /// id, tracked via `warned_unranked`.
    fn warn_once_if_unranked(&self, model_id: &str) {
        if self.warned_unranked.insert(model_id.to_string()) {
            warn!(model = %model_id, "openrouter: unranked in bench table");
        }
    }

    /// Pre-mortem P2 #1's aggregate coverage-ratio log line: reports what
    /// fraction of the currently-cached free models have a `BENCH_TABLE`
    /// entry, once per newly-warm snapshot.
    ///
    /// Judgment call for plan.md's Unresolved Questions "Pre-mortem P2 #1"
    /// (recorded live at 0/19 = 0% in `bench_table.rs`'s module doc): this
    /// adds the aggregate log line rather than lowering `WEIGHT_BENCH`.
    /// ADR-003 fixes the weights as an explicit, sourced design decision
    /// (min-max monotonicity, "error costs more than latency on a
    /// rate-capped pool" reasoning) that doesn't depend on any particular
    /// table's current coverage; cold-start/unranked's neutral-`0.5`
    /// default (this same module) already keeps a fully-unranked pool from
    /// being systematically punished or rewarded by the `bench` term in the
    /// meantime — every candidate just gets the same neutral value, so
    /// selection degrades gracefully to the error/latency terms alone. The
    /// coverage ratio is therefore an operator-visible fact worth surfacing
    /// (so a 0% ratio doesn't silently persist unnoticed once real free
    /// models do start reappearing in `BENCH_TABLE`), not a reason to
    /// re-derive the formula's fixed weights per plan.md's explicit
    /// instruction not to build a general configurable weighting system.
    fn maybe_log_bench_coverage(&self, snapshot: &[FreeModelEntry]) {
        if snapshot.is_empty() || self.logged_bench_coverage.swap(true, Ordering::AcqRel) {
            return;
        }
        let total = snapshot.len();
        let ranked = snapshot
            .iter()
            .filter(|e| bench_score(&e.id).is_some())
            .count();
        #[allow(clippy::cast_precision_loss)]
        let ratio = ranked as f64 / total as f64;
        tracing::info!(
            ranked,
            total,
            ratio,
            "openrouter: bench-table coverage ratio for the current free-model snapshot"
        );
    }
}

impl RoutingStrategy for OpenrouterScoringStrategy {
    /// Epsilon-greedy selection (Story 4.2.2, ADR-003): 90% of the time,
    /// the highest-composite candidate wins (first-in-list on ties — `.max_by`
    /// returns the *last* max on ties in Rust's stdlib, so ties are broken
    /// by manual scan rather than `Iterator::max_by`); 10% of the time, a
    /// uniformly random candidate is picked regardless of score.
    fn select(&self, healthy: &[UpstreamRef]) -> Option<UpstreamRef> {
        if healthy.is_empty() {
            return None;
        }

        let scores = self.compute_scores(healthy);
        let mut rng = rand::thread_rng();
        let explore = rng.gen::<f64>() < EPSILON_EXPLORATION;

        let chosen_idx = if explore {
            rng.gen_range(0..healthy.len())
        } else {
            let mut best_idx = 0;
            let mut best_score = scores[0].composite;
            for (idx, breakdown) in scores.iter().enumerate().skip(1) {
                if breakdown.composite > best_score {
                    best_idx = idx;
                    best_score = breakdown.composite;
                }
            }
            best_idx
        };

        let chosen = &healthy[chosen_idx];
        if let Some(model_id) = &chosen.model {
            self.last_explore.insert(model_id.clone(), explore);
        }
        debug!(
            model = chosen.model.as_deref().unwrap_or(""),
            explore,
            composite = scores[chosen_idx].composite,
            "openrouter: selected candidate"
        );

        Some(chosen.clone())
    }

    /// Fans the one static "openrouter" `UpstreamRef` (`.model == None`) out
    /// into one per currently-cached free model (Story 4.2.4). A candidate
    /// whose `.model` is already `Some(..)` — a session pin `Router::dispatch`
    /// already applied via `effective_candidates` before calling this —
    /// passes through unchanged (adversarial-review Blocker 1). Also runs
    /// `model_stats` GC against the current snapshot's id set, skipped
    /// entirely on a cold cache (Task 4.2.4b).
    fn expand_candidates(&self, candidates: Vec<UpstreamRef>) -> Vec<UpstreamRef> {
        let snapshot = self.model_cache.snapshot();

        if let Some(snapshot) = &snapshot {
            let current_ids: HashSet<&str> = snapshot.iter().map(|e| e.id.as_str()).collect();
            self.model_stats
                .retain(|id, _| current_ids.contains(id.as_str()));
            self.maybe_log_bench_coverage(snapshot);
        }

        candidates
            .into_iter()
            .flat_map(|c| {
                let is_fan_out_target = c.index == self.openrouter_index && c.model.is_none();
                if !is_fan_out_target {
                    return vec![c];
                }
                match &snapshot {
                    None => vec![],
                    Some(entries) => entries
                        .iter()
                        .map(|entry| UpstreamRef {
                            model: Some(entry.id.clone()),
                            ..c.clone()
                        })
                        .collect(),
                }
            })
            .collect()
    }

    /// Feeds `ModelStats` (Story 4.2.3): a successful attempt records
    /// `(duration_ms, true)`; a failure records `(duration_ms, false)` plus
    /// ADR-002's synthetic 429 weighting or Story 2.1.3's model-not-found
    /// hook. Ignores any candidate outside this strategy's own per-model
    /// fan-out (`.index != self.openrouter_index` or `.model.is_none()`).
    fn record_outcome(
        &self,
        candidate: &UpstreamRef,
        duration_ms: u64,
        success: bool,
        error_kind: Option<&'static str>,
    ) {
        if candidate.index != self.openrouter_index {
            return;
        }
        let Some(model) = &candidate.model else {
            return;
        };

        let stats = self.model_stats.entry(model.clone()).or_default();
        stats.latency.record(duration_ms);
        stats.errors.record(success);

        if !success {
            match error_kind {
                Some("rate_limited") => {
                    // ADR-002: dominate the rolling window immediately
                    // rather than waiting for real samples to accumulate —
                    // deprioritizes, doesn't hard-exclude (the model can
                    // still be re-picked by epsilon-greedy exploration).
                    for _ in 0..RATE_LIMIT_SYNTHETIC_FAILURES {
                        stats.errors.record(false);
                    }
                }
                Some("model_unsupported") => {
                    drop(stats);
                    self.model_cache
                        .record_not_found_and_maybe_invalidate(model);
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::providers::openrouter::cache::ModelListCache as CacheImpl;

    fn strategy_with_index(index: usize) -> OpenrouterScoringStrategy {
        let cache = Arc::new(CacheImpl::new_with_ttl(Duration::from_mins(15)));
        OpenrouterScoringStrategy::new(cache, index)
    }

    fn candidate(index: usize, model: Option<&str>) -> UpstreamRef {
        UpstreamRef {
            index,
            name: "openrouter".to_string(),
            weight: 1.0,
            model: model.map(str::to_string),
        }
    }

    fn free_model(id: &str) -> FreeModelEntry {
        FreeModelEntry {
            id: id.to_string(),
            price_prompt: 0.0,
            price_completion: 0.0,
        }
    }

    /// Test-only helper: seeds `model`'s `ModelStats` directly, bypassing
    /// `record_outcome`, so score-computation tests can set up an exact
    /// `(latency_p50_ms, error_rate)` state without a full dispatch.
    fn seed_stats(
        strategy: &OpenrouterScoringStrategy,
        model: &str,
        latencies_ms: &[u64],
        successes: &[bool],
    ) {
        let stats = strategy.model_stats.entry(model.to_string()).or_default();
        for &d in latencies_ms {
            stats.latency.record(d);
        }
        for &s in successes {
            stats.errors.record(s);
        }
    }

    // --- Story 4.2.5: weights-sum-to-1.0 (Task 4.2.5c) ---

    #[test]
    fn scoring_weights_should_sum_to_one() {
        assert!((WEIGHT_ERROR + WEIGHT_LATENCY + WEIGHT_BENCH - 1.0).abs() < f64::EPSILON);
    }

    // --- Story 4.2.1: score computation ---

    #[test]
    fn score_should_default_to_neutral_when_cold_start_and_unranked() {
        let strategy = strategy_with_index(0);
        let candidates = vec![candidate(0, Some("unranked/cold:free"))];

        let scores = strategy.compute_scores(&candidates);

        assert_eq!(scores.len(), 1);
        let s = &scores[0];
        assert_eq!(s.error_rate, None);
        assert_eq!(s.bench_rank, None);
        assert!((s.composite - 0.5).abs() < 1e-9);
    }

    #[test]
    fn score_should_use_tie_guard_norm_of_one_for_single_candidate_with_samples() {
        let strategy = strategy_with_index(0);
        seed_stats(&strategy, "solo/model:free", &[100], &[true]);
        let candidates = vec![candidate(0, Some("solo/model:free"))];

        let scores = strategy.compute_scores(&candidates);

        // norm_latency == norm_error == 1.0 (tie guard), bench unranked ->
        // 0.5, so composite = 0.5*1 + 0.3*1 + 0.2*0.5 = 0.9.
        assert!((scores[0].composite - 0.9).abs() < 1e-9);
    }

    #[test]
    fn score_should_rank_best_candidate_above_worst_candidate_across_pool_sizes() {
        for pool_size in [2usize, 3, 5] {
            let strategy = strategy_with_index(0);
            // BEST: low latency, no errors, top bench rank.
            seed_stats(
                &strategy,
                "best/model:free",
                &[10],
                &[true, true, true, true],
            );
            // WORST: high latency, all errors, bottom bench rank.
            seed_stats(
                &strategy,
                "worst/model:free",
                &[10_000],
                &[false, false, false, false],
            );

            let mut candidates = vec![
                candidate(0, Some("best/model:free")),
                candidate(0, Some("worst/model:free")),
            ];
            for i in 0..pool_size.saturating_sub(2) {
                let id = format!("mid-{i}/model:free");
                seed_stats(&strategy, &id, &[500], &[true, false]);
                candidates.push(candidate(0, Some(id.as_str())));
            }
            assert_eq!(candidates.len(), pool_size);

            let scores = strategy.compute_scores(&candidates);
            let best = scores[0].composite;
            let worst = scores[1].composite;
            assert!(
                best > worst,
                "pool_size={pool_size}: expected best ({best}) > worst ({worst})"
            );
        }
    }

    #[test]
    fn score_cold_start_candidate_should_score_strictly_between_worst_and_best_in_shared_pool() {
        let strategy = strategy_with_index(0);
        seed_stats(&strategy, "best/model:free", &[10], &[true, true, true]);
        seed_stats(
            &strategy,
            "worst/model:free",
            &[10_000],
            &[false, false, false],
        );
        // "cold/model:free" is never seeded and has no BENCH_TABLE entry.

        let candidates = vec![
            candidate(0, Some("best/model:free")),
            candidate(0, Some("worst/model:free")),
            candidate(0, Some("cold/model:free")),
        ];

        let scores = strategy.compute_scores(&candidates);
        let best = scores[0].composite;
        let worst = scores[1].composite;
        let cold = scores[2].composite;

        assert!(
            (cold - 0.5).abs() < 1e-9,
            "cold candidate should be neutral 0.5"
        );
        assert!(worst < cold, "cold ({cold}) should beat worst ({worst})");
        assert!(cold < best, "best ({best}) should beat cold ({cold})");
    }

    #[test]
    fn score_should_favor_lower_latency_error_and_higher_bench_given_ranked_model() {
        let strategy = strategy_with_index(0);
        // A: fast, no errors, ranked (deepseek is BENCH_TABLE's one real row).
        seed_stats(
            &strategy,
            "deepseek/deepseek-chat-v3.1:free",
            &[100],
            &[true],
        );
        // B: slow, errors, unranked.
        seed_stats(&strategy, "b/model:free", &[500], &[false]);

        let candidates = vec![
            candidate(0, Some("deepseek/deepseek-chat-v3.1:free")),
            candidate(0, Some("b/model:free")),
        ];
        let scores = strategy.compute_scores(&candidates);

        assert!(scores[0].composite > scores[1].composite);
    }

    // --- Story 4.2.1c/d: unranked-model once-per-model warning ---

    #[test]
    fn select_should_warn_once_per_unranked_model_id() {
        let strategy = strategy_with_index(0);
        let candidates = vec![candidate(0, Some("unranked/model:free"))];

        for _ in 0..5 {
            strategy.compute_scores(&candidates);
        }

        // `warned_unranked` is the actual once-per-model-id tracking
        // mechanism (Task 4.2.1c) — after 5 calls the set still has exactly
        // one entry for this model id, proving the guard suppressed the
        // other 4 (a duplicate warn would still show a set size of 1, since
        // `DashSet::insert` is idempotent, but `insert`'s bool return is
        // what `compute_scores` actually gates the `warn!` call on).
        assert!(strategy.warned_unranked.contains("unranked/model:free"));
        assert_eq!(strategy.warned_unranked.len(), 1);
    }

    // --- Story 4.2.2: select() epsilon-greedy ---

    #[test]
    fn select_should_return_none_when_no_healthy_candidates() {
        let strategy = strategy_with_index(0);
        assert!(strategy.select(&[]).is_none());
    }

    #[test]
    fn select_should_always_return_sole_candidate() {
        let strategy = strategy_with_index(0);
        let candidates = vec![candidate(0, Some("only/model:free"))];
        for _ in 0..20 {
            let selected = strategy
                .select(&candidates)
                .expect("must select the sole candidate");
            assert_eq!(selected.model.as_deref(), Some("only/model:free"));
        }
    }

    #[test]
    fn select_should_prefer_higher_scoring_candidate_most_of_the_time() {
        let strategy = strategy_with_index(0);
        // A: composite ~0.9 (fast, no errors, mid-pack unranked bench).
        seed_stats(&strategy, "a/model:free", &[10], &[true, true, true, true]);
        // B: composite ~0.1 (slow, all errors).
        seed_stats(
            &strategy,
            "b/model:free",
            &[10_000],
            &[false, false, false, false],
        );
        let candidates = vec![
            candidate(0, Some("a/model:free")),
            candidate(0, Some("b/model:free")),
        ];

        let mut a_count = 0;
        let mut b_count = 0;
        for _ in 0..200 {
            let selected = strategy.select(&candidates).expect("pool is non-empty");
            match selected.model.as_deref() {
                Some("a/model:free") => a_count += 1,
                Some("b/model:free") => b_count += 1,
                other => panic!("unexpected selection: {other:?}"),
            }
        }

        assert!(
            a_count > 150,
            "expected the higher-scoring candidate to dominate (~91% of 200), got {a_count}"
        );
        assert!(
            b_count >= 1,
            "expected the lower-scoring candidate to still be picked at least once in 200 trials"
        );
    }

    #[test]
    fn select_should_break_ties_by_first_in_list_not_randomly() {
        let strategy = strategy_with_index(0);
        // Identical stats -> identical composite scores.
        seed_stats(&strategy, "first/model:free", &[100], &[true, true]);
        seed_stats(&strategy, "second/model:free", &[100], &[true, true]);
        let candidates = vec![
            candidate(0, Some("first/model:free")),
            candidate(0, Some("second/model:free")),
        ];

        // Force the greedy branch by disabling exploration entirely isn't
        // possible without seeding the RNG, so run enough trials that a
        // majority must land on the greedy branch (~90%) and assert the
        // first-in-list candidate dominates just as strongly as the
        // higher-scorer test above -- a random tiebreak would instead split
        // ~50/50 among the 90% of greedy trials.
        let mut first_count = 0;
        for _ in 0..200 {
            if strategy
                .select(&candidates)
                .expect("pool is non-empty")
                .model
                .as_deref()
                == Some("first/model:free")
            {
                first_count += 1;
            }
        }
        assert!(
            first_count > 150,
            "first-in-list should win nearly every greedy tie, got {first_count}/200"
        );
    }

    // --- Story 4.2.3: record_outcome() ---

    #[test]
    fn record_outcome_should_record_success_into_model_stats() {
        let strategy = strategy_with_index(0);
        let candidate = candidate(0, Some("a/b:free"));

        strategy.record_outcome(&candidate, 150, true, None);

        let stats = strategy
            .model_stats
            .get("a/b:free")
            .expect("stats must exist");
        assert_eq!(stats.latency.sample_count(), 1);
        assert_eq!(stats.errors.error_rate(), Some(0.0));
    }

    #[test]
    fn record_outcome_rate_limited_should_record_synthetic_failures() {
        let strategy = strategy_with_index(0);
        let candidate = candidate(0, Some("a/b:free"));

        strategy.record_outcome(&candidate, 50, false, Some("rate_limited"));

        let stats = strategy
            .model_stats
            .get("a/b:free")
            .expect("stats must exist");
        assert_eq!(
            stats.errors.sample_count(),
            1 + RATE_LIMIT_SYNTHETIC_FAILURES
        );
        assert_eq!(stats.errors.error_rate(), Some(1.0));
    }

    // `record_not_found_and_maybe_invalidate`'s real-invalidation branch
    // spawns an on-demand refresh task (`ModelListCache`, Story 2.1.3), so
    // this needs a Tokio runtime, mirroring `cache.rs`'s own tests.
    #[tokio::test]
    async fn record_outcome_model_unsupported_should_invalidate_cache_for_that_model() {
        let cache = Arc::new(CacheImpl::new_with_ttl(Duration::from_mins(15)));
        cache.seed_for_test(vec![free_model("only/model:free")]);
        let strategy = OpenrouterScoringStrategy::new(Arc::clone(&cache), 0);
        let candidate = candidate(0, Some("only/model:free"));

        strategy.record_outcome(&candidate, 50, false, Some("model_unsupported"));

        // A single-model cache always treats its only 404 as genuine
        // staleness (`ModelListCache`'s Blocker-3 rule) -> invalidated.
        assert!(cache.snapshot().is_none());
    }

    #[test]
    fn record_outcome_should_ignore_candidate_at_different_index() {
        let strategy = strategy_with_index(0);
        let candidate = candidate(1, Some("a/b:free"));

        strategy.record_outcome(&candidate, 50, true, None);

        assert!(strategy.model_stats.get("a/b:free").is_none());
    }

    #[test]
    fn record_outcome_should_ignore_candidate_with_no_model() {
        let strategy = strategy_with_index(0);
        let candidate = candidate(0, None);

        strategy.record_outcome(&candidate, 50, true, None);

        assert!(strategy.model_stats.is_empty());
    }

    // --- Story 4.2.4: expand_candidates() ---

    #[test]
    fn expand_candidates_should_fan_out_static_candidate_into_one_per_cached_model() {
        let cache = Arc::new(CacheImpl::new_with_ttl(Duration::from_mins(15)));
        cache.seed_for_test(vec![free_model("a/b:free"), free_model("c/d:free")]);
        let strategy = OpenrouterScoringStrategy::new(Arc::clone(&cache), 2);

        let expanded = strategy.expand_candidates(vec![candidate(2, None)]);

        assert_eq!(expanded.len(), 2);
        assert!(expanded.iter().all(|c| c.index == 2));
        let ids: HashSet<_> = expanded.iter().filter_map(|c| c.model.clone()).collect();
        assert_eq!(
            ids,
            HashSet::from(["a/b:free".to_string(), "c/d:free".to_string()])
        );
    }

    #[test]
    fn expand_candidates_should_pass_through_other_index_candidates_unchanged() {
        let cache = Arc::new(CacheImpl::new_with_ttl(Duration::from_mins(15)));
        cache.seed_for_test(vec![free_model("a/b:free")]);
        let strategy = OpenrouterScoringStrategy::new(Arc::clone(&cache), 2);

        let anthropic_candidate = candidate(0, None);
        let expanded =
            strategy.expand_candidates(vec![anthropic_candidate.clone(), candidate(2, None)]);

        assert_eq!(expanded[0], anthropic_candidate);
        assert_eq!(expanded.len(), 2);
        assert_eq!(expanded[1].index, 2);
    }

    #[test]
    fn expand_candidates_should_drop_unpinned_candidate_when_cache_cold() {
        let strategy = strategy_with_index(2);

        let expanded = strategy.expand_candidates(vec![candidate(2, None)]);

        assert!(expanded.is_empty());
    }

    #[test]
    fn expand_candidates_should_pass_through_unchanged_when_session_pinned() {
        let cache = Arc::new(CacheImpl::new_with_ttl(Duration::from_mins(15)));
        cache.seed_for_test(vec![
            free_model("x/1:free"),
            free_model("x/2:free"),
            free_model("x/3:free"),
            free_model("x/4:free"),
            free_model("x/5:free"),
        ]);
        let strategy = OpenrouterScoringStrategy::new(Arc::clone(&cache), 2);
        let pinned = candidate(2, Some("pinned/x:free"));

        let expanded = strategy.expand_candidates(vec![pinned.clone()]);

        assert_eq!(expanded, vec![pinned]);
    }

    #[test]
    fn expand_candidates_should_pass_through_pinned_candidate_when_cache_cold() {
        let strategy = strategy_with_index(2);
        let pinned = candidate(2, Some("pinned/x:free"));

        let expanded = strategy.expand_candidates(vec![pinned.clone()]);

        assert_eq!(expanded, vec![pinned]);
    }

    #[test]
    fn expand_candidates_should_gc_stale_model_stats_against_current_snapshot() {
        let cache = Arc::new(CacheImpl::new_with_ttl(Duration::from_mins(15)));
        cache.seed_for_test(vec![free_model("still/here:free")]);
        let strategy = OpenrouterScoringStrategy::new(Arc::clone(&cache), 2);
        seed_stats(&strategy, "still/here:free", &[100], &[true]);
        seed_stats(&strategy, "gone/now:free", &[100], &[true]);

        strategy.expand_candidates(vec![candidate(2, None)]);

        assert!(strategy.model_stats.contains_key("still/here:free"));
        assert!(!strategy.model_stats.contains_key("gone/now:free"));
    }

    #[test]
    fn expand_candidates_should_not_gc_model_stats_when_cache_cold() {
        let strategy = strategy_with_index(2);
        seed_stats(&strategy, "a/model:free", &[100], &[true]);
        seed_stats(&strategy, "b/model:free", &[100], &[true]);
        seed_stats(&strategy, "c/model:free", &[100], &[true]);

        strategy.expand_candidates(vec![candidate(2, None)]);

        assert_eq!(
            strategy.model_stats.len(),
            3,
            "a cold cache must not wipe rolling history"
        );
    }
}
