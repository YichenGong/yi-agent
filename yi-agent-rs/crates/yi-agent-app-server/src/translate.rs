//! `AgentEvent` → 线协议通知的翻译层。
//!
//! [`Translator`] 是纯同步、无 I/O 的状态机:调用方把 `yi_agent_core::AgentEvent`
//! 逐个喂进 [`Translator::on_event`],拿回零个或多个 [`Notification`]。
//! 它只负责事件翻译,不负责发帧——发帧由上层 server 主循环通过
//! `transport::MessageWriter` 完成。

use std::collections::HashMap;

use serde_json::Value;
use yi_agent_core::message::ContentBlock;
use yi_agent_core::{AgentEvent, DoneReason};

use crate::protocol::{Item, Notification, ToolStatus, TurnStatus};

/// 正在累积、尚未 finalize 的 agent 文本消息。
struct ActiveAgentMsg {
    item_id: String,
    text: String,
}

/// 已开始、可能尚未结束的工具调用条目。
struct ToolItem {
    item_id: String,
    name: String,
    input: Value,
}

/// 把 `AgentEvent` 流翻译成 [`Notification`]。
///
/// 每个 `Translator` 绑定一个 thread;当前 turn id 由上层在 turn 开始时通过
/// [`Translator::set_turn`] 注入。内部维护"当前打开的 agentMessage"和
/// 每个 `call_id` 对应的工具条目,以保证 `item/started` / `item/delta` /
/// `item/completed` 生命周期配对。
///
/// `tool_items` 是有界的:条目在收到 `ToolResult` / `ToolExit` /
/// `ToolTimeout` 或 turn 终结事件时被移除,不会跨 turn 累积。收到终结事件
/// (`Done` / `Cancelled` / `Error`)后 `tool_items` 必为空;上层必须为下一个
/// turn 调用 [`Translator::set_turn`] 再继续喂事件。
pub struct Translator {
    thread_id: String,
    turn_id: String,
    next_item: u64,
    active_agent_msg: Option<ActiveAgentMsg>,
    tool_items: HashMap<String, ToolItem>,
}

impl Translator {
    /// `thread_id` is stamped onto every emitted notification.
    pub fn new(thread_id: String) -> Self {
        Self {
            thread_id,
            turn_id: String::new(),
            next_item: 1,
            active_agent_msg: None,
            tool_items: HashMap::new(),
        }
    }

    /// Set the active turn id (called by the server when a turn starts).
    ///
    /// Notifications that carry `turn_id` use this value. Emits nothing.
    pub fn set_turn(&mut self, turn_id: String) {
        self.turn_id = turn_id;
    }

    fn alloc_item_id(&mut self) -> String {
        let id = format!("item-{}", self.next_item);
        self.next_item += 1;
        id
    }

    /// Finalize any open agent message, pushing an `item/completed` if present.
    fn finalize_agent_msg(&mut self, out: &mut Vec<Notification>) {
        if let Some(msg) = self.active_agent_msg.take() {
            out.push(Notification::ItemCompleted {
                thread_id: self.thread_id.clone(),
                item: Item::AgentMessage {
                    id: msg.item_id,
                    text: msg.text,
                },
            });
        }
    }

    /// Open the agent message if needed, append `s`, and emit the delta.
    fn append_agent_text(&mut self, s: String, out: &mut Vec<Notification>) {
        if self.active_agent_msg.is_none() {
            let item_id = self.alloc_item_id();
            out.push(Notification::ItemStarted {
                thread_id: self.thread_id.clone(),
                item: Item::AgentMessage {
                    id: item_id.clone(),
                    text: String::new(),
                },
            });
            self.active_agent_msg = Some(ActiveAgentMsg {
                item_id,
                text: String::new(),
            });
        }
        if let Some(msg) = self.active_agent_msg.as_mut() {
            msg.text.push_str(&s);
            out.push(Notification::ItemDelta {
                thread_id: self.thread_id.clone(),
                item_id: msg.item_id.clone(),
                delta: s,
            });
        }
    }

