//! `AgentEvent` → 线协议通知的翻译层。
//!
//! [`Translator`] 是纯同步、无 I/O 的状态机:调用方把 `yi_agent_core::AgentEvent`
//! 逐个喂进 [`Translator::on_event`],拿回零个或多个 [`Notification`]。
//! 它只负责事件翻译,不负责发帧——发帧由上层 server 主循环通过
//! `transport::MessageWriter` 完成。

use std::collections::HashMap;

use serde_json::Value;
use yi_agent_core::message::{ContentBlock, ImageSource, ThinkingToken};
use yi_agent_core::{AgentEvent, DoneReason, TokenUsage};

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

/// 单个 turn 内的用量快照。
///
/// Anthropic 把一次 provider 调用拆成两个事件:`message_start` 带 input/cache,
/// `message_delta` 带 output。翻译层把一次调用的多个事件合并成完整快照
/// (新调用整体替换、同一调用内按字段补齐);多步 turn 下即最后一次调用的快照。
#[derive(Default)]
struct UsageSnapshot {
    model: String,
    input_tokens: u32,
    output_tokens: u32,
    cache_creation_input_tokens: u32,
    cache_read_input_tokens: u32,
}

impl UsageSnapshot {
    /// 字段级合并:仅在事件携带非零值 / `Some` 时覆盖,使拆分事件可拼回完整快照。
    ///
    /// `input_tokens > 0` 标识一次新的 provider 调用(`message_start`,或 OpenAI 的
    /// 单条合并事件):此时整体替换,避免上一次调用的字段残留。同一次调用的后续
    /// 事件(`message_delta`,只带 output)按字段合并。
    ///
    /// 假设一次新调用必带非零 `input_tokens`(Anthropic / OpenAI 均如此);若某网关
    /// 省略该字段,会被误判为同一调用的后续事件。
    fn merge(&mut self, model: &str, usage: &TokenUsage) {
        self.model = model.to_string();
        if usage.input_tokens > 0 {
            // 新的一次调用:整体替换,防止上一调用字段残留。
            self.input_tokens = usage.input_tokens;
            self.output_tokens = usage.output_tokens;
            self.cache_creation_input_tokens = usage.cache_creation_input_tokens.unwrap_or(0);
            self.cache_read_input_tokens = usage.cache_read_input_tokens.unwrap_or(0);
        } else {
            // 同一次调用的后续事件:只覆盖事件携带的字段。
            if usage.output_tokens > 0 {
                self.output_tokens = usage.output_tokens;
            }
            if let Some(v) = usage.cache_creation_input_tokens {
                self.cache_creation_input_tokens = v;
            }
            if let Some(v) = usage.cache_read_input_tokens {
                self.cache_read_input_tokens = v;
            }
        }
    }
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
///
/// `TokenUsage` 通知携带的是**本轮累积快照**:provider 可能把一次调用的用量
/// 拆成多个事件(Anthropic 的 `message_start` 带 input/cache、`message_delta`
/// 带 output),翻译层合并后发出(新调用整体替换、同一调用内按字段补齐),
/// 故每条通知都是迄今完整的本轮用量。
pub struct Translator {
    thread_id: String,
    turn_id: String,
    next_item: u64,
    active_agent_msg: Option<ActiveAgentMsg>,
    tool_items: HashMap<String, ToolItem>,
    usage: UsageSnapshot,
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
            usage: UsageSnapshot::default(),
        }
    }

    /// Set the active turn id (called by the server when a turn starts).
    ///
    /// Notifications that carry `turn_id` use this value. Emits nothing.
    pub fn set_turn(&mut self, turn_id: String) {
        self.turn_id = turn_id;
        self.usage = UsageSnapshot::default();
    }

    fn alloc_item_id(&mut self) -> String {
        let id = format!("item-{}-{}", self.turn_id, self.next_item);
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
    ///
    /// `images` carries only *references* to tool-result images (never the
    /// base64 payload); see [`image_refs_from`].
    fn complete_tool(
        &mut self,
        call_id: &str,
        status: ToolStatus,
        result: Option<String>,
        images: Vec<crate::protocol::ImageRef>,
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
                images,
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
            self.complete_tool(&call_id, status.clone(), None, Vec::new(), out);
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
            AgentEvent::AssistantText(s) => {
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
                        images: Vec::new(),
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
                let images = image_refs_from(&result.content);
                self.complete_tool(&id, status, Some(rendered), images, &mut out);
            }
            AgentEvent::ToolExit { id, code } => {
                let status = if code == Some(0) {
                    ToolStatus::Completed
                } else {
                    ToolStatus::Failed
                };
                self.complete_tool(&id, status, None, Vec::new(), &mut out);
            }
            AgentEvent::ToolTimeout { id } => {
                self.complete_tool(&id, ToolStatus::Failed, None, Vec::new(), &mut out);
            }
            AgentEvent::ModelChanged { model } => {
                // Design §5.3 step 4: the driver rebuilt the agent around a new
                // model; surface the authoritative "now using this model" string
                // so clients can update without polling.
                out.push(Notification::ModelChanged {
                    thread_id: self.thread_id.clone(),
                    model,
                });
            }
            AgentEvent::ReasoningDelta { text, kind } => {
                // Display-only CoT. It is deliberately NOT merged into the open
                // agent message: a client renders it in a separate block
                // (design §6). The block-level credential is a different
                // channel — see the `ThinkingBlock` arm below.
                out.push(Notification::ReasoningDelta {
                    thread_id: self.thread_id.clone(),
                    text,
                    kind,
                });
            }
            AgentEvent::ThinkingBlock { text, token } => {
                // 块级凭证已在 `accumulate_stream` 进入 session，并随 `TurnLine.messages`
                // 落盘（server.rs 直接取 `agent.session().messages()`），故此处**无需**再
                // 持久化任何东西——本任务只接入展示通道，不改变落盘行为。
                //
                // 展示：明文增量由 `ReasoningDelta` 走 `thread/reasoningDelta` 实时下发；
                // 这里额外产出一个**不携带凭证**的 `Item::Thinking`（design §6.3），
                // 让客户端在历史回放期也能渲染「思考」卡片（实时期前端会用它接管
                // 已打开的本地推理块）。凭证（signature / encrypted_content / redacted
                // data）**绝不**进入该 item——`redacted` 只暴露"这是占位"这一位信息。
                //
                // 本分支绝不打开/追加 agentMessage：那会把 CoT 当答案正文（既有测试
                // `reasoning_delta_becomes_a_reasoning_notification` 钉住了这一点）。
                let redacted = matches!(token, ThinkingToken::Redacted(_));
                let id = self.alloc_item_id();
                let item = Item::Thinking { id, text, redacted };
                out.push(Notification::ItemStarted {
                    thread_id: self.thread_id.clone(),
                    item: item.clone(),
                });
                out.push(Notification::ItemCompleted {
                    thread_id: self.thread_id.clone(),
                    item,
                });
            }
            AgentEvent::Notice { level, text } => {
                // A non-fatal runtime notice (channel fallback / effort
                // downgrade). Status-line level by design §7: the client must
                // not render it as a turn failure. No barrier flush is needed —
                // the server flushes pending deltas on any non-delta frame.
                out.push(Notification::Notice {
                    thread_id: self.thread_id.clone(),
                    level,
                    text,
                });
            }
            AgentEvent::Usage { model, usage } => {
                self.usage.merge(&model, &usage);
                out.push(Notification::TokenUsage {
                    thread_id: self.thread_id.clone(),
                    model: self.usage.model.clone(),
                    input_tokens: self.usage.input_tokens,
                    output_tokens: self.usage.output_tokens,
                    cache_creation_input_tokens: self.usage.cache_creation_input_tokens,
                    cache_read_input_tokens: self.usage.cache_read_input_tokens,
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
            AgentEvent::InterjectionAccepted { text, tag, .. } => {
                // `tag` is the interjection id minted at the RPC boundary; fall
                // back to the item namespace when a caller supplied none (the
                // TUI does not need cross-process matching).
                let id = tag.unwrap_or_else(|| self.alloc_item_id());
                let item = crate::protocol::Item::UserInterjection {
                    id,
                    text,
                    attachments: Vec::new(),
                    images: Vec::new(),
                };
                out.push(Notification::ItemStarted {
                    thread_id: self.thread_id.clone(),
                    item: item.clone(),
                });
                out.push(Notification::ItemCompleted {
                    thread_id: self.thread_id.clone(),
                    item,
                });
            }
            AgentEvent::InterjectionsReturned { items } => {
                out.push(Notification::InterjectionsReturned {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    items: items
                        .into_iter()
                        .map(|i| i.tag.unwrap_or_default())
                        .collect(),
                });
            }
            AgentEvent::Error(e) => {
                self.finish_turn(TurnStatus::Failed, Some(e.to_string()), &mut out);
            }
            AgentEvent::ProviderRetry {
                attempt,
                max,
                cause,
                ..
            } => {
                // The desktop client shows this so a stalled stream does not
                // look like a hang during the backoff window. The cause lets it
                // word the notice accurately.
                out.push(Notification::TurnRetry {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    attempt,
                    max,
                    cause: match cause {
                        yi_agent_core::RetryCause::IdleStall => "idle_stall".to_string(),
                        yi_agent_core::RetryCause::RequestTimeout => "request_timeout".to_string(),
                        yi_agent_core::RetryCause::RateLimited => "rate_limited".to_string(),
                        yi_agent_core::RetryCause::ServerError => "server_error".to_string(),
                        yi_agent_core::RetryCause::NetworkError => "network_error".to_string(),
                    },
                });
            }
            // 暂不产生通知的事件。
            //
            // 翻译层有意忽略 `PermissionRequest` / `PermissionResolved`:
            // `PermissionRequest` 由 server 的 driver 直接拦截并发出反向请求
            // `item/toolCall/requestApproval`(不走 translator);`PermissionResolved`
            // 在 baseline 中保持 no-op。此处不 panic。
            //
            // `DecodeDelta` 是**工具调用入参**的流式 JSON 分片(`agent.rs` 把
            // `ProviderEvent::ToolUseDelta.partial_json` 原样转发),不是助手散文。
            // 若当成 agent 正文渲染,同一段命令会在正文里出现一次、又在随后的
            // `toolCall` 卡片里出现一次(且分片到达时会显示成半截 JSON)。TUI
            // (`tui/history.rs` 的 `DecodeDelta => 不跟踪`)与 headless drain 都
            // 有意忽略它,这里同样忽略。
            // App-server has no runtime bring-up of its own, so this event is
            // unreachable from its driver; a non-exhaustive arm would silently
            // swallow a future producer's notice.
            AgentEvent::SubagentRuntimeUnavailable { .. } => {
                tracing::warn!("subagent runtime unavailable for this session");
            }
            // The desktop side has no reclaim notice of its own: only the CLI
            // emits this event (the shared attach path used by the app-server
            // stays silent and logs instead), so the arm is defensive rather
            // than reachable. It logs rather than vanishing, so a future
            // producer's notice cannot be swallowed without a trace.
            AgentEvent::OrphanedTasksReclaimed { count } => {
                tracing::info!(count, "reclaimed orphaned subagent tasks");
            }
            AgentEvent::Start
            | AgentEvent::ToolRetry { .. }
            | AgentEvent::EstimatedPrefill(_)
            | AgentEvent::AutoCompacting { .. }
            | AgentEvent::ManualCompacted { .. }
            | AgentEvent::ManualCompactFailed { .. }
            | AgentEvent::DecodeDelta(_)
            | AgentEvent::PermissionRequest { .. }
            | AgentEvent::PermissionResolved { .. } => {}
        }
        out
    }
}

/// Collect the *references* to images carried by tool-result content blocks.
///
/// A `ContentBlock::Image` holds a base64 payload; nothing of that payload may
/// reach the wire (items carry refs, and the UI fetches bytes separately over
/// `image/read`). Only the image's on-disk `path` and `media_type` are kept.
///
/// A block without a `path` is not on-disk-referencable (the UI could not locate
/// it), so it produces no ref. `size` is `0`: the tool side that produced the
/// image does not know the original file's size, and the ref consumer does not
/// need it — inventing a value would be worse than the sentinel.
fn image_refs_from(blocks: &[ContentBlock]) -> Vec<crate::protocol::ImageRef> {
    let mut out = Vec::new();
    for block in blocks {
        if let ContentBlock::Image {
            source,
            path: Some(rel),
            ..
        } = block
        {
            let media_type = match source {
                ImageSource::Base64 { media_type, .. } => media_type.clone(),
                _ => "image/*".to_string(),
            };
            out.push(crate::protocol::ImageRef {
                path: rel.clone(),
                media_type,
                size: 0,
                detail: None,
            });
        }
    }
    out
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
            ContentBlock::Thinking { .. } => rendered.push_str("[thinking]"),
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
    use yi_agent_core::message::{ImageDetail, ImageSource, ThinkingToken};
    use yi_agent_core::permission::PermissionKind;
    use yi_agent_core::provider::TokenUsage;
    use yi_agent_core::tool::ToolResult;

    fn translator() -> Translator {
        let mut t = Translator::new("th1".into());
        t.set_turn("turn1".into());
        t
    }

    /// App-server runs no runtime bring-up of its own, so this must not be
    /// mistaken for a turn failure (which would abort a turn the user can
    /// actually continue).
    #[test]
    fn subagent_runtime_unavailable_is_not_a_turn_failure() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::SubagentRuntimeUnavailable {
            stage: "runtime attach".into(),
            cause: "file is not a database".into(),
        });
        assert!(out.is_empty(), "unexpected notifications: {out:?}");
    }

    #[test]
    fn provider_retry_becomes_turn_retry_notification() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::ProviderRetry {
            attempt: 1,
            max: 3,
            idle_secs: 60,
            cause: yi_agent_core::RetryCause::RequestTimeout,
        });
        assert!(
            matches!(
                out.as_slice(),
                [Notification::TurnRetry {
                    attempt: 1,
                    max: 3,
                    cause,
                    ..
                }] if cause == "request_timeout"
            ),
            "expected TurnRetry carrying the cause, got: {out:?}"
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
    fn decode_delta_is_not_rendered_as_agent_text() {
        // `DecodeDelta` carries streamed tool-call *arguments* JSON, not
        // assistant prose. Rendering it as an agentMessage duplicated the
        // command in the UI (once as text, once in the tool card). The TUI and
        // the headless drain both ignore it; the translator must too.
        let mut t = translator();
        t.on_event(AgentEvent::AssistantText("hi".into()));
        let out = t.on_event(AgentEvent::DecodeDelta("{\"command\":\"ls\"}".into()));
        assert!(
            out.is_empty(),
            "decode delta must emit nothing, got {out:?}"
        );
    }

    #[test]
    fn decode_delta_alone_does_not_open_agent_message() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::DecodeDelta("{\"command\":\"ls\"}".into()));
        assert!(
            out.is_empty(),
            "decode delta must not open an agentMessage, got {out:?}"
        );
    }

    /// 推理增量必须变成 `thread/reasoningDelta`，并带上本 translator 的 thread。
    ///
    /// 它**不能**打开/追加 agentMessage：CoT 是仅展示的旁路，翻译进正文会让
    /// 客户端把思考当答案，也会让它被后续轮次回灌（design §6.3 明确不做）。
    #[test]
    fn reasoning_delta_becomes_a_reasoning_notification() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::AssistantText("hi".into()));
        assert!(
            out.iter()
                .any(|n| matches!(n, Notification::ItemStarted { .. })),
            "precondition: an agent message is open"
        );

        let out = t.on_event(AgentEvent::ReasoningDelta {
            text: "let me think".into(),
            kind: yi_agent_core::ReasoningKind::Content,
        });
        assert_eq!(out.len(), 1, "expected exactly the notice, got {out:?}");
        match &out[0] {
            Notification::ReasoningDelta {
                thread_id,
                text,
                kind,
            } => {
                assert_eq!(thread_id, "th1");
                assert_eq!(text, "let me think");
                assert_eq!(*kind, yi_agent_core::ReasoningKind::Content);
            }
            other => panic!("expected ReasoningDelta, got {other:?}"),
        }

        // 打开的 agentMessage 不受影响：CoT 不落到正文里。
        let out = t.on_event(AgentEvent::AssistantText(" world".into()));
        assert!(
            out.iter().any(|n| matches!(
                n,
                Notification::ItemDelta { delta, .. } if delta == " world"
            )),
            "the open agent message must keep receiving text, got {out:?}"
        );
    }

    /// 块级凭证事件**不得**把 CoT 写进可见的 agentMessage。
    ///
    /// `ThinkingBlock` 携带 `ContentBlock::Thinking` 的凭证，而凭证已由
    /// `accumulate_stream` 写进 session、随 `TurnLine.messages` 落盘
    /// （server.rs 直接取 `agent.session().messages()`）。翻译层因此无需再
    /// 做任何持久化——它唯一需要保证的是**不污染展示**：明文增量已由
    /// `reasoningDelta` 走旁路，若这里再打开/追加 agentMessage，CoT 就会
    /// 被当成答案正文（同一约束见
    /// `reasoning_delta_becomes_a_reasoning_notification`）。
    ///
    /// 断言的是真实保证：不产生任何开/追加 agentMessage 的通知。它**不**断言
    /// 通知为空——Task 13 已在此接入 `Item::Thinking`（不携带凭证）的展示条目，
    /// 故本事件会多出一条 thinking 通知（见
    /// `thinking_block_becomes_a_credential_free_thinking_item`）。
    #[test]
    fn thinking_block_does_not_leak_into_the_visible_agent_message() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::AssistantText("hi".into()));
        assert!(
            out.iter()
                .any(|n| matches!(n, Notification::ItemStarted { .. })),
            "precondition: an agent message is open"
        );

        let out = t.on_event(AgentEvent::ThinkingBlock {
            text: "cot summary".into(),
            token: ThinkingToken::Signature("SIG".into()),
        });
        assert!(
            !out.iter().any(|n| matches!(
                n,
                Notification::ItemStarted {
                    item: Item::AgentMessage { .. },
                    ..
                } | Notification::ItemDelta { .. }
                    | Notification::ItemCompleted {
                        item: Item::AgentMessage { .. },
                        ..
                    }
            )),
            "the thinking block must not open or append the visible agent \
             message body, got {out:?}"
        );

        // 正文仍是纯答案：把消息收尾后，完成的 agentMessage 只含答案文本。
        let out = t.on_event(AgentEvent::Done {
            reason: yi_agent_core::DoneReason::EndTurn,
        });
        let body = out.iter().find_map(|n| match n {
            Notification::ItemCompleted {
                item: Item::AgentMessage { text, .. },
                ..
            } => Some(text.clone()),
            _ => None,
        });
        assert_eq!(
            body.as_deref(),
            Some("hi"),
            "the completed message body must be the answer alone, got {out:?}"
        );
    }

    /// 块级 `ThinkingBlock` 必须产出一个**不携带凭证**的 `Item::Thinking`
    /// 展示条目（design §6.3 / §6.1）：`item/started` + `item/completed` 各一次，
    /// text 为明文、redacted 由 token 变体决定，wire 上**绝不**出现 signature /
    /// encrypted_content。
    #[test]
    fn thinking_block_becomes_a_credential_free_thinking_item() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::ThinkingBlock {
            text: "cot summary".into(),
            token: ThinkingToken::Signature("SIG-MUST-NOT-LEAK".into()),
        });

        let started = out.iter().find_map(|n| match n {
            Notification::ItemStarted {
                item: Item::Thinking { id, text, redacted },
                ..
            } => Some((id.clone(), text.clone(), *redacted)),
            _ => None,
        });
        let (id, text, redacted) = started.expect("a thinking item must be started");
        assert_eq!(text, "cot summary");
        assert!(!redacted, "a signature block is not a redacted placeholder");

        let completed = out
            .iter()
            .find_map(|n| match n {
                Notification::ItemCompleted {
                    item: Item::Thinking { id: cid, .. },
                    ..
                } => Some(cid.clone()),
                _ => None,
            })
            .expect("a thinking item must be completed");
        assert_eq!(id, completed, "started and completed must share one id");

        // 凭证保真铁律的反面：展示条目不得把 signature 带到 wire 上。
        let raw = serde_json::to_string(&out).unwrap();
        assert!(
            !raw.contains("SIG-MUST-NOT-LEAK") && !raw.contains("signature"),
            "the thinking item must never carry the block credential: {raw}"
        );
    }

    /// redacted 块（Anthropic `redacted_thinking`）展示为占位：明文为空、
    /// `redacted = true`，同样不带凭证本体。
    #[test]
    fn redacted_thinking_block_becomes_a_placeholder_item() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::ThinkingBlock {
            text: String::new(),
            token: ThinkingToken::Redacted("REDACTED-DATA".into()),
        });

        let found = out.iter().find_map(|n| match n {
            Notification::ItemStarted {
                item: Item::Thinking { text, redacted, .. },
                ..
            } => Some((text.clone(), *redacted)),
            _ => None,
        });
        assert_eq!(
            found,
            Some((String::new(), true)),
            "a redacted block renders as an empty-text placeholder, got {out:?}"
        );
        let raw = serde_json::to_string(&out).unwrap();
        assert!(
            !raw.contains("REDACTED-DATA"),
            "the redacted credential must not reach the client: {raw}"
        );
    }

    /// 运行期告知必须变成 `thread/notice`，带 level 与 text。
    #[test]
    fn notice_becomes_a_notice_notification() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::Notice {
            level: yi_agent_core::NoticeLevel::Info,
            text: "已自动改用 Chat".into(),
        });
        assert_eq!(
            out.len(),
            1,
            "expected exactly one notification, got {out:?}"
        );
        match &out[0] {
            Notification::Notice {
                thread_id,
                level,
                text,
            } => {
                assert_eq!(thread_id, "th1");
                assert_eq!(*level, yi_agent_core::NoticeLevel::Info);
                assert_eq!(text, "已自动改用 Chat");
            }
            other => panic!("expected Notice, got {other:?}"),
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
    fn tool_result_image_becomes_an_item_ref_without_base64() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::ToolCall {
            id: "call-1".into(),
            name: "view_image".into(),
            input: json!({"path": "logo.png"}),
        });
        assert!(!out.is_empty());
        let out = t.on_event(AgentEvent::ToolResult {
            id: "call-1".into(),
            result: ToolResult::with_content(vec![
                ContentBlock::Text("viewed logo.png (8x8, image/png)".into()),
                ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "AAAA".into(),
                    },
                    detail: ImageDetail::High,
                    path: Some(".yi-agent/attachments/t1/a1-logo.png".into()),
                },
            ]),
        });
        let item = match out.into_iter().next() {
            Some(Notification::ItemCompleted {
                item: item @ Item::ToolCall { .. },
                ..
            }) => item,
            other => panic!("expected one ItemCompleted ToolCall, got {other:?}"),
        };
        let images = match &item {
            Item::ToolCall { images, .. } => images,
            _ => unreachable!(),
        };
        assert_eq!(images.len(), 1, "expected exactly one ref, got {images:?}");
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(
            v["images"][0]["path"], ".yi-agent/attachments/t1/a1-logo.png",
            "tool-result image must surface as a path ref, got {v}"
        );
        assert_eq!(v["images"][0]["media_type"], "image/png");
        assert_eq!(v["images"][0]["size"], 0);
        assert!(
            !v.to_string().contains("AAAA"),
            "no base64 in the item, got {v}"
        );
        // render_content text behaviour is unchanged: the placeholder stays.
        assert!(
            v["result"]
                .as_str()
                .is_some_and(|s| s == "viewed logo.png (8x8, image/png)[image]"),
            "render_content output must be unchanged, got {}",
            v["result"]
        );
    }

    /// A tool-result image that is not on disk (no `path`) has nothing to
    /// reference: `image/read` could not locate it, so it must not become a ref.
    #[test]
    fn tool_result_image_without_a_path_produces_no_ref() {
        let mut t = translator();
        t.on_event(AgentEvent::ToolCall {
            id: "call-1".into(),
            name: "view_image".into(),
            input: json!({"path": "logo.png"}),
        });
        let out = t.on_event(AgentEvent::ToolResult {
            id: "call-1".into(),
            result: ToolResult::with_content(vec![
                ContentBlock::Text("viewed".into()),
                ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "AAAA".into(),
                    },
                    detail: ImageDetail::High,
                    path: None,
                },
            ]),
        });
        match &out[0] {
            Notification::ItemCompleted {
                item: Item::ToolCall { images, .. },
                ..
            } => assert!(
                images.is_empty(),
                "a pathless image is not referencable, got {images:?}"
            ),
            other => panic!("expected ItemCompleted ToolCall, got {other:?}"),
        }
        let v = serde_json::to_value(&out[0]).unwrap();
        assert!(
            !v.to_string().contains("AAAA"),
            "no base64 in the item, got {v}"
        );
    }

    #[test]
    fn tool_result_without_images_omits_the_field() {
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
        let v = serde_json::to_value(&out[0]).unwrap();
        assert!(
            v["item"].get("images").is_none(),
            "empty images must be skipped on the wire, got {v}"
        );
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
    fn usage_carries_cache_tokens() {
        let mut t = translator();
        let usage = TokenUsage {
            input_tokens: 3,
            output_tokens: 5,
            cache_creation_input_tokens: Some(100),
            cache_read_input_tokens: Some(200),
            reasoning_tokens: None,
        };
        let out = t.on_event(AgentEvent::Usage {
            model: "m".into(),
            usage,
        });
        match &out[0] {
            Notification::TokenUsage {
                cache_creation_input_tokens,
                cache_read_input_tokens,
                ..
            } => {
                assert_eq!(*cache_creation_input_tokens, 100);
                assert_eq!(*cache_read_input_tokens, 200);
            }
            other => panic!("expected TokenUsage, got {other:?}"),
        }
    }

    #[test]
    fn usage_missing_cache_tokens_defaults_to_zero() {
        let mut t = translator();
        let usage = TokenUsage {
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        };
        let out = t.on_event(AgentEvent::Usage {
            model: "m".into(),
            usage,
        });
        match &out[0] {
            Notification::TokenUsage {
                cache_creation_input_tokens,
                cache_read_input_tokens,
                ..
            } => {
                assert_eq!(*cache_creation_input_tokens, 0);
                assert_eq!(*cache_read_input_tokens, 0);
            }
            other => panic!("expected TokenUsage, got {other:?}"),
        }
    }

    #[test]
    fn usage_merges_split_anthropic_events() {
        let mut t = translator();
        // message_start:input + cache
        let _ = t.on_event(AgentEvent::Usage {
            model: "m".into(),
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 0,
                cache_creation_input_tokens: Some(7),
                cache_read_input_tokens: Some(9),
                reasoning_tokens: None,
            },
        });
        // message_delta:output only
        let out = t.on_event(AgentEvent::Usage {
            model: "m".into(),
            usage: TokenUsage {
                input_tokens: 0,
                output_tokens: 42,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                reasoning_tokens: None,
            },
        });
        match &out[0] {
            Notification::TokenUsage {
                input_tokens,
                output_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
                ..
            } => {
                assert_eq!(*input_tokens, 100, "input from message_start must survive");
                assert_eq!(
                    *output_tokens, 42,
                    "output from message_delta must be merged"
                );
                assert_eq!(*cache_creation_input_tokens, 7);
                assert_eq!(*cache_read_input_tokens, 9);
            }
            other => panic!("expected TokenUsage, got {other:?}"),
        }
    }

    #[test]
    fn usage_snapshot_resets_each_turn() {
        let mut t = translator();
        let _ = t.on_event(AgentEvent::Usage {
            model: "m".into(),
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 42,
                ..Default::default()
            },
        });
        t.set_turn("turn-2".into());
        let out = t.on_event(AgentEvent::Usage {
            model: "m".into(),
            usage: TokenUsage {
                input_tokens: 5,
                output_tokens: 0,
                ..Default::default()
            },
        });
        match &out[0] {
            Notification::TokenUsage {
                input_tokens,
                output_tokens,
                ..
            } => {
                assert_eq!(*input_tokens, 5);
                assert_eq!(*output_tokens, 0, "previous turn's output must not leak");
            }
            other => panic!("expected TokenUsage, got {other:?}"),
        }
    }

    #[test]
    fn usage_new_call_replaces_previous_snapshot() {
        let mut t = translator();
        // 第一次调用:input + cache,再补 output。
        let _ = t.on_event(AgentEvent::Usage {
            model: "m".into(),
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 0,
                cache_creation_input_tokens: Some(7),
                cache_read_input_tokens: Some(9),
                reasoning_tokens: None,
            },
        });
        let _ = t.on_event(AgentEvent::Usage {
            model: "m".into(),
            usage: TokenUsage {
                input_tokens: 0,
                output_tokens: 20,
                ..Default::default()
            },
        });
        // 第二次调用:新的 input,且 cache 缺省 → 不得残留上一次调用的 output/cache。
        let out = t.on_event(AgentEvent::Usage {
            model: "m".into(),
            usage: TokenUsage {
                input_tokens: 150,
                output_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                reasoning_tokens: None,
            },
        });
        match &out[0] {
            Notification::TokenUsage {
                input_tokens,
                output_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
                ..
            } => {
                assert_eq!(*input_tokens, 150);
                assert_eq!(*output_tokens, 0, "previous call's output must not leak");
                assert_eq!(
                    *cache_creation_input_tokens, 0,
                    "previous call's cache must not leak"
                );
                assert_eq!(*cache_read_input_tokens, 0);
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
    fn interjection_accepted_becomes_a_user_interjection_item() {
        let mut t = translator();
        t.set_turn("turn-1".into());
        let out = t.on_event(AgentEvent::InterjectionAccepted {
            seq: 1,
            text: "also check the logs".into(),
            tag: Some("interject-turn-1-abc".into()),
        });
        assert_eq!(out.len(), 2, "expected ItemStarted then ItemCompleted");
        for n in &out {
            match n {
                Notification::ItemStarted {
                    item: Item::UserInterjection { id, text, .. },
                    ..
                }
                | Notification::ItemCompleted {
                    item: Item::UserInterjection { id, text, .. },
                    ..
                } => {
                    // The id must be the caller's tag, not a freshly allocated
                    // item id: that is what lets the client reconcile.
                    assert_eq!(id, "interject-turn-1-abc");
                    assert_eq!(text, "also check the logs");
                }
                other => panic!("expected UserInterjection item, got {other:?}"),
            }
        }
    }

    #[test]
    fn interjection_without_a_tag_falls_back_to_an_item_id() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::InterjectionAccepted {
            seq: 1,
            text: "hi".into(),
            tag: None,
        });
        match &out[0] {
            Notification::ItemStarted {
                item: Item::UserInterjection { id, .. },
                ..
            } => assert!(!id.is_empty(), "expected an allocated item id"),
            other => panic!("expected UserInterjection, got {other:?}"),
        }
    }

    #[test]
    fn interjections_returned_is_reported_to_the_client() {
        let mut t = translator();
        t.set_turn("turn-7".into());
        let out = t.on_event(AgentEvent::InterjectionsReturned {
            items: vec![
                yi_agent_core::Interjection {
                    seq: 3,
                    text: "alpha".into(),
                    tag: Some("interject-a".into()),
                },
                yi_agent_core::Interjection {
                    seq: 4,
                    text: "beta".into(),
                    tag: Some("interject-b".into()),
                },
            ],
        });
        assert_eq!(out.len(), 1, "expected exactly one notification");
        match &out[0] {
            Notification::InterjectionsReturned {
                thread_id,
                turn_id,
                items,
            } => {
                assert_eq!(thread_id, "th1");
                assert_eq!(turn_id, "turn-7");
                assert_eq!(
                    items,
                    &vec!["interject-a".to_string(), "interject-b".to_string()]
                );
            }
            other => panic!("expected InterjectionsReturned, got {other:?}"),
        }
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
