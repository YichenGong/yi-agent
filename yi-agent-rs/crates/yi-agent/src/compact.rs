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

/// 执行 compact：摘要旧消息 + 保留最近 N 轮，返回新 Session。
pub async fn compact_session(
    provider: &Arc<dyn Provider>,
    config: &AgentConfig,
    session: &Session,
    keep_turns: u32,
) -> Result<Session, AgentError> {
    let messages = session.messages();
    let keep_count = (keep_turns as usize) * 2;

    if messages.len() <= keep_count {
        return Ok(session.clone());
    }

    let (old_messages, recent_messages) = messages.split_at(messages.len() - keep_count);

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