    /// Mark a tool item completed, emitting `item/completed` exactly once.
    ///
    /// The entry is removed from `tool_items`, so a second completion (or a
    /// late `ToolOutputDelta`) for the same `call_id` is automatically a no-op.
    fn complete_tool(
        &mut self,
        call_id: &str,
        status: ToolStatus,
        result: Option<String>,
        out: &mut Vec<Notification>,
    ) {
        let Some(tool) = self.tool_items.remove(call_id) else {
            return;
        };
        out.push(Notification::ItemCompleted {
            thread_id: self.thread_id.clone(),
            item: Item::ToolCall {
                id: tool.item_id,
                call_id: call_id.to_string(),
                name: tool.name,
                input: tool.input,
                status,
                result,
            },
        });
    }

    /// Close every still-open tool item with `status`.
    ///
    /// Terminal events (`Done` / `Cancelled` / `Error`) can arrive while tool
    /// futures are still in flight (e.g. a cancelled turn drops them before a
    /// `ToolResult` is emitted). This sweep guarantees the UI never leaves a
    /// tool card stuck on `running`.
    fn finalize_open_tools(&mut self, status: ToolStatus, out: &mut Vec<Notification>) {
        let open: Vec<String> = self.tool_items.keys().cloned().collect();
        for call_id in open {
            self.complete_tool(&call_id, status.clone(), None, out);
        }
    }

    /// Close the turn: finalize any open items, then emit `turn/completed`.
    fn finish_turn(
        &mut self,
        status: TurnStatus,
        error: Option<String>,
        out: &mut Vec<Notification>,
    ) {
        self.finalize_agent_msg(out);
        self.finalize_open_tools(ToolStatus::Failed, out);
        out.push(Notification::TurnCompleted {
            thread_id: self.thread_id.clone(),
            turn_id: self.turn_id.clone(),
            status,
            error,
        });
    }

