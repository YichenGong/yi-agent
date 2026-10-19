//! 组长的**写通道**：把「取消 / 打回」这类针对**别的 thread** 的写动作，
//! 从组长工具（driver 侧）送到主循环（唯一持有 `&mut threads` 的地方）。
//!
//! 为什么不直接写：`threads` 是主循环独占的 `&mut`（上级 spec §6.1），组长工具
//! 在装配期只拿到 `project_key`，天然碰不到它。这与 P2 的后台 tick 同一约束，
//! 故解法同款——一条 broadcast 通道 + 主循环 `select!` 消费。
//!
//! 方向与 `lead::LeadInbox` **相反**：那个是 thread→lead（会话给组长留言），
//! 这个是 lead→thread（组长下指令）。刻意不复用同一条通道：两种语义挤在一起
//! 后续必长歪（见 spec §5.2）。
//!
//! 本模块只放**纯逻辑**：指令结构、通道、目标分类判定。真正的执行在主循环里
//! （它才知道 `threads` 的内容）——这样「同项目/异项目/不在场」的判定可以脱
//! 主循环单测，零 LLM、零 I/O。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{broadcast, oneshot};

/// 组长能对另一个 thread 做的写动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeadControlKind {
    /// 打断目标当前的活跃轮（复用 `turn/interrupt` 语义）。
    Interrupt,
    /// 打回：向目标投递一条「组长指令 + 理由」（非阻塞注入；若目标无活跃轮则起一轮）。
    Reject,
}

/// 一条投给主循环的写指令。
///
/// 纯进程内结构（非 wire 类型，故与 `serde` 无关），与 `lead::LeadEnvelope` 同款。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeadControlCmd {
    /// 发起这条指令的组长所属的项目键。
    pub project_key: String,
    pub kind: LeadControlKind,
    /// 目标 thread（必须在同一个项目 store 内）。
    pub thread_id: String,
    /// 给用户/目标的理由——打回会把它带进目标转录。
    pub reason: String,
    /// 结果回填 id：工具侧登记一个 oneshot，主循环执行完按它回填。
    pub request_id: String,
}

/// 组长的写通道（发送端）。
///
/// 用 `broadcast` 而非 `mpsc`：与 `LeadInbox` 同一理由——主循环是唯一消费者，
/// 但订阅者（主循环）可能晚于发送方建立，`broadcast` 让「有没有人听」成为
/// 一次可判定的返回值（`send` 返回订阅者数是否为 0），而不是静默丢弃。
#[derive(Clone)]
pub struct LeadControlInbox {
    tx: broadcast::Sender<LeadControlCmd>,
}

impl LeadControlInbox {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(64);
        Self { tx }
    }

    /// 投递一条指令。返回 `true` 表示至少有一个订阅者（主循环在听）。
    ///
    /// 与 `LeadInbox::deliver` 的差别：那个把「无人听」退化成 `Ok(())`（组长尚未
    /// 拉起不算错），这个**必须如实返回 false**——主循环不在听意味着这条写动作
    /// 根本不会发生，静默成功会让组长以为已经打回/取消了。调用方据此回错。
    pub fn send(&self, cmd: LeadControlCmd) -> bool {
        self.tx.send(cmd).is_ok()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LeadControlCmd> {
        self.tx.subscribe()
    }
}

impl Default for LeadControlInbox {
    fn default() -> Self {
        Self::new()
    }
}

/// 等待中的写结果回填端：`request_id` → 结果。抽成别名是为了满足
/// `clippy::type_complexity`（裸的嵌套泛型超过阈值）。
type Waiting = Arc<Mutex<HashMap<String, oneshot::Sender<Result<String, String>>>>>;

/// 写结果的登记表。形状照抄 `lead::LeadReplies`：投递方登记 `oneshot` 后 await，
/// 主循环执行完回填 `Ok(执行摘要)` 或 `Err(失败原因)`。
///
/// 结果是 `Result` 而非裸 `String`：写动作可能失败（目标无活跃轮、目标不在场），
/// 失败必须让组长看到，不能伪装成成功。
#[derive(Clone, Default)]
pub struct LeadControlReplies {
    waiting: Waiting,
}

