//! Cached, typed, sanitized per-upstream model lists (Epic 3 of
//! `webui-trace-model-mapping`).
//!
//! A refresh never loses the last good list: a failure turns `Ok` into
//! `Stale`, and only a never-loaded upstream becomes `Error`. Error text that
//! reaches the browser is a kind, a status and a fixed phrase; upstream bodies
//! (which can echo keys or account ARNs) are logged at `debug` only.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::config::schema::{CatalogRefreshMode, Config, ModelCatalogConfig, UpstreamKind};
use crate::metrics::ids::UpstreamName;
use crate::providers::{ModelInfo, Provider, ProviderError};

/// Most ids sent per upstream; the rest are counted in `total_count`.
pub const MAX_MODELS_SENT: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CatalogState {
    Loading,
    Ok,
    Stale,
    Error,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CatalogErrorKind {
    Auth,
    RateLimited,
    Timeout,
    Upstream,
    UnexpectedResponse,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogError {
    pub kind: CatalogErrorKind,
    pub status: Option<u16>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelEntry {
    pub id: String,
    pub owned_by: Option<String>,
}

impl From<ModelInfo> for ModelEntry {
    fn from(info: ModelInfo) -> Self {
        Self {
            id: info.id,
            owned_by: info.owned_by,
        }
    }
}

/// The outcome of one `list_models` call.
#[derive(Debug, Clone)]
pub enum RefreshResult {
    Models(Vec<ModelInfo>),
    Failed(ProviderError),
}

/// Wall-clock time for display plus a monotonic instant for throttle and
/// retry-floor arithmetic (which must follow tokio's paused clock in tests).
#[derive(Debug, Clone, Copy)]
pub struct Stamp {
    pub wall: DateTime<Utc>,
    pub instant: Instant,
}

impl Stamp {
    #[must_use]
    pub fn now() -> Self {
        Self {
            wall: Utc::now(),
            instant: Instant::now(),
        }
    }
}

/// One upstream's catalog entry as sent to the browser (plus two private
/// instants the throttle logic reads).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamModels {
    pub state: CatalogState,
    pub models: Vec<ModelEntry>,
    pub total_count: usize,
    pub truncated: bool,
    pub refreshed_at: Option<DateTime<Utc>>,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub error: Option<CatalogError>,
    pub refreshing: bool,
    #[serde(skip)]
    pub(crate) last_attempt: Option<Instant>,
    #[serde(skip)]
    pub(crate) last_success: Option<Instant>,
}

impl UpstreamModels {
    #[must_use]
    pub fn loading() -> Self {
        Self {
            state: CatalogState::Loading,
            models: Vec::new(),
            total_count: 0,
            truncated: false,
            refreshed_at: None,
            last_attempt_at: None,
            error: None,
            refreshing: false,
            last_attempt: None,
            last_success: None,
        }
    }

    /// Folds one refresh outcome into the entry; a failure never drops the
    /// last good list.
    pub fn apply(&mut self, result: RefreshResult, at: Stamp) {
        self.last_attempt_at = Some(at.wall);
        self.last_attempt = Some(at.instant);
        match result {
            RefreshResult::Models(infos) => {
                self.total_count = infos.len();
                self.truncated = infos.len() > MAX_MODELS_SENT;
                self.models = infos
                    .into_iter()
                    .take(MAX_MODELS_SENT)
                    .map(ModelEntry::from)
                    .collect();
                self.state = CatalogState::Ok;
                self.error = None;
                self.refreshed_at = Some(at.wall);
                self.last_success = Some(at.instant);
            }
            RefreshResult::Failed(ProviderError::ModelUnsupported(_)) => {
                self.models.clear();
                self.total_count = 0;
                self.truncated = false;
                self.refreshed_at = None;
                self.last_success = None;
                self.state = CatalogState::Unsupported;
                self.error = None;
            }
            RefreshResult::Failed(err) => {
                self.state = if self.refreshed_at.is_some() {
                    CatalogState::Stale
                } else {
                    CatalogState::Error
                };
                self.error = Some(classify(&err));
            }
        }
    }
}

/// Hard stop for one upstream's `list_models` call.
pub const PER_UPSTREAM_TIMEOUT: Duration = Duration::from_secs(10);
/// Minimum gap between manual refreshes of one upstream.
pub const MANUAL_THROTTLE: Duration = Duration::from_secs(10);

pub type NamedProvider = (UpstreamName, Arc<dyn Provider>);
/// Called once per finished refresh, after the in-flight guard is released.
pub type Notifier = Arc<dyn Fn(&UpstreamName) + Send + Sync>;

/// What asked for a refresh; decides which skip rules apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// A click: bypasses the retry floor, auth backoff and staleness, not the throttle.
    Manual,
    /// `?ifStale=true`: only `Loading` or old entries, subject to floor and backoff.
    IfStale,
    /// The opt-in timer: every upstream, subject to floor and backoff.
    Interval,
}

#[derive(Debug, Clone, Copy)]
pub enum Target<'a> {
    All,
    One(&'a UpstreamName),
}

/// Response fields every refresh request carries next to the snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshAck {
    pub refreshing: bool,
    pub coalesced: bool,
    pub retry_after_secs: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct UnknownUpstream;

/// The ack plus the background batch (awaited by tests, dropped by handlers).
pub struct RefreshStarted {
    pub ack: RefreshAck,
    pub task: Option<JoinHandle<()>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CatalogSnapshot {
    pub upstreams: BTreeMap<String, UpstreamModels>,
}

struct State {
    current: Vec<UpstreamName>,
    entries: HashMap<UpstreamName, UpstreamModels>,
}

/// Marks one upstream as refreshing for exactly as long as it lives, so a
/// panic or an aborted task can never leave the flag stuck.
struct InFlight {
    name: UpstreamName,
    set: Arc<Mutex<HashSet<UpstreamName>>>,
}

impl InFlight {
    fn claim(set: &Arc<Mutex<HashSet<UpstreamName>>>, name: &UpstreamName) -> Option<Self> {
        lock(set).insert(name.clone()).then(|| Self {
            name: name.clone(),
            set: Arc::clone(set),
        })
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        lock(&self.set).remove(&self.name);
    }
}

/// Aborts the wrapped task when dropped, so a timed-out or cancelled
/// `list_models` call cannot keep running detached.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

enum Decision {
    Start,
    Skip,
    Throttled(u64),
}

pub struct ModelCatalog {
    settings: ModelCatalogConfig,
    mode: CatalogRefreshMode,
    state: Mutex<State>,
    in_flight: Arc<Mutex<HashSet<UpstreamName>>>,
    coalesced_total: AtomicU64,
    notifier: Option<Notifier>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whole seconds, rounded up.
fn ceil_secs(d: Duration) -> u64 {
    d.as_secs() + u64::from(d.subsec_nanos() > 0)
}

impl ModelCatalog {
    #[must_use]
    pub fn new(settings: ModelCatalogConfig, notifier: Option<Notifier>) -> Arc<Self> {
        let mode = settings.refresh_mode();
        Arc::new(Self {
            settings,
            mode,
            state: Mutex::new(State {
                current: Vec::new(),
                entries: HashMap::new(),
            }),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            coalesced_total: AtomicU64::new(0),
            notifier,
        })
    }

    /// Makes `names` the current upstream set: removes the rest, adds
    /// `Loading` entries for new ones. A refresh still running for a removed
    /// name cannot resurrect it (`apply_result` checks `current`).
    pub fn prune(&self, names: &[UpstreamName]) {
        let mut state = lock(&self.state);
        state.entries.retain(|name, _| names.contains(name));
        for name in names {
            state
                .entries
                .entry(name.clone())
                .or_insert_with(UpstreamModels::loading);
        }
        state.current = names.to_vec();
    }

    #[must_use]
    pub fn snapshot(&self) -> CatalogSnapshot {
        let in_flight = lock(&self.in_flight).clone();
        let state = lock(&self.state);
        let upstreams = state
            .current
            .iter()
            .filter_map(|name| {
                let mut entry = state.entries.get(name)?.clone();
                entry.refreshing = in_flight.contains(name);
                Some((name.to_string(), entry))
            })
            .collect();
        CatalogSnapshot { upstreams }
    }

    #[must_use]
    pub fn coalesced_total(&self) -> u64 {
        self.coalesced_total.load(Ordering::Relaxed)
    }

    /// Counter and per-upstream summary for `GET /metrics`.
    #[must_use]
    pub fn stats_json(&self) -> serde_json::Value {
        let upstreams: serde_json::Map<String, serde_json::Value> = self
            .snapshot()
            .upstreams
            .into_iter()
            .map(|(name, entry)| {
                let summary = serde_json::json!({
                    "state": entry.state,
                    "refreshedAt": entry.refreshed_at,
                    "errorKind": entry.error.map(|e| e.kind),
                });
                (name, summary)
            })
            .collect();
        serde_json::json!({
            "catalog_refresh_coalesced_total": self.coalesced_total(),
            "upstreams": upstreams,
        })
    }

    fn decide(&self, entry: &UpstreamModels, trigger: Trigger, now: Instant) -> Decision {
        let since_attempt = entry
            .last_attempt
            .map(|at| now.saturating_duration_since(at));
        if trigger == Trigger::Manual {
            return match since_attempt {
                Some(elapsed) if elapsed < MANUAL_THROTTLE => {
                    Decision::Throttled(ceil_secs(MANUAL_THROTTLE - elapsed))
                }
                _ => Decision::Start,
            };
        }
        let auth_failed = entry
            .error
            .as_ref()
            .is_some_and(|e| e.kind == CatalogErrorKind::Auth);
        let floor = Duration::from_secs(self.settings.min_retry_secs);
        if auth_failed
            || entry.state == CatalogState::Unsupported
            || since_attempt.is_some_and(|elapsed| elapsed < floor)
        {
            return Decision::Skip;
        }
        let fresh = trigger == Trigger::IfStale
            && entry.state == CatalogState::Ok
            && entry.last_success.is_some_and(|at| {
                now.saturating_duration_since(at)
                    <= Duration::from_secs(self.settings.stale_after_secs)
            });
        if fresh {
            Decision::Skip
        } else {
            Decision::Start
        }
    }

    /// Decides per targeted upstream (start / coalesce / throttle / skip) and
    /// starts one background batch for the ones to run.
    ///
    /// # Errors
    ///
    /// `UnknownUpstream` when `target` names an upstream not in `providers`.
    pub fn request_refresh(
        self: &Arc<Self>,
        providers: &[NamedProvider],
        target: Target<'_>,
        trigger: Trigger,
    ) -> Result<RefreshStarted, UnknownUpstream> {
        let targets: Vec<&NamedProvider> = match target {
            Target::All => providers.iter().collect(),
            Target::One(wanted) => providers.iter().filter(|(n, _)| n == wanted).collect(),
        };
        if targets.is_empty() && matches!(target, Target::One(_)) {
            return Err(UnknownUpstream);
        }
        let automatic_ignored = self.mode == CatalogRefreshMode::Off && trigger != Trigger::Manual;
        let now = Instant::now();
        let mut started = Vec::new();
        let mut already_running = 0usize;
        let mut retry_after: Option<u64> = None;
        for (name, provider) in targets {
            if lock(&self.in_flight).contains(name) {
                already_running += 1;
                continue;
            }
            if automatic_ignored {
                continue;
            }
            let decision = match lock(&self.state).entries.get(name) {
                Some(entry) => self.decide(entry, trigger, now),
                None => continue,
            };
            match decision {
                Decision::Skip => {}
                Decision::Throttled(secs) => {
                    retry_after = Some(retry_after.map_or(secs, |prev| prev.min(secs)));
                }
                Decision::Start => match InFlight::claim(&self.in_flight, name) {
                    Some(guard) => started.push((name.clone(), Arc::clone(provider), guard)),
                    None => already_running += 1,
                },
            }
        }
        let coalesced = started.is_empty() && already_running > 0;
        if coalesced {
            self.coalesced_total.fetch_add(1, Ordering::Relaxed);
        }
        let ack = RefreshAck {
            refreshing: !started.is_empty() || already_running > 0,
            coalesced,
            retry_after_secs: retry_after,
        };
        let task = (!started.is_empty()).then(|| tokio::spawn(Arc::clone(self).run_batch(started)));
        Ok(RefreshStarted { ack, task })
    }

    /// Manual refresh of every upstream, awaited to completion.
    pub async fn refresh_all(self: &Arc<Self>, providers: &[NamedProvider]) -> RefreshAck {
        let Ok(started) = self.request_refresh(providers, Target::All, Trigger::Manual) else {
            return RefreshAck::default();
        };
        if let Some(task) = started.task {
            let _ = task.await;
        }
        started.ack
    }

    async fn run_batch(self: Arc<Self>, started: Vec<(UpstreamName, Arc<dyn Provider>, InFlight)>) {
        let mut set = tokio::task::JoinSet::new();
        for (name, provider, guard) in started {
            set.spawn(Arc::clone(&self).run_one(name, provider, guard));
        }
        while set.join_next().await.is_some() {}
    }

    async fn run_one(
        self: Arc<Self>,
        name: UpstreamName,
        provider: Arc<dyn Provider>,
        guard: InFlight,
    ) {
        let began = Instant::now();
        let result = match list_with_timeout(provider, PER_UPSTREAM_TIMEOUT).await {
            Ok(models) => RefreshResult::Models(models),
            Err(err) => RefreshResult::Failed(err),
        };
        let duration_ms = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
        log_refresh(&name, &result, duration_ms);
        self.apply_result(&name, result, Stamp::now());
        drop(guard);
        if let Some(notify) = &self.notifier {
            notify(&name);
        }
    }

    /// Writes `result` only while `name` is still a current upstream (the
    /// check and the write share one lock, so a prune cannot interleave).
    fn apply_result(&self, name: &UpstreamName, result: RefreshResult, at: Stamp) -> bool {
        let mut state = lock(&self.state);
        if !state.current.contains(name) {
            return false;
        }
        state.entries.get_mut(name).is_some_and(|entry| {
            entry.apply(result, at);
            true
        })
    }
}

/// Exactly one log line per refresh; never model ids or error text.
fn log_refresh(name: &UpstreamName, result: &RefreshResult, duration_ms: u64) {
    match result {
        RefreshResult::Models(models) => tracing::info!(
            upstream = %name,
            outcome = "ok",
            model_count = models.len(),
            duration_ms,
            "model catalog refreshed"
        ),
        RefreshResult::Failed(ProviderError::ModelUnsupported(_)) => tracing::info!(
            upstream = %name,
            outcome = "unsupported",
            duration_ms,
            "model catalog refresh: listing unsupported"
        ),
        RefreshResult::Failed(err) => {
            let classified = classify(err);
            tracing::warn!(
                upstream = %name,
                outcome = "error",
                kind = ?classified.kind,
                status = ?classified.status,
                duration_ms,
                "model catalog refresh failed"
            );
        }
    }
}

/// Runs `list_models` in its own task so a panic is contained and a timeout
/// really stops the call.
async fn list_with_timeout(
    provider: Arc<dyn Provider>,
    limit: Duration,
) -> Result<Vec<ModelInfo>, ProviderError> {
    let mut task = AbortOnDrop(tokio::spawn(async move { provider.list_models().await }));
    match tokio::time::timeout(limit, &mut task.0).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(ProviderError::Upstream {
            status: 0,
            body: "list_models task failed".to_string(),
        }),
        Err(_) => Err(ProviderError::Timeout),
    }
}

/// Opt-in timer: one `Interval` refresh every `secs`, starting immediately,
/// until the catalog is dropped.
pub async fn run_refresh_loop(
    catalog: std::sync::Weak<ModelCatalog>,
    secs: u64,
    providers: impl Fn() -> Vec<NamedProvider> + Send + 'static,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(secs));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let Some(catalog) = catalog.upgrade() else {
            return;
        };
        let _ = catalog.request_refresh(&providers(), Target::All, Trigger::Interval);
    }
}

