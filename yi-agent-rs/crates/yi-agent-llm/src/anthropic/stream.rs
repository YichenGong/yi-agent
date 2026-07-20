//! SSE stream parser for Anthropic streaming responses.
//!
//! Reads raw bytes from a `reqwest` response stream and emits `ProviderEvent`s.
//! Handles the standard SSE wire format: `event: <name>\n` / `data: <json>\n` / `\n` (event boundary).

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::Value;

use yi_agent_core::{ProviderError, ProviderEvent, StopReason};

/// One SSE frame parsed from the byte stream.
struct SseFrame {
    data: String,
}

/// Parses raw byte chunks into SSE frames.
///
/// Buffers raw bytes (not a `String`) so that TCP chunk boundaries splitting a
/// multi-byte UTF-8 character do not cause data loss. Bytes are only decoded to
/// UTF-8 once a complete line (terminated by `\n`) is available.
struct SseLineParser {
    buf: Vec<u8>,
    current_data_lines: Vec<String>,
}

impl SseLineParser {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            current_data_lines: Vec::new(),
        }
    }

    /// Feed a chunk of bytes. Returns completed SSE frames.
    fn feed(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();

        loop {
            // Find the next newline in the buffer.
            let Some(nl) = self.buf.iter().position(|&b| b == b'\n') else {
                break;
            };
            // Extract the line (without the `\n`) and remove it from the buffer.
            let line_bytes: Vec<u8> = self.buf.drain(..=nl).collect();
            let line_bytes = &line_bytes[..line_bytes.len().saturating_sub(1)];

            // Decode the complete line to UTF-8. A complete SSE line is always
            // valid UTF-8 from Anthropic; if it isn't, we surface the error
            // rather than silently dropping bytes.
            let mut line = match std::str::from_utf8(line_bytes) {
                Ok(s) => s.to_string(),
                Err(e) => {
                    // Drop the malformed line but keep going; surface the error
                    // via a synthetic frame so the caller can decide.
                    frames.push(SseFrame {
                        data: format!("{{\"type\":\"__parse_error\",\"error\":\"invalid UTF-8 in SSE line: {e}\"}}"),
                    });
                    continue;
                }
            };

            // Strip trailing \r if present (CRLF)
            if line.ends_with('\r') {
                line.pop();
            }

            if line.is_empty() {
                // Empty line = event boundary. Emit a frame if we have data.
                if !self.current_data_lines.is_empty() {
                    let data = std::mem::take(&mut self.current_data_lines).join("\n");
                    frames.push(SseFrame { data });
                }
                continue;
            }

            if let Some(rest) = line.strip_prefix("data:") {
                self.current_data_lines.push(rest.trim().to_string());
            } else if line.starts_with(':') {
                // Comment, ignore.
            } else {
                // Unknown field (including `event:`) — we dispatch on the JSON
                // `type` field in the data payload, so the SSE `event:` line
                // is not needed.
            }
        }

        frames
    }
}

/// Converts a `Stream<Item = Result<Bytes, reqwest::Error>>` into a
/// `Stream<Item = Result<ProviderEvent, ProviderError>>`.
pub struct AnthropicStream<S> {
    line_parser: SseLineParser,
    inner: S,
    /// Frames parsed from the inner stream but not yet emitted as events.
    pending_frames: VecDeque<SseFrame>,
    /// Maps content block index → tool_use_id (set on content_block_start, read on delta/stop).
    block_ids: HashMap<usize, String>,
}

