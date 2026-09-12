//! `FamilyTable`: the immutable, config-built alias → member map consulted
//! in `Router::dispatch` (auto-model-family Epic 1, ADR-001 resolution
//! site A+C), plus Epic 3's ranked resolution (`FamilyResolver`), the
//! pre-dispatch exclusion decision (`decide_route`), hysteresis, the
//! exploration probe, and the concurrency-cap partition.
//!
//! Split of duties: [`FamilyResolver::rank`] is the pure ordering core
//! (members + stats views + incumbent → ordered list, unit-testable without
//! I/O); [`decide_route`] adds the stateful dispatch wiring around it —
//! paid guard, denylist/backpressure/cooldown exclusion, probe cadence,
//! snapshot/counter publish, and the cooldown-scoped `SafetyNetBypass`. The
//! mutable runtime all of this reads/writes lives in `MetricsCollector`'s
//! `FamilyRuntime` (ADR-003); the table stays immutable and Router-owned.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::config::schema::{Config, FamilyMember};
use crate::metrics::{FamilyRuntime, MemberKey, MemberView, ResolutionSnapshot};

/// Immutable alias table built once per [`Config`] in
/// `Router::from_config` and rebuilt on every `post_route` hot-swap
/// (families ride the conf.d reload, not `RuntimeOverrides::apply`).
#[derive(Debug, Default, Clone)]
pub struct FamilyTable {
    entries: HashMap<String, FamilyEntry>,
}

/// One alias's ordered member list plus its paid flag.
#[derive(Debug, Clone)]
pub struct FamilyEntry {
    /// Declared config order — the cold-start default until Epic 3's
    /// ranker reorders by stats.
    pub members: Vec<FamilyMember>,
    pub allow_paid: bool,
}

impl FamilyTable {
    /// Builds the table from config, preserving declared member order.
    /// Duplicate aliases: last definition wins (matches conf.d
    /// array-replace layering).
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let mut entries = HashMap::with_capacity(config.families.len());
        for family in &config.families {
            entries.insert(
                family.alias.clone(),
                FamilyEntry {
                    members: family.members.clone(),
                    allow_paid: family.allow_paid,
                },
            );
        }
        Self { entries }
    }

    /// Empty table — the `Router::new` default so existing call sites don't
    /// churn; with no entries the dispatch gate never fires.
    #[must_use]
    pub fn empty() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Ordered members for `alias`, or `None` for an unknown alias
    /// (dispatch then leaves the body untouched).
    #[must_use]
    pub fn members(&self, alias: &str) -> Option<&[FamilyMember]> {
        self.entries.get(alias).map(|e| e.members.as_slice())
    }

    /// Epic 1 resolution stub: the config-order first member (Epic 3's
    /// ranker will pick lowest-error then lowest-latency among healthy).
    /// Unknown alias or empty member list → `None` (no expansion).
    #[must_use]
    pub fn resolve_model(&self, alias: &str) -> Option<String> {
        self.members(alias)?.first().map(|m| m.model.clone())
    }

    /// Scope-clear helper for the Epic 3 denylist: drop every
    /// `(upstream, model)` key no longer present in the rebuilt table, so a
    /// removed member's stale exclusion can never shadow a new member.
    /// (No denylist type exists yet in Epic 1 — `FamilyRuntime` lands in
    /// Epic 2 — so this is the pure set-difference core the rebuild path
    /// will call.)
    #[must_use]
    pub fn drop_stale_keys(&self, live: &HashSet<(String, String)>) -> HashSet<(String, String)> {
        let current: HashSet<(String, String)> = self
            .entries
            .values()
            .flat_map(|e| &e.members)
            .map(|m| (m.upstream.clone(), m.model.clone()))
            .collect();
        live.intersection(&current).cloned().collect()
    }

    /// Paid flag for `alias`, or `None` for an unknown alias.
    #[must_use]
    pub fn allow_paid(&self, alias: &str) -> Option<bool> {
        self.entries.get(alias).map(|e| e.allow_paid)
    }

    /// Full entry for `alias`, or `None` for an unknown alias. Epic 3's
    /// [`decide_route`] resolves through this (not the config-order stubs
    /// below) so ranking, hysteresis, and the denylist apply.
    #[must_use]
    pub fn entry(&self, alias: &str) -> Option<&FamilyEntry> {
        self.entries.get(alias)
    }

    /// Paid-aware resolution stub (Epic 7): strips paid IDs out of free
    /// aliases via [`filter_paid_for_free_alias`], resolves the config-order
    /// first survivor, and records the resolution into `counters` (a served
    /// model that is not verifiably free bumps `paid_resolutions`).
    ///
    /// Epic 3's ranker takes over member ordering at this call site; the
    /// guard + counter calls stay — Epic 3 must keep both, not reimplement
    /// them. Unknown alias or empty survivor list → `None` (no expansion).
    pub fn resolve_paid_aware(
        &self,
        alias: &str,
        counters: &mut PerAliasCounters,
    ) -> Option<String> {
        let entry = self.entries.get(alias)?;
        let surviving = filter_paid_for_free_alias(&entry.members, entry.allow_paid);
        let pick = surviving.first()?;
        counters.record_resolution(alias, !is_verifiably_free(&pick.model));
        Some(pick.model.clone())
    }

    /// Safety-net scoped pick — the Epic 7 paid-isolation invariant, exposed
    /// as a guard Epic 3's full `SafetyNetBypass` calls after it has decided
    /// the bypass applies (cooldown/empty-pool scope only; Epic 3 owns the
    /// 429-cooldown and 404/auth/validation exclusions, which never reach
    /// this fn).
    ///
    /// Serves the config-order first *verifiably-free* member of the named
    /// alias ("least-bad free" until Epic 3 feeds ranked order in), bumps
    /// `fallback_to_default_total`, and logs WARN. NEVER returns a paid ID:
    /// the filter applies regardless of `allow_paid`, so even a paid ID
    /// that leaked into the free alias (e.g. via a bad hot-swap past
    /// FreeGuard) cannot be served from here, and the paid alias's pool is
    /// never consulted for a free-alias bypass.
    pub fn safety_net_resolve_free_only(
        &self,
        alias: &str,
        counters: &mut PerAliasCounters,
    ) -> Option<String> {
        let entry = self.entries.get(alias)?;
        let free_only: Vec<&FamilyMember> = entry
            .members
            .iter()
            .filter(|m| is_verifiably_free(&m.model))
            .collect();
        let pick = free_only.first()?;
        tracing::warn!(
            alias = %alias,
            served = %pick.model,
            "SafetyNetBypass: all members unavailable; serving least-bad free member (never paid)"
        );
        counters.record_fallback(alias);
        Some(pick.model.clone())
    }
}

