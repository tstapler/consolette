//! Cost-tracking helpers shared by every entrypoint handler (ADR-016).
//!
//! `begin_cost_tracking` is the one call site that establishes the
//! `record_pending`-before-`dispatch` sequencing every handler needs.
//! `CostTrackingStream` is the streaming-path tee (ADR-016/ADR-018): it
//! passes provider SSE bytes through unchanged while scanning for
//! `message_delta`/`message_stop` frames to record real usage, and
//! synthesizes an in-band `event: error` frame on a mid-stream cut instead
//! of ever calling `Router::dispatch` a second time.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_core::Stream;

use crate::cost_metrics::tracker::CostTracker;
use crate::cost_metrics::types::{EstimatorKind, RequestId, TokenCount, TokenSource};
use crate::session_compaction::{CompactionTier, SessionKey};

/// Records a `Pending` cost row *before* dispatch is called, per ADR-016.
/// Every handler (Anthropic-native and OpenAI-compat, streaming and not)
/// calls this first so the ordering is structural rather than
/// per-call-site discipline.
pub async fn begin_cost_tracking(tracker: &CostTracker) -> (SessionKey, RequestId) {
    let session_key = SessionKey::new(format!("http:{}", uuid::Uuid::new_v4()));
    let request_id = RequestId::new();
    tracker
        .record_pending(&session_key, request_id, CompactionTier::Off)
        .await;
    (session_key, request_id)
}

/// The synthesized in-band SSE error frame emitted on a mid-stream cut
/// (ADR-018) — never propagated as a `Stream::Err`, since that would abort
/// the HTTP response body instead of ending it cleanly.
const MID_STREAM_ERROR_FRAME: &str = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"upstream stream interrupted\"}}\n\n";

/// Wraps a raw provider SSE byte stream, passing bytes through unchanged
/// while scanning for `message_delta`/`message_stop` frames to record real
/// usage against `(session_key, request_id)`. On a mid-stream `Err`, emits
/// the synthesized error frame as the final item instead of propagating
/// the error (ADR-018) — no second `dispatch` call is ever made.
pub struct CostTrackingStream<S> {
    inner: S,
    tracker: Arc<CostTracker>,
    session_key: SessionKey,
    request_id: RequestId,
    #[allow(dead_code)] // reserved for future model-aware cost recording
    model: String,
    buf: Vec<u8>,
    last_usage: Option<crate::providers::AnthropicUsage>,
    /// Set once a terminal frame (`message_stop`, a mid-stream error frame,
    /// or an unattributed end-of-stream) has been handled — suppresses
    /// further polling of `inner` and further cost recording.
    done: bool,
}

impl<S> CostTrackingStream<S> {
    pub fn new(
        inner: S,
        tracker: Arc<CostTracker>,
        session_key: SessionKey,
        request_id: RequestId,
        model: String,
    ) -> Self {
        Self {
            inner,
            tracker,
            session_key,
            request_id,
            model,
            buf: Vec::new(),
            last_usage: None,
            done: false,
        }
    }

    /// Scans `buf` for complete `\n\n`-terminated SSE frames, draining each
    /// one as it's found. Returns `true` if a `message_stop` frame was
    /// observed (the caller should finalize accounting and stop polling
    /// `inner` further).
    fn scan_buf_for_frames(&mut self) -> bool {
        loop {
            let Some(pos) = find_double_newline(&self.buf) else {
                return false;
            };
            let frame: Vec<u8> = self.buf.drain(..pos + 2).collect();
            let frame_str = String::from_utf8_lossy(&frame);

            let mut event_name: Option<&str> = None;
            let mut data_line: Option<&str> = None;
            for line in frame_str.lines() {
                if let Some(rest) = line.strip_prefix("event:") {
                    event_name = Some(rest.trim());
                } else if let Some(rest) = line.strip_prefix("data:") {
                    data_line = Some(rest.trim());
                }
            }

            match event_name {
                Some("message_delta") => {
                    if let Some(data) = data_line {
                        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(data) {
                            if let Some(usage) = crate::providers::extract_usage(&parsed) {
                                self.last_usage = Some(usage);
                            }
                        }
                    }
                }
                Some("message_stop") => {
                    return true;
                }
                _ => {}
            }
        }
    }

