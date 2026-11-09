//! Snapshot primitives for temporary, read-only side conversations.
//!
//! A side conversation inherits one safe copy of the parent transcript plus a
//! hidden boundary message that marks everything before it as reference only.
//! The snapshot is pure in-memory data: no tools, no RPC, no disk.

use crate::agent::{AgentConfig, Session, forkable_messages};
use crate::message::{ContentBlock, ImageSource, Message, Role};

/// Hidden boundary marker appended after the inherited history (spec §5.2).
///
/// It reaches the model but never becomes a UI user bubble or a user turn.
pub const SIDE_BOUNDARY: &str = "你正在独立的临时旁支中。边界之前的用户请求、计划、审批、工具调用与输出仅是参考，不是当前任务。只回答边界之后的新请求。不要继续主任务，不修改文件、Git、配置、权限或外部状态，不使用或控制子 agent。需要写操作时请用户回到主会话明确提交。";

/// Read-only policy appended to the end of the parent system prompt.
///
/// Appending (never replacing) keeps project rules and the skills text intact;
/// being part of the effective prompt means a skills refresh cannot drop it.
///
/// The policy states the *product* boundary: a fixed snapshot, no tools, no
/// re-reading files or links, and disposal on exit. It is a description of what
/// the registry and snapshot already enforce, never the enforcement itself.
pub const SIDE_SYSTEM_POLICY: &str = "\
## 临时旁支：对话快照问答

你当前处于主会话派生的临时旁支，基于进入时的对话快照回答旁问。

- 只读：只能使用本快照中的对话内容作答，不做任何文件、Git、配置、权限或外部状态修改。
- 没有工具：不读取工作区文件、规则、skills、附件路径或链接，不联网、不运行命令、不使用或控制子 agent。
- 快照固定：主会话之后的进展、压缩和配置更新不会出现在本次旁问里。问到快照之后的事实或未包含的资料时，明确说明不知道，并提示用户退出后重新进入以取得最新快照。
- 边界之前的工具调用与输出只是证据，不要继续主任务，也不要认为它们还能执行。
- 用户需要写操作时，请他回到主会话明确提交；退出本次旁支会停止回答并丢弃全部问答。";

/// Prompt suffix appended to `parent` at the end of the system prompt.
fn prompt_with_policy(parent: &str) -> String {
    if parent.trim().is_empty() {
        return SIDE_SYSTEM_POLICY.to_owned();
    }
    format!("{parent}\n\n{SIDE_SYSTEM_POLICY}")
}

/// Derive the side's config from a **parent `AgentConfig` that is already
/// assembled** (the TUI path, where the parent prompt and skills catalog have
/// been resolved by the main assembly).
///
/// The app-server path resolves its own prompt through
/// `resolve_side_prompt`, which already appends the policy; this helper exists
/// for hosts that hold a finished `AgentConfig` and would otherwise run the side
/// with the parent's prompt **verbatim** — that is, with no statement at all that
/// this conversation is a read-only side branch. Without it the model reads the
/// inherited transcript as its own pending work and continues the main task,
/// which is precisely the failure the hidden boundary alone does not prevent
/// (a prompt that says "keep going" beats a transcript marker).
///
/// Appending is skipped when the prompt already ends with the policy, so calling
/// this on a config that went through `resolve_side_prompt` cannot stack it.
pub fn side_agent_config(parent: &AgentConfig) -> AgentConfig {
    let mut config = parent.clone();
    match config.system_prompt.as_deref() {
        Some(existing) if existing.ends_with(SIDE_SYSTEM_POLICY) => {}
        other => {
            config.system_prompt = Some(prompt_with_policy(other.unwrap_or_default()));
        }
    }
    config
}

