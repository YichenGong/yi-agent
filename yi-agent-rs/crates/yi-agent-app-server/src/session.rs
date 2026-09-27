//! thread 级状态:app-server 内存中的 thread 会话。

use std::sync::Arc;

use tokio::sync::mpsc;

/// 一次 turn 的输入(由 driver task 消费)。
#[derive(Debug)]
pub struct TurnPrompt {
    pub turn_id: String,
    pub prompt: String,
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
    /// 该 thread 的 store;driver 与主循环的 rename/delete 共用同一实例,
    /// 以共享 `ThreadStore.meta_lock`(否则并发 touch/rename 会丢更新)。
    pub store: Arc<crate::thread_store::ThreadStore>,
}
