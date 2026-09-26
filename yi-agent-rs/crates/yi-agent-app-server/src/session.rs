//! thread 级状态:app-server 内存中的 thread 会话。

use tokio::sync::mpsc;

/// 一次 turn 的输入(由 C4b 的 driver task 消费)。
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
    /// 向该 thread 的 driver task 投递 turn。
    pub(crate) prompt_tx: mpsc::Sender<TurnPrompt>,
    /// 请求中断当前 turn。
    pub(crate) interrupt_tx: mpsc::Sender<()>,
}
