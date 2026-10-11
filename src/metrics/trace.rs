//! Terminal trace ownership (ADR-002): a [`TraceScope`] guarantees that every
//! request which got a ring-buffer entry ends with exactly one `RequestTrace`
//! event, whether it finishes, fails, or has its future dropped mid-flight.
//!
//! The wrapper that owns a request holds the scope as a plain local and lends
//! `&mut TraceScope` to inner code; nothing else ever owns or clones it.
//! Layering: this module imports only `metrics` siblings, std and serde.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::ids::UpstreamName;
use super::{MetricsCollector, RequestDetail, RequestTimingUpdate, ScopeFinish};
use crate::entrypoint::events::TraceKind;

/// How one upstream attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    Ok,
    RateLimited,
    Timeout,
    Auth,
    Validation,
    ShapeMismatch,
    Upstream,
    Shed,
    Cancelled,
}

/// Closed, sanitized failure class of an attempt; derived from the error variant and status,
/// never from error text, so it cannot carry upstream content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptErrorClass {
    Auth,
    RateLimited,
    Overloaded,
    Timeout,
    ServerError,
    Unreachable,
    ClientError,
    BadResponse,
    Shed,
    Cancelled,
}

/// One upstream attempt of a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub iteration: u8,
    pub upstream: UpstreamName,
    pub model: Option<String>,
    pub outcome: AttemptOutcome,
    /// `None` for an `Ok` attempt.
    pub error_class: Option<AttemptErrorClass>,
    pub status: Option<u16>,
    pub latency_ms: u64,
}

/// Most attempts kept per request; later ones are dropped and flagged.
pub const MAX_ATTEMPTS: usize = 16;

/// Bounded attempt list owned by one scope (mutated under `&mut`, no lock).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttemptLog {
    records: Vec<AttemptRecord>,
    truncated: bool,
    iterations: u8,
}

impl AttemptLog {
    pub fn push(&mut self, record: AttemptRecord) {
        if self.records.len() < MAX_ATTEMPTS {
            self.records.push(record);
        } else {
            self.truncated = true;
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    #[must_use]
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    #[must_use]
    pub fn records(&self) -> &[AttemptRecord] {
        &self.records
    }

    #[must_use]
    pub fn iterations(&self) -> u8 {
        self.iterations
    }
}

/// Why the request went where it did; one row chip per request (ADR-004 item 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteReason {
    SessionPin,
    /// Carries the `RoutingStrategy::name()` that chose the upstream.
    Strategy(&'static str),
    Exploration,
    Fallback,
    RateLimitHold,
    Default,
    Cache,
    Local,
}

/// Plain values `RouteReason::for_request` decides from; no router or strategy types.
#[derive(Debug, Clone, Copy)]
pub struct RouteInputs<'a> {
    pub kind: TraceKind,
    pub session_pinned: bool,
    pub strategy_name: &'static str,
    pub exploration: bool,
    pub hold_retries: u8,
    pub attempts: &'a [AttemptRecord],
}

/// The ordered `fallback` strategy (and an unnamed one) is the default route, not a decision.
const DEFAULT_ROUTE_STRATEGY: &str = "fallback";

impl RouteReason {
    /// Precedence: Cache/Local > SessionPin > Exploration > Fallback > RateLimitHold > Strategy > Default.
    #[must_use]
    pub fn for_request(inputs: &RouteInputs<'_>) -> Self {
        match inputs.kind {
            TraceKind::CacheHit => return Self::Cache,
            TraceKind::Local => return Self::Local,
            TraceKind::Routed => {}
        }
        // Earlier attempts always failed or were shed, since the loop stops at the first success.
        let followed_a_failure = inputs.attempts.len() > 1;
        if inputs.session_pinned {
            Self::SessionPin
        } else if inputs.exploration {
            Self::Exploration
        } else if followed_a_failure {
            Self::Fallback
        } else if inputs.hold_retries > 0 {
            Self::RateLimitHold
        } else if inputs.strategy_name.is_empty() || inputs.strategy_name == DEFAULT_ROUTE_STRATEGY
        {
            Self::Default
        } else {
            Self::Strategy(inputs.strategy_name)
        }
    }