impl LeadControlReplies {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一次等待，返回接收端。等待方超时后 drop 掉接收端即可（`resolve` 会安全失败）。
    pub fn register(&self, request_id: String) -> oneshot::Receiver<Result<String, String>> {
        let (tx, rx) = oneshot::channel();
        self.waiting
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(request_id, tx);
        rx
    }

    /// 回填一次。无人等待（已超时 / 从未登记）返回 `false`，不 panic。
    pub fn resolve(&self, request_id: &str, outcome: Result<String, String>) -> bool {
        let sender = self
            .waiting
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(request_id);
        match sender {
            Some(tx) => tx.send(outcome).is_ok(),
            None => false,
        }
    }
}

/// 一条指令的目标在**本进程**里能不能落地。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlTarget {
    /// 目标 thread 就在本项目的 `threads` 里，可以执行。
    Session,
    /// 目标不可达：项目键不符，或该 thread 不在场的冷会话。
    NotOpen,
}

/// 判定指令目标是否可执行。
///
/// `project_key` = 指令发起方（组长）的项目键，`cmd_project_key` = 主循环当前正在
/// 服务的项目键，`in_threads` = 目标是否在当前 `threads` 里。
///
/// 项目键必须**完全相等**：不同项目即使有同名 thread，也绝不能互达（跨项目写是
/// 隔离不变量的红线）。冷会话（不在 `threads`）一律 `NotOpen`——给别的 thread
/// 冷启动一轮需要复刻 `deliver_to_lead` 的整套恢复逻辑，超出本设计（spec §5.4）。
pub fn classify_target(
    project_key: &str,
    cmd_project_key: &str,
    in_threads: bool,
) -> ControlTarget {
    if project_key != cmd_project_key {
        return ControlTarget::NotOpen;
    }
    if !in_threads {
        return ControlTarget::NotOpen;
    }
    ControlTarget::Session
}

use serde::Deserialize;
use serde_json::Value;
use yi_agent_core::{Tool, ToolMetadata, ToolResult};

/// 两个写工具共用的参数形状。
#[derive(Deserialize)]
struct LeadControlArgs {
    thread_id: String,
    #[serde(default)]
    reason: String,
}

/// 两个写工具共用的执行骨架：校验 → 投递 → 等主循环回填结果。
///
/// 抽出来是为了让「校验 + 投递 + 有界等待 + 错误归一」只有一份；两个工具的差别
/// 仅在 `kind` 与文案。
async fn run_control(
    inbox: &LeadControlInbox,
    replies: &LeadControlReplies,
    project_key: &str,
    kind: LeadControlKind,
    args: Value,
) -> ToolResult {
    let parsed: LeadControlArgs = match serde_json::from_value(args) {
        Ok(a) => a,
        Err(e) => return ToolResult::error(format!("invalid arguments: {e}")),
    };
    let thread_id = parsed.thread_id.trim().to_string();
    if thread_id.is_empty() {
        return ToolResult::error("thread_id must not be empty");
    }
    let reason = parsed.reason.trim().to_string();
    if reason.is_empty() {
        return ToolResult::error("reason must not be empty");
    }
    let request_id = format!("lead-ctl-{}", uuid::Uuid::new_v4());
    let rx = replies.register(request_id.clone());
    let delivered = inbox.send(LeadControlCmd {
        project_key: project_key.to_string(),
        kind,
        thread_id,
        reason,
        request_id,
    });
    if !delivered {
        // 主循环不在听：这条写动作不会发生，如实报错（不静默成功）。
        return ToolResult::error("组长写通道当前无人接收（主循环未在运行），本条指令未下达。");
    }
    // 有界等待：主循环是本进程内一步操作，毫秒级；给 5s 兜底，避免主循环卡死时
    // 工具永久挂住（与 `ask_lead blocking` 的有界收口同一原则）。
    match tokio::time::timeout(std::time::Duration::from_secs(5), rx).await {
        Ok(Ok(Ok(summary))) => ToolResult::text(summary),
        Ok(Ok(Err(reason))) => ToolResult::error(format!("指令未能执行：{reason}")),
        Ok(Err(_)) => ToolResult::error("组长写通道已关闭。"),
        Err(_) => ToolResult::error("等待主循环执行超时（5s）。"),
    }
}

