//! 会话压缩：用 LLM 摘要旧消息，保留最近 N 轮。

use std::{collections::HashSet, sync::Arc};

use crate::{
    agent::{AgentConfig, AgentError, Session},
    message::{ContentBlock, Message, Role},
    provider::{Provider, ProviderRequest},
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

/// Default amount of real user context retained by compaction.
pub const DEFAULT_COMPACT_USER_BUDGET_TOKENS: usize = 20_000;
/// Default amount of complete raw tool context retained by compaction.
pub const DEFAULT_COMPACT_TOOL_BUDGET_TOKENS: usize = 12_000;

#[derive(Debug, Clone)]
struct ToolInteractionUnit {
    messages: Vec<Message>,
    token_estimate: usize,
}

/// A provider-independent compaction decision.
#[derive(Debug, Clone)]
pub struct CompactionPlan {
    retained_user: Message,
    retained_tool_suffix: Vec<Message>,
    retained_user_tokens: usize,
    retained_tool_tokens: usize,
}

impl CompactionPlan {
    pub fn retained_user_tokens(&self) -> usize {
        self.retained_user_tokens
    }

    pub fn retained_tool_tokens(&self) -> usize {
        self.retained_tool_tokens
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CompactError {
    #[error("compaction would not reduce history")]
    NoReduction,
    #[error("compacted history is invalid: {0}")]
    InvalidHistory(String),
}

/// Heuristic token estimate: ASCII text averages four characters per token;
/// non-ASCII text averages 1.5 characters per token.
fn estimate_text_tokens(text: &str) -> usize {
    let mut ascii = 0usize;
    let mut non_ascii = 0usize;
    for character in text.chars() {
        if character.is_ascii() {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
    }
    ascii.div_ceil(4) + (non_ascii * 2).div_ceil(3)
}

fn estimate_block_tokens(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text(text) => estimate_text_tokens(text),
        ContentBlock::ToolUse { name, input, .. } => {
            estimate_text_tokens(name) + estimate_text_tokens(&input.to_string())
        }
        ContentBlock::ToolResult { content, .. } => content.iter().map(estimate_block_tokens).sum(),
        ContentBlock::Image { .. } => 0,
    }
}

fn estimate_message_tokens(message: &Message) -> usize {
    message.content.iter().map(estimate_block_tokens).sum()
}

fn is_summary_message(message: &Message) -> bool {
    message.role == Role::Assistant
        && matches!(message.content.first(), Some(ContentBlock::Text(text)) if text.starts_with("[对话摘要]\n"))
}

fn select_retained_user_message(messages: &[Message], budget: usize) -> Option<(Message, usize)> {
    if budget == 0 {
        return None;
    }

    let mut selected = Vec::new();
    let mut used = 0usize;
    for message in messages.iter().rev() {
        if message.role != Role::User || is_summary_message(message) {
            continue;
        }
        let estimate = estimate_message_tokens(message);
        if used + estimate <= budget {
            used += estimate;
            selected.push(message);
        }
    }
    if selected.is_empty() {
        return None;
    }

    selected.reverse();
    let combined = selected
        .into_iter()
        .map(|message| {
            let text = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            format!("[用户消息]\n{text}")
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    Some((Message::user(combined), used))
}

fn tool_use_ids(message: &Message) -> Vec<String> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

fn tool_result_ids(message: &Message) -> Option<Vec<String>> {
    (message.role == Role::Tool).then(|| {
        message
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::ToolResult { tool_use_id, .. } => Ok(tool_use_id.clone()),
                _ => Err(()),
            })
            .collect::<Result<Vec<_>, _>>()
            .ok()
    })?
}

fn extract_complete_tool_units(messages: &[Message]) -> Vec<ToolInteractionUnit> {
    let mut units = Vec::new();
    let mut index = 0usize;
    while index < messages.len() {
        let assistant = &messages[index];
        let use_ids = if assistant.role == Role::Assistant {
            tool_use_ids(assistant)
        } else {
            Vec::new()
        };
        let expected: HashSet<_> = use_ids.iter().cloned().collect();
        if use_ids.is_empty() || expected.len() != use_ids.len() {
            index += 1;
            continue;
        }

        let mut next = index + 1;
        let mut unit_messages = vec![assistant.clone()];
        let mut seen = HashSet::new();
        let mut valid = true;
        while next < messages.len() && messages[next].role == Role::Tool {
            let Some(result_ids) = tool_result_ids(&messages[next]) else {
                valid = false;
                break;
            };
            for result_id in result_ids {
                if !expected.contains(&result_id) || !seen.insert(result_id) {
                    valid = false;
                    break;
                }
            }
            if !valid {
                break;
            }
            unit_messages.push(messages[next].clone());
            next += 1;
        }
        if valid && seen == expected {
            let token_estimate = unit_messages.iter().map(estimate_message_tokens).sum();
            units.push(ToolInteractionUnit {
                messages: unit_messages,
                token_estimate,
            });
            index = next;
        } else {
            index += 1;
        }
    }
    units
}

fn select_retained_tool_suffix(
    units: &[ToolInteractionUnit],
    budget: usize,
) -> (Vec<Message>, usize) {
    if budget == 0 {
        return (Vec::new(), 0);
    }

    let mut selected = Vec::new();
    let mut used = 0usize;
    for unit in units.iter().rev() {
        if selected.is_empty() && unit.token_estimate > budget {
            selected.push(unit);
            used = unit.token_estimate;
            break;
        }
        if used + unit.token_estimate > budget {
            break;
        }
        used += unit.token_estimate;
        selected.push(unit);
    }
    selected.reverse();
    (
        selected
            .into_iter()
            .flat_map(|unit| unit.messages.iter().cloned())
            .collect(),
        used,
    )
}

/// Build a compaction plan without calling a provider. `None` means a safe
/// replacement cannot make the message history shorter.
pub fn plan_compaction(
    messages: &[Message],
    user_budget: usize,
    tool_budget: usize,
) -> Option<CompactionPlan> {
    let (retained_user, retained_user_tokens) =
        select_retained_user_message(messages, user_budget)?;
    let (retained_tool_suffix, retained_tool_tokens) =
        select_retained_tool_suffix(&extract_complete_tool_units(messages), tool_budget);
    let plan = CompactionPlan {
        retained_user,
        retained_tool_suffix,
        retained_user_tokens,
        retained_tool_tokens,
    };
    let rebuilt = build_compacted_messages(&plan, "[规划摘要]").ok()?;
    (rebuilt.len() < messages.len()).then_some(plan)
}

/// Rebuild a provider-neutral, valid history from a plan and a rolling summary.
pub fn build_compacted_messages(
    plan: &CompactionPlan,
    summary: &str,
) -> Result<Vec<Message>, CompactError> {
    let summary = ContentBlock::Text(format!("[对话摘要]\n{summary}"));
    let mut messages = vec![plan.retained_user.clone()];
    if plan.retained_tool_suffix.is_empty() {
        messages.push(Message::assistant(vec![summary]));
    } else {
        let mut suffix = plan.retained_tool_suffix.clone();
        let Some(first) = suffix.first_mut() else {
            return Err(CompactError::NoReduction);
        };
        if first.role != Role::Assistant || tool_use_ids(first).is_empty() {
            return Err(CompactError::InvalidHistory(
                "tool suffix must start with assistant tool use".into(),
            ));
        }
        first.content.insert(0, summary);
        messages.extend(suffix);
    }
    validate_compacted_messages(&messages)?;
    Ok(messages)
}

/// Validate the message sequence consumed by both provider adapters.
pub fn validate_compacted_messages(messages: &[Message]) -> Result<(), CompactError> {
    if messages.first().map(|message| message.role) != Some(Role::User) {
        return Err(CompactError::InvalidHistory(
            "first message must be user".into(),
        ));
    }
    for pair in messages.windows(2) {
        if pair[0].role == Role::User && pair[1].role == Role::User {
            return Err(CompactError::InvalidHistory(
                "adjacent user messages".into(),
            ));
        }
        if pair[0].role == Role::Assistant && pair[1].role == Role::Assistant {
            return Err(CompactError::InvalidHistory(
                "adjacent assistant messages".into(),
            ));
        }
    }

    let mut outstanding: Option<HashSet<String>> = None;
    for message in messages {
        match message.role {
            Role::Assistant => {
                if outstanding.is_some() {
                    return Err(CompactError::InvalidHistory(
                        "tool use has no matching result".into(),
                    ));
                }
                let ids = tool_use_ids(message);
                if !ids.is_empty() {
                    let expected: HashSet<_> = ids.iter().cloned().collect();
                    if expected.len() != ids.len() {
                        return Err(CompactError::InvalidHistory("duplicate tool use id".into()));
                    }
                    outstanding = Some(expected);
                }
            }
            Role::Tool => {
                let Some(expected) = outstanding.as_mut() else {
                    return Err(CompactError::InvalidHistory("orphan tool result".into()));
                };
                let Some(ids) = tool_result_ids(message) else {
                    return Err(CompactError::InvalidHistory(
                        "tool message contains non-result content".into(),
                    ));
                };
                if ids.is_empty() {
                    return Err(CompactError::InvalidHistory(
                        "empty tool result message".into(),
                    ));
                }
                for id in ids {
                    if !expected.remove(&id) {
                        return Err(CompactError::InvalidHistory(
                            "unknown or duplicate tool result".into(),
                        ));
                    }
                }
                if expected.is_empty() {
                    outstanding = None;
                }
            }
            Role::User | Role::System if outstanding.is_some() => {
                return Err(CompactError::InvalidHistory(
                    "tool use has no matching result".into(),
                ));
            }
            Role::User | Role::System => {}
        }
    }
    if outstanding.is_some() {
        return Err(CompactError::InvalidHistory(
            "tool use has no matching result".into(),
        ));
    }
    Ok(())
}

/// 将消息列表格式化为纯文本对话（用于摘要请求）。
pub fn format_messages_for_summary(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        let role = match msg.role {
            Role::User => "用户",
            Role::Assistant => "助手",
            Role::Tool => "工具结果",
            Role::System => "系统",
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
        if messages[i].role == Role::User {
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
    // 摘要使用 Assistant 角色,避免与后续 recent_messages 首条 User 消息
    // 产生连续 user 消息,Anthropic API 会拒绝这种序列。
    new_session.push(Message::assistant(vec![ContentBlock::Text(format!(
        "[对话摘要]\n{summary_text}"
    ))]));
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
        use crate::provider::{ProviderError, ProviderEvent};
        use async_trait::async_trait;
        use futures::stream::{BoxStream, StreamExt};

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
        assert_eq!(first_recent.role, Role::User);

        // The old part must not end with an orphaned tool_use
        // (verified by the fact that the split point is a User message)
    }

    #[tokio::test]
    async fn compact_summary_is_assistant_role() {
        use crate::provider::{ProviderError, ProviderEvent};
        use async_trait::async_trait;
        use futures::stream::{BoxStream, StreamExt};

        struct SummaryProvider;
        #[async_trait]
        impl Provider for SummaryProvider {
            async fn call_stream(
                &self,
                _req: ProviderRequest,
            ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
                Ok(
                    futures::stream::iter(vec![ProviderEvent::TextDelta("summary text".into())])
                        .boxed(),
                )
            }
        }

        // Build a session with enough turns to trigger compact.
        let mut session = Session::new();
        session.push(Message::user("prompt 1"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply 1".into(),
        )]));
        session.push(Message::user("prompt 2"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply 2".into(),
        )]));

        let provider: Arc<dyn Provider> = Arc::new(SummaryProvider);
        let config = AgentConfig::default();

        let result = compact_session(&provider, &config, &session, 1).await;
        assert!(result.is_ok());
        let new_session = result.unwrap();

        // First message must be Assistant (the summary), not User.
        assert_eq!(
            new_session.messages()[0].role,
            Role::Assistant,
            "summary should be Assistant role"
        );

        // No two consecutive User messages anywhere in the new session
        // (Anthropic API requires strict alternation).
        for w in new_session.messages().windows(2) {
            assert!(
                !(w[0].role == Role::User && w[1].role == Role::User),
                "consecutive User messages found"
            );
        }
    }

    fn planned_tool_use(id: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.into(),
            name: "read".into(),
            input: serde_json::json!({"path": "src/lib.rs"}),
        }
    }

    fn planned_tool_result(id: &str, text: impl Into<String>) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: id.into(),
            content: vec![ContentBlock::Text(text.into())],
            is_error: false,
        }
    }

    fn tool_unit(id: &str, result: impl Into<String>) -> [Message; 2] {
        [
            Message::assistant(vec![planned_tool_use(id)]),
            Message::tool_results(vec![planned_tool_result(id, result)]),
        ]
    }

    #[test]
    fn single_user_tool_loop_plan_preserves_task_summary_and_suffix() {
        let mut messages = vec![Message::user("fix the test")];
        for (id, result) in [("t1", "first"), ("t2", "second"), ("t3", "third")] {
            messages.extend(tool_unit(id, result));
        }

        let plan = plan_compaction(&messages, 20_000, 7).expect("must compact");
        let compacted = build_compacted_messages(&plan, "checkpoint").unwrap();

        assert!(compacted.len() < messages.len());
        assert_eq!(compacted[0].role, Role::User);
        assert!(matches!(
            &compacted[0].content[0],
            ContentBlock::Text(text) if text.contains("fix the test")
        ));
        assert!(compacted[1].content.iter().any(|block| {
            matches!(block, ContentBlock::Text(text) if text == "[对话摘要]\ncheckpoint")
        }));
        assert!(
            compacted[1]
                .content
                .iter()
                .any(|block| { matches!(block, ContentBlock::ToolUse { id, .. } if id == "t3") })
        );
        assert!(validate_compacted_messages(&compacted).is_ok());
    }

    #[test]
    fn retained_tool_suffix_uses_token_budget_not_unit_count() {
        let mut messages = vec![Message::user("task")];
        messages.extend(tool_unit("old", "x".repeat(800)));
        messages.extend(tool_unit("new", "fresh result"));
        messages.extend(tool_unit("latest", "latest result"));

        let compacted = build_compacted_messages(
            &plan_compaction(&messages, 20_000, 21).expect("must compact"),
            "summary",
        )
        .unwrap();

        let retained_ids = compacted
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(retained_ids, vec!["new", "latest"]);
    }

    #[test]
    fn newest_oversized_tool_unit_is_retained_whole() {
        let mut messages = vec![Message::user("task")];
        messages.extend(tool_unit("old", "old"));
        messages.extend(tool_unit("new", "x".repeat(800)));

        let compacted = build_compacted_messages(
            &plan_compaction(&messages, 20_000, 1).expect("must compact"),
            "summary",
        )
        .unwrap();

        assert_eq!(compacted.len(), 3);
        assert!(
            matches!(&compacted[1].content[1], ContentBlock::ToolUse { id, .. } if id == "new")
        );
        assert!(
            matches!(&compacted[2].content[0], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "new")
        );
    }

    #[test]
    fn parallel_tool_uses_and_results_are_retained_as_one_unit() {
        let mut messages = vec![Message::user("task")];
        messages.extend(tool_unit("old", "old"));
        messages.push(Message::assistant(vec![
            planned_tool_use("one"),
            planned_tool_use("two"),
        ]));
        messages.push(Message::tool_results(vec![
            planned_tool_result("one", "one result"),
            planned_tool_result("two", "two result"),
        ]));

        let compacted = build_compacted_messages(
            &plan_compaction(&messages, 20_000, 21).expect("must compact"),
            "summary",
        )
        .unwrap();
        let ids = compacted[1]
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["one", "two"]);
        assert!(
            matches!(&compacted[2].content[..], [ContentBlock::ToolResult { tool_use_id, .. }, ContentBlock::ToolResult { tool_use_id: second, .. }] if tool_use_id == "one" && second == "two")
        );
    }

    #[test]
    fn retained_users_are_newest_first_and_never_truncated() {
        let messages = vec![
            Message::user("x".repeat(800)),
            Message::assistant(vec![ContentBlock::Text("acknowledged".into())]),
            Message::user("recent task"),
        ];
        let compacted = build_compacted_messages(
            &plan_compaction(&messages, 10, 0).expect("must compact"),
            "summary",
        )
        .unwrap();
        assert!(
            matches!(&compacted[0].content[0], ContentBlock::Text(text) if text.contains("recent task") && !text.contains(&"x".repeat(800)))
        );
    }

    #[test]
    fn existing_summary_is_not_retained_as_real_user_message() {
        let messages = vec![
            Message::user("task"),
            Message::assistant(vec![ContentBlock::Text(
                "[对话摘要]\nold checkpoint".into(),
            )]),
            Message::assistant(vec![ContentBlock::Text("internal note".into())]),
        ];
        let compacted = build_compacted_messages(
            &plan_compaction(&messages, 20_000, 0).expect("must compact"),
            "new checkpoint",
        )
        .unwrap();
        assert!(
            matches!(&compacted[0].content[0], ContentBlock::Text(text) if !text.contains("old checkpoint"))
        );
    }

    #[test]
    fn empty_tool_suffix_builds_user_then_summary_assistant() {
        let messages = vec![
            Message::user("first request"),
            Message::assistant(vec![ContentBlock::Text("reply".into())]),
            Message::user("second request"),
        ];
        let compacted = build_compacted_messages(
            &plan_compaction(&messages, 20_000, 0).expect("must compact"),
            "summary",
        )
        .unwrap();
        assert_eq!(compacted.len(), 2);
        assert_eq!(compacted[0].role, Role::User);
        assert_eq!(compacted[1].role, Role::Assistant);
    }

    #[test]
    fn validator_rejects_orphan_results_unmatched_uses_and_adjacent_roles() {
        let orphan = vec![
            Message::user("task"),
            Message::tool_results(vec![planned_tool_result("missing", "result")]),
        ];
        assert!(matches!(
            validate_compacted_messages(&orphan),
            Err(CompactError::InvalidHistory(_))
        ));
        let unmatched = vec![
            Message::user("task"),
            Message::assistant(vec![planned_tool_use("unmatched")]),
        ];
        assert!(matches!(
            validate_compacted_messages(&unmatched),
            Err(CompactError::InvalidHistory(_))
        ));
        assert!(matches!(
            validate_compacted_messages(&[Message::user("one"), Message::user("two")]),
            Err(CompactError::InvalidHistory(_))
        ));
    }

    #[test]
    fn plan_returns_none_when_reconstruction_cannot_shrink_history() {
        assert!(plan_compaction(&[Message::user("task")], 20_000, 12_000).is_none());
    }

    #[tokio::test]
    async fn compact_session_with_few_messages_returns_clone() {
        use crate::provider::{ProviderError, ProviderEvent};
        use async_trait::async_trait;
        use futures::stream::{BoxStream, StreamExt};

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
