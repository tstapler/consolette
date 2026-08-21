//! Translates a raw Anthropic SSE byte stream into `OpenAI`
//! `chat.completion.chunk` SSE frames (plan.md Phase 3, Epic 3.2).

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures_core::Stream;
use serde_json::json;

/// Wraps a raw Anthropic SSE byte stream (as produced by
/// `Router::dispatch`/`CostTrackingStream`) and translates it live into
/// OpenAI-shaped `chat.completion.chunk` SSE frames.
pub struct OpenAiStreamTranslator<S> {
    inner: eventsource_stream::EventStream<S>,
    id: String,
    model: String,
    /// Set once the `[DONE]` sentinel has been emitted — suppresses further
    /// polling of `inner`.
    done: bool,
    /// Set once a terminal event (`message_stop` or the synthesized
    /// `error` frame) has been observed; the *next* poll emits `[DONE]`
    /// and then `done` is set.
    finished: bool,
}

impl<S> OpenAiStreamTranslator<S>
where
    S: Stream<Item = Result<Bytes, anyhow::Error>>,
{
    pub fn new(inner: S, model: String) -> Self {
        Self {
            inner: inner.eventsource(),
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4()),
            model,
            done: false,
            finished: false,
        }
    }

    fn finish_reason_chunk(&self, finish_reason: &str) -> Bytes {
        let chunk = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "model": self.model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}]
        });
        Bytes::from(format!("data: {chunk}\n\n"))
    }

    fn content_delta_chunk(&self, text: &str) -> Bytes {
        let chunk = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "model": self.model,
            "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
        });
        Bytes::from(format!("data: {chunk}\n\n"))
    }
}

const DONE_SENTINEL: &str = "data: [DONE]\n\n";

