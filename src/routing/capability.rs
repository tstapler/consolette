//! Capability admission eval: prove a pinned model actually emits tool
//! calls before the router serves it.
//!
//! Background: `translate_openai_to_anthropic` once dropped `tools`
//! silently, and several free-tier models answer tool-shaped prompts with
//! narrated pseudo-XML (`<tool_call>…`) instead of function calls. Either
//! failure mode looks identical from the client (no tools ever execute),
//! so the router now probes each pinned free model with a synthetic
//! tool-call request and excludes fresh failures from dispatch until a
//! later round passes.
//!
//! Design notes:
//! - Probes travel the same `translate_openai_to_anthropic` →
//!   provider-`send` path as real `/v1/chat/completions` traffic, but via
//!   [`Router::probe_upstream`], which bypasses metrics/stats/health —
//!   eval traffic must never move production signals or trip cooldowns.
//! - Only route pins ending in `:free` are evaluated. Paid-model pins are
//!   left alone (probes cost real money); they stay admitted exactly as
//!   today. Unpinned upstreams have no model id to evaluate.
//! - Verdicts expire (`EVAL_TTL_SECS`): free lineups rotate and degraded
//!   models recover, so a `Fail` only excludes while fresh. Verdict
//!   `Unknown` (rate limits, timeouts, auth problems) never excludes —
//!   admission fails open, matching pre-eval behavior.
//! - When every candidate is freshly failed, dispatch also fails open
//!   (with a loud log) rather than refusing all traffic.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;

use crate::providers::{translate_openai_to_anthropic, ProviderError, ProviderResponse};

/// A distinctive tool name for probes: identifiable in logs, and never a
/// real client tool (a probe must not be mistaken for production traffic).
pub const EVAL_TOOL_NAME: &str = "get_time";

/// Best-of-N probes per model per round: one flaky miss must not exile a
/// working model.
pub const EVAL_PROBES_PER_ROUND: usize = 2;

/// Pause between a model's probes so a single burst doesn't read as two
/// independent samples.
pub const EVAL_PROBE_SPACING_SECS: u64 = 3;

/// Small output budget: a tool call fits easily; narration gets cut off.
pub const EVAL_MAX_TOKENS: u64 = 64;

/// Delay before the first evaluation round after startup (let the server
/// bind and the first real requests settle first).
pub const EVAL_STARTUP_DELAY_SECS: u64 = 60;

/// Interval between evaluation rounds.
pub const EVAL_INTERVAL_SECS: u64 = 1800;

/// How long a verdict gates admission.
pub const EVAL_TTL_SECS: u64 = 3600;

/// Admission verdict for one pinned model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityVerdict {
    /// The model emitted a real tool call during the last round.
    Pass,
    /// The model answered without calling (or the id is dead/delisted).
    /// The `String` is the human-readable reason surfaced in logs and
    /// `/metrics`.
    Fail { reason: String },
    /// The probe couldn't run to a decision (rate limit, timeout, auth,
    /// transport error). Never excludes; retried next round.
    Unknown,
}

/// Freshness-bounded admission verdicts, keyed by pinned model id.
///
/// Thread-safe via an internal `DashMap`; the same `Arc` is carried across
/// route hot-swaps (like `SessionOverrideStore`) so a rebuild doesn't
/// wipe learned verdicts.
pub struct CapabilityCache {
    inner: DashMap<String, (CapabilityVerdict, Instant)>,
    ttl: Duration,
}

impl CapabilityCache {
    #[must_use]
    pub fn new(ttl: Duration) -> Arc<Self> {
        Arc::new(Self {
            inner: DashMap::new(),
            ttl,
        })
    }

    /// The stored verdict, or `None` when absent or expired. Expired
    /// entries read as absent (fail-open); they are left in place and
    /// overwritten by the next round.
    #[must_use]
    pub fn get(&self, model_id: &str) -> Option<CapabilityVerdict> {
        let entry = self.inner.get(model_id)?;
        if entry.value().1.elapsed() >= self.ttl {
            return None;
        }
        Some(entry.value().0.clone())
    }

    /// Record a verdict, returning the previous *fresh* verdict (if any)
    /// so callers can log changes.
    #[must_use]
    pub fn set(&self, model_id: String, verdict: CapabilityVerdict) -> Option<CapabilityVerdict> {
        let previous = self.get(&model_id);
        self.inner.insert(model_id, (verdict, Instant::now()));
        previous
    }

    /// Admission check for dispatch: unpinned candidates (`None`) and
    /// anything but a fresh `Fail` are admitted.
    #[must_use]
    pub fn is_admitted(&self, model: &Option<String>) -> bool {
        match model {
            None => true,
            Some(id) => !matches!(self.get(id), Some(CapabilityVerdict::Fail { .. })),
        }
    }

