//! thread 级状态:app-server 内存中的 thread 会话。

use std::sync::Arc;
use std::sync::Mutex;

use tokio::sync::mpsc;

use crate::protocol::ThreadStatus;

/// 一次 turn 的输入(由 driver task 消费)。
#[derive(Debug)]
pub struct TurnPrompt {
    pub turn_id: String,
    pub prompt: String,
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
