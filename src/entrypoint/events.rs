//! Telemetry event schema and Axum SSE broadcast handler.

use std::collections::HashMap;
use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::entrypoint::EntrypointState;
use crate::metrics::trace::{AttemptErrorClass, AttemptOutcome, AttemptRecord, RoutedModelSource};
use crate::metrics::RequestDetail;

/// Wire contract is camelCase and pinned by `ui/src/testing/fixtures/sse/*.json` (ADR-001).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MetricsTickData {
    pub rpm: f64,
    pub total_requests: u64,
    pub total_tokens_saved: u64,
    pub current_lag_ms: f64,
    pub provider_health: HashMap<String, String>,
}

/// Outcome of the ingress compression step. This project only ever sends `Off`; the
/// other variants are reserved for the deferred compression-wiring follow-up (ADR-005).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompressionStatus {
    #[default]
    Off,
    None,
    Skipped,
    Failed,
    Compressed,
}

/// Which path produced the trace.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TraceKind {
    #[default]
    Routed,
    CacheHit,
    Local,
}

/// State of the first-byte measurement on a streamed row; never sent in this project.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FirstByteState {
    Pending,
    Measured,
    EndedEmpty,
    ClientClosed,
}

/// Wire form of one upstream attempt. `error_message` comes from a fixed phrase per class plus
/// the numeric status, so no upstream text can reach the event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AttemptTrace {
    pub iteration: u8,
    pub upstream: String,
    pub model: Option<String>,
    pub outcome: AttemptOutcome,
    pub status: Option<u16>,
    pub latency_ms: u64,
    pub error_class: Option<AttemptErrorClass>,
    pub error_message: Option<String>,
}

/// Fixed phrase per class and whether the numeric status is appended.
fn error_phrase(class: AttemptErrorClass) -> (&'static str, bool) {
    match class {
        AttemptErrorClass::Auth => ("Upstream rejected credentials", true),
        AttemptErrorClass::RateLimited => ("Upstream rate limit", true),
        AttemptErrorClass::Overloaded => ("Upstream overloaded", true),
        AttemptErrorClass::Timeout => ("Upstream timed out", true),
        AttemptErrorClass::ServerError => ("Upstream server error", true),
        AttemptErrorClass::Unreachable => ("Upstream unreachable", false),
        AttemptErrorClass::ClientError => ("Upstream rejected the request", true),
        AttemptErrorClass::BadResponse => ("Upstream response not understood", true),
        AttemptErrorClass::Shed => ("Skipped by admission control", false),
        AttemptErrorClass::Cancelled => (
            "Client closed the request before the upstream answered",
            false,
        ),
    }
}

