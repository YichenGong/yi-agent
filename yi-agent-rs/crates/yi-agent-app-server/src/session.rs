//! thread 级状态:app-server 内存中的 thread 会话。

use std::sync::Arc;
use std::sync::Mutex;

use tokio::sync::{mpsc, oneshot};

use crate::protocol::ThreadStatus;

/// 一次 turn 的输入(由 driver task 消费)。
///
/// 不用 `Debug`:字段含 `RuntimeBinding`,其内部持有 daemon 句柄与 std `Mutex`,
/// 既不便打印也不应被打印。
pub struct TurnPrompt {
    pub turn_id: String,
    pub prompt: String,
    /// 该 thread 若已 attach 到项目 runtime,则带上它的 binding,在**首个 turn** 激活。
    ///
    /// 激活是同步 socket 调用,放在 driver 里(而不是请求循环)才不会让一个 thread
    /// 卡住所有 thread 的请求处理;用首个 turn 的真正 prompt 作 objective,因为
    /// objective 会被写进 root 任务。带上 binding 而非某个快照,这样激活前能先探活、
    /// 必要时重建 runtime。
    pub activate: Option<Arc<yi_agent_subagent::binding::RuntimeBinding>>,
}

/// 一条中途追加的用户消息,投递给该 thread 的 driver。
#[derive(Debug)]
pub struct InterjectionRequest {
    /// 这条消息要并入的 turn。driver 只接受与当前 turn 匹配的请求。
    pub turn_id: String,
    /// 协议层 item id,在 RPC 入口铸好,便于客户端精确对账。
    pub interjection_id: String,
    pub text: String,
}

/// 发给 driver 的会话级命令（清空 / 压缩）。
///
/// 必须走 driver 而不是主循环：`Agent::session()` 只返回 `Session` 的 clone，
/// 修改 session 的可变访问权只存在于持有 agent 的 driver 内部。
pub enum SessionCommand {
    /// 清空该 thread 的上下文，并截断其持久化对话日志。
    Clear {
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// 压缩该 thread 的上下文。
    Compact {
        reply: oneshot::Sender<CompactOutcome>,
    },
}

/// `/compact` 的三种结果。`NotReduced` 是"历史太短、无需压缩"，**不是错误**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactOutcome {
    Compacted,
    NotReduced,
    Failed(String),
}

/// app-server 侧的一个 thread。
pub struct ThreadSession {
    pub thread_id: String,
    pub cwd: String,
    pub model: String,
    /// 当前活跃 turn 的 id;无活跃 turn 时为 None。
    pub active_turn_id: Option<String>,
    /// 该线程的运行时自主权开关(与沙箱、权限层共享同一 `Arc`)。
    pub yolo: yi_agent_core::autonomy::YoloSwitch,
    /// 向该 thread 的 driver task 投递 turn。
    pub(crate) prompt_tx: mpsc::Sender<TurnPrompt>,
    /// 请求中断当前 turn(携带目标 turn id,driver 据此丢弃残留信号)。
    pub(crate) interrupt_tx: mpsc::Sender<String>,
    /// 向该 thread 的 driver 投递中途追加消息(与 prompt_tx/interrupt_tx 同类)。
    pub(crate) interject_tx: mpsc::Sender<InterjectionRequest>,
    /// 会话级命令（清空 / 压缩）的投递端；与 prompt_tx/interrupt_tx 同类。
    pub(crate) session_tx: mpsc::Sender<SessionCommand>,
    /// 该 thread 的 store;driver 与主循环的 rename/delete 共用同一实例,
    /// 以共享 `ThreadStore.meta_lock`(否则并发 touch/rename 会丢更新)。
    pub store: Arc<crate::thread_store::ThreadStore>,
    /// 该 thread 的实时状态。driver 与主循环共享同一句柄：driver 更新并推送
    /// `thread/status/updated`，主循环在 `thread/list(:All)` 里读取。用共享句柄
    /// 而非 `TurnEvent`，避免扰动 `interrupt_and_wait_for_persist` 的收事件循环。
    pub status: Arc<Mutex<ThreadStatus>>,
}

impl ThreadSession {
    /// 新建一个共享状态句柄（初值 `Idle`）。
    pub fn new_status() -> Arc<Mutex<ThreadStatus>> {
        Arc::new(Mutex::new(ThreadStatus::Idle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `SessionCommand` 是 driver 与主循环之间的请求/应答桥：reply 通道必须
    /// 能原样把结果带回调用方（clear 的结果、compact 的三态）。
    #[tokio::test]
    async fn session_command_replies_round_trip() {
        let (tx, mut rx) = mpsc::channel::<SessionCommand>(1);
        let (reply, answer) = oneshot::channel();
        tx.send(SessionCommand::Clear { reply }).await.unwrap();
        let command = rx.recv().await.expect("command must arrive");
        match command {
            SessionCommand::Clear { reply } => reply.send(Ok(())).unwrap(),
            SessionCommand::Compact { .. } => panic!("expected Clear"),
        }
        assert_eq!(answer.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn compact_outcome_distinguishes_the_three_states() {
        let (tx, mut rx) = mpsc::channel::<SessionCommand>(1);
        let (reply, answer) = oneshot::channel();
        tx.send(SessionCommand::Compact { reply }).await.unwrap();
        let SessionCommand::Compact { reply } = rx.recv().await.unwrap() else {
            panic!("expected Compact");
        };
        reply.send(CompactOutcome::NotReduced).unwrap();
        assert_eq!(answer.await.unwrap(), CompactOutcome::NotReduced);
        assert_ne!(CompactOutcome::Compacted, CompactOutcome::NotReduced);
    }
}