/// A pinned model the upstream's catalog does not list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigWarning {
    pub upstream: String,
    pub model: String,
    pub referenced_by: String,
}

/// Models pinned on the primary route (`config.routes.first()`, as
/// `select_primary_route` picks it) that a loaded catalog does not list.
/// Not checked: other routes, `model_family` refs, fallback targets outside
/// this route, and upstreams that are loading, errored, unsupported or whose
/// list was truncated (absence proves nothing there).
#[must_use]
pub fn config_warnings(config: &Config, snapshot: &CatalogSnapshot) -> Vec<ConfigWarning> {
    let Some(route) = config.routes.first() else {
        return Vec::new();
    };
    let mut warnings = Vec::new();
    for (index, reference) in route.upstreams.iter().enumerate() {
        let (Some(model), None) = (&reference.model, &reference.model_family) else {
            continue;
        };
        let Some(entry) = snapshot.upstreams.get(&reference.name) else {
            continue;
        };
        let listing_is_complete =
            matches!(entry.state, CatalogState::Ok | CatalogState::Stale) && !entry.truncated;
        if !listing_is_complete {
            continue;
        }
        let is_bedrock = config
            .upstreams
            .iter()
            .any(|u| u.name == reference.name && matches!(u.kind, UpstreamKind::Bedrock { .. }));
        let key = |id: &str| {
            if is_bedrock {
                crate::providers::bedrock::BedrockProvider::normalize_model(id)
            } else {
                id.to_string()
            }
        };
        let wanted = key(model);
        if !entry.models.iter().any(|m| key(&m.id) == wanted) {
            warnings.push(ConfigWarning {
                upstream: reference.name.clone(),
                model: model.clone(),
                referenced_by: format!("route {} upstream {index}", route.name),
            });
        }
    }
    warnings
}

