//! Proxy-internal agentic loop (Story 2.1.2).
//!
//! `run_loop` implements the architecture.md algorithm: rewrite once,
//! dispatch, execute searches, append function-shape history, re-dispatch
//! until the model stops searching or a bound fires. The loop itself holds
//! no routing logic — each iteration calls the injected [`Dispatch`]
//! (the snapshotted router in production, a scripted fake in tests).
//!
//! Termination, in order: no new `web_search` `tool_use` blocks → done;
//! iterations at `min(max_uses, max_iterations, hard-ceiling-10)` → finalize
//! → finalize with unexecuted calls stripped; total deadline exceeded →
//! finalize; executor circuit-open mid-loop → finalize with the best answer
//! so far. Executor failures never propagate: they become error-content
//! results inside the turn (or drop-degrade when the backend is unavailable
//! upfront).

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::server_tools::detect::rewrite_for_upstream;
use crate::server_tools::executor::{ErrorKind, SearchBackend, SearchExecutor};
use crate::server_tools::mapping::{
    aggregate_usage, build_final_turn, extract_usage_pair, find_web_search_calls, results_to_text,
    server_block_id, to_function_tool_result, ExecutedSearch,
};
use crate::server_tools::{ServerWebSearchDef, HARD_CEILING_ITERATIONS};

/// Per-request loop bounds (from [`crate::server_tools::ServerToolsConfig`]
/// via `loop_limits`, clamped to the hard ceiling).
#[derive(Debug, Clone)]
pub struct LoopLimits {
    pub max_iterations: u32,
    pub max_results: u32,
    pub total_timeout: Duration,
}

/// One dispatch round-trip: a closure capturing the request-scoped
/// snapshotted router `Arc`, headers, and token estimate (C1: the router
/// `Arc` is loaded once per client request so a mid-loop `POST /api/route`
/// hot-swap cannot change routes between iterations). Always dispatches
/// with `stream: false` and returns the full Anthropic response JSON.
pub trait Dispatch: Send + Sync {
    /// Dispatch one rewritten body.
    ///
    /// # Errors
    ///
    /// Returns [`crate::providers::ProviderError`] on genuine provider
    /// failure (flows through normal ADR-003 handling in the caller).
    fn dispatch<'a>(
        &'a self,
        body: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, crate::providers::ProviderError>> + Send + 'a>>;
}

impl<F, Fut> Dispatch for F
where
    F: Fn(Value) -> Fut + Send + Sync,
    Fut: Future<Output = Result<Value, crate::providers::ProviderError>> + Send + 'static,
{
    fn dispatch<'a>(
        &'a self,
        body: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, crate::providers::ProviderError>> + Send + 'a>>
    {
        Box::pin((self)(body))
    }
}

/// What the loop produced, for the entrypoint to record (cost, metrics) and
/// return.
#[derive(Debug, Clone)]
pub struct LoopOutcome {
    /// Client-ready Anthropic `message` JSON (server shapes, aggregated
    /// usage). Verbatim upstream JSON on the never-searched and drop-degrade
    /// paths.
    pub message: Value,
    /// Dispatch round-trips performed.
    pub iterations: u32,
    /// Total `web_search` calls executed.
    pub searches_executed: u64,
    /// Served by Brave (metrics `backend="brave"`).
    pub searches_brave: u64,
    /// Served by the browser fallback (metrics `backend="browser"`).
    pub searches_browser: u64,
    /// Executed but failed (error-content rendered, loop continued).
    pub searches_failed: u64,
    /// True when the loop did not fully satisfy the model (cap, deadline,
    /// circuit, or drop-degrade) — still a complete 200 answer.
    pub degraded: bool,
    /// True when the request carried V1-ignored domain/location filters
    /// (drives the entrypoint log line; pre-mortem failure 5).
    pub filters_ignored: bool,
}

