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
use super::{MetricsCollector, RequestDetail};
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
        self.iterations = self.iterations.max(record.iteration.saturating_add(1));
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

/// Whether the routed model came from the route's pin or the client's own name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutedModelSource {
    Pinned,
    Passthrough,
}

/// An attempt whose upstream call is in flight; it has no outcome until the call returns.
struct PendingAttempt {
    upstream: UpstreamName,
    model: Option<String>,
    iteration: u8,
    started: tokio::time::Instant,
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
/// Upstream name recorded for the attempt of a local-model request.
const LOCAL_UPSTREAM: &str = "local";
const CANCELLED_STATUS: u16 = 499;
const UNFINISHED_STATUS: u16 = 500;
/// In-flight placeholders are revision 0; the terminal emit outranks them.
const TERMINAL_TRACE_REVISION: u8 = 1;

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
    /// The scope's own copy of the ring-buffer entry; `finish` completes it and writes it back once.
    detail: RequestDetail,
    pending: Option<PendingAttempt>,
    /// Emulation-loop round that the next attempt belongs to (0 for a plain request).
    iteration: u8,
    hold_retries: u8,
    /// Holds spent by the current `dispatch_traced` call only (the budget); reset per call.
    dispatch_hold_retries: u8,
    session_pinned: bool,
    strategy_name: &'static str,
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
        let detail = RequestDetail::from_body(
            request_id.clone(),
            stream,
            tokens_before,
            body,
            crate::session::extract_session_id(body),
        );
        metrics.push_request(detail.clone());
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
            detail,
            pending: None,
            iteration: 0,
            hold_retries: 0,
            dispatch_hold_retries: 0,
            session_pinned: false,
            strategy_name: "",
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

    /// Inputs to the route reason that are known before any attempt.
    pub fn record_route(&mut self, session_pinned: bool, strategy_name: &'static str) {
        self.session_pinned = session_pinned;
        self.strategy_name = strategy_name;
    }

    /// Total holds across every dispatch round on this scope (shown on the trace).
    #[must_use]
    pub fn hold_retries(&self) -> u32 {
        u32::from(self.hold_retries)
    }

    /// Holds spent by the current dispatch round; this is what the hold budget limits.
    #[must_use]
    pub fn dispatch_hold_retries(&self) -> u32 {
        u32::from(self.dispatch_hold_retries)
    }

    /// Starts a dispatch round with a fresh hold budget; the total keeps accumulating.
    pub fn begin_dispatch(&mut self) {
        self.dispatch_hold_retries = 0;
    }

    pub fn note_hold_retry(&mut self) {
        self.hold_retries = self.hold_retries.saturating_add(1);
        self.dispatch_hold_retries = self.dispatch_hold_retries.saturating_add(1);
    }

    /// Opens an attempt before its upstream call, so a dropped request still records it.
    pub fn begin_attempt(&mut self, upstream: &str, model: Option<&str>, iteration: u8) {
        self.pending = Some(PendingAttempt {
            upstream: upstream.into(),
            model: model.map(str::to_string),
            iteration,
            started: tokio::time::Instant::now(),
        });
    }

    #[must_use]
    pub fn iteration(&self) -> u8 {
        self.iteration
    }

    /// Tags the attempts of the next dispatch with an emulation-loop round.
    pub fn set_iteration(&mut self, iteration: u8) {
        self.iteration = iteration;
    }

    /// Opens the single attempt of a local-model request and marks the trace local.
    pub fn begin_local_attempt(&mut self, model: &str) {
        self.detail.trace_kind = TraceKind::Local;
        self.record_provider(LOCAL_UPSTREAM);
        self.begin_attempt(LOCAL_UPSTREAM, Some(model), self.iteration);
    }

    /// Terminal exit for a response-cache hit: no upstream was called.
    pub fn finish_cache_hit(&mut self) {
        self.detail.trace_kind = TraceKind::CacheHit;
        self.emit(OK_STATUS);
    }

    /// Closes the open attempt into the log; a shed has no upstream call, so zero latency.
    pub fn complete_attempt(
        &mut self,
        outcome: AttemptOutcome,
        error_class: Option<AttemptErrorClass>,
        status: Option<u16>,
    ) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let latency_ms = if outcome == AttemptOutcome::Shed {
            0
        } else {
            elapsed_ms(pending.started)
        };
        self.record_attempt(AttemptRecord {
            iteration: pending.iteration,
            upstream: pending.upstream,
            model: pending.model,
            outcome,
            error_class,
            status,
            latency_ms,
        });
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

    /// A still-open attempt means the upstream never answered (the future was dropped).
    fn cancel_pending_attempt(&mut self) {
        if self.pending.is_some() {
            self.complete_attempt(
                AttemptOutcome::Cancelled,
                Some(AttemptErrorClass::Cancelled),
                None,
            );
        }
    }