/// Maps a provider error to the only error data the browser may see.
/// The full error goes to `debug` logs.
#[must_use]
pub fn classify(err: &ProviderError) -> CatalogError {
    tracing::debug!(error = %err, "model catalog refresh error (unsanitized)");
    let (kind, status, message) = match err {
        ProviderError::Auth(_) => (
            CatalogErrorKind::Auth,
            None,
            "Upstream rejected credentials".to_string(),
        ),
        ProviderError::RateLimited | ProviderError::RateLimitedWithRetry { .. } => (
            CatalogErrorKind::RateLimited,
            Some(429),
            "Upstream rate limited the request (429)".to_string(),
        ),
        ProviderError::Timeout => (
            CatalogErrorKind::Timeout,
            None,
            "Upstream did not answer in time".to_string(),
        ),
        ProviderError::Upstream { status, .. } => classify_status(*status),
        // Anthropic reports every 4xx (401/403/429 included) as `Validation`.
        ProviderError::Validation(_, status) => classify_status(*status),
        ProviderError::ResponseShapeMismatch(_) => (
            CatalogErrorKind::UnexpectedResponse,
            None,
            "Upstream returned an unexpected response".to_string(),
        ),
        ProviderError::ModelUnsupported(_) => (
            CatalogErrorKind::Other,
            None,
            "Upstream does not support listing models".to_string(),
        ),
        ProviderError::Exhausted => (
            CatalogErrorKind::Other,
            None,
            "Upstream request failed".to_string(),
        ),
    };
    CatalogError {
        kind,
        status,
        message,
    }
}