/// 两个写工具的元数据：与 `lead_remember` 同源（`ToolSource::Plugin { name: "lead" }`），
/// `requires_confirmation: false`（后台无人值守时若需批准，功能就废了，spec §6）。
/// **不设** `read_only`——它们是写动作。
fn lead_control_metadata() -> ToolMetadata {
    crate::lead_memory::lead_memory_metadata()
}

/// 打断目标 thread 的当前活跃轮。**只由组长使用**（注册点见 `build_runtime_tooling`）。
pub struct LeadInterruptTool {
    inbox: LeadControlInbox,
    replies: LeadControlReplies,
    project_key: String,
}

impl LeadInterruptTool {
    pub fn new(inbox: LeadControlInbox, replies: LeadControlReplies, project_key: String) -> Self {
        Self {
            inbox,
            replies,
            project_key,
        }
    }
}

#[async_trait::async_trait]
impl Tool for LeadInterruptTool {
    fn name(&self) -> &str {
        "lead_interrupt_thread"
    }

    fn description(&self) -> &str {
        "Interrupt the current in-flight turn of another thread in this project (same effect as the user pressing Esc). Fails if that thread has no active turn — it never silently no-ops. Use it to stop a thread that has gone off track mid-turn; to correct a thread without stopping it, use lead_reject_thread instead."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "thread_id": { "type": "string", "description": "The thread whose current turn to interrupt (must be in this project)." },
                "reason": { "type": "string", "description": "Why you are interrupting. Recorded so the user can audit it." }
            },
            "required": ["thread_id", "reason"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        run_control(
            &self.inbox,
            &self.replies,
            &self.project_key,
            LeadControlKind::Interrupt,
            args,
        )
        .await
    }

    fn metadata(&self) -> ToolMetadata {
        lead_control_metadata()
    }
}

/// 打回目标 thread：投递一条「组长指令 + 理由」。**只由组长使用**。
pub struct LeadRejectTool {
    inbox: LeadControlInbox,
    replies: LeadControlReplies,
    project_key: String,
}

impl LeadRejectTool {
    pub fn new(inbox: LeadControlInbox, replies: LeadControlReplies, project_key: String) -> Self {
        Self {
            inbox,
            replies,
            project_key,
        }
    }
}

#[async_trait::async_trait]
impl Tool for LeadRejectTool {
    fn name(&self) -> &str {
        "lead_reject_thread"
    }

    fn description(&self) -> &str {
        "Send a correcting instruction to another thread in this project. If that thread is mid-turn the instruction is folded into the current turn (like a user interjection); if it is idle a fresh turn is started so the correction takes effect. To retract an earlier rejection, send another one that says so — the transcript is the ledger. Use it to steer a thread that is heading the wrong way."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "thread_id": { "type": "string", "description": "The thread to reject (must be in this project)." },
                "reason": { "type": "string", "description": "The correcting instruction plus why. It lands verbatim in that thread's transcript, so write it for the thread to act on." }
            },
            "required": ["thread_id", "reason"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        run_control(
            &self.inbox,
            &self.replies,
            &self.project_key,
            LeadControlKind::Reject,
            args,
        )
        .await
    }