    #[must_use]
    pub fn wire(self) -> &'static str {
        match self {
            Self::SessionPin => "session_pin",
            Self::Strategy(_) => "strategy",
            Self::Exploration => "exploration",
            Self::Fallback => "fallback",
            Self::RateLimitHold => "rate_limit_hold",
            Self::Default => "default",
            Self::Cache => "cache",
            Self::Local => "local",
        }
    }

    /// The strategy name, set only for [`Self::Strategy`].
    #[must_use]
    pub fn strategy(self) -> Option<&'static str> {
        match self {
            Self::Strategy(name) => Some(name),
            _ => None,
        }
    }
}

impl Serialize for RouteReason {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.wire())
    }
}

/// What a request ended with; carries the HTTP status of its terminal trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestOutcome {
    status_code: u16,
}

impl RequestOutcome {
    #[must_use]
    pub const fn ok() -> Self {
        Self {
            status_code: OK_STATUS,
        }
    }

    #[must_use]
    pub const fn status(status_code: u16) -> Self {
        Self { status_code }
    }

    #[must_use]
    pub const fn status_code(self) -> u16 {
        self.status_code
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeState {
    Active,
    Finished,
}

const OK_STATUS: u16 = 200;
const CANCELLED_STATUS: u16 = 499;
const UNFINISHED_STATUS: u16 = 500;

/// Owns one request's terminal trace. Call [`finish`](Self::finish) then
/// [`mark_handler_returned`](Self::mark_handler_returned) on every normal exit;
/// `Drop` covers the rest (499 when the future was dropped, 500 when the
/// handler returned or panicked without finishing).
pub struct TraceScope {
    metrics: Arc<MetricsCollector>,
    request_id: String,
    state: ScopeState,
    handler_returned: bool,
    started: Instant,
    attempts: AttemptLog,
    provider: String,
    selected_model: Option<String>,
    selected_model_was_exploration: Option<bool>,
}

impl TraceScope {
    /// Creates the request's ring-buffer entry (and caches its original body).
    #[must_use]
    pub fn begin(
        metrics: &Arc<MetricsCollector>,
        body: &serde_json::Value,
        stream: bool,
        tokens_before: u64,
    ) -> Self {
        let request_id = uuid::Uuid::new_v4().to_string();
        metrics.push_request(RequestDetail::from_body(
            request_id.clone(),
            stream,
            tokens_before,
            body,
            crate::session::extract_session_id(body),
        ));
        metrics.push_original_body(request_id.clone(), body.clone());
        Self {
            metrics: Arc::clone(metrics),
            request_id,
            state: ScopeState::Active,
            handler_returned: false,
            started: Instant::now(),
            attempts: AttemptLog::default(),
            provider: String::new(),
            selected_model: None,
            selected_model_was_exploration: None,
        }
    }

    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    #[must_use]
    pub fn attempts(&self) -> &[AttemptRecord] {
        self.attempts.records()
    }

    /// Records the candidate just chosen; the last selection wins across retries.
    pub fn record_selection(
        &mut self,
        selected_model: Option<String>,
        was_exploration: Option<bool>,
    ) {
        self.selected_model = selected_model;
        self.selected_model_was_exploration = was_exploration;
    }

    pub fn record_attempt(&mut self, attempt: AttemptRecord) {
        self.attempts.push(attempt);
    }

    /// Names the upstream that produced the response (left empty on failure).
    pub fn record_provider(&mut self, provider: &str) {
        self.provider = provider.to_string();
    }

    /// Normal terminal exit; later calls and the eventual `Drop` emit nothing.
    pub fn finish(&mut self, outcome: RequestOutcome) {
        self.emit(outcome.status_code);
    }

    /// Marks that the owning handler produced its response, so an unfinished
    /// drop afterwards is a bug (500), not a client abort (499).
    pub fn mark_handler_returned(&mut self) {
        self.handler_returned = true;
    }

    fn emit(&mut self, status_code: u16) {
        if self.state == ScopeState::Finished {
            return;
        }
        self.state = ScopeState::Finished;
        let duration_ms = self.started.elapsed().as_secs_f64() * 1000.0;
        let first_byte_ms = if self.provider.is_empty() {
            0.0
        } else {
            duration_ms
        };
        self.metrics.finish_scope(
            &self.request_id,
            ScopeFinish {
                status_code,
                timing: RequestTimingUpdate {
                    provider: &self.provider,
                    duration_ms,
                    first_byte_ms,
                    bedrock_invocation_ms: 0,
                    bedrock_first_byte_ms: 0,
                },
                selected_model: self.selected_model.take(),
                selected_model_was_exploration: self.selected_model_was_exploration,
            },
        );
    }
}

#[cfg(test)]
thread_local! {
    static UNFINISHED_PATH_HITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Test hook: how many scopes on this thread reached the unfinished (bug) path,
/// so handler tests can assert it stays untouched.
#[cfg(test)]
pub(crate) fn unfinished_path_hits() -> u64 {
    UNFINISHED_PATH_HITS.with(std::cell::Cell::get)
}

/// Must never panic: a panic in `Drop` while unwinding aborts the process.
impl Drop for TraceScope {
    fn drop(&mut self) {
        if self.state == ScopeState::Finished {
            return;
        }
        let counters = &self.metrics.counters;
        if self.handler_returned || std::thread::panicking() {
            counters
                .trace_unfinished_total
                .fetch_add(1, Ordering::Relaxed);
            #[cfg(test)]
            UNFINISHED_PATH_HITS.with(|hits| hits.set(hits.get() + 1));
            tracing::error!(
                request_id = %self.request_id,
                "TraceScope dropped unfinished after its handler returned or panicked"
            );
            self.emit(UNFINISHED_STATUS);
        } else {
            counters
                .trace_cancelled_total
                .fetch_add(1, Ordering::Relaxed);
            self.emit(CANCELLED_STATUS);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    fn attempt(upstream: &str, outcome: AttemptOutcome) -> AttemptRecord {
        AttemptRecord {
            iteration: 0,
            upstream: upstream.into(),
            model: None,
            outcome,
            error_class: None,
            status: None,
            latency_ms: 0,
        }
    }

    #[test]
    fn attempt_log_should_cap_at_16_keep_first_16_in_order_and_flag_truncation() {
        let mut log = AttemptLog::default();
        assert!(!log.truncated());
        for i in 0..20_u64 {
            let mut record = attempt("bedrock", AttemptOutcome::RateLimited);
            record.status = Some(429);
            record.latency_ms = i;
            log.push(record);
        }
        assert_eq!(log.len(), 16);
        assert!(log.truncated());
        let latencies: Vec<u64> = log.records().iter().map(|r| r.latency_ms).collect();
        assert_eq!(latencies, (0..16).collect::<Vec<u64>>());
    }

    #[test]
    fn attempt_log_should_never_exceed_16_entries_under_20_pushes() {
        let mut log = AttemptLog::default();
        for _ in 0..20 {
            log.push(attempt("a", AttemptOutcome::Ok));
            assert!(log.len() <= MAX_ATTEMPTS);
        }
        assert_eq!(MAX_ATTEMPTS, 16);
    }

    fn inputs(attempts: &[AttemptRecord]) -> RouteInputs<'_> {
        RouteInputs {
            kind: TraceKind::Routed,
            session_pinned: false,
            strategy_name: "weighted",
            exploration: false,
            hold_retries: 0,
            attempts,
        }
    }

    #[test]
    fn route_reason_for_request_should_follow_precedence_cache_local_pin_explore_fallback_hold_strategy_default(
    ) {
        let ok = [attempt("a", AttemptOutcome::Ok)];
        let failed_then_ok = [
            attempt("a", AttemptOutcome::RateLimited),
            attempt("b", AttemptOutcome::Ok),
        ];
        let shed_then_ok = [
            attempt("a", AttemptOutcome::Shed),
            attempt("b", AttemptOutcome::Ok),
        ];
        // Cache and Local beat everything else.
        for (kind, want) in [
            (TraceKind::CacheHit, RouteReason::Cache),
            (TraceKind::Local, RouteReason::Local),
        ] {
            let all_flags = RouteInputs {
                kind,
                session_pinned: true,
                exploration: true,
                hold_retries: 2,
                ..inputs(&failed_then_ok)
            };
            assert_eq!(RouteReason::for_request(&all_flags), want);
        }
        let pinned = RouteInputs {
            session_pinned: true,
            exploration: true,
            ..inputs(&failed_then_ok)
        };
        assert_eq!(RouteReason::for_request(&pinned), RouteReason::SessionPin);
        let exploring = RouteInputs {
            exploration: true,
            ..inputs(&failed_then_ok)
        };
        assert_eq!(
            RouteReason::for_request(&exploring),
            RouteReason::Exploration
        );
        assert_eq!(
            RouteReason::for_request(&inputs(&failed_then_ok)),
            RouteReason::Fallback
        );
        assert_eq!(
            RouteReason::for_request(&inputs(&shed_then_ok)),
            RouteReason::Fallback
        );
        // Hold with no failed attempt is exactly RateLimitHold; with one it is Fallback.
        let held = RouteInputs {
            hold_retries: 2,
            ..inputs(&ok)
        };
        assert_eq!(RouteReason::for_request(&held), RouteReason::RateLimitHold);
        let held_after_failure = RouteInputs {
            hold_retries: 2,
            ..inputs(&failed_then_ok)
        };
        assert_eq!(
            RouteReason::for_request(&held_after_failure),
            RouteReason::Fallback
        );
        // A single failed attempt has no successor, so it is not a fallback.
        let lone_failure = [attempt("a", AttemptOutcome::Upstream)];
        assert_eq!(
            RouteReason::for_request(&inputs(&lone_failure)),
            RouteReason::Strategy("weighted")
        );
        assert_eq!(
            RouteReason::for_request(&inputs(&ok)),
            RouteReason::Strategy("weighted")
        );
        // The ordered fallback strategy is the default route, an empty name too.
        for name in ["fallback", ""] {
            let ordered = RouteInputs {
                strategy_name: name,
                ..inputs(&ok)
            };
            assert_eq!(RouteReason::for_request(&ordered), RouteReason::Default);
        }
    }

    /// Exhaustive on purpose: a new `RouteReason` variant fails to compile here.
    fn expected_wire(reason: RouteReason) -> (&'static str, Option<&'static str>) {
        match reason {
            RouteReason::SessionPin => ("session_pin", None),
            RouteReason::Strategy(name) => ("strategy", Some(name)),
            RouteReason::Exploration => ("exploration", None),
            RouteReason::Fallback => ("fallback", None),
            RouteReason::RateLimitHold => ("rate_limit_hold", None),
            RouteReason::Default => ("default", None),
            RouteReason::Cache => ("cache", None),
            RouteReason::Local => ("local", None),
        }
    }

    #[test]
    fn route_reason_should_serialize_to_exact_wire_strings_session_pin_strategy_exploration_fallback_rate_limit_hold_default_cache_local_via_exhaustive_helper(
    ) {
        let all = [
            RouteReason::SessionPin,
            RouteReason::Strategy("weighted"),
            RouteReason::Exploration,
            RouteReason::Fallback,
            RouteReason::RateLimitHold,
            RouteReason::Default,
            RouteReason::Cache,
            RouteReason::Local,
        ];
        for reason in all {
            let (wire, strategy) = expected_wire(reason);
            assert_eq!(serde_json::to_value(reason).unwrap(), wire);
            assert_eq!(reason.wire(), wire);
            assert_eq!(reason.strategy(), strategy);
        }
    }

    use super::*;
    use crate::entrypoint::events::{DashboardEvent, RequestTraceData};

    fn scope_with_events() -> (
        Arc<MetricsCollector>,
        tokio::sync::broadcast::Receiver<DashboardEvent>,
        TraceScope,
    ) {
        let metrics = MetricsCollector::new();
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        metrics.set_event_tx(tx);
        let scope = TraceScope::begin(&metrics, &serde_json::json!({"model": "m"}), false, 7);
        (metrics, rx, scope)
    }

    fn traces(rx: &mut tokio::sync::broadcast::Receiver<DashboardEvent>) -> Vec<RequestTraceData> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let DashboardEvent::RequestTrace(t) = event {
                out.push(t);
            }
        }
        out
    }

    fn counter(
        metrics: &MetricsCollector,
        which: fn(&MetricsCollector) -> &std::sync::atomic::AtomicU64,
    ) -> u64 {
        which(metrics).load(Ordering::Relaxed)
    }

    #[test]
    fn begin_should_create_ring_buffer_entry_with_pending_status_zero() {
        let (metrics, _rx, scope) = scope_with_events();
        let entry = &metrics.get_recent_requests(1)[0];
        assert_eq!(entry.request_id, scope.request_id());
        assert_eq!(entry.status_code, 0);
        assert_eq!(entry.tokens_before, 7);
        assert!(metrics.get_original_body(scope.request_id()).is_some());
    }

    #[test]
    fn finish_then_drop_should_emit_exactly_one_event() {
        let (metrics, mut rx, mut scope) = scope_with_events();
        scope.record_provider("up");
        scope.finish(RequestOutcome::ok());
        scope.mark_handler_returned();
        drop(scope);

        let t = traces(&mut rx);
        assert_eq!(t.len(), 1, "{t:?}");
        assert_eq!(t[0].status_code, 200);
        assert_eq!(t[0].provider, "up");
        assert_eq!(metrics.get_recent_requests(1)[0].status_code, 200);
        assert_eq!(counter(&metrics, |m| &m.counters.trace_cancelled_total), 0);
        assert_eq!(counter(&metrics, |m| &m.counters.trace_unfinished_total), 0);
    }

    #[test]
    fn finish_should_write_selection_into_ring_buffer_entry() {
        let (metrics, _rx, mut scope) = scope_with_events();
        scope.record_selection(Some("a".into()), Some(false));
        scope.record_selection(Some("b".into()), Some(true));
        scope.finish(RequestOutcome::ok());

        let entry = &metrics.get_recent_requests(1)[0];
        assert_eq!(entry.selected_model.as_deref(), Some("b"));
        assert_eq!(entry.selected_model_was_exploration, Some(true));
    }

    #[test]
    fn drop_should_emit_499_and_count_cancelled_when_unfinished_and_handler_not_returned() {
        let (metrics, mut rx, mut scope) = scope_with_events();
        scope.record_attempt(attempt("a", AttemptOutcome::RateLimited));
        drop(scope);

        let t = traces(&mut rx);
        assert_eq!(t.len(), 1, "{t:?}");
        assert_eq!(t[0].status_code, 499);
        assert_eq!(metrics.get_recent_requests(1)[0].status_code, 499);
        assert_eq!(counter(&metrics, |m| &m.counters.trace_cancelled_total), 1);
        assert_eq!(counter(&metrics, |m| &m.counters.trace_unfinished_total), 0);
    }

    #[test]
    fn drop_should_emit_500_and_count_unfinished_when_handler_returned() {
        let (metrics, mut rx, mut scope) = scope_with_events();
        scope.mark_handler_returned();
        drop(scope);

        let t = traces(&mut rx);
        assert_eq!(t.len(), 1, "{t:?}");
        assert_eq!(t[0].status_code, 500);
        assert_eq!(counter(&metrics, |m| &m.counters.trace_unfinished_total), 1);
        assert_eq!(counter(&metrics, |m| &m.counters.trace_cancelled_total), 0);
    }

    #[test]
    fn drop_should_emit_500_and_count_unfinished_when_panicking() {
        let (metrics, mut rx, scope) = scope_with_events();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _held = scope;
            panic!("handler panic");
        }));
        assert!(result.is_err());

