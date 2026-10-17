//! Message model for agent communication.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    System,
    User,
    Assistant,
    /// Tool result message (serialized as "user" by provider impls).
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ContentBlock {
    Text(String),

    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },

    ToolResult {
        tool_use_id: String,
        content: Vec<ContentBlock>,
        is_error: bool,
    },

    Image {
        source: ImageSource,
        #[serde(default)]
        detail: ImageDetail,
        /// 图片的来源路径（相对 workspace 根），供前端显示与 `image/read` 定位。
        /// 旧序列化数据没有这个字段，缺省 `None`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },

    Thinking {
        /// 可读摘要。`display:"summarized"` 时有值；`omitted` 或 redacted 时为空串。
        text: String,
        /// 不透明推理状态凭证。回传时必须逐字不变。
        token: ThinkingToken,
    },
}

/// 推理状态凭证的形态。
///
/// 注意：`Signature` 与 `Redacted` 都是 Anthropic 的凭证载荷，但序列化成**不同**的
/// `type` 值（`thinking` vs `redacted_thinking`），故必须区分为两个变体——单靠
/// 「有没有凭证」无法决定回传时的 `type`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThinkingToken {
    /// Anthropic 明文 thinking 块（text 可有内容，display 决定）。
    Signature(String),
    /// Anthropic redacted_thinking 块（text 恒为空，只有 data）。
    Redacted(String),
    /// OpenAI Responses 的 encrypted_content。
    Encrypted(String),
    /// 无凭证。DeepSeek 的 reasoning_content 按规范不需回传，明文仅供展示。
    None,
}

/// Requested fidelity for an image block. Only affects how the image is
/// resized before being sent; OpenAI maps it to `image_url.detail`,
/// Anthropic has no equivalent field and ignores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ImageDetail {
    #[default]
    High,
    Original,
}