    /// `/metrics`-facing snapshot: every model with a fresh verdict.
    #[must_use]
    pub fn snapshot(&self) -> serde_json::Value {
        let mut result = serde_json::Map::new();
        for entry in &self.inner {
            let (verdict, at) = entry.value();
            if at.elapsed() >= self.ttl {
                continue;
            }
            let (status, reason) = match verdict {
                CapabilityVerdict::Pass => ("pass", None),
                CapabilityVerdict::Fail { reason } => ("fail", Some(reason.clone())),
                CapabilityVerdict::Unknown => ("unknown", None),
            };
            let mut obj = serde_json::Map::new();
            obj.insert(
                "status".to_string(),
                serde_json::Value::String(status.to_string()),
            );
            if let Some(reason) = reason {
                obj.insert("reason".to_string(), serde_json::Value::String(reason));
            }
            result.insert(entry.key().clone(), serde_json::Value::Object(obj));
        }
        serde_json::Value::Object(result)
    }
}

/// Build the synthetic probe: `OpenAI` shape (the same shape real
/// `/v1/chat/completions` clients send), one trivial tool, a blunt
/// instruction, non-streaming so the verdict can be read off the full
/// response body.
#[must_use]
pub fn tool_probe_request(model_id: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model_id,
        "messages": [{
            "role": "user",
            "content": "Call the get_time tool with zone utc. Make the call now; do not narrate or explain."
        }],
        "tools": [{
            "type": "function",
            "function": {
                "name": EVAL_TOOL_NAME,
                "description": "Get the current time",
                "parameters": {
                    "type": "object",
                    "properties": {"zone": {"type": "string"}}
                }
            }
        }],
        "tool_choice": "auto",
        "max_tokens": EVAL_MAX_TOKENS,
        "stream": false
    })
}

/// True when an Anthropic-shaped provider response contains a tool call
/// for the probe tool. Text-only replies — including narrated pseudo-XML
/// (`<tool_call>…`) — fail: the defect under test is precisely models
/// that describe calling instead of calling.
#[must_use]
pub fn probe_passed(response: &ProviderResponse) -> bool {
    let ProviderResponse::Full(body) = response else {
        return false;
    };
    let Some(content) = body.get("content").and_then(serde_json::Value::as_array) else {
        return false;
    };
    content.iter().any(|block| {
        block.get("type").and_then(serde_json::Value::as_str) == Some("tool_use")
            && block.get("name").and_then(serde_json::Value::as_str) == Some(EVAL_TOOL_NAME)
    })
}

/// Classify a probe dispatch error into a verdict. Only hard failures
/// exile: a dead/delisted id (`Validation`, `ModelUnsupported`) or an
/// upstream 4xx. Rate limits, timeouts, auth problems, transport errors
/// and shape mismatches are `Unknown` — the model may be fine, the
/// network/key/quota is not, and admission must fail open.
#[must_use]
pub fn error_verdict(error: &ProviderError) -> CapabilityVerdict {
    match error {
        ProviderError::Validation(message, _) | ProviderError::ModelUnsupported(message) => {
            CapabilityVerdict::Fail {
                reason: format!("upstream rejected the model id: {message}"),
            }
        }
        ProviderError::Upstream { status, body } if (400..500).contains(status) => {
            CapabilityVerdict::Fail {
                reason: format!("upstream {status}: {body}"),
            }
        }
        _ => CapabilityVerdict::Unknown,
    }
}

/// Probe one pinned model through `router` (translation + provider send,
/// bypassing metrics/stats/health) and return its verdict. Best-of
/// [`EVAL_PROBES_PER_ROUND`]: any emitted tool call passes.
pub async fn evaluate_model(
    router: &super::router::Router,
    index: usize,
    model_id: &str,
) -> CapabilityVerdict {
    // A round with no model answer at all (every probe rate-limited,
    // timed out, or otherwise inconclusive) is `Unknown`, not `Fail`:
    // exiling a model for quota noise would punish it for the network,
    // not for its tool behavior.
    let mut saw_answer = false;
    for attempt in 0..EVAL_PROBES_PER_ROUND {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(EVAL_PROBE_SPACING_SECS)).await;
        }
        let translated = translate_openai_to_anthropic(&tool_probe_request(model_id));
        let mut attempt_body = translated;
        if let Some(obj) = attempt_body.as_object_mut() {
            obj.insert(
                "model".to_string(),
                serde_json::Value::String(model_id.to_string()),
            );
        }
        match router.probe_upstream(index, attempt_body).await {
            Ok(response) if probe_passed(&response) => return CapabilityVerdict::Pass,
            Ok(_) => {
                saw_answer = true;
                // A clean answer with no call is signal, not noise — but a
                // single miss could be sampling luck, so later attempts in
                // this round may still rescue the verdict.
            }
            Err(error) => {
                let verdict = error_verdict(&error);
                if matches!(verdict, CapabilityVerdict::Fail { .. }) {
                    return verdict;
                }
                tracing::warn!(
                    model = model_id,
                    error = %error,
                    "capability probe inconclusive; verdict stays undecided for this round"
                );
            }
        }
    }
    if !saw_answer {
        return CapabilityVerdict::Unknown;
    }
    CapabilityVerdict::Fail {
        reason: "model answered without emitting a tool call".to_string(),
    }
}