impl<S> Stream for OpenAiStreamTranslator<S>
where
    S: Stream<Item = Result<Bytes, anyhow::Error>> + Unpin,
{
    type Item = Result<Bytes, anyhow::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        if this.done {
            return Poll::Ready(None);
        }
        if this.finished {
            this.done = true;
            return Poll::Ready(Some(Ok(Bytes::from_static(DONE_SENTINEL.as_bytes()))));
        }

        loop {
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    // Inner ended without a terminal frame we recognized —
                    // still emit [DONE] so clients see a well-formed stream.
                    this.finished = true;
                    this.done = true;
                    return Poll::Ready(Some(Ok(Bytes::from_static(DONE_SENTINEL.as_bytes()))));
                }
                Poll::Ready(Some(Err(e))) => {
                    tracing::warn!(error = %e, "openai stream translator: eventsource parse error");
                    this.finished = true;
                    this.done = true;
                    return Poll::Ready(Some(Ok(Bytes::from_static(DONE_SENTINEL.as_bytes()))));
                }
                Poll::Ready(Some(Ok(event))) => match event.event.as_str() {
                    "content_block_delta" => {
                        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&event.data) {
                            if let Some(text) = parsed
                                .get("delta")
                                .and_then(|d| d.get("text"))
                                .and_then(|t| t.as_str())
                            {
                                return Poll::Ready(Some(Ok(this.content_delta_chunk(text))));
                            }
                        }
                        // Non-text delta (e.g. a tool-use partial-json
                        // delta) — nothing translatable, keep looping.
                    }
                    "message_stop" | "error" => {
                        this.finished = true;
                        return Poll::Ready(Some(Ok(this.finish_reason_chunk("stop"))));
                    }
                    // "ping", "message_start", "content_block_start",
                    // "content_block_stop", "message_delta" (without a
                    // stop reason) — silently consumed.
                    _ => {}
                },
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use futures_util::{stream, StreamExt};

    async fn drain<S>(s: S) -> Vec<Bytes>
    where
        S: Stream<Item = Result<Bytes, anyhow::Error>>,
    {
        s.map(|item| item.unwrap()).collect().await
    }

    fn frame(event: &str, data: &str) -> Bytes {
        Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
    }

    #[tokio::test]
    async fn content_block_delta_produces_expected_chunk() {
        let inner = stream::iter(vec![Ok(frame(
            "content_block_delta",
            r#"{"delta":{"type":"text_delta","text":"Hi"}}"#,
        ))]);
        let translator = OpenAiStreamTranslator::new(inner, "claude-sonnet-4-5".to_string());
        let out = drain(translator).await;
        // One content chunk, then [DONE] (inner ended without message_stop).
        assert_eq!(out.len(), 2);
        let chunk: serde_json::Value = serde_json::from_slice(
            out[0]
                .strip_prefix(b"data: ")
                .unwrap()
                .strip_suffix(b"\n\n")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(chunk["choices"][0]["delta"]["content"], "Hi");
        assert_eq!(out[1], Bytes::from_static(DONE_SENTINEL.as_bytes()));
    }

    #[tokio::test]
    async fn message_stop_produces_finish_reason_chunk_then_done() {
        let inner = stream::iter(vec![Ok(frame("message_stop", "{}"))]);
        let translator = OpenAiStreamTranslator::new(inner, "claude-sonnet-4-5".to_string());
        let out = drain(translator).await;
        assert_eq!(out.len(), 2);
        let chunk: serde_json::Value = serde_json::from_slice(
            out[0]
                .strip_prefix(b"data: ")
                .unwrap()
                .strip_suffix(b"\n\n")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(chunk["choices"][0]["finish_reason"], "stop");
        assert_eq!(out[1], Bytes::from_static(DONE_SENTINEL.as_bytes()));
    }

    #[tokio::test]
    async fn ping_frame_produces_no_output_of_its_own() {
        let inner = stream::iter(vec![
            Ok(frame("ping", "{}")),
            Ok(frame("message_stop", "{}")),
        ]);
        let translator = OpenAiStreamTranslator::new(inner, "claude-sonnet-4-5".to_string());
        let out = drain(translator).await;
        // Only the message_stop finish-reason chunk + [DONE] — the ping
        // contributed nothing.
        assert_eq!(out.len(), 2);
    }

    #[tokio::test]
    async fn full_transcript_produces_ordered_chunks_ending_in_done() {
        let inner = stream::iter(vec![
            Ok(frame("message_start", "{}")),
            Ok(frame(
                "content_block_delta",
                r#"{"delta":{"type":"text_delta","text":"Hel"}}"#,
            )),
            Ok(frame(
                "content_block_delta",
                r#"{"delta":{"type":"text_delta","text":"lo"}}"#,
            )),
            Ok(frame("message_stop", "{}")),
        ]);
        let translator = OpenAiStreamTranslator::new(inner, "claude-sonnet-4-5".to_string());
        let out = drain(translator).await;
        assert_eq!(out.len(), 4); // "Hel", "lo", finish_reason, [DONE]
        assert_eq!(out[3], Bytes::from_static(DONE_SENTINEL.as_bytes()));
    }

    #[tokio::test]
    async fn synthesized_error_frame_produces_finish_reason_and_done() {
        let inner = stream::iter(vec![
            Ok(frame(
                "content_block_delta",
                r#"{"delta":{"type":"text_delta","text":"Hi"}}"#,
            )),
            Ok(frame(
                "error",
                r#"{"type":"error","error":{"type":"api_error","message":"upstream stream interrupted"}}"#,
            )),
        ]);
        let translator = OpenAiStreamTranslator::new(inner, "claude-sonnet-4-5".to_string());
        let out = drain(translator).await;
        assert_eq!(out.len(), 3);
        let chunk: serde_json::Value = serde_json::from_slice(
            out[1]
                .strip_prefix(b"data: ")
                .unwrap()
                .strip_suffix(b"\n\n")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(chunk["choices"][0]["finish_reason"], "stop");
        assert_eq!(out[2], Bytes::from_static(DONE_SENTINEL.as_bytes()));
    }
}
