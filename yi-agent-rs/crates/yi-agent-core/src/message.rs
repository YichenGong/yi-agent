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
    },
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
        let block = ContentBlock::Image { source };
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, back);
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
}