/// FreeGuard-at-runtime: whether `model` is verifiably free. Delegates to
/// the shared [`crate::cost_metrics::pricing::is_free_model_id`] predicate
/// over the cached vendored default (parsed once via `OnceLock`, not per
/// member per request) — the same rule
/// [`crate::config::validate::validate_free_guard`] enforces at load time,
/// so the two can never disagree.
#[must_use]
pub fn is_verifiably_free(model: &str) -> bool {
    crate::cost_metrics::pricing::is_free_model_id(
        model,
        crate::cost_metrics::pricing::vendored_default(),
    )
}

/// Pure paid-isolation guard: the member list Epic 3's resolution/bypass
/// may serve. Paid aliases (`allow_paid = true`) keep every member; free
/// aliases keep only verifiably-free members ([`is_verifiably_free`]).
#[must_use]
pub fn filter_paid_for_free_alias(
    members: &[FamilyMember],
    allow_paid: bool,
) -> Vec<&FamilyMember> {
    if allow_paid {
        return members.iter().collect();
    }
    members
        .iter()
        .filter(|m| is_verifiably_free(&m.model))
        .collect()
}

/// Per-alias resolution counters (Epic 7 thin map; Epic 5a merges these
/// into the `/metrics` `family` section — merge point: read
/// `PerAliasCounters::get` via `FamilyRuntime` snapshots/counters in
/// `src/metrics/mod.rs` rather than rebuilding per-alias accounting there).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ResolutionCounters {
    /// Successful alias resolutions (paid-aware path).
    pub resolutions_total: u32,
    /// Of those, resolutions that served a non-verifiably-free model ID.
    /// Must stay 0 for every free alias — the paid-leak audit signal.
    pub paid_resolutions: u32,
    /// Safety-net bypass servings (least-bad free, cooldown/empty-pool only).
    pub fallback_to_default_total: u32,
}

/// Per-alias counter table: every counter is keyed by alias, so free↔paid
/// traffic never shares a bucket. (Stats-value separation is Epic 2's
/// `MemberStatsMap`; the alias-scoping discipline for it is
/// [`scoped_stats_key`] — Epic 2's map is currently keyed
/// `(upstream, model)` and must adopt the alias dimension or stay split
/// per alias. Recorded here as a coordinator gap, not implemented here:
/// `member_stats.rs` is Epic 2-owned.)
#[derive(Debug, Default)]
pub struct PerAliasCounters {
    counters: HashMap<String, ResolutionCounters>,
}

impl PerAliasCounters {
    /// Empty table — counters appear on first record for their alias.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one paid-aware resolution of `alias`; `served_paid` must be
    /// `!is_verifiably_free(served_model)`.
    pub fn record_resolution(&mut self, alias: &str, served_paid: bool) {
        let entry = self.counters.entry(alias.to_string()).or_default();
        entry.resolutions_total += 1;
        if served_paid {
            entry.paid_resolutions += 1;
        }
    }

    /// Records one safety-net bypass serving for `alias`.
    pub fn record_fallback(&mut self, alias: &str) {
        self.counters
            .entry(alias.to_string())
            .or_default()
            .fallback_to_default_total += 1;
    }

    /// Counters for `alias`, or `None` before its first record.
    #[must_use]
    pub fn get(&self, alias: &str) -> Option<&ResolutionCounters> {
        self.counters.get(alias)
    }
}

/// Alias-scoped stats key `(alias, upstream, model)`: the keying discipline
/// that keeps free↔paid `MemberStats` from leaking into each other when both
/// aliases resolve members on the same upstream. Offered for Epic 2's
/// `MemberStatsMap` to adopt (either as the full key or as a per-alias map
/// split); until then, per-alias `ResolutionCounters` above carry the
/// isolation on this side.
//
// Alias-scoping note (Epic 3 decision): ADR-002 mandates `(upstream, model)`
// keying and paid/free members are distinct model IDs, so buckets are
// naturally disjoint. The alias dimension is NOT adopted unless free and
// paid aliases share an identical `(upstream, model)` member AND tests
// prove leakage — that case is recorded as a gap, not reworked here.
#[must_use]
pub fn scoped_stats_key(alias: &str, upstream: &str, model: &str) -> (String, String, String) {
    (alias.to_string(), upstream.to_string(), model.to_string())
}

