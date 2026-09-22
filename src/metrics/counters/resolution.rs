//! `openai-model-resolution` (Epic 5.1/5.2) counters: per-candidate
//! resolution-walk outcomes, cumulative exhaustion, and the self-healing
//! per-family resolution state.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

use super::ProxyMetrics;

/// Per-candidate outcome label for [`ProxyMetrics::record_resolution_attempt`]
/// (Story 5.1.1, `openai-model-resolution` project). `Other` is deliberately
/// a distinct series from `Transient` (pre-mortem.md P1 #1): an
/// `Other`-classified probe failure means the resolution loop's
/// classification table didn't recognize the error at all — a different
/// operator signal from a genuine transient blip — so folding the two
/// together would hide exactly the failure mode this project exists to fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionOutcome {
    Success,
    Advance,
    RetryResponses,
    Transient,
    Other,
    Exhausted,
}

impl ResolutionOutcome {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ResolutionOutcome::Success => "success",
            ResolutionOutcome::Advance => "advance",
            ResolutionOutcome::RetryResponses => "retry_responses",
            ResolutionOutcome::Transient => "transient",
            ResolutionOutcome::Other => "other",
            ResolutionOutcome::Exhausted => "exhausted",
        }
    }
}

/// Per-family resolution state (Story 5.1.2, Domain Glossary). Unlike
/// `ProxyMetrics::resolution_exhausted_total` (a cumulative counter that
/// never resets), this is self-healing: it flips back to `Newest`/`Fallback`
/// on the next successful resolution for the family, so the dashboard status
/// (Story 5.2.2) doesn't stay stuck red after a real recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionState {
    /// The top-ranked (first) candidate in the walk resolved successfully.
    Newest,
    /// A lower-ranked candidate won because higher-ranked ones failed.
    Fallback,
    /// Every candidate in the family failed; no model is currently resolved.
    Exhausted,
}

impl ResolutionState {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ResolutionState::Newest => "newest",
            ResolutionState::Fallback => "fallback",
            ResolutionState::Exhausted => "exhausted",
        }
    }
}

