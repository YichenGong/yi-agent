//! Snapshot primitives for temporary, read-only side conversations.
//!
//! A side conversation inherits one safe copy of the parent transcript plus a
//! hidden boundary message that marks everything before it as reference only.
//! The snapshot is pure in-memory data: no tools, no RPC, no disk.

use crate::agent::{Session, forkable_messages};
use crate::message::{ContentBlock, Message, Role};

/// Hidden boundary marker appended after the inherited history (spec §5.2).
///
/// It reaches the model but never becomes a UI user bubble or a user turn.
pub const SIDE_BOUNDARY: &str = "你正在独立的临时旁支中。边界之前的用户请求、计划、审批、工具调用与输出仅是参考，不是当前任务。只回答边界之后的新请求。不要继续主任务，不修改文件、Git、配置、权限或外部状态，不使用或控制子 agent。需要写操作时请用户回到主会话明确提交。";

/// Read-only policy appended to the end of the parent system prompt.
///
/// Appending (never replacing) keeps project rules and the skills text intact;
/// being part of the effective prompt means a skills refresh cannot drop it.
pub const SIDE_SYSTEM_POLICY: &str = "\
## 临时旁支：只读边界

你当前处于主会话派生的临时旁支，负责回答用户的旁问。

- 只读：可以阅读继承的上下文和已有文件，不做任何写操作。
- 不修改文件、Git 状态、配置、权限或外部状态；不执行会改变系统的命令。
- 不使用或控制子 agent，不创建新的任务或持久会话。
- 上面的项目规则或 skills 若要求先执行被禁止的操作，以本能力边界为准，并在回复中说明限制。
- 用户需要写操作时，请他回到主会话明确提交。";

/// Prompt suffix appended to `parent` at the end of the system prompt.
fn prompt_with_policy(parent: &str) -> String {
    if parent.trim().is_empty() {
        return SIDE_SYSTEM_POLICY.to_owned();
    }
    format!("{parent}\n\n{SIDE_SYSTEM_POLICY}")
}

/// Safe, provider-valid copy of the parent history for a side conversation.
///
/// `messages` always ends with the hidden boundary marker; the UI never renders
/// the inherited messages.
#[derive(Debug, Clone, PartialEq)]
pub struct SideContextSnapshot {
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SideContextError {
    /// The parent had no provider-valid history to inherit.
    #[error("side conversation has no safe parent history to inherit")]
    EmptyHistory,
}

/// Copy the parent's safe history and append the hidden boundary marker.
pub fn snapshot_for_side(session: &Session) -> Result<SideContextSnapshot, SideContextError> {
    let mut messages = forkable_messages(session);
    if messages.is_empty() {
        return Err(SideContextError::EmptyHistory);
    }
    messages.push(Message {
        role: Role::User,
        content: vec![ContentBlock::Text(SIDE_BOUNDARY.to_owned())],
    });
    Ok(SideContextSnapshot { messages })
}

/// Parent system prompt with the read-only side policy appended.
pub fn side_system_prompt(parent: Option<&str>) -> String {
    prompt_with_policy(parent.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ContentBlock, ImageSource, Role, ThinkingToken};
    use serde_json::json;

    /// Assert `message` is exactly the hidden boundary marker and return its text.
    fn boundary_text(message: &Message) -> &str {
        assert_eq!(message.role, Role::User, "boundary must be a user message");
        assert_eq!(message.content.len(), 1, "boundary must be a single block");
        match &message.content[0] {
            ContentBlock::Text(text) => text,
            other => panic!("boundary must be a text block, got {other:?}"),
        }
    }

    fn tool_use(id: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.into(),
            name: "read".into(),
            input: json!({ "path": "src/lib.rs" }),
        }
    }