/// Evaluate every eval-eligible pinned model on `router` and record
/// verdicts, logging changes. Eligible: pinned ids ending in `:free`
/// (paid pins are never probed — probes cost real money).
pub async fn evaluate_round(router: &super::router::Router) {
    let mut passed = 0usize;
    let mut failed = 0usize;
    for (index, model_id) in router.eval_targets() {
        let verdict = evaluate_model(router, index, &model_id).await;
        let changed = router.capability().set(model_id.clone(), verdict.clone());
        match (&changed, &verdict) {
            (Some(previous), current) if previous == current => {}
            _ => tracing::info!(
                model = %model_id,
                previous = ?changed,
                verdict = ?verdict,
                "capability verdict updated"
            ),
        }
        match verdict {
            CapabilityVerdict::Pass => passed += 1,
            CapabilityVerdict::Fail { .. } => failed += 1,
            CapabilityVerdict::Unknown => {}
        }
    }
    tracing::info!(passed, failed, "capability evaluation round complete");
}

/// Background loop: an initial round shortly after startup, then one per
/// [`EVAL_INTERVAL_SECS`]. Always reads the *current* router out of the
/// `ArcSwap`, so route hot-swaps take effect on the next round without a
/// restart.
pub async fn run_eval_loop(dispatch_router: Arc<arc_swap::ArcSwap<super::router::Router>>) {
    tokio::time::sleep(Duration::from_secs(EVAL_STARTUP_DELAY_SECS)).await;
    loop {
        evaluate_round(&dispatch_router.load()).await;
        tokio::time::sleep(Duration::from_secs(EVAL_INTERVAL_SECS)).await;
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Scripted provider for `evaluate_model` tests: replays queued
    /// `Full` bodies or errors in order.
    struct ScriptedProvider {
        script: Mutex<VecDeque<Result<serde_json::Value, ProviderError>>>,
    }

    #[async_trait::async_trait]
    impl crate::providers::Provider for ScriptedProvider {
        fn name(&self) -> &'static str {
            "scripted"
        }

        async fn send(
            &self,
            _body: serde_json::Value,
            _headers: http::HeaderMap,
            _stream: bool,
        ) -> Result<ProviderResponse, ProviderError> {
            self.script
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .expect("script exhausted")
                .map(ProviderResponse::Full)
        }

        async fn list_models(&self) -> Result<Vec<crate::providers::ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }
    }

    fn tool_use_body() -> serde_json::Value {
        serde_json::json!({
            "content": [{"type": "tool_use", "id": "call_1", "name": EVAL_TOOL_NAME, "input": {"zone": "utc"}}]
        })
    }

    fn text_body() -> serde_json::Value {
        serde_json::json!({"content": [{"type": "text", "text": "the time is noon"}]})
    }

    fn eval_router(
        script: Vec<Result<serde_json::Value, ProviderError>>,
    ) -> super::super::router::Router {
        use std::sync::Arc;

        super::super::router::Router::new(super::super::router::RouterDeps {
            candidates: vec![],
            providers: vec![Arc::new(ScriptedProvider {
                script: Mutex::new(script.into()),
            })],
            strategy: Arc::new(super::super::strategy::FallbackStrategy),
            health: Arc::new(super::super::health::HealthRegistry::new(300)),
            admission: Arc::new(crate::ratelimit::RateLimiters::new(
                &crate::config::schema::RateLimitConfig::default(),
            )),
            metrics: crate::metrics::MetricsCollector::new(),
        })
    }

    #[tokio::test]
    async fn evaluate_model_passes_on_an_emitted_tool_call() {
        let router = eval_router(vec![Ok(tool_use_body())]);

        assert_eq!(
            evaluate_model(&router, 0, "m:free").await,
            CapabilityVerdict::Pass
        );
    }

    #[tokio::test]
    async fn evaluate_model_fails_clean_answers_without_calls() {
        let router = eval_router(vec![Ok(text_body()), Ok(text_body())]);

        assert!(matches!(
            evaluate_model(&router, 0, "m:free").await,
            CapabilityVerdict::Fail { .. }
        ));
    }

    #[tokio::test]
    async fn evaluate_model_stays_undecided_when_no_probe_answered() {
        // Regression: all-inconclusive rounds (e.g. every probe rate
        // limited) must not exile a model for quota noise.
        let router = eval_router(vec![
            Err(ProviderError::RateLimited),
            Err(ProviderError::Timeout),
        ]);

        assert_eq!(
            evaluate_model(&router, 0, "m:free").await,
            CapabilityVerdict::Unknown
        );
    }

    #[test]
    fn probe_request_is_a_non_streaming_tool_call_test() {
        let probe = tool_probe_request("some/model:free");

        assert_eq!(probe["model"], serde_json::json!("some/model:free"));
        assert_eq!(probe["stream"], serde_json::json!(false));
        assert_eq!(probe["tool_choice"], serde_json::json!("auto"));
        let tools = probe["tools"].as_array().expect("probe has tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(
            tools[0]["function"]["name"],
            serde_json::json!(EVAL_TOOL_NAME)
        );
    }

    #[test]
    fn probe_passed_requires_a_tool_use_for_the_probe_tool() {
        let hit = ProviderResponse::Full(serde_json::json!({
            "content": [
                {"type": "text", "text": "calling now"},
                {"type": "tool_use", "id": "call_1", "name": EVAL_TOOL_NAME, "input": {"zone": "utc"}}
            ]
        }));
        assert!(probe_passed(&hit));

        let narrated = ProviderResponse::Full(serde_json::json!({
            "content": [{"type": "text", "text": "<tool_call>get_time<arg_key>zone</arg_key></tool_call>"}]
        }));
        assert!(!probe_passed(&narrated));

        let wrong_tool = ProviderResponse::Full(serde_json::json!({
            "content": [{"type": "tool_use", "id": "call_9", "name": "other_tool", "input": {}}]
        }));
        assert!(!probe_passed(&wrong_tool));
    }

    #[test]
    fn error_verdict_exiles_only_hard_failures() {
        assert!(matches!(
            error_verdict(&ProviderError::Validation("bad id".to_string(), 400)),
            CapabilityVerdict::Fail { .. }
        ));
        assert!(matches!(
            error_verdict(&ProviderError::ModelUnsupported("gone".to_string())),
            CapabilityVerdict::Fail { .. }
        ));
        assert!(matches!(
            error_verdict(&ProviderError::Upstream {
                status: 404,
                body: String::new()
            }),
            CapabilityVerdict::Fail { .. }
        ));
        // Everything transient or environmental stays undecided.
        for error in [
            ProviderError::RateLimited,
            ProviderError::Timeout,
            ProviderError::Auth("no key".to_string()),
            ProviderError::Upstream {
                status: 500,
                body: String::new(),
            },
            ProviderError::Exhausted,
        ] {
            assert!(
                matches!(error_verdict(&error), CapabilityVerdict::Unknown),
                "expected Unknown for {error:?}"
            );
        }
    }

    #[test]
    fn cache_admits_everything_but_fresh_failures() {
        let cache = CapabilityCache::new(Duration::from_mins(1));

        assert!(cache.is_admitted(&None));
        assert!(cache.is_admitted(&Some("unseen/model".to_string())));

        let _ = cache.set("ok/model:free".to_string(), CapabilityVerdict::Pass);
        assert!(cache.is_admitted(&Some("ok/model:free".to_string())));

        let _ = cache.set("flaky/model:free".to_string(), CapabilityVerdict::Unknown);
        assert!(cache.is_admitted(&Some("flaky/model:free".to_string())));

        let _ = cache.set(
            "bad/model:free".to_string(),
            CapabilityVerdict::Fail {
                reason: "no call".to_string(),
            },
        );
        assert!(!cache.is_admitted(&Some("bad/model:free".to_string())));
    }

    #[test]
    fn cache_treats_expired_verdicts_as_absent() {
        let cache = CapabilityCache::new(Duration::ZERO);
        let _ = cache.set(
            "stale/model:free".to_string(),
            CapabilityVerdict::Fail {
                reason: "old news".to_string(),
            },
        );

        assert!(cache.get("stale/model:free").is_none());
        assert!(cache.is_admitted(&Some("stale/model:free".to_string())));
        assert_eq!(
            cache.snapshot(),
            serde_json::json!({}),
            "expired verdicts stay out of /metrics"
        );
    }

    #[test]
    fn snapshot_reports_fresh_verdicts_with_reasons() {
        let cache = CapabilityCache::new(Duration::from_mins(1));
        let _ = cache.set("a/m:free".to_string(), CapabilityVerdict::Pass);
        let _ = cache.set(
            "b/m:free".to_string(),
            CapabilityVerdict::Fail {
                reason: "narrated".to_string(),
            },
        );

        assert_eq!(
            cache.snapshot(),
            serde_json::json!({
                "a/m:free": {"status": "pass"},
                "b/m:free": {"status": "fail", "reason": "narrated"}
            })
        );
    }
}
