//! SSE stream parser for OpenAI Chat Completions streaming responses.
//!
//! Reads raw bytes from a `reqwest` response stream and emits `ProviderEvent`s.
//! Handles SSE wire format: `data: <json>\n\n` with `data: [DONE]` terminator.

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::Value;

use yi_agent_core::provider::TokenUsage;
use yi_agent_core::{ProviderError, ProviderEvent, StopReason};

struct SseFrame {
    data: String,
}

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

    fn feed(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();

        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = self.buf.drain(..=nl).collect();
            let line_bytes = &line_bytes[..line_bytes.len().saturating_sub(1)];

            let mut line = match std::str::from_utf8(line_bytes) {
                Ok(s) => s.to_string(),
                Err(e) => {
                    frames.push(SseFrame {
                        data: format!("{{\"__parse_error\":\"invalid UTF-8 in SSE line: {e}\"}}"),
                    });
                    continue;
                }
            };

            if line.ends_with('\r') {
                line.pop();
            }

            if line.is_empty() {
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
            }
        }

        frames
    }
}

pub struct OpenaiStream<S> {
    line_parser: SseLineParser,
    inner: S,
    pending_frames: VecDeque<SseFrame>,
    pending_events: VecDeque<ProviderEvent>,
    tool_calls: HashMap<usize, (String, String)>,
    /// Whether a Stop event has already been emitted (from finish_reason or [DONE]).
    stopped: bool,
}

