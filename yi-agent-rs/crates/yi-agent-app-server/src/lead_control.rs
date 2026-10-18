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

/// 写结果的登记表。形状照抄 `lead::LeadReplies`：投递方登记 `oneshot` 后 await，
/// 主循环执行完回填 `Ok(执行摘要)` 或 `Err(失败原因)`。
///
/// 结果是 `Result` 而非裸 `String`：写动作可能失败（目标无活跃轮、目标不在场），
/// 失败必须让组长看到，不能伪装成成功。
#[derive(Clone, Default)]
pub struct LeadControlReplies {
    waiting: Arc<Mutex<HashMap<String, oneshot::Sender<Result<String, String>>>>>,
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
}
