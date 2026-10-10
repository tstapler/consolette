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

use super::{MetricsCollector, RequestDetail, RequestTimingUpdate, ScopeFinish};

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
    Cancelled,
}

/// One upstream attempt of a request (minimal form; Epic 2 extends it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub upstream: String,
    pub outcome: AttemptOutcome,
    pub latency_ms: u64,
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
    attempts: Vec<AttemptRecord>,
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
            attempts: Vec::new(),
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
        &self.attempts
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
        scope.record_attempt(AttemptRecord {
            upstream: "a".into(),
            outcome: AttemptOutcome::RateLimited,
            latency_ms: 3,
        });
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
        let failed = AttemptRecord {
            upstream: "a".into(),
            outcome: AttemptOutcome::Upstream,
            latency_ms: 1,
        };
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
            guard.record_attempt(AttemptRecord {
                upstream: "round-1".into(),
                outcome: AttemptOutcome::Upstream,
                latency_ms: 1,
            });
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
            let attempt = AttemptRecord {
                upstream: "a".into(),
                outcome: AttemptOutcome::Timeout,
                latency_ms: 9,
            };
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