impl<S> AnthropicStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    pub fn new(inner: S) -> Self {
        Self {
            line_parser: SseLineParser::new(),
            inner,
            pending_frames: VecDeque::new(),
            block_ids: HashMap::new(),
        }
    }

    /// Try to parse one SSE frame into a `ProviderEvent`.
    /// Returns `Ok(None)` for frames that don't emit an event (ping, message_start, etc).
    fn parse_frame(&mut self, frame: SseFrame) -> Result<Option<ProviderEvent>, ProviderError> {
        let data: Value = serde_json::from_str(&frame.data).map_err(|e| {
            ProviderError::Stream(format!("invalid SSE JSON: {e}; data: {}", frame.data))
        })?;

        let event_type = data.get("type").and_then(Value::as_str).unwrap_or("");

        match event_type {
            "__parse_error" => {
                let msg = data
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("invalid UTF-8 in SSE line");
                Err(ProviderError::Stream(msg.to_string()))
            }
            "content_block_start" => {
                let index = data.get("index").and_then(Value::as_u64).ok_or_else(|| {
                    ProviderError::Stream("content_block_start missing index".into())
                })? as usize;
                let block = data.get("content_block").cloned().unwrap_or(Value::Null);
                let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");

                if block_type == "tool_use" {
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    self.block_ids.insert(index, id.clone());
                    return Ok(Some(ProviderEvent::ToolUseStart { id, name }));
                }
                Ok(None)
            }
            "content_block_delta" => {
                let index = data.get("index").and_then(Value::as_u64).ok_or_else(|| {
                    ProviderError::Stream("content_block_delta missing index".into())
                })? as usize;
                let delta = data.get("delta").cloned().unwrap_or(Value::Null);
                let delta_type = delta.get("type").and_then(Value::as_str).unwrap_or("");

                match delta_type {
                    "text_delta" => {
                        let text = delta
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        Ok(Some(ProviderEvent::TextDelta(text)))
                    }
                    "input_json_delta" => {
                        let partial_json = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let id = self.block_ids.get(&index).cloned().ok_or_else(|| {
                            ProviderError::Stream(format!(
                                "input_json_delta for unknown block index {index}"
                            ))
                        })?;
                        Ok(Some(ProviderEvent::ToolUseDelta { id, partial_json }))
                    }
                    _ => Ok(None),
                }
            }
            "content_block_stop" => {
                let index = data.get("index").and_then(Value::as_u64).ok_or_else(|| {
                    ProviderError::Stream("content_block_stop missing index".into())
                })? as usize;
                if let Some(id) = self.block_ids.remove(&index) {
                    Ok(Some(ProviderEvent::ToolUseEnd { id }))
                } else {
                    Ok(None)
                }
            }
            "message_delta" => {
                let delta = data.get("delta").cloned().unwrap_or(Value::Null);
                if let Some(reason_str) = delta.get("stop_reason").and_then(Value::as_str) {
                    let reason = match reason_str {
                        "end_turn" => StopReason::EndTurn,
                        "max_tokens" => StopReason::MaxTokens,
                        "stop_sequence" => StopReason::StopSequence,
                        other => StopReason::Other(other.to_string()),
                    };
                    Ok(Some(ProviderEvent::Stop { reason }))
                } else {
                    Ok(None)
                }
            }
            "message_start" | "message_stop" | "ping" => Ok(None),
            "error" => {
                let msg = data
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                Err(ProviderError::Stream(msg.to_string()))
            }
            _ => Ok(None),
        }
    }
}

