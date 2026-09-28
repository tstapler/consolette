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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MetricsTickData {
    pub rpm: f64,
    pub total_requests: u64,
    pub total_tokens_saved: u64,
    pub current_lag_ms: f64,
    pub provider_health: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RequestTraceData {
    pub request_id: String,
    pub timestamp: String,
    pub provider: String,
    pub model: String,
    pub duration_ms: u64,
    pub first_byte_ms: u64,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub compressed: bool,
    pub status_code: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ErrorLoggedData {
    pub timestamp: String,
    pub provider: String,
    pub model: String,
    pub error_type: String,
    pub status_code: Option<u16>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfigChangedData {
    pub timestamp: String,
    pub route_name: String,
    pub strategy: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "data")]
pub enum DashboardEvent {
    MetricsTick(MetricsTickData),
    RequestTrace(RequestTraceData),
    ErrorLogged(ErrorLoggedData),
    ConfigChanged(ConfigChangedData),
}

/// GET `/v1/dashboard/events` — Server-Sent Events endpoint streaming real-time proxy telemetry.
pub async fn handle_sse_events(
    State(state): State<EntrypointState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.event_tx.subscribe();
    let stream = BroadcastStream::new(rx).map(|msg| match msg {
        Ok(event) => {
            let json = serde_json::to_string(&event).unwrap_or_default();
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

    #[test]
    fn request_trace_event_serialization() {
        let trace = DashboardEvent::RequestTrace(RequestTraceData {
            request_id: "req_123".to_string(),
            timestamp: "2026-09-27T14:00:00Z".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-3-5-sonnet".to_string(),
            duration_ms: 250,
            first_byte_ms: 100,
            tokens_before: 1000,
            tokens_after: 800,
            compressed: true,
            status_code: 200,
        });

        let json = serde_json::to_string(&trace).expect("serialize trace");
        assert!(json.contains(r#""type":"RequestTrace""#));
        assert!(json.contains(r#""request_id":"req_123""#));
    }
}
