//! Static/config-overridable `PricingTable`, vendored `LiteLLM` snapshot, and
//! an optional background live-refresh task (never on the hot read/write
//! path — pricing is always read synchronously from an already-resolved
//! `Arc<PricingTable>`).
//!
//! See `project_plans/compaction-cost-metrics/implementation/plan.md` Epic
//! 1.4 (ADR-013) for the full design. `CostTracker` (Epic 1.3) previously
//! carried a minimal local stub of `PricingTable`/`ModelPrice` just to
//! compile against; this module is the real implementation and
//! `src/cost_metrics/tracker.rs` now imports its types from here.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::watch;

use crate::cost_metrics::types::PricingSource;

/// The vendored `LiteLLM` snapshot, filtered to models `src/providers/*.rs`
/// actually routes to (Task 1.4.1a). Re-sync by re-fetching
/// `model_prices_and_context_window.json` from
/// `github.com/BerriAI/litellm` and re-filtering.
const PRICING_DEFAULT_JSON: &str = include_str!("pricing_default.json");

/// Upstream source for [`spawn_pricing_refresh_task`]'s live refresh — the
/// same `LiteLLM` file `pricing_default.json` was vendored from.
pub const LITELLM_PRICING_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

/// On-disk shape of `pricing_default.json`: a documented source/note plus
/// the actual per-model rates, so the fixture can carry its own provenance
/// without that provenance being parsed as a model entry.
#[derive(Debug, Deserialize)]
struct PricingSnapshot {
    models: HashMap<String, ModelPrice>,
}

/// Per-token USD rates for one model. Matches `LiteLLM`'s
/// `input_cost_per_token`/`output_cost_per_token` source data — never a
/// per-million rate (repair iteration 1, was arch B5.2/adv B5).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct ModelPrice {
    #[serde(rename = "input_cost_per_token")]
    pub input_usd_per_token: f64,
    #[serde(rename = "output_cost_per_token")]
    pub output_usd_per_token: f64,
}

/// Static/config-overridable model → price lookup. `price_for` on a model
/// present in neither the default snapshot nor any override returns `None`,
/// never a default/zero price (plan.md Story 1.4.1 AC).
#[derive(Debug, Clone, Default)]
pub struct PricingTable {
    prices: HashMap<String, ModelPrice>,
    source: PricingSource,
}

impl PricingTable {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this table's rates came from the static vendored snapshot or
    /// a successful live refresh (Story 1.4.2's `report_for_session`
    /// `pricing_source` tag).
    #[must_use]
    pub fn source(&self) -> PricingSource {
        self.source
    }

    /// Parse the vendored `pricing_default.json` fixture. No network
    /// dependency — this is requirements.md's "load-bearing fallback."
    ///
    /// # Panics
    ///
    /// Panics if the checked-in fixture fails to parse — that indicates the
    /// fixture itself is corrupt, not a runtime/caller error.
    #[must_use]
    #[allow(clippy::expect_used)]
    pub fn load_default() -> Self {
        let snapshot: PricingSnapshot = serde_json::from_str(PRICING_DEFAULT_JSON)
            .expect("pricing_default.json must parse as the documented PricingSnapshot shape");
        PricingTable {
            prices: snapshot.models,
            source: PricingSource::Static,
        }
    }

    pub fn insert(&mut self, model: impl Into<String>, price: ModelPrice) {
        self.prices.insert(model.into(), price);
    }

    /// An override for a model already in the table replaces it; a model
    /// not present is added.
    pub fn merge_overrides(&mut self, overrides: HashMap<String, ModelPrice>) {
        self.prices.extend(overrides);
    }

    #[must_use]
    pub fn price_for(&self, model: &str) -> Option<ModelPrice> {
        self.prices.get(model).copied()
    }
}

/// Process-wide count of pricing live-refresh fallback events
/// (`cost_metrics_pricing_fallback_total` in ux.md's Surface 3). Simple
/// atomic rather than routed through `crate::metrics::ProxyMetrics` — this
/// counter's only consumer today is `pricing.rs`'s own log/metric-parity
/// tests; wiring it into a shared metrics registry/endpoint is a separate
/// epic's concern.
static PRICING_FALLBACK_TOTAL: AtomicU64 = AtomicU64::new(0);

#[must_use]
pub fn pricing_fallback_total() -> u64 {
    PRICING_FALLBACK_TOTAL.load(Ordering::Relaxed)
}

/// Why a live pricing refresh attempt failed. Carried as structured text
/// (not just "failed") so an operator can distinguish a transient network
/// blip from a permanently broken upstream JSON shape (ux.md Surface 3 AC2).
#[derive(Debug)]
pub enum PricingRefreshError {
    Timeout,
    NonSuccessStatus(u16),
    MalformedJson(String),
}