impl<S> OpenaiStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    pub fn new(inner: S) -> Self {
        Self {
            line_parser: SseLineParser::new(),
            inner,
            pending_frames: VecDeque::new(),
            pending_events: VecDeque::new(),
            tool_calls: HashMap::new(),
            stopped: false,
        }
    }

    fn parse_frame(&mut self, frame: SseFrame) -> Result<Vec<ProviderEvent>, ProviderError> {
        if frame.data == "[DONE]" {
            if self.stopped {
                return Ok(vec![]);
            }
            self.stopped = true;
            return Ok(vec![ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            }]);
        }

        if let Some(rest) = frame.data.strip_prefix("{\"__parse_error\":") {
            let msg = rest.trim_end_matches('}');
            return Err(ProviderError::Stream(msg.to_string()));
        }

        let data: Value = serde_json::from_str(&frame.data).map_err(|e| {
            ProviderError::Stream(format!("invalid SSE JSON: {e}; data: {}", frame.data))
        })?;

        let mut events = Vec::new();

        if let Some(usage) = data.get("usage") {
            if !usage.is_null() {
                let prompt_tokens = usage
                    .get("prompt_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32;
                let completion_tokens = usage
                    .get("completion_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32;
                events.push(ProviderEvent::Usage(TokenUsage {
                    input_tokens: prompt_tokens,
                    output_tokens: completion_tokens,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                }));
            }
        }

        if let Some(choices) = data.get("choices").and_then(|c| c.as_array()) {
            for choice in choices {
                if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
                    if self.stopped {
                        continue;
                    }
                    match finish {
                        "stop" => {
                            self.stopped = true;
                            events.push(ProviderEvent::Stop {
                                reason: StopReason::EndTurn,
                            });
                        }
                        "length" => {
                            self.stopped = true;
                            events.push(ProviderEvent::Stop {
                                reason: StopReason::MaxTokens,
                            });
                        }
                        "tool_calls" => {
                            self.stopped = true;
                            let mut indices: Vec<usize> = self.tool_calls.keys().copied().collect();
                            indices.sort();
                            for idx in indices {
                                if let Some((id, _)) = self.tool_calls.remove(&idx) {
                                    events.push(ProviderEvent::ToolUseEnd { id });
                                }
                            }
                            events.push(ProviderEvent::Stop {
                                reason: StopReason::EndTurn,
                            });
                        }
                        other => {
                            self.stopped = true;
                            events.push(ProviderEvent::Stop {
                                reason: StopReason::Other(other.to_string()),
                            });
                        }
                    }
                }

                if let Some(delta) = choice.get("delta") {
                    if let Some(content) = delta.get("content").and_then(Value::as_str) {
                        if !content.is_empty() {
                            events.push(ProviderEvent::TextDelta(content.to_string()));
                        }
                    }

                    if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                        for tc in tool_calls {
                            let index =
                                tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;

                            if let Some(id) = tc.get("id").and_then(Value::as_str) {
                                let name = tc
                                    .get("function")
                                    .and_then(|f| f.get("name"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                self.tool_calls
                                    .insert(index, (id.to_string(), name.clone()));
                                events.push(ProviderEvent::ToolUseStart {
                                    id: id.to_string(),
                                    name,
                                });
                            }

                            if let Some(args) = tc
                                .get("function")
                                .and_then(|f| f.get("arguments"))
                                .and_then(Value::as_str)
                            {
                                if !args.is_empty() {
                                    if let Some((id, _)) = self.tool_calls.get(&index) {
                                        events.push(ProviderEvent::ToolUseDelta {
                                            id: id.clone(),
                                            partial_json: args.to_string(),
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(events)
    }
}

impl<S> Stream for OpenaiStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    type Item = Result<ProviderEvent, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(event) = self.pending_events.pop_front() {
                return Poll::Ready(Some(Ok(event)));
            }

            while let Some(frame) = self.pending_frames.pop_front() {
                match self.parse_frame(frame) {
                    Ok(events) => {
                        if events.is_empty() {
                            continue;
                        }
                        for ev in events {
                            self.pending_events.push_back(ev);
                        }
                        // Break out of the while loop to go back to the top of
                        // the outer loop, which will drain pending_events first.
                        break;
                    }
                    Err(e) => return Poll::Ready(Some(Err(e))),
                }
            }

            // If we have events queued from the while loop above, loop back
            // to drain them before polling the inner stream.
            if !self.pending_events.is_empty() {
                continue;
            }

            match self.inner.poll_next_unpin(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(ProviderError::Network(e.to_string()))));
                }
                Poll::Ready(Some(Ok(chunk))) => {
                    let frames = self.line_parser.feed(&chunk);
                    self.pending_frames.extend(frames);
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

    async fn collect_events(chunks: Vec<&[u8]>) -> Vec<Result<ProviderEvent, ProviderError>> {
        let inner = stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, reqwest::Error>(Bytes::copy_from_slice(c))),
        );
        let mut s = OpenaiStream::new(inner);
        let mut out = Vec::new();
        while let Some(item) = s.next().await {
            out.push(item);
        }
        out
    }

    #[tokio::test]
    async fn parses_text_delta_sequence() {
        let body = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n\
             data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
             data: [DONE]\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
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
    async fn parses_tool_call_with_incremental_arguments() {
        let body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"main.rs\\\"}\"}}]}}]}\n\n\
             data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n\
             data: [DONE]\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        assert_eq!(events.len(), 5, "events: {:?}", events);
        match &events[0] {
            ProviderEvent::ToolUseStart { id, name } => {
                assert_eq!(id, "call_abc");
                assert_eq!(name, "read");
            }
            _ => panic!("expected ToolUseStart"),
        }
        match &events[1] {
            ProviderEvent::ToolUseDelta { id, partial_json } => {
                assert_eq!(id, "call_abc");
                assert_eq!(partial_json, "{\"path\":");
            }
            _ => panic!("expected ToolUseDelta 1"),
        }
        match &events[2] {
            ProviderEvent::ToolUseDelta { id, partial_json } => {
                assert_eq!(id, "call_abc");
                assert_eq!(partial_json, "\"main.rs\"}");
            }
            _ => panic!("expected ToolUseDelta 2"),
        }
        match &events[3] {
            ProviderEvent::ToolUseEnd { id } => assert_eq!(id, "call_abc"),
            _ => panic!("expected ToolUseEnd"),
        }
        assert!(matches!(
            &events[4],
            ProviderEvent::Stop {
                reason: StopReason::EndTurn
            }
        ));
    }

    #[tokio::test]
    async fn handles_chunk_splits_inside_line() {
        let full = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n".to_string();
        let split_at = "data: {\"cho".len();
        let part1 = &full.as_bytes()[..split_at];
        let part2 = &full.as_bytes()[split_at..];
        let events = collect_events(vec![part1, part2]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ProviderEvent::TextDelta(t) if t == "Hello"));
    }

    #[tokio::test]
    async fn handles_chunk_split_inside_multibyte_utf8() {
        let text = "你好";
        let full = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{}\"}}}}]}}\n\n",
            text
        );
        let full_bytes = full.into_bytes();
        let text_bytes = text.as_bytes();
        let text_start = full_bytes
            .windows(text_bytes.len())
            .position(|w| w == text_bytes)
            .expect("text bytes should be present");
        let split_at = text_start + 3;
        let part1 = &full_bytes[..split_at];
        let part2 = &full_bytes[split_at..];
        let events = collect_events(vec![part1, part2]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ProviderEvent::TextDelta(t) => assert_eq!(t, "你好"),
            _ => panic!("expected TextDelta"),
        }
    }

    #[tokio::test]
    async fn maps_finish_reasons_correctly() {
        for (input, expected) in [
            ("stop", StopReason::EndTurn),
            ("length", StopReason::MaxTokens),
            ("tool_calls", StopReason::EndTurn),
        ] {
            let body = format!(
                "data: {{\"choices\":[{{\"finish_reason\":\"{}\"}}]}}\n\ndata: [DONE]\n\n",
                input
            );
            let bytes = body.into_bytes();
            let events = collect_events(vec![bytes.as_slice()]).await;
            let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
            let stop = events
                .iter()
                .find(|e| matches!(e, ProviderEvent::Stop { .. }));
            assert!(stop.is_some(), "input: {}, events: {:?}", input, events);
            match stop.unwrap() {
                ProviderEvent::Stop { reason } => assert_eq!(*reason, expected, "input: {}", input),
                _ => unreachable!(),
            }
        }
    }

    #[tokio::test]
    async fn parses_done_marker_without_finish_reason() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
             data: [DONE]\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], ProviderEvent::TextDelta(t) if t == "hi"));
        assert!(matches!(
            &events[1],
            ProviderEvent::Stop {
                reason: StopReason::EndTurn
            }
        ));
    }

    #[tokio::test]
    async fn parses_usage_in_final_chunk() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
             data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n\
             data: [DONE]\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        assert_eq!(events.len(), 3);
        match &events[1] {
            ProviderEvent::Usage(u) => {
                assert_eq!(u.input_tokens, 10);
                assert_eq!(u.output_tokens, 5);
                assert_eq!(u.cache_creation_input_tokens, None);
                assert_eq!(u.cache_read_input_tokens, None);
            }
            _ => panic!("expected Usage event, got: {:?}", events[1]),
        }
    }

    #[tokio::test]
    async fn ignores_empty_delta_chunks() {
        let body = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
             data: [DONE]\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn multiple_tool_calls_in_one_response() {
        let body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_b\",\"function\":{\"name\":\"write\",\"arguments\":\"\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{}\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\"{}\"}}]}}]}\n\n\
             data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n\
             data: [DONE]\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        assert_eq!(events.len(), 7, "events: {:?}", events);
        assert!(
            matches!(&events[0], ProviderEvent::ToolUseStart { id, name } if id == "call_a" && name == "read")
        );
        assert!(
            matches!(&events[1], ProviderEvent::ToolUseStart { id, name } if id == "call_b" && name == "write")
        );
        assert!(matches!(&events[2], ProviderEvent::ToolUseDelta { id, .. } if id == "call_a"));
        assert!(matches!(&events[3], ProviderEvent::ToolUseDelta { id, .. } if id == "call_b"));
        assert!(matches!(&events[4], ProviderEvent::ToolUseEnd { id } if id == "call_a"));
        assert!(matches!(&events[5], ProviderEvent::ToolUseEnd { id } if id == "call_b"));
        assert!(matches!(
            &events[6],
            ProviderEvent::Stop {
                reason: StopReason::EndTurn
            }
        ));
    }

    #[tokio::test]
    async fn surfaces_invalid_json_as_error() {
        let body = "data: not valid json\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], Err(ProviderError::Stream(_))));
    }
}
