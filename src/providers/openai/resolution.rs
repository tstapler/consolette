//! Dynamic model-family resolution: candidate discovery/ranking, the
//! resolution cache, the single-flight guard, and the probe-and-walk loop.
//! Populated in Phase 2/4 of the `openai-model-resolution` project.
//!
//! Epic 2.1 (this file): turning a raw `/v1/models` listing into a ranked,
//! family-filtered candidate list (`filter_candidates`, `rank_candidates`),
//! and making a `fetch_models()` failure a first-class, transient-shaped
//! outcome (`resolve_candidates`) rather than a panic or a silently-empty
//! candidate list.
//!
//! Epic 2.2 (this file): the per-family positive cache (`ResolvedModel`,
//! owned by `OpenaiProvider` as `resolution: Arc<DashMap<String,
//! ResolvedModel>>`), the single-flight guard (`SingleFlightGuard`/
//! `SingleFlightPermit`) preventing concurrent redundant candidate walks, the
//! negative-cache/backoff window for sequential re-resolution during an
//! outage, and `resolve_family` — the entry point tying all three together.
//!
//! Epic 2.3 (this file): the real per-candidate probe loop
//! (`build_probe_body`, `walk_candidates`), replacing Epic 2.2's
//! `walk_candidates_stub` placeholder. A `RateLimited` probe result is
//! checked *before* classification and always aborts the walk immediately
//! (Blocker 3); `Deprecated` advances to the next candidate; `Transient` and
//! `Other` both abort immediately but are logged/labeled distinctly
//! (pre-mortem.md P1 #1); `WrongEndpoint` retries the *same* candidate
//! against `/v1/responses` (Epic 3.6) rather than advancing — success there
//! writes the cache entry with `Endpoint::Responses`; a second failure aborts
//! the walk like `Transient`/`Other`. Consecutive probes are spaced by
//! `capability::EVAL_PROBE_SPACING_SECS`
//! (Blocker 4) and each probe gets its own shortened timeout (Task 2.3.2c) so
//! a multi-candidate walk can't silently blow past a real client-side
//! request-timeout budget (pre-mortem.md P1 #2). Story 2.3.3 layers a
//! failure-triggered cache invalidation (wired from `openai/mod.rs::send`) and
//! a secondary TTL-on-read safety net (`RESOLUTION_TTL`) on top of the
//! Epic 2.2 cache.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use serde_json::{json, Value};
use tokio::sync::Notify;
// `tokio::time::Instant`, not `std::time::Instant`: its `now()`/`elapsed()`
// respect `tokio::time::pause()`/`advance()`, which Story 2.2.3's backoff
// tests rely on for virtual-time control (real-time sleeps would otherwise
// make those tests slow/flaky — research/pitfalls.md §5).
use tokio::time::Instant;

use crate::metrics::counters::{ResolutionOutcome, ResolutionState as ResolutionMetricState};
use crate::metrics::ProxyMetrics;
use crate::providers::openai::{classify_openai_error, OpenaiErrorClass};
use crate::providers::{ModelInfo, ProviderError};
use crate::routing::capability::EVAL_PROBE_SPACING_SECS;

/// Keep only `models` whose id starts with `family`, preserving the order
/// `/v1/models` returned them in. Ranking newest-first is a separate step —
/// see [`rank_candidates`].
pub(crate) fn filter_candidates(models: &[ModelInfo], family: &str) -> Vec<String> {
    models
        .iter()
        .filter(|m| m.id.starts_with(family))
        .map(|m| m.id.clone())
        .collect()
}

/// Parse the leading run of ASCII digits in `segment`, skipping any leading
/// non-digit characters first (e.g. a `v` version marker: `"v10"` -> `10`).
/// `None` if `segment` contains no digits at all.
fn parse_leading_numeric(segment: &str) -> Option<u64> {
    let digits: String = segment
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse::<u64>().ok()
    }
}

/// Strip `prefix` from `id` and extract a leading-numeric token from each
/// `.`/`-`-delimited segment of what remains, stopping at the first segment
/// with no extractable digits (ADR-002). `None` if `id` doesn't start with
/// `prefix`, or if zero tokens were extracted (e.g. the first segment after
/// the prefix has no digits at all).
pub(crate) fn extract_version_tokens(id: &str, prefix: &str) -> Option<Vec<u64>> {
    let rest = id.strip_prefix(prefix)?;
    let mut tokens = Vec::new();
    for segment in rest.split(['.', '-']) {
        match parse_leading_numeric(segment) {
            Some(n) => tokens.push(n),
            None => break,
        }
    }
    if tokens.is_empty() {
        None
    } else {
        Some(tokens)
    }
}

/// Order `ids` newest-first (ADR-002): descending comparison of the numeric
/// token tuples [`extract_version_tokens`] extracts after `prefix`. Ids with
/// no extractable numeric token sort after all numerically-tokenized ids, in
/// reverse-lexicographic order among themselves.
pub(crate) fn rank_candidates(prefix: &str, ids: &[String]) -> Vec<String> {
    let mut numeric: Vec<(Vec<u64>, &String)> = Vec::new();
    let mut non_numeric: Vec<&String> = Vec::new();

    for id in ids {
        match extract_version_tokens(id, prefix) {
            Some(tokens) => numeric.push((tokens, id)),
            None => non_numeric.push(id),
        }
    }

    // Stable sort: ids with identical token tuples keep their relative
    // input order rather than an arbitrary tie-break.
    numeric.sort_by(|a, b| b.0.cmp(&a.0));
    non_numeric.sort_by(|a, b| b.cmp(a));

    numeric
        .into_iter()
        .map(|(_, id)| id.clone())
        .chain(non_numeric.into_iter().cloned())
        .collect()
}

/// Turn an already-fetched `/v1/models` result into a ranked candidate list
/// for `family`, or a transient-shaped error if the fetch itself failed
/// (Story 2.1.3). `models_result`'s error variants — network/non-200/parse
/// failures from `OpenaiProvider::list_models` — are never
/// [`ProviderError::Exhausted`], so propagating unchanged already keeps a
/// fetch failure from being mistaken for "all candidates exhausted"; no
/// cache exists yet to (not) touch (Epic 2.2).
pub(crate) fn resolve_candidates(
    models_result: Result<Vec<ModelInfo>, ProviderError>,
    family: &str,
) -> Result<Vec<String>, ProviderError> {
    let models = models_result?;
    let filtered = filter_candidates(&models, family);
    Ok(rank_candidates(family, &filtered))
}

// ────────────────────────────────────────────────────────────────────────
// Epic 2.2: Resolution Cache
// ────────────────────────────────────────────────────────────────────────

/// Which `OpenAI` wire endpoint a [`ResolvedModel`] must be sent to. Set once
/// during resolution when a `WrongEndpoint` classification is observed for a
/// candidate (Epic 3.6); otherwise defaults to `ChatCompletions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Endpoint {
    #[default]
    ChatCompletions,
    Responses,
}

/// Which token-limit body field a [`ResolvedModel`] needs. Determined by the
/// same probe attempt that resolves the model id (Epic 4.1) — one combined
/// probe, one combined cache write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenParamStyle {
    MaxTokens,
    MaxCompletionTokens,
}

/// The cache entry for one model family: the currently-winning candidate
/// model id, its [`Endpoint`], its [`TokenParamStyle`], and the timestamp of
/// last (re-)resolution.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedModel {
    pub(crate) model_id: String,
    pub(crate) endpoint: Endpoint,
    pub(crate) token_param: TokenParamStyle,
    pub(crate) resolved_at: Instant,
}

/// Bounded wait for a non-winning single-flight caller before it re-checks
/// the cache and either uses a populated entry or propagates a transient
/// error (Story 2.2.2). The plan calls for `min(remaining request budget, a
/// 5s ceiling)`; this codebase has no per-request deadline-propagation
/// mechanism yet, so callers pass their own budget and it's clamped to this
/// ceiling here.
pub(crate) const SINGLE_FLIGHT_WAIT_CEILING: Duration = Duration::from_secs(5);

/// Negative-cache/backoff window (Story 2.2.3): a resolution walk that ends
/// `Exhausted` or with a `fetch_models()` failure blocks sequential re-tries
/// for this long, so a sustained outage fails fast instead of re-paying a
/// full candidate walk per request (adversarial-review.md Blocker 2).
pub(crate) const RESOLUTION_BACKOFF_WINDOW: Duration = Duration::from_secs(30);

/// Secondary safety net (Story 2.3.3b): a cache entry older than this is
/// treated as a miss on next read and triggers re-resolution, even without an
/// observed `Deprecated` failure on a real request. Also the backstop for a
/// `WrongEndpoint` misclassification surviving into steady state (a model
/// that becomes Responses-only after being cached as chat/completions) — see
/// plan.md Story 2.3.3's Acceptance Criteria.
pub(crate) const RESOLUTION_TTL: Duration = Duration::from_hours(1);

/// Per-family single-flight guard preventing concurrent redundant candidate
/// walks (Story 2.2.2), modeled on `openrouter/cache.rs`'s `ModelListCache`
/// compare-exchange pattern. Unlike that precedent, release is RAII
/// ([`SingleFlightPermit`]'s `Drop`), not a manual clear at the end of the
/// happy path, so an early return, `?`-propagated error, or panic-unwind
/// during the winning walk still releases the guard (this crate does not set
/// `panic = "abort"` in `Cargo.toml`, so `Drop` runs on unwind —
/// adversarial-review.md Blocker 5).
pub(crate) struct SingleFlightGuard {
    flags: DashMap<String, Arc<AtomicBool>>,
    notifies: DashMap<String, Arc<Notify>>,
}

impl SingleFlightGuard {
    pub(crate) fn new() -> Self {
        Self {
            flags: DashMap::new(),
            notifies: DashMap::new(),
        }
    }

    fn notify_for(&self, family: &str) -> Arc<Notify> {
        self.notifies
            .entry(family.to_string())
            .or_insert_with(|| Arc::new(Notify::new()))
            .clone()
    }

    /// Attempt to become the single walker for `family`. `Some(permit)` if
    /// this caller won the compare-exchange and should perform the walk;
    /// `None` if a walk is already in flight (the caller should await
    /// [`SingleFlightGuard::notified_for`] instead of starting its own walk).
    pub(crate) fn try_acquire(&self, family: &str) -> Option<SingleFlightPermit> {
        let flag = self
            .flags
            .entry(family.to_string())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone();
        let notify = self.notify_for(family);
        if flag
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        Some(SingleFlightPermit { flag, notify })
    }

    /// The `Notify` a non-winning caller should await for `family`'s
    /// in-flight walk to finish (or be released on panic/early-return).
    pub(crate) fn notified_for(&self, family: &str) -> Arc<Notify> {
        self.notify_for(family)
    }
}

/// RAII permit returned by a winning [`SingleFlightGuard::try_acquire`]
/// call. `Drop` always clears the flag and wakes any waiters, regardless of
/// how the holder's scope exits — early return, `?`-propagated error, or
/// panic-unwind.
pub(crate) struct SingleFlightPermit {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl Drop for SingleFlightPermit {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Release);
        self.notify.notify_waiters();
    }
}

