//! `hold_for_rate_limit_cooldown` coverage — every other test in this suite
//! builds its `HealthRegistry` with a 300s cooldown, so none of them ever
//! enter this <=15s hold branch (the PR's namesake feature).
//!
//! `HealthRegistry::remaining_secs` floors to whole seconds, so an
//! organically-tripped *whole-second* cooldown (the registry's own
//! `new(secs: u64)` default, or a `Retry-After` value) always leaves under
//! a second of real cooldown after its first hold sleep — which then floors
//! to 0 and is filtered out of the "still cooling" candidate set, so
//! `hold_for_rate_limit_cooldown` gives up rather than sleeping again. A
//! deterministic "recovers and succeeds" test therefore needs an explicit
//! sub-second-margin override (e.g. `2050ms`, not a flat `2s`) rather than a
//! whole-number cooldown.

use super::*;

fn single_candidate_router(provider: Arc<dyn Provider>, health: Arc<HealthRegistry>) -> Router {
    Router::new(RouterDeps {
        candidates: vec![upstream(0, "only")],
        providers: vec![provider],
        strategy: Arc::new(FallbackStrategy),
        health,
        admission: Arc::new(AlwaysAllow),
        metrics: MetricsCollector::new(),
    })
}

/// (a) hold-then-retry-succeeds: the only candidate is cooling down before
/// dispatch even starts, with a cooldown short enough to clear inside a
/// single hold sleep; `dispatch` must hold and then succeed once it does.
#[tokio::test]
async fn holds_then_succeeds_once_short_cooldown_clears() {
    let call_count = Arc::new(AtomicU32::new(0));
    let provider: Arc<dyn Provider> = Arc::new(AlwaysOkProvider {
        name: "only",
        call_count: call_count.clone(),
    });
    let health = Arc::new(HealthRegistry::new(300));
    health.trip(0, Some(Duration::from_millis(2050)));
    let router = single_candidate_router(provider, health);

    let res = router
        .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
        .await;

    assert!(
        res.is_ok(),
        "expected dispatch to succeed once the hold's cooldown cleared, got Err({:?})",
        res.err().map(|e| e.to_string())
    );
    assert_eq!(call_count.load(Ordering::SeqCst), 1);
}

/// A `Provider` that fails once with a non-rate-limited (`Timeout`) error —
/// which does *not* trip `HealthRegistry` — then succeeds. Used to prove a
/// candidate excluded only by `already_tried` (not by health) is freed up
/// again once a hold clears `already_tried`.
struct TimeoutThenOkProvider {
    name: &'static str,
    call_count: Arc<AtomicU32>,
}

#[async_trait::async_trait]
impl Provider for TimeoutThenOkProvider {
    fn name(&self) -> &str {
        self.name
    }

    async fn send(
        &self,
        _body: serde_json::Value,
        _headers: HeaderMap,
        _stream: bool,
    ) -> Result<ProviderResponse, ProviderError> {
        let n = self.call_count.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Err(ProviderError::Timeout)
        } else {
            Ok(ProviderResponse::Full(serde_json::json!({"ok": true})))
        }
    }

    async fn list_models(&self) -> Result<Vec<crate::providers::ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }
}

/// An "a"/"b" router where "a" always rate-limits (re-trips health on every
/// retry, for a *long* default cooldown so it never recovers mid-test) and
/// "b" starts healthy — the fixture shared by the `already_tried.clear()`
/// test below.
fn a_rate_limited_b_timeout_then_ok_router(
    a_calls: Arc<AtomicU32>,
    b_calls: Arc<AtomicU32>,
) -> Router {
    let providers: Vec<Arc<dyn Provider>> = vec![
        Arc::new(AlwaysErrProvider {
            name: "a",
            error: || ProviderError::RateLimited,
            call_count: a_calls,
        }),
        Arc::new(TimeoutThenOkProvider {
            name: "b",
            call_count: b_calls,
        }),
    ];
    let health = Arc::new(HealthRegistry::new(300));
    // A short, explicit, recoverable starting cooldown on "a" only.
    health.trip(0, Some(Duration::from_millis(2050)));
    Router::new(RouterDeps {
        candidates: vec![upstream(0, "a"), upstream(1, "b")],
        providers,
        strategy: Arc::new(FallbackStrategy),
        health,
        admission: Arc::new(AlwaysAllow),
        metrics: MetricsCollector::new(),
    })
}