    fn emit(&mut self, status_code: u16) {
        if self.state == ScopeState::Finished {
            return;
        }
        self.state = ScopeState::Finished;
        let duration_ms = self.started.elapsed().as_secs_f64() * 1000.0;
        let reason = RouteReason::for_request(&RouteInputs {
            kind: self.detail.trace_kind,
            session_pinned: self.session_pinned,
            strategy_name: self.strategy_name,
            exploration: self.selected_model_was_exploration == Some(true),
            hold_retries: self.hold_retries,
            attempts: self.attempts.records(),
        });
        let last_attempt = self.attempts.records().last();
        let routed_model = match (&self.selected_model, &self.detail.requested_model) {
            (Some(pinned), _) => Some((pinned.clone(), RoutedModelSource::Pinned)),
            (None, Some(asked)) if last_attempt.is_some() => {
                Some((asked.clone(), RoutedModelSource::Passthrough))
            }
            _ => None,
        };
        let d = &mut self.detail;
        d.provider.clone_from(&self.provider);
        d.duration_ms = (duration_ms * 10.0).round() / 10.0;
        d.first_byte_ms = if self.provider.is_empty() {
            0.0
        } else {
            d.duration_ms
        };
        d.selected_model = self.selected_model.take();
        d.selected_model_was_exploration = self.selected_model_was_exploration;
        d.status_code = status_code;
        d.trace_revision = TERMINAL_TRACE_REVISION;
        d.routed_provider = last_attempt.map(|a| a.upstream.to_string());
        (d.routed_model, d.routed_model_source) = match routed_model {
            Some((model, source)) => (Some(model), Some(source)),
            None => (None, None),
        };
        d.route_reason = Some(reason.wire().to_string());
        d.route_strategy = reason.strategy().map(str::to_string);
        d.attempts = self.attempts.records().to_vec();
        d.attempts_truncated = self.attempts.truncated();
        d.iterations = self.attempts.iterations().max(1);
        d.hold_retries = self.hold_retries;
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let total_ms = duration_ms.max(0.0) as u64;
        d.total_ms = Some(total_ms);
        self.metrics.finish_detail(&self.detail);
    }
}