// ────────────────────────────────────────────────────────────────────────────
// Epic 3: ranked resolution, pre-dispatch exclusion, hysteresis, probe,
// concurrency cap, cooldown-scoped SafetyNetBypass
// ────────────────────────────────────────────────────────────────────────────

/// Confirmed hysteresis values (plan Unresolved Questions,
/// RESOLVED-CONFIRMED): a challenger dethrones the incumbent only by beating
/// it on BOTH axes — error delta > 2pp AND latency improvement > 10%.
/// Prevents flapping on near-ties.
pub const HYSTERESIS_ERR_PP: f64 = 0.02;
/// See [`HYSTERESIS_ERR_PP`].
pub const HYSTERESIS_LAT_PCT: f64 = 0.10;
/// Exploration probe cadence (confirmed value): every Nth family resolution
/// samples the best non-pick member — excluding denylisted/cooled members —
/// so a demoted model can recover rank. Tagged `reason=probe` in logs.
pub const PROBE_EVERY_N: u64 = 25;
/// Per-member in-flight concurrency cap (confirmed default band 2–4; 4 is
/// the wired default). At resolve time under-cap members sort ahead of
/// at-cap ones (overflow routes to the sibling); the dispatch loop enforces
/// the cap with [`FamilyRuntime::try_acquire_inflight`] so concurrent bursts
/// can't race past it. The bypass ignores the cap (last resort by design).
pub const DEFAULT_CONCURRENCY_CAP: usize = 4;

/// Pure ranker output: the ordered member list plus whether it is a
/// cold-start default (no member has minimum samples, so config order won
/// and unknown must not read as perfect).
pub struct RankedOrder {
    pub ordered: Vec<FamilyMember>,
    pub cold: bool,
}

/// Pure ranking function: orders members by decayed error-rate, then p50
/// latency, over a stats snapshot — no I/O, unit-testable. Cold members
/// (below `MIN_SAMPLES`, or absent from the snapshot — unknown ≠ perfect)
/// sort after every warm member in config order; an all-cold pool returns
/// config order flagged `cold` (the `ColdStartDefault`).
///
/// Hysteresis: when `incumbent` names a warm member of this pool and the
/// pure-stat winner is someone else, the challenger takes the front only if
/// it beats the incumbent by [`HYSTERESIS_ERR_PP`] AND
/// [`HYSTERESIS_LAT_PCT`]; otherwise the incumbent stays first (no flap).
/// A cold incumbent never pins (pure rank wins) — this is what keeps a
/// just-probed member from hijacking the pick.
pub struct FamilyResolver;

impl FamilyResolver {
    #[must_use]
    pub fn rank(
        members: &[FamilyMember],
        views: &HashMap<MemberKey, MemberView>,
        incumbent: Option<&MemberKey>,
    ) -> RankedOrder {
        fn view_of(views: &HashMap<MemberKey, MemberView>, m: &FamilyMember) -> MemberView {
            views
                .get(&(m.upstream.clone(), m.model.clone()))
                .copied()
                .unwrap_or(MemberView {
                    error_rate: 0.0,
                    latency_p50_ms: 0,
                    samples: 0,
                    cold: true,
                })
        }

        let mut ranked: Vec<(&FamilyMember, MemberView)> =
            members.iter().map(|m| (m, view_of(views, m))).collect();
        if ranked.iter().all(|(_, v)| v.cold) {
            return RankedOrder {
                ordered: members.to_vec(),
                cold: true,
            };
        }
        // Stable: cold members keep config relative order at the tail.
        ranked.sort_by(|(_, va), (_, vb)| match (va.cold, vb.cold) {
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            _ => va
                .error_rate
                .partial_cmp(&vb.error_rate)
                .unwrap_or(Ordering::Equal)
                .then(va.latency_p50_ms.cmp(&vb.latency_p50_ms)),
        });

        if let Some(inc_key) = incumbent {
            let first_key = (ranked[0].0.upstream.clone(), ranked[0].0.model.clone());
            if first_key != *inc_key {
                if let Some(inc_pos) = ranked
                    .iter()
                    .position(|(m, _)| (m.upstream.clone(), m.model.clone()) == *inc_key)
                {
                    let inc_view = ranked[inc_pos].1;
                    // Warm incumbent + insufficient challenger margin → no flap.
                    // (Cold incumbent or sufficient margin → pure rank stands.)
                    if !inc_view.cold && !challenger_dethrones(ranked[0].1, inc_view) {
                        let entry = ranked.remove(inc_pos);
                        ranked.insert(0, entry);
                    }
                }
            }
        }

        RankedOrder {
            ordered: ranked.into_iter().map(|(m, _)| m.clone()).collect(),
            cold: false,
        }
    }
}

/// Hysteresis gate: the challenger dethrones only on a >2pp error win AND a
/// >10% latency win, both measured against the incumbent.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn challenger_dethrones(challenger: MemberView, incumbent: MemberView) -> bool {
    if incumbent.error_rate - challenger.error_rate <= HYSTERESIS_ERR_PP {
        return false;
    }
    if incumbent.latency_p50_ms == 0 || challenger.latency_p50_ms >= incumbent.latency_p50_ms {
        return false;
    }
    let improvement = (incumbent.latency_p50_ms - challenger.latency_p50_ms) as f64
        / incumbent.latency_p50_ms as f64;
    improvement > HYSTERESIS_LAT_PCT
}