/// The per-`OpenaiProvider`-instance resolution state (Epic 2.2): the
/// positive cache, the single-flight guard, and the negative-cache/backoff
/// map, bundled so `OpenaiProvider` and [`resolve_family`] each carry one
/// handle instead of three.
pub(crate) struct ResolutionState {
    pub(crate) cache: DashMap<String, ResolvedModel>,
    pub(crate) single_flight: SingleFlightGuard,
    pub(crate) backoff: DashMap<String, (Instant, ProviderError)>,
    /// Epic 5.1: the process-wide metrics counters the walk loop records
    /// against, and the upstream name attempts/exhaustion are labeled with.
    /// Test construction (`ResolutionState::new`) gets a private,
    /// never-exposed `ProxyMetrics` instance so a walk-loop test doesn't
    /// need a live `Router`/`MetricsCollector` just to run.
    pub(crate) metrics: Arc<ProxyMetrics>,
    pub(crate) upstream: String,
}

impl ResolutionState {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::with_metrics(String::new(), Arc::new(ProxyMetrics::new()))
    }

    pub(crate) fn with_metrics(upstream: String, metrics: Arc<ProxyMetrics>) -> Self {
        Self {
            cache: DashMap::new(),
            single_flight: SingleFlightGuard::new(),
            backoff: DashMap::new(),
            metrics,
            upstream,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────
// Epic 2.3: Probe-and-Walk Resolution Loop
// ────────────────────────────────────────────────────────────────────────

/// Story 2.3.1: a minimal, side-effect-free probe request body for
/// `model_id` — a single short user message, no `tools` key, and a token
/// budget well below `capability.rs`'s `EVAL_MAX_TOKENS = 64` tool-probing
/// shape (this probe only needs to confirm the model id is alive/routable,
/// not that it can call tools).
pub(crate) fn build_probe_body(model_id: &str) -> Value {
    json!({
        "model": model_id,
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 16,
    })
}

/// Epic 3.6's `/v1/responses` counterpart to [`build_probe_body`] — same
/// candidate id, but shaped for the Responses API's `input`/
/// `max_output_tokens` fields (see `responses::translate_anthropic_request_to_responses`)
/// instead of `/v1/chat/completions`'s `messages`/`max_tokens`.
pub(crate) fn build_probe_body_responses(model_id: &str) -> Value {
    json!({
        "model": model_id,
        "input": "hi",
        "max_output_tokens": 16,
    })
}

/// Rename the `max_tokens` key to `max_completion_tokens` on `body` in
/// place, if present. Shared by two call sites (Epic 4.1): substituting a
/// probe body's token-limit key for the retry in [`walk_candidates`]
/// (Story 4.1.1), and post-processing a real outgoing request body once a
/// candidate's [`TokenParamStyle::MaxCompletionTokens`] is known
/// (Story 4.1.2, `OpenaiProvider::send`).
pub(crate) fn rename_max_tokens_to_max_completion_tokens(body: &mut Value) {
    if let Some(obj) = body.as_object_mut() {
        if let Some(value) = obj.remove("max_tokens") {
            obj.insert("max_completion_tokens".to_string(), value);
        }
    }
}

/// Story 4.1.1: does a 400 response name `max_completion_tokens` as the
/// required replacement for `max_tokens`? A narrower, independent signal
/// from [`classify_openai_error`]'s four-variant contract — checked before
/// (and separately from) that classification in the probe loop, since a
/// candidate that merely needs a different token-limit key is very much
/// alive, not `Deprecated`/`WrongEndpoint`/`Transient`/`Other`.
fn is_max_tokens_unsupported(status: u16, body: &str) -> bool {
    if status != 400 {
        return false;
    }
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .is_some_and(|message| message.contains("max_completion_tokens"))
}

/// Task 2.3.2c: shrink the per-probe timeout so an `N`-candidate walk can't
/// silently consume `N × request_timeout_secs` on a cold cache. The
/// `clamp(1, 5)` bounds the divisor against a pathological candidate count —
/// requirements.md's own examples show 2-5 real candidates per family — so a
/// long candidate list can't drive the per-probe timeout arbitrarily low on
/// an otherwise-healthy, merely-slow upstream.
fn probe_timeout(request_timeout_secs: u64, candidate_count: usize) -> Duration {
    let divisor = candidate_count.clamp(1, 5) as u64;
    Duration::from_secs((request_timeout_secs / divisor).min(10))
}

/// Classify a probe failure that isn't `RateLimited`/`RateLimitedWithRetry`
/// (those are checked, and handled, before this is ever called — Blocker 3).
/// `Validation`/`Upstream` carry the status+body `classify_openai_error`
/// needs; `Timeout` has neither, so it's unambiguously `Transient` (mirrors
/// `classify_openai_error`'s own `status == 0` case). Anything else (`Auth`,
/// `ModelUnsupported`, `Exhausted`, `ResponseShapeMismatch`) is not an
/// expected probe outcome — classified `Other` rather than silently folded
/// into `Transient`, so it surfaces as "resolution didn't recognize this"
/// rather than "genuine transient blip" (pre-mortem.md P1 #1).
fn classify_probe_failure(err: &ProviderError) -> OpenaiErrorClass {
    match err {
        ProviderError::Validation(body, status) | ProviderError::Upstream { body, status } => {
            classify_openai_error(*status, body)
        }
        ProviderError::Timeout => OpenaiErrorClass::Transient,
        _ => OpenaiErrorClass::Other,
    }
}

/// Epic 2.3's real probe-and-walk loop: try each ranked candidate in order,
/// classifying each failure via [`classify_openai_error`] to decide whether
/// to advance, abort, or (Epic 3.6) retry against `/v1/responses`.
///
/// `probe` sends one probe request for a candidate id against the given
/// [`Endpoint`] and [`TokenParamStyle`] (Epic 4.1: which token-limit body key
/// to probe with), with the given per-probe timeout (Task 2.3.2c), and
/// returns the raw response `Value` on success, or the `ProviderError`
/// `OpenaiProvider::send_request`'s own `map_error_status`/error-mapping
/// already produces (so a 429 always surfaces as `ProviderError::RateLimited`
/// here too, never something this loop would need to reclassify). It is
/// called with `Endpoint::ChatCompletions`/`TokenParamStyle::MaxTokens` for
/// every candidate's initial probe; on a `WrongEndpoint` classification, a
/// second time with `Endpoint::Responses` for the *same* candidate (Epic
/// 3.6); on a [`is_max_tokens_unsupported`] classification, a second time
/// with `TokenParamStyle::MaxCompletionTokens` substituted for the *same*
/// candidate and endpoint (Story 4.1.1) — the two retries are independent
/// axes and never combined in one probe call.
///
/// `metrics`/`upstream` (Epic 5.1) label every `resolution_attempts_total`
/// increment this walk records; `upstream` and `family` alone label the
/// single `resolution_exhausted_total` increment on the all-candidates-failed
/// path.
fn record_resolution_success(
    metrics: &ProxyMetrics,
    upstream: &str,
    family: &str,
    candidate: &str,
    index: usize,
) {
    metrics.record_resolution_attempt(upstream, family, candidate, ResolutionOutcome::Success);
    // `index == 0` is the top-ranked candidate winning outright (`Newest`);
    // any later index means higher-ranked candidates failed first
    // (`Fallback`) — see plan.md Story 5.1.2's Newest/Fallback definition.
    let state = if index == 0 {
        ResolutionMetricState::Newest
    } else {
        ResolutionMetricState::Fallback
    };
    metrics.record_resolution_success(family, state, candidate);
}

/// Record a candidate abort that isn't `Deprecated`/`WrongEndpoint`/`Other`
/// (a genuine 5xx/timeout, a `RateLimited` probe — which Blocker 3 aborts
/// exactly like `Transient` — or a retry-of-an-alive-candidate that still
/// failed): all three share `resolution_attempts_total`'s `Transient` bucket.
fn record_resolution_transient(
    metrics: &ProxyMetrics,
    upstream: &str,
    family: &str,
    candidate: &str,
) {
    metrics.record_resolution_attempt(upstream, family, candidate, ResolutionOutcome::Transient);
}

#[allow(clippy::too_many_lines)] // one cohesive fallback-walk loop; splitting fragments the state machine
async fn walk_candidates<F, Fut, P, FutP>(
    fetch_models: F,
    family: &str,
    request_timeout_secs: u64,
    mut probe: P,
    metrics: &ProxyMetrics,
    upstream: &str,
) -> Result<ResolvedModel, ProviderError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Vec<ModelInfo>, ProviderError>>,
    P: FnMut(String, Duration, Endpoint, TokenParamStyle) -> FutP,
    FutP: Future<Output = Result<Value, ProviderError>>,
{
    let candidates = resolve_candidates(fetch_models().await, family)?;
    let timeout = probe_timeout(request_timeout_secs, candidates.len());

    for (index, candidate) in candidates.iter().enumerate() {
        if index > 0 {
            // Story 2.3.5 / Blocker 4: space consecutive probes so a
            // cold-cache multi-candidate walk doesn't read as a correlated
            // burst to the upstream's rate limiter — self-inflicting the
            // exact "every candidate looks dead" failure mode this feature
            // exists to avoid.
            tokio::time::sleep(Duration::from_secs(EVAL_PROBE_SPACING_SECS)).await;
        }

        match probe(
            candidate.clone(),
            timeout,
            Endpoint::ChatCompletions,
            TokenParamStyle::MaxTokens,
        )
        .await
        {
            Ok(_value) => {
                record_resolution_success(metrics, upstream, family, candidate, index);
                return Ok(ResolvedModel {
                    model_id: candidate.clone(),
                    endpoint: Endpoint::ChatCompletions,
                    token_param: TokenParamStyle::MaxTokens,
                    resolved_at: Instant::now(),
                });
            }
            // Blocker 3 / pre-mortem P1: a 429 must never reach
            // `classify_openai_error` at all — checked here, before any
            // classification, and always aborts exactly like `Transient`
            // (Blocker 3's own framing) — so it shares `Transient`'s
            // `resolution_attempts_total` bucket rather than getting a
            // seventh label the plan's Observability Plan doesn't define.
            Err(
                err @ (ProviderError::RateLimited | ProviderError::RateLimitedWithRetry { .. }),
            ) => {
                tracing::warn!(
                    family,
                    candidate = %candidate,
                    outcome = "rate_limited",
                    "resolution probe rate limited; aborting walk without advancing"
                );
                record_resolution_transient(metrics, upstream, family, candidate);
                return Err(err);
            }
            // Story 4.1.1: checked before `classify_openai_error`'s main
            // four-variant classification, and independently of it — a 400
            // naming `max_completion_tokens` as the required replacement
            // means the candidate is alive, not `Deprecated`/`Other`; retry
            // the *same* candidate/endpoint once with the key substituted.
            // Success sets `TokenParamStyle::MaxCompletionTokens` on the
            // `ResolvedModel` this walk returns — one combined probe, one
            // combined cache write (features.md Edge Case 5).
            Err(err)
                if matches!(
                    &err,
                    ProviderError::Validation(body, status)
                        | ProviderError::Upstream { body, status }
                        if is_max_tokens_unsupported(*status, body)
                ) =>
            {
                tracing::debug!(
                    family,
                    candidate = %candidate,
                    outcome = "max_tokens_unsupported_retry",
                    "resolution candidate rejected max_tokens; retrying with max_completion_tokens"
                );
                match probe(
                    candidate.clone(),
                    timeout,
                    Endpoint::ChatCompletions,
                    TokenParamStyle::MaxCompletionTokens,
                )
                .await
                {
                    Ok(_value) => {
                        record_resolution_success(metrics, upstream, family, candidate, index);
                        return Ok(ResolvedModel {
                            model_id: candidate.clone(),
                            endpoint: Endpoint::ChatCompletions,
                            token_param: TokenParamStyle::MaxCompletionTokens,
                            resolved_at: Instant::now(),
                        });
                    }
                    Err(retry_err) => {
                        tracing::warn!(
                            family,
                            candidate = %candidate,
                            outcome = "max_tokens_unsupported_retry_failed",
                            "resolution candidate's max_completion_tokens retry also failed; \
                             aborting walk without advancing"
                        );
                        // Not `Other`/`Deprecated`/`WrongEndpoint` — the
                        // candidate is alive but its retry failed, an abort
                        // shaped like `Transient` for metrics purposes.
                        record_resolution_transient(metrics, upstream, family, candidate);
                        return Err(retry_err);
                    }
                }
            }
            Err(err) => match classify_probe_failure(&err) {
                OpenaiErrorClass::Deprecated => {
                    tracing::debug!(
                        family,
                        candidate = %candidate,
                        outcome = "advance",
                        "resolution candidate deprecated; advancing to next candidate"
                    );
                    metrics.record_resolution_attempt(
                        upstream,
                        family,
                        candidate,
                        ResolutionOutcome::Advance,
                    );
                }
                OpenaiErrorClass::WrongEndpoint => {
                    tracing::warn!(
                        family,
                        candidate = %candidate,
                        outcome = "wrong_endpoint",
                        "resolution candidate requires the Responses API; retrying \
                         the same candidate against /v1/responses"
                    );
                    metrics.record_resolution_attempt(
                        upstream,
                        family,
                        candidate,
                        ResolutionOutcome::RetryResponses,
                    );
                    // Epic 3.6: retry the *same* candidate against
                    // `/v1/responses` rather than advancing — advancing here
                    // would wrongly treat a Responses-only model as "dead"
                    // (Story 3.6.1).
                    match probe(
                        candidate.clone(),
                        timeout,
                        Endpoint::Responses,
                        TokenParamStyle::MaxTokens,
                    )
                    .await
                    {
                        Ok(_value) => {
                            record_resolution_success(metrics, upstream, family, candidate, index);
                            return Ok(ResolvedModel {
                                model_id: candidate.clone(),
                                endpoint: Endpoint::Responses,
                                token_param: TokenParamStyle::MaxTokens,
                                resolved_at: Instant::now(),
                            });
                        }
                        Err(err) => {
                            // The candidate already showed signs of life
                            // (the chat/completions probe got far enough to
                            // be classified `WrongEndpoint`, not a generic
                            // failure) but the Responses retry also failed —
                            // treat this candidate as not viable and abort
                            // the walk, consistent with `Transient`/`Other`,
                            // rather than looping on the same candidate.
                            tracing::warn!(
                                family,
                                candidate = %candidate,
                                outcome = "wrong_endpoint_responses_retry_failed",
                                "resolution candidate's /v1/responses retry also failed; \
                                 aborting walk without advancing"
                            );
                            record_resolution_transient(metrics, upstream, family, candidate);
                            return Err(err);
                        }
                    }
                }
                OpenaiErrorClass::Transient => {
                    tracing::warn!(
                        family,
                        candidate = %candidate,
                        outcome = "transient",
                        "resolution candidate probe failed transiently; aborting walk"
                    );
                    record_resolution_transient(metrics, upstream, family, candidate);
                    return Err(err);
                }
                OpenaiErrorClass::Other => {
                    // Distinct outcome label from `Transient` — an `Other`
                    // classification means the classification table didn't
                    // recognize this error at all, a different operator
                    // signal than a genuine transient blip (pre-mortem.md
                    // P1 #1).
                    tracing::error!(
                        family,
                        candidate = %candidate,
                        outcome = "other",
                        "resolution candidate probe failed with an unrecognized error; \
                         aborting walk"
                    );
                    metrics.record_resolution_attempt(
                        upstream,
                        family,
                        candidate,
                        ResolutionOutcome::Other,
                    );
                    return Err(err);
                }
            },
        }
    }

    // Every candidate advanced (all `Deprecated`), or the candidate list was
    // empty to begin with (Surface 1: a zero-match family fails closed here,
    // never silently sends the literal family string as `model`). Story
    // 5.1.2: a family-level (not per-candidate) event, so `candidate` labels
    // as the family itself in `resolution_attempts_total` for the
    // `Exhausted` bucket, alongside the dedicated `resolution_exhausted_total`
    // cumulative counter and self-healing `resolution_state` flag.
    metrics.record_resolution_attempt(upstream, family, family, ResolutionOutcome::Exhausted);
    metrics.record_resolution_exhausted(upstream, family);
    Err(ProviderError::Upstream {
        status: 0,
        body: format!("all candidates in family {family} exhausted"),
    })
}

/// A walk for `family` is already in flight elsewhere: wait for its
/// completion signal (or `wait_budget`, clamped to
/// [`SINGLE_FLIGHT_WAIT_CEILING`]), then re-check the cache exactly once
/// rather than starting a second independent walk (Story 2.2.2).
async fn await_concurrent_walk(
    state: &ResolutionState,
    family: &str,
    wait_budget: Duration,
) -> Result<ResolvedModel, ProviderError> {
    let notified = state.single_flight.notified_for(family);
    let wait = wait_budget.min(SINGLE_FLIGHT_WAIT_CEILING);
    tokio::select! {
        () = notified.notified() => {}
        () = tokio::time::sleep(wait) => {}
    }
    match state.cache.get(family) {
        Some(entry) => Ok(entry.clone()),
        None => Err(ProviderError::Upstream {
            status: 0,
            body: format!("resolution already in flight for family {family:?}; no result yet"),
        }),
    }
}

/// The Epic 2.2 resolution entry point: checks the positive cache (Story
/// 2.3.3b: a cache entry older than [`RESOLUTION_TTL`] is treated as a miss,
/// not returned), then the negative-cache/backoff window (Story 2.2.3), then
/// single-flights the walk against concurrent callers for the same family
/// (Story 2.2.2).
///
/// `wait_budget` is the caller's own bound on how long it's willing to wait
/// for a concurrent winner before giving up and propagating a transient
/// error; it's clamped to [`SINGLE_FLIGHT_WAIT_CEILING`]. `probe` is Epic
/// 2.3's per-candidate probe sender (see [`walk_candidates`]).
///
/// Every test exercising this function mocks `fetch_models`/`probe` (Story
/// 6.1.1 kept those tests on synthetic `family-v*`-style ids, never a real
/// catalog snapshot). The one test that runs this walk against a real
/// `ExampleCorp` Model Gateway is `tests/openai_gateway_live_probe.rs` —
/// `#[ignore]`d, requires VPN/SBN Dev Agent access, run manually via
/// `cargo test --test openai_gateway_live_probe -- --ignored`.
pub(crate) async fn resolve_family<F, Fut, P, FutP>(
    state: &ResolutionState,
    family: &str,
    wait_budget: Duration,
    request_timeout_secs: u64,
    fetch_models: F,
    probe: P,
) -> Result<ResolvedModel, ProviderError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Vec<ModelInfo>, ProviderError>>,
    P: FnMut(String, Duration, Endpoint, TokenParamStyle) -> FutP,
    FutP: Future<Output = Result<Value, ProviderError>>,
{
    if let Some(entry) = state.cache.get(family) {
        if entry.resolved_at.elapsed() <= RESOLUTION_TTL {
            return Ok(entry.clone());
        }
        // Stale by TTL: fall through and re-walk rather than returning this
        // entry. Left in place (not removed) — if the re-walk itself fails,
        // the next read simply re-evaluates the same stale-by-TTL check
        // rather than needing a separate "resolution in progress, cache
        // empty" state.
    }

    if let Some(entry) = state.backoff.get(family) {
        let (failed_at, error) = entry.value().clone();
        if failed_at.elapsed() < RESOLUTION_BACKOFF_WINDOW {
            return Err(error);
        }
    }

    let Some(_permit) = state.single_flight.try_acquire(family) else {
        return await_concurrent_walk(state, family, wait_budget).await;
    };

    // `_permit` is held for the duration of the walk and released via its
    // `Drop` impl no matter how this scope exits below.
    let result = walk_candidates(
        fetch_models,
        family,
        request_timeout_secs,
        probe,
        &state.metrics,
        &state.upstream,
    )
    .await;
    match &result {
        Ok(resolved) => {
            state.cache.insert(family.to_string(), resolved.clone());
            state.backoff.remove(family);
        }
        Err(error) => {
            state
                .backoff
                .insert(family.to_string(), (Instant::now(), error.clone()));
        }
    }
    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn model(id: &str) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            owned_by: None,
        }
    }

    fn ids(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| (*s).to_string()).collect()
    }

    // ────────────────────────────────────────────────────────────────────
    // Story 2.1.1: filter_candidates
    // ────────────────────────────────────────────────────────────────────

    // Synthetic ids (`family-vN`), not real OpenAI catalog names, per
    // pitfalls.md §5: a test asserting against today's real catalog snapshot
    // stops proving anything about the algorithm once that catalog moves on.
    #[test]
    fn filter_candidates_should_keep_only_ids_starting_with_family_prefix() {
        let models = vec![
            model("family-v1"),
            model("family-v2"),
            model("family-v3-preview"),
            model("other-family-v1"),
        ];

        let result = filter_candidates(&models, "family-v");

        assert_eq!(result, vec!["family-v1", "family-v2", "family-v3-preview"]);
    }

    #[test]
    fn filter_candidates_should_return_empty_vec_when_no_id_matches_family_prefix() {
        let models = vec![model("other-family-v1"), model("unrelated")];

        let result = filter_candidates(&models, "family-v");

        assert!(result.is_empty());
    }

    // ────────────────────────────────────────────────────────────────────
    // Story 2.1.2: extract_version_tokens / rank_candidates (ADR-002)
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn extract_version_tokens_should_stop_at_first_non_numeric_segment() {
        assert_eq!(
            extract_version_tokens("family-5.3-codex", "family-"),
            Some(vec![5, 3])
        );
    }

    #[test]
    fn extract_version_tokens_should_return_none_when_no_digits_after_prefix() {
        assert_eq!(
            extract_version_tokens("family-experimental", "family-"),
            None
        );
    }

    #[test]
    fn rank_candidates_should_order_multi_digit_version_above_single_digit() {
        // Deliberately adversarial synthetic ids (ADR-002): a plain
        // lexicographic sort would put "family-v10" first only by
        // accident ("1" < "3" < "9" as characters) — this fixture forces a
        // numeric comparison instead.
        let result = rank_candidates(
            "family-",
            &ids(&["family-v2", "family-v10", "family-v3-preview"]),
        );

        assert_eq!(result, vec!["family-v10", "family-v3-preview", "family-v2"]);
    }

    #[test]
    fn rank_candidates_should_sort_non_numeric_id_after_numerically_tokenized_ids() {
        let result = rank_candidates("family-", &ids(&["family-v2", "family-experimental"]));

        assert_eq!(result, vec!["family-v2", "family-experimental"]);
    }

    #[test]
    fn rank_candidates_should_preserve_input_order_on_tie() {
        // Two ids with identical numeric tokens (both extract to [2]) —
        // the sort is stable, so relative input order is the tie-break.
        let result = rank_candidates("family-", &ids(&["family-v2-b", "family-v2-a"]));

        assert_eq!(result, vec!["family-v2-b", "family-v2-a"]);
    }

    // ────────────────────────────────────────────────────────────────────
    // Story 2.1.3: resolve_candidates fetch-failure handling
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn resolve_candidates_should_rank_and_filter_on_success() {
        let models_result = Ok(vec![
            model("family-v2"),
            model("family-v10"),
            model("other-thing"),
        ]);

        let result = resolve_candidates(models_result, "family-");

        assert_eq!(result.unwrap(), vec!["family-v10", "family-v2"]);
    }

    #[test]
    fn resolve_should_abort_without_cache_mutation_when_fetch_models_returns_network_error() {
        // No `ResolutionCache` exists yet (Epic 2.2) — this test's point is
        // that a fetch failure surfaces as a transient-shaped error, never
        // as `Exhausted`, and never panics or falls back to an empty `Ok`.
        let models_result: Result<Vec<ModelInfo>, ProviderError> = Err(ProviderError::Upstream {
            status: 0,
            body: "connection refused".to_string(),
        });

        let result = resolve_candidates(models_result, "family-");

        assert!(result.is_err());
        assert!(!matches!(result, Err(ProviderError::Exhausted)));
        assert!(matches!(
            result,
            Err(ProviderError::Upstream { status: 0, .. })
        ));
    }

    #[test]
    fn resolve_should_abort_without_cache_mutation_when_fetch_models_returns_non_200() {
        let models_result: Result<Vec<ModelInfo>, ProviderError> = Err(ProviderError::Validation(
            "Internal Server Error".to_string(),
            500,
        ));

        let result = resolve_candidates(models_result, "family-");

        assert!(result.is_err());
        assert!(!matches!(result, Err(ProviderError::Exhausted)));
    }

    #[test]
    fn resolve_should_abort_without_cache_mutation_when_fetch_models_body_is_malformed() {
        // Mirrors what `map_error_status` actually produces when a 200
        // response body fails `response.json()`: a `ProviderError::Upstream`
        // carrying the parse error, not a panic and not an empty candidate
        // list.
        let models_result: Result<Vec<ModelInfo>, ProviderError> = Err(ProviderError::Upstream {
            status: 200,
            body: "expected value at line 1 column 1".to_string(),
        });

        let result = resolve_candidates(models_result, "family-");

        assert!(result.is_err());
        assert!(!matches!(result, Err(ProviderError::Exhausted)));
    }

    // ────────────────────────────────────────────────────────────────────
    // Epic 2.2 / Story 2.2.2: SingleFlightGuard + SingleFlightPermit
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn single_flight_guard_should_deny_second_acquire_while_first_permit_is_held() {
        let guard = SingleFlightGuard::new();
        let _permit = guard
            .try_acquire("family")
            .expect("first acquire should win");

        assert!(
            guard.try_acquire("family").is_none(),
            "a second acquire must not win while the first permit is still held"
        );
    }

    #[test]
    fn single_flight_guard_should_allow_acquire_after_permit_is_dropped() {
        let guard = SingleFlightGuard::new();
        let permit = guard
            .try_acquire("family")
            .expect("first acquire should win");
        drop(permit);

        assert!(
            guard.try_acquire("family").is_some(),
            "dropping the permit must release the guard for a subsequent acquire"
        );
    }

    #[tokio::test]
    async fn single_flight_permit_should_release_guard_when_winning_walk_task_panics() {
        let guard = Arc::new(SingleFlightGuard::new());
        let winner_guard = Arc::clone(&guard);

        // The winning walk panics mid-flight, inside a `tokio::spawn`ed task
        // so the panic doesn't tear down the test itself. This crate does
        // not set `panic = "abort"` in `Cargo.toml` (verified), so the
        // task's stack unwinds and `_permit`'s `Drop` impl still runs before
        // the panic surfaces as a `JoinError`.
        let handle = tokio::spawn(async move {
            let _permit = winner_guard
                .try_acquire("family")
                .expect("first acquire should win");
            panic!("simulated resolution walk failure");
        });

        assert!(
            handle.await.is_err(),
            "the spawned task should have panicked"
        );

        assert!(
            guard.try_acquire("family").is_some(),
            "the guard must not be permanently stuck after the winning walk panicked"
        );
    }

    // ────────────────────────────────────────────────────────────────────
    // Epic 2.2 / Story 2.2.2: resolve_family's waiting-caller timeout path
    // ────────────────────────────────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn resolve_should_return_transient_error_when_waiting_caller_times_out_on_still_empty_cache(
    ) {
        let state = Arc::new(ResolutionState::new());
        // Hold the permit for the whole test — simulates a walk that is
        // still genuinely in flight when the waiting caller's ceiling
        // elapses (never resolved, never released).
        let permit = state
            .single_flight
            .try_acquire("family-")
            .expect("first acquire should win");

        let waiter_state = Arc::clone(&state);
        let handle = tokio::spawn(async move {
            resolve_family(
                &waiter_state,
                "family-",
                Duration::from_millis(50),
                60,
                || async { unreachable!("a waiting caller must never call fetch_models itself") },
                |_, _, _, _| async { unreachable!("a waiting caller must never probe itself") },
            )
            .await
        });

        // Let the spawned task run until it parks inside `select!` (which
        // registers its `tokio::time::sleep` timer), then advance the
        // paused clock past the waiter's ceiling so the timeout branch — not
        // the `Notify` branch, which never fires while `permit` is held —
        // is what resolves it.
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(51)).await;

        let result = handle.await.expect("waiter task should not panic");

        assert!(
            matches!(result, Err(ProviderError::Upstream { status: 0, .. })),
            "expected a transient upstream-shaped error, got {result:?}"
        );

        drop(permit);
    }

    // ────────────────────────────────────────────────────────────────────
    // Epic 2.2 / Story 2.2.3: negative-cache/backoff for sequential misses
    // ────────────────────────────────────────────────────────────────────

    /// A `fetch_models` stand-in that counts its own calls and always fails
    /// as a transient (network-shaped) error — the outcome Story 2.2.3's
    /// backoff is triggered by.
    #[allow(clippy::type_complexity)]
    fn counting_transient_fetch(
        call_count: &Arc<std::sync::atomic::AtomicUsize>,
    ) -> impl FnOnce() -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<ModelInfo>, ProviderError>>>>
    {
        let call_count = Arc::clone(call_count);
        move || {
            Box::pin(async move {
                call_count.fetch_add(1, Ordering::SeqCst);
                Err(ProviderError::Upstream {
                    status: 0,
                    body: "connection refused".to_string(),
                })
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn resolve_should_fail_fast_with_no_http_calls_when_second_request_arrives_within_backoff_window(
    ) {
        let state = ResolutionState::new();
        let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let first = resolve_family(
            &state,
            "family-",
            Duration::from_secs(5),
            60,
            counting_transient_fetch(&call_count),
            |_, _, _, _| async {
                unreachable!("a fetch_models failure must never reach the probe step")
            },
        )
        .await;
        assert!(first.is_err());
        assert_eq!(call_count.load(Ordering::SeqCst), 1);

        // T+5s — still within the 30s backoff window.
        tokio::time::advance(Duration::from_secs(5)).await;

        let second = resolve_family(
            &state,
            "family-",
            Duration::from_secs(5),
            60,
            counting_transient_fetch(&call_count),
            |_, _, _, _| async {
                unreachable!("a fetch_models failure must never reach the probe step")
            },
        )
        .await;

        assert!(second.is_err());
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "a request within the backoff window must make no fetch_models call"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn resolve_should_attempt_fresh_walk_when_request_arrives_after_backoff_window_elapses() {
        let state = ResolutionState::new();
        let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let first = resolve_family(
            &state,
            "family-",
            Duration::from_secs(5),
            60,
            counting_transient_fetch(&call_count),
            |_, _, _, _| async {
                unreachable!("a fetch_models failure must never reach the probe step")
            },
        )
        .await;
        assert!(first.is_err());

        // T+31s — past the 30s backoff window.
        tokio::time::advance(RESOLUTION_BACKOFF_WINDOW + Duration::from_secs(1)).await;

        let second = resolve_family(
            &state,
            "family-",
            Duration::from_secs(5),
            60,
            counting_transient_fetch(&call_count),
            |_, _, _, _| async {
                unreachable!("a fetch_models failure must never reach the probe step")
            },
        )
        .await;

        assert!(second.is_err());
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            2,
            "a request after the backoff window elapses must re-attempt fetch_models"
        );
    }

    // ────────────────────────────────────────────────────────────────────
    // Story 2.3.1: build_probe_body
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn build_probe_body_should_omit_tools_and_cap_token_budget_at_sixteen() {
        let body = build_probe_body("family-v3-preview");

        assert_eq!(body["model"], serde_json::json!("family-v3-preview"));
        assert!(
            body.get("tools").is_none(),
            "a probe body must never carry a tools array"
        );
        let max_tokens = body["max_tokens"]
            .as_u64()
            .expect("probe body must set a max_tokens budget");
        assert!(
            max_tokens <= 16,
            "probe max_tokens must stay well below capability.rs's EVAL_MAX_TOKENS (64)"
        );
    }

    // ────────────────────────────────────────────────────────────────────
    // Task 2.3.2c: per-probe timeout formula
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn probe_timeout_should_divide_request_timeout_by_clamped_candidate_count() {
        assert_eq!(probe_timeout(60, 5), Duration::from_secs(10));
        assert_eq!(probe_timeout(60, 2), Duration::from_secs(10));
        assert_eq!(probe_timeout(20, 2), Duration::from_secs(10));
        assert_eq!(probe_timeout(9, 3), Duration::from_secs(3));
    }

    #[test]
    fn probe_timeout_should_clamp_candidate_count_divisor_to_five() {
        // A pathological candidate count must not drive the per-probe
        // timeout below the 5-candidate floor on an otherwise-healthy,
        // merely-slow upstream (requirements.md's own examples show 2-5
        // real candidates per family).
        assert_eq!(probe_timeout(60, 100), probe_timeout(60, 5));
    }

    // ────────────────────────────────────────────────────────────────────
    // Task 2.3.2f (pre-mortem.md P1 #2): worst-case walk duration stays
    // under Claude Code's own hardcoded client-side request timeout.
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn walk_worst_case_duration_should_stay_under_client_timeout_budget() {
        // `anthropics/claude-code` issue #39906 documents Claude Code's own
        // internal `requestTimeout` default as 300000ms (300s) — looked up,
        // not guessed. The Anthropic Python SDK's own default non-streaming
        // `httpx` timeout is a separately-looser 600s, so 300s is the
        // tighter, more conservative bound to design this feature against.
        const CLIENT_TIMEOUT_BUDGET_SECS: u64 = 300;

        // requirements.md's documented candidate-count range tops out at 5;
        // `default_request_timeout()` (src/config/schema.rs:314-316) is 60.
        let candidate_count: usize = 5;
        let request_timeout_secs: u64 = 60;

        let per_probe_secs = probe_timeout(request_timeout_secs, candidate_count).as_secs();
        let candidate_count = candidate_count as u64;
        let worst_case_secs =
            per_probe_secs * candidate_count + (candidate_count - 1) * EVAL_PROBE_SPACING_SECS;

        assert_eq!(per_probe_secs, 10, "sanity check on the worked example");
        assert_eq!(worst_case_secs, 62, "sanity check on the worked example");
        assert!(
            worst_case_secs < CLIENT_TIMEOUT_BUDGET_SECS,
            "worst-case walk duration {worst_case_secs}s must stay comfortably under the \
             {CLIENT_TIMEOUT_BUDGET_SECS}s client-timeout budget — a future change to \
             candidate count, spacing, or the probe-timeout floor must not silently blow \
             this budget"
        );
    }

    // ────────────────────────────────────────────────────────────────────
    // Task 2.3.2a (pre-mortem.md P1 #1): Other vs Transient stay distinct
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn classify_probe_failure_should_keep_other_distinct_from_transient() {
        let transient = ProviderError::Upstream {
            status: 503,
            body: "{}".to_string(),
        };
        let other = ProviderError::Validation(
            r#"{"error":{"message":"invalid api key"}}"#.to_string(),
            401,
        );

        assert_eq!(
            classify_probe_failure(&transient),
            OpenaiErrorClass::Transient
        );
        assert_eq!(classify_probe_failure(&other), OpenaiErrorClass::Other);
        assert_ne!(
            classify_probe_failure(&transient),
            classify_probe_failure(&other),
            "Other must never be folded into the same outcome as Transient"
        );
    }

    #[test]
    fn classify_probe_failure_should_classify_timeout_as_transient() {
        assert_eq!(
            classify_probe_failure(&ProviderError::Timeout),
            OpenaiErrorClass::Transient
        );
    }

    // ────────────────────────────────────────────────────────────────────
    // Integration: resolve_candidates driven by a real OpenaiProvider fetch
    // against a local HTTP double, exercising the actual
    // `list_models`/`fetch_models`/`map_error_status` error paths rather
    // than a hand-constructed `ProviderError`. Uses a small local `axum`
    // server (same pattern as this module's sibling
    // `model_family_body_key` tests in `openai/mod.rs`), not
    // `src/cost_metrics/test_support.rs::MockServer` — that harness is
    // hardcoded to `POST /v1/messages/count_tokens` and always serializes
    // valid JSON, so it can't serve `GET /v1/models` or a genuinely
    // malformed body.
    // ────────────────────────────────────────────────────────────────────
    #[allow(clippy::expect_used)]
    mod fetch_integration {
        use super::*;
        use crate::auth::exec::ExecCredentialCache;
        use crate::auth::SystemSecretResolver;
        use crate::config::schema::{AuthMethod, SecretRef, Upstream, UpstreamKind};
        use crate::providers::openai::OpenaiProvider;
        use crate::providers::Provider;
        use std::sync::Arc;
        use tokio::net::TcpListener;

        fn provider_for(base_url: String) -> OpenaiProvider {
            let upstream = Arc::new(Upstream {
                name: "test-openai".to_string(),
                kind: UpstreamKind::Openai {
                    base_url: base_url.clone(),
                },
                auth: Some(AuthMethod::Bearer {
                    token: SecretRef::Inline {
                        value: "test-token".to_string(),
                    },
                }),
            });
            OpenaiProvider::new(
                upstream,
                base_url,
                Arc::new(SystemSecretResolver),
                Arc::new(ExecCredentialCache::new()),
                5,
                Arc::new(ProxyMetrics::new()),
            )
            .expect("provider construction should succeed")
        }

        /// Bind an ephemeral port and immediately drop the listener, so the
        /// address is guaranteed to have nothing listening on it — a real
        /// network connection failure, not a fabricated one.
        async fn unreachable_base_url() -> String {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind should succeed");
            let addr = listener.local_addr().expect("local_addr should succeed");
            drop(listener);
            format!("http://{addr}")
        }

        async fn start_v1_models_server(
            status: axum::http::StatusCode,
            body: &'static str,
        ) -> (String, tokio::task::JoinHandle<()>) {
            async fn handler(
                axum::extract::State((status, body)): axum::extract::State<(
                    axum::http::StatusCode,
                    &'static str,
                )>,
            ) -> impl axum::response::IntoResponse {
                (status, body)
            }

            let app = axum::Router::new()
                .route("/v1/models", axum::routing::get(handler))
                .with_state((status, body));
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("mock server bind should succeed");
            let addr = listener.local_addr().expect("local_addr should succeed");
            let handle = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            (format!("http://{addr}"), handle)
        }

        /// A `/v1/models` double that counts requests it receives (Story
        /// 2.2.1/2.2.2's "no HTTP call"/"exactly one HTTP call" assertions)
        /// and always returns one well-formed candidate.
        async fn start_counting_v1_models_server(
            call_count: Arc<std::sync::atomic::AtomicUsize>,
        ) -> (String, tokio::task::JoinHandle<()>) {
            async fn handler(
                axum::extract::State(call_count): axum::extract::State<
                    Arc<std::sync::atomic::AtomicUsize>,
                >,
            ) -> impl axum::response::IntoResponse {
                call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                axum::Json(serde_json::json!({"data": [{"id": "family-v1"}]}))
            }

            let app = axum::Router::new()
                .route("/v1/models", axum::routing::get(handler))
                .with_state(call_count);
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("mock server bind should succeed");
            let addr = listener.local_addr().expect("local_addr should succeed");
            let handle = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            (format!("http://{addr}"), handle)
        }

        #[tokio::test]
        async fn resolve_should_skip_fetch_models_and_probe_when_cache_already_holds_resolved_model(
        ) {
            let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let (base_url, _server) =
                start_counting_v1_models_server(Arc::clone(&call_count)).await;
            let provider = provider_for(base_url);
            let state = ResolutionState::new();
            state.cache.insert(
                "family-".to_string(),
                ResolvedModel {
                    model_id: "family-v3".to_string(),
                    endpoint: Endpoint::ChatCompletions,
                    token_param: TokenParamStyle::MaxTokens,
                    resolved_at: Instant::now(),
                },
            );

            let result = resolve_family(
                &state,
                "family-",
                Duration::from_secs(5),
                60,
                || provider.list_models(),
                |_, _, _, _| async { unreachable!("a cache hit must never probe") },
            )
            .await
            .expect("cache hit should resolve without touching the network");

            assert_eq!(result.model_id, "family-v3");
            assert_eq!(
                call_count.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "a cache hit must make no /v1/models call"
            );
        }

        #[tokio::test]
        async fn resolve_should_call_fetch_models_exactly_once_when_two_concurrent_requests_miss_same_family_cache(
        ) {
            let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let (base_url, _server) =
                start_counting_v1_models_server(Arc::clone(&call_count)).await;
            let provider = provider_for(base_url);
            let state = ResolutionState::new();

            // A probe stand-in returning a transient (5xx-shaped) failure —
            // this test's own point is `fetch_models`' call count, not the
            // walk's outcome shape, so the winning walk still ends in error
            // without needing a real chat/completions double.
            let always_transient_probe =
                |_: String, _: Duration, _: Endpoint, _: TokenParamStyle| async {
                    Err(ProviderError::Upstream {
                        status: 500,
                        body: "boom".to_string(),
                    })
                };

            let (first, second) = tokio::join!(
                resolve_family(
                    &state,
                    "family-",
                    Duration::from_secs(5),
                    60,
                    || provider.list_models(),
                    always_transient_probe,
                ),
                resolve_family(
                    &state,
                    "family-",
                    Duration::from_secs(5),
                    60,
                    || provider.list_models(),
                    always_transient_probe,
                ),
            );

            assert_eq!(
                call_count.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "two concurrent misses for the same family must fetch /v1/models exactly once"
            );
            assert!(first.is_err());
            assert!(second.is_err());
        }

        #[tokio::test]
        async fn resolve_should_abort_without_cache_mutation_when_fetch_models_returns_network_error(
        ) {
            let provider = provider_for(unreachable_base_url().await);

            let models_result = provider.list_models().await;
            let result = resolve_candidates(models_result, "family-");

            assert!(result.is_err());
            assert!(!matches!(result, Err(ProviderError::Exhausted)));
        }

        #[tokio::test]
        async fn resolve_should_abort_without_cache_mutation_when_fetch_models_returns_non_200() {
            let (base_url, _server) =
                start_v1_models_server(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom").await;
            let provider = provider_for(base_url);

            let models_result = provider.list_models().await;
            let result = resolve_candidates(models_result, "family-");

            assert!(result.is_err());
            assert!(!matches!(result, Err(ProviderError::Exhausted)));
        }

        #[tokio::test]
        async fn resolve_should_abort_without_cache_mutation_when_fetch_models_body_is_malformed() {
            let (base_url, _server) =
                start_v1_models_server(axum::http::StatusCode::OK, "not json at all").await;
            let provider = provider_for(base_url);

            let models_result = provider.list_models().await;
            let result = resolve_candidates(models_result, "family-");

            assert!(result.is_err());
            assert!(!matches!(result, Err(ProviderError::Exhausted)));
        }
    }

    // ────────────────────────────────────────────────────────────────────
    // Stories 2.3.2/2.3.3/2.3.5: the real probe-and-walk loop end-to-end,
    // driven by a real `OpenaiProvider` against a scripted local `axum`
    // server that serves both `GET /v1/models` and `POST
    // /v1/chat/completions` (per-candidate-scripted responses, keyed by the
    // `model` field in the request body) — `cost_metrics::test_support`'s
    // `MockServer` is hardcoded to a single unrelated route
    // (`POST /v1/messages/count_tokens`) and can't serve either of these,
    // per validation.md's noted test-infrastructure gap.
    // ────────────────────────────────────────────────────────────────────
    #[allow(clippy::expect_used, clippy::unwrap_used)]
    mod walk_integration {
        use super::*;
        use crate::auth::exec::ExecCredentialCache;
        use crate::auth::SystemSecretResolver;
        use crate::config::schema::{AuthMethod, SecretRef, Upstream, UpstreamKind};
        use crate::providers::openai::OpenaiProvider;
        use crate::providers::Provider;
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::Json;
        use std::collections::HashMap;
        use std::sync::Mutex;
        use tokio::net::TcpListener;

        fn provider_for(base_url: String, request_timeout_secs: u64) -> OpenaiProvider {
            let upstream = Arc::new(Upstream {
                name: "test-openai".to_string(),
                kind: UpstreamKind::Openai {
                    base_url: base_url.clone(),
                },
                auth: Some(AuthMethod::Bearer {
                    token: SecretRef::Inline {
                        value: "test-token".to_string(),
                    },
                }),
            });
            OpenaiProvider::new(
                upstream,
                base_url,
                Arc::new(SystemSecretResolver),
                Arc::new(ExecCredentialCache::new()),
                request_timeout_secs,
                Arc::new(ProxyMetrics::new()),
            )
            .expect("provider construction should succeed")
        }

        #[derive(Clone)]
        struct ScriptedResponse {
            status: StatusCode,
            body: serde_json::Value,
        }

        impl ScriptedResponse {
            fn ok() -> Self {
                Self {
                    status: StatusCode::OK,
                    body: serde_json::json!({"choices": [{"message": {"role": "assistant", "content": "hi"}}]}),
                }
            }

            fn error(status: u16, message: &str) -> Self {
                Self {
                    status: StatusCode::from_u16(status).expect("valid status"),
                    body: serde_json::json!({"error": {"message": message}}),
                }
            }
        }

        /// State backing the scripted `/v1/models`, `/v1/chat/completions`,
        /// and `/v1/responses` server: a fixed candidate list, a
        /// per-candidate-id scripted response for each endpoint, and the
        /// order in which candidates were actually probed on each (for the
        /// "no advance"/"exactly N probes" assertions).
        struct ScriptedServerState {
            models: Vec<String>,
            responses: HashMap<String, ScriptedResponse>,
            /// Epic 3.6: `/v1/responses` scripted responses, keyed the same
            /// way as `responses`. Empty for tests that never expect a
            /// `WrongEndpoint` retry to fire.
            responses_endpoint_responses: HashMap<String, ScriptedResponse>,
            /// Story 4.1.1: `/v1/chat/completions` scripted responses for a
            /// probe body carrying `max_completion_tokens` instead of
            /// `max_tokens`, keyed the same way as `responses`. Empty for
            /// tests that never expect the max-tokens-unsupported retry to
            /// fire.
            max_completion_tokens_responses: HashMap<String, ScriptedResponse>,
            probed: Mutex<Vec<String>>,
            /// Candidates actually sent to `/v1/responses` (Epic 3.6's retry
            /// path), in order.
            responses_probed: Mutex<Vec<String>>,
        }

        async fn models_handler(
            State(state): State<Arc<ScriptedServerState>>,
        ) -> impl IntoResponse {
            let data: Vec<_> = state
                .models
                .iter()
                .map(|id| serde_json::json!({"id": id}))
                .collect();
            Json(serde_json::json!({"data": data}))
        }

        async fn chat_completions_handler(
            State(state): State<Arc<ScriptedServerState>>,
            body: axum::body::Bytes,
        ) -> impl IntoResponse {
            let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let model = parsed
                .get("model")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            state
                .probed
                .lock()
                .expect("mock server mutex poisoned")
                .push(model.clone());

            // Story 4.1.1: a probe body carrying `max_completion_tokens`
            // (the retry substitution) is scripted independently of one
            // carrying `max_tokens` (the initial probe) — both requests
            // target the same candidate id, so they can't be told apart by
            // `model` alone.
            if parsed.get("max_completion_tokens").is_some() {
                return match state.max_completion_tokens_responses.get(&model) {
                    Some(resp) => (resp.status, Json(resp.body.clone())).into_response(),
                    None => ScriptedResponse::ok().status.into_response(),
                };
            }

            match state.responses.get(&model) {
                Some(resp) => (resp.status, Json(resp.body.clone())).into_response(),
                None => ScriptedResponse::ok().status.into_response(),
            }
        }

        /// Epic 3.6: scripted `/v1/responses` handler, mirroring
        /// `chat_completions_handler` but keyed off
        /// `responses_endpoint_responses`/`responses_probed`.
        async fn responses_handler(
            State(state): State<Arc<ScriptedServerState>>,
            body: axum::body::Bytes,
        ) -> impl IntoResponse {
            let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let model = parsed
                .get("model")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            state
                .responses_probed
                .lock()
                .expect("mock server mutex poisoned")
                .push(model.clone());

            match state.responses_endpoint_responses.get(&model) {
                Some(resp) => (resp.status, Json(resp.body.clone())).into_response(),
                None => ScriptedResponse::ok().status.into_response(),
            }
        }

        /// Start a scripted server for `models` (in listing order — ranking
        /// happens client-side) with `responses` keyed by candidate id.
        /// Candidates absent from `responses` get a bare 200 with no body
        /// content (a walk should never reach an unscripted candidate in
        /// these tests, so an empty body is a deliberate "you weren't
        /// supposed to get here" shape). No candidate id is scripted on
        /// `/v1/responses` — tests that don't expect Epic 3.6's
        /// `WrongEndpoint` retry to fire at all should never hit that route.
        async fn start_scripted_server(
            models: &[&str],
            responses: HashMap<String, ScriptedResponse>,
        ) -> (
            String,
            Arc<ScriptedServerState>,
            tokio::task::JoinHandle<()>,
        ) {
            start_scripted_server_with_responses_endpoint(models, responses, HashMap::new()).await
        }

        /// Like [`start_scripted_server`], but also scripts a probe carrying
        /// `max_completion_tokens` per candidate id via
        /// `max_completion_tokens_responses` (Story 4.1.1's retry path).
        async fn start_scripted_server_with_max_completion_tokens_response(
            models: &[&str],
            responses: HashMap<String, ScriptedResponse>,
            max_completion_tokens_responses: HashMap<String, ScriptedResponse>,
        ) -> (
            String,
            Arc<ScriptedServerState>,
            tokio::task::JoinHandle<()>,
        ) {
            start_scripted_server_full(
                models,
                responses,
                HashMap::new(),
                max_completion_tokens_responses,
            )
            .await
        }

        /// Like [`start_scripted_server`], but also scripts `/v1/responses`
        /// per candidate id via `responses_endpoint_responses` (Epic 3.6's
        /// `WrongEndpoint`-retry path), keyed the same way as `responses`.
        async fn start_scripted_server_with_responses_endpoint(
            models: &[&str],
            responses: HashMap<String, ScriptedResponse>,
            responses_endpoint_responses: HashMap<String, ScriptedResponse>,
        ) -> (
            String,
            Arc<ScriptedServerState>,
            tokio::task::JoinHandle<()>,
        ) {
            start_scripted_server_full(
                models,
                responses,
                responses_endpoint_responses,
                HashMap::new(),
            )
            .await
        }

        /// Shared constructor behind [`start_scripted_server`] and its two
        /// siblings above — scripts all three axes (`/v1/responses` retry,
        /// `max_completion_tokens` retry) at once, defaulted to empty by the
        /// narrower helpers.
        async fn start_scripted_server_full(
            models: &[&str],
            responses: HashMap<String, ScriptedResponse>,
            responses_endpoint_responses: HashMap<String, ScriptedResponse>,
            max_completion_tokens_responses: HashMap<String, ScriptedResponse>,
        ) -> (
            String,
            Arc<ScriptedServerState>,
            tokio::task::JoinHandle<()>,
        ) {
            let state = Arc::new(ScriptedServerState {
                models: models.iter().map(|s| (*s).to_string()).collect(),
                responses,
                responses_endpoint_responses,
                max_completion_tokens_responses,
                probed: Mutex::new(Vec::new()),
                responses_probed: Mutex::new(Vec::new()),
            });

            let app = axum::Router::new()
                .route("/v1/models", axum::routing::get(models_handler))
                .route(
                    "/v1/chat/completions",
                    axum::routing::post(chat_completions_handler),
                )
                .route("/v1/responses", axum::routing::post(responses_handler))
                .with_state(Arc::clone(&state));

            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("mock server bind should succeed");
            let addr = listener.local_addr().expect("local_addr should succeed");
            let handle = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            (format!("http://{addr}"), state, handle)
        }

        /// Build a `probe` closure suitable for [`resolve_family`]/
        /// [`walk_candidates`] from a real `OpenaiProvider`, mirroring
        /// exactly how `OpenaiProvider::send` wires it in production.
        // `'a` must stay named (not elided): it appears twice in the return
        // type (the `Box<dyn Future<..> + 'a>` bound and the outer `+ 'a`),
        // and clippy's own elision suggestion here doesn't compile
        // (verified — `rustc` rejects the elided form with E0106).
        #[allow(
            clippy::needless_lifetimes,
            clippy::elidable_lifetime_names,
            clippy::type_complexity
        )]
        fn probe_via<'a>(
            provider: &'a OpenaiProvider,
        ) -> impl FnMut(
            String,
            Duration,
            Endpoint,
            TokenParamStyle,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<Value, ProviderError>> + Send + 'a>,
        > + 'a {
            move |candidate: String,
                  timeout: Duration,
                  endpoint: Endpoint,
                  token_param: TokenParamStyle| {
                Box::pin(async move {
                    match endpoint {
                        Endpoint::ChatCompletions => {
                            provider
                                .probe_candidate(&candidate, timeout, token_param)
                                .await
                        }
                        // Mirrors `OpenaiProvider::send`'s production wiring
                        // (Epic 3.6): the Responses retry reuses the real
                        // send path, not a probe-specific method.
                        Endpoint::Responses => {
                            provider
                                .send_responses_request(build_probe_body_responses(&candidate))
                                .await
                        }
                    }
                })
            }
        }

        // Story 2.3.2 / validation.md: Deprecated-classified failure on the
        // first candidate advances to the second, which succeeds.
        //
        // Not `start_paused = true`: this test drives a real localhost HTTP
        // round trip (the scripted `axum` server), and tokio's paused-clock
        // auto-advance-when-idle logic races that real socket I/O — it can
        // fire the probe's own `.timeout()` before the response actually
        // arrives (confirmed empirically: every test in this module mixing
        // `start_paused` with a real HTTP call failed with
        // `ProviderError::Timeout`). Story 2.3.5's actual spacing-duration
        // assertion lives in the dedicated `resolve_should_wait_at_least_..`
        // test below, which avoids real I/O entirely so it *can* safely use
        // paused time; this test accepts one real ~3s spacing sleep in
        // exchange for exercising the real HTTP path end-to-end.
        #[tokio::test]
        async fn resolve_should_advance_to_next_candidate_when_first_candidate_classifies_deprecated(
        ) {
            let mut responses = HashMap::new();
            responses.insert(
                "fam-3".to_string(),
                ScriptedResponse::error(400, "fam-3 has been deprecated"),
            );
            responses.insert("fam-2".to_string(), ScriptedResponse::ok());
            let (base_url, state, _server) =
                start_scripted_server(&["fam-3", "fam-2", "fam-1"], responses).await;
            let provider = provider_for(base_url, 60);
            let resolution_state = ResolutionState::new();

            let result = resolve_family(
                &resolution_state,
                "fam-",
                Duration::from_secs(5),
                60,
                || provider.list_models(),
                probe_via(&provider),
            )
            .await;
            let resolved = result.expect("walk should succeed on the second candidate");

            assert_eq!(resolved.model_id, "fam-2");
            assert_eq!(
                *state.probed.lock().unwrap(),
                vec!["fam-3".to_string(), "fam-2".to_string()],
                "fam-1 must never be probed once fam-2 succeeds"
            );
        }

        // Story 2.3.2 / validation.md: a Transient (503) failure aborts the
        // walk immediately, without advancing and without a cache write.
        #[tokio::test]
        async fn resolve_should_abort_walk_without_advancing_when_candidate_returns_transient_error(
        ) {
            let mut responses = HashMap::new();
            responses.insert(
                "fam-3".to_string(),
                ScriptedResponse {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    body: serde_json::json!({}),
                },
            );
            let (base_url, state, _server) =
                start_scripted_server(&["fam-3", "fam-2", "fam-1"], responses).await;
            let provider = provider_for(base_url, 60);
            let resolution_state = ResolutionState::new();

            let result = resolve_family(
                &resolution_state,
                "fam-",
                Duration::from_secs(5),
                60,
                || provider.list_models(),
                probe_via(&provider),
            )
            .await;

            assert!(result.is_err());
            assert_eq!(
                *state.probed.lock().unwrap(),
                vec!["fam-3".to_string()],
                "a transient failure must abort the walk after exactly one attempt"
            );
            assert!(
                resolution_state.cache.get("fam-").is_none(),
                "a transient abort must never write the positive cache"
            );
        }

        // Story 3.6.1 / validation.md: a `WrongEndpoint`-classified 404 on
        // `/v1/chat/completions` retries the *same* candidate against
        // `/v1/responses`; success there writes the cache entry with
        // `Endpoint::Responses` and never advances to the next candidate.
        // (Replaces Story 2.3.2's pre-Epic-3.6 "abort" placeholder now that
        // the Responses send path exists.)
        #[tokio::test]
        async fn resolve_should_retry_same_candidate_against_responses_endpoint_when_classified_wrong_endpoint(
        ) {
            let mut chat_responses = HashMap::new();
            chat_responses.insert(
                "fam-3".to_string(),
                ScriptedResponse::error(404, "this model is only available via v1/responses"),
            );
            let mut responses_endpoint_responses = HashMap::new();
            responses_endpoint_responses.insert("fam-3".to_string(), ScriptedResponse::ok());
            let (base_url, state, _server) = start_scripted_server_with_responses_endpoint(
                &["fam-3", "fam-2"],
                chat_responses,
                responses_endpoint_responses,
            )
            .await;
            let provider = provider_for(base_url, 60);
            let resolution_state = ResolutionState::new();

            let result = resolve_family(
                &resolution_state,
                "fam-",
                Duration::from_secs(5),
                60,
                || provider.list_models(),
                probe_via(&provider),
            )
            .await;

            let resolved = result.expect("the /v1/responses retry should succeed");
            assert_eq!(resolved.model_id, "fam-3");
            assert_eq!(resolved.endpoint, Endpoint::Responses);
            assert_eq!(
                *state.probed.lock().unwrap(),
                vec!["fam-3".to_string()],
                "fam-2 must never be probed once the /v1/responses retry for fam-3 succeeds"
            );
            assert_eq!(
                *state.responses_probed.lock().unwrap(),
                vec!["fam-3".to_string()],
                "the /v1/responses retry must target the same candidate id"
            );
        }

        // Story 3.6.1: if the `/v1/responses` retry also fails, the
        // candidate is not viable — abort the walk without advancing, same
        // as `Transient`/`Other`, rather than looping on the same candidate.
        #[tokio::test]
        async fn resolve_should_abort_walk_without_advancing_when_responses_endpoint_retry_also_fails(
        ) {
            let mut chat_responses = HashMap::new();
            chat_responses.insert(
                "fam-3".to_string(),
                ScriptedResponse::error(404, "this model is only available via v1/responses"),
            );
            let mut responses_endpoint_responses = HashMap::new();
            responses_endpoint_responses.insert(
                "fam-3".to_string(),
                ScriptedResponse {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    body: serde_json::json!({}),
                },
            );
            let (base_url, state, _server) = start_scripted_server_with_responses_endpoint(
                &["fam-3", "fam-2"],
                chat_responses,
                responses_endpoint_responses,
            )
            .await;
            let provider = provider_for(base_url, 60);
            let resolution_state = ResolutionState::new();

            let result = resolve_family(
                &resolution_state,
                "fam-",
                Duration::from_secs(5),
                60,
                || provider.list_models(),
                probe_via(&provider),
            )
            .await;

            assert!(result.is_err());
            assert_eq!(
                *state.probed.lock().unwrap(),
                vec!["fam-3".to_string()],
                "must not advance to fam-2 after the /v1/responses retry also fails"
            );
            assert!(
                resolution_state.cache.get("fam-").is_none(),
                "a failed /v1/responses retry must never write the positive cache"
            );
        }

        // Story 2.3.2 / validation.md: a 429 mid-walk aborts before
        // classification, never advances, and never writes the cache.
        // `classify_openai_error` is structurally unreachable here:
        // `ProviderError::RateLimited` carries no status/body for it to
        // classify, and the walk loop's match arm for it returns before any
        // call to `classify_probe_failure`/`classify_openai_error`.
        #[tokio::test]
        async fn resolve_should_abort_walk_without_calling_classify_when_candidate_returns_rate_limited(
        ) {
            let mut responses = HashMap::new();
            responses.insert(
                "fam-3".to_string(),
                ScriptedResponse {
                    status: StatusCode::TOO_MANY_REQUESTS,
                    body: serde_json::json!({}),
                },
            );
            let (base_url, state, _server) =
                start_scripted_server(&["fam-3", "fam-2", "fam-1"], responses).await;
            let provider = provider_for(base_url, 60);
            let resolution_state = ResolutionState::new();

            let result = resolve_family(
                &resolution_state,
                "fam-",
                Duration::from_secs(5),
                60,
                || provider.list_models(),
                probe_via(&provider),
            )
            .await;

            assert!(
                matches!(result, Err(ProviderError::RateLimited)),
                "expected a rate-limited error, got {result:?}"
            );
            assert_eq!(
                *state.probed.lock().unwrap(),
                vec!["fam-3".to_string()],
                "a 429 must abort the walk after exactly one attempt, never advancing"
            );
            assert!(resolution_state.cache.get("fam-").is_none());
        }

        // Story 2.3.2 / pre-mortem.md P1 #1: an `Other`-classified (401)
        // failure aborts exactly like Transient, but is a distinguishable
        // code path (see `classify_probe_failure_should_keep_other_distinct_from_transient`
        // for the pure unit-level assertion of the distinction itself).
        #[tokio::test]
        async fn resolve_should_abort_walk_without_advancing_when_candidate_returns_other_error() {
            let mut responses = HashMap::new();
            responses.insert(
                "fam-3".to_string(),
                ScriptedResponse::error(401, "invalid api key"),
            );
            let (base_url, state, _server) =
                start_scripted_server(&["fam-3", "fam-2"], responses).await;
            let provider = provider_for(base_url, 60);
            let resolution_state = ResolutionState::new();

            let result = resolve_family(
                &resolution_state,
                "fam-",
                Duration::from_secs(5),
                60,
                || provider.list_models(),
                probe_via(&provider),
            )
            .await;

            assert!(result.is_err());
            assert_eq!(*state.probed.lock().unwrap(), vec!["fam-3".to_string()]);
            assert!(resolution_state.cache.get("fam-").is_none());
        }

        // Story 4.1.1 / validation.md: a 400 naming `max_completion_tokens`
        // as the required replacement for `max_tokens` retries the *same*
        // candidate once with the key substituted; success on the retry
        // sets `TokenParamStyle::MaxCompletionTokens` on the resulting
        // `ResolvedModel`, all within this one resolution round — exactly
        // one cache write, and it already carries the correct token_param
        // (no separate cache write/update).
        #[tokio::test]
        async fn resolve_should_set_max_completion_tokens_style_when_probe_retry_with_substituted_param_succeeds(
        ) {
            let mut responses = HashMap::new();
            responses.insert(
                "fam-1".to_string(),
                ScriptedResponse::error(
                    400,
                    "Unsupported parameter: 'max_tokens' is not supported with this model. \
                     Use 'max_completion_tokens' instead.",
                ),
            );
            let mut max_completion_tokens_responses = HashMap::new();
            max_completion_tokens_responses.insert("fam-1".to_string(), ScriptedResponse::ok());
            let (base_url, state, _server) =
                start_scripted_server_with_max_completion_tokens_response(
                    &["fam-1"],
                    responses,
                    max_completion_tokens_responses,
                )
                .await;
            let provider = provider_for(base_url, 60);
            let resolution_state = ResolutionState::new();

            let result = resolve_family(
                &resolution_state,
                "fam-",
                Duration::from_secs(5),
                60,
                || provider.list_models(),
                probe_via(&provider),
            )
            .await;

            let resolved = result.expect("the max_completion_tokens retry should succeed");
            assert_eq!(resolved.model_id, "fam-1");
            assert_eq!(resolved.token_param, TokenParamStyle::MaxCompletionTokens);
            assert_eq!(
                *state.probed.lock().unwrap(),
                vec!["fam-1".to_string(), "fam-1".to_string()],
                "the same candidate must be probed twice: once with max_tokens, once retried \
                 with max_completion_tokens"
            );

            let cache_entries: Vec<_> = resolution_state
                .cache
                .iter()
                .map(|e| (e.key().clone(), e.value().clone()))
                .collect();
            assert_eq!(
                cache_entries.len(),
                1,
                "exactly one cache write for the whole resolution round, already carrying the \
                 correct token_param — not a separate cache write/update"
            );
            assert_eq!(
                cache_entries[0].1.token_param,
                TokenParamStyle::MaxCompletionTokens
            );
        }

        // Story 2.3.2b / validation.md: every candidate classifies
        // Deprecated -> exhaustion, which writes the negative-cache/backoff
        // entry (Story 2.2.3's plumbing, exercised here end-to-end) and
        // surfaces as `ProviderError::Upstream{status: 0, ..}`.
        // Not `start_paused = true` — see the comment on
        // `resolve_should_advance_to_next_candidate_when_first_candidate_classifies_deprecated`
        // for why paused time doesn't mix safely with this real HTTP round
        // trip.
        #[tokio::test]
        async fn resolve_should_return_upstream_error_and_write_backoff_when_all_candidates_exhausted(
        ) {
            let mut responses = HashMap::new();
            responses.insert(
                "fam-2".to_string(),
                ScriptedResponse::error(400, "fam-2 has been deprecated"),
            );
            responses.insert(
                "fam-1".to_string(),
                ScriptedResponse::error(400, "fam-1 has been deprecated"),
            );
            let (base_url, state, _server) =
                start_scripted_server(&["fam-2", "fam-1"], responses).await;
            let provider = provider_for(base_url, 60);
            let resolution_state = ResolutionState::new();

            let result = resolve_family(
                &resolution_state,
                "fam-",
                Duration::from_secs(5),
                60,
                || provider.list_models(),
                probe_via(&provider),
            )
            .await;

            match &result {
                Err(ProviderError::Upstream { status: 0, body }) => {
                    assert!(
                        body.contains("fam-"),
                        "exhaustion body should name the family: {body}"
                    );
                    assert!(body.contains("exhausted"), "got: {body}");
                }
                other => panic!("expected Upstream{{status: 0, ..}}, got {other:?}"),
            }
            assert_eq!(
                *state.probed.lock().unwrap(),
                vec!["fam-2".to_string(), "fam-1".to_string()],
                "every candidate must be probed once before exhaustion"
            );
        }

        // Story 2.3.3a: a real (non-probe) request against the cached model
        // failing `Deprecated` invalidates that family's cache entry, so the
        // subsequent request re-runs the walk instead of reusing the dead
        // cached model.
        // Not `start_paused = true` — see the comment on
        // `resolve_should_advance_to_next_candidate_when_first_candidate_classifies_deprecated`
        // for why paused time doesn't mix safely with this real HTTP round
        // trip.
        #[tokio::test]
        async fn send_should_invalidate_cache_and_reresolve_when_real_request_against_cached_model_classifies_deprecated(
        ) {
            use crate::providers::Provider;

            let mut responses = HashMap::new();
            // The cached model ("fam-2") is deprecated on the real
            // (non-probe) request; the fresh walk that follows finds
            // "fam-2" still deprecated but "fam-1" alive.
            responses.insert(
                "fam-2".to_string(),
                ScriptedResponse::error(400, "fam-2 has been deprecated"),
            );
            responses.insert("fam-1".to_string(), ScriptedResponse::ok());
            let (base_url, state, _server) =
                start_scripted_server(&["fam-2", "fam-1"], responses).await;
            let provider = provider_for(base_url, 60);

            provider.resolution.cache.insert(
                "fam-".to_string(),
                ResolvedModel {
                    model_id: "fam-2".to_string(),
                    endpoint: Endpoint::ChatCompletions,
                    token_param: TokenParamStyle::MaxTokens,
                    resolved_at: Instant::now(),
                },
            );

            let mut body = serde_json::json!({
                "model": "unused",
                "messages": [{"role": "user", "content": "hi"}],
                super::super::super::MODEL_FAMILY_BODY_KEY: "fam-",
            });
            // The real (non-probe) request against the stale cached model:
            // the scripted server returns `Deprecated` for it, so this call
            // must fail *and* invalidate the cache entry as a side effect.
            let first = provider
                .send(body.clone(), http::HeaderMap::new(), false)
                .await;
            assert!(
                first.is_err(),
                "the real request against the deprecated cached model must fail"
            );
            assert!(
                provider.resolution.cache.get("fam-").is_none(),
                "a Deprecated failure on a real request must invalidate the cache entry"
            );

            // The next request for the same family must re-walk (crossing
            // one real inter-candidate spacing sleep) and land on "fam-1".
            body["model"] = serde_json::Value::String("unused".to_string());
            let second = provider.send(body, http::HeaderMap::new(), false).await;

            assert!(
                second.is_ok(),
                "the re-walk after invalidation must succeed on fam-1, got is_err={}",
                second.is_err()
            );
            let probed = state.probed.lock().unwrap().clone();
            assert!(
                probed.contains(&"fam-1".to_string()),
                "the re-walk must have probed fam-1: {probed:?}"
            );
        }

        // Story 2.3.3b: a cache entry older than RESOLUTION_TTL is treated
        // as a miss on the next read and triggers re-resolution. Backdates
        // `resolved_at` directly via `Instant` subtraction rather than
        // `tokio::time::pause`/`advance` — this test also drives a real
        // localhost HTTP round trip, which (per the comment on
        // `resolve_should_advance_to_next_candidate_when_first_candidate_classifies_deprecated`)
        // doesn't mix safely with a paused clock; there's no real sleep to
        // avoid here in the first place, since the point is a cache-entry
        // *timestamp* being stale, not any elapsed wait during the test.
        #[tokio::test]
        async fn resolve_should_treat_cache_entry_as_miss_when_resolved_at_exceeds_ttl() {
            let mut responses = HashMap::new();
            responses.insert("fam-1".to_string(), ScriptedResponse::ok());
            let (base_url, _state, _server) = start_scripted_server(&["fam-1"], responses).await;
            let provider = provider_for(base_url, 60);
            let resolution_state = ResolutionState::new();
            resolution_state.cache.insert(
                "fam-".to_string(),
                ResolvedModel {
                    model_id: "fam-stale".to_string(),
                    endpoint: Endpoint::ChatCompletions,
                    token_param: TokenParamStyle::MaxTokens,
                    resolved_at: Instant::now() - (RESOLUTION_TTL + Duration::from_secs(1)),
                },
            );

            let result = resolve_family(
                &resolution_state,
                "fam-",
                Duration::from_secs(5),
                60,
                || provider.list_models(),
                probe_via(&provider),
            )
            .await
            .expect("re-resolution past the TTL should succeed on fam-1");

            assert_eq!(
                result.model_id, "fam-1",
                "a stale-by-TTL entry must be treated as a miss, not returned as-is"
            );
        }

        // Story 2.3.5: consecutive candidate probes within one walk are
        // spaced by at least EVAL_PROBE_SPACING_SECS. Deliberately bypasses
        // real HTTP (unlike this module's other walk tests) so it *can*
        // safely use `tokio::time::pause`/`advance` — mixing paused virtual
        // time with a real socket round trip is what breaks the other tests
        // in this module (see the comment on
        // `resolve_should_advance_to_next_candidate_when_first_candidate_classifies_deprecated`).
        // The in-memory fake probe below exercises `walk_candidates`
        // directly, which is exactly where Task 2.3.5a's `sleep` call lives.
        #[tokio::test(start_paused = true)]
        async fn resolve_should_wait_at_least_eval_probe_spacing_secs_between_consecutive_candidate_probes(
        ) {
            let probed_at: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
            let probed_at_for_closure = Arc::clone(&probed_at);
            let candidates = [
                "fam-3".to_string(),
                "fam-2".to_string(),
                "fam-1".to_string(),
            ];

            let fetch_models = || async move {
                Ok(candidates
                    .iter()
                    .map(|id| ModelInfo {
                        id: id.clone(),
                        owned_by: None,
                    })
                    .collect())
            };
            let probe = move |candidate: String,
                              _timeout: Duration,
                              _endpoint: Endpoint,
                              _token_param: TokenParamStyle| {
                let probed_at = Arc::clone(&probed_at_for_closure);
                async move {
                    probed_at
                        .lock()
                        .expect("mutex poisoned")
                        .push(Instant::now());
                    if candidate == "fam-1" {
                        Ok(serde_json::json!({"ok": true}))
                    } else {
                        Err(ProviderError::Validation(
                            serde_json::json!({"error": {"message": format!("{candidate} has been deprecated")}})
                                .to_string(),
                            400,
                        ))
                    }
                }
            };

            let metrics = Arc::new(ProxyMetrics::new());
            let handle = tokio::spawn(async move {
                walk_candidates(fetch_models, "fam-", 60, probe, &metrics, "test-upstream").await
            });

            // Let the walk run until it parks on the first spacing sleep
            // (fam-3 -> fam-2). Advancing by less than the spacing must not
            // be enough to let it complete.
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_millis(EVAL_PROBE_SPACING_SECS * 1000 - 500)).await;
            assert!(
                !handle.is_finished(),
                "the walk must not complete before crossing the first spacing sleep"
            );

            // Cross the first spacing sleep, then the second (fam-2 ->
            // fam-1).
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_secs(EVAL_PROBE_SPACING_SECS + 1)).await;

            let result = handle.await.expect("walk task should not panic");
            assert_eq!(
                result.expect("walk should succeed on fam-1").model_id,
                "fam-1"
            );

            let timestamps = probed_at.lock().expect("mutex poisoned").clone();
            assert_eq!(timestamps.len(), 3, "all three candidates must be probed");
            assert!(
                timestamps[1] - timestamps[0] >= Duration::from_secs(EVAL_PROBE_SPACING_SECS),
                "fam-3 -> fam-2 must be spaced by at least EVAL_PROBE_SPACING_SECS"
            );
            assert!(
                timestamps[2] - timestamps[1] >= Duration::from_secs(EVAL_PROBE_SPACING_SECS),
                "fam-2 -> fam-1 must be spaced by at least EVAL_PROBE_SPACING_SECS"
            );
        }

        // ────────────────────────────────────────────────────────────
        // Epic 5.1: resolution_attempts_total / resolution_exhausted_total
        // wiring, exercised through the real walk loop (not just direct
        // `ProxyMetrics` calls — see metrics::counters::tests for those).
        // ────────────────────────────────────────────────────────────

        // Task 5.1.1d / pre-mortem.md P1 #1: an `Other`-classified abort
        // (here, an unrecognized `ProviderError::Auth`) must increment
        // `outcome="other"` and must NOT also increment `outcome="transient"`.
        #[tokio::test]
        async fn walk_candidates_should_record_other_outcome_distinct_from_transient() {
            let metrics = Arc::new(ProxyMetrics::new());
            let fetch_models = || async { Ok(vec![model("fam-1")]) };
            let probe = |_candidate: String, _timeout: Duration, _endpoint: Endpoint, _t| async {
                Err(ProviderError::Auth("bad token".to_string()))
            };

            let result = walk_candidates(fetch_models, "fam-", 60, probe, &metrics, "gw").await;
            assert!(result.is_err());

            let json = metrics.to_json();
            let attempts = json["resolution"]["attempts"].as_array().unwrap();
            assert!(
                attempts
                    .iter()
                    .any(|a| a["outcome"] == "other" && a["candidate"] == "fam-1"),
                "an Other-classified failure must record outcome=\"other\": {attempts:?}"
            );
            assert!(
                !attempts.iter().any(|a| a["outcome"] == "transient"),
                "an Other-classified failure must not also record outcome=\"transient\": {attempts:?}"
            );
        }

        // Task 5.1.1b/5.1.2a: every candidate advancing (all `Deprecated`)
        // exhausts the walk, incrementing the cumulative
        // `resolution_exhausted_total` counter and setting the self-healing
        // `resolution_state` flag to `"exhausted"`.
        #[tokio::test(start_paused = true)]
        async fn walk_candidates_should_record_exhaustion_when_every_candidate_is_deprecated() {
            let metrics = Arc::new(ProxyMetrics::new());
            let fetch_models = || async {
                Ok(ids(&["fam-2", "fam-1"])
                    .into_iter()
                    .map(|id| model(&id))
                    .collect())
            };
            let probe = |candidate: String, _timeout: Duration, _endpoint: Endpoint, _t| async move {
                Err(ProviderError::Validation(
                    serde_json::json!({"error": {"message": format!("{candidate} has been deprecated")}})
                        .to_string(),
                    400,
                ))
            };

            let metrics_for_task = Arc::clone(&metrics);
            let handle = tokio::spawn(async move {
                walk_candidates(fetch_models, "fam-", 60, probe, &metrics_for_task, "gw").await
            });
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_secs(EVAL_PROBE_SPACING_SECS + 1)).await;
            let result = handle.await.expect("walk task should not panic");
            assert!(
                result.is_err(),
                "an all-Deprecated walk must exhaust, not succeed"
            );

            let json = metrics.to_json();
            let exhausted = json["resolution"]["exhausted"].as_array().unwrap();
            assert_eq!(exhausted.len(), 1);
            assert_eq!(exhausted[0]["upstream"], "gw");
            assert_eq!(exhausted[0]["family"], "fam-");
            assert_eq!(exhausted[0]["count"], 1);
            assert_eq!(json["resolution"]["state"]["fam-"], "exhausted");

            // Story 5.1.2c: a subsequent success self-heals `resolution_state`
            // but must never decrement the cumulative exhaustion counter.
            metrics.record_resolution_success("fam-", ResolutionMetricState::Newest, "v-fake");
            let json = metrics.to_json();
            assert_eq!(json["resolution"]["exhausted"][0]["count"], 1);
            assert_eq!(json["resolution"]["state"]["fam-"], "newest");
        }
    }
}