impl ImageDetail {
    /// Lowercase name used on the wire (OpenAI `image_url.detail`).
    pub fn as_wire_str(&self) -> &'static str {
        match self {
            ImageDetail::High => "high",
            ImageDetail::Original => "original",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
    Url(String),
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text(text.into())],
        }
    }

    pub fn assistant(blocks: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::Assistant,
            content: blocks,
        }
    }

    pub fn tool_results(results: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::Tool,
            content: results,
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: vec![ContentBlock::Text(text.into())],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_constructor() {
        let m = Message::user("hello");
        assert_eq!(m.role, Role::User);
        assert_eq!(m.content, vec![ContentBlock::Text("hello".to_string())]);
    }

    #[test]
    fn assistant_message_constructor() {
        let m = Message::assistant(vec![ContentBlock::Text("hi".into())]);
        assert_eq!(m.role, Role::Assistant);
        assert_eq!(m.content.len(), 1);
    }

    #[test]
    fn tool_results_message_has_tool_role() {
        let result = ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: vec![ContentBlock::Text("ok".into())],
            is_error: false,
        };
        let m = Message::tool_results(vec![result]);
        assert_eq!(m.role, Role::Tool);
        assert_eq!(m.content.len(), 1);
    }

    #[test]
    fn system_message_constructor() {
        let m = Message::system("be helpful");
        assert_eq!(m.role, Role::System);
    }

    #[test]
    fn content_block_serde_roundtrip() {
        let block = ContentBlock::ToolUse {
            id: "t1".into(),
            name: "read".into(),
            input: serde_json::json!({"path": "/tmp"}),
        };
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, back);
    }

    #[test]
    fn nested_tool_result_content() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: vec![
                ContentBlock::Text("summary".into()),
                ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "base64data".into(),
                    },
                    detail: ImageDetail::High,
                    path: None,
                },
            ],
            is_error: false,
        };
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, back);
    }

    #[test]
    fn image_source_url_serde_roundtrip() {
        let source = ImageSource::Url("https://example.com/img.png".into());
        let block = ContentBlock::Image {
            source,
            detail: ImageDetail::High,
            path: None,
        };
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, back);
    }

    #[test]
    fn image_detail_defaults_to_high_when_absent() {
        // A block deserialized without `detail` must default to High so old
        // serialized sessions keep loading.
        let json = r#"{"Image":{"source":{"Base64":{"media_type":"image/png","data":"AAA"}}}}"#;
        let block: ContentBlock = serde_json::from_str(json).unwrap();
        match block {
            ContentBlock::Image { detail, .. } => assert_eq!(detail, ImageDetail::High),
            _ => panic!("expected Image"),
        }
    }

    #[test]
    fn image_detail_roundtrips_original() {
        let block = ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "AAA".into(),
            },
            detail: ImageDetail::Original,
            path: None,
        };
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, back);
    }

    #[test]
    fn image_detail_wire_str() {
        assert_eq!(ImageDetail::High.as_wire_str(), "high");
        assert_eq!(ImageDetail::Original.as_wire_str(), "original");
    }

    #[test]
    fn role_serde_roundtrip_all_variants() {
        for role in [Role::System, Role::User, Role::Assistant, Role::Tool] {
            let json = serde_json::to_string(&role).unwrap();
            let back: Role = serde_json::from_str(&json).unwrap();
            assert_eq!(role, back);
        }
    }

    #[test]
    fn message_with_multiple_mixed_content_blocks() {
        let msg = Message::assistant(vec![
            ContentBlock::Text("thinking...".into()),
            ContentBlock::ToolUse {
                id: "t1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "/a"}),
            },
            ContentBlock::Text("more text".into()),
        ]);
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.content.len(), 3);
        // Verify order preserved
        assert!(matches!(msg.content[0], ContentBlock::Text(_)));
        assert!(matches!(msg.content[1], ContentBlock::ToolUse { .. }));
        assert!(matches!(msg.content[2], ContentBlock::Text(_)));
    }

    #[test]
    fn tool_result_block_is_error_flag_serde() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: vec![ContentBlock::Text("failed".into())],
            is_error: true,
        };
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, back);
        match back {
            ContentBlock::ToolResult { is_error, .. } => assert!(is_error),
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn image_block_carries_an_optional_source_path() {
        let block = ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "AAA".into(),
            },
            detail: ImageDetail::High,
            path: Some(".yi-agent/attachments/t1/deadbeef-x.png".into()),
        };
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, back);
    }

    #[test]
    fn image_block_without_path_defaults_to_none() {
        // 旧会话里没有 `path` 字段，必须仍能反序列化。
        let json = r#"{"Image":{"source":{"Base64":{"media_type":"image/png","data":"AAA"}},"detail":"High"}}"#;
        let block: ContentBlock = serde_json::from_str(json).unwrap();
        match block {
            ContentBlock::Image { path, .. } => assert!(path.is_none()),
            _ => panic!("expected Image"),
        }
    }

    #[test]
    fn thinking_block_roundtrips_all_token_kinds() {
        let cases = vec![
            ThinkingToken::Signature("sig-abc".into()),
            ThinkingToken::Redacted("red-xyz".into()),
            ThinkingToken::Encrypted("enc-123".into()),
            ThinkingToken::None,
        ];
        for token in cases {
            let block = ContentBlock::Thinking {
                text: "summary".into(),
                token: token.clone(),
            };
            let json = serde_json::to_string(&block).unwrap();
            let back: ContentBlock = serde_json::from_str(&json).unwrap();
            assert_eq!(block, back, "thinking block must round-trip losslessly");
        }
    }

    #[test]
    fn thinking_block_preserves_empty_text_with_token() {
        // display:"omitted" 场景：明文为空串，凭证必须仍在。
        let block = ContentBlock::Thinking {
            text: String::new(),
            token: ThinkingToken::Signature("sig-full".into()),
        };
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        match back {
            ContentBlock::Thinking { text, token } => {
                assert_eq!(text, "");
                assert_eq!(token, ThinkingToken::Signature("sig-full".into()));
            }
            other => panic!("expected Thinking, got {other:?}"),
        }
    }
}
