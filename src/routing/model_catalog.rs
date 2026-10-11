//! Cached, typed, sanitized per-upstream model lists (Epic 3 of
//! `webui-trace-model-mapping`).
//!
//! A refresh never loses the last good list: a failure turns `Ok` into
//! `Stale`, and only a never-loaded upstream becomes `Error`. Error text that
//! reaches the browser is a kind, a status and a fixed phrase; upstream bodies
//! (which can echo keys or account ARNs) are logged at `debug` only.

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::time::Instant;

use crate::providers::{ModelInfo, ProviderError};

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
        ProviderError::Validation(_, status) => (
            CatalogErrorKind::Upstream,
            Some(*status),
            format!("Upstream rejected the request ({status})"),
        ),
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
