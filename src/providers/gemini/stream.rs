//! `GeminiToAnthropicStream`: translates a Cloud Code Assist SSE byte stream
//! (`v1internal:streamGenerateContent?alt=sse`) into an Anthropic Messages
//! SSE event stream (Story 2.1.1).
//!
//! Structurally mirrors `OpenaiToAnthropicStream`
//! (`src/providers/openai.rs:369-535`) — same `VecDeque<Bytes>` buffering
//! pattern and Anthropic bracketing shape (`message_start` /
//! `content_block_start` / `content_block_delta` / `content_block_stop` /
//! `message_delta` / `message_stop`) — but tracks **multiple** indexed
//! content blocks instead of a single hardcoded index-0 text block, since
//! Gemini/Cloud-Code streaming can interleave text + (Phase 3)
//! `functionCall` parts within one candidate's accumulating `parts[]` (see
//! plan.md's Domain Glossary and the "Streaming reconstruction" Pattern
//! Decisions row).
//!
//! Each SSE `data:` line's JSON wraps the native Gemini response in an extra
//! `"response"` layer: `{"response": {"candidates": [...], "usageMetadata":
//! {...}}}` — unwrapped before walking `candidates[0].content.parts[]`.
//!
//! Unparseable-chunk handling is fail-closed (Epic 2.2), mirroring
//! `bedrock.rs`'s streaming precedent at `src/providers/bedrock.rs:719-735`:
//! an unparseable `data:` line ends the stream with one
//! `Err(ProviderError::ResponseShapeMismatch(..))` item, with no further
//! items yielded after it — not `OpenaiToAnthropicStream`'s
//! `continue`-on-parse-failure behavior.

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures_core::Stream;
use serde_json::{json, Value};
use tracing::warn;

use crate::providers::ProviderError;

use super::translate::{map_gemini_finish_reason, GeminiUsageMetadata};

/// Which Anthropic content-block type a Gemini part maps to. Only `Text`
/// exists through Phase 2 — Phase 3 adds a `FunctionCall` variant once tool
/// calls land (see plan.md's Domain Glossary `GeminiPart` entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockKind {
    Text,
    /// Test-only stand-in for a second real variant (not added until Phase
    /// 3's `FunctionCall`). Exists solely so
    /// `gemini_to_anthropic_stream_should_open_new_indexed_block_when_part_type_changes`
    /// can prove `resolve_block_index`'s type-change branch generically
    /// opens a *new* index — the "new Gemini-part-position" branch is
    /// already exercised by `Text`-only production code on its own, but the
    /// "an already-tracked position's kind changes" branch has no real
    /// second variant to trigger it with until Phase 3.
    #[cfg(test)]
    TestOther,
}

impl BlockKind {
    fn anthropic_block_json(self) -> Value {
        match self {
            BlockKind::Text => json!({"type": "text", "text": ""}),
            #[cfg(test)]
            BlockKind::TestOther => json!({"type": "test_other"}),
        }
    }
}

/// Classifies a raw Gemini `parts[]` entry's `BlockKind`. Only `Text` exists
/// through Phase 2 — Phase 3 adds `functionCall` detection here.
fn classify_part_kind(_part: &Value) -> BlockKind {
    BlockKind::Text
}

/// Translates a Cloud Code Assist `streamGenerateContent` SSE byte stream
/// into an Anthropic Messages SSE event stream.
///
/// `active_blocks[i]` tracks the `BlockKind` of the Anthropic content block
/// opened for Gemini part-position `i` — instead of
/// `OpenaiToAnthropicStream`'s single implicit index-0 assumption, a new
/// Gemini part-position (or a part whose kind changes) gets its own,
/// newly-appended index rather than corrupting/overwriting an existing one.
pub(crate) struct GeminiToAnthropicStream<S> {
    inner: eventsource_stream::EventStream<S>,
    id: String,
    model: String,
    started: bool,
    finished: bool,
    done: bool,
    active_blocks: Vec<BlockKind>,
    pending: VecDeque<Bytes>,
}

impl<S> GeminiToAnthropicStream<S> {
    fn frame(event: &str, data: &Value) -> Bytes {
        Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
    }

