//! 渲染层抽象：trait + 实现。

pub mod inline;

pub use inline::InlineRenderer;

use yi_agent_core::{AgentError, AgentEvent};

/// 渲染器 trait：消费事件并渲染到输出。
///
/// 只负责"渲染"，不持有 agent 状态、不驱动 agent。
/// 起步实现 `InlineRenderer`，将来可加 `TuiRenderer`（ratatui）。
pub trait Renderer {
    /// 渲染用户输入的 prompt（回显）
    fn render_user_input(&mut self, text: &str);
    /// 渲染 agent 事件流中的一个事件
    fn render_agent_event(&mut self, event: &AgentEvent);
    /// 渲染错误
    fn render_error(&mut self, err: &AgentError);
    /// 渲染系统消息（如中断提示、状态）
    fn render_system(&mut self, msg: &str);
}
