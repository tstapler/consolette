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
    /// Anthropic block index → `OpenAI` `tool_calls[].index` (position in
    /// `tools`, assigned in order of first appearance). Needed because
    /// Anthropic block indices share numbering with text blocks while
    /// `OpenAI` tool indices are dense over tool calls only.
    tool_index: std::collections::HashMap<u64, usize>,
    tools: Vec<ToolCall>,
    /// Last `stop_reason` seen on a `message_delta` frame, if any.
    stop_reason: Option<String>,
}

/// One in-progress tool call: id/name captured at `content_block_start`,
/// arguments accumulated from `input_json_delta` fragments (fragments are
/// forwarded verbatim so split JSON reassembles client-side). The fields
/// document what was opened; only the count drives the terminal reason.
#[allow(dead_code)]
struct ToolCall {
    id: String,
    name: String,
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
            tool_index: std::collections::HashMap::new(),
            tools: Vec::new(),
            stop_reason: None,
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

    /// First chunk for one tool call: carries `id` + `name` with empty
    /// arguments so the client can open the slot; fragments follow via
    /// [`Self::tool_args_chunk`].
    fn tool_start_chunk(&self, oi_index: usize, id: &str, name: &str) -> Bytes {
        let chunk = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "model": self.model,
            "choices": [{"index": 0, "delta": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "index": oi_index,
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": ""}
                }]
            }, "finish_reason": null}]
        });
        Bytes::from(format!("data: {chunk}\n\n"))
    }

    /// One `input_json_delta` fragment for an already-opened tool call.
    fn tool_args_chunk(&self, oi_index: usize, fragment: &str) -> Bytes {
        let chunk = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "model": self.model,
            "choices": [{"index": 0, "delta": {
                "tool_calls": [{
                    "index": oi_index,
                    "function": {"arguments": fragment}
                }]
            }, "finish_reason": null}]
        });
        Bytes::from(format!("data: {chunk}\n\n"))
    }

    /// Map the Anthropic `stop_reason` to `OpenAI` `finish_reason` vocabulary.
    /// A stream that opened any tool call always closes as `tool_calls`
    /// (mirroring the non-streaming rule: blocks present ⇔ matching stop).
    fn terminal_finish_reason(&self) -> &'static str {
        if self.tools.is_empty() {
            match self.stop_reason.as_deref() {
                Some("max_tokens") => "length",
                Some("tool_use") => "tool_calls",
                _ => "stop",
            }
        } else {
            "tool_calls"
        }
    }

    /// Translate one Anthropic SSE event into zero or one `OpenAI` chunk.
    /// `Terminal` carries the closing finish-reason chunk (the caller marks
    /// the stream finished); `Continue` means keep polling the inner stream.
    fn handle_event(&mut self, name: &str, data: &str) -> StreamAction {
        match name {
            "content_block_start" => {
                // Open a tool slot: `content_block` carries
                // `{type:"tool_use",id,name}`. Text blocks need no
                // preamble (deltas open the content implicitly).
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(data) {
                    let block = parsed.get("content_block");
                    let is_tool = block.and_then(|b| b.get("type")).and_then(|t| t.as_str())
                        == Some("tool_use");
                    if is_tool {
                        let index = parsed
                            .get("index")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0);
                        let id = block
                            .and_then(|b| b.get("id"))
                            .and_then(|i| i.as_str())
                            .unwrap_or("call_unknown");
                        let name = block
                            .and_then(|b| b.get("name"))
                            .and_then(|n| n.as_str())
                            .unwrap_or("");
                        if !name.is_empty() {
                            let oi_index = self.tools.len();
                            self.tool_index.insert(index, oi_index);
                            self.tools.push(ToolCall {
                                id: id.to_string(),
                                name: name.to_string(),
                            });
                            return StreamAction::Emit(self.tool_start_chunk(oi_index, id, name));
                        }
                    }
                }
                StreamAction::Continue
            }
            "content_block_delta" => {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(data) {
                    let delta = parsed.get("delta");
                    // Tool argument fragment for a known block.
                    if delta.and_then(|d| d.get("type")).and_then(|t| t.as_str())
                        == Some("input_json_delta")
                    {
                        let index = parsed
                            .get("index")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0);
                        if let Some(oi_index) = self.tool_index.get(&index).copied() {
                            let fragment = delta
                                .and_then(|d| d.get("partial_json"))
                                .and_then(|p| p.as_str())
                                .unwrap_or("");
                            if !fragment.is_empty() {
                                return StreamAction::Emit(
                                    self.tool_args_chunk(oi_index, fragment),
                                );
                            }
                        }
                    } else if let Some(text) =
                        delta.and_then(|d| d.get("text")).and_then(|t| t.as_str())
                    {
                        return StreamAction::Emit(self.content_delta_chunk(text));
                    }
                }
                // Unknown delta shape — nothing translatable, keep looping.
                StreamAction::Continue
            }
            "message_delta" => {
                // Capture the stop reason for the terminal chunk;
                // otherwise silently consumed.
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(data) {
                    if let Some(reason) = parsed
                        .get("delta")
                        .and_then(|d| d.get("stop_reason"))
                        .and_then(|s| s.as_str())
                    {
                        self.stop_reason = Some(reason.to_string());
                    }
                }
                StreamAction::Continue
            }
            "message_stop" | "error" => {
                let reason = self.terminal_finish_reason();
                StreamAction::Terminal(self.finish_reason_chunk(reason))
            }
            // "ping", "message_start", "content_block_stop" —
            // silently consumed.
            _ => StreamAction::Continue,
        }
    }
}