    /// Records final usage (exact, from an observed `message_stop`) —
    /// called once, when the stream reaches its normal terminal frame.
    fn finalize_success(&self) {
        let tracker = Arc::clone(&self.tracker);
        let session_key = self.session_key.clone();
        let request_id = self.request_id;
        let last_usage = self.last_usage;
        tokio::spawn(async move {
            if let Some(usage) = last_usage {
                let actual = TokenCount {
                    value: usage.input_tokens + usage.output_tokens,
                    source: TokenSource::Exact,
                };
                let _ = tracker
                    .record_actual_usage(&session_key, request_id, actual)
                    .await;
            } else {
                tracker
                    .record_request_failed(&session_key, request_id)
                    .await;
            }
        });
    }

    /// Records usage on a mid-stream cut: `Estimated` if partial usage was
    /// observed before the cut, `record_request_failed` otherwise.
    ///
    /// `EstimatorKind` has no variant for "partial usage read from an
    /// interrupted SSE stream" (its three variants are all real
    /// tokenizers/APIs) — this reuses `AnthropicCountTokensApi` as the
    /// closest existing tag since the numbers themselves did come from a
    /// real Anthropic `usage.*` field, just an incomplete read of it. This
    /// is a documented tradeoff, not a claim that `EstimatorKind` was
    /// extended.
    fn finalize_mid_stream_cut(&self) {
        let tracker = Arc::clone(&self.tracker);
        let session_key = self.session_key.clone();
        let request_id = self.request_id;
        let last_usage = self.last_usage;
        tokio::spawn(async move {
            if let Some(usage) = last_usage {
                let actual = TokenCount {
                    value: usage.input_tokens + usage.output_tokens,
                    source: TokenSource::Estimated {
                        via: EstimatorKind::AnthropicCountTokensApi,
                    },
                };
                let _ = tracker
                    .record_actual_usage(&session_key, request_id, actual)
                    .await;
            } else {
                tracker
                    .record_request_failed(&session_key, request_id)
                    .await;
            }
        });
    }
}

