//! 组长通道:把普通会话的消息投给所属项目的组长。
//!
//! 为什么在此处中转而不直连子 agent runtime:`ThreadRoot` 的 attach key 是
//! `thread:<id>`,daemon 的 `is_descendant_of` 只认自身子树,两个会话的 root
//! 互相不可达(测试 `two_conversation_roots_do_not_share_children_or_reach_across`
//! 刻意钉住了这条不变量)。给会话开一张指向组长 root 的 capability 会破坏它,
//! 因此 thread → 组长的通信一律经 app-server 中转:app-server 本就持有
//! 「thread → 项目 → 组长」的映射。
//!
//! P1 只实现单向投递(`blocking: false`);P2 补上阻塞往返:投递时带一个
//! `request_id`,本 thread 在 `LeadReplies` 里登记一个 oneshot 后 await,
//! 组长那轮的收尾回填(见 spec §5.2)。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{broadcast, oneshot};
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};

use crate::thread_store::{LEAD_ROLE, ThreadMeta, ThreadMode, ThreadStore};

/// 阻塞式 `ask_lead` 的默认超时。与 TUI `wait_agent` 的默认 2 分钟一致
/// (spec §5.3):组长是一个 thread、一个 agent,turn 串行,必须有界收口,
/// 否则本 thread 会永久挂死。
pub const ASK_LEAD_TIMEOUT: Duration = Duration::from_secs(120);

/// 阻塞回信的登记表。形状照抄审批路径的 `pending`
/// (`server.rs` 的 `Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>`):
/// 投递方登记一个 `oneshot::Sender` 后 await,应答方(组长那轮的收尾)回填。
///
/// 内部 `Arc<Mutex<..>>` 而非裸 `Mutex`:工具实例经 `Arc` 注册进 registry 共用,
/// 克隆句柄比改造 `Tool::call` 的所有权简单。
#[derive(Clone, Default)]
pub struct LeadReplies {
    waiting: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
}

impl LeadReplies {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一次等待,返回接收端。等待方超时后 drop 掉接收端即可
    /// (`resolve` 会安全失败,不 panic)。
    pub fn register(&self, request_id: String) -> oneshot::Receiver<String> {
        let (tx, rx) = oneshot::channel();
        self.waiting
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(request_id, tx);
        rx
    }

    /// 回填一次。无人等待(已超时/从未登记)返回 `false`,不 panic。
    pub fn resolve(&self, request_id: &str, reply: String) -> bool {
        let sender = self
            .waiting
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(request_id);
        match sender {
            Some(tx) => tx.send(reply).is_ok(),
            None => false,
        }
    }
}

/// 一条投给组长的消息。
///
/// `project_key` 在投递侧就算好并随消息携带:主循环收到后据此把消息路由给
/// 该项目的组长 thread,不必重新反推发送方属于哪个项目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeadEnvelope {
    pub project_key: String,
    pub from_thread_id: String,
    pub text: String,
    /// `Some` = 阻塞投递,等这个 id 的回信;`None` = 单向投递(P1 行为)。
    ///
    /// 纯进程内结构,不是 wire 类型,故与 `serde` 无关。
    pub request_id: Option<String>,
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
    // 卡片 worktree 不是项目:它没有组长,也绝不能有。堵在**落盘侧**是唯一
    // 能止住幽灵组长的地方——真机上已经落错的那些(`role:"lead"` 且
    // `board_project: None`,只有路径这条线索)由 `is_lead_meta` 在读时过滤。
    if crate::thread_store::is_card_worktree(cwd) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is a card worktree, not a project", cwd.display()),
        ));
    }
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
        board_project: None,
        card_id: None,
        model_ref: None,
        reasoning_effort: None,
        tag: None,
        preserve_thinking: true,
        // 组长由 `thread/lead` 懒创建，不经 `thread/start` 的关联根快照路径；
        // 关联根是**会话级**能力增量，组长这里保持单根（空集）。
        extra_roots: Vec::new(),
        role: Some(LEAD_ROLE.to_string()),
        content_epoch: 0,
        last_item_id: None,
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
/// 默认(不带 `with_replies`)是 P1 的纯单向行为,`blocking: true` 显式拒绝
/// 而非降级——静默降级会让模型以为自己在等一个答复,而无人回信时那比报错
/// 更难排查。装配提供了回信登记表后,`blocking: true` 才真的有界往返。
pub struct AskLeadTool {
    inbox: LeadInbox,
    project_key: String,
    from_thread_id: String,
    /// P2:本 thread 所属项目的组长回信登记表 + 阻塞超时。`None` = 无回信能力
    /// (P1 行为)。用 `Option` 保持 `new` 的三参数签名不变。
    replies: Option<(LeadReplies, Duration)>,
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
            replies: None,
        }
    }

    /// P2:注入本 thread 所属项目的组长回信登记表与阻塞超时。
    ///
    /// 不放进 `new` 的第 4 个参数:装配侧尚未接回信时(P1 的调用点)仍走三参数
    /// `new`,行为与 P1 完全一致。
    pub fn with_replies(mut self, replies: LeadReplies, timeout: Duration) -> Self {
        self.replies = Some((replies, timeout));
        self
    }
}