    /// Push the synthetic `message_start` event, if not already emitted, so
    /// every stream opens with a well-formed Anthropic preamble even if the
    /// first Gemini chunk carries no parts yet.
    fn ensure_started(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        self.pending.push_back(Self::frame(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": self.id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": self.model,
                    "stop_reason": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            }),
        ));
    }

    /// Resolves the Anthropic block index the next Gemini part of `kind`
    /// should emit into. If the most-recently-opened block's kind matches
    /// `kind`, reuses it (so multiple same-kind parts — whether split across
    /// separate SSE chunks, e.g. incremental text deltas, or appearing
    /// together within one chunk's `parts[]` — all continue appending to the
    /// same running content block, matching Anthropic's own one-block-per-
    /// content-run streaming shape). Otherwise opens a **new**, appended
    /// index (never reusing/overwriting an existing one) and emits its
    /// `content_block_start` event.
    fn resolve_block_index(&mut self, kind: BlockKind) -> usize {
        self.ensure_started();
        if let Some(last_kind) = self.active_blocks.last() {
            if *last_kind == kind {
                return self.active_blocks.len() - 1;
            }
        }
        self.open_new_block(kind)
    }

    /// Unconditionally opens a new, appended block index for `kind` and
    /// emits its `content_block_start` event — used both by
    /// `resolve_block_index` (when the kind changed/no block is open yet)
    /// and directly by callers that need a forced-new block regardless of
    /// the most recently opened kind (e.g. a synthesized safety/recitation
    /// annotation appended after generation stops, which is never a
    /// continuation of preceding text deltas).
    fn open_new_block(&mut self, kind: BlockKind) -> usize {
        self.ensure_started();
        let new_index = self.active_blocks.len();
        self.active_blocks.push(kind);
        self.pending.push_back(Self::frame(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": new_index,
                "content_block": kind.anthropic_block_json(),
            }),
        ));
        new_index
    }

    fn push_delta(&mut self, index: usize, text: &str) {
        self.pending.push_back(Self::frame(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "text_delta", "text": text}
            }),
        ));
    }

    /// Emits `content_block_stop` for every open block, then
    /// `message_delta`/`message_stop`, and marks the stream finished.
    /// Idempotent.
    fn close(&mut self, stop_reason: &str, usage: &Value) {
        if self.finished {
            return;
        }
        self.ensure_started();
        self.finished = true;
        for index in 0..self.active_blocks.len() {
            self.pending.push_back(Self::frame(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": index}),
            ));
        }
        self.pending.push_back(Self::frame(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason},
                "usage": usage
            }),
        ));
        self.pending.push_back(Self::frame(
            "message_stop",
            &json!({"type": "message_stop"}),
        ));
    }
}

impl<S> GeminiToAnthropicStream<S>
where
    S: Stream<Item = Result<Bytes, anyhow::Error>>,
{
    pub(crate) fn new(inner: S, model: String) -> Self {
        Self {
            inner: inner.eventsource(),
            id: format!("msg_{}", uuid::Uuid::new_v4()),
            model,
            started: false,
            finished: false,
            done: false,
            active_blocks: Vec::new(),
            pending: VecDeque::new(),
        }
    }
}

/// Maps a `GeminiUsageMetadata` (already parsed off the chunk's
/// `usageMetadata` field, reusing Story 1.3.2's wire struct) onto the
/// `message_delta` event's `usage` object.
fn usage_delta_json(usage_metadata: Option<&GeminiUsageMetadata>) -> Value {
    usage_metadata.map_or_else(
        || json!({"input_tokens": 0, "output_tokens": 0}),
        |u| {
            json!({
                "input_tokens": u.prompt_token_count,
                "output_tokens": u.candidates_token_count,
            })
        },
    )
}

