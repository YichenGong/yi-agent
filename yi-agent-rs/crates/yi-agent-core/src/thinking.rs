//! 思考块的剥离与保真辅助。
//!
//! 集中一处：`preserve_thinking` 开关与 compaction 前缀兼容都用同一判据，
//! 避免各 provider 各自实现导致漂移。

use crate::message::{ContentBlock, Message, Role};

/// 剥离所有 thinking 块。
///
/// 用途：`preserve_thinking=false` 时清历史；compaction 时清被压缩区间。
/// 空壳消息**保留**（不删消息），避免破坏 user/assistant 交替与 tool_use 配对——
/// 那会让 provider 请求非法。
pub fn strip_thinking(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .map(|m| Message {
            role: m.role,
            content: m
                .content
                .iter()
                .filter(|b| !matches!(b, ContentBlock::Thinking { .. }))
                .cloned()
                .collect(),
        })
        .collect()
}

/// 剥离所有 thinking 块，但保留**最后一条 assistant 消息**中的。
///
/// 用途：轮内硬保证——当前轮工具循环的凭证必须回传，历史轮的可释放。
pub fn strip_thinking_except_last_assistant(messages: &[Message]) -> Vec<Message> {
    let last_assistant = messages.iter().rposition(|m| m.role == Role::Assistant);

    messages
        .iter()
        .enumerate()
        .map(|(i, m)| {
            if Some(i) == last_assistant {
                m.clone()
            } else {
                Message {
                    role: m.role,
                    content: m
                        .content
                        .iter()
                        .filter(|b| !matches!(b, ContentBlock::Thinking { .. }))
                        .cloned()
                        .collect(),
                }
            }
        })
        .collect()
}

/// 该消息是否含 tool_use 块。DeepSeek 的 `reasoning_content` 仅在有工具调用时回传。
///
/// 命名带 `message_` 前缀以避免与 `agent.rs` 的私有 `has_tool_use` 撞名。
pub fn message_has_tool_use(msg: &Message) -> bool {
    msg.content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ThinkingToken;

    fn thinking(text: &str) -> ContentBlock {
        ContentBlock::Thinking {
            text: text.into(),
            token: ThinkingToken::Signature(format!("sig-{text}")),
        }
    }

    #[test]
    fn strip_thinking_removes_all_blocks_and_keeps_others() {
        let msgs = vec![
            Message::user("hi"),
            Message::assistant(vec![thinking("a"), ContentBlock::Text("t1".into())]),
            Message::assistant(vec![thinking("b")]),
        ];
        let out = strip_thinking(&msgs);
        assert_eq!(out[1].content.len(), 1, "thinking must be gone");
        assert!(matches!(out[1].content[0], ContentBlock::Text(_)));
        assert!(
            out[2].content.is_empty(),
            "block-less message stays, emptied"
        );
    }

    #[test]
    fn strip_except_last_assistant_keeps_only_that_one() {
        let msgs = vec![
            Message::assistant(vec![thinking("old")]),
            Message::user("hi"),
            Message::assistant(vec![thinking("current"), ContentBlock::Text("t".into())]),
        ];
        let out = strip_thinking_except_last_assistant(&msgs);
        assert!(
            out[0].content.is_empty(),
            "earlier thinking stripped, block-less message stays emptied"
        );
        assert!(
            matches!(out[2].content[0], ContentBlock::Thinking { .. }),
            "last assistant thinking preserved"
        );
    }

    #[test]
    fn message_has_tool_use_detects_tool_use_blocks() {
        let with = Message::assistant(vec![ContentBlock::ToolUse {
            id: "t1".into(),
            name: "read".into(),
            input: serde_json::json!({}),
        }]);
        let without = Message::assistant(vec![ContentBlock::Text("hi".into())]);
        assert!(message_has_tool_use(&with));
        assert!(!message_has_tool_use(&without));
    }
}