fn elapsed_ms(started: tokio::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
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
        self.cancel_pending_attempt();
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
    fn finish_should_stamp_terminal_trace_revision_above_in_flight_placeholder() {
        let (metrics, mut rx, mut scope) = scope_with_events();
        let in_flight = metrics.get_recent_requests(1)[0].trace_revision;
        scope.finish(RequestOutcome::ok());
        let t = traces(&mut rx);
        assert!(t[0].trace_revision > in_flight, "{t:?}");
        assert_eq!(
            metrics.get_recent_requests(1)[0].trace_revision,
            t[0].trace_revision
        );
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

    fn drive_two_attempt_chain(scope: &mut TraceScope) {
        scope.begin_attempt("openrouter", Some("gpt-4o-mini"), 0);
        scope.complete_attempt(
            AttemptOutcome::Upstream,
            Some(AttemptErrorClass::Overloaded),
            Some(529),
        );
        scope.begin_attempt("openrouter", Some("gpt-4o-mini"), 0);
        scope.complete_attempt(AttemptOutcome::Ok, None, Some(200));
        scope.record_selection(Some("gpt-4o-mini".to_string()), None);
        scope.record_provider("openrouter");
    }

    #[test]
    fn finish_should_serialize_requested_routed_pinned_provider_reason_and_two_attempts() {
        let metrics = MetricsCollector::new();
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        metrics.set_event_tx(tx);
        let mut scope =
            TraceScope::begin(&metrics, &serde_json::json!({"model": "gpt-4o"}), false, 7);
        scope.record_route(false, "fallback");
        drive_two_attempt_chain(&mut scope);
        scope.finish(RequestOutcome::ok());

        let json = serde_json::to_value(&traces(&mut rx)[0]).unwrap();
        assert_eq!(json["requestedModel"], "gpt-4o");
        assert_eq!(json["routedModel"], "gpt-4o-mini");
        assert_eq!(json["routedModelSource"], "pinned");
        assert_eq!(json["routedProvider"], "openrouter");
        assert_eq!(json["routeReason"], "fallback");
        assert!(json["routeStrategy"].is_null());
        assert_eq!(json["attempts"].as_array().unwrap().len(), 2);
        assert_eq!(json["attempts"][0]["errorClass"], "overloaded");
        assert_eq!(
            json["attempts"][0]["errorMessage"],
            "Upstream overloaded (HTTP 529)"
        );
        assert!(json["attempts"][1]["errorClass"].is_null());
        assert!(json["attempts"][1]["errorMessage"].is_null());
        assert_eq!(json["attemptsTruncated"], false);
        assert_eq!(json["iterations"], 1);
        assert!(json["totalMs"].is_u64());
    }

    #[test]
    fn finish_should_serialize_strategy_name_and_rate_limit_hold_reasons() {
        let metrics = MetricsCollector::new();
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        metrics.set_event_tx(tx);
        let mut scope = TraceScope::begin(&metrics, &serde_json::json!({"model": "m"}), false, 7);
        scope.record_route(false, "weighted");
        scope.begin_attempt("a", None, 0);
        scope.complete_attempt(AttemptOutcome::Ok, None, Some(200));
        scope.finish(RequestOutcome::ok());
        let mut held = TraceScope::begin(&metrics, &serde_json::json!({"model": "m"}), false, 7);
        held.record_route(false, "fallback");
        held.note_hold_retry();
        held.begin_attempt("a", None, 0);
        held.complete_attempt(AttemptOutcome::Ok, None, Some(200));
        held.finish(RequestOutcome::ok());

        let t = traces(&mut rx);
        let strategy = serde_json::to_value(&t[0]).unwrap();
        assert_eq!(strategy["routeReason"], "strategy");
        assert_eq!(strategy["routeStrategy"], "weighted");
        let hold = serde_json::to_value(&t[1]).unwrap();
        assert_eq!(hold["routeReason"], "rate_limit_hold");
        assert!(hold["routeStrategy"].is_null());
    }

    #[test]
    fn finish_should_label_passthrough_when_route_not_pinned() {
        let metrics = MetricsCollector::new();
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        metrics.set_event_tx(tx);
        let mut scope = TraceScope::begin(
            &metrics,
            &serde_json::json!({"model": "claude-haiku-4-5"}),
            false,
            7,
        );
        scope.begin_attempt("anthropic", None, 0);
        scope.complete_attempt(AttemptOutcome::Ok, None, Some(200));
        scope.record_provider("anthropic");
        scope.finish(RequestOutcome::ok());

        let json = serde_json::to_value(&traces(&mut rx)[0]).unwrap();
        assert_eq!(json["routedModel"], "claude-haiku-4-5");
        assert_eq!(json["routedModelSource"], "passthrough");
        assert_eq!(json["routedProvider"], "anthropic");
    }

    #[test]
    fn finish_should_map_missing_model_placeholder_to_no_requested_model() {
        let metrics = MetricsCollector::new();
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        metrics.set_event_tx(tx);
        let mut scope = TraceScope::begin(&metrics, &serde_json::json!({}), false, 7);
        scope.finish(RequestOutcome::status(502));
        let json = serde_json::to_value(&traces(&mut rx)[0]).unwrap();
        assert!(json["requestedModel"].is_null());
        assert!(json["routedModel"].is_null());
    }

    #[test]
    fn finish_should_use_scope_local_detail_and_count_evicted_when_ring_buffer_entry_evicted() {
        let (metrics, mut rx, mut scope) = scope_with_events();
        scope.begin_attempt("a", None, 0);
        scope.complete_attempt(AttemptOutcome::Ok, None, Some(200));
        for i in 0..100 {
            metrics.push_request(RequestDetail::from_body(
                format!("filler-{i}"),
                false,
                0,
                &serde_json::json!({}),
                None,
            ));
        }
        let id = scope.request_id().to_string();
        scope.finish(RequestOutcome::ok());

        let t = traces(&mut rx);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].request_id, id);
        assert_eq!(t[0].attempts.len(), 1);
        assert_eq!(t[0].status_code, 200);
        assert!(metrics
            .get_recent_requests(100)
            .iter()
            .all(|r| r.request_id != id));
        assert_eq!(
            counter(&metrics, |m| &m.counters.trace_finish_evicted_total),
            1
        );
        assert_eq!(counter(&metrics, |m| &m.counters.trace_finish_total), 1);
    }

    #[tokio::test]
    async fn finish_should_deliver_event_to_subscriber_within_100ms() {
        let (_metrics, mut rx, mut scope) = scope_with_events();
        scope.finish(RequestOutcome::ok());
        let event = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event, DashboardEvent::RequestTrace(_)));
    }

    #[test]
    fn finish_should_not_block_when_event_receiver_lagging_or_absent() {
        let metrics = MetricsCollector::new();
        let mut absent = TraceScope::begin(&metrics, &serde_json::json!({}), false, 0);
        absent.finish(RequestOutcome::ok());
        let (tx, rx) = tokio::sync::broadcast::channel(1);
        metrics.set_event_tx(tx);
        for _ in 0..5 {
            let mut scope = TraceScope::begin(&metrics, &serde_json::json!({}), false, 0);
            scope.finish(RequestOutcome::ok());
        }
        drop(rx);
        assert_eq!(counter(&metrics, |m| &m.counters.trace_finish_total), 6);
    }

    #[test]
    fn three_attempt_dispatch_should_perform_exactly_one_replace_request_plus_begin_push() {
        let metrics = MetricsCollector::new();
        let mut scope = TraceScope::begin(&metrics, &serde_json::json!({"model": "m"}), false, 0);
        for _ in 0..3 {
            scope.begin_attempt("a", None, 0);
            scope.complete_attempt(
                AttemptOutcome::Timeout,
                Some(AttemptErrorClass::Timeout),
                Some(504),
            );
        }
        assert_eq!(metrics.replace_request_count(), 0, "no per-attempt writes");
        scope.finish(RequestOutcome::status(504));
        assert_eq!(metrics.replace_request_count(), 1);
        assert_eq!(metrics.push_request_count(), 1);
    }

    #[test]
    fn dropping_scope_with_pending_attempt_should_record_cancelled_attempt() {
        let (_metrics, mut rx, mut scope) = scope_with_events();
        scope.begin_attempt("a", Some("m"), 0);
        drop(scope);
        let t = traces(&mut rx);
        assert_eq!(t[0].attempts.len(), 1);
        assert_eq!(t[0].attempts[0].outcome, AttemptOutcome::Cancelled);
        assert_eq!(
            t[0].attempts[0].error_class,
            Some(AttemptErrorClass::Cancelled)
        );
        assert_eq!(t[0].attempts[0].status, None);
    }

    struct ScriptedProvider(std::sync::atomic::AtomicU32);

    #[async_trait::async_trait]
    impl crate::providers::Provider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }
        async fn send(
            &self,
            _body: serde_json::Value,
            _headers: http::HeaderMap,
            _stream: bool,
        ) -> Result<crate::providers::ProviderResponse, crate::providers::ProviderError> {
            // Timeouts do not trip a cooldown, so every dispatch sees the same chain.
            if self.0.fetch_add(1, Ordering::Relaxed) % 3 == 2 {
                Ok(crate::providers::ProviderResponse::Full(
                    serde_json::json!({"ok": true}),
                ))
            } else {
                Err(crate::providers::ProviderError::Timeout)
            }
        }
        async fn list_models(
            &self,
        ) -> Result<Vec<crate::providers::ModelInfo>, crate::providers::ProviderError> {
            Ok(Vec::new())
        }
    }

    struct Allow;
    #[async_trait::async_trait]
    impl crate::ratelimit::AdmissionControl for Allow {
        async fn admit(&self, _upstream: &str, _est_tokens: u32) -> crate::ratelimit::Admit {
            crate::ratelimit::Admit::Allowed
        }
    }

    #[tokio::test]
    async fn two_thousand_three_attempt_dispatches_should_finish_within_10s_and_keep_ring_buffer_and_attempt_log_bounded(
    ) {
        use crate::routing::router::{Router, RouterDeps};
        use crate::routing::strategy::{FallbackStrategy, UpstreamRef};
        let metrics = MetricsCollector::new();
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        metrics.set_event_tx(tx);
        let candidates = (0..3)
            .map(|index| UpstreamRef {
                index,
                name: format!("u{index}"),
                weight: 1.0,
                model: None,
                model_family: None,
            })
            .collect();
        let provider = std::sync::Arc::new(ScriptedProvider(std::sync::atomic::AtomicU32::new(0)));
        let router = Router::new(RouterDeps {
            candidates,
            providers: vec![provider.clone(), provider.clone(), provider],
            strategy: std::sync::Arc::new(FallbackStrategy),
            health: std::sync::Arc::new(crate::routing::health::HealthRegistry::new(300)),
            admission: std::sync::Arc::new(Allow),
            metrics: std::sync::Arc::clone(&metrics),
        });

        let started = Instant::now();
        for _ in 0..2_000 {
            let res = router
                .dispatch(
                    serde_json::json!({"model": "m"}),
                    http::HeaderMap::new(),
                    false,
                    0,
                )
                .await;
            assert!(res.is_ok());
            // The two timeouts would otherwise open their breakers after a few rounds.
            router.health.record_success(0);
            router.health.record_success(1);
            while rx.try_recv().is_ok() {}
        }

        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        let recent = metrics.get_recent_requests(1_000);
        assert!(recent.len() <= 100);
        assert!(recent
            .iter()
            .all(|r| r.attempts.len() == 3 && r.attempts.len() <= MAX_ATTEMPTS));
    }
}