/// Status 0 is the Bedrock SDK's "no HTTP status" marker; its body can carry
/// account ARNs, so only the fixed phrase survives.
fn classify_status(status: u16) -> (CatalogErrorKind, Option<u16>, String) {
    match status {
        0 => (
            CatalogErrorKind::Other,
            None,
            "Upstream request failed".to_string(),
        ),
        401 | 403 => (
            CatalogErrorKind::Auth,
            Some(status),
            format!("Upstream rejected credentials ({status})"),
        ),
        429 => (
            CatalogErrorKind::RateLimited,
            Some(429),
            "Upstream rate limited the request (429)".to_string(),
        ),
        _ => (
            CatalogErrorKind::Upstream,
            Some(status),
            format!("Upstream returned an error ({status})"),
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn infos(n: usize) -> Vec<ModelInfo> {
        (0..n)
            .map(|i| ModelInfo {
                id: format!("model-{i}"),
                owned_by: Some("acme".to_string()),
            })
            .collect()
    }

    fn stamp(secs: i64) -> Stamp {
        Stamp {
            wall: DateTime::from_timestamp(1_760_000_000 + secs, 0).unwrap(),
            instant: Instant::now(),
        }
    }

    #[tokio::test]
    async fn apply_should_keep_last_good_list_and_become_stale_when_refresh_fails_with_auth() {
        let mut entry = UpstreamModels::loading();
        let t0 = stamp(0);
        entry.apply(RefreshResult::Models(infos(31)), t0);
        assert_eq!(entry.state, CatalogState::Ok);

        entry.apply(
            RefreshResult::Failed(ProviderError::Upstream {
                status: 403,
                body: "denied".into(),
            }),
            stamp(60),
        );

        assert_eq!(entry.state, CatalogState::Stale);
        assert_eq!(entry.models.len(), 31);
        assert_eq!(entry.refreshed_at, Some(t0.wall));
        assert_eq!(entry.error.as_ref().unwrap().kind, CatalogErrorKind::Auth);
    }

    #[tokio::test]
    async fn apply_should_become_error_with_empty_models_when_first_refresh_times_out() {
        let mut entry = UpstreamModels::loading();

        entry.apply(RefreshResult::Failed(ProviderError::Timeout), stamp(0));

        assert_eq!(entry.state, CatalogState::Error);
        assert!(entry.models.is_empty());
        assert_eq!(
            entry.error.as_ref().unwrap().kind,
            CatalogErrorKind::Timeout
        );
        assert_eq!(entry.refreshed_at, None);
    }

    #[tokio::test]
    async fn apply_should_cap_at_1000_and_flag_truncated_when_upstream_returns_1200() {
        let mut entry = UpstreamModels::loading();

        entry.apply(RefreshResult::Models(infos(1200)), stamp(0));

        assert_eq!(entry.models.len(), 1000);
        assert_eq!(entry.total_count, 1200);
        assert!(entry.truncated);
    }

    #[tokio::test]
    async fn classify_should_return_sanitized_auth_error_without_sentinel_when_upstream_401() {
        let err = ProviderError::Upstream {
            status: 401,
            body: "bad key sk-ant-SENTINEL123 at https://x/v1/models?key=AIzaSENTINEL".into(),
        };

        let classified = classify(&err);

        assert_eq!(
            classified,
            CatalogError {
                kind: CatalogErrorKind::Auth,
                status: Some(401),
                message: "Upstream rejected credentials (401)".into(),
            }
        );
        let mut entry = UpstreamModels::loading();
        entry.apply(RefreshResult::Failed(err), stamp(0));
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("SENTINEL"), "{json}");
    }

    #[test]
    fn classify_should_treat_validation_401_and_403_as_auth_because_anthropic_maps_all_4xx_to_validation(
    ) {
        for status in [401, 403] {
            let classified = classify(&ProviderError::Validation("SENTINEL".into(), status));

            assert_eq!(classified.kind, CatalogErrorKind::Auth);
            assert_eq!(classified.status, Some(status));
            assert!(!classified.message.contains("SENTINEL"));
        }
        assert_eq!(
            classify(&ProviderError::Validation("x".into(), 400)).kind,
            CatalogErrorKind::Upstream
        );
    }

    #[test]
    fn classify_should_hide_account_id_when_bedrock_status_zero() {
        let err = ProviderError::Upstream {
            status: 0,
            body: "arn:aws:iam::123456789012:role/x not authorized".into(),
        };

        let classified = classify(&err);

        assert_eq!(classified.status, None);
        assert!(matches!(
            classified.kind,
            CatalogErrorKind::Auth | CatalogErrorKind::Other
        ));
        let json = serde_json::to_string(&classified).unwrap();
        assert!(!json.contains("123456789012"), "{json}");
    }

    #[tokio::test]
    async fn classify_should_map_model_unsupported_to_unsupported_state() {
        let mut entry = UpstreamModels::loading();

        entry.apply(
            RefreshResult::Failed(ProviderError::ModelUnsupported("x".into())),
            stamp(0),
        );

        assert_eq!(entry.state, CatalogState::Unsupported);
    }

    #[test]
    fn catalog_json_should_not_contain_key_or_arn_sentinel_after_classify() {
        let errors = [
            ProviderError::Auth("token sk-SENTINEL-1".into()),
            ProviderError::Validation("key=SENTINEL-2".into(), 400),
            ProviderError::ModelUnsupported("SENTINEL-3".into()),
            ProviderError::ResponseShapeMismatch("SENTINEL-4".into()),
            ProviderError::Upstream {
                status: 500,
                body: "SENTINEL-5".into(),
            },
            ProviderError::Upstream {
                status: 0,
                body: "arn:aws:iam::123456789012:role/SENTINEL-6".into(),
            },
        ];
        for err in errors {
            let json = serde_json::to_string(&classify(&err)).unwrap();
            assert!(
                !json.contains("SENTINEL") && !json.contains("123456789012"),
                "{json}"
            );
        }
    }
}

#[cfg(test)]
// Test names are pre-designed in validation.md and keep `ifStale`/`lastAttemptAt` verbatim.
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    non_snake_case
)]
mod refresh_tests {
    use std::sync::atomic::AtomicUsize;

