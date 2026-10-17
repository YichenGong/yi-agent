//! `/v1/responses` 的 SSE 解析。
//!
//! 使用共享的 `crate::sse` 内核（`SseStreamCore`）切分并解析 SSE 帧，按行累积
//! `data:` 负载。区别在于 responses 流同时下发 `event:` 行——那些行一律忽略，
//! 分派只看 JSON 里的 `type` 字段。

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::Stream;
use serde_json::Value;

use yi_agent_core::provider::TokenUsage;
use yi_agent_core::{ProviderError, ProviderEvent, ReasoningKind, StopReason};

use crate::sse::{SseFrame, SseStreamCore};

pub struct ResponsesStream<S> {
    core: SseStreamCore<S>,
    pending_events: VecDeque<ProviderEvent>,
    /// `output_item.added` 记录的 item id → call id。
    ///
    /// `function_call_arguments.delta` 只带 `item_id`，靠它还原成对外稳定的
    /// `call_id`（工具往返要用 call_id 配对）。
    call_ids: HashMap<String, String>,
    /// 是否已发出终止事件，避免重复 `Stop`。
    stopped: bool,
}

impl<S> ResponsesStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    pub fn new(inner: S) -> Self {
        Self {
            core: SseStreamCore::new(inner),
            pending_events: VecDeque::new(),
            call_ids: HashMap::new(),
            stopped: false,
        }
    }

    fn parse_frame(&mut self, frame: SseFrame) -> Result<Vec<ProviderEvent>, ProviderError> {
        let data: Value = match serde_json::from_str(&frame.data) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    provider = "openai-responses",
                    error = %e,
                    data = %frame.data,
                    "dropping malformed SSE JSON frame"
                );
                return Ok(vec![]);
            }
        };

        Ok(self.map_data(&data))
    }

    /// 按 `data.type` 分派一帧。未知类型一律忽略（协议演进时保持宽容）。
    fn map_data(&mut self, data: &Value) -> Vec<ProviderEvent> {
        let mut events = Vec::new();
        let ty = data.get("type").and_then(Value::as_str).unwrap_or_default();

        match ty {
            "response.output_text.delta" => {
                if let Some(delta) = data.get("delta").and_then(Value::as_str) {
                    if !delta.is_empty() {
                        events.push(ProviderEvent::TextDelta(delta.to_string()));
                    }
                }
            }
            "response.reasoning_text.delta" => {
                if let Some(delta) = data.get("delta").and_then(Value::as_str) {
                    if !delta.is_empty() {
                        events.push(ProviderEvent::ReasoningDelta {
                            text: delta.to_string(),
                            kind: ReasoningKind::Content,
                        });
                    }
                }
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(delta) = data.get("delta").and_then(Value::as_str) {
                    if !delta.is_empty() {
                        events.push(ProviderEvent::ReasoningDelta {
                            text: delta.to_string(),
                            kind: ReasoningKind::Summary,
                        });
                    }
                }
            }
            "response.output_item.added" => {
                if let Some(item) = data.get("item") {
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        let item_id = item.get("id").and_then(Value::as_str).unwrap_or_default();
                        let call_id = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
                        if !item_id.is_empty() && !call_id.is_empty() {
                            self.call_ids
                                .insert(item_id.to_string(), call_id.to_string());
                        }
                        if !call_id.is_empty() && !name.is_empty() {
                            events.push(ProviderEvent::ToolUseStart {
                                id: call_id.to_string(),
                                name: name.to_string(),
                            });
                        } else {
                            tracing::warn!(
                                provider = "openai-responses",
                                item_id,
                                call_id,
                                "function_call item missing call_id or name"
                            );
                        }
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let item_id = data
                    .get("item_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match self.call_ids.get(item_id) {
                    Some(call_id) => {
                        if let Some(delta) = data.get("delta").and_then(Value::as_str) {
                            if !delta.is_empty() {
                                events.push(ProviderEvent::ToolUseDelta {
                                    id: call_id.clone(),
                                    partial_json: delta.to_string(),
                                });
                            }
                        }
                    }
                    None => {
                        // 未见过该 item 的 added 帧：跳过，绝不发出空 id 的增量。
                        tracing::warn!(
                            provider = "openai-responses",
                            item_id,
                            "arguments delta for unknown item_id; skipping"
                        );
                    }
                }
            }
            "response.output_item.done" => {
                if let Some(item) = data.get("item") {
                    if item.get("type").and_then(Value::as_str) == Some("reasoning") {
                        if let Some(enc) = item
                            .get("encrypted_content")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                        {
                            events.push(ProviderEvent::ThinkingBlock {
                                text: String::new(),
                                token: yi_agent_core::ThinkingToken::Encrypted {
                                    encrypted_content: enc.to_string(),
                                    // 逐字带走上游 `summary`（缺失/非数组时落为空 Vec），
                                    // 供下一轮回放——回放时该字段必须显式在场。
                                    summary: item
                                        .get("summary")
                                        .and_then(Value::as_array)
                                        .cloned()
                                        .unwrap_or_default(),
                                },
                            });
                        }
                    }
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        let item_id = item.get("id").and_then(Value::as_str).unwrap_or_default();
                        let call_id = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .or_else(|| self.call_ids.remove(item_id));
                        if let Some(call_id) = call_id {
                            events.push(ProviderEvent::ToolUseEnd { id: call_id });
                        }
                    }
                }
            }
            "response.completed" => {
                if self.stopped {
                    return events;
                }
                self.stopped = true;

                // Usage 必须先于 Stop：下游一旦收到 Stop 就收口，晚到的 Usage 会丢。
                if let Some(usage) = data
                    .get("response")
                    .and_then(|r| r.get("usage"))
                    .filter(|u| !u.is_null())
                {
                    events.push(ProviderEvent::Usage(usage_from(usage)));
                }
                events.push(ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                });
            }
            // created / in_progress / content_part.* / output_text.done 等一律忽略。
            _ => {
                tracing::debug!(
                    provider = "openai-responses",
                    event_type = ty,
                    "ignoring event"
                );
            }
        }

        events
    }
}