impl fmt::Display for PricingRefreshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PricingRefreshError::Timeout => write!(f, "request timed out"),
            PricingRefreshError::NonSuccessStatus(status) => {
                write!(f, "non-200 response (status {status})")
            }
            PricingRefreshError::MalformedJson(detail) => {
                write!(f, "malformed JSON: {detail}")
            }
        }
    }
}

/// Fetch and parse `LiteLLM`'s raw pricing JSON from `url`, returning the
/// per-model rate map on success.
async fn fetch_live_pricing(
    client: &reqwest::Client,
    url: &str,
) -> Result<HashMap<String, ModelPrice>, PricingRefreshError> {
    let response = client.get(url).send().await.map_err(|e| {
        if e.is_timeout() {
            PricingRefreshError::Timeout
        } else {
            PricingRefreshError::NonSuccessStatus(0)
        }
    })?;

    let status = response.status();
    if !status.is_success() {
        return Err(PricingRefreshError::NonSuccessStatus(status.as_u16()));
    }

    let body = response
        .text()
        .await
        .map_err(|e| PricingRefreshError::MalformedJson(e.to_string()))?;

    serde_json::from_str(&body).map_err(|e| PricingRefreshError::MalformedJson(e.to_string()))
}

/// Run one live-refresh attempt against `url`.
///
/// On success, atomically swaps the `watch`-held `Arc<PricingTable>` that
/// pricing-at-write-time reads synchronously (Story 1.3.2's fold step reads
/// this, never `report_for_session` — that stays a pure read over
/// already-priced records).
///
/// On failure, the current table (static or last-successful-live) is left
/// in place. Exactly one `tracing::warn!` and one
/// `cost_metrics_pricing_fallback_total` increment happen in this same
/// branch, so a metrics dashboard and a raw log grep never disagree about
/// how many fallbacks occurred (ux.md Surface 3 AC1/AC3).
///
/// # Errors
///
/// Returns the underlying `PricingRefreshError` if the fetch times out,
/// the response is a non-2xx status, or the body fails to parse — the
/// static/last-successful-live table is left in place in all such cases.
pub async fn refresh_once(
    tx: &watch::Sender<Arc<PricingTable>>,
    client: &reqwest::Client,
    url: &str,
) -> Result<(), PricingRefreshError> {
    match fetch_live_pricing(client, url).await {
        Ok(prices) => {
            tx.send_replace(Arc::new(PricingTable {
                prices,
                source: PricingSource::Live,
            }));
            Ok(())
        }
        Err(err) => {
            PRICING_FALLBACK_TOTAL.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                reason = %err,
                "pricing table live refresh failed, falling back to static table"
            );
            Err(err)
        }
    }
}

