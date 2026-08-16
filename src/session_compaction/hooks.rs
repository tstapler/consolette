//! `CompactHooks`: pre/post-compaction plugin callbacks.
//!
//! Registered as `Vec<Arc<dyn CompactHooks>>`, mirroring the existing
//! `Vec<Arc<dyn Availability>>` seam from ADR-003 rather than inventing a new
//! plugin shape. This repo has no dynamic/external plugin-loading mechanism
//! today (ADR-007 only covers credential-helper subprocesses), so this is
//! in-process registration only — dynamic loading is explicitly out of
//! scope for this pass, same as ADR-003 scoped hot-reload out.

use std::sync::Arc;

use super::session_state::SessionState;
use super::summarizer::SummarizerStats;
use super::tiered::CompactionTier;
use super::tool_result_budget::ToolResultBudgetStats;

/// Summary of one `SessionCompactionPipeline::apply` run, handed to
/// `post_compact` hooks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionReport {
    pub tier: Option<CompactionTier>,
    pub tool_result_stats: ToolResultBudgetStats,
    pub summarizer_stats: SummarizerStats,
}

/// Pre/post-compaction callback seam. Both methods default to no-ops so
/// implementers only override what they need.
pub trait CompactHooks: Send + Sync {
    fn pre_compact(&self, _session: &SessionState) {}
    fn post_compact(&self, _session: &SessionState, _report: &CompactionReport) {}
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

    pub fn run_post_compact(&self, session: &SessionState, report: &CompactionReport) {
        for hook in &self.hooks {
            hook.post_compact(session, report);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[derive(Default)]
    struct CountingHook {
        pre_calls: AtomicUsize,
        post_calls: AtomicUsize,
    }

    impl CompactHooks for CountingHook {
        fn pre_compact(&self, _session: &SessionState) {
            self.pre_calls.fetch_add(1, Ordering::SeqCst);
        }
        fn post_compact(&self, _session: &SessionState, _report: &CompactionReport) {
            self.post_calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn runs_registered_hooks() {
        let hook = Arc::new(CountingHook::default());
        let mut registry = CompactHookRegistry::new();
        registry.register(hook.clone());

        let session = SessionState::default();
        registry.run_pre_compact(&session);
        registry.run_post_compact(&session, &CompactionReport::default());

        assert_eq!(hook.pre_calls.load(Ordering::SeqCst), 1);
        assert_eq!(hook.post_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn default_hook_methods_are_no_ops() {
        struct NoopHook;
        impl CompactHooks for NoopHook {}

        let mut registry = CompactHookRegistry::new();
        registry.register(Arc::new(NoopHook));
        // Should not panic with no overrides.
        registry.run_pre_compact(&SessionState::default());
        registry.run_post_compact(&SessionState::default(), &CompactionReport::default());
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
