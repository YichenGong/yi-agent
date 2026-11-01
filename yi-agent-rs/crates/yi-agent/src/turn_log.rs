//! 一轮对话的落盘记录器：TUI driver 与 headless `run` 共用。
//!
//! 抽出来的唯一理由是**避免第二套映射与裁剪规则**。这条链上有三个容易写错
//! 却不显眼的地方，复制一份就等于给未来的漂移开闸：
//!
//! 1. 用户消息必须自己补。`Translator::on_event` 只翻译 agent 侧事件，从不
//!    产出 `Item::UserMessage`（`server.rs` 的 userPrompt 补齐注释同样处理）。
//!    漏了这一步，resume 出来的会话只有回答没有问题。
//! 2. 取消路径不发 `Done`。`agent.rs` 里取消是 `truncate(keep)` → 发
//!    `Cancelled` → `return`，所以落盘必须挂在 stream 结束上，不能挂在 `Done`。
//! 3. 写多少条要按 session 实际长度裁剪。session 在取消时被回滚滚短了，
//!    多写的条目在上下文里并不存在，resume 后会显示模型没说过的话。

use yi_agent_app_server::protocol::{Item, Notification};
use yi_agent_app_server::thread_store::TurnUsage;
use yi_agent_app_server::translate::Translator;

/// 收集一轮的 items 与用量，结束时按 session 真实长度裁剪成可落盘的一轮。
pub struct TurnLog {
    translator: Option<Translator>,
    items: Vec<Item>,
    /// 本轮最后一次请求的用量。
    usage: Option<TurnUsage>,
    /// 含历史在内的累计用量。`None` 表示这轮没有出现用量事件。
    usage_total: Option<TurnUsage>,
}

impl TurnLog {
    /// `translator` 为 `None` 时整轮不落盘（会话存储不可用），
    /// 事件仍会被消费，只是不产生 items。
    pub fn new(translator: Option<Translator>) -> Self {
        Self {
            translator,
            items: Vec::new(),
            usage: None,
            usage_total: None,
        }
    }

    /// 每轮开始时调用。`turn_id` 必须是新的：`alloc_item_id` 依赖它，
    /// 空串或复用会让 item id 跨轮重复，而 `partial_is_committed` 正是靠
    /// 首个 item id 判断「这轮是否已落盘」。
    pub fn begin_turn(&mut self) {
        let Some(t) = self.translator.as_mut() else {
            return;
        };
        t.set_turn(format!("turn-{}", uuid::Uuid::new_v4()));
        self.items.clear();
        self.usage = None;
        self.usage_total = None;
    }

    /// `/clear` 后把累计归零：否则清空对话后状态栏仍显示旧总数，
    /// 用户会以为清空没生效。app-server 在同一位置调 `reset_totals`。
    pub fn reset_totals(&mut self) {
        if let Some(t) = self.translator.as_mut() {
            let _ = t.reset_totals();
        }
    }

    /// 喂一个事件。**取消轮也必须喂**：那轮的内容只是被裁剪，不是不该存在。
    pub fn feed(&mut self, event: yi_agent_core::AgentEvent) {
        let Some(t) = self.translator.as_mut() else {
            return;
        };
        for notification in t.on_event(event) {
            match notification {
                Notification::ItemCompleted { item, .. } => self.items.push(item),
                Notification::TokenUsage {
                    model,
                    input_tokens,
                    output_tokens,
                    cache_creation_input_tokens,
                    cache_read_input_tokens,
                    ..
                } => {
                    self.usage = Some(TurnUsage {
                        model: model.clone(),
                        input_tokens,
                        output_tokens,
                        cache_creation_input_tokens,
                        cache_read_input_tokens,
                    });
                    let totals = t.turn_totals();
                    self.usage_total = Some(TurnUsage {
                        model,
                        input_tokens: yi_agent_app_server::server::saturate_u32(
                            totals.input_tokens,
                        ),
                        output_tokens: yi_agent_app_server::server::saturate_u32(
                            totals.output_tokens,
                        ),
                        cache_creation_input_tokens: yi_agent_app_server::server::saturate_u32(
                            totals.cache_creation_input_tokens,
                        ),
                        cache_read_input_tokens: yi_agent_app_server::server::saturate_u32(
                            totals.cache_read_input_tokens,
                        ),
                    });
                }
                _ => {}
            }
        }
    }