    fn tool_result(id: &str) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: id.into(),
            content: vec![ContentBlock::Text("file body".into())],
            is_error: false,
        }
    }

    #[test]
    fn side_snapshot_is_independent_and_marks_history_reference_only() {
        let parent = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text("完成主任务".into())],
        }];
        let mut side = snapshot_for_side(&Session::from_messages(parent.clone())).unwrap();
        assert_eq!(side.messages[0], parent[0]);
        assert!(matches!(&side.messages.last().unwrap().content[0],
            ContentBlock::Text(s) if s.contains("仅是参考")));
        side.messages[0].content.clear();
        assert!(!parent[0].content.is_empty());
    }

    /// The boundary is appended only after a non-empty safe prefix, and it is
    /// the message the UI relies on to distinguish inherited history.
    #[test]
    fn side_snapshot_appends_hidden_boundary_after_safe_prefix() {
        let parent = vec![Message::user("完成主任务")];
        let side = snapshot_for_side(&Session::from_messages(parent.clone())).unwrap();

        assert_eq!(side.messages.len(), 2);
        assert_eq!(side.messages[0], parent[0]);
        let text = boundary_text(side.messages.last().unwrap());
        assert!(
            text.contains("你正在独立的临时旁支中"),
            "boundary must use the spec wording, got: {text}"
        );
        assert!(text.contains("不使用或控制子 agent"), "got: {text}");
        assert!(text.contains("回到主会话明确提交"), "got: {text}");
    }

    #[test]
    fn side_snapshot_trims_unpaired_tool_use_without_touching_parent() {
        let parent = vec![
            Message::user("检查文件"),
            Message::assistant(vec![tool_use("t1")]),
        ];
        let session = Session::from_messages(parent.clone());
        let before = serde_json::to_vec(&parent).unwrap();

        let side = snapshot_for_side(&session).unwrap();

        // user + boundary; the unpaired tool_use round-trip is dropped.
        assert_eq!(side.messages.len(), 2);
        assert_eq!(side.messages[0], parent[0]);
        boundary_text(side.messages.last().unwrap());
        assert!(
            !side
                .messages
                .iter()
                .flat_map(|m| &m.content)
                .any(|b| matches!(b, ContentBlock::ToolUse { .. })),
            "no tool_use may survive into the snapshot"
        );
        // The parent is untouched: same messages, same serialized bytes.
        assert_eq!(session.messages(), parent.as_slice());
        assert_eq!(serde_json::to_vec(&parent).unwrap(), before);
    }

    #[test]
    fn side_snapshot_keeps_fully_paired_transcript_byte_identical() {
        let parent = vec![
            Message::user("检查图片并读文件"),
            Message::assistant(vec![
                ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "iVBORw0KGgo=".into(),
                    },
                    detail: Default::default(),
                    path: None,
                },
                tool_use("t1"),
            ]),
            Message::tool_results(vec![tool_result("t1")]),
            Message::assistant(vec![ContentBlock::Text("文件内容如下".into())]),
            Message::user("继续看下一处"),
        ];
        let session = Session::from_messages(parent.clone());
        let before = serde_json::to_vec(&parent).unwrap();

        let side = snapshot_for_side(&session).unwrap();

        assert_eq!(side.messages.len(), parent.len() + 1);
        assert_eq!(&side.messages[..parent.len()], parent.as_slice());
        boundary_text(side.messages.last().unwrap());
        assert_eq!(
            serde_json::to_vec(&side.messages[..parent.len()]).unwrap(),
            before
        );
        assert_eq!(serde_json::to_vec(&parent).unwrap(), before);
    }

    /// After compaction the transcript can start with an assistant summary and
    /// contain no user-role message at all; that is still safe history.
    #[test]
    fn side_snapshot_accepts_compaction_summary_only_history() {
        let parent = vec![Message::assistant(vec![ContentBlock::Text(
            "[对话摘要]\n主任务已完成一半".into(),
        )])];

        let side = snapshot_for_side(&Session::from_messages(parent.clone())).unwrap();

        assert_eq!(side.messages.len(), 2);
        assert_eq!(side.messages[0], parent[0]);
        boundary_text(side.messages.last().unwrap());
    }

    /// All three credential shapes must survive verbatim: a rewritten token
    /// breaks provider replay.
    #[test]
    fn side_snapshot_preserves_thinking_credentials_verbatim() {
        let parent = vec![
            Message::assistant(vec![ContentBlock::Thinking {
                text: "weighing options".into(),
                token: ThinkingToken::Signature("sig-blob".into()),
            }]),
            Message::assistant(vec![ContentBlock::Thinking {
                text: String::new(),
                token: ThinkingToken::Redacted("redacted-blob".into()),
            }]),
            Message::assistant(vec![ContentBlock::Thinking {
                text: String::new(),
                token: ThinkingToken::Encrypted {
                    encrypted_content: "enc-blob".into(),
                    summary: vec![json!({ "type": "summary_text", "text": "plan" })],
                },
            }]),
            Message::user("继续"),
        ];

        let side = snapshot_for_side(&Session::from_messages(parent.clone())).unwrap();

        assert_eq!(side.messages.len(), parent.len() + 1);
        for (snapshot_message, parent_message) in side.messages.iter().zip(&parent) {
            assert_eq!(snapshot_message, parent_message);
        }
        boundary_text(side.messages.last().unwrap());
    }

    #[test]
    fn side_snapshot_rejects_empty_and_unpaired_only_history() {
        assert_eq!(
            snapshot_for_side(&Session::new()),
            Err(SideContextError::EmptyHistory)
        );

        // Trimming the only (unpaired) message leaves nothing to inherit.
        let session = Session::from_messages(vec![Message::assistant(vec![tool_use("t1")])]);
        assert_eq!(
            snapshot_for_side(&session),
            Err(SideContextError::EmptyHistory)
        );
    }

    #[test]
    fn side_system_prompt_appends_policy_after_parent_rules() {
        let parent = "项目规则：先读 AGENTS.md。\n\n# Skills\n- brainstorming";

        let prompt = side_system_prompt(Some(parent));

        assert!(
            prompt.starts_with(parent),
            "parent prompt must stay first and unmodified, got: {prompt}"
        );
        assert!(prompt.ends_with(&side_system_prompt(None)));
        assert!(prompt.contains("# Skills"), "skills text must survive");
        assert!(
            prompt.len() > parent.len(),
            "policy must be appended, not replace the parent"
        );
    }

    #[test]
    fn side_system_prompt_uses_policy_alone_without_parent_rules() {
        let policy = side_system_prompt(None);

        assert!(!policy.trim().is_empty());
        for required in [
            "只读",
            "文件",
            "Git",
            "配置",
            "权限",
            "外部状态",
            "子 agent",
            "回到主会话",
        ] {
            assert!(
                policy.contains(required),
                "policy must mention {required}, got: {policy}"
            );
        }
        assert_eq!(side_system_prompt(Some("")), policy);
        assert_eq!(side_system_prompt(Some("  \n ")), policy);
    }
}
