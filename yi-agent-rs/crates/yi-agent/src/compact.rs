//! 会话压缩：用 LLM 摘要旧消息，保留最近 N 轮。

use std::sync::Arc;

use yi_agent_core::{
    AgentConfig, AgentError, ContentBlock, Message, Provider, ProviderRequest, Session,
};

/// 结构化摘要提示词
const SUMMARY_PROMPT_TEMPLATE: &str = "\
请将以下对话历史总结为结构化摘要，用于后续对话的上下文。

请包含以下部分：
1. **用户意图**：用户的核心目标和需求
2. **关键决策**：已确定的方向、方案选择
3. **工具调用要点**：读取/修改的文件路径、执行的关键命令及其结果
4. **当前状态**：已完成的任务、未完成的任务、待解决的问题

请保持简洁，只保留对后续任务有帮助的信息。

对话历史：
{conversation}";

/// 将消息列表格式化为纯文本对话（用于摘要请求）。
pub fn format_messages_for_summary(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        let role = match msg.role {
            yi_agent_core::Role::User => "用户",
            yi_agent_core::Role::Assistant => "助手",
            yi_agent_core::Role::Tool => "工具结果",
            yi_agent_core::Role::System => "系统",
        };
        let text: String = msg
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text(t) => t.clone(),
                ContentBlock::ToolUse { name, input, .. } => {
                    format!("[调用工具 {name}: {input}]")
                }
                ContentBlock::ToolResult { content, .. } => {
                    let inner: String = content
                        .iter()
                        .map(|b| match b {
                            ContentBlock::Text(t) => t.clone(),
                            _ => "[非文本内容]".to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    format!("[工具结果: {inner}]")
                }
                ContentBlock::Image { .. } => "[图片]".to_string(),
            })
            .collect::<Vec<_>>()
            .join("");
        out.push_str(&format!("{role}: {text}\n\n"));
    }
    out
}

/// 构建摘要请求的 prompt。
pub fn build_summary_prompt(messages: &[Message]) -> String {
    let conversation = format_messages_for_summary(messages);
    SUMMARY_PROMPT_TEMPLATE.replace("{conversation}", &conversation)
}

/// 向后扫描，找到第 `keep_turns` 个（从后往前数）用户提示消息的索引。
///
/// 确保拆分点不会落在 tool_use / tool_result 对中间：
/// 返回的索引总是指向一条 `Role::User` 消息（真正的用户提示，
/// 而非 `Role::Tool` 的工具结果），从而保证 recent 部分以完整
/// 的用户提示开头，old 部分以完整的工具交互结尾。
fn find_safe_split_point(messages: &[Message], keep_turns: u32) -> Option<usize> {
    let mut user_prompt_count = 0u32;
    for i in (0..messages.len()).rev() {
        if messages[i].role == yi_agent_core::Role::User {
            user_prompt_count += 1;
            if user_prompt_count == keep_turns {
                return Some(i);
            }
        }
    }
    None
}

