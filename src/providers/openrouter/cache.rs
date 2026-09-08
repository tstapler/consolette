//! Placeholder for Epic 2.1 (project_plans/openrouter-routing/implementation/plan.md
//! Story 2.1.1) — full TTL/background-refresh/data-policy-invalidation logic lands
//! in the next epic. This minimal shape exists only so Epic 1.2 compiles and its
//! own tests (which stub/mock cache behavior, not test the cache itself) can run.
//!
//! Do NOT add TTL expiry, `last_refresh`, `last_invalidation_reason`,
//! `recent_not_found`, or a `Weak<OpenrouterProvider>` back-reference here —
//! those are Epic 2.1's Story 2.1.1/2.1.2/2.1.3 scope and will replace this
//! placeholder wholesale.

use std::sync::{Arc, Mutex};

use super::OpenrouterProvider;

/// One entry of the cached free-model list — carries price forward (not
/// just the bare id) so `OpenrouterProvider::send()`'s per-dispatch recheck
/// (money-safety backstop mechanism 2, plan.md Risk Control) can verify the
/// *specific selected model's* price, not just its membership.
#[derive(Debug, Clone, PartialEq)]
pub struct FreeModelEntry {
    pub id: String,
    pub price_prompt: f64,
    pub price_completion: f64,
}

/// Minimal single-slot free-model-list cache: an `Option<Arc<Vec<FreeModelEntry>>>`
/// behind a `Mutex`, no TTL/expiry, no data-policy-vs-staleness distinction,
/// no background refresh, no `Weak` back-reference to its owning provider —
/// all of that is Epic 2.1's job. For Epic 1.2, `OpenrouterProvider::new()`
/// populates this once via `refresh()` at construction and never again on
/// its own.
pub struct ModelListCache {
    inner: Mutex<Option<Arc<Vec<FreeModelEntry>>>>,
}

impl ModelListCache {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    /// The last successfully fetched free-model list, or `None` if no
    /// refresh has ever succeeded.
    #[must_use]
    pub fn snapshot(&self) -> Option<Arc<Vec<FreeModelEntry>>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Re-fetches `provider.list_free_models()` and replaces the cached
    /// snapshot on success. Leaves the previous snapshot in place on
    /// failure (fail-soft, matching `research/ux.md`'s stance) rather than
    /// clearing it, so a transient fetch error doesn't blank out an
    /// otherwise-good cached list.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `provider.list_free_models()` fails.
    pub async fn refresh(&self, provider: &OpenrouterProvider) -> anyhow::Result<()> {
        let entries = provider.list_free_models().await?;
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(Arc::new(entries));
        Ok(())
    }

    /// Clears the cached snapshot immediately, without waiting for the next
    /// scheduled refresh. Epic 2.1 wires this into TTL expiry and the
    /// data-policy-404 detector; this epic's own money-safety backstop
    /// (Story 1.2.4) also calls it directly on a confirmed nonzero-cost
    /// signal, when one exists (see `mod.rs`'s `check_for_unexpected_cost`).
    pub fn invalidate(&self) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = None;
    }
}

impl Default for ModelListCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl ModelListCache {
    /// Test-only helper: seeds the cache with a fixed snapshot without
    /// going through `refresh()` (which requires a live `OpenrouterProvider`
    /// and network access) — used by `mod.rs`'s money-safety-backstop tests
    /// (Story 1.2.4), which need a populated cache but not a real fetch.
    pub(crate) fn seed_for_test(&self, entries: Vec<FreeModelEntry>) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(Arc::new(entries));
    }
}