/// Run the emulation loop for one client request.
///
/// # Errors
///
/// Returns [`crate::providers::ProviderError`] only on genuine dispatch
/// failure (provider down for every candidate). Executor failures degrade
/// internally and never surface here.
pub async fn run_loop<D, E>(
    initial_body: &Value,
    dispatch: &D,
    executor: &E,
    limits: &LoopLimits,
) -> Result<LoopOutcome, crate::providers::ProviderError>
where
    D: Dispatch,
    E: SearchExecutor,
{
    let (mut body, sidecars) = rewrite_for_upstream(initial_body);
    let filters_ignored = sidecars.iter().any(ServerWebSearchDef::has_ignored_filters);
    let cap = iteration_cap(&sidecars, limits.max_iterations);
    let deadline = Instant::now() + limits.total_timeout;

    // Drop-degrade fast path: backend unavailable upfront (binary missing,
    // tool drift, circuit already open) ⇒ strip defs, single dispatch,
    // verbatim answer. Never an error response for a backend outage (S-4).
    if executor.probe().await.is_err() {
        let stripped = strip_server_defs(initial_body);
        let response = dispatch.dispatch(stripped).await?;
        return Ok(LoopOutcome {
            message: response,
            iterations: 1,
            searches_executed: 0,
            searches_brave: 0,
            searches_browser: 0,
            searches_failed: 0,
            degraded: true,
            filters_ignored,
        });
    }

    let outcome = run_loop_inner(&mut body, dispatch, executor, limits, cap, deadline).await?;
    Ok(LoopOutcome {
        filters_ignored,
        ..outcome
    })
}

#[allow(clippy::too_many_lines)]
async fn run_loop_inner<D, E>(
    body: &mut Value,
    dispatch: &D,
    executor: &E,
    limits: &LoopLimits,
    cap: u32,
    deadline: Instant,
) -> Result<LoopOutcome, crate::providers::ProviderError>
where
    D: Dispatch,
    E: SearchExecutor,
{
    let mut usages: Vec<(u64, u64)> = Vec::new();
    let mut executed: Vec<ExecutedSearch> = Vec::new();
    let mut dispatches: u32 = 0;
    let mut rounds: u32 = 0;
    let (mut searches_brave, mut searches_browser, mut searches_failed) = (0_u64, 0_u64, 0_u64);

    loop {
        let response = dispatch.dispatch(body.clone()).await?;
        dispatches += 1;
        usages.push(extract_usage_pair(&response));
        let calls = find_web_search_calls(&response);

        // Model stopped searching: verbatim on the first round (S-3 parity),
        // otherwise the final answer turn with the full search trail.
        if calls.is_empty() {
            if dispatches == 1 {
                return Ok(LoopOutcome {
                    message: response,
                    iterations: dispatches,
                    searches_executed: 0,
                    searches_brave,
                    searches_browser,
                    searches_failed,
                    degraded: false,
                    filters_ignored: false,
                });
            }
            return Ok(finalize(
                &response,
                &usages,
                &executed,
                dispatches,
                searches_brave,
                searches_browser,
                searches_failed,
                false,
            ));
        }

        // Bounds: iteration cap or total deadline ⇒ finalize with unexecuted
        // calls stripped (still a complete answer, flagged degraded).
        if rounds >= cap || Instant::now() >= deadline {
            return Ok(finalize(
                &response,
                &usages,
                &executed,
                dispatches,
                searches_brave,
                searches_browser,
                searches_failed,
                true,
            ));
        }

        // Execute this turn's searches sequentially (result order must match
        // call order for the paired mapping).
        let mut tool_results = Vec::with_capacity(calls.len());
        let mut circuit_open = false;
        for (index, call) in calls.iter().enumerate() {
            let server_id = server_block_id(rounds, index);
            if call.query.trim().is_empty() {
                let message = "Empty search query: the web_search call provided no query string; unable to search.".to_string();
                tool_results.push(to_function_tool_result(&call.id, &message));
                executed.push(ExecutedSearch {
                    call_id: call.id.clone(),
                    query: call.query.clone(),
                    server_id,
                    result: Err(message),
                });
                searches_failed += 1;
                continue;
            }
            match executor.search(&call.query, limits.max_results).await {
                Ok(outcome) => {
                    match outcome.backend {
                        SearchBackend::Brave => searches_brave += 1,
                        SearchBackend::Browser => searches_browser += 1,
                    }
                    tool_results.push(to_function_tool_result(
                        &call.id,
                        &results_to_text_or_empty(&outcome.results),
                    ));
                    executed.push(ExecutedSearch {
                        call_id: call.id.clone(),
                        query: call.query.clone(),
                        server_id,
                        result: Ok(outcome.results),
                    });
                }
                Err(e) if e.kind == ErrorKind::CircuitOpen => {
                    circuit_open = true;
                    break;
                }
                Err(e) => {
                    let message =
                        format!("Web search failed ({}): {}", e.kind.kind_label(), e.message);
                    tool_results.push(to_function_tool_result(&call.id, &message));
                    executed.push(ExecutedSearch {
                        call_id: call.id.clone(),
                        query: call.query.clone(),
                        server_id,
                        result: Err(message),
                    });
                    searches_failed += 1;
                }
            }
        }
        if circuit_open {
            // Best answer so far: finalize without the pending calls.
            return Ok(finalize(
                &response,
                &usages,
                &executed,
                dispatches,
                searches_brave,
                searches_browser,
                searches_failed,
                true,
            ));
        }

        rounds += 1;
        append_turn(body, &response, &tool_results);
    }
}