/// Why a resolution served the member it did (log + snapshot dimension).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FamilyReason {
    /// Ranked pick (lowest error, then lowest latency, hysteresis applied).
    RankedPick,
    /// Cold-start default: config-order first healthy, stats below threshold.
    ColdDefault,
    /// Exploration probe: best non-pick member sampled for recovery.
    Probe,
}

impl FamilyReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FamilyReason::RankedPick => "rank",
            FamilyReason::ColdDefault => "cold",
            FamilyReason::Probe => "probe",
        }
    }
}

/// Outcome of Epic 3's pre-dispatch exclusion decision for one alias request.
#[derive(Debug)]
pub enum FamilyRouteDecision {
    /// Alias unknown to the table — dispatch leaves the body untouched.
    UnknownAlias,
    /// Ordered members to try in order (forced fallback: rank order is
    /// meaningless under random sampling, so the loop iterates this list
    /// directly regardless of the route's strategy).
    Serve {
        ordered: Vec<FamilyMember>,
        reason: FamilyReason,
        cold: bool,
    },
    /// All members excluded by cooldown/empty-pool (never 404/auth/429):
    /// serve these least-bad-first, bypassing the index cools. Counter +
    /// WARN already recorded; never contains a paid ID for a free alias;
    /// ignores the concurrency cap (last resort by design).
    Bypass { ordered: Vec<FamilyMember> },
    /// Nothing servable without violating a guard. `all_denylisted` (every
    /// candidate 404-denylisted, or no servable members at all) → dispatch
    /// surfaces a validation-flavored error (the bypass must never mask
    /// 404/auth/validation); otherwise (all 429-walled) → dispatch errors
    /// without retrying the rate-limited upstream.
    Unavailable { all_denylisted: bool },
}

/// Dispatch-side inputs for [`decide_route`]. The closures map a member's
/// upstream name onto Router-owned state (index lookup, `HealthRegistry`
/// reads) so this module stays free of Router/registry types.
pub struct DecideCtx<'a> {
    pub runtime: &'a FamilyRuntime,
    pub index_of: &'a dyn Fn(&str) -> Option<usize>,
    /// Whether the upstream's index is currently cooled (any cause).
    pub index_cooled: &'a dyn Fn(&str) -> bool,
    /// Whether the index cool is 429-driven (sibling stays eligible, bypass
    /// must not retry).
    pub index_429_cooled: &'a dyn Fn(&str) -> bool,
}

/// Pre-dispatch exclusion decision for one alias request: paid guard → rank
/// → exclusion (denylist, personal backpressure, non-429 index cools) →
///
/// concurrency partition → probe override → snapshot/counter publish, or the
/// cooldown-scoped `SafetyNetBypass`.
///
/// Exclusion semantics (Story 3.2):
/// - Denylist excludes the MEMBER (404 → dead ID skipped before dispatch;
///   only `Validation(_, 404)` — never 400 — and `ModelUnsupported` feed it,
///   via the dispatch loop's writer).
/// - Index cooldown excludes the index, EXCEPT when the cool is 429-driven
///   and the member carries no personal 429 mark: then the sibling stays
///   eligible (shared-upstream-429 rule).
/// - First request after a fresh delist still fails: providers map 404 to
///   `ProviderError::Validation` and dispatch returns immediately on
///   `is_validation` with no failover — that failure feeds the denylist, so
///   requests N+1.. skip the dead ID until the 1h TTL expires.
///
/// Counter home (canonical location): `FamilyRuntime` (owned by
/// `MetricsCollector`, survives `post_route`, feeds Epic 5a's `/metrics`).
/// Epic 7's `PerAliasCounters` keeps proving the guard at unit level, but
/// dispatch records here — including the paid dimension
/// (`record_paid_resolution`), which must stay 0 for every free alias.
#[must_use]
pub fn decide_route(table: &FamilyTable, alias: &str, ctx: &DecideCtx) -> FamilyRouteDecision {
    let Some(entry) = table.entry(alias) else {
        return FamilyRouteDecision::UnknownAlias;
    };
    let runtime = ctx.runtime;

    let allowed = paid_guarded_members(entry, ctx);
    if allowed.is_empty() {
        return FamilyRouteDecision::Unavailable {
            all_denylisted: true,
        };
    }

    let views: HashMap<MemberKey, MemberView> = allowed
        .iter()
        .map(|m| {
            (
                (m.upstream.clone(), m.model.clone()),
                runtime.member_view(&m.upstream, &m.model),
            )
        })
        .collect();
    let last = runtime.last_pick(alias);
    let ranked = FamilyResolver::rank(&allowed, &views, last.as_ref());

    let pool = eligible_pool(&ranked.ordered, runtime, ctx);
    if !pool.is_empty() {
        return serve_decision(runtime, alias, pool, &views, ranked.cold, last.as_ref());
    }
    bypass_decision(
        runtime,
        alias,
        entry.allow_paid,
        ranked.ordered,
        &views,
        &allowed,
        ctx,
    )
}