impl<S> Stream for AnthropicStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    type Item = Result<ProviderEvent, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // First, drain any pending frames from a previous chunk.
            while let Some(frame) = self.pending_frames.pop_front() {
                match self.parse_frame(frame) {
                    Ok(Some(event)) => return Poll::Ready(Some(Ok(event))),
                    Ok(None) => continue,
                    Err(e) => return Poll::Ready(Some(Err(e))),
                }
            }

            // No pending frames — poll the inner stream for more bytes.
            match self.inner.poll_next_unpin(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(ProviderError::Network(e.to_string()))));
                }
                Poll::Ready(Some(Ok(chunk))) => {
                    let frames = self.line_parser.feed(&chunk);
                    self.pending_frames.extend(frames);
                    // Loop back to drain the newly queued frames.
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use futures::stream::StreamExt;

    /// Helper: feed bytes through an AnthropicStream and collect all events.
    async fn collect_events(chunks: Vec<&[u8]>) -> Vec<Result<ProviderEvent, ProviderError>> {
        let inner = stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, reqwest::Error>(Bytes::copy_from_slice(c))),
        );
        let mut s = AnthropicStream::new(inner);
        let mut out = Vec::new();
        while let Some(item) = s.next().await {
            out.push(item);
        }
        out
    }

    #[tokio::test]
    async fn parses_text_delta_sequence() {
        let body = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n\
             event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
             event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n\
             event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n\
             event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
             event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n\
             event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;

        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        // 3 events: TextDelta("Hello"), TextDelta(" world"), Stop{EndTurn}.
        // (content_block_stop for a text block emits None — no ToolUseEnd.)
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], ProviderEvent::TextDelta(t) if t == "Hello"));
        assert!(matches!(&events[1], ProviderEvent::TextDelta(t) if t == " world"));
        assert!(matches!(
            &events[2],
            ProviderEvent::Stop {
                reason: StopReason::EndTurn
            }
        ));
    }

    #[tokio::test]
    async fn parses_tool_use_with_partial_json() {
        let body = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"read\",\"input\":{}}}\n\n\
             event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n\
             event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"main.rs\\\"}\"}}\n\n\
             event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
             event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;

        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        // 5 events: ToolUseStart, 2x ToolUseDelta, ToolUseEnd, Stop.
        assert_eq!(events.len(), 5);
        match &events[0] {
            ProviderEvent::ToolUseStart { id, name } => {
                assert_eq!(id, "toolu_01");
                assert_eq!(name, "read");
            }
            _ => panic!("expected ToolUseStart"),
        }
        match &events[1] {
            ProviderEvent::ToolUseDelta { id, partial_json } => {
                assert_eq!(id, "toolu_01");
                assert_eq!(partial_json, "{\"path\":");
            }
            _ => panic!("expected ToolUseDelta"),
        }
        match &events[2] {
            ProviderEvent::ToolUseDelta { id, partial_json } => {
                assert_eq!(id, "toolu_01");
                assert_eq!(partial_json, "\"main.rs\"}");
            }
            _ => panic!("expected ToolUseDelta"),
        }
        match &events[3] {
            ProviderEvent::ToolUseEnd { id } => assert_eq!(id, "toolu_01"),
            _ => panic!("expected ToolUseEnd"),
        }
        match &events[4] {
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            } => {}
            _ => panic!("expected Stop{{EndTurn}}"),
        }
    }

    #[tokio::test]
    async fn handles_chunk_splits_inside_line() {
        // Simulate the case where a SSE event is split across multiple byte chunks.
        let full = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n".to_string();
        // Split into "event: content_block_delta\nda" and the rest.
        let split_at = "event: content_block_delta\nda".len();
        let part1 = &full.as_bytes()[..split_at];
        let part2 = &full.as_bytes()[split_at..];

        let events = collect_events(vec![part1, part2]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ProviderEvent::TextDelta(t) if t == "Hello"));
    }

    #[tokio::test]
    async fn handles_chunk_split_inside_multibyte_utf8() {
        // "你好" is 6 bytes in UTF-8: E4 BD A0 E5 A5 BD.
        // Split after 3 bytes (inside the first character) to simulate TCP fragmentation.
        let text = "你好";
        let text_bytes = text.as_bytes();
        // Build an SSE event containing "你好" in the text field.
        let full = format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{}\"}}}}\n\n",
            text
        );
        let full_bytes = full.into_bytes();
        // Find where "你好" starts in the full byte stream and split in the middle of it.
        let text_start = full_bytes
            .windows(text_bytes.len())
            .position(|w| w == text_bytes)
            .expect("text bytes should be present");
        let split_at = text_start + 3; // middle of first character (3 bytes)
        let part1 = &full_bytes[..split_at];
        let part2 = &full_bytes[split_at..];

        let events = collect_events(vec![part1, part2]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        assert_eq!(events.len(), 1, "expected exactly one event");
        match &events[0] {
            ProviderEvent::TextDelta(t) => assert_eq!(t, "你好", "text should be intact"),
            _ => panic!("expected TextDelta"),
        }
    }

    #[tokio::test]
    async fn maps_stop_reasons_correctly() {
        for (input, expected) in [
            ("end_turn", StopReason::EndTurn),
            ("max_tokens", StopReason::MaxTokens),
            ("stop_sequence", StopReason::StopSequence),
            ("tool_use", StopReason::Other("tool_use".to_string())),
        ] {
            let body = format!(
                "event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{}\"}}}}\n\n",
                input
            );
            let bytes = body.into_bytes();
            let events = collect_events(vec![bytes.as_slice()]).await;
            let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
            assert_eq!(events.len(), 1, "input: {}", input);
            match &events[0] {
                ProviderEvent::Stop { reason } => assert_eq!(*reason, expected),
                _ => panic!("expected Stop for input {}", input),
            }
        }
    }

    #[tokio::test]
    async fn ignores_ping_and_message_start() {
        let body = "event: ping\ndata: {\"type\":\"ping\"}\n\n\
             event: message_start\ndata: {\"type\":\"message_start\",\"message\":{}}\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn surfaces_sse_error_event() {
        let body =
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"overloaded\"}}\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            Err(ProviderError::Stream(msg)) => assert_eq!(msg, "overloaded"),
            _ => panic!("expected Stream error"),
        }
    }
}