    /// Translate one agent event into zero or more protocol notifications.
    ///
    /// After a terminal event (`Done` / `Cancelled` / `Error`) all open items
    /// are closed and `tool_items` is empty; call [`Translator::set_turn`] for
    /// the next turn before feeding further events.
    pub fn on_event(&mut self, ev: AgentEvent) -> Vec<Notification> {
        let mut out = Vec::new();
        match ev {
            AgentEvent::AssistantText(s) | AgentEvent::DecodeDelta(s) => {
                if !s.is_empty() {
                    self.append_agent_text(s, &mut out);
                }
            }
            AgentEvent::ToolCall { id, name, input } => {
                self.finalize_agent_msg(&mut out);
                let item_id = self.alloc_item_id();
                // Overwrites any existing entry with the same `call_id`; provider
                // tool ids are unique in practice.
                self.tool_items.insert(
                    id.clone(),
                    ToolItem {
                        item_id: item_id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    },
                );
                out.push(Notification::ItemStarted {
                    thread_id: self.thread_id.clone(),
                    item: Item::ToolCall {
                        id: item_id,
                        call_id: id,
                        name,
                        input,
                        status: ToolStatus::Running,
                        result: None,
                    },
                });
            }
            AgentEvent::ToolOutputDelta { id, text, .. } => {
                if let Some(tool) = self.tool_items.get(&id) {
                    out.push(Notification::ItemDelta {
                        thread_id: self.thread_id.clone(),
                        item_id: tool.item_id.clone(),
                        delta: text,
                    });
                }
            }
            AgentEvent::ToolResult { id, result } => {
                let status = if result.is_error {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Completed
                };
                let rendered = render_content(&result.content);
                self.complete_tool(&id, status, Some(rendered), &mut out);
            }
            AgentEvent::ToolExit { id, code } => {
                let status = if code == Some(0) {
                    ToolStatus::Completed
                } else {
                    ToolStatus::Failed
                };
                self.complete_tool(&id, status, None, &mut out);
            }
            AgentEvent::ToolTimeout { id } => {
                self.complete_tool(&id, ToolStatus::Failed, None, &mut out);
            }
            AgentEvent::Usage { model, usage } => {
                out.push(Notification::TokenUsage {
                    thread_id: self.thread_id.clone(),
                    model,
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                });
            }
            AgentEvent::Done { reason } => {
                let (status, error) = match reason {
                    DoneReason::EndTurn | DoneReason::MaxTurns => (TurnStatus::Completed, None),
                    DoneReason::Interrupted { reason } => (TurnStatus::Interrupted, Some(reason)),
                };
                self.finish_turn(status, error, &mut out);
            }
            AgentEvent::Cancelled => {
                self.finish_turn(TurnStatus::Interrupted, None, &mut out);
            }
            AgentEvent::Error(e) => {
                self.finish_turn(TurnStatus::Failed, Some(e.to_string()), &mut out);
            }
            AgentEvent::ProviderRetry { attempt, max, .. } => {
                // The desktop client shows this so a stalled stream does not
                // look like a hang during the backoff window.
                out.push(Notification::TurnRetry {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    attempt,
                    max,
                });
            }
            // 暂不产生通知的事件。
            //
            // 翻译层有意忽略 `PermissionRequest` / `PermissionResolved`:
            // `PermissionRequest` 由 server 的 driver 直接拦截并发出反向请求
            // `item/toolCall/requestApproval`(不走 translator);`PermissionResolved`
            // 在 baseline 中保持 no-op。此处不 panic。
            AgentEvent::Start
            | AgentEvent::ToolRetry { .. }
            | AgentEvent::EstimatedPrefill(_)
            | AgentEvent::AutoCompacting { .. }
            | AgentEvent::ManualCompacted { .. }
            | AgentEvent::ManualCompactFailed { .. }
            | AgentEvent::PermissionRequest { .. }
            | AgentEvent::PermissionResolved { .. } => {}
        }
        out
    }
}