impl<S> Stream for GeminiToAnthropicStream<S>
where
    S: Stream<Item = Result<Bytes, anyhow::Error>> + Unpin,
{
    type Item = Result<Bytes, anyhow::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            if let Some(frame) = this.pending.pop_front() {
                return Poll::Ready(Some(Ok(frame)));
            }
            if this.done {
                return Poll::Ready(None);
            }
            if this.finished {
                this.done = true;
                continue;
            }

            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    this.close("end_turn", &usage_delta_json(None));
                }
                Poll::Ready(Some(Err(e))) => {
                    warn!(error = %e, "gemini->anthropic stream translator: eventsource parse error");
                    this.close("end_turn", &usage_delta_json(None));
                }
                Poll::Ready(Some(Ok(event))) => {
                    let parsed = match serde_json::from_str::<Value>(&event.data) {
                        Ok(parsed) => parsed,
                        Err(e) => {
                            // Fail-closed (Epic 2.2, mirroring
                            // bedrock.rs:719-735): end the stream with one
                            // error item, never resuming as if nothing
                            // happened.
                            this.done = true;
                            return Poll::Ready(Some(Err(ProviderError::ResponseShapeMismatch(
                                format!("gemini stream: unparseable data line: {e}"),
                            )
                            .into())));
                        }
                    };
                    let response_body = parsed.get("response").unwrap_or(&parsed);

                    let candidate = response_body
                        .get("candidates")
                        .and_then(Value::as_array)
                        .and_then(|a| a.first());

                    let Some(candidate) = candidate else {
                        continue;
                    };

                    if let Some(parts) = candidate
                        .get("content")
                        .and_then(|c| c.get("parts"))
                        .and_then(Value::as_array)
                    {
                        for part in parts {
                            // Fail closed (ADR-002): streaming tool-call
                            // support isn't implemented (only the
                            // non-streaming path stashes
                            // functionCall/thoughtSignature) — surfacing a
                            // functionCall part here as if it were text
                            // would silently drop the tool_use block and
                            // produce an incomplete answer with no error.
                            if part.get("functionCall").is_some() {
                                this.done = true;
                                return Poll::Ready(Some(Err(ProviderError::ResponseShapeMismatch(
                                    "streaming responses containing tool calls are not yet supported by this provider; retry without streaming, or use a non-tool-calling conversation".to_string(),
                                )
                                .into())));
                            }
                            let kind = classify_part_kind(part);
                            let index = this.resolve_block_index(kind);
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    this.push_delta(index, text);
                                }
                            }
                        }
                    }

                    if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                        let (stop_reason, synthesized) = map_gemini_finish_reason(Some(reason));
                        if let Some(text) = synthesized {
                            let index = this.open_new_block(BlockKind::Text);
                            this.push_delta(index, &text);
                        }
                        let usage_metadata = response_body.get("usageMetadata").and_then(|v| {
                            serde_json::from_value::<GeminiUsageMetadata>(v.clone()).ok()
                        });
                        this.close(stop_reason, &usage_delta_json(usage_metadata.as_ref()));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use futures_util::stream::{self, StreamExt};

    fn sse(data: &str) -> Bytes {
        Bytes::from(format!("data: {data}\n\n"))
    }

    fn parse_event(frame: &Bytes) -> (String, Value) {
        let text = String::from_utf8(frame.to_vec()).unwrap();
        let mut event = String::new();
        let mut data = String::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("event: ") {
                event = rest.to_string();
            } else if let Some(rest) = line.strip_prefix("data: ") {
                data = rest.to_string();
            }
        }
        (event, serde_json::from_str(&data).unwrap())
    }

    async fn drain<S>(s: S) -> Vec<Bytes>
    where
        S: Stream<Item = Result<Bytes, anyhow::Error>>,
    {
        s.map(|item| item.unwrap()).collect().await
    }

    // REQ-18 — happy path: a single text block's bracketing sequence matches
    // `OpenaiToAnthropicStream`'s existing single-block shape.
    #[tokio::test]
    async fn gemini_to_anthropic_stream_should_emit_correct_bracketing_sequence_for_single_text_block(
    ) {
        let inner = stream::iter(vec![
            Ok(sse(
                r#"{"response":{"candidates":[{"content":{"parts":[{"text":"Hel"}]}}]}}"#,
            )),
            Ok(sse(
                r#"{"response":{"candidates":[{"content":{"parts":[{"text":"lo"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7}}}"#,
            )),
        ]);
        let translator = GeminiToAnthropicStream::new(inner, "gemini-3-pro".to_string());
        let out = drain(translator).await;

        let events: Vec<String> = out.iter().map(|f| parse_event(f).0).collect();
        assert_eq!(
            events,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );

        let (_, block_start) = parse_event(&out[1]);
        assert_eq!(block_start["index"], 0);
        assert_eq!(block_start["content_block"]["type"], "text");

        let (_, delta1) = parse_event(&out[2]);
        assert_eq!(delta1["delta"]["text"], "Hel");
        let (_, delta2) = parse_event(&out[3]);
        assert_eq!(delta2["delta"]["text"], "lo");

        let (_, block_stop) = parse_event(&out[4]);
        assert_eq!(block_stop["index"], 0);

        let (_, message_delta) = parse_event(&out[5]);
        assert_eq!(message_delta["delta"]["stop_reason"], "end_turn");
        assert_eq!(message_delta["usage"]["input_tokens"], 5);
        assert_eq!(message_delta["usage"]["output_tokens"], 2);
    }

    // Fix 8 (code review) — a single SSE chunk whose `parts[]` carries TWO
    // text entries (real Gemini chunks can legitimately split text across
    // multiple `parts[]` entries within one event) must produce ONE
    // continuous text content block with the fragments appended in order —
    // not two separate content blocks. This pins down a real bug found while
    // adding this coverage: `resolve_block_index` used to key off each
    // part's raw array index, so a second same-kind part at index 1 (with no
    // block yet tracked at that index) always opened its own new block
    // instead of continuing the running text block.
    #[tokio::test]
    async fn gemini_to_anthropic_stream_should_concatenate_two_text_parts_within_one_chunk_into_one_block(
    ) {
        let inner = stream::iter(vec![Ok(sse(
            r#"{"response":{"candidates":[{"content":{"parts":[{"text":"A"},{"text":" B"}]},"finishReason":"STOP"}]}}"#,
        ))]);
        let translator = GeminiToAnthropicStream::new(inner, "gemini-3-pro".to_string());
        let out = drain(translator).await;

        let events: Vec<String> = out.iter().map(|f| parse_event(f).0).collect();
        assert_eq!(
            events,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ],
            "two same-kind parts in one chunk must open exactly ONE content block, not two"
        );

        let (_, block_start) = parse_event(&out[1]);
        assert_eq!(block_start["index"], 0);

        let (_, delta1) = parse_event(&out[2]);
        assert_eq!(delta1["index"], 0);
        assert_eq!(delta1["delta"]["text"], "A");
        let (_, delta2) = parse_event(&out[3]);
        assert_eq!(delta2["index"], 0);
        assert_eq!(delta2["delta"]["text"], " B");

        let (_, block_stop) = parse_event(&out[4]);
        assert_eq!(block_stop["index"], 0);
    }

    // REQ-18 — edge path: proves `resolve_block_index`'s kind-change branch
    // generically opens a *new*, appended index rather than
    // reusing/overwriting an existing one, while same-kind calls in a row
    // reuse the most recently opened block (Fix 8's streaming-concatenation
    // correction). Exercised as a white-box unit test directly against
    // `resolve_block_index` (rather than through a full SSE fixture) because
    // only `BlockKind::Text` exists as a real variant through Phase 2 — a
    // `#[cfg(test)]`-only second variant (`BlockKind::TestOther`) stands in
    // for the real second variant Phase 3 adds, so the *mechanism* (not a
    // specific Phase-3 kind) is what's proven here. See
    // `BlockKind::TestOther`'s doc comment for why this approach was chosen
    // over a synthetic-JSON end-to-end test.
    #[test]
    fn gemini_to_anthropic_stream_should_open_new_indexed_block_when_kind_changes() {
        let inner = stream::iter(Vec::<Result<Bytes, anyhow::Error>>::new());
        let mut translator = GeminiToAnthropicStream::new(inner, "gemini-3-pro".to_string());

        // First call: no block open yet, opens index 0.
        let first_index = translator.resolve_block_index(BlockKind::Text);
        assert_eq!(first_index, 0);

        // Same kind again: reuses index 0 — not a new block (this is what
        // lets two same-kind parts/chunks in a row concatenate into one
        // running content block).
        let same_index = translator.resolve_block_index(BlockKind::Text);
        assert_eq!(same_index, 0);

        // Kind changes: a brand-new index is opened, never reusing or
        // silently merging into index 0.
        let changed_index = translator.resolve_block_index(BlockKind::TestOther);
        assert_eq!(changed_index, 1);
        assert_eq!(
            translator.active_blocks,
            vec![BlockKind::Text, BlockKind::TestOther]
        );

        // Switching back to a previously-seen kind (Text) after an
        // intervening different kind still opens a brand-new index — never
        // reuses the stale index 0.
        let back_to_text_index = translator.resolve_block_index(BlockKind::Text);
        assert_eq!(back_to_text_index, 2);
    }

    // REQ-20 — fail-closed: an unparseable `data:` line ends the stream with
    // `ResponseShapeMismatch`, with no further items after it (matches
    // `bedrock.rs:719-735`'s "breaks the stream" precedent, not "skip and
    // continue").
    #[tokio::test]
    async fn gemini_to_anthropic_stream_should_end_stream_with_response_shape_mismatch_on_unparseable_chunk(
    ) {
        let inner = stream::iter(vec![
            Ok(sse(
                r#"{"response":{"candidates":[{"content":{"parts":[{"text":"Hel"}]}}]}}"#,
            )),
            Ok(sse("not valid json")),
            // Still sitting in the inner stream's buffer after the error —
            // must never be reached.
            Ok(sse(
                r#"{"response":{"candidates":[{"content":{"parts":[{"text":"lo"}]},"finishReason":"STOP"}]}}"#,
            )),
        ]);
        let mut translator = GeminiToAnthropicStream::new(inner, "gemini-3-pro".to_string());

        // Drain the valid frames produced by the first, well-formed chunk.
        let mut saw_ok_frame = false;
        let err = loop {
            match translator.next().await {
                Some(Ok(_)) => saw_ok_frame = true,
                Some(Err(e)) => break e,
                None => panic!("stream ended before yielding the expected error"),
            }
        };
        assert!(
            saw_ok_frame,
            "expected at least one Ok frame from the valid first chunk"
        );

        let provider_err = err
            .downcast_ref::<ProviderError>()
            .expect("expected the Err to wrap a ProviderError");
        assert!(
            matches!(provider_err, ProviderError::ResponseShapeMismatch(_)),
            "expected ResponseShapeMismatch, got {provider_err:?}"
        );

        // No further items — not even from the well-formed third chunk still
        // sitting in the inner stream.
        assert!(translator.next().await.is_none());
    }

    // Fail-closed per ADR-002: a streaming chunk carrying a `functionCall`
    // part must end the stream with `ResponseShapeMismatch`, never a
    // silently-incomplete text-only response.
    #[tokio::test]
    async fn gemini_to_anthropic_stream_should_end_stream_with_response_shape_mismatch_on_function_call_part(
    ) {
        let inner = stream::iter(vec![
            Ok(sse(
                r#"{"response":{"candidates":[{"content":{"parts":[{"text":"Hel"}]}}]}}"#,
            )),
            Ok(sse(
                r#"{"response":{"candidates":[{"content":{"parts":[{"functionCall":{"name":"get_weather","args":{"city":"Boise"}}}]},"finishReason":"STOP"}]}}"#,
            )),
            // Still sitting in the inner stream's buffer after the error —
            // must never be reached.
            Ok(sse(
                r#"{"response":{"candidates":[{"content":{"parts":[{"text":"lo"}]},"finishReason":"STOP"}]}}"#,
            )),
        ]);
        let mut translator = GeminiToAnthropicStream::new(inner, "gemini-3-pro".to_string());

        let mut saw_ok_frame = false;
        let err = loop {
            match translator.next().await {
                Some(Ok(_)) => saw_ok_frame = true,
                Some(Err(e)) => break e,
                None => panic!("stream ended before yielding the expected error"),
            }
        };
        assert!(
            saw_ok_frame,
            "expected at least one Ok frame from the valid first chunk"
        );

        let provider_err = err
            .downcast_ref::<ProviderError>()
            .expect("expected the Err to wrap a ProviderError");
        assert!(
            matches!(provider_err, ProviderError::ResponseShapeMismatch(_)),
            "expected ResponseShapeMismatch, got {provider_err:?}"
        );

        assert!(translator.next().await.is_none());
    }

    #[tokio::test]
    async fn empty_stream_still_produces_well_formed_bracketing_events() {
        let inner = stream::iter(Vec::<Result<Bytes, anyhow::Error>>::new());
        let translator = GeminiToAnthropicStream::new(inner, "gemini-3-pro".to_string());
        let out = drain(translator).await;

        let events: Vec<String> = out.iter().map(|f| parse_event(f).0).collect();
        assert_eq!(
            events,
            vec!["message_start", "message_delta", "message_stop"]
        );
    }
}
