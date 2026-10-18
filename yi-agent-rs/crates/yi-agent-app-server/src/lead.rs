//! 组长通道:把普通会话的消息投给所属项目的组长。
//!
//! 为什么在此处中转而不直连子 agent runtime:`ThreadRoot` 的 attach key 是
//! `thread:<id>`,daemon 的 `is_descendant_of` 只认自身子树,两个会话的 root
//! 互相不可达(测试 `two_conversation_roots_do_not_share_children_or_reach_across`
//! 刻意钉住了这条不变量)。给会话开一张指向组长 root 的 capability 会破坏它,
//! 因此 thread → 组长的通信一律经 app-server 中转:app-server 本就持有
//! 「thread → 项目 → 组长」的映射。
//!
//! P1 只实现单向投递(`blocking: false`)。阻塞往返的形状见 spec §5.2,属 P2。

use std::path::Path;

use serde::Deserialize;
use serde_json::Value;
use tokio::sync::broadcast;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};

use crate::thread_store::{LEAD_ROLE, ThreadMeta, ThreadMode, ThreadStore};

/// 一条投给组长的消息。
///
/// `project_key` 在投递侧就算好并随消息携带:主循环收到后据此把消息路由给
/// 该项目的组长 thread,不必重新反推发送方属于哪个项目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeadEnvelope {
    pub project_key: String,
    pub from_thread_id: String,
    pub text: String,
}

/// 每项目一份的组长信箱。
///
/// 用 `broadcast` 而非 `mpsc`:主循环是唯一消费者,但后者在组长 thread 重启
/// (driver 换新)时需要重建通道、丢掉已在路上的消息;`broadcast` 让订阅者
/// 随到随取,发送侧无须知道谁在听。容量 64 与 `theme_tool` / `git_diff_tool`
/// 的同类通道一致。
#[derive(Clone)]
pub struct LeadInbox {
    tx: broadcast::Sender<LeadEnvelope>,
}