impl From<&AttemptRecord> for AttemptTrace {
    fn from(record: &AttemptRecord) -> Self {
        let error_message = record.error_class.map(|class| {
            let (phrase, with_status) = error_phrase(class);
            match record.status {
                Some(status) if with_status => format!("{phrase} (HTTP {status})"),
                _ => phrase.to_string(),
            }
        });
        Self {
            iteration: record.iteration,
            upstream: record.upstream.to_string(),
            model: record.model.clone(),
            outcome: record.outcome,
            status: record.status,
            latency_ms: record.latency_ms,
            error_class: record.error_class,
            error_message,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RequestTraceData {
    /// Equals `request_id`; the UI keys rows on it.
    pub id: String,
    pub request_id: String,
    pub timestamp: String,
    pub provider: String,
    pub model: String,
    pub time_to_headers_ms: u64,
    pub first_byte_ms: Option<u64>,
    pub total_ms: Option<u64>,
    pub tokens_before: u64,
    pub tokens_after: Option<u64>,
    pub compression_status: CompressionStatus,
    pub stream: bool,
    pub trace_kind: TraceKind,
    pub status_code: u16,
    /// 0 = terminal trace; the UI never lets a lower revision overwrite a higher one.
    pub trace_revision: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_byte_state: Option<FirstByteState>,
    /// The raw model the client asked for; `None` when the request named none.
    #[serde(default)]
    pub requested_model: Option<String>,
    #[serde(default)]
    pub routed_model: Option<String>,
    #[serde(default)]
    pub routed_model_source: Option<RoutedModelSource>,
    #[serde(default)]
    pub routed_provider: Option<String>,
    #[serde(default)]
    pub route_reason: Option<String>,
    /// `RoutingStrategy::name()`, set only when `route_reason` is `strategy`.
    #[serde(default)]
    pub route_strategy: Option<String>,
    #[serde(default)]
    pub attempts: Vec<AttemptTrace>,
    #[serde(default)]
    pub attempts_truncated: bool,
    #[serde(default)]
    pub iterations: u8,
}

impl RequestTraceData {
    /// The only constructor: SSE emit, `finish` and the backfill endpoint all build through here.
    /// Measurements this project does not take (first byte, total, tokens after) stay `None`
    /// rather than a misleading zero.
    #[must_use]
    pub fn from_detail(detail: &RequestDetail) -> Self {
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let time_to_headers_ms = detail.duration_ms.max(0.0) as u64;
        Self {
            id: detail.request_id.clone(),
            request_id: detail.request_id.clone(),
            timestamp: detail.timestamp.clone(),
            provider: detail.provider.clone(),
            model: detail.model.clone(),
            time_to_headers_ms,
            first_byte_ms: None,
            total_ms: detail.total_ms,
            tokens_before: detail.tokens_before,
            tokens_after: None,
            compression_status: detail.compression_status,
            stream: detail.stream,
            trace_kind: detail.trace_kind,
            status_code: detail.status_code,
            trace_revision: detail.trace_revision,
            first_byte_state: detail.first_byte_state,
            requested_model: detail.requested_model.clone(),
            routed_model: detail.routed_model.clone(),
            routed_model_source: detail.routed_model_source,
            routed_provider: detail.routed_provider.clone(),
            route_reason: detail.route_reason.clone(),
            route_strategy: detail.route_strategy.clone(),
            attempts: detail.attempts.iter().map(AttemptTrace::from).collect(),
            attempts_truncated: detail.attempts_truncated,
            iterations: detail.iterations,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ErrorLoggedData {
    pub timestamp: String,
    pub provider: String,
    pub model: String,
    pub error_type: String,
    pub status_code: Option<u16>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigChangedData {
    pub timestamp: String,
    pub route_name: String,
    pub strategy: String,
}

/// One upstream's catalog entry changed (a refresh finished); the UI refetches
/// `GET /api/model-catalog`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalogChangedData {
    pub timestamp: String,
    pub upstream: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "data")]
// Short-lived broadcast items; boxing `RequestTrace` would ripple through every consumer for no gain.
#[allow(clippy::large_enum_variant)]
pub enum DashboardEvent {
    MetricsTick(MetricsTickData),
    RequestTrace(RequestTraceData),
    ErrorLogged(ErrorLoggedData),
    ConfigChanged(ConfigChangedData),
    ModelCatalogChanged(ModelCatalogChangedData),
}

/// Legacy snake_case key -> camelCase source key, for the one-release `compat_snake_case` rollback.
const LEGACY_REQUEST_TRACE_KEYS: [(&str, &str); 6] = [
    ("request_id", "requestId"),
    ("duration_ms", "timeToHeadersMs"),
    ("first_byte_ms", "firstByteMs"),
    ("tokens_before", "tokensBefore"),
    ("tokens_after", "tokensAfter"),
    ("status_code", "statusCode"),
];

/// Serializes one event for the SSE frame. With `compat_snake_case`, a `RequestTrace` also
/// carries the pre-camelCase keys so an external consumer keeps working for one release
/// (ADR-001 item 4). Serialization failure yields an empty string, matching the old behavior.
#[must_use]
pub fn event_json(event: &DashboardEvent, compat_snake_case: bool) -> String {
    if !compat_snake_case || !matches!(event, DashboardEvent::RequestTrace(_)) {
        return serde_json::to_string(event).unwrap_or_default();
    }
    let Ok(mut value) = serde_json::to_value(event) else {
        return String::new();
    };
    if let Some(data) = value.get_mut("data").and_then(|d| d.as_object_mut()) {
        for (legacy, current) in LEGACY_REQUEST_TRACE_KEYS {
            if let Some(v) = data.get(current).cloned() {
                data.insert(legacy.to_string(), v);
            }
        }
    }
    value.to_string()
}

/// GET `/v1/dashboard/events` — Server-Sent Events endpoint streaming real-time proxy telemetry.
pub async fn handle_sse_events(
    State(state): State<EntrypointState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.event_tx.subscribe();
    let compat_snake_case = state.compat_snake_case;
    let stream = BroadcastStream::new(rx).map(move |msg| match msg {
        Ok(event) => {
            let json = event_json(&event, compat_snake_case);
            Ok(Event::default().event("message").data(json))
        }
        Err(BroadcastStreamRecvError::Lagged(skipped)) => {
            let payload = serde_json::json!({
                "skipped": skipped
            });
            Ok(Event::default()
                .event("system_lag")
                .data(payload.to_string()))
        }
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn dashboard_event_serialization_matches_tag_content_schema() {
        let event = DashboardEvent::MetricsTick(MetricsTickData {
            rpm: 12.5,
            total_requests: 100,
            total_tokens_saved: 500,
            current_lag_ms: 2.1,
            provider_health: HashMap::from([("anthropic".to_string(), "healthy".to_string())]),
        });

        let json = serde_json::to_string(&event).expect("serialize event");
        assert!(json.contains(r#""type":"MetricsTick""#));
        assert!(json.contains(r#""data":{"#));
        assert!(json.contains(r#""rpm":12.5"#));

        let deserialized: DashboardEvent = serde_json::from_str(&json).expect("deserialize event");
        assert_eq!(event, deserialized);
    }

    const FIXTURES: [(&str, &str); 6] = [
        (
            "request-trace-success-compat.json",
            include_str!("../../ui/src/testing/fixtures/sse/request-trace-success-compat.json"),
        ),
        (
            "request-trace-success.json",
            include_str!("../../ui/src/testing/fixtures/sse/request-trace-success.json"),
        ),
        (
            "request-trace-failure.json",
            include_str!("../../ui/src/testing/fixtures/sse/request-trace-failure.json"),
        ),
        (
            "metrics-tick.json",
            include_str!("../../ui/src/testing/fixtures/sse/metrics-tick.json"),
        ),
        (
            "error-logged.json",
            include_str!("../../ui/src/testing/fixtures/sse/error-logged.json"),
        ),
        (
            "config-changed.json",
            include_str!("../../ui/src/testing/fixtures/sse/config-changed.json"),
        ),
    ];

    #[test]
    fn fixtures_should_pin_each_variant_so_snake_to_camel_drift_fails_both_sides() {
        for (name, raw) in FIXTURES
            .iter()
            .filter(|(n, _)| !n.ends_with("-compat.json"))
        {
            let expected: serde_json::Value = serde_json::from_str(raw).expect(name);
            let event: DashboardEvent = serde_json::from_str(raw)
                .unwrap_or_else(|e| panic!("{name} does not parse into DashboardEvent: {e}"));
            let actual = serde_json::to_value(&event).expect("serialize");
            assert_eq!(actual, expected, "{name} drifted from the Rust contract");
        }
    }

    fn sample_detail() -> RequestDetail {
        RequestDetail {
            request_id: "req_123".to_string(),
            timestamp: "2026-10-10T18:09:53.222465+00:00".to_string(),
            model: "claude-haiku-4-5".to_string(),
            provider: "anthropic".to_string(),
            tokens_before: 1870,
            tokens_after: 0,
            compressed: false,
            stream: false,
            msg_types: "{}".to_string(),
            has_context_management: false,
            message_count: 1,
            duration_ms: 42.4,
            first_byte_ms: 42.4,
            bedrock_invocation_ms: 0,
            bedrock_first_byte_ms: 0,
            session_id: None,
            selected_model: None,
            selected_model_was_exploration: None,
            status_code: 200,
            routed_model: Some("claude-haiku-4-5".to_string()),
            routed_model_source: Some(crate::metrics::trace::RoutedModelSource::Passthrough),
            routed_provider: Some("anthropic".to_string()),
            route_reason: Some("default".to_string()),
            attempts: vec![AttemptRecord {
                iteration: 0,
                upstream: "anthropic".into(),
                model: None,
                outcome: AttemptOutcome::Ok,
                error_class: None,
                status: Some(200),
                latency_ms: 40,
            }],
            iterations: 1,
            ..RequestDetail::from_body(
                String::new(),
                false,
                0,
                &serde_json::json!({"model": "claude-haiku-4-5"}),
                None,
            )
        }
    }

    fn fixture_value(name: &str) -> serde_json::Value {
        let raw = FIXTURES
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, raw)| *raw)
            .expect("fixture registered");
        serde_json::from_str(raw).expect("fixture json")
    }

    #[test]
    fn request_trace_should_serialize_camel_case_equal_to_fixture_when_success() {
        let data = RequestTraceData::from_detail(&sample_detail());
        let json = serde_json::to_value(DashboardEvent::RequestTrace(data)).expect("serialize");
        assert_eq!(json, fixture_value("request-trace-success.json"));
    }

    #[test]
    fn request_trace_should_emit_null_not_zero_when_first_byte_and_tokens_after_absent() {
        let json = serde_json::to_value(RequestTraceData::from_detail(&sample_detail()))
            .expect("serialize");
        assert!(json["firstByteMs"].is_null());
        assert!(json["tokensAfter"].is_null());
        assert!(json["totalMs"].is_null());
        assert!(json.get("firstByteState").is_none(), "never sent here");
        assert_eq!(json["timeToHeadersMs"], 42);
        assert_eq!(json["id"], json["requestId"]);
    }

    #[test]
    fn model_catalog_changed_should_serialize_camel_case_with_type_envelope() {
        let event = DashboardEvent::ModelCatalogChanged(ModelCatalogChangedData {
            timestamp: "2026-10-10T18:10:03.000000+00:00".to_string(),
            upstream: "bedrock".to_string(),
        });

        let json: serde_json::Value = serde_json::from_str(&event_json(&event, false)).unwrap();

        assert_eq!(
            json,
            serde_json::json!({
                "type": "ModelCatalogChanged",
                "data": {"timestamp": "2026-10-10T18:10:03.000000+00:00", "upstream": "bedrock"}
            })
        );
    }

    #[test]
    fn every_dashboard_event_variant_should_serialize_non_empty_json_with_type_tag() {
        let events = [
            DashboardEvent::MetricsTick(MetricsTickData {
                rpm: 0.0,
                total_requests: 0,
                total_tokens_saved: 0,
                current_lag_ms: 0.0,
                provider_health: HashMap::new(),
            }),
            DashboardEvent::RequestTrace(RequestTraceData::from_detail(&sample_detail())),
            DashboardEvent::ErrorLogged(ErrorLoggedData {
                timestamp: String::new(),
                provider: String::new(),
                model: String::new(),
                error_type: String::new(),
                status_code: None,
                message: String::new(),
            }),
            DashboardEvent::ConfigChanged(ConfigChangedData {
                timestamp: String::new(),
                route_name: String::new(),
                strategy: String::new(),
            }),
            DashboardEvent::ModelCatalogChanged(ModelCatalogChangedData {
                timestamp: String::new(),
                upstream: String::new(),
            }),
        ];
        for event in &events {
            let json = serde_json::to_string(event).expect("serialize");
            assert!(!json.is_empty());
            assert!(json.contains(r#""type""#), "{json}");
        }
    }

    #[test]
    fn request_trace_should_emit_legacy_snake_case_keys_alongside_camel_case_only_when_compat_snake_case_true(
    ) {
        let event = DashboardEvent::RequestTrace(RequestTraceData::from_detail(&sample_detail()));

        let compat: serde_json::Value =
            serde_json::from_str(&event_json(&event, true)).expect("compat json");
        let default: serde_json::Value =
            serde_json::from_str(&event_json(&event, false)).expect("default json");

        assert_eq!(compat, fixture_value("request-trace-success-compat.json"));
        assert_eq!(default, fixture_value("request-trace-success.json"));
    }

    #[test]
    fn compat_snake_case_should_leave_other_event_types_untouched() {
        let event: DashboardEvent =
            serde_json::from_value(fixture_value("metrics-tick.json")).expect("parse");
        let json: serde_json::Value =
            serde_json::from_str(&event_json(&event, true)).expect("json");
        assert_eq!(json, fixture_value("metrics-tick.json"));
    }

    #[test]
    fn attempt_trace_should_serialize_outcome_status_error_class_and_fixed_phrase_message_with_nulls_for_ok_attempt(
    ) {
        use crate::metrics::trace::{AttemptErrorClass as C, AttemptOutcome as O, AttemptRecord};
        let record = |outcome, error_class, status, latency_ms| AttemptRecord {
            iteration: 0,
            upstream: "anthropic".into(),
            model: Some("claude-haiku-4-5".to_string()),
            outcome,
            error_class,
            status,
            latency_ms,
        };
        let failed = serde_json::to_value(AttemptTrace::from(&record(
            O::Upstream,
            Some(C::Overloaded),
            Some(529),
            120,
        )))
        .expect("serialize");
        assert_eq!(
            failed,
            serde_json::json!({
                "iteration": 0,
                "upstream": "anthropic",
                "model": "claude-haiku-4-5",
                "outcome": "upstream",
                "status": 529,
                "latencyMs": 120,
                "errorClass": "overloaded",
                "errorMessage": "Upstream overloaded (HTTP 529)",
            })
        );
        let ok = serde_json::to_value(AttemptTrace::from(&record(O::Ok, None, Some(200), 80)))
            .expect("serialize");
        assert!(ok["errorClass"].is_null() && ok["errorMessage"].is_null());
        let shed =
            serde_json::to_value(AttemptTrace::from(&record(O::Shed, Some(C::Shed), None, 0)))
                .expect("serialize");
        assert_eq!(shed["errorMessage"], "Skipped by admission control");
        let unreachable = serde_json::to_value(AttemptTrace::from(&record(
            O::Upstream,
            Some(C::Unreachable),
            Some(502),
            5,
        )))
        .expect("serialize");
        assert_eq!(unreachable["errorMessage"], "Upstream unreachable");
    }

    /// Walks `src/` and fails if any non-test code outside this file builds the struct literally.
    #[test]
    fn request_trace_data_from_detail_should_be_only_constructor_in_non_test_code() {
        fn visit(dir: &std::path::Path, hits: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("read_dir").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    visit(&path, hits);
                } else if path.extension().is_some_and(|e| e == "rs")
                    && !path.ends_with("entrypoint/events.rs")
                {
                    let text = std::fs::read_to_string(&path).expect("read");
                    let non_test = text.split("#[cfg(test)]").next().unwrap_or_default();
                    if non_test.contains("RequestTraceData {") {
                        hits.push(path.display().to_string());
                    }
                }
            }
        }
        let mut hits = Vec::new();
        visit(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut hits,
        );
        assert!(
            hits.is_empty(),
            "literal RequestTraceData outside events.rs: {hits:?}"
        );
    }
}
