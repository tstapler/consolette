//! Epic 3 Story 3.3 AC4 perf budget (auto-model-family): family resolution
//! overhead vs the static-pin baseline at family sizes ≤8 must stay under
//! p99 ≤ 1ms — the rollout gate for enabling the family route.
//!
//! Measured 2026-09-12 (dev profile, `cargo test --test family_perf
//! -- --nocapture`): rank p99 ≈ 3.6µs, full dispatch seam p99 ≈ 54µs,
//! static-pin baseline ≈ 190ns — the seam sits ~20× inside the 1ms budget.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use http::HeaderMap;

use consolette::config::schema::{Config, FamilyMember, ModelFamily};
use consolette::metrics::MetricsCollector;
use consolette::providers::{ModelInfo, Provider, ProviderError, ProviderResponse};
use consolette::ratelimit::{AdmissionControl, Admit};
use consolette::routing::family::{FamilyResolver, FamilyTable};
use consolette::routing::health::HealthRegistry;
use consolette::routing::router::Router;
use consolette::routing::strategy::{FallbackStrategy, UpstreamRef};

struct AlwaysOk;

#[async_trait::async_trait]
impl Provider for AlwaysOk {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn send(
        &self,
        _body: serde_json::Value,
        _headers: HeaderMap,
        _stream: bool,
    ) -> Result<ProviderResponse, ProviderError> {
        Ok(ProviderResponse::Full(serde_json::json!({"ok": true})))
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }
}

struct AlwaysAllow;

#[async_trait::async_trait]
impl AdmissionControl for AlwaysAllow {
    async fn admit(&self, _upstream: &str, _est_tokens: u32) -> Admit {
        Admit::Allowed
    }
}

/// 8-member family across 4 mock upstreams with mixed warm stats (varying
/// error rates + latencies so the ranker actually compares).
fn eight_member_setup() -> (Router, Arc<MetricsCollector>) {
    let members: Vec<(String, String)> = (0..8)
        .map(|i| (format!("mock-{}", i % 4), format!("model-{i}:free")))
        .collect();
    let config = Config {
        families: vec![ModelFamily {
            alias: "auto-coding".to_string(),
            members: members
                .iter()
                .map(|(upstream, model)| FamilyMember {
                    upstream: upstream.clone(),
                    model: model.clone(),
                })
                .collect(),
            allow_paid: false,
        }],
        ..Config::default()
    };
    let table = Arc::new(FamilyTable::from_config(&config));

    let metrics = MetricsCollector::new();
    for (i, (upstream, model)) in members.iter().enumerate() {
        // Member i: i errors out of 20+i samples → spread error rates.
        let err = ProviderError::Timeout;
        for _ in 0..i {
            let _ = metrics
                .family
                .record_member(upstream, model, Some(&err), 80 + i as u64 * 10);
        }
        for _ in 0..20 {
            let _ = metrics
                .family
                .record_member(upstream, model, None, 80 + i as u64 * 10);
        }
    }

    let names: Vec<String> = (0..4).map(|i| format!("mock-{i}")).collect();
    let candidates = names
        .iter()
        .enumerate()
        .map(|(index, name)| UpstreamRef {
            index,
            name: name.clone(),
            weight: 1.0,
            model: None,
        })
        .collect();
    let providers: Vec<Arc<dyn Provider>> = (0..4)
        .map(|_| Arc::new(AlwaysOk) as Arc<dyn Provider>)
        .collect();
    let router = Router::new(
        candidates,
        providers,
        Arc::new(FallbackStrategy),
        Arc::new(HealthRegistry::new(300)),
        Arc::new(AlwaysAllow),
        metrics.clone(),
    )
    .with_family_table(table, Some("auto-coding".to_string()));
    (router, metrics)
}

fn percentile_p99(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    samples[(samples.len() * 99 / 100).min(samples.len() - 1)]
}

#[test]
fn resolution_overhead_should_stay_under_1ms_p99_when_family_has_8_members() {
    let (router, metrics) = eight_member_setup();

    // Rank micro-bench: the pure ordering core over a snapshot of views
    // (built exactly as dispatch does).
    let table_members = {
        use std::collections::HashMap;
        let mut views = HashMap::new();
        for i in 0..8 {
            let upstream = format!("mock-{}", i % 4);
            let model = format!("model-{i}:free");
            views.insert(
                (upstream.clone(), model.clone()),
                metrics.family.member_view(&upstream, &model),
            );
        }
        let members: Vec<FamilyMember> = (0..8)
            .map(|i| FamilyMember {
                upstream: format!("mock-{}", i % 4),
                model: format!("model-{i}:free"),
            })
            .collect();
        (members, views)
    };
    let mut rank_samples = Vec::with_capacity(5000);
    for _ in 0..5000 {
        let start = Instant::now();
        let ranked = FamilyResolver::rank(&table_members.0, &table_members.1, None);
        std::hint::black_box(ranked.ordered.len());
        rank_samples.push(start.elapsed());
    }
    let rank_p99 = percentile_p99(rank_samples);

    // Dispatch-seam timing: the real `resolve_family` seam dispatch calls
    // (exclusion checks + rank + hysteresis + probe cadence + snapshot and
    // counter publish), vs the static-pin baseline (body clone + verbatim
    // model-field compare — the pre-family hot path).
    let mut seam_samples = Vec::with_capacity(2000);
    for _ in 0..2000 {
        let start = Instant::now();
        std::hint::black_box(router.resolve_family("auto-coding"));
        seam_samples.push(start.elapsed());
    }
    let seam_p99 = percentile_p99(seam_samples);

    let body = serde_json::json!({"model": "model-0:free"});
    let mut base_samples = Vec::with_capacity(2000);
    for _ in 0..2000 {
        let start = Instant::now();
        let owned = body.clone();
        std::hint::black_box(
            owned.get("model").and_then(serde_json::Value::as_str) == Some("auto-coding"),
        );
        base_samples.push(start.elapsed());
    }
    let base_p99 = percentile_p99(base_samples);

    println!("family resolution overhead (8 members, p99 over iters):");
    println!("  rank micro-bench (5000 iters):   {rank_p99:?}");
    println!("  dispatch seam resolve_family (2000 iters): {seam_p99:?}");
    println!("  static-pin baseline (2000 iters): {base_p99:?}");
    println!("  budget: 1ms");

    assert!(
        rank_p99 < Duration::from_millis(1),
        "rank p99 {rank_p99:?} must stay under the 1ms budget"
    );
    assert!(
        seam_p99 < Duration::from_millis(1),
        "dispatch-seam p99 {seam_p99:?} must stay under the 1ms budget"
    );
}