/// Spawn a background task that calls `refresh_once` on a fixed interval
/// (default e.g. 24h — the caller decides). Never panics, never blocks any
/// `apply()` caller: a fetch failure is handled entirely inside
/// `refresh_once`.
#[must_use]
pub fn spawn_pricing_refresh_task(
    tx: watch::Sender<Arc<PricingTable>>,
    client: reqwest::Client,
    url: String,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // The first tick fires immediately; skip it so the static table is
        // used until the first real interval elapses.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let _ = refresh_once(&tx, &client, &url).await;
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use axum::extract::State;
    use axum::routing::get;
    use axum::Router;
    use tokio::net::TcpListener;
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    fn tc_price(input: f64, output: f64) -> ModelPrice {
        ModelPrice {
            input_usd_per_token: input,
            output_usd_per_token: output,
        }
    }

    #[test]
    fn pricing_table_load_default_should_return_model_price_when_model_present_in_snapshot() {
        let table = PricingTable::load_default();
        let price = table
            .price_for("claude-sonnet-5")
            .expect("claude-sonnet-5 must be present in the vendored snapshot");
        assert_eq!(price.input_usd_per_token, 0.000_003);
        assert_eq!(price.output_usd_per_token, 0.000_015);
    }

    #[test]
    fn price_for_should_return_none_when_model_absent_from_default_and_overrides() {
        let table = PricingTable::load_default();
        assert_eq!(table.price_for("no-such-model-anywhere"), None);
    }

    #[test]
    fn merge_overrides_should_replace_existing_and_add_new_model_when_called() {
        let mut table = PricingTable::new();
        table.insert("claude-sonnet-5", tc_price(0.000_003, 0.000_015));

        let mut overrides = HashMap::new();
        overrides.insert(
            "claude-sonnet-5".to_string(),
            tc_price(0.000_004, 0.000_015),
        );
        overrides.insert(
            "brand-new-model".to_string(),
            tc_price(0.000_001, 0.000_002),
        );

        table.merge_overrides(overrides);

        assert_eq!(
            table
                .price_for("claude-sonnet-5")
                .unwrap()
                .input_usd_per_token,
            0.000_004
        );
        assert_eq!(
            table
                .price_for("brand-new-model")
                .unwrap()
                .input_usd_per_token,
            0.000_001
        );
    }

    /// Task 1.4.1e — every distinct model-name string `src/providers/*.rs`
    /// can emit must resolve against the vendored snapshot. Enumerated via
    /// `grep -rn "normalize_model_name\|claude-\|gpt-" src/providers/*.rs`
    /// (not re-typed from memory): `src/providers/bedrock.rs`'s
    /// `MODEL_MAPPING` short-name keys, `src/providers/anthropic.rs`'s
    /// `normalize_model_name` output (its own unit test asserts
    /// `"us.anthropic.claude-3-5-sonnet-20241022-v2:0"` normalizes to
    /// `"claude-3-5-sonnet-20241022"`), and `src/providers/mod.rs`'s
    /// hardcoded default fallback (`"claude-3-haiku-20240307"`).
    #[test]
    fn price_for_should_resolve_real_provider_model_name_strings_when_queried() {
        let table = PricingTable::load_default();
        let provider_emitted_names = [
            "claude-sonnet-4-6",
            "claude-opus-4-6",
            "claude-sonnet-4-5-20250929",
            "claude-opus-4-5-20251101",
            "claude-haiku-4-5-20251001",
            "claude-3-7-sonnet-20250219",
            "claude-3-5-haiku-20241022",
            "claude-3-haiku-20240307",
            "claude-3-5-sonnet-20241022",
        ];
        for name in provider_emitted_names {
            assert!(
                table.price_for(name).is_some(),
                "expected pricing_default.json to resolve provider-emitted model name {name:?}"
            );
        }
    }

    // ---- Live-refresh fallback (Story 1.4.2, stretch) ----

    // `PRICING_FALLBACK_TOTAL` is a single process-wide counter (by design —
    // it backs one real `cost_metrics_pricing_fallback_total` metric).
    // `cargo test` runs `#[tokio::test]`s concurrently across threads, so
    // any test that triggers a fallback (whether or not it asserts an exact
    // count) must not overlap with another such test, or a fallback from one
    // leaks into another's delta assertion. Every test in this module that
    // drives `refresh_once` down its `Err` path takes this lock first. It is
    // test-only serialization; it does not change production behavior.
    // `tokio::sync::Mutex`, not `std::sync::Mutex`: the guard is held across
    // `.await` points for the whole test body by design (see above), and an
    // async-aware mutex is the correct primitive for that rather than a
    // std lock (which clippy's `await_holding_lock` flags for good reason).
    static FALLBACK_COUNTER_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[derive(Clone, Default)]
    struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("log buffer mutex poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for SharedBuf {
        type Writer = SharedBuf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl SharedBuf {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().expect("log buffer mutex poisoned").clone())
                .expect("log output must be valid utf8")
        }
    }

    struct FailingServerState {
        status: axum::http::StatusCode,
        delay: Duration,
        hit_count: AtomicUsize,
    }

    async fn handle_pricing_fetch(
        State(state): State<Arc<FailingServerState>>,
    ) -> axum::http::StatusCode {
        state.hit_count.fetch_add(1, Ordering::SeqCst);
        if !state.delay.is_zero() {
            tokio::time::sleep(state.delay).await;
        }
        state.status
    }

    async fn start_pricing_mock(
        status: axum::http::StatusCode,
        delay: Duration,
    ) -> (String, Arc<FailingServerState>, tokio::task::JoinHandle<()>) {
        let state = Arc::new(FailingServerState {
            status,
            delay,
            hit_count: AtomicUsize::new(0),
        });
        let app = Router::new()
            .route("/pricing.json", get(handle_pricing_fetch))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock pricing server bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}/pricing.json"), state, handle)
    }

    #[tokio::test]
    async fn pricing_refresh_task_should_leave_table_unchanged_and_log_warning_when_fetch_returns_500(
    ) {
        let _lock = FALLBACK_COUNTER_TEST_LOCK.lock().await;
        let (url, state, _server) = start_pricing_mock(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Duration::ZERO,
        )
        .await;
        let client = reqwest::Client::new();
        let (tx, rx) = watch::channel(Arc::new(PricingTable::load_default()));
        let before = rx.borrow().clone();
        let before_fallbacks = pricing_fallback_total();

        let buf = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .without_time()
            .with_target(false)
            .with_ansi(false)
            .finish();
        let result = {
            let _guard = tracing::subscriber::set_default(subscriber);
            refresh_once(&tx, &client, &url).await
        };

        assert!(result.is_err(), "500 response must be a refresh error");
        assert_eq!(state.hit_count.load(Ordering::SeqCst), 1);
        assert!(
            Arc::ptr_eq(&before, &rx.borrow()),
            "table must be left unchanged on fetch failure"
        );
        assert_eq!(pricing_fallback_total(), before_fallbacks + 1);
        let logs = buf.contents();
        assert_eq!(
            logs.matches("pricing table live refresh failed").count(),
            1,
            "fallback must be logged exactly once per event, logs: {logs}"
        );
    }

    #[tokio::test]
    async fn pricing_refresh_task_should_log_warning_exactly_once_when_fetch_fails_once() {
        // Also triggers a fallback increment; must not interleave with the
        // other fallback-triggering tests below (see
        // `FALLBACK_COUNTER_TEST_LOCK`'s doc comment).
        let _lock = FALLBACK_COUNTER_TEST_LOCK.lock().await;
        let (url, _state, _server) = start_pricing_mock(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Duration::ZERO,
        )
        .await;
        let client = reqwest::Client::new();
        let (tx, _rx) = watch::channel(Arc::new(PricingTable::load_default()));

        let buf = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .without_time()
            .with_target(false)
            .with_ansi(false)
            .finish();
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let _ = refresh_once(&tx, &client, &url).await;
        }

        let logs = buf.contents();
        assert_eq!(
            logs.lines().filter(|l| l.contains("WARN")).count(),
            1,
            "exactly one WARN line expected regardless of how many requests were served from the stale table, logs: {logs}"
        );
    }

    #[tokio::test]
    async fn pricing_fallback_log_should_include_failure_reason_when_timeout_occurs() {
        // Also triggers a fallback increment; must not interleave with the
        // other fallback-triggering tests (see
        // `FALLBACK_COUNTER_TEST_LOCK`'s doc comment).
        let _lock = FALLBACK_COUNTER_TEST_LOCK.lock().await;
        // A 50ms client timeout against a server that sleeps for 300ms
        // forces a `PricingRefreshError::Timeout`.
        let (url, _state, _server) =
            start_pricing_mock(axum::http::StatusCode::OK, Duration::from_millis(300)).await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(50))
            .build()
            .expect("client with timeout should build");
        let (tx, _rx) = watch::channel(Arc::new(PricingTable::load_default()));

        let buf = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .without_time()
            .with_target(false)
            .with_ansi(false)
            .finish();
        let result = {
            let _guard = tracing::subscriber::set_default(subscriber);
            refresh_once(&tx, &client, &url).await
        };

        assert!(matches!(result, Err(PricingRefreshError::Timeout)));
        let logs = buf.contents();
        assert!(
            logs.contains("timed out"),
            "expected the timeout reason to be logged as structured text, logs: {logs}"
        );
    }

    #[tokio::test]
    async fn pricing_fallback_should_increment_metric_in_same_code_path_as_log_line() {
        let _lock = FALLBACK_COUNTER_TEST_LOCK.lock().await;
        let (url, _state, _server) = start_pricing_mock(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Duration::ZERO,
        )
        .await;
        let client = reqwest::Client::new();
        let (tx, _rx) = watch::channel(Arc::new(PricingTable::load_default()));
        let before = pricing_fallback_total();

        let buf = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .without_time()
            .with_target(false)
            .with_ansi(false)
            .finish();
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let _ = refresh_once(&tx, &client, &url).await;
        }

        // Both the log line and the metric increment happen inside
        // refresh_once's single Err branch — asserting both landed from one
        // call proves they can never disagree.
        assert_eq!(pricing_fallback_total(), before + 1);
        assert_eq!(
            buf.contents()
                .matches("pricing table live refresh failed")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn pricing_refresh_task_should_update_table_when_fetch_returns_200() {
        let mut snapshot = serde_json::Map::new();
        snapshot.insert(
            "brand-new-live-model".to_string(),
            serde_json::json!({"input_cost_per_token": 0.000_009, "output_cost_per_token": 0.000_02}),
        );
        let body = serde_json::Value::Object(snapshot);

        let state = Arc::new(std::sync::Mutex::new(body));
        let app_state = state.clone();
        let app = Router::new().route(
            "/pricing.json",
            get(move || {
                let state = app_state.clone();
                async move { axum::Json(state.lock().expect("mutex poisoned").clone()) }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let _server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = reqwest::Client::new();
        let (tx, rx) = watch::channel(Arc::new(PricingTable::load_default()));
        refresh_once(&tx, &client, &format!("http://{addr}/pricing.json"))
            .await
            .expect("200 response should succeed");

        assert_eq!(
            rx.borrow()
                .price_for("brand-new-live-model")
                .unwrap()
                .input_usd_per_token,
            0.000_009
        );
    }
}