/// 执行 compact：摘要旧消息 + 保留最近 N 轮，返回新 Session。
pub async fn compact_session(
    provider: &Arc<dyn Provider>,
    config: &AgentConfig,
    session: &Session,
    keep_turns: u32,
) -> Result<Session, AgentError> {
    let messages = session.messages();
    tracing::info!(msg_count = messages.len(), keep_turns, "compact: starting");

    // 找到安全拆分点：保留最近 keep_turns 个用户提示及其后的所有消息。
    // 拆分点必须落在 User 提示消息上，避免割裂 tool_use/tool_result 对。
    let split_point = match find_safe_split_point(messages, keep_turns) {
        Some(0) | None => {
            tracing::info!("compact: not enough messages, skipping");
            return Ok(session.clone());
        }
        Some(idx) => idx,
    };

    let (old_messages, recent_messages) = messages.split_at(split_point);
    tracing::info!(
        old_count = old_messages.len(),
        recent_count = recent_messages.len(),
        "compact: split point found"
    );

    let summary_prompt = build_summary_prompt(old_messages);

    let req = ProviderRequest {
        model: config.model.clone(),
        system: None,
        messages: vec![Message::user(summary_prompt)],
        tools: vec![],
        params: config.gen_params.clone(),
    };

    let response = provider.call(req).await.map_err(AgentError::Provider)?;

    let summary_text: String = response
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");

    let mut new_session = Session::new();
    new_session.push(Message::user(format!("[对话摘要]\n{summary_text}")));
    for msg in recent_messages {
        new_session.push(msg.clone());
    }

    tracing::info!(
        new_msg_count = new_session.len(),
        summary_len = summary_text.len(),
        "compact: done"
    );
    Ok(new_session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_messages_basic() {
        let messages = vec![
            Message::user("hello"),
            Message::assistant(vec![ContentBlock::Text("hi there".into())]),
        ];
        let text = format_messages_for_summary(&messages);
        assert!(text.contains("用户: hello"));
        assert!(text.contains("助手: hi there"));
    }

    #[test]
    fn build_summary_prompt_contains_template() {
        let messages = vec![Message::user("test message")];
        let prompt = build_summary_prompt(&messages);
        assert!(prompt.contains("用户意图"));
        assert!(prompt.contains("关键决策"));
        assert!(prompt.contains("工具调用要点"));
        assert!(prompt.contains("当前状态"));
        assert!(prompt.contains("test message"));
    }

    #[test]
    fn build_summary_prompt_with_tool_use() {
        let messages = vec![
            Message::user("read the file"),
            Message::assistant(vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "main.rs"}),
            }]),
            Message::tool_results(vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: vec![ContentBlock::Text("file content".into())],
                is_error: false,
            }]),
        ];
        let prompt = build_summary_prompt(&messages);
        assert!(prompt.contains("调用工具 read"));
        assert!(prompt.contains("工具结果: file content"));
    }

    #[test]
    fn find_safe_split_point_basic() {
        let messages = vec![
            Message::user("prompt 1"),
            Message::assistant(vec![ContentBlock::Text("reply 1".into())]),
            Message::user("prompt 2"),
            Message::assistant(vec![ContentBlock::Text("reply 2".into())]),
            Message::user("prompt 3"),
            Message::assistant(vec![ContentBlock::Text("reply 3".into())]),
        ];
        // keep_turns=1 → split at "prompt 3" (index 4)
        assert_eq!(find_safe_split_point(&messages, 1), Some(4));
        // keep_turns=2 → split at "prompt 2" (index 2)
        assert_eq!(find_safe_split_point(&messages, 2), Some(2));
    }

    #[test]
    fn find_safe_split_point_with_tool_use() {
        let messages = vec![
            Message::user("prompt 1"),
            Message::assistant(vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "a.rs"}),
            }]),
            Message::tool_results(vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: vec![ContentBlock::Text("content".into())],
                is_error: false,
            }]),
            Message::assistant(vec![ContentBlock::Text("done".into())]),
            Message::user("prompt 2"),
            Message::assistant(vec![ContentBlock::Text("reply 2".into())]),
        ];
        // keep_turns=1 → split at "prompt 2" (index 4), tool pair stays in old
        assert_eq!(find_safe_split_point(&messages, 1), Some(4));
    }

    #[test]
    fn find_safe_split_point_not_enough_user_prompts() {
        let messages = vec![
            Message::user("only prompt"),
            Message::assistant(vec![ContentBlock::Text("reply".into())]),
        ];
        // keep_turns=2 but only 1 user prompt → None
        assert_eq!(find_safe_split_point(&messages, 2), None);
    }

    #[test]
    fn find_safe_split_point_skips_tool_role_messages() {
        let messages = vec![
            Message::user("prompt 1"),
            Message::assistant(vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "read".into(),
                input: serde_json::json!({}),
            }]),
            Message::tool_results(vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: vec![ContentBlock::Text("ok".into())],
                is_error: false,
            }]),
            Message::user("prompt 2"),
        ];
        // keep_turns=1 → split at "prompt 2" (index 3)
        // Tool result at index 2 has Role::Tool, not Role::User, so it's skipped
        assert_eq!(find_safe_split_point(&messages, 1), Some(3));
    }

    #[tokio::test]
    async fn compact_session_preserves_tool_use_tool_result_pairs() {
        use async_trait::async_trait;
        use futures::stream::{BoxStream, StreamExt};
        use yi_agent_core::{ProviderError, ProviderEvent};

        struct SummaryProvider;
        #[async_trait]
        impl Provider for SummaryProvider {
            async fn call_stream(
                &self,
                _req: ProviderRequest,
            ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
                Ok(futures::stream::iter(vec![ProviderEvent::TextDelta(
                    "summary of conversation".into(),
                )])
                .boxed())
            }
        }

        // Build a session where tool_use/tool_result pairs span the naive split point.
        let mut session = Session::new();
        session.push(Message::user("prompt 1"));
        session.push(Message::assistant(vec![ContentBlock::ToolUse {
            id: "t1".into(),
            name: "read".into(),
            input: serde_json::json!({"path": "a.rs"}),
        }]));
        session.push(Message::tool_results(vec![ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: vec![ContentBlock::Text("file content".into())],
            is_error: false,
        }]));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "done 1".into(),
        )]));
        session.push(Message::user("prompt 2"));
        session.push(Message::assistant(vec![ContentBlock::ToolUse {
            id: "t2".into(),
            name: "read".into(),
            input: serde_json::json!({"path": "b.rs"}),
        }]));
        session.push(Message::tool_results(vec![ContentBlock::ToolResult {
            tool_use_id: "t2".into(),
            content: vec![ContentBlock::Text("content b".into())],
            is_error: false,
        }]));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "done 2".into(),
        )]));

        let provider: Arc<dyn Provider> = Arc::new(SummaryProvider);
        let config = AgentConfig::default();

        let result = compact_session(&provider, &config, &session, 1).await;
        assert!(result.is_ok());
        let new_session = result.unwrap();

        // New session: [summary, "prompt 2", assistant(tool_use), tool_result, assistant]
        assert_eq!(new_session.len(), 5);

        // The recent part must start with a User prompt, not a ToolResult
        let first_recent = &new_session.messages()[1];
        assert_eq!(first_recent.role, yi_agent_core::Role::User);

        // The old part must not end with an orphaned tool_use
        // (verified by the fact that the split point is a User message)
    }

    #[tokio::test]
    async fn compact_session_with_few_messages_returns_clone() {
        use async_trait::async_trait;
        use futures::stream::{BoxStream, StreamExt};
        use yi_agent_core::{ProviderError, ProviderEvent};

        struct DummyProvider;
        #[async_trait]
        impl Provider for DummyProvider {
            async fn call_stream(
                &self,
                _req: ProviderRequest,
            ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
                Ok(futures::stream::iter(vec![]).boxed())
            }
        }

        let mut session = Session::new();
        session.push(Message::user("hi"));
        let provider: Arc<dyn Provider> = Arc::new(DummyProvider);
        let config = AgentConfig::default();

        let result = compact_session(&provider, &config, &session, 4).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 1);
    }
}