/// Paid guard — the same filter `resolve_paid_aware` applies, so the
/// free-never-paid invariant holds on the ranked path too (even a paid ID
/// that leaked past `FreeGuard` via a bad hot-swap is stripped here).
/// Members with no addressable upstream are dropped as well (validation
/// guarantees mappability — belt-and-braces).
fn paid_guarded_members(entry: &FamilyEntry, ctx: &DecideCtx) -> Vec<FamilyMember> {
    filter_paid_for_free_alias(&entry.members, entry.allow_paid)
        .into_iter()
        .filter(|m| (ctx.index_of)(&m.upstream).is_some())
        .cloned()
        .collect()
}

/// Pre-dispatch exclusion + concurrency-cap partition: denylisted,
/// personally-backpressured, and non-429-index-cooled members are out;
/// survivors sort under-cap first in rank order, at-cap overflow last
/// (overflow routes to the sibling).
fn eligible_pool(
    ranked: &[FamilyMember],
    runtime: &FamilyRuntime,
    ctx: &DecideCtx,
) -> Vec<FamilyMember> {
    let mut under_cap = Vec::new();
    let mut over_cap = Vec::new();
    for m in ranked {
        if runtime.is_denylisted(&m.upstream, &m.model)
            || runtime.is_backpressured(&m.upstream, &m.model)
            || ((ctx.index_cooled)(&m.upstream) && !(ctx.index_429_cooled)(&m.upstream))
        {
            continue;
        }
        if runtime.inflight_count(&m.upstream, &m.model) < DEFAULT_CONCURRENCY_CAP {
            under_cap.push(m.clone());
        } else {
            over_cap.push(m.clone());
        }
    }
    let mut pool = under_cap;
    pool.append(&mut over_cap);
    pool
}

/// Serves the pool's head pick — or the best eligible non-pick when the
/// probe is due — publishing the snapshot/counters and the resolution logs.
fn serve_decision(
    runtime: &FamilyRuntime,
    alias: &str,
    mut pool: Vec<FamilyMember>,
    views: &HashMap<MemberKey, MemberView>,
    cold: bool,
    last: Option<&MemberKey>,
) -> FamilyRouteDecision {
    let seen = runtime.resolutions_seen(alias);
    let probe_due = (seen + 1).is_multiple_of(PROBE_EVERY_N);
    let (pick_idx, reason) = if probe_due && pool.len() > 1 {
        // Best eligible non-pick, preferring an under-cap member; the pool
        // is already denylist/cooldown-filtered, so every entry from [1..]
        // is a legal probe target.
        let target = pool
            .iter()
            .skip(1)
            .position(|m| runtime.inflight_count(&m.upstream, &m.model) < DEFAULT_CONCURRENCY_CAP)
            .map_or(1, |p| p + 1);
        (target, FamilyReason::Probe)
    } else if cold {
        (0, FamilyReason::ColdDefault)
    } else {
        (0, FamilyReason::RankedPick)
    };
    pool.swap(0, pick_idx);
    let pick = pool[0].clone();
    publish_resolution(runtime, alias, &pick, views);
    if reason == FamilyReason::Probe {
        tracing::info!(
            alias = %alias,
            probed = %pick.model,
            reason = "probe",
            "family exploration probe: sampling non-pick member"
        );
    } else {
        log_resolution(alias, &pick, &pool, views, reason, last);
    }
    FamilyRouteDecision::Serve {
        ordered: pool,
        reason,
        cold,
    }
}

/// Empty pool: denylist-scope (→ error, never bypass) vs cooldown-scope
/// (→ bypass, never paid-from-free, never past a 429).
#[allow(clippy::too_many_arguments)]
fn bypass_decision(
    runtime: &FamilyRuntime,
    alias: &str,
    allow_paid: bool,
    full_order: Vec<FamilyMember>,
    views: &HashMap<MemberKey, MemberView>,
    allowed: &[FamilyMember],
    ctx: &DecideCtx,
) -> FamilyRouteDecision {
    let all_denylisted = allowed
        .iter()
        .all(|m| runtime.is_denylisted(&m.upstream, &m.model));
    let bypassable: Vec<FamilyMember> = full_order
        .into_iter()
        .filter(|m| {
            !runtime.is_denylisted(&m.upstream, &m.model)
                && !runtime.is_backpressured(&m.upstream, &m.model)
                && !(ctx.index_429_cooled)(&m.upstream)
        })
        .collect();
    if bypassable.is_empty() {
        tracing::warn!(
            alias = %alias,
            all_denylisted,
            "`SafetyNetBypass` declined: no member servable without violating a guard (denylisted or 429-walled); returning error"
        );
        return FamilyRouteDecision::Unavailable { all_denylisted };
    }
    let pick = bypassable[0].clone();
    debug_assert!(
        allow_paid || is_verifiably_free(&pick.model),
        "bypass must never serve paid from a free alias"
    );
    runtime.record_fallback(alias);
    tracing::warn!(
        alias = %alias,
        served = %pick.model,
        "`SafetyNetBypass`: all members cooldown-excluded; serving least-bad member (never paid-from-free, never past a 429)"
    );
    publish_resolution(runtime, alias, &pick, views);
    FamilyRouteDecision::Bypass {
        ordered: bypassable,
    }
}

