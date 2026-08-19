//! `CompactHooks`: pre/post-compaction plugin callbacks.
//!
//! Registered as `Vec<Arc<dyn CompactHooks>>`, mirroring the existing
//! `Vec<Arc<dyn Availability>>` seam from ADR-003 rather than inventing a new
//! plugin shape. This repo has no dynamic/external plugin-loading mechanism
//! today (ADR-007 only covers credential-helper subprocesses), so this is
//! in-process registration only — dynamic loading is explicitly out of
//! scope for this pass, same as ADR-003 scoped hot-reload out.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use super::session_state::{SessionKey, SessionState};
use super::summarizer::SummarizerStats;
use super::tiered::CompactionTier;
use super::tool_result_budget::ToolResultBudgetStats;
use crate::cost_metrics::types::RequestId;

/// Summary of one `SessionCompactionPipeline::apply` run, handed to
/// `post_compact` hooks.
///
/// `request_id` (Epic 2.1, Task 2.1.1a) is a fresh `RequestId::new()`
/// generated once at the top of `apply()` — never via `Default` outside test
/// fixtures, since `RequestId`'s `Default` is deliberately `Uuid::nil()` (see
/// `cost_metrics::types::RequestId` docs) precisely so
/// `CompactionReport::default() == CompactionReport::default()` keeps
/// holding.
///
/// `compacted_messages` (Epic 2.1, Task 2.1.1c) carries `apply()`'s final
/// post-compaction `out` array, so `CostTrackingHook` can estimate both
/// sides of `tokens_saved` from data already in `PostCompactContext`
/// without a fifth field/second hook method.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionReport {
    pub request_id: RequestId,
    pub tier: Option<CompactionTier>,
    pub tool_result_stats: ToolResultBudgetStats,
    pub summarizer_stats: SummarizerStats,
    pub compacted_messages: Value,
}

/// Context handed to every `post_compact` hook: everything `apply()` has in
/// scope at its one hook call site, bundled into one struct rather than
/// growing the trait method's parameter list or adding a second method (see
/// module docs' Epic 2.1 rationale).
pub struct PostCompactContext<'a> {
    pub session_key: &'a SessionKey,
    pub session: &'a SessionState,
    pub pre_compaction_messages: &'a Value,
    pub report: &'a CompactionReport,
}

/// Pre/post-compaction callback seam. Both methods default to no-ops so
/// implementers only override what they need.
///
/// `post_compact` is `async` (via `#[async_trait]`) rather than the plain
/// sync `fn` plan.md's literal text specifies — a deliberate Epic 2.1
/// wiring deviation: `CostTrackingHook::post_compact` (`src/cost_metrics/hook.rs`)
/// must `.await` `CostTracker::record_pending` (an async fn) before
/// returning, so the "row exists the instant `post_compact` returns"
/// guarantee holds. Blocking on that future from a genuinely synchronous fn
/// would require `block_in_place`/`Handle::block_on`, which panics under a
/// current-thread Tokio runtime (e.g. the default `#[tokio::test]` flavor).
/// A real `.await` on an async trait method gives the same guarantee
/// without that fragility. `pre_compact` stays sync — nothing here needs to
/// await anything.
#[async_trait]
pub trait CompactHooks: Send + Sync {
    fn pre_compact(&self, _session: &SessionState) {}
    async fn post_compact(&self, _ctx: &PostCompactContext<'_>) {}
}

/// Ordered set of registered hooks, invoked in registration order.
#[derive(Clone, Default)]
pub struct CompactHookRegistry {
    hooks: Vec<Arc<dyn CompactHooks>>,
}

impl CompactHookRegistry {
    #[must_use]
    pub fn new() -> Self {
        CompactHookRegistry { hooks: Vec::new() }
    }

    pub fn register(&mut self, hook: Arc<dyn CompactHooks>) {
        self.hooks.push(hook);
    }

    pub fn run_pre_compact(&self, session: &SessionState) {
        for hook in &self.hooks {
            hook.pre_compact(session);
        }
    }

    pub async fn run_post_compact(&self, ctx: &PostCompactContext<'_>) {
        for hook in &self.hooks {
            hook.post_compact(ctx).await;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    fn ctx<'a>(
        session_key: &'a SessionKey,
        session: &'a SessionState,
        messages: &'a Value,
        report: &'a CompactionReport,
    ) -> PostCompactContext<'a> {
        PostCompactContext {
            session_key,
            session,
            pre_compaction_messages: messages,
            report,
        }
    }

    #[derive(Default)]
    struct CountingHook {
        pre_calls: AtomicUsize,
        post_calls: AtomicUsize,
    }

    #[async_trait]
    impl CompactHooks for CountingHook {
        fn pre_compact(&self, _session: &SessionState) {
            self.pre_calls.fetch_add(1, Ordering::SeqCst);
        }
        async fn post_compact(&self, _ctx: &PostCompactContext<'_>) {
            self.post_calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn runs_registered_hooks() {
        let hook = Arc::new(CountingHook::default());
        let mut registry = CompactHookRegistry::new();
        registry.register(hook.clone());

        let session = SessionState::default();
        let key = SessionKey::new("hooks-test");
        let messages = Value::Null;
        let report = CompactionReport::default();
        registry.run_pre_compact(&session);
        registry
            .run_post_compact(&ctx(&key, &session, &messages, &report))
            .await;

        assert_eq!(hook.pre_calls.load(Ordering::SeqCst), 1);
        assert_eq!(hook.post_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn default_hook_methods_are_no_ops() {
        struct NoopHook;
        #[async_trait]
        impl CompactHooks for NoopHook {}

        let mut registry = CompactHookRegistry::new();
        registry.register(Arc::new(NoopHook));
        // Should not panic with no overrides.
        let session = SessionState::default();
        let key = SessionKey::new("hooks-test-noop");
        let messages = Value::Null;
        let report = CompactionReport::default();
        registry.run_pre_compact(&session);
        registry
            .run_post_compact(&ctx(&key, &session, &messages, &report))
            .await;
    }

    #[test]
    fn hooks_run_in_registration_order() {
        struct OrderedHook {
            id: usize,
            order: Arc<Mutex<Vec<usize>>>,
        }
        impl CompactHooks for OrderedHook {
            fn pre_compact(&self, _session: &SessionState) {
                self.order.lock().unwrap().push(self.id);
            }
        }

        let order = Arc::new(Mutex::new(Vec::new()));
        let mut registry = CompactHookRegistry::new();
        registry.register(Arc::new(OrderedHook {
            id: 1,
            order: order.clone(),
        }));
        registry.register(Arc::new(OrderedHook {
            id: 2,
            order: order.clone(),
        }));

        registry.run_pre_compact(&SessionState::default());
        assert_eq!(*order.lock().unwrap(), vec![1, 2]);
    }
}
