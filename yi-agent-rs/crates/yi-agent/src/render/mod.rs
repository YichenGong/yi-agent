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

    /// 渲染权限请求并返回用户决策。
    /// 默认实现:拒绝(无法交互)。
    fn render_permission_request(
        &mut self,
        _req: &yi_agent_core::permission::PermissionRequest,
    ) -> yi_agent_core::permission::Decision {
        yi_agent_core::permission::Decision::Deny
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yi_agent_core::permission::{Decision, PermissionKind, PermissionRequest};

    /// A minimal renderer that uses all default implementations.
    struct DefaultRenderer;

    impl Renderer for DefaultRenderer {
        fn render_user_input(&mut self, _text: &str) {}
        fn render_agent_event(&mut self, _event: &AgentEvent) {}
        fn render_error(&mut self, _err: &AgentError) {}
        fn render_system(&mut self, _msg: &str) {}
    }

    #[test]
    fn default_render_permission_request_returns_deny() {
        let mut renderer = DefaultRenderer;
        let req = PermissionRequest {
            request_id: 1,
            tool_name: "bash".to_string(),
            tool_input: serde_json::json!({"command": "ls"}),
            prefix_suggestion: None,
            kind: PermissionKind::Normal,
        };
        let decision = renderer.render_permission_request(&req);
        assert!(matches!(decision, Decision::Deny));
    }
}