/// What [`OpenAiStreamTranslator::handle_event`] decided for one inbound event.
enum StreamAction {
    /// Emit this chunk immediately.
    Emit(Bytes),
    /// Emit this closing chunk and finish the stream after it.
    Terminal(Bytes),
    /// Nothing translatable; keep polling.
    Continue,
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
                Poll::Ready(Some(Ok(event))) => {
                    match this.handle_event(&event.event, &event.data) {
                        StreamAction::Emit(chunk) => return Poll::Ready(Some(Ok(chunk))),
                        StreamAction::Terminal(chunk) => {
                            this.finished = true;
                            return Poll::Ready(Some(Ok(chunk)));
                        }
                        StreamAction::Continue => {}
                    }
                }
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

    #[tokio::test]
    async fn tool_use_blocks_stream_as_tool_calls_with_tool_calls_finish() {
        // Anthropic tool streaming must surface as OpenAI `tool_calls`
        // deltas closing with `finish_reason: "tool_calls"` — without this
        // the client never executes tools and the loop stalls after one turn.
        let inner = stream::iter(vec![
            Ok(frame(
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read","input":{}}}"#,
            )),
            Ok(frame(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            )),
            Ok(frame(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"a\"}"}}"#,
            )),
            Ok(frame(
                "content_block_stop",
                r#"{"type":"content_block_stop","index":1}"#,
            )),
            Ok(frame(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}"#,
            )),
            Ok(frame("message_stop", "{}")),
        ]);
        let translator = OpenAiStreamTranslator::new(inner, "claude-sonnet-4-5".to_string());
        let out = drain(translator).await;

        let chunks: Vec<serde_json::Value> = out[..out.len() - 1]
            .iter()
            .map(|f| {
                serde_json::from_slice(
                    f.strip_prefix(b"data: ")
                        .unwrap()
                        .strip_suffix(b"\n\n")
                        .unwrap(),
                )
                .unwrap()
            })
            .collect();

        // First tool chunk opens the slot with id + name.
        assert_eq!(
            chunks[0]["choices"][0]["delta"]["tool_calls"][0]["id"],
            "toolu_1"
        );
        assert_eq!(
            chunks[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
            "read"
        );
        // Argument fragments concatenate to valid JSON.
        let args: String = chunks
            .iter()
            .filter_map(|c| {
                c["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                    .as_str()
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(args, "{\"path\":\"a\"}");
        serde_json::from_str::<serde_json::Value>(&args).unwrap();

        // Terminal chunk reports tool_calls, then [DONE].
        assert_eq!(
            chunks[chunks.len() - 1]["choices"][0]["finish_reason"],
            "tool_calls"
        );
        assert_eq!(
            out[out.len() - 1],
            Bytes::from_static(DONE_SENTINEL.as_bytes())
        );
    }

    #[tokio::test]
    async fn max_tokens_message_delta_maps_to_length_finish() {
        let inner = stream::iter(vec![
            Ok(frame(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#,
            )),
            Ok(frame("message_stop", "{}")),
        ]);
        let translator = OpenAiStreamTranslator::new(inner, "claude-sonnet-4-5".to_string());
        let out = drain(translator).await;
        let chunk: serde_json::Value = serde_json::from_slice(
            out[0]
                .strip_prefix(b"data: ")
                .unwrap()
                .strip_suffix(b"\n\n")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(chunk["choices"][0]["finish_reason"], "length");
    }
}