/// Publishes one alias's last-resolution record + counters (the
/// `ResolutionSnapshot` Epic 5a serves from `/metrics` + dashboard), and the
/// paid dimension for the paid-leak audit.
fn publish_resolution(
    runtime: &FamilyRuntime,
    alias: &str,
    pick: &FamilyMember,
    views: &HashMap<MemberKey, MemberView>,
) {
    let v = views
        .get(&(pick.upstream.clone(), pick.model.clone()))
        .copied()
        .unwrap_or(MemberView {
            error_rate: 0.0,
            latency_p50_ms: 0,
            samples: 0,
            cold: true,
        });
    let previous_pick = runtime.snapshot(alias).map(|s| s.picked);
    runtime.record_snapshot(ResolutionSnapshot::new(
        alias,
        &pick.upstream,
        &pick.model,
        v.error_rate,
        v.latency_p50_ms,
        v.samples,
        previous_pick,
    ));
    if !is_verifiably_free(&pick.model) {
        runtime.record_paid_resolution(alias);
    }
}

/// Per-resolution info log + the pick-change event line (alias, from→to,
/// margins) so flapping is visible without log spam.
#[allow(clippy::cast_precision_loss)]
fn log_resolution(
    alias: &str,
    pick: &FamilyMember,
    pool: &[FamilyMember],
    views: &HashMap<MemberKey, MemberView>,
    reason: FamilyReason,
    last: Option<&MemberKey>,
) {
    let key = (pick.upstream.clone(), pick.model.clone());
    let v = views.get(&key).copied().unwrap_or(MemberView {
        error_rate: 0.0,
        latency_p50_ms: 0,
        samples: 0,
        cold: true,
    });
    let runner_up = pool.get(1);
    let (margin_err_pp, margin_lat_pct) = runner_up
        .and_then(|r| {
            views.get(&(r.upstream.clone(), r.model.clone())).map(|rv| {
                (
                    (v.error_rate - rv.error_rate) * 100.0,
                    if v.latency_p50_ms == 0 {
                        0.0
                    } else {
                        (rv.latency_p50_ms as f64 - v.latency_p50_ms as f64)
                            / v.latency_p50_ms as f64
                            * 100.0
                    },
                )
            })
        })
        .unwrap_or((0.0, 0.0));
    tracing::info!(
        alias = %alias,
        chosen = %pick.model,
        runner_up = ?runner_up.map(|r| r.model.as_str()),
        margin_err_pp,
        margin_lat_pct,
        samples = v.samples,
        reason = reason.as_str(),
        "family resolution"
    );
    if let Some(last_key) = last {
        if *last_key != key {
            tracing::info!(
                alias = %alias,
                from = %format!("{}:{}", last_key.0, last_key.1),
                to = %pick.model,
                margin_err_pp,
                margin_lat_pct,
                "family pick changed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::config::schema::{Config, FamilyMember, ModelFamily};
    use crate::config::validate_free_guard;
    use crate::metrics::{FamilyRuntime, MemberKey, MemberStats, MemberView};

    use super::{
        challenger_dethrones, decide_route, filter_paid_for_free_alias, is_verifiably_free,
        DecideCtx, FamilyResolver, FamilyRouteDecision, FamilyTable, PerAliasCounters,
    };

    /// Paid-alias config from Story 7.1 Task 1: `auto-coding-paid` with
    /// `allow_paid = true`, a snapshot-listed paid ID (`gpt-4o`, proven paid
    /// by the FreeGuard tests) plus a pricing-unknown ID (fail-closed in a
    /// free family, accepted here). No new schema field needed — Epic 1's
    /// `allow_paid` covers it (verified, not reworked).
    fn paid_alias_config() -> Config {
        let mut config = Config::default();
        config.families = vec![ModelFamily {
            alias: "auto-coding-paid".to_string(),
            members: vec![
                FamilyMember {
                    upstream: "anthropic".to_string(),
                    model: "gpt-4o".to_string(),
                },
                FamilyMember {
                    upstream: "anthropic".to_string(),
                    model: "anthropic/claude-x".to_string(),
                },
            ],
            allow_paid: true,
        }];
        config
    }

    fn free_alias_config() -> Config {
        let mut config = Config::default();
        config.families = vec![ModelFamily {
            alias: "auto-coding".to_string(),
            members: vec![
                FamilyMember {
                    upstream: "anthropic".to_string(),
                    model: "model-a:free".to_string(),
                },
                FamilyMember {
                    upstream: "anthropic".to_string(),
                    model: "model-b:free".to_string(),
                },
            ],
            allow_paid: false,
        }];
        config
    }

    #[test]
    fn family_resolver_should_resolve_paid_member_when_allow_paid_true() {
        // Config-only half: the paid alias passes FreeGuard with paid IDs.
        let config = paid_alias_config();
        assert!(
            validate_free_guard(&config).is_ok(),
            "auto-coding-paid with allow_paid=true must accept paid member IDs"
        );

        // Resolution half: the paid member resolves and the paid counter
        // increments; the free alias stays at zero paid resolutions.
        let table = FamilyTable::from_config(&config);
        assert_eq!(table.allow_paid("auto-coding-paid"), Some(true));
        let mut counters = PerAliasCounters::new();
        assert_eq!(
            table
                .resolve_paid_aware("auto-coding-paid", &mut counters)
                .as_deref(),
            Some("gpt-4o")
        );
        let paid = counters
            .get("auto-coding-paid")
            .expect("paid alias must have counters after resolving");
        assert_eq!(paid.resolutions_total, 1);
        assert_eq!(
            paid.paid_resolutions, 1,
            "serving a paid ID must bump paid_resolutions"
        );

        let free_table = FamilyTable::from_config(&free_alias_config());
        assert_eq!(
            free_table
                .resolve_paid_aware("auto-coding", &mut counters)
                .as_deref(),
            Some("model-a:free")
        );
        let free = counters
            .get("auto-coding")
            .expect("free alias must have counters after resolving");
        assert_eq!(free.resolutions_total, 1);
        assert_eq!(
            free.paid_resolutions, 0,
            "free alias must never accrue paid resolutions"
        );

        // Guard-level proof the paid ID would not survive a free alias.
        let paid_member = FamilyMember {
            upstream: "anthropic".to_string(),
            model: "gpt-4o".to_string(),
        };
        assert!(
            filter_paid_for_free_alias(std::slice::from_ref(&paid_member), false).is_empty(),
            "paid ID must be filtered out of a free alias"
        );
        assert!(!is_verifiably_free("gpt-4o"));
        assert!(is_verifiably_free("model-a:free"));
    }

    #[test]
    fn safety_net_bypass_should_never_serve_paid_id_when_free_members_all_down() {
        // Free alias pool + a paid pool on the side: the bypass may only
        // draw from the free alias, even with every free member down.
        let mut config = free_alias_config();
        config.families.push(ModelFamily {
            alias: "auto-coding-paid".to_string(),
            members: vec![FamilyMember {
                upstream: "anthropic".to_string(),
                model: "gpt-4o".to_string(),
            }],
            allow_paid: true,
        });
        let table = FamilyTable::from_config(&config);
        let mut counters = PerAliasCounters::new();

        // All-free-down (cooldown/empty-pool scope — Epic 3 decides scope;
        // here every member counts as down, so least-bad free serves).
        let served = table.safety_net_resolve_free_only("auto-coding", &mut counters);
        assert_eq!(served.as_deref(), Some("model-a:free"));
        assert!(
            served.is_some_and(|m| is_verifiably_free(&m)),
            "bypass must serve a verifiably-free ID, never paid"
        );
        let free = counters
            .get("auto-coding")
            .expect("bypass must record its serving");
        assert_eq!(free.fallback_to_default_total, 1);
        assert_eq!(
            free.paid_resolutions, 0,
            "bypass must never accrue paid resolutions"
        );
        assert!(
            counters.get("auto-coding-paid").is_none(),
            "bypass on the free alias must not touch the paid alias's counters"
        );

        // Belt-and-braces: a paid ID polluting the free alias (bad hot-swap
        // past FreeGuard) is stripped by the bypass too — it serves the
        // remaining free member, never the paid one.
        let mut polluted = free_alias_config();
        polluted.families[0].members.push(FamilyMember {
            upstream: "anthropic".to_string(),
            model: "gpt-4o".to_string(),
        });
        let polluted_table = FamilyTable::from_config(&polluted);
        let mut polluted_counters = PerAliasCounters::new();
        let served =
            polluted_table.safety_net_resolve_free_only("auto-coding", &mut polluted_counters);
        assert_eq!(
            served.as_deref(),
            Some("model-a:free"),
            "polluting paid ID must be skipped by the bypass"
        );

        // Unknown alias → no fabrication.
        assert_eq!(
            table.safety_net_resolve_free_only("no-such-alias", &mut counters),
            None
        );
        assert_eq!(
            table.resolve_paid_aware("no-such-alias", &mut counters),
            None
        );
    }

    // ── Epic 3 (Story 3.1/3.3): pure ranker + cold default + hysteresis ──

    fn two_member_list() -> Vec<FamilyMember> {
        vec![
            FamilyMember {
                upstream: "mock-a".to_string(),
                model: "model-a:free".to_string(),
            },
            FamilyMember {
                upstream: "mock-b".to_string(),
                model: "model-b:free".to_string(),
            },
        ]
    }

    fn view_of_stats(stats: &MemberStats) -> MemberView {
        MemberView {
            error_rate: stats.error_rate(),
            latency_p50_ms: stats.latency_p50_ms(),
            samples: stats.sample_count(),
            cold: stats.is_cold(),
        }
    }

    #[test]
    fn family_resolver_should_pick_lowest_error_member_when_both_healthy() {
        // Plan Story 3.1 AC1 figures, read from real `MemberStats` (not
        // hand-built views): A at 12.5% err (4 timeouts + 28 ok, n=32), B at
        // 0% (30 ok, n=30). Both above MIN_SAMPLES=20, so both are warm and
        // the lowest error-rate wins, then latency.
        //
        // NOTE: validation.md's R1 row cites n=8 vs n=10 here, which predates
        // the confirmed N≥20 cold gate (below threshold → cold default, not
        // ranking). This test follows plan.md's authoritative n=32/n=30.
        use crate::providers::ProviderError;
        let timeout = ProviderError::Timeout;
        let stats_a = MemberStats::new();
        for _ in 0..4 {
            stats_a.record(Some(&timeout), 100);
        }
        for _ in 0..28 {
            stats_a.record(None, 100);
        }
        let stats_b = MemberStats::new();
        for _ in 0..30 {
            stats_b.record(None, 90);
        }
        assert!((stats_a.error_rate() - 0.125).abs() < f64::EPSILON);
        assert!(stats_b.error_rate().abs() < f64::EPSILON);

        let members = two_member_list();
        let views: HashMap<MemberKey, MemberView> = [
            (("mock-a", "model-a:free"), &stats_a),
            (("mock-b", "model-b:free"), &stats_b),
        ]
        .into_iter()
        .map(|((u, m), s)| ((u.to_string(), m.to_string()), view_of_stats(s)))
        .collect();
        let ranked = FamilyResolver::rank(&members, &views, None);
        assert!(!ranked.cold);
        assert_eq!(ranked.ordered.len(), 2);
        assert_eq!(ranked.ordered[0].model, "model-b:free");
        assert_eq!(ranked.ordered[1].model, "model-a:free");
    }

    #[test]
    fn family_resolver_should_return_no_candidate_when_alias_unknown() {
        // Unknown alias → `UnknownAlias` (dispatch leaves the body
        // untouched); the paid-aware stub agrees with `None`.
        let table = FamilyTable::from_config(&free_alias_config());
        let runtime = FamilyRuntime::new();
        let index_of = |_: &str| -> Option<usize> { Some(0) };
        let index_cooled = |_: &str| -> bool { false };
        let index_429 = |_: &str| -> bool { false };
        let ctx = DecideCtx {
            runtime: &runtime,
            index_of: &index_of,
            index_cooled: &index_cooled,
            index_429_cooled: &index_429,
        };
        assert!(matches!(
            decide_route(&table, "no-such-alias", &ctx),
            FamilyRouteDecision::UnknownAlias
        ));
        let mut counters = PerAliasCounters::new();
        assert_eq!(
            table.resolve_paid_aware("no-such-alias", &mut counters),
            None
        );
    }

    #[test]
    fn family_resolver_should_serve_cold_start_default_when_samples_below_threshold() {
        // No member meets MIN_SAMPLES → config-order first healthy, flagged
        // cold (unknown ≠ perfect: the 0-sample member must NOT outrank).
        let members = two_member_list();
        let views: HashMap<MemberKey, MemberView> = members
            .iter()
            .map(|m| {
                (
                    (m.upstream.clone(), m.model.clone()),
                    MemberView {
                        error_rate: 0.0,
                        latency_p50_ms: 0,
                        samples: 3,
                        cold: true,
                    },
                )
            })
            .collect();
        let ranked = FamilyResolver::rank(&members, &views, None);
        assert!(ranked.cold);
        assert_eq!(ranked.ordered[0].model, "model-a:free");
        assert_eq!(ranked.ordered[1].model, "model-b:free");
    }

    #[test]
    fn family_resolver_should_keep_incumbent_when_challenger_margin_below_hysteresis() {
        // Incumbent A (err 10%, p50 2.0s), challenger B (err 9%, p50 1.9s =
        // 5% better): err margin 1pp < 2pp AND latency 5% < 10% → pick stays
        // A (no flap). Positive control: a 10pp + 50% challenger dethrones.
        let members = two_member_list();
        let key_a: MemberKey = ("mock-a".to_string(), "model-a:free".to_string());
        let views: HashMap<MemberKey, MemberView> = [
            (
                key_a.clone(),
                MemberView {
                    error_rate: 0.10,
                    latency_p50_ms: 2000,
                    samples: 40,
                    cold: false,
                },
            ),
            (
                ("mock-b".to_string(), "model-b:free".to_string()),
                MemberView {
                    error_rate: 0.09,
                    latency_p50_ms: 1900,
                    samples: 40,
                    cold: false,
                },
            ),
        ]
        .into_iter()
        .collect();
        let ranked = FamilyResolver::rank(&members, &views, Some(&key_a));
        assert_eq!(ranked.ordered[0].model, "model-a:free");

        let mut winning = views.clone();
        winning.insert(
            ("mock-b".to_string(), "model-b:free".to_string()),
            MemberView {
                error_rate: 0.0,
                latency_p50_ms: 1000,
                samples: 40,
                cold: false,
            },
        );
        let ranked = FamilyResolver::rank(&members, &winning, Some(&key_a));
        assert_eq!(ranked.ordered[0].model, "model-b:free");
        assert!(challenger_dethrones(
            MemberView {
                error_rate: 0.0,
                latency_p50_ms: 1000,
                samples: 40,
                cold: false,
            },
            MemberView {
                error_rate: 0.10,
                latency_p50_ms: 2000,
                samples: 40,
                cold: false,
            }
        ));
    }

    #[test]
    fn family_table_should_leave_alias_untouched_when_route_has_no_family_field() {
        // Validation R2 (unit error): a route without the `family` field
        // builds its `Router` with `FamilyTable::empty()` and
        // `active_family: None`, so the dispatch gate never fires — every
        // table lookup for the alias misses and the body flows through
        // untouched per existing pin semantics. (Dispatch-level proof lives
        // in `family_route_gate_should_leave_alias_untouched_when_route_has_no_family_field`
        // in `tests/family_resolution.rs`; this is the thin table-level
        // half under the validation.md name.)
        let table = FamilyTable::empty();
        assert_eq!(table.members("auto-coding"), None);
        assert_eq!(table.resolve_model("auto-coding"), None);
        assert_eq!(table.allow_paid("auto-coding"), None);
        assert!(table.entry("auto-coding").is_none());
        let mut counters = PerAliasCounters::new();
        assert_eq!(table.resolve_paid_aware("auto-coding", &mut counters), None);
        assert_eq!(
            table.safety_net_resolve_free_only("auto-coding", &mut counters),
            None
        );
        assert!(counters.get("auto-coding").is_none());
    }
}