    /// 把本轮裁剪成可落盘的一轮：`(items, usage, usage_total)`。
    ///
    /// `base_len` 是**调用 `agent.run` 之前**的 session 长度；`messages` 是
    /// 本轮结束后的整份转录。两者之差即本轮真实净增，取消时会被回滚抛小。
    ///
    /// 返回 `None` 表示没有会话存储，调用方不必落盘。
    pub fn finish(
        &self,
        prompt: &str,
        messages: &[yi_agent_core::Message],
        base_len: usize,
        has_store: bool,
    ) -> Option<(Vec<Item>, Option<TurnUsage>, Option<TurnUsage>)> {
        if !has_store {
            return None;
        }

        let net = messages.len().saturating_sub(base_len);

        // 用户消息自己补：translator 不产出它。
        let mut items: Vec<Item> = Vec::with_capacity(self.items.len() + 1);
        items.push(Item::UserMessage {
            id: format!("user-{}", uuid::Uuid::new_v4()),
            text: prompt.to_string(),
            attachments: Vec::new(),
            images: Vec::new(),
        });

        // 按 session 实际长度裁剪：宁可少写不可多写。`net == 0` 表示连
        // user prompt 都被回滚了，此时只留那 1 条 user item——它确实是
        // 用户问过的，不算伪造。
        let keep = net.max(1).min(self.items.len() + 1);
        items.extend(self.items.iter().take(keep - 1).cloned());

        Some((items, self.usage.clone(), self.usage_total.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yi_agent_app_server::thread_store::UsageTotals;

    fn log_with_translator() -> TurnLog {
        TurnLog::new(Some(Translator::new(
            "thread-t".into(),
            UsageTotals::default(),
        )))
    }

    #[test]
    fn finish_always_writes_the_user_message() {
        let mut log = log_with_translator();
        log.begin_turn();
        let out = log.finish("你好", &[], 0, true).expect("有 store 就该落盘");
        assert_eq!(out.0.len(), 1);
        assert!(matches!(
            &out.0[0],
            Item::UserMessage { text, .. } if text == "你好"
        ));
    }

    #[test]
    fn finish_trims_to_the_net_growth_when_a_turn_is_rolled_back() {
        let mut log = log_with_translator();
        log.begin_turn();
        // 假装 translator 产出了 3 条 item。
        log.items = vec![
            Item::AgentMessage {
                id: "a1".into(),
                text: "一".into(),
            },
            Item::AgentMessage {
                id: "a2".into(),
                text: "二".into(),
            },
            Item::AgentMessage {
                id: "a3".into(),
                text: "三".into(),
            },
        ];
        // 取消回滚：session 相对 base 只净增 2 条。
        let messages = vec![
            yi_agent_core::Message {
                role: yi_agent_core::Role::User,
                content: vec![],
            },
            yi_agent_core::Message {
                role: yi_agent_core::Role::Assistant,
                content: vec![],
            },
        ];
        let out = log.finish("问", &messages, 0, true).unwrap();
        // 1 条 user + 只留前 1 条 item（keep=2）。
        assert_eq!(out.0.len(), 2, "多写的条目在 session 里不存在，必须裁掉");
    }

    #[test]
    fn finish_without_a_store_returns_none() {
        let mut log = log_with_translator();
        log.begin_turn();
        assert!(log.finish("x", &[], 0, false).is_none());
    }

    #[test]
    fn finish_with_a_fully_rolled_back_turn_still_keeps_the_user_message() {
        let mut log = log_with_translator();
        log.begin_turn();
        log.items = vec![Item::AgentMessage {
            id: "a1".into(),
            text: "被回滚".into(),
        }];
        // net == 0：连 user prompt 都被回滚。
        let out = log.finish("问", &[], 5, true).unwrap();
        assert_eq!(out.0.len(), 1, "只留用户真正问过的那条");
    }
}