/// Build the client-facing final turn from the last response plus every
/// executed pair, with aggregated usage.
#[allow(clippy::too_many_arguments)]
fn finalize(
    response: &Value,
    usages: &[(u64, u64)],
    executed: &[ExecutedSearch],
    dispatches: u32,
    searches_brave: u64,
    searches_browser: u64,
    searches_failed: u64,
    degraded: bool,
) -> LoopOutcome {
    let (content, stop_reason) = build_final_turn(response, executed);
    #[allow(clippy::cast_possible_truncation)]
    let searches_executed = executed.len() as u64;
    LoopOutcome {
        message: with_content_usage(
            response,
            content,
            stop_reason,
            aggregate_usage(usages, searches_executed),
        ),
        iterations: dispatches,
        searches_executed,
        searches_brave,
        searches_browser,
        searches_failed,
        degraded,
        filters_ignored: false,
    }
}

/// Effective iteration cap: the tightest of per-request `max_uses`, the
/// configured max, and the hard ceiling (D2, pre-mortem failure 1).
fn iteration_cap(sidecars: &[ServerWebSearchDef], configured: u32) -> u32 {
    let requested = sidecars
        .iter()
        .filter_map(|s| s.max_uses)
        .min()
        .unwrap_or(configured);
    requested.min(configured).min(HARD_CEILING_ITERATIONS)
}

/// Strip server defs without emulation (drop-degrade parity with the Cohere
/// working-tree fix: capability removed, request otherwise untouched).
fn strip_server_defs(body: &Value) -> Value {
    use crate::server_tools::detect::is_server_web_search_def;

    let mut stripped = body.clone();
    if let Some(tools) = stripped.get_mut("tools").and_then(Value::as_array_mut) {
        tools.retain(|t| !is_server_web_search_def(t));
    }
    stripped
}

/// Append one assistant turn (upstream content blocks, function shape) plus
/// the user turn of tool results — the history the next iteration dispatches.
fn append_turn(body: &mut Value, response: &Value, tool_results: &[Value]) {
    let assistant_blocks = response
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let messages = body.get_mut("messages").and_then(Value::as_array_mut);
    if let Some(messages) = messages {
        messages.push(json!({"role": "assistant", "content": assistant_blocks}));
        messages.push(json!({"role": "user", "content": tool_results}));
    } else {
        body["messages"] = json!([
            {"role": "assistant", "content": assistant_blocks},
            {"role": "user", "content": tool_results}
        ]);
    }
}

fn with_content_usage(
    response: &Value,
    content: Vec<Value>,
    stop_reason: String,
    usage: Value,
) -> Value {
    let mut message = response.clone();
    if let Some(obj) = message.as_object_mut() {
        obj.insert("content".to_string(), Value::Array(content));
        obj.insert("stop_reason".to_string(), Value::String(stop_reason));
        obj.insert("usage".to_string(), usage);
    }
    message
}

fn results_to_text_or_empty(results: &[crate::server_tools::executor::SearchResult]) -> String {
    if results.is_empty() {
        "(no results found)".to_string()
    } else {
        results_to_text(results)
    }
}