fn find_double_newline(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

impl<S> Stream for CostTrackingStream<S>
where
    S: Stream<Item = Result<Bytes, anyhow::Error>> + Unpin,
{
    type Item = Result<Bytes, anyhow::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }

        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                // Mid-stream cut: inner ended without a message_stop frame.
                this.done = true;
                this.finalize_mid_stream_cut();
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(e))) => {
                tracing::warn!(error = %e, "upstream stream interrupted mid-response");
                this.done = true;
                this.finalize_mid_stream_cut();
                Poll::Ready(Some(Ok(Bytes::from_static(
                    MID_STREAM_ERROR_FRAME.as_bytes(),
                ))))
            }
            Poll::Ready(Some(Ok(bytes))) => {
                this.buf.extend_from_slice(&bytes);
                if this.scan_buf_for_frames() {
                    this.done = true;
                    this.finalize_success();
                }
                Poll::Ready(Some(Ok(bytes)))
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use futures_util::stream;

    async fn build_tracker_and_session() -> (Arc<CostTracker>, SessionKey, RequestId) {
        let tracker = Arc::new(
            CostTracker::new(crate::cost_metrics::pricing::PricingTable::load_default()).await,
        );
        let (session_key, request_id) = begin_cost_tracking(&tracker).await;
        (tracker, session_key, request_id)
    }

    async fn drain<S>(s: S) -> Vec<Bytes>
    where
        S: Stream<Item = Result<Bytes, anyhow::Error>>,
    {
        use futures_util::StreamExt;
        s.map(|item| item.unwrap()).collect().await
    }

    #[tokio::test]
    async fn full_stream_with_message_stop_records_exact_usage() {
        let (tracker, session_key, request_id) = build_tracker_and_session().await;

        let frame1 = Bytes::from_static(b"event: message_start\ndata: {}\n\n");
        let frame2 = Bytes::from_static(
            b"event: message_delta\ndata: {\"usage\":{\"input_tokens\":10,\"output_tokens\":32}}\n\n",
        );
        let frame3 = Bytes::from_static(b"event: message_stop\ndata: {}\n\n");
        let inner = stream::iter(vec![
            Ok(frame1.clone()),
            Ok(frame2.clone()),
            Ok(frame3.clone()),
        ]);

        let tee = CostTrackingStream::new(
            inner,
            Arc::clone(&tracker),
            session_key.clone(),
            request_id,
            "claude-sonnet-4-5".to_string(),
        );
        let out = drain(tee).await;
        assert_eq!(out, vec![frame1, frame2, frame3]);

        // finalize_success spawns a task; give it a moment to run.
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let report = tracker.report_for_session(&session_key).await.unwrap();
        assert_eq!(report.actual_tokens, Some(42));
    }

    #[tokio::test]
    async fn stream_ending_after_message_start_only_records_failure() {
        let (tracker, session_key, request_id) = build_tracker_and_session().await;

        let frame1 = Bytes::from_static(b"event: message_start\ndata: {}\n\n");
        let inner = stream::iter(vec![Ok(frame1.clone())]);

        let tee = CostTrackingStream::new(
            inner,
            Arc::clone(&tracker),
            session_key.clone(),
            request_id,
            "claude-sonnet-4-5".to_string(),
        );
        let out = drain(tee).await;
        assert_eq!(out, vec![frame1]);

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Session exists (record_pending happened), but no usage was ever
        // recorded — report_for_session must still succeed.
        assert!(tracker.report_for_session(&session_key).await.is_ok());
    }

    #[tokio::test]
    async fn stream_ending_after_message_delta_before_stop_records_estimated_usage() {
        let (tracker, session_key, request_id) = build_tracker_and_session().await;

        let frame1 = Bytes::from_static(b"event: message_start\ndata: {}\n\n");
        let frame2 = Bytes::from_static(
            b"event: message_delta\ndata: {\"usage\":{\"input_tokens\":5,\"output_tokens\":7}}\n\n",
        );
        let inner = stream::iter(vec![Ok(frame1.clone()), Ok(frame2.clone())]);

        let tee = CostTrackingStream::new(
            inner,
            Arc::clone(&tracker),
            session_key.clone(),
            request_id,
            "claude-sonnet-4-5".to_string(),
        );
        let out = drain(tee).await;
        assert_eq!(out, vec![frame1, frame2]);

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let report = tracker.report_for_session(&session_key).await.unwrap();
        assert_eq!(report.actual_tokens, Some(12));
    }

    #[tokio::test]
    async fn mid_stream_error_emits_synthesized_frame_and_ends() {
        let (tracker, session_key, request_id) = build_tracker_and_session().await;

        let frame1 = Bytes::from_static(b"event: message_start\ndata: {}\n\n");
        let inner: Pin<Box<dyn Stream<Item = Result<Bytes, anyhow::Error>> + Send>> =
            Box::pin(stream::iter(vec![
                Ok(frame1.clone()),
                Err(anyhow::anyhow!("connection reset")),
            ]));

        let tee = CostTrackingStream::new(
            inner,
            Arc::clone(&tracker),
            session_key.clone(),
            request_id,
            "claude-sonnet-4-5".to_string(),
        );
        let out = drain(tee).await;
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], frame1);
        assert_eq!(
            out[1],
            Bytes::from_static(MID_STREAM_ERROR_FRAME.as_bytes())
        );
    }
}