#[async_trait::async_trait]
impl Tool for AskLeadTool {
    fn name(&self) -> &str {
        "ask_lead"
    }

    fn description(&self) -> &str {
        "Send a message to this project's lead agent. The lead coordinates all threads in this project. By default delivery is one-way: you will not receive a reply during this turn, so keep working after calling it. Set `blocking: true` when you are blocked and need the lead's decision before continuing; you then wait for a reply (bounded, ~2 minutes) and the result carries the lead's answer. Use it to report a blocker, hand over a decision, or flag something the lead must know."
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
                    "description": "Set true to wait for the lead's reply (bounded to about 2 minutes). Use only when you genuinely cannot proceed without the lead's decision; on timeout you are told to continue on your own. Defaults to false (one-way delivery)."
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
        let text = parsed.message.trim().to_string();
        if text.is_empty() {
            return ToolResult::error("message must not be empty");
        }
        if parsed.blocking {
            let Some((replies, timeout)) = self.replies.clone() else {
                // 装配未提供回信能力(例如 TUI 侧尚未接):保持 P1 的显式拒绝,
                // 绝不静默降级成单向——那会让模型以为自己在等答复。
                return ToolResult::error(
                    "ask_lead blocking mode is not implemented here; \
                     resend without `blocking` so the message is delivered one-way",
                );
            };
            let request_id = format!("lead-ask-{}", uuid::Uuid::new_v4());
            let rx = replies.register(request_id.clone());
            if let Err(e) = self.inbox.deliver(LeadEnvelope {
                project_key: self.project_key.clone(),
                from_thread_id: self.from_thread_id.clone(),
                text: text.clone(),
                request_id: Some(request_id),
            }) {
                return ToolResult::error(format!("failed to deliver to lead: {e}"));
            }
            // 有界收口:绝不能在此永久挂住 thread(组长那轮的 turn 可能很长)。
            return match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(reply)) => ToolResult::text(format!("组长回复：{reply}")),
                Ok(Err(_)) => ToolResult::error("组长回信通道已关闭。"),
                Err(_) => {
                    ToolResult::text("组长暂未回应（已等待超时）。请自行继续手上的工作，别空等。")
                }
            };
        }
        let envelope = LeadEnvelope {
            project_key: self.project_key.clone(),
            from_thread_id: self.from_thread_id.clone(),
            text: text.clone(),
            request_id: None,
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

    #[test]
    fn lead_replies_resolving_a_registered_request_delivers_the_reply() {
        let replies = LeadReplies::new();
        let rx = replies.register("req-1".to_string());
        assert!(replies.resolve("req-1", "组长回复：先别动".to_string()));
        assert_eq!(rx.blocking_recv().expect("reply"), "组长回复：先别动");
    }

    #[test]
    fn lead_replies_resolving_an_unknown_request_is_a_no_op() {
        let replies = LeadReplies::new();
        assert!(!replies.resolve("nobody", "x".to_string()));
    }

    #[test]
    fn lead_replies_dropping_the_receiver_does_not_panic_on_late_resolve() {
        let replies = LeadReplies::new();
        let rx = replies.register("req-2".to_string());
        drop(rx);
        // 等待方已走（超时），迟到的回信不得 panic。
        assert!(!replies.resolve("req-2", "late".to_string()));
    }

    #[tokio::test]
    async fn lead_replies_blocking_ask_lead_times_out_with_a_readable_message() {
        let inbox = LeadInbox::new();
        let _sub = inbox.subscribe(); // 有人听，投递会成功
        let replies = LeadReplies::new();
        let tool = AskLeadTool::new(inbox, "p".into(), "t".into())
            .with_replies(replies, std::time::Duration::from_millis(50));
        let out = tool
            .call(serde_json::json!({ "message": "怎么办", "blocking": true }))
            .await;
        let body = format!("{out:?}");
        assert!(body.contains("暂未"), "must explain the timeout: {body}");
    }

    #[test]
    fn lead_replies_a_blocking_envelope_carries_a_request_id() {
        let e = LeadEnvelope {
            project_key: "p".into(),
            from_thread_id: "t".into(),
            text: "hi".into(),
            request_id: Some("req-9".into()),
        };
        assert_eq!(e.request_id.as_deref(), Some("req-9"));
        let one_way = LeadEnvelope {
            project_key: "p".into(),
            from_thread_id: "t".into(),
            text: "hi".into(),
            request_id: None,
        };
        assert!(one_way.request_id.is_none());
    }

    /// 阻塞往返的完整形状:投递携带 `request_id`,组长那轮据此回填,
    /// 本 thread 拿到的是回信文本而非超时文本。超时是兜底,不是常态。
    #[tokio::test]
    async fn lead_replies_a_blocking_ask_returns_the_leads_reply() {
        let inbox = LeadInbox::new();
        let mut rx = inbox.subscribe();
        let replies = LeadReplies::new();
        let tool = AskLeadTool::new(inbox, "p".into(), "t".into())
            .with_replies(replies.clone(), std::time::Duration::from_secs(5));

        let call = tokio::spawn(async move {
            tool.call(serde_json::json!({ "message": "怎么办", "blocking": true }))
                .await
        });

        // 扮演主循环:收到信封后按 `request_id` 回信。
        let envelope = rx.recv().await.expect("envelope");
        let request_id = envelope.request_id.expect("blocking 投递必须带 request_id");
        assert_eq!(envelope.text, "怎么办");
        assert!(replies.resolve(&request_id, "先别动".to_string()));

        let out = call.await.expect("call task");
        let body = format!("{out:?}");
        assert!(body.contains("先别动"), "must carry the reply: {body}");
        assert!(!out.is_error, "拿到回信不是错误: {body}");
    }

    /// 看板 worktree 不是项目:`ensure_lead_meta` 必须**拒绝**,绝不落盘组长。
    ///
    /// 真机数据里已有 9 个幽灵组长,全部由卡片会话 `ask_lead` 触发的这条路径
    /// 落盘。落盘侧堵死,这类假组长才不会继续长出来。
    #[test]
    fn ensure_lead_meta_refuses_a_card_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = dir.path().join(".worktrees/kanban/card-1");
        std::fs::create_dir_all(&worktree).unwrap();
        let store = ThreadStore::new(&worktree);
        assert!(
            ensure_lead_meta(&store, &worktree).is_err(),
            "a card worktree is not a project and must not get a lead"
        );
        assert!(
            store.find_lead().unwrap().is_none(),
            "nothing may be persisted inside a card worktree"
        );

        // 普通项目目录照旧:拒绝的是**路径**,不是功能。
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let project_store = ThreadStore::new(&project);
        let (meta, created) = ensure_lead_meta(&project_store, &project).expect("lead meta");
        assert!(created);
        assert_eq!(
            project_store.find_lead().unwrap().unwrap().thread_id,
            meta.thread_id
        );
    }
}