    use tokio::sync::Notify;

    use super::*;
    use crate::config::schema::CatalogRefreshSetting;

    #[derive(Clone)]
    enum Behavior {
        Models(Vec<&'static str>),
        Fail(fn() -> ProviderError),
        Delay(Duration, Vec<&'static str>),
        Hang,
        Gated(Arc<Notify>, Vec<&'static str>),
        Panic,
    }

    struct FakeProvider {
        calls: Arc<AtomicUsize>,
        behavior: Behavior,
    }

    #[async_trait::async_trait]
    impl Provider for FakeProvider {
        fn name(&self) -> &str {
            "fake"
        }
        async fn send(
            &self,
            _body: serde_json::Value,
            _headers: http::HeaderMap,
            _stream: bool,
        ) -> Result<crate::providers::ProviderResponse, ProviderError> {
            Err(ProviderError::Exhausted)
        }
        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let to_infos = |ids: &[&str]| {
                ids.iter()
                    .map(|id| ModelInfo {
                        id: (*id).to_string(),
                        owned_by: None,
                    })
                    .collect::<Vec<_>>()
            };
            match &self.behavior {
                Behavior::Models(ids) => Ok(to_infos(ids)),
                Behavior::Fail(make) => Err(make()),
                Behavior::Delay(d, ids) => {
                    tokio::time::sleep(*d).await;
                    Ok(to_infos(ids))
                }
                Behavior::Hang => std::future::pending().await,
                Behavior::Gated(gate, ids) => {
                    gate.notified().await;
                    Ok(to_infos(ids))
                }
                Behavior::Panic => panic!("fake list_models panic"),
            }
        }
    }

    fn fake(behavior: Behavior) -> (Arc<AtomicUsize>, Arc<dyn Provider>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(FakeProvider {
            calls: Arc::clone(&calls),
            behavior,
        });
        (calls, provider)
    }

    fn name(n: &str) -> UpstreamName {
        UpstreamName::from(n)
    }

    fn catalog_with(settings: ModelCatalogConfig, upstreams: &[&str]) -> Arc<ModelCatalog> {
        let catalog = ModelCatalog::new(settings, None);
        let names: Vec<UpstreamName> = upstreams.iter().map(|n| name(n)).collect();
        catalog.prune(&names);
        catalog
    }

    fn state_of(catalog: &ModelCatalog, upstream: &str) -> CatalogState {
        catalog.snapshot().upstreams[upstream].state
    }

    async fn finish(started: RefreshStarted) -> RefreshAck {
        if let Some(task) = started.task {
            task.await.unwrap();
        }
        started.ack
    }

    async fn settle() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    fn auth_error() -> ProviderError {
        ProviderError::Upstream {
            status: 401,
            body: "no".into(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn on_demand_mode_should_make_no_list_models_call_at_startup_or_hot_swap_or_get() {
        let (calls, provider) = fake(Behavior::Models(vec!["m"]));
        let providers = vec![(name("a"), provider)];
        let catalog = catalog_with(ModelCatalogConfig::default(), &["a"]);

        catalog.prune(&[name("a")]);
        for _ in 0..50 {
            let _ = catalog.snapshot();
        }
        tokio::time::advance(Duration::from_secs(3600)).await;

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(state_of(&catalog, "a"), CatalogState::Loading);
        drop(providers);
    }

    #[tokio::test(start_paused = true)]
    async fn ten_concurrent_ifStale_posts_should_call_list_models_once_and_count_9_coalesced() {
        let gate = Arc::new(Notify::new());
        let (calls, provider) = fake(Behavior::Gated(Arc::clone(&gate), vec!["m"]));
        let providers = vec![(name("a"), provider)];
        let catalog = catalog_with(ModelCatalogConfig::default(), &["a"]);

        let mut starts = Vec::new();
        for _ in 0..10 {
            starts.push(
                catalog
                    .request_refresh(&providers, Target::All, Trigger::IfStale)
                    .unwrap(),
            );
        }
        tokio::task::yield_now().await;
        assert!(starts.iter().all(|s| s.ack.refreshing));
        assert!(catalog.snapshot().upstreams["a"].refreshing);
        gate.notify_one();
        let mut acks = Vec::new();
        for s in starts {
            acks.push(finish(s).await);
        }

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(catalog.coalesced_total(), 9);
        assert_eq!(acks.iter().filter(|a| a.coalesced).count(), 9);
        assert_eq!(state_of(&catalog, "a"), CatalogState::Ok);
        assert!(!catalog.snapshot().upstreams["a"].refreshing);
    }

    #[tokio::test(start_paused = true)]
    async fn refreshing_flag_should_reset_after_list_models_panics_or_task_aborted() {
        let (_, panicking) = fake(Behavior::Panic);
        let catalog = catalog_with(ModelCatalogConfig::default(), &["a"]);
        let providers = vec![(name("a"), panicking)];

        let started = catalog
            .request_refresh(&providers, Target::All, Trigger::Manual)
            .unwrap();
        finish(started).await;
        assert!(!catalog.snapshot().upstreams["a"].refreshing);
        assert_eq!(state_of(&catalog, "a"), CatalogState::Error);

        tokio::time::advance(Duration::from_secs(11)).await;
        let (calls, hanging) = fake(Behavior::Hang);
        let providers = vec![(name("a"), hanging)];
        let started = catalog
            .request_refresh(&providers, Target::All, Trigger::Manual)
            .unwrap();
        tokio::task::yield_now().await;
        assert!(catalog.snapshot().upstreams["a"].refreshing);
        let task = started.task.unwrap();
        task.abort();
        let _ = task.await;
        assert!(!catalog.snapshot().upstreams["a"].refreshing);

        tokio::time::advance(Duration::from_secs(11)).await;
        let again = catalog
            .request_refresh(&providers, Target::All, Trigger::Manual)
            .unwrap();
        assert!(again.ack.refreshing && !again.ack.coalesced);
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_one_should_call_only_named_upstream_and_coalesce_when_in_flight() {
        let gate = Arc::new(Notify::new());
        let (a_calls, a) = fake(Behavior::Models(vec!["x"]));
        let (b_calls, b) = fake(Behavior::Gated(Arc::clone(&gate), vec!["y"]));
        let providers = vec![(name("a"), a), (name("b"), b)];
        let catalog = catalog_with(ModelCatalogConfig::default(), &["a", "b"]);

        let first = catalog
            .request_refresh(&providers, Target::One(&name("b")), Trigger::Manual)
            .unwrap();
        tokio::task::yield_now().await;
        let second = catalog
            .request_refresh(&providers, Target::One(&name("b")), Trigger::Manual)
            .unwrap();
        let unknown =
            catalog.request_refresh(&providers, Target::One(&name("nope")), Trigger::Manual);

        assert!(first.ack.refreshing && !first.ack.coalesced);
        assert!(second.ack.coalesced && second.task.is_none());
        assert!(matches!(unknown, Err(UnknownUpstream)));
        gate.notify_one();
        finish(first).await;
        assert_eq!(a_calls.load(Ordering::SeqCst), 0);
        assert_eq!(b_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_floor_should_skip_at_60s_and_run_at_301s_measured_from_lastAttemptAt() {
        let (calls, provider) = fake(Behavior::Fail(|| ProviderError::Timeout));
        let providers = vec![(name("a"), provider)];
        let catalog = catalog_with(ModelCatalogConfig::default(), &["a"]);
        finish(
            catalog
                .request_refresh(&providers, Target::All, Trigger::Manual)
                .unwrap(),
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        tokio::time::advance(Duration::from_secs(60)).await;
        let skipped = catalog
            .request_refresh(&providers, Target::All, Trigger::IfStale)
            .unwrap();
        assert!(!skipped.ack.refreshing && skipped.task.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        tokio::time::advance(Duration::from_secs(241)).await;
        finish(
            catalog
                .request_refresh(&providers, Target::All, Trigger::IfStale)
                .unwrap(),
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn auth_failure_should_skip_automatic_refresh_until_manual() {
        let (calls, provider) = fake(Behavior::Fail(auth_error));
        let providers = vec![(name("a"), provider)];
        let catalog = catalog_with(ModelCatalogConfig::default(), &["a"]);
        finish(
            catalog
                .request_refresh(&providers, Target::All, Trigger::Manual)
                .unwrap(),
        )
        .await;

        tokio::time::advance(Duration::from_secs(3600)).await;
        for trigger in [Trigger::IfStale, Trigger::Interval] {
            let started = catalog
                .request_refresh(&providers, Target::All, trigger)
                .unwrap();
            assert!(started.task.is_none());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        finish(
            catalog
                .request_refresh(&providers, Target::All, Trigger::Manual)
                .unwrap(),
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn off_mode_should_refresh_only_on_manual_post_without_ifStale() {
        let (calls, provider) = fake(Behavior::Models(vec!["m"]));
        let providers = vec![(name("a"), provider)];
        let settings = ModelCatalogConfig {
            refresh: CatalogRefreshSetting::Off,
            ..ModelCatalogConfig::default()
        };
        let catalog = catalog_with(settings, &["a"]);

        let ignored = catalog
            .request_refresh(&providers, Target::All, Trigger::IfStale)
            .unwrap();
        assert!(ignored.task.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        finish(
            catalog
                .request_refresh(&providers, Target::All, Trigger::Manual)
                .unwrap(),
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn interval_mode_should_clamp_below_300s_and_refresh_on_timer() {
        let (calls, provider) = fake(Behavior::Models(vec!["m"]));
        let settings = ModelCatalogConfig {
            refresh: CatalogRefreshSetting::Interval,
            interval_secs: 60,
            ..ModelCatalogConfig::default()
        };
        let CatalogRefreshMode::Interval { secs } = settings.refresh_mode() else {
            panic!("interval expected");
        };
        assert_eq!(secs, 300);
        let catalog = catalog_with(settings, &["a"]);
        let providers = vec![(name("a"), provider)];
        let loop_task = tokio::spawn(run_refresh_loop(
            Arc::downgrade(&catalog),
            secs,
            move || providers.clone(),
        ));

        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(299)).await;
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(2)).await;
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        drop(catalog);
        tokio::time::advance(Duration::from_secs(301)).await;
        loop_task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_all_should_finish_two_ok_at_1s_and_timeout_third_at_10s_under_paused_time() {
        let (_, a) = fake(Behavior::Delay(Duration::from_secs(1), vec!["a1"]));
        let (_, b) = fake(Behavior::Delay(Duration::from_secs(1), vec!["b1"]));
        let (_, c) = fake(Behavior::Hang);
        let providers = vec![(name("a"), a), (name("b"), b), (name("c"), c)];
        let catalog = catalog_with(ModelCatalogConfig::default(), &["a", "b", "c"]);
        let start = Instant::now();

        let probe = Arc::clone(&catalog);
        let watcher = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            (
                state_of(&probe, "a"),
                state_of(&probe, "b"),
                state_of(&probe, "c"),
            )
        });
        catalog.refresh_all(&providers).await;

        assert_eq!(
            watcher.await.unwrap(),
            (CatalogState::Ok, CatalogState::Ok, CatalogState::Loading)
        );
        assert_eq!(start.elapsed(), Duration::from_secs(10));
        let snapshot = catalog.snapshot();
        let c = &snapshot.upstreams["c"];
        assert_eq!(c.state, CatalogState::Error);
        assert_eq!(c.error.as_ref().unwrap().kind, CatalogErrorKind::Timeout);
        assert_eq!(snapshot.upstreams["a"].models.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn manual_refresh_should_return_retryAfterSecs_7_when_last_attempt_3s_ago() {
        let (a_calls, a) = fake(Behavior::Models(vec!["x"]));
        let (b_calls, b) = fake(Behavior::Models(vec!["y"]));
        let providers = vec![(name("a"), a), (name("b"), b)];
        let catalog = catalog_with(ModelCatalogConfig::default(), &["a", "b"]);
        finish(
            catalog
                .request_refresh(&providers, Target::One(&name("a")), Trigger::Manual)
                .unwrap(),
        )
        .await;
        tokio::time::advance(Duration::from_secs(3)).await;

        let single = catalog
            .request_refresh(&providers, Target::One(&name("a")), Trigger::Manual)
            .unwrap();
        assert_eq!(single.ack.retry_after_secs, Some(7));
        assert!(single.task.is_none() && !single.ack.refreshing);

        let all = catalog
            .request_refresh(&providers, Target::All, Trigger::Manual)
            .unwrap();
        let ack = finish(all).await;
        assert_eq!(ack.retry_after_secs, Some(7));
        assert_eq!(a_calls.load(Ordering::SeqCst), 1);
        assert_eq!(b_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn prune_should_prevent_in_flight_refresh_resurrecting_removed_upstream() {
        let gate = Arc::new(Notify::new());
        let (_, c) = fake(Behavior::Gated(Arc::clone(&gate), vec!["m"]));
        let providers = vec![(name("c"), c)];
        let catalog = catalog_with(ModelCatalogConfig::default(), &["a", "b", "c"]);
        let started = catalog
            .request_refresh(&providers, Target::One(&name("c")), Trigger::Manual)
            .unwrap();
        tokio::task::yield_now().await;

        catalog.prune(&[name("a"), name("b")]);
        gate.notify_one();
        finish(started).await;

        assert!(!catalog.snapshot().upstreams.contains_key("c"));
    }

    #[tokio::test(start_paused = true)]
    async fn prune_should_remove_c_keep_a_b_and_make_no_calls_on_hot_swap() {
        let (calls, provider) = fake(Behavior::Models(vec!["m"]));
        drop(provider);
        let catalog = catalog_with(ModelCatalogConfig::default(), &["a", "b", "c"]);

        catalog.prune(&[name("a"), name("b")]);

        let keys: Vec<String> = catalog.snapshot().upstreams.keys().cloned().collect();
        assert_eq!(keys, ["a", "b"]);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[derive(Clone, Default)]
    struct LogBuf(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            lock(&self.0).extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
        type Writer = LogBuf;
        fn make_writer(&'a self) -> LogBuf {
            self.clone()
        }
    }

    impl LogBuf {
        fn lines_with(&self, level: &str) -> Vec<String> {
            String::from_utf8_lossy(&lock(&self.0))
                .lines()
                .filter(|l| l.contains(level) && l.contains("model catalog refresh"))
                .map(str::to_string)
                .collect()
        }
    }

    fn capture_logs() -> (LogBuf, tracing::subscriber::DefaultGuard) {
        let buf = LogBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        (buf, tracing::subscriber::set_default(subscriber))
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_should_emit_one_info_log_with_upstream_outcome_ok_model_count_duration_and_no_model_ids(
    ) {
        let (buf, _guard) = capture_logs();
        let (_, provider) = fake(Behavior::Models(vec!["secret-model-id-1", "m2"]));
        let providers = vec![(name("a"), provider)];
        let catalog = catalog_with(ModelCatalogConfig::default(), &["a"]);

        catalog.refresh_all(&providers).await;

        let info = buf.lines_with("INFO");
        assert_eq!(info.len(), 1, "{info:?}");
        for needle in [
            "upstream=a",
            "outcome=\"ok\"",
            "model_count=2",
            "duration_ms=",
        ] {
            assert!(info[0].contains(needle), "{needle} missing in {}", info[0]);
        }
        assert!(!info[0].contains("secret-model-id-1"));
        assert!(buf.lines_with("WARN").is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_failure_should_emit_one_warn_with_upstream_outcome_error_kind_status_duration_and_no_info_line_and_full_error_at_debug(
    ) {
        let (buf, _guard) = capture_logs();
        let (_, provider) = fake(Behavior::Fail(|| ProviderError::Upstream {
            status: 401,
            body: "SENTINEL-body".into(),
        }));
        let providers = vec![(name("a"), provider)];
        let catalog = catalog_with(ModelCatalogConfig::default(), &["a"]);

        catalog.refresh_all(&providers).await;

        let warn = buf.lines_with("WARN");
        assert_eq!(warn.len(), 1, "{warn:?}");
        for needle in [
            "upstream=a",
            "outcome=\"error\"",
            "kind=Auth",
            "status=Some(401)",
            "duration_ms=",
        ] {
            assert!(warn[0].contains(needle), "{needle} missing in {}", warn[0]);
        }
        assert!(!warn[0].contains("SENTINEL"));
        assert!(buf.lines_with("INFO").is_empty());
        let all = String::from_utf8_lossy(&lock(&buf.0)).to_string();
        assert!(all.contains("DEBUG") && all.contains("SENTINEL-body"));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod warning_tests {
    use super::*;
    use crate::config::schema::{Route, RouteUpstreamRef, Strategy, Upstream};

    fn config_with(
        upstreams: &[(&str, UpstreamKind)],
        refs: Vec<(&str, Option<&str>, Option<&str>)>,
    ) -> Config {
        Config {
            upstreams: upstreams
                .iter()
                .map(|(name, kind)| Upstream {
                    name: (*name).to_string(),
                    kind: kind.clone(),
                    auth: None,
                })
                .collect(),
            routes: vec![Route {
                name: "main".to_string(),
                strategy: Strategy::Fallback,
                upstreams: refs
                    .into_iter()
                    .map(|(name, model, family)| RouteUpstreamRef {
                        name: name.to_string(),
                        weight: None,
                        model: model.map(str::to_string),
                        model_family: family.map(str::to_string),
                    })
                    .collect(),
            }],
            ..Config::default()
        }
    }

    fn openai() -> UpstreamKind {
        UpstreamKind::Openai {
            base_url: "https://example.invalid".to_string(),
        }
    }

    fn bedrock() -> UpstreamKind {
        UpstreamKind::Bedrock {
            aws_region: None,
            aws_profile: None,
            max_retries: None,
        }
    }

    fn snapshot_with(entries: Vec<(&str, UpstreamModels)>) -> CatalogSnapshot {
        CatalogSnapshot {
            upstreams: entries
                .into_iter()
                .map(|(n, e)| (n.to_string(), e))
                .collect(),
        }
    }

    fn ok_entry(ids: &[&str]) -> UpstreamModels {
        let mut entry = UpstreamModels::loading();
        let infos = ids
            .iter()
            .map(|id| ModelInfo {
                id: (*id).to_string(),
                owned_by: None,
            })
            .collect();
        entry.apply(RefreshResult::Models(infos), Stamp::now());
        entry
    }

    #[tokio::test]
    async fn config_warnings_should_report_pinned_model_missing_upstream() {
        let config = config_with(
            &[("openai", openai())],
            vec![("openai", Some("gpt-5-turbo"), None)],
        );
        let snapshot = snapshot_with(vec![("openai", ok_entry(&["gpt-5", "gpt-4o"]))]);

        let warnings = config_warnings(&config, &snapshot);

        assert_eq!(
            serde_json::to_value(&warnings).unwrap(),
            serde_json::json!([{
                "upstream": "openai",
                "model": "gpt-5-turbo",
                "referencedBy": "route main upstream 0"
            }])
        );
        let listed = config_with(
            &[("openai", openai())],
            vec![("openai", Some("gpt-5"), None)],
        );
        assert!(config_warnings(&listed, &snapshot).is_empty());
    }

    #[tokio::test]
    async fn config_warnings_should_skip_error_loading_and_family_upstreams() {
        let config = config_with(
            &[
                ("openai", openai()),
                ("anthropic", openai()),
                ("gemini", openai()),
                ("big", openai()),
            ],
            vec![
                ("openai", None, Some("gpt-5")),
                ("anthropic", Some("missing-a"), None),
                ("gemini", Some("missing-g"), None),
                ("big", Some("missing-b"), None),
            ],
        );
        let mut errored = UpstreamModels::loading();
        errored.apply(RefreshResult::Failed(ProviderError::Timeout), Stamp::now());
        let many: Vec<String> = (0..1200).map(|i| format!("m{i}")).collect();
        let many_refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let snapshot = snapshot_with(vec![
            ("openai", ok_entry(&["gpt-5"])),
            ("anthropic", errored),
            ("gemini", UpstreamModels::loading()),
            ("big", ok_entry(&many_refs)),
        ]);

        assert!(config_warnings(&config, &snapshot).is_empty());
    }

    #[tokio::test]
    async fn config_warnings_should_compare_bedrock_ids_after_normalize_model() {
        let config = config_with(
            &[("bedrock", bedrock())],
            vec![(
                "bedrock",
                Some("us.anthropic.claude-3-5-sonnet-20241022-v1:0"),
                None,
            )],
        );
        let snapshot = snapshot_with(vec![(
            "bedrock",
            ok_entry(&["anthropic.claude-3-5-sonnet-20241022-v1:0"]),
        )]);

        assert!(config_warnings(&config, &snapshot).is_empty());
    }
}