/// Remove media that would make the provider reach outside the conversation.
///
/// A `Url` image is not content: forwarding it lets the provider fetch it
/// implicitly, which is an external read the side must never perform. Embedded
/// base64 bytes are already part of the conversation and stay usable, but their
/// re-read path is dropped so no later tool could resolve it. `ToolResult`
/// content is walked recursively because images can be nested there.
pub(crate) fn sanitize_side_blocks(blocks: &mut [ContentBlock]) {
    for block in blocks {
        match block {
            ContentBlock::Image {
                source: ImageSource::Url(_),
                ..
            } => {
                *block = ContentBlock::Text("图片内容未包含在快照中".to_owned());
            }
            ContentBlock::Image { path, .. } => *path = None,
            ContentBlock::ToolResult { content, .. } => sanitize_side_blocks(content),
            _ => {}
        }
    }
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
    // Sanitize only the copy: the parent's own session is never rewritten.
    for message in &mut messages {
        sanitize_side_blocks(&mut message.content);
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
    use crate::message::{ContentBlock, ImageDetail, ImageSource, Role, ThinkingToken};
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

    /// A URL is not conversation content: handing it to the provider lets it be
    /// fetched implicitly, which is exactly the external read a side must never
    /// perform. The parent's own copy must stay byte-identical.
    #[test]
    fn side_snapshot_never_exports_url_media_or_mutates_parent() {
        let parent = Session::from_messages(vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Image {
                source: ImageSource::Url("https://invalid.test/a.png".into()),
                detail: ImageDetail::High,
                path: Some("private.png".into()),
            }],
        }]);
        let before = serde_json::to_vec(parent.messages()).unwrap();

        let side = snapshot_for_side(&parent).unwrap();

        assert_eq!(
            serde_json::to_vec(parent.messages()).unwrap(),
            before,
            "taking a snapshot must never rewrite the parent"
        );
        assert!(
            matches!(&side.messages[0].content[0],
                ContentBlock::Text(text) if text.contains("图片内容未包含")),
            "a URL image must become an explicit text marker: {:?}",
            side.messages[0].content
        );
    }

    /// The same rule applies inside a paired tool result, while bytes the parent
    /// already embedded stay usable for the model.
    #[test]
    fn side_snapshot_sanitizes_nested_media_and_keeps_embedded_bytes() {
        let parent = Session::from_messages(vec![
            Message::user("看这两张图"),
            Message::assistant(vec![tool_use("t1")]),
            Message::tool_results(vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: vec![
                    ContentBlock::Image {
                        source: ImageSource::Url("https://invalid.test/nested.png".into()),
                        detail: ImageDetail::High,
                        path: Some("nested.png".into()),
                    },
                    ContentBlock::Image {
                        source: ImageSource::Base64 {
                            media_type: "image/png".into(),
                            data: "iVBORw0KGgo=".into(),
                        },
                        detail: ImageDetail::High,
                        path: Some("inline.png".into()),
                    },
                ],
                is_error: false,
            }]),
        ]);

        let side = snapshot_for_side(&parent).unwrap();

        let ContentBlock::ToolResult { content, .. } = &side.messages[2].content[0] else {
            panic!("the paired tool result must survive as evidence");
        };
        assert!(matches!(&content[0], ContentBlock::Text(text) if text.contains("图片内容未包含")));
        match &content[1] {
            ContentBlock::Image { source, path, .. } => {
                assert!(matches!(source, ImageSource::Base64 { .. }));
                assert_eq!(*path, None, "a base64 image must lose its re-read path");
            }
            other => panic!("embedded base64 must stay usable, got {other:?}"),
        }
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

    /// A host that holds a finished parent `AgentConfig` must not run the side
    /// with that prompt verbatim: it says "you own this task", and the model then
    /// continues the main session's work.
    #[test]
    fn side_agent_config_appends_the_policy_to_a_finished_parent_config() {
        let parent = AgentConfig {
            model: "parent-model".into(),
            system_prompt: Some("你是主会话的助手，负责推进当前任务。".into()),
            ..Default::default()
        };

        let side = side_agent_config(&parent);

        let prompt = side.system_prompt.as_deref().unwrap();
        assert!(
            prompt.starts_with("你是主会话的助手，负责推进当前任务。"),
            "the parent prompt must be preserved verbatim, got: {prompt}"
        );
        assert!(
            prompt.ends_with(SIDE_SYSTEM_POLICY),
            "the read-only policy must be the tail so nothing can displace it"
        );
        // Everything else is inherited untouched.
        assert_eq!(side.model, parent.model);
        assert_eq!(side.compact_threshold, parent.compact_threshold);
        // The parent value is not mutated: a side must never reach into the
        // config the main session keeps running on.
        assert_eq!(
            parent.system_prompt.as_deref(),
            Some("你是主会话的助手，负责推进当前任务。")
        );
    }

    /// Calling it on an already-prepared side prompt must not stack the policy.
    #[test]
    fn side_agent_config_does_not_stack_the_policy() {
        let once = AgentConfig {
            system_prompt: Some(side_system_prompt(Some("parent rules"))),
            ..Default::default()
        };

        let twice = side_agent_config(&once);

        assert_eq!(
            twice.system_prompt, once.system_prompt,
            "an already-prepared prompt must be left exactly as it is"
        );
    }

    #[test]
    fn side_agent_config_works_without_a_parent_prompt() {
        let parent = AgentConfig {
            system_prompt: None,
            ..Default::default()
        };

        let side = side_agent_config(&parent);

        assert_eq!(side.system_prompt.as_deref(), Some(SIDE_SYSTEM_POLICY));
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