impl ProxyMetrics {
    /// Story 5.1.1: record one resolution-walk candidate outcome.
    pub fn record_resolution_attempt(
        &self,
        upstream: &str,
        family: &str,
        candidate: &str,
        outcome: ResolutionOutcome,
    ) {
        self.upstream_family
            .insert(upstream.to_string(), family.to_string());
        self.resolution_attempts
            .entry((
                upstream.to_string(),
                family.to_string(),
                candidate.to_string(),
                outcome.label().to_string(),
            ))
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Story 5.1.2/5.2.1: a resolution walk for `family` resolved
    /// successfully on `model_id`, with `state` being
    /// `ResolutionState::Newest` (the top-ranked candidate won) or
    /// `ResolutionState::Fallback` (a lower-ranked candidate won because
    /// higher-ranked ones failed) — never `ResolutionState::Exhausted`,
    /// which is only set by [`ProxyMetrics::record_resolution_exhausted`].
    /// Self-heals `resolution_state` and `resolved_model` — the
    /// dashboard-visible flags that clear/update on success, unlike
    /// `resolution_exhausted_total`.
    pub fn record_resolution_success(&self, family: &str, state: ResolutionState, model_id: &str) {
        debug_assert_ne!(
            state,
            ResolutionState::Exhausted,
            "record_resolution_success must not be called with Exhausted"
        );
        self.resolution_state.insert(family.to_string(), state);
        self.resolved_model
            .insert(family.to_string(), model_id.to_string());
    }

    /// Story 5.1.2: every candidate in `family` on `upstream` was exhausted.
    /// Increments the cumulative `resolution_exhausted_total` counter (never
    /// decrements — that's `resolution_state`'s job), sets the self-healing
    /// `resolution_state` flag, clears the now-stale `resolved_model` (Story
    /// 5.2.1: a dashboard consumer should see "no model resolved," not a
    /// model id that stopped working), and logs at `error` level, per the
    /// Observability Plan's "human-must-notice" requirement for this signal.
    pub fn record_resolution_exhausted(&self, upstream: &str, family: &str) {
        self.upstream_family
            .insert(upstream.to_string(), family.to_string());
        self.resolution_exhausted_total
            .entry((upstream.to_string(), family.to_string()))
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
        self.resolution_state
            .insert(family.to_string(), ResolutionState::Exhausted);
        self.resolved_model.remove(family);
        tracing::error!(
            upstream,
            family,
            "resolution exhausted every candidate for this family"
        );
    }

    /// Story 5.1.1: per-`(upstream, family, candidate, outcome)` attempt
    /// tally for the `/metrics` endpoint.
    fn resolution_attempts_json(&self) -> Vec<Value> {
        self.resolution_attempts
            .iter()
            .map(|entry| {
                let (upstream, family, candidate, outcome) = entry.key().clone();
                json!({
                    "upstream": upstream,
                    "family": family,
                    "candidate": candidate,
                    "outcome": outcome,
                    "count": entry.value().load(Ordering::Relaxed),
                })
            })
            .collect()
    }

    /// Story 5.1.2: per-`(upstream, family)` cumulative exhaustion tally for
    /// the `/metrics` endpoint.
    fn resolution_exhausted_json(&self) -> Vec<Value> {
        self.resolution_exhausted_total
            .iter()
            .map(|entry| {
                let (upstream, family) = entry.key().clone();
                json!({
                    "upstream": upstream,
                    "family": family,
                    "count": entry.value().load(Ordering::Relaxed),
                })
            })
            .collect()
    }

    /// Story 5.1.2: the self-healing per-family resolution state for the
    /// `/metrics` endpoint.
    fn resolution_state_json(&self) -> serde_json::Map<String, Value> {
        self.resolution_state
            .iter()
            .map(|entry| (entry.key().clone(), json!(entry.value().label())))
            .collect()
    }

    /// Story 5.1.1/5.1.2: snapshots the resolution-walk counters — the
    /// per-candidate-outcome attempt tally, the cumulative exhaustion tally,
    /// and the self-healing per-family state — for the `/metrics` endpoint.
    pub(super) fn resolution_json(&self) -> Value {
        json!({
            "attempts": self.resolution_attempts_json(),
            "exhausted": self.resolution_exhausted_json(),
            "state": self.resolution_state_json(),
        })
    }

    /// Story 5.2.1: the flat `resolution_family`/`resolution_state`/
    /// `resolved_model` siblings to attach to one upstream's `providers[..]`
    /// JSON entry — present only for upstreams that have actually run a
    /// resolution walk (i.e. have `model_family` configured), absent (not
    /// null) otherwise. Merged into `provider` in place by `upstream_json`.
    pub(super) fn merge_resolution_fields(&self, upstream: &str, provider: &mut Value) {
        let Some(family) = self.upstream_family.get(upstream) else {
            return;
        };
        let family = family.clone();
        let Value::Object(obj) = provider else {
            return;
        };
        obj.insert("resolution_family".to_string(), json!(family));
        if let Some(state) = self.resolution_state.get(&family) {
            obj.insert("resolution_state".to_string(), json!(state.label()));
        }
        if let Some(model_id) = self.resolved_model.get(&family) {
            obj.insert("resolved_model".to_string(), json!(model_id.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::ProxyMetrics;
    use super::*;

    // validation.md: In-Scope 5 / Story 5.1.1.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn resolution_attempts_total_should_increment_once_per_candidate_outcome_with_correct_labels() {
        let m = ProxyMetrics::new();
        m.record_resolution_attempt(
            "model-gateway-openai",
            "gpt-5",
            "v3",
            ResolutionOutcome::Advance,
        );
        m.record_resolution_attempt(
            "model-gateway-openai",
            "gpt-5",
            "v2",
            ResolutionOutcome::Success,
        );

        let json = m.to_json();
        let attempts = json["resolution"]["attempts"].as_array().unwrap();
        assert_eq!(attempts.len(), 2);
        assert!(attempts
            .iter()
            .any(|a| a["upstream"] == "model-gateway-openai"
                && a["family"] == "gpt-5"
                && a["candidate"] == "v3"
                && a["outcome"] == "advance"
                && a["count"] == 1));
        assert!(attempts
            .iter()
            .any(|a| a["upstream"] == "model-gateway-openai"
                && a["family"] == "gpt-5"
                && a["candidate"] == "v2"
                && a["outcome"] == "success"
                && a["count"] == 1));
    }

    // pre-mortem.md P1 #1 / Task 5.1.1d: `other` must never be folded into
    // `transient` — they must be distinct series.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn resolution_attempts_total_should_track_other_and_transient_as_distinct_series() {
        let m = ProxyMetrics::new();
        m.record_resolution_attempt("gw", "gpt-5", "v3", ResolutionOutcome::Other);

        let attempts = m.to_json()["resolution"]["attempts"].clone();
        let attempts = attempts.as_array().unwrap();
        assert_eq!(
            attempts.len(),
            1,
            "an Other-classified abort must increment exactly one series"
        );
        assert_eq!(attempts[0]["outcome"], "other");

        // A genuinely transient failure on the same candidate is a second,
        // independent series, never merged into the `other` count.
        m.record_resolution_attempt("gw", "gpt-5", "v3", ResolutionOutcome::Transient);
        let attempts = m.to_json()["resolution"]["attempts"].clone();
        let attempts = attempts.as_array().unwrap();
        let other = attempts.iter().find(|a| a["outcome"] == "other").unwrap();
        let transient = attempts
            .iter()
            .find(|a| a["outcome"] == "transient")
            .unwrap();
        assert_eq!(other["count"], 1);
        assert_eq!(transient["count"], 1);
    }

    // validation.md: In-Scope 5 / Story 5.1.2 — cumulative, never decrements.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn resolution_exhausted_total_should_remain_nonzero_after_a_subsequent_successful_resolution() {
        let m = ProxyMetrics::new();
        m.record_resolution_exhausted("model-gateway-openai", "gpt-5");

        let json = m.to_json();
        let exhausted = json["resolution"]["exhausted"].as_array().unwrap();
        assert_eq!(exhausted.len(), 1);
        assert_eq!(exhausted[0]["count"], 1);

        // A subsequent successful resolution for the same family must not
        // decrement or clear the cumulative counter — only the separate
        // `resolution_state` flag self-heals.
        m.record_resolution_success("gpt-5", ResolutionState::Newest, "gpt-5.3-codex");

        let json = m.to_json();
        let exhausted = json["resolution"]["exhausted"].as_array().unwrap();
        assert_eq!(
            exhausted[0]["count"], 1,
            "resolution_exhausted_total must never decrement on a later success"
        );
    }

    // validation.md: In-Scope 5 / Story 5.1.2 — state flag self-heals.
    #[test]
    fn resolution_state_should_reset_to_newest_or_fallback_after_exhaustion_followed_by_success() {
        let m = ProxyMetrics::new();
        m.record_resolution_exhausted("model-gateway-openai", "gpt-5");
        assert_eq!(
            m.to_json()["resolution"]["state"]["gpt-5"],
            json!("exhausted")
        );

        m.record_resolution_success("gpt-5", ResolutionState::Newest, "gpt-5.3-codex");
        assert_eq!(m.to_json()["resolution"]["state"]["gpt-5"], json!("newest"));

        m.record_resolution_exhausted("model-gateway-openai", "gpt-5");
        m.record_resolution_success("gpt-5", ResolutionState::Fallback, "gpt-5.2-codex");
        assert_eq!(
            m.to_json()["resolution"]["state"]["gpt-5"],
            json!("fallback")
        );
    }

    // Story 5.2.1: flat resolved_model/resolution_state/resolution_family
    // siblings on providers[upstream] — the dashboard's own per-upstream
    // rendering shape, distinct from (and additional to) the "resolution"
    // aggregate section above.
    #[test]
    fn to_json_should_expose_flat_resolution_fields_on_the_resolving_upstream() {
        let m = ProxyMetrics::new();
        m.record_request_success("model-gateway-openai", 100, 0);
        m.record_resolution_attempt(
            "model-gateway-openai",
            "gpt-5-codex",
            "gpt-5.3-codex",
            ResolutionOutcome::Success,
        );
        m.record_resolution_success("gpt-5-codex", ResolutionState::Newest, "gpt-5.3-codex");

        let json = m.to_json();
        let provider = &json["providers"]["model-gateway-openai"];
        assert_eq!(provider["resolved_model"], json!("gpt-5.3-codex"));
        assert_eq!(provider["resolution_state"], json!("newest"));
        assert_eq!(provider["resolution_family"], json!("gpt-5-codex"));
    }

    // Story 5.2.1: an exhausted family still names which family to check
    // (the JSON-side half of ux.md's "no dead end" rule), even though
    // `resolved_model` is no longer present (Task 5.2.1a/b acceptance
    // criteria).
    #[test]
    fn to_json_should_keep_resolution_family_but_drop_resolved_model_when_exhausted() {
        let m = ProxyMetrics::new();
        m.record_request_success("model-gateway-openai", 100, 0);
        m.record_resolution_attempt(
            "model-gateway-openai",
            "gpt-5-codex",
            "gpt-5.3-codex",
            ResolutionOutcome::Advance,
        );
        m.record_resolution_exhausted("model-gateway-openai", "gpt-5-codex");

        let json = m.to_json();
        let provider = &json["providers"]["model-gateway-openai"];
        assert_eq!(provider["resolution_state"], json!("exhausted"));
        assert_eq!(provider["resolution_family"], json!("gpt-5-codex"));
        assert!(
            provider.get("resolved_model").is_none(),
            "resolved_model must be absent (not null/stale) once the family is exhausted, got {provider:?}"
        );
    }

    // Story 5.2.1: a static-pin upstream (no `model_family`, so no
    // resolution walk ever runs against it) must not gain any of the new
    // fields — they're absent, not present-as-null.
    #[test]
    fn to_json_should_omit_resolution_fields_for_a_static_pin_upstream() {
        let m = ProxyMetrics::new();
        m.record_request_success("anthropic", 100, 0);

        let json = m.to_json();
        let provider = &json["providers"]["anthropic"];
        assert!(provider.get("resolved_model").is_none());
        assert!(provider.get("resolution_state").is_none());
        assert!(provider.get("resolution_family").is_none());
    }
}