        let t = traces(&mut rx);
        assert_eq!(t.len(), 1, "{t:?}");
        assert_eq!(t[0].status_code, 500);
        assert_eq!(counter(&metrics, |m| &m.counters.trace_unfinished_total), 1);
        assert_eq!(counter(&metrics, |m| &m.counters.trace_cancelled_total), 0);
    }

    #[test]
    fn unfinished_scope_should_keep_recorded_attempts_without_adding_cancelled_one() {
        let (_metrics, _rx, mut scope) = scope_with_events();
        let failed = attempt("a", AttemptOutcome::Upstream);
        scope.record_attempt(failed.clone());
        assert_eq!(scope.attempts(), [failed]);
    }

    /// Emulation-loop stand-in: `ScopedDispatch` (Story 2.1.6) will lend the
    /// scope as `tokio::sync::Mutex<&mut TraceScope>`; the same borrow shape
    /// is used here so ordering and unwinding are exercised at scope level.
    async fn inner_borrowing_scope(
        scope: &mut TraceScope,
        then: impl std::future::Future<Output = RequestOutcome>,
    ) -> RequestOutcome {
        let shared = tokio::sync::Mutex::new(scope);
        {
            let mut guard = shared.lock().await;
            guard.record_attempt(attempt("round-1", AttemptOutcome::Upstream));
        }
        let _held = shared.lock().await;
        then.await
    }

    async fn wrapper(
        scope: &mut TraceScope,
        then: impl std::future::Future<Output = RequestOutcome>,
    ) {
        let outcome = inner_borrowing_scope(scope, then).await;
        scope.finish(outcome);
        scope.mark_handler_returned();
    }

    fn current_thread_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
    }

    #[test]
    fn wrapper_finish_should_emit_failure_status_when_inner_fails_after_emulation_loop_started() {
        let (metrics, mut rx, mut scope) = scope_with_events();
        current_thread_runtime()
            .block_on(wrapper(&mut scope, async { RequestOutcome::status(502) }));
        assert_eq!(scope.attempts().len(), 1, "loop attempts survive to finish");
        drop(scope);

        let t = traces(&mut rx);
        assert_eq!(t.len(), 1, "{t:?}");
        assert_eq!(t[0].status_code, 502);
        assert_eq!(counter(&metrics, |m| &m.counters.trace_cancelled_total), 0);
        assert_eq!(counter(&metrics, |m| &m.counters.trace_unfinished_total), 0);
    }

    #[test]
    fn drop_should_emit_single_cancelled_trace_when_future_dropped_while_scoped_dispatch_mid_dispatch(
    ) {
        use futures_util::FutureExt;
        let (metrics, mut rx, scope) = scope_with_events();
        let mut owned = scope;
        {
            // The wrapper future borrows `owned`; dropping it ends the borrow
            // before `owned` itself drops, which is what the compiler enforces.
            let fut = wrapper(&mut owned, std::future::pending());
            assert!(fut.now_or_never().is_none(), "dispatch is still in flight");
        }
        assert!(traces(&mut rx).is_empty(), "no trace while the scope lives");
        drop(owned);

        let t = traces(&mut rx);
        assert_eq!(t.len(), 1, "{t:?}");
        assert_eq!(t[0].status_code, 499);
        assert_eq!(counter(&metrics, |m| &m.counters.trace_cancelled_total), 1);
        assert_eq!(counter(&metrics, |m| &m.counters.trace_unfinished_total), 0);
    }

    #[test]
    fn drop_should_emit_one_unfinished_trace_when_inner_panics_holding_scoped_dispatch_guard() {
        let (metrics, mut rx, scope) = scope_with_events();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut scope = scope;
            current_thread_runtime().block_on(wrapper(&mut scope, async {
                panic!("inner panics under the guard")
            }));
        }));
        assert!(result.is_err());

        let t = traces(&mut rx);
        assert_eq!(t.len(), 1, "{t:?}");
        assert_eq!(t[0].status_code, 500);
        assert_eq!(counter(&metrics, |m| &m.counters.trace_unfinished_total), 1);
        assert_eq!(counter(&metrics, |m| &m.counters.trace_cancelled_total), 0);
    }

    #[test]
    fn drop_should_emit_no_second_trace_when_stream_body_dropped_after_finish() {
        struct StreamBody(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for StreamBody {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let body_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (metrics, mut rx, mut scope) = scope_with_events();
        scope.record_provider("up");
        scope.finish(RequestOutcome::ok());
        let body = StreamBody(Arc::clone(&body_dropped));
        scope.mark_handler_returned();
        drop(body); // consumer abandons the stream mid-flight
        assert!(body_dropped.load(Ordering::Relaxed));
        drop(scope);

        let t = traces(&mut rx);
        assert_eq!(t.len(), 1, "{t:?}");
        assert_eq!(t[0].status_code, 200);
        assert_eq!(counter(&metrics, |m| &m.counters.trace_cancelled_total), 0);
        assert_eq!(counter(&metrics, |m| &m.counters.trace_unfinished_total), 0);
    }

    fn poison<T: Send + 'static>(m: &std::sync::Mutex<T>) {
        // Safety net: scoped thread panics while holding the lock.
        std::thread::scope(|s| {
            let _ = s
                .spawn(|| {
                    let _guard = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    panic!("poison");
                })
                .join();
        });
        assert!(m.is_poisoned());
    }

    #[test]
    fn drop_should_not_panic_and_still_emit_when_ring_buffer_or_event_tx_mutex_poisoned() {
        let (metrics, mut rx, scope) = scope_with_events();
        poison(&metrics.recent_requests);
        poison(&metrics.event_tx);
        drop(scope);

        let t = traces(&mut rx);
        assert_eq!(t.len(), 1, "{t:?}");
        assert_eq!(t[0].status_code, 499);
        assert_eq!(metrics.get_recent_requests(1)[0].status_code, 499);
    }

    #[test]
    fn drop_should_emit_500_unfinished_and_not_cancelled_attempt_when_handler_returned_or_panicking(
    ) {
        for panicking in [false, true] {
            let (metrics, mut rx, mut scope) = scope_with_events();
            let attempt = attempt("a", AttemptOutcome::Timeout);
            scope.record_attempt(attempt.clone());
            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let seen_in = std::sync::Arc::clone(&seen);
            let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                if panicking {
                    let held = scope;
                    seen_in.lock().unwrap().push(held.attempts().to_vec());
                    panic!("handler panic");
                } else {
                    scope.mark_handler_returned();
                    seen_in.lock().unwrap().push(scope.attempts().to_vec());
                }
            }));
            assert_eq!(run.is_err(), panicking);

            let t = traces(&mut rx);
            assert_eq!(t.len(), 1, "{t:?}");
            assert_eq!(t[0].status_code, 500);
            assert_eq!(
                *seen.lock().unwrap(),
                vec![vec![attempt]],
                "no cancelled attempt added"
            );
            assert_eq!(counter(&metrics, |m| &m.counters.trace_unfinished_total), 1);
            assert_eq!(counter(&metrics, |m| &m.counters.trace_cancelled_total), 0);
        }
    }

    #[test]
    fn counters_should_split_cancelled_from_unfinished_across_scopes() {
        let metrics = MetricsCollector::new();
        let begin = || TraceScope::begin(&metrics, &serde_json::json!({"model": "m"}), false, 0);
        drop(begin());
        drop(begin());
        let mut returned = begin();
        returned.mark_handler_returned();
        drop(returned);
        let mut finished = begin();
        finished.finish(RequestOutcome::ok());
        drop(finished);

        assert_eq!(counter(&metrics, |m| &m.counters.trace_cancelled_total), 2);
        assert_eq!(counter(&metrics, |m| &m.counters.trace_unfinished_total), 1);
    }

    #[derive(Clone, Default)]
    struct LogBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn unfinished_scope_should_log_error_and_increment_trace_unfinished_total() {
        let logs = LogBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .finish();
        let (metrics, _rx, mut scope) = scope_with_events();
        let before = unfinished_path_hits();
        tracing::subscriber::with_default(subscriber, || {
            scope.mark_handler_returned();
            drop(scope);
        });

        let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(
            text.contains("ERROR") && text.contains("dropped unfinished"),
            "{text}"
        );
        assert_eq!(counter(&metrics, |m| &m.counters.trace_unfinished_total), 1);
        assert_eq!(
            unfinished_path_hits(),
            before + 1,
            "test hook sees the bug path"
        );
    }

    #[test]
    fn unfinished_hook_should_not_fire_for_finished_or_cancelled_scopes() {
        let before = unfinished_path_hits();
        let (_m, _rx, mut finished) = scope_with_events();
        finished.finish(RequestOutcome::ok());
        finished.mark_handler_returned();
        drop(finished);
        let (_m2, _rx2, cancelled) = scope_with_events();
        drop(cancelled);
        assert_eq!(unfinished_path_hits(), before);
    }

    fn production_source(src: &str) -> &str {
        src.split("#[cfg(test)]").next().unwrap()
    }

    #[test]
    fn trace_module_should_not_import_providers_or_routing() {
        let prod = production_source(include_str!("trace.rs"));
        for forbidden in ["crate::providers", "crate::routing"] {
            let uses = prod.lines().filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && t.contains(forbidden)
            });
            assert_eq!(uses.count(), 0, "trace.rs must not reference {forbidden}");
        }
    }

    #[test]
    fn metrics_module_should_not_contain_hardcoded_status_200_in_production_code() {
        let needle = format!("status_code: {}", 200);
        for src in [include_str!("mod.rs"), include_str!("trace.rs")] {
            let prod = production_source(src);
            let hits = prod
                .lines()
                .filter(|l| !l.trim_start().starts_with("//") && l.contains(&needle))
                .count();
            assert_eq!(hits, 0);
        }
    }
}
