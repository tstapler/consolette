//! `Router` test suite, split by theme (kibitzer file-size: the original
//! single `router.rs` test module was ~2000 lines). This file holds only
//! the fixtures/mocks genuinely shared across more than one theme; each
//! theme's own single-use helpers live with that theme's tests instead of
//! being centralized here.

use std::sync::atomic::{AtomicU32, Ordering};

use super::*;

mod basic_dispatch;
mod from_config;
mod model_pinning;
mod observability;
mod per_model_retry;
mod rate_limit_hold;
mod selected_model;
mod selection;

struct AlwaysOkProvider {
    name: &'static str,
    call_count: Arc<AtomicU32>,
}

#[async_trait::async_trait]
impl Provider for AlwaysOkProvider {
    fn name(&self) -> &str {
        self.name
    }

    async fn send(
        &self,
        _body: serde_json::Value,
        _headers: HeaderMap,
        _stream: bool,
    ) -> Result<ProviderResponse, ProviderError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Ok(ProviderResponse::Full(serde_json::json!({"ok": true})))
    }

    async fn list_models(&self) -> Result<Vec<crate::providers::ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }
}

struct AlwaysErrProvider {
    name: &'static str,
    error: fn() -> ProviderError,
    call_count: Arc<AtomicU32>,
}

#[async_trait::async_trait]
impl Provider for AlwaysErrProvider {
    fn name(&self) -> &str {
        self.name
    }

    async fn send(
        &self,
        _body: serde_json::Value,
        _headers: HeaderMap,
        _stream: bool,
    ) -> Result<ProviderResponse, ProviderError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Err((self.error)())
    }

    async fn list_models(&self) -> Result<Vec<crate::providers::ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }
}

/// A `Provider` test double that records the last body it was sent —
/// shared between the `model_pinning` (proving a pinned model overrides
/// the body) and `selection` (proving `expand_candidates` runs before the
/// health filter) themes.
struct CapturingProvider {
    name: &'static str,
    received_body: Arc<std::sync::Mutex<Option<serde_json::Value>>>,
}

#[async_trait::async_trait]
impl Provider for CapturingProvider {
    fn name(&self) -> &str {
        self.name
    }

    #[allow(clippy::unwrap_used)]
    async fn send(
        &self,
        body: serde_json::Value,
        _headers: HeaderMap,
        _stream: bool,
    ) -> Result<ProviderResponse, ProviderError> {
        *self.received_body.lock().unwrap() = Some(body);
        Ok(ProviderResponse::Full(serde_json::json!({"ok": true})))
    }

    async fn list_models(&self) -> Result<Vec<crate::providers::ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }
}

fn upstream(index: usize, name: &str) -> UpstreamRef {
    UpstreamRef {
        index,
        name: name.to_string(),
        weight: 1.0,
        model: None,
    }
}

struct AlwaysAllow;

#[async_trait::async_trait]
impl AdmissionControl for AlwaysAllow {
    async fn admit(&self, _upstream: &str, _est_tokens: u32) -> Admit {
        Admit::Allowed
    }
}

/// A `FallbackStrategy` router over caller-supplied `candidates`/
/// `providers`, with default health/admission and metrics shared back
/// to the caller — the common `RouterDeps` tail repeated across several
/// `dispatch` tests that only vary their candidates/providers (kibitzer
/// duplicate-code). Shared across the `model_pinning` and `observability`
/// themes.
fn fallback_router_with_metrics(
    candidates: Vec<UpstreamRef>,
    providers: Vec<Arc<dyn Provider>>,
    metrics: &Arc<MetricsCollector>,
) -> Router {
    Router::new(RouterDeps {
        candidates,
        providers,
        strategy: Arc::new(FallbackStrategy),
        health: Arc::new(HealthRegistry::new(300)),
        admission: Arc::new(AlwaysAllow),
        metrics: Arc::clone(metrics),
    })
}

/// A single `AlwaysOkProvider` named `name`, dispatched to through a
/// plain `FallbackStrategy` router with default health/admission — the
/// minimal single-candidate setup shared by several `dispatch`
/// observability tests (kibitzer duplicate-code). Shared across the
/// `observability` and `selected_model` themes.
fn single_ok_provider_fallback_router(
    name: &'static str,
    metrics: Arc<MetricsCollector>,
) -> Router {
    let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(AlwaysOkProvider {
        name,
        call_count: Arc::new(AtomicU32::new(0)),
    })];
    Router::new(RouterDeps {
        candidates: vec![upstream(0, name)],
        providers,
        strategy: Arc::new(FallbackStrategy),
        health: Arc::new(HealthRegistry::new(300)),
        admission: Arc::new(AlwaysAllow),
        metrics,
    })
}

/// A live `OpenrouterScoringStrategy` over a fresh, empty
/// `ModelListCache` scoped to `openrouter_index` — the same
/// cache+strategy construction repeated verbatim across the
/// per-model-candidate `dispatch` tests below (kibitzer
/// duplicate-code). Shared across the `per_model_retry` and
/// `observability` themes.
fn openrouter_scoring_strategy(openrouter_index: usize) -> Arc<dyn RoutingStrategy> {
    let model_cache = Arc::new(
        crate::providers::openrouter::cache::ModelListCache::new_with_ttl(Duration::from_mins(15)),
    );
    Arc::new(OpenrouterScoringStrategy::new(
        model_cache,
        openrouter_index,
    )) as Arc<dyn RoutingStrategy>
}