/// `response.usage` → `TokenUsage`；`reasoning_tokens` 取自
/// `output_tokens_details.reasoning_tokens`。
fn usage_from(usage: &Value) -> TokenUsage {
    let input_tokens = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let output_tokens = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let reasoning_tokens = usage
        .get("output_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .map(|v| v as u32);
    TokenUsage {
        input_tokens,
        output_tokens,
        reasoning_tokens,
        ..Default::default()
    }
}

impl<S> Stream for ResponsesStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    type Item = Result<ProviderEvent, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(event) = self.pending_events.pop_front() {
                return Poll::Ready(Some(Ok(event)));
            }

            match self.core.poll_next_frame(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(Some(Ok(frame))) => match self.parse_frame(frame) {
                    Ok(events) => {
                        if events.is_empty() {
                            continue;
                        }
                        self.pending_events.extend(events);
                    }
                    Err(e) => return Poll::Ready(Some(Err(e))),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use futures::stream;
    use yi_agent_core::{ContentBlock, GenParams, Message, ProviderRequest, ToolSchema};

    use crate::openai::responses::types::responses_request_from;

    /// 把整段 body 喂给解析器，收集全部事件（测试用）。
    async fn collect(body: &str) -> Vec<ProviderEvent> {
        collect_chunks(&[body.as_bytes()]).await
    }

    async fn collect_chunks(chunks: &[&[u8]]) -> Vec<ProviderEvent> {
        let inner = stream::iter(
            chunks
                .iter()
                .map(|c| Ok::<_, reqwest::Error>(Bytes::copy_from_slice(c))),
        );
        let mut s = ResponsesStream::new(inner);
        let mut out = Vec::new();
        while let Some(item) = s.next().await {
            out.push(item.expect("stream item should be Ok"));
        }
        out
    }

    #[tokio::test]
    async fn maps_output_text_and_reasoning_deltas() {
        let body = "event: response.output_text.delta\n\
            data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n\
            event: response.reasoning_text.delta\n\
            data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\"think\"}\n\n\
            event: response.reasoning_summary_text.delta\n\
            data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"sum\"}\n\n\
            event: response.completed\n\
            data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":4,\"output_tokens_details\":{\"reasoning_tokens\":9}}}}\n\n";
        let events = collect(body).await;
        assert!(matches!(&events[0], ProviderEvent::TextDelta(t) if t == "Hi"));
        assert!(
            matches!(&events[1], ProviderEvent::ReasoningDelta{ text, kind: ReasoningKind::Content } if text == "think")
        );
        assert!(
            matches!(&events[2], ProviderEvent::ReasoningDelta{ text, kind: ReasoningKind::Summary } if text == "sum")
        );
        assert!(matches!(events.last(), Some(ProviderEvent::Stop { .. })));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ProviderEvent::Usage(u) if u.reasoning_tokens == Some(9)))
        );
    }

    #[tokio::test]
    async fn maps_function_call_lifecycle() {
        let body = "event: response.output_item.added\n\
            data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"ls\",\"arguments\":\"\"}}\n\n\
            event: response.function_call_arguments.delta\n\
            data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{}\"}\n\n\
            event: response.output_item.done\n\
            data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"ls\",\"arguments\":\"{}\"}}\n\n\
            event: response.completed\n\
            data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
        let events = collect(body).await;
        assert!(
            matches!(&events[0], ProviderEvent::ToolUseStart{ id, name } if id == "call_1" && name == "ls")
        );
        assert!(
            matches!(&events[1], ProviderEvent::ToolUseDelta{ id, partial_json } if id == "call_1" && partial_json == "{}")
        );
        assert!(matches!(&events[2], ProviderEvent::ToolUseEnd{ id } if id == "call_1"));
    }

    #[test]
    fn request_builds_instructions_input_tools_and_reasoning() {
        let req = ProviderRequest {
            model: "HORIZON-OG-Luna".into(),
            system: Some("be brief".into()),
            messages: vec![Message::user("hi")],
            tools: vec![ToolSchema {
                name: "ls".into(),
                description: "list".into(),
                input_schema: serde_json::json!({"type":"object"}),
            }],
            params: GenParams {
                reasoning_effort: Some("low".into()),
                ..Default::default()
            },
        };
        let body = serde_json::to_value(responses_request_from(req)).unwrap();
        assert_eq!(body["instructions"], "be brief");
        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "ls");
        assert!(
            body["tools"][0].get("function").is_none(),
            "responses tools 无 function 包裹"
        );
        assert_eq!(body["input"][0]["role"], "user");
    }

    #[tokio::test]
    async fn request_omits_reasoning_block_when_effort_unset() {
        let req = ProviderRequest {
            model: "m".into(),
            system: None,
            messages: vec![Message::user("hi")],
            tools: vec![],
            params: GenParams::default(),
        };
        let body = serde_json::to_value(responses_request_from(req)).unwrap();
        assert!(body.get("reasoning").is_none());
    }

    // ---- 以下为超出 brief 的防御性用例 ----

    #[tokio::test]
    async fn request_replays_assistant_and_tool_turns() {
        let req = ProviderRequest {
            model: "m".into(),
            system: None,
            messages: vec![
                Message::user("hi"),
                Message::assistant(vec![
                    ContentBlock::Text("calling".into()),
                    ContentBlock::ToolUse {
                        id: "call_1".into(),
                        name: "ls".into(),
                        input: serde_json::json!({"path":"/"}),
                    },
                ]),
                Message::tool_results(vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: vec![ContentBlock::Text("boom".into())],
                    is_error: true,
                }]),
            ],
            tools: vec![],
            params: GenParams::default(),
        };
        let body = serde_json::to_value(responses_request_from(req)).unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["content"][0]["type"], "output_text");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["arguments"], "{\"path\":\"/\"}");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_1");
        assert_eq!(input[3]["output"], "error: boom");
    }

    #[tokio::test]
    async fn ignores_unknown_events_and_missing_usage() {
        let body = "event: response.created\n\
            data: {\"type\":\"response.created\",\"response\":{\"id\":\"r_1\"}}\n\n\
            event: response.in_progress\n\
            data: {\"type\":\"response.in_progress\"}\n\n\
            event: response.totally.unknown\n\
            data: {\"type\":\"response.totally.unknown\",\"delta\":\"x\"}\n\n\
            event: response.completed\n\
            data: {\"type\":\"response.completed\",\"response\":{}}\n\n";
        let events = collect(body).await;
        assert_eq!(events.len(), 1, "events: {events:?}");
        assert!(matches!(events[0], ProviderEvent::Stop { .. }));
    }

    #[tokio::test]
    async fn skips_arguments_delta_for_unknown_item() {
        let body = "event: response.function_call_arguments.delta\n\
            data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"nope\",\"delta\":\"{}\"}\n\n\
            event: response.completed\n\
            data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}\n\n";
        let events = collect(body).await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, ProviderEvent::ToolUseDelta { .. })),
            "no delta for unseen item: {events:?}"
        );
        assert!(matches!(events.last(), Some(ProviderEvent::Stop { .. })));
    }

    #[tokio::test]
    async fn drops_malformed_json_frame_without_killing_stream() {
        let body = "event: response.output_text.delta\n\
            data: not json\n\n\
            event: response.output_text.delta\n\
            data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n";
        let events = collect(body).await;
        assert!(
            matches!(&events[0], ProviderEvent::TextDelta(t) if t == "ok"),
            "events: {events:?}"
        );
    }

    #[tokio::test]
    async fn handles_chunk_splits_inside_line() {
        let full = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}\n\n";
        let split_at = "event: response.out".len();
        let events =
            collect_chunks(&[&full.as_bytes()[..split_at], &full.as_bytes()[split_at..]]).await;
        assert_eq!(events.len(), 1, "events: {events:?}");
        assert!(matches!(&events[0], ProviderEvent::TextDelta(t) if t == "Hello"));
    }

    #[tokio::test]
    async fn respects_crlf_line_endings() {
        let body = "event: response.output_text.delta\r\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\r\n\r\n";
        let events = collect(body).await;
        assert_eq!(events.len(), 1, "events: {events:?}");
        assert!(matches!(&events[0], ProviderEvent::TextDelta(t) if t == "Hi"));
    }

    #[tokio::test]
    async fn reasoning_item_done_emits_thinking_block_with_encrypted_content() {
        let body = "event: response.output_item.done\n\
             data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"enc-blob\",\"summary\":[]}}\n\n\
             event: response.completed\n\
             data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
        let bytes = body.to_string().into_bytes();
        let events: Vec<ProviderEvent> = collect_chunks(&[bytes.as_slice()])
            .await
            .into_iter()
            .collect();

        let found = events.iter().any(|e| {
            matches!(
                e,
                ProviderEvent::ThinkingBlock { token, .. }
                    if *token == yi_agent_core::ThinkingToken::Encrypted {
                        encrypted_content: "enc-blob".into(),
                        summary: vec![],
                    }
            )
        });
        assert!(found, "reasoning item must surface its encrypted_content");
    }

    #[tokio::test]
    async fn reasoning_item_done_captures_summary_verbatim() {
        // B 方案内核：捕获端必须把上游 `summary` 原样存进凭证，供下一轮回放。
        let body = "event: response.output_item.done\n\
             data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"enc-blob\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"thought\"}]}}\n\n\
             event: response.completed\n\
             data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
        let bytes = body.to_string().into_bytes();
        let events: Vec<ProviderEvent> = collect_chunks(&[bytes.as_slice()])
            .await
            .into_iter()
            .collect();

        let found = events.iter().any(|e| {
            matches!(
                e,
                ProviderEvent::ThinkingBlock { token, .. }
                    if *token == yi_agent_core::ThinkingToken::Encrypted {
                        encrypted_content: "enc-blob".into(),
                        summary: vec![serde_json::json!({"type": "summary_text", "text": "thought"})],
                    }
            )
        });
        assert!(
            found,
            "reasoning item must capture its summary verbatim: {events:?}"
        );
    }

    #[tokio::test]
    async fn reasoning_item_done_captures_an_empty_summary_as_an_empty_vec() {
        // 上游 `summary` 常见为 `[]`；捕获端须落为空 Vec（回放时仍显式发出）。
        let body = "event: response.output_item.done\n\
             data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"enc-blob\",\"summary\":[]}}\n\n\
             event: response.completed\n\
             data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
        let bytes = body.to_string().into_bytes();
        let events: Vec<ProviderEvent> = collect_chunks(&[bytes.as_slice()])
            .await
            .into_iter()
            .collect();

        let found = events.iter().any(|e| {
            matches!(
                e,
                ProviderEvent::ThinkingBlock { token, .. }
                    if *token == yi_agent_core::ThinkingToken::Encrypted {
                        encrypted_content: "enc-blob".into(),
                        summary: vec![],
                    }
            )
        });
        assert!(found, "empty summary must be captured as an empty vec");
    }

    // ---- 以下为超出 brief 的防御性用例 ----

    #[tokio::test]
    async fn reasoning_item_without_encrypted_content_emits_no_block() {
        // 空 encrypted_content 与缺失字段都不得产出块（否则回传空凭证）。
        for item in [
            r#"{"type":"reasoning","id":"rs_1","encrypted_content":"","summary":[]}"#,
            r#"{"type":"reasoning","id":"rs_1","summary":[]}"#,
        ] {
            let body = format!(
                "event: response.output_item.done\n\
                 data: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{item}}}\n\n\
                 event: response.completed\n\
                 data: {{\"type\":\"response.completed\",\"response\":{{\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}}}\n\n"
            );
            let events = collect(&body).await;
            assert!(
                !events
                    .iter()
                    .any(|e| matches!(e, ProviderEvent::ThinkingBlock { .. })),
                "item {item} must not emit a thinking block: {events:?}"
            );
        }
    }
}