impl ErrorKind {
    /// Short stable label for error-content rendering (never leaks key
    /// material — messages are backend strings, never credentials).
    pub(crate) fn kind_label(self) -> &'static str {
        match self {
            ErrorKind::Timeout => "timeout",
            ErrorKind::BadResponse => "bad response",
            ErrorKind::Backend => "backend error",
            ErrorKind::MissingBinary
            | ErrorKind::SpawnFailed
            | ErrorKind::ToolMissing
            | ErrorKind::CircuitOpen => "backend unavailable",
        }
    }
}

/// Boxed dispatch helper for tests and harnesses that prefer an explicit
/// wrapper over a bare closure (closures already implement [`Dispatch`] via
/// the blanket impl).
pub struct FnDispatch<F> {
    f: F,
}

impl<F> FnDispatch<F> {
    #[must_use]
    pub fn new(f: F) -> Self {
        Self { f }
    }
}

impl<F, Fut> Dispatch for FnDispatch<F>
where
    F: Fn(Value) -> Fut + Send + Sync,
    Fut: Future<Output = Result<Value, crate::providers::ProviderError>> + Send + 'static,
{
    fn dispatch<'a>(
        &'a self,
        body: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, crate::providers::ProviderError>> + Send + 'a>>
    {
        Box::pin((self.f)(body))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::server_tools::executor::SearchOutcome;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct StubExecutor {
        results: Vec<crate::server_tools::executor::SearchResult>,
        backend: SearchBackend,
        fail: bool,
        probe_ok: bool,
        searches: AtomicUsize,
    }

    impl StubExecutor {
        fn ok() -> Self {
            Self {
                results: vec![crate::server_tools::executor::SearchResult {
                    title: "t".to_string(),
                    url: "https://example.com".to_string(),
                    description: "d".to_string(),
                }],
                backend: SearchBackend::Brave,
                fail: false,
                probe_ok: true,
                searches: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl SearchExecutor for StubExecutor {
        async fn search(
            &self,
            _query: &str,
            _count: u32,
        ) -> Result<SearchOutcome, crate::server_tools::executor::ExecutorError> {
            self.searches.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(crate::server_tools::executor::ExecutorError {
                    kind: ErrorKind::Backend,
                    message: "boom".to_string(),
                });
            }
            Ok(SearchOutcome {
                results: self.results.clone(),
                backend: self.backend,
            })
        }

        async fn probe(&self) -> Result<(), crate::server_tools::executor::ExecutorError> {
            if self.probe_ok {
                Ok(())
            } else {
                Err(crate::server_tools::executor::ExecutorError {
                    kind: ErrorKind::MissingBinary,
                    message: "no binary".to_string(),
                })
            }
        }
    }

    fn limits() -> LoopLimits {
        LoopLimits {
            max_iterations: 5,
            max_results: 5,
            total_timeout: Duration::from_secs(30),
        }
    }

    fn request() -> Value {
        json!({
            "model": "test",
            "messages": [{"role": "user", "content": "search?"}],
            "tools": [{"type": "web_search_20250305", "name": "web_search"}]
        })
    }

    fn answer(text: &str) -> Value {
        json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "test",
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })
    }

    fn search_call(id: &str, query: &str) -> Value {
        json!({
            "id": "msg_2", "type": "message", "role": "assistant", "model": "test",
            "content": [
                {"type": "text", "text": "searching"},
                {"type": "tool_use", "id": id, "name": "web_search", "input": {"query": query}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })
    }

    #[tokio::test]
    async fn loop_should_return_verbatim_answer_when_model_never_searches() {
        let expected = answer("plain answer");
        let dispatch = FnDispatch::new(|_: Value| {
            let expected = expected.clone();
            async move { Ok(expected) }
        });

        let outcome = run_loop(&request(), &dispatch, &StubExecutor::ok(), &limits())
            .await
            .unwrap();

        assert_eq!(outcome.message, expected);
        assert!(!outcome.degraded);
        assert_eq!(outcome.searches_executed, 0);
    }

    #[tokio::test]
    async fn loop_should_terminate_at_cap_when_model_always_searches() {
        let calls = Arc::new(AtomicUsize::new(0));
        let dispatch = FnDispatch::new(|_: Value| {
            let calls = Arc::clone(&calls);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(search_call("toolu_1", "q"))
            }
        });
        let limits = LoopLimits {
            max_iterations: 2,
            ..limits()
        };

        let outcome = run_loop(&request(), &dispatch, &StubExecutor::ok(), &limits)
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 3, "cap 2 ⇒ 3 dispatches");
        assert_eq!(outcome.searches_executed, 2);
        assert!(outcome.degraded);
        let blocks = outcome.message["content"]
            .as_array()
            .expect("content array");
        assert!(
            !blocks
                .iter()
                .any(|b| b["type"] == json!("tool_use") && b["name"] == json!("web_search")),
            "no dangling function-shape calls: {blocks:?}"
        );
        assert!(outcome.message["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["type"] == json!("server_tool_use")));
    }

    #[tokio::test]
    async fn loop_should_terminate_at_deadline_when_iterations_slow() {
        let dispatch = FnDispatch::new(|_: Value| async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(search_call("toolu_1", "q"))
        });
        let limits = LoopLimits {
            total_timeout: Duration::from_millis(50),
            ..limits()
        };

        let outcome = run_loop(&request(), &dispatch, &StubExecutor::ok(), &limits)
            .await
            .unwrap();

        assert!(outcome.degraded);
        assert_eq!(outcome.searches_executed, 0);
    }

    #[tokio::test]
    async fn loop_should_feedback_error_when_query_empty() {
        let seen: Arc<std::sync::Mutex<Vec<Value>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let dispatch = FnDispatch::new(|body: Value| {
            let seen = Arc::clone(&seen);
            async move {
                seen.lock().unwrap().push(body);
                if seen.lock().unwrap().len() == 1 {
                    Ok(search_call("toolu_1", ""))
                } else {
                    Ok(answer("done without searching"))
                }
            }
        });

        let outcome = run_loop(&request(), &dispatch, &StubExecutor::ok(), &limits())
            .await
            .unwrap();

        assert_eq!(outcome.searches_executed, 1);
        let bodies = seen.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        let feedback = bodies[1].to_string();
        assert!(
            feedback.contains("Empty search query"),
            "error fed back to model: {feedback}"
        );
        assert!(outcome
            .message
            .to_string()
            .contains("done without searching"));
    }

    #[tokio::test]
    async fn loop_should_execute_sequential_searches_when_multiple_calls() {
        let two_calls = json!({
            "id": "msg_2", "type": "message", "role": "assistant", "model": "test",
            "content": [
                {"type": "tool_use", "id": "toolu_a", "name": "web_search", "input": {"query": "a"}},
                {"type": "tool_use", "id": "toolu_b", "name": "web_search", "input": {"query": "b"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let dispatch = FnDispatch::new(move |body: Value| {
            let two_calls = two_calls.clone();
            async move {
                let redispatch = body["messages"].as_array().is_some_and(|m| m.len() > 1);
                if redispatch {
                    Ok(answer("both done"))
                } else {
                    Ok(two_calls)
                }
            }
        });
        let executor = StubExecutor::ok();

        let outcome = run_loop(&request(), &dispatch, &executor, &limits())
            .await
            .unwrap();

        assert_eq!(outcome.searches_executed, 2);
        assert_eq!(executor.searches.load(Ordering::SeqCst), 2);
        let pairs = outcome.message["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["type"] == json!("server_tool_use"))
            .count();
        assert_eq!(pairs, 2);
    }

    #[tokio::test]
    async fn loop_should_degrade_to_verbatim_when_probe_fails() {
        let expected = answer("no search for you");
        let dispatch = FnDispatch::new(|body: Value| {
            let expected = expected.clone();
            async move {
                assert!(
                    !body.to_string().contains("web_search_20250305"),
                    "defs must be stripped on degrade"
                );
                Ok(expected)
            }
        });
        let executor = StubExecutor {
            probe_ok: false,
            ..StubExecutor::ok()
        };

        let outcome = run_loop(&request(), &dispatch, &executor, &limits())
            .await
            .unwrap();

        assert!(outcome.degraded);
        assert_eq!(outcome.message, expected);
    }
}