/// (c) `already_tried.clear()` interaction: "b" fails its first attempt
/// with a non-rate-limited error, which leaves it excluded from
/// re-selection *only* via `already_tried`, not health. Once the hold
/// recovers "a"'s cooldown and clears `already_tried`, "b" (never touched
/// by health) must be selectable again — proven by "b" succeeding on its
/// second call rather than dispatch giving up with "a"'s error.
#[tokio::test]
async fn already_tried_is_cleared_so_previously_tried_candidate_retries() {
    let a_calls = Arc::new(AtomicU32::new(0));
    let b_calls = Arc::new(AtomicU32::new(0));
    let router = a_rate_limited_b_timeout_then_ok_router(a_calls, b_calls.clone());

    let res = router
        .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
        .await;

    assert!(
        res.is_ok(),
        "expected \"b\" to be retried and succeed after already_tried was cleared, got Err({:?})",
        res.err().map(|e| e.to_string())
    );
    assert_eq!(
        b_calls.load(Ordering::SeqCst),
        2,
        "\"b\" must be retried after the hold, not left permanently excluded by already_tried"
    );
}

/// (b) the `hold_retries >= 3` give-up path, (d) the boundary at exactly
/// 15s: a cooldown just inside the hold window enters the hold branch, but
/// under a paused clock it never actually advances in wall-clock terms
/// (`HealthRegistry` uses `std::time::Instant`, which a paused tokio clock
/// doesn't affect), so `dispatch` holds 3 times and then gives up — at zero
/// real wall-clock cost despite the ~45s of virtual sleeping.
#[tokio::test(start_paused = true)]
async fn gives_up_after_three_holds_when_cooldown_never_clears() {
    let call_count = Arc::new(AtomicU32::new(0));
    let provider: Arc<dyn Provider> = Arc::new(AlwaysOkProvider {
        name: "only",
        call_count: call_count.clone(),
    });
    let health = Arc::new(HealthRegistry::new(300));
    // Just inside the <=15s hold window (padded past the whole second so
    // `remaining_secs`'s floor reliably reports 15, not 14).
    health.trip(
        0,
        Some(Duration::from_secs(15) + Duration::from_millis(500)),
    );
    let router = single_candidate_router(provider, health);

    let res = router
        .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
        .await;

    assert!(
        matches!(res, Err(ProviderError::Exhausted)),
        "expected Exhausted after 3 holds that never clear, got {:?}",
        res.as_ref().err()
    );
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        0,
        "provider must never be reached while every candidate is cooling down"
    );
}

/// The mirror boundary case: a cooldown just outside the hold window (>15s)
/// must skip holding entirely and fail fast rather than sleep at all.
#[tokio::test]
async fn skips_hold_when_cooldown_exceeds_15s() {
    let call_count = Arc::new(AtomicU32::new(0));
    let provider: Arc<dyn Provider> = Arc::new(AlwaysOkProvider {
        name: "only",
        call_count: call_count.clone(),
    });
    let health = Arc::new(HealthRegistry::new(300));
    // Padded past the whole second so the floor reliably reports 16 (> 15),
    // not 15 (see this file's module doc comment on the floor behavior).
    health.trip(
        0,
        Some(Duration::from_secs(16) + Duration::from_millis(500)),
    );
    let router = single_candidate_router(provider, health);

    let started = std::time::Instant::now();
    let res = router
        .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
        .await;

    assert!(matches!(res, Err(ProviderError::Exhausted)));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "a >15s cooldown must fail fast, not hold: took {:?}",
        started.elapsed()
    );
}