/// Render tool-result content blocks into a single display string.
///
/// Text blocks are concatenated verbatim; non-text blocks become a short
/// placeholder so the UI still shows that something was there.
fn render_content(blocks: &[ContentBlock]) -> String {
    let mut rendered = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => rendered.push_str(text),
            ContentBlock::ToolUse { .. } => rendered.push_str("[tool_use]"),
            ContentBlock::ToolResult { .. } => rendered.push_str("[tool_result]"),
            ContentBlock::Image { .. } => rendered.push_str("[image]"),
        }
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Item, ToolStatus, TurnStatus};
    use serde_json::json;
    use yi_agent_core::AgentEvent;
    use yi_agent_core::agent::{AgentError, DoneReason};
    use yi_agent_core::permission::PermissionKind;
    use yi_agent_core::provider::TokenUsage;
    use yi_agent_core::tool::ToolResult;

    fn translator() -> Translator {
        let mut t = Translator::new("th1".into());
        t.set_turn("turn1".into());
        t
    }

    /// Extract the item id from a leading `ItemStarted { AgentMessage }`.
    fn agent_msg_id(ns: &[Notification]) -> String {
        match ns.first() {
            Some(Notification::ItemStarted {
                item: Item::AgentMessage { id, .. },
                ..
            }) => id.clone(),
            other => panic!("expected ItemStarted AgentMessage, got {other:?}"),
        }
    }

    #[test]
    fn provider_retry_becomes_turn_retry_notification() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::ProviderRetry {
            attempt: 1,
            max: 3,
            idle_secs: 60,
        });
        assert!(
            matches!(
                out.as_slice(),
                [Notification::TurnRetry {
                    attempt: 1,
                    max: 3,
                    ..
                }]
            ),
            "expected TurnRetry, got: {out:?}"
        );
    }

    #[test]
    fn assistant_text_starts_agent_message() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::AssistantText("hello".into()));
        assert!(
            matches!(
                out.first(),
                Some(Notification::ItemStarted {
                    item: Item::AgentMessage { .. },
                    ..
                })
            ),
            "first notification should open an agentMessage, got {out:?}"
        );
    }

    #[test]
    fn assistant_text_also_emits_delta() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::AssistantText("hello".into()));
        assert!(
            out.iter().any(|n| matches!(
                n,
                Notification::ItemDelta { delta, .. } if delta == "hello"
            )),
            "expected ItemDelta with delta == hello, got {out:?}"
        );
    }

    #[test]
    fn decode_delta_emits_item_delta() {
        let mut t = translator();
        let first = t.on_event(AgentEvent::AssistantText("hi".into()));
        let item_id = agent_msg_id(&first);

        let out = t.on_event(AgentEvent::DecodeDelta(" world".into()));
        assert_eq!(out.len(), 1, "decode delta should only emit ItemDelta");
        match &out[0] {
            Notification::ItemDelta {
                item_id: got_id,
                delta,
                ..
            } => {
                assert_eq!(got_id, &item_id, "decode delta must reuse the same item");
                assert_eq!(delta, " world");
            }
            other => panic!("expected ItemDelta, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_starts_tool_item() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            input: json!({"cmd": "ls"}),
        });
        assert_eq!(out.len(), 1);
        match &out[0] {
            Notification::ItemStarted {
                item:
                    Item::ToolCall {
                        call_id,
                        name,
                        status,
                        result,
                        ..
                    },
                ..
            } => {
                assert_eq!(call_id, "c1");
                assert_eq!(name, "bash");
                assert!(matches!(status, ToolStatus::Running));
                assert!(result.is_none());
            }
            other => panic!("expected ItemStarted ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_completes_item() {
        let mut t = translator();
        t.on_event(AgentEvent::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            input: json!({}),
        });
        let out = t.on_event(AgentEvent::ToolResult {
            id: "c1".into(),
            result: ToolResult::text("ok"),
        });
        assert_eq!(out.len(), 1);
        match &out[0] {
            Notification::ItemCompleted {
                item: Item::ToolCall { status, result, .. },
                ..
            } => {
                assert!(matches!(status, ToolStatus::Completed));
                assert_eq!(result.as_deref(), Some("ok"));
            }
            other => panic!("expected ItemCompleted ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_error_maps_to_failed() {
        let mut t = translator();
        t.on_event(AgentEvent::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            input: json!({}),
        });
        let out = t.on_event(AgentEvent::ToolResult {
            id: "c1".into(),
            result: ToolResult::error("nope"),
        });
        match &out[0] {
            Notification::ItemCompleted {
                item: Item::ToolCall { status, .. },
                ..
            } => assert!(matches!(status, ToolStatus::Failed)),
            other => panic!("expected ItemCompleted ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn tool_output_delta_emits_item_delta() {
        let mut t = translator();
        let started = t.on_event(AgentEvent::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            input: json!({}),
        });
        let tool_item_id = match &started[0] {
            Notification::ItemStarted {
                item: Item::ToolCall { id, .. },
                ..
            } => id.clone(),
            other => panic!("expected ItemStarted ToolCall, got {other:?}"),
        };

        let out = t.on_event(AgentEvent::ToolOutputDelta {
            id: "c1".into(),
            stream: yi_agent_core::tool::OutputStream::Stdout,
            text: "chunk".into(),
        });
        assert_eq!(out.len(), 1);
        match &out[0] {
            Notification::ItemDelta { item_id, delta, .. } => {
                assert_eq!(item_id, &tool_item_id);
                assert_eq!(delta, "chunk");
            }
            other => panic!("expected ItemDelta, got {other:?}"),
        }
    }

    #[test]
    fn tool_output_delta_for_unknown_id_is_noop() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::ToolOutputDelta {
            id: "missing".into(),
            stream: yi_agent_core::tool::OutputStream::Stdout,
            text: "chunk".into(),
        });
        assert!(out.is_empty());
    }

    #[test]
    fn tool_exit_zero_completes_item() {
        let mut t = translator();
        t.on_event(AgentEvent::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            input: json!({}),
        });
        let out = t.on_event(AgentEvent::ToolExit {
            id: "c1".into(),
            code: Some(0),
        });
        match &out[0] {
            Notification::ItemCompleted {
                item: Item::ToolCall { status, .. },
                ..
            } => assert!(matches!(status, ToolStatus::Completed)),
            other => panic!("expected ItemCompleted ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn tool_exit_nonzero_fails_item() {
        let mut t = translator();
        t.on_event(AgentEvent::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            input: json!({}),
        });
        let out = t.on_event(AgentEvent::ToolExit {
            id: "c1".into(),
            code: Some(2),
        });
        match &out[0] {
            Notification::ItemCompleted {
                item: Item::ToolCall { status, .. },
                ..
            } => assert!(matches!(status, ToolStatus::Failed)),
            other => panic!("expected ItemCompleted ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_then_exit_does_not_double_complete() {
        let mut t = translator();
        t.on_event(AgentEvent::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            input: json!({}),
        });
        let first = t.on_event(AgentEvent::ToolResult {
            id: "c1".into(),
            result: ToolResult::text("ok"),
        });
        assert_eq!(first.len(), 1);
        let second = t.on_event(AgentEvent::ToolExit {
            id: "c1".into(),
            code: Some(0),
        });
        assert!(second.is_empty(), "already-completed tool must be a no-op");
    }

    #[test]
    fn tool_timeout_fails_item() {
        let mut t = translator();
        t.on_event(AgentEvent::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            input: json!({}),
        });
        let out = t.on_event(AgentEvent::ToolTimeout { id: "c1".into() });
        match &out[0] {
            Notification::ItemCompleted {
                item: Item::ToolCall { status, .. },
                ..
            } => assert!(matches!(status, ToolStatus::Failed)),
            other => panic!("expected ItemCompleted ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn done_end_turn_completes() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::Done {
            reason: DoneReason::EndTurn,
        });
        assert_eq!(out.len(), 1);
        match &out[0] {
            Notification::TurnCompleted { status, error, .. } => {
                assert!(matches!(status, TurnStatus::Completed));
                assert!(error.is_none());
            }
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    #[test]
    fn done_interrupted_maps_to_interrupted_with_reason() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::Done {
            reason: DoneReason::Interrupted {
                reason: "user stop".into(),
            },
        });
        match &out[0] {
            Notification::TurnCompleted { status, error, .. } => {
                assert!(matches!(status, TurnStatus::Interrupted));
                assert_eq!(error.as_deref(), Some("user stop"));
            }
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    #[test]
    fn cancelled_completes_interrupted() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::Cancelled);
        match &out[0] {
            Notification::TurnCompleted { status, error, .. } => {
                assert!(matches!(status, TurnStatus::Interrupted));
                assert!(error.is_none());
            }
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    #[test]
    fn error_completes_failed() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::Error(AgentError::ProviderTurnAdmission(
            "boom".into(),
        )));
        match &out[0] {
            Notification::TurnCompleted { status, error, .. } => {
                assert!(matches!(status, TurnStatus::Failed));
                assert!(
                    error.as_deref().is_some_and(|e| e.contains("boom")),
                    "error should carry the agent error text, got {error:?}"
                );
            }
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    #[test]
    fn done_finalizes_open_agent_message_first() {
        let mut t = translator();
        t.on_event(AgentEvent::AssistantText("hello".into()));
        let out = t.on_event(AgentEvent::Done {
            reason: DoneReason::EndTurn,
        });
        assert_eq!(out.len(), 2, "expected ItemCompleted then TurnCompleted");
        match &out[0] {
            Notification::ItemCompleted {
                item: Item::AgentMessage { text, .. },
                ..
            } => assert_eq!(text, "hello"),
            other => panic!("expected ItemCompleted AgentMessage, got {other:?}"),
        }
        assert!(matches!(out[1], Notification::TurnCompleted { .. }));
    }

    #[test]
    fn usage_emits_token_usage() {
        let mut t = translator();
        let usage = TokenUsage {
            input_tokens: 3,
            output_tokens: 5,
            ..Default::default()
        };
        let out = t.on_event(AgentEvent::Usage {
            model: "m".into(),
            usage,
        });
        assert_eq!(out.len(), 1);
        match &out[0] {
            Notification::TokenUsage {
                model,
                input_tokens,
                output_tokens,
                ..
            } => {
                assert_eq!(model, "m");
                assert_eq!(*input_tokens, 3);
                assert_eq!(*output_tokens, 5);
            }
            other => panic!("expected TokenUsage, got {other:?}"),
        }
    }

    #[test]
    fn permission_request_is_noop_in_translator() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::PermissionRequest {
            request_id: 1,
            tool_name: "bash".into(),
            tool_input: json!({}),
            prefix_suggestion: None,
            kind: PermissionKind::Normal,
        });
        assert!(out.is_empty());
    }

    #[test]
    fn permission_resolved_is_noop_in_translator() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::PermissionResolved {
            request_id: 1,
            decision: yi_agent_core::permission::Decision::AllowOnce,
        });
        assert!(out.is_empty());
    }

    #[test]
    fn benign_events_are_noops() {
        let mut t = translator();
        assert!(t.on_event(AgentEvent::Start).is_empty());
        assert!(
            t.on_event(AgentEvent::ToolRetry { id: "c1".into() })
                .is_empty()
        );
        assert!(t.on_event(AgentEvent::EstimatedPrefill(10)).is_empty());
        assert!(
            t.on_event(AgentEvent::AutoCompacting {
                old_msg_count: 9,
                new_msg_count: 3,
            })
            .is_empty()
        );
        assert!(
            t.on_event(AgentEvent::ManualCompacted {
                old_msg_count: 9,
                new_msg_count: 3,
            })
            .is_empty()
        );
        assert!(
            t.on_event(AgentEvent::ManualCompactFailed {
                message: "nope".into(),
            })
            .is_empty()
        );
    }

    #[test]
    fn set_turn_stamps_turn_id() {
        let mut t = Translator::new("th1".into());
        t.set_turn("u1".into());
        let out = t.on_event(AgentEvent::Done {
            reason: DoneReason::EndTurn,
        });
        match &out[0] {
            Notification::TurnCompleted { turn_id, .. } => assert_eq!(turn_id, "u1"),
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_finalizes_open_agent_message_first() {
        let mut t = translator();
        t.on_event(AgentEvent::AssistantText("hi".into()));
        let out = t.on_event(AgentEvent::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            input: json!({}),
        });
        assert_eq!(out.len(), 2, "expected ItemCompleted then ItemStarted");
        match &out[0] {
            Notification::ItemCompleted {
                item: Item::AgentMessage { text, .. },
                ..
            } => assert_eq!(text, "hi"),
            other => panic!("expected ItemCompleted AgentMessage, got {other:?}"),
        }
        assert!(matches!(
            out[1],
            Notification::ItemStarted {
                item: Item::ToolCall { .. },
                ..
            }
        ));
    }

    #[test]
    fn done_max_turns_completes() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::Done {
            reason: DoneReason::MaxTurns,
        });
        assert_eq!(out.len(), 1);
        match &out[0] {
            Notification::TurnCompleted { status, error, .. } => {
                assert!(matches!(status, TurnStatus::Completed));
                assert!(error.is_none());
            }
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    #[test]
    fn cancelled_finalizes_open_agent_message() {
        let mut t = translator();
        t.on_event(AgentEvent::AssistantText("hi".into()));
        let out = t.on_event(AgentEvent::Cancelled);
        assert_eq!(out.len(), 2, "expected ItemCompleted then TurnCompleted");
        match &out[0] {
            Notification::ItemCompleted {
                item: Item::AgentMessage { text, .. },
                ..
            } => assert_eq!(text, "hi"),
            other => panic!("expected ItemCompleted AgentMessage, got {other:?}"),
        }
        match &out[1] {
            Notification::TurnCompleted { status, .. } => {
                assert!(matches!(status, TurnStatus::Interrupted));
            }
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    #[test]
    fn error_finalizes_open_agent_message() {
        let mut t = translator();
        t.on_event(AgentEvent::AssistantText("hi".into()));
        let out = t.on_event(AgentEvent::Error(AgentError::ProviderTurnAdmission(
            "boom".into(),
        )));
        assert_eq!(out.len(), 2, "expected ItemCompleted then TurnCompleted");
        match &out[0] {
            Notification::ItemCompleted {
                item: Item::AgentMessage { text, .. },
                ..
            } => assert_eq!(text, "hi"),
            other => panic!("expected ItemCompleted AgentMessage, got {other:?}"),
        }
        match &out[1] {
            Notification::TurnCompleted { status, error, .. } => {
                assert!(matches!(status, TurnStatus::Failed));
                assert!(
                    error.as_deref().is_some_and(|e| e.contains("boom")),
                    "error should carry the agent error text, got {error:?}"
                );
            }
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    #[test]
    fn cancelled_closes_open_tool_items() {
        let mut t = translator();
        t.on_event(AgentEvent::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            input: json!({}),
        });
        let out = t.on_event(AgentEvent::Cancelled);
        assert!(
            out.iter().any(|n| matches!(
                n,
                Notification::ItemCompleted {
                    item: Item::ToolCall {
                        call_id,
                        status: ToolStatus::Failed,
                        result: None,
                        ..
                    },
                    ..
                } if call_id == "c1"
            )),
            "expected ItemCompleted Failed for call_id c1, got {out:?}"
        );
        assert!(
            out.iter().any(|n| matches!(
                n,
                Notification::TurnCompleted {
                    status: TurnStatus::Interrupted,
                    ..
                }
            )),
            "expected TurnCompleted Interrupted, got {out:?}"
        );
    }

    #[test]
    fn error_closes_open_tool_items() {
        let mut t = translator();
        t.on_event(AgentEvent::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            input: json!({}),
        });
        let out = t.on_event(AgentEvent::Error(AgentError::ProviderTurnAdmission(
            "boom".into(),
        )));
        assert!(
            out.iter().any(|n| matches!(
                n,
                Notification::ItemCompleted {
                    item: Item::ToolCall {
                        call_id,
                        status: ToolStatus::Failed,
                        ..
                    },
                    ..
                } if call_id == "c1"
            )),
            "expected ItemCompleted Failed for call_id c1, got {out:?}"
        );
    }

    #[test]
    fn tool_output_delta_after_completion_is_noop() {
        let mut t = translator();
        t.on_event(AgentEvent::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            input: json!({}),
        });
        t.on_event(AgentEvent::ToolResult {
            id: "c1".into(),
            result: ToolResult::text("ok"),
        });
        let out = t.on_event(AgentEvent::ToolOutputDelta {
            id: "c1".into(),
            stream: yi_agent_core::tool::OutputStream::Stdout,
            text: "late".into(),
        });
        assert!(
            out.is_empty(),
            "late ToolOutputDelta for a completed call must be a no-op, got {out:?}"
        );
    }

    #[test]
    fn empty_assistant_text_is_noop() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::AssistantText(String::new()));
        assert!(out.is_empty(), "empty delta must not open a message");
    }
}