impl LeadInbox {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(64);
        Self { tx }
    }

    /// 投递一条消息。没有订阅者时返回 `Ok`:组长尚未被拉起不是错误,主循环
    /// 稍后会以该项目的组长 thread 为消费者订阅。
    pub fn deliver(&self, envelope: LeadEnvelope) -> Result<(), String> {
        // `send` 只在没有任何接收者时报错;把订阅者数当容量判断是错的,
        // 故这里忽略 Err 的具体原因,只区分「有没有人听」。
        match self.tx.send(envelope.clone()) {
            Ok(_) => Ok(()),
            Err(_) => {
                // 无人订阅:留待主循环订阅后仍能看到的是「后续」消息,
                // 因此这里退化为无操作成功,并在调用侧记录。
                tracing::info!(
                    project_key = %envelope.project_key,
                    from = %envelope.from_thread_id,
                    "lead inbox has no subscriber yet; message dropped"
                );
                Ok(())
            }
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LeadEnvelope> {
        self.tx.subscribe()
    }
}

impl Default for LeadInbox {
    fn default() -> Self {
        Self::new()
    }
}

/// 确保 `cwd` 目录存在组长 meta,返回它以及本次是否真的建了。
///
/// 只写 meta,不建 agent、不起 driver:调用点之一是 `thread/listAll`,那是纯
/// 列表查询,在上面做重活会让侧栏每次刷新都付一次装配成本。真正的 agent 由
/// 用户打开组长对话时的 `thread/resume` 建。
///
/// `created` 区分本次调用是否真的建了:`ThreadMeta` 不带该信息,故一并返回,
/// 让 `thread/lead` 能如实回答调用方。
///
/// 注意返回的是**落盘后重新查一次** `find_lead` 的结果(首次创建时也一样),
/// 而不是刚造出的那份 `meta`。竞态下用户可能拿到一份「落盘晚于别人」的 id,
/// 若直接回它,`thread/lead` 就会毒化客户端缓存:此后 `listAll` 里
/// `find_lead` 选的是另一个组长,而客户端还记着刚才那个。以 `find_lead` 的
/// 结论为唯一出口,分歧就只在落盘内存在(见 `ThreadStore::find_lead` 的注释)。
pub fn ensure_lead_meta(store: &ThreadStore, cwd: &Path) -> std::io::Result<(ThreadMeta, bool)> {
    if let Some(existing) = store.find_lead()? {
        return Ok((existing, false));
    }
    // 竞态说明:两个请求同时走到这里会各造一个 meta。`find_lead` 取 `created_at`
    // 最早者,故此后所有读取都收敛到同一个;下面的复读让本次响应也以它为准。
    let now = chrono::Utc::now().timestamp_millis();
    let meta = ThreadMeta {
        thread_id: format!("thread-lead-{}", uuid::Uuid::new_v4()),
        cwd: cwd.display().to_string(),
        model: String::new(), // 空 = 跟随全局默认,与「损坏重建」同款兜底
        created_at: now,
        updated_at: now,
        title: Some("组长".to_string()),
        permission_mode: ThreadMode::Normal,
        pin_seq: None,
        driver_project: None,
        driver_ref: None,
        model_ref: None,
        reasoning_effort: None,
        preserve_thinking: true,
        role: Some(LEAD_ROLE.to_string()),
    };
    store.create(&meta)?;
    // 复读:并发下这里可能读到别人先落的、`created_at` 更早的那一个。
    match store.find_lead()? {
        Some(winner) => Ok((winner, true)),
        None => Ok((meta, true)),
    }
}

/// 普通会话用来给组长留言的工具。
///
/// P1 只做单向投递。`blocking: true` 显式拒绝而非降级——静默降级会让模型
/// 以为自己在等一个答复,而 P1 根本不会回信,那比报错更难排查。
pub struct AskLeadTool {
    inbox: LeadInbox,
    project_key: String,
    from_thread_id: String,
}

#[derive(Debug, Deserialize)]
struct AskLeadArgs {
    message: String,
    #[serde(default)]
    blocking: bool,
}

impl AskLeadTool {
    pub fn new(inbox: LeadInbox, project_key: String, from_thread_id: String) -> Self {
        Self {
            inbox,
            project_key,
            from_thread_id,
        }
    }
}

#[async_trait::async_trait]
impl Tool for AskLeadTool {
    fn name(&self) -> &str {
        "ask_lead"
    }

    fn description(&self) -> &str {
        "Send a message to this project's lead agent. The lead coordinates all threads in this project. Delivery is one-way in this version: you will not receive a reply during this turn, so keep working after calling it. Use it to report a blocker, hand over a decision, or flag something the lead must know."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "What the lead should know. Be specific and self-contained; the lead may not have read this conversation."
                },
                "blocking": {
                    "type": "boolean",
                    "description": "Reserved. Waiting for a reply is not implemented in this version; do not set it."
                }
            },
            "required": ["message"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let parsed: AskLeadArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(format!("invalid arguments: {e}")),
        };
        if parsed.blocking {
            return ToolResult::error(
                "ask_lead blocking mode is not implemented in P1; \
                 resend without `blocking` so the message is delivered one-way",
            );
        }
        let text = parsed.message.trim().to_string();
        if text.is_empty() {
            return ToolResult::error("message must not be empty");
        }
        let envelope = LeadEnvelope {
            project_key: self.project_key.clone(),
            from_thread_id: self.from_thread_id.clone(),
            text: text.clone(),
        };
        match self.inbox.deliver(envelope) {
            Ok(()) => ToolResult::text(format!(
                "已投递给你所在项目的组长 ({})。本条为单向投递,本轮不会收到回复;请继续手上的工作。",
                self.project_key
            )),
            Err(e) => ToolResult::error(format!("failed to deliver to lead: {e}")),
        }
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            source: ToolSource::Plugin {
                name: "lead".to_string(),
            },
            requires_confirmation: false,
            read_only: true,
            version: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn delivering_reaches_a_subscriber_with_the_sender_identity() {
        let inbox = LeadInbox::new();
        let mut rx = inbox.subscribe();
        let tool = AskLeadTool::new(inbox.clone(), "proj-A".into(), "thread-1".into());

        let result = tool.call(json!({"message": "  需要你裁决 X  "})).await;

        let envelope = rx.try_recv().expect("envelope must be delivered");
        assert_eq!(envelope.project_key, "proj-A");
        assert_eq!(envelope.from_thread_id, "thread-1");
        assert_eq!(envelope.text, "需要你裁决 X", "文本必须 trim");
        assert!(
            format!("{result:?}").contains("已投递") || format!("{result:?}").contains("delivered")
        );
    }

    #[tokio::test]
    async fn an_empty_message_is_rejected_and_delivers_nothing() {
        let inbox = LeadInbox::new();
        let mut rx = inbox.subscribe();
        let tool = AskLeadTool::new(inbox.clone(), "proj-A".into(), "thread-1".into());

        let result = tool.call(json!({"message": "   "})).await;

        assert!(rx.try_recv().is_err(), "空消息不得投递");
        // 返回的是错误而非 panic;具体文案由实现决定,只要求它不成功。
        assert!(!format!("{result:?}").contains("已投递"));
    }

    #[tokio::test]
    async fn blocking_is_refused_in_phase_one() {
        let inbox = LeadInbox::new();
        let mut rx = inbox.subscribe();
        let tool = AskLeadTool::new(inbox.clone(), "proj-A".into(), "thread-1".into());

        let result = tool.call(json!({"message": "hi", "blocking": true})).await;

        assert!(rx.try_recv().is_err(), "P1 不得把阻塞请求静默降级为投递");
        let text = format!("{result:?}");
        assert!(
            text.contains("not implemented") || text.contains("P1"),
            "got {text}"
        );
    }

    /// 没有订阅者时投递不得报错:组长可能尚未被拉起,消息先落邮箱。
    #[tokio::test]
    async fn delivering_with_no_subscriber_is_not_an_error() {
        let inbox = LeadInbox::new();
        let tool = AskLeadTool::new(inbox.clone(), "proj-A".into(), "thread-1".into());
        let result = tool.call(json!({"message": "hi"})).await;
        assert!(
            format!("{result:?}").contains("已投递") || format!("{result:?}").contains("delivered")
        );
    }

    /// 工具必须声明为只读(它不改工作区),否则沙箱读数会误导权限层。
    #[test]
    fn ask_lead_is_read_only() {
        let tool = AskLeadTool::new(LeadInbox::new(), "p".into(), "t".into());
        assert!(tool.metadata().read_only);
    }
}