    fn metadata(&self) -> ToolMetadata {
        lead_control_metadata()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_from_another_project_is_not_applicable() {
        // project_key 是「本进程正在服务的项目」判定：不同项目一律 NotOpen，
        // 绝不去别的项目目录里找同名 thread。
        assert_eq!(
            classify_target("/a/.yi-agent", "/b/.yi-agent", true),
            ControlTarget::NotOpen
        );
    }

    #[test]
    fn a_matching_project_with_the_session_present_is_a_session() {
        assert_eq!(
            classify_target("/a/.yi-agent", "/a/.yi-agent", true),
            ControlTarget::Session
        );
    }

    #[test]
    fn a_matching_project_without_the_session_is_not_open() {
        assert_eq!(
            classify_target("/a/.yi-agent", "/a/.yi-agent", false),
            ControlTarget::NotOpen
        );
    }

    #[tokio::test]
    async fn a_delivered_command_reaches_the_subscriber() {
        let inbox = LeadControlInbox::new();
        let mut rx = inbox.subscribe();
        let cmd = LeadControlCmd {
            project_key: "/a/.yi-agent".into(),
            kind: LeadControlKind::Reject,
            thread_id: "t1".into(),
            reason: "redo it".into(),
            request_id: "r1".into(),
        };
        assert!(inbox.send(cmd.clone()), "a subscriber must receive it");
        assert_eq!(rx.recv().await.unwrap(), cmd);
    }

    #[tokio::test]
    async fn an_undelivered_command_returns_false() {
        let inbox = LeadControlInbox::new();
        let cmd = LeadControlCmd {
            project_key: "/a/.yi-agent".into(),
            kind: LeadControlKind::Interrupt,
            thread_id: "t1".into(),
            reason: "stop".into(),
            request_id: "r1".into(),
        };
        // 无订阅者：返回 false，调用方据此回一条「主循环未在听」的错误。
        assert!(!inbox.send(cmd));
    }

    #[tokio::test]
    async fn resolving_a_registered_request_hands_back_the_outcome() {
        let replies = LeadControlReplies::default();
        let rx = replies.register("r1".into());
        assert!(replies.resolve("r1", Ok("done".into())));
        assert_eq!(rx.await.unwrap().unwrap(), "done");
    }

    #[tokio::test]
    async fn resolving_an_unknown_request_is_a_noop() {
        let replies = LeadControlReplies::default();
        assert!(!replies.resolve("nope", Ok("x".into())));
    }

    use yi_agent_core::Tool;

    #[tokio::test]
    async fn interrupt_reports_an_empty_reason_as_an_error() {
        let tool = LeadInterruptTool::new(
            LeadControlInbox::new(),
            LeadControlReplies::default(),
            "/a/.yi-agent".into(),
        );
        let out = tool
            .call(serde_json::json!({ "thread_id": "t1", "reason": "   " }))
            .await;
        assert!(out.is_error, "a blank reason must be rejected");
    }

    #[tokio::test]
    async fn interrupt_fails_when_no_one_is_listening() {
        // 无订阅者 = 主循环不在听：绝不能报成功。
        let tool = LeadInterruptTool::new(
            LeadControlInbox::new(),
            LeadControlReplies::default(),
            "/a/.yi-agent".into(),
        );
        let out = tool
            .call(serde_json::json!({ "thread_id": "t1", "reason": "stop" }))
            .await;
        assert!(out.is_error, "no subscriber must be an error, got {out:?}");
    }

    #[tokio::test]
    async fn interrupt_returns_the_main_loop_outcome() {
        let inbox = LeadControlInbox::new();
        let replies = LeadControlReplies::default();
        let mut rx = inbox.subscribe();
        let tool = LeadInterruptTool::new(inbox, replies.clone(), "/a/.yi-agent".into());
        // 扮演主循环：收到指令后回填失败（目标无活跃轮）。
        let outcome = tokio::spawn(async move {
            let cmd = rx.recv().await.unwrap();
            assert_eq!(cmd.kind, LeadControlKind::Interrupt);
            assert_eq!(cmd.thread_id, "t1");
            replies.resolve(&cmd.request_id, Err("目标没有活跃轮".into()));
        });
        let out = tool
            .call(serde_json::json!({ "thread_id": "t1", "reason": "stop" }))
            .await;
        outcome.await.unwrap();
        assert!(
            out.is_error,
            "a failed interruption must surface as an error"
        );
    }

    #[tokio::test]
    async fn reject_carries_the_reason_to_the_main_loop() {
        let inbox = LeadControlInbox::new();
        let replies = LeadControlReplies::default();
        let mut rx = inbox.subscribe();
        let tool = LeadRejectTool::new(inbox, replies.clone(), "/a/.yi-agent".into());
        let outcome = tokio::spawn(async move {
            let cmd = rx.recv().await.unwrap();
            assert_eq!(cmd.kind, LeadControlKind::Reject);
            assert_eq!(cmd.reason, "redo with tests");
            replies.resolve(&cmd.request_id, Ok("已打回".into()));
        });
        let out = tool
            .call(serde_json::json!({ "thread_id": "t2", "reason": "redo with tests" }))
            .await;
        outcome.await.unwrap();
        assert!(
            !out.is_error,
            "a successful reject must not be an error: {out:?}"
        );
        assert!(
            format!("{out:?}").contains("已打回")
                || serde_json::to_string(&out.content)
                    .unwrap()
                    .contains("已打回"),
            "the outcome must reach the tool result: {out:?}"
        );
    }
}
