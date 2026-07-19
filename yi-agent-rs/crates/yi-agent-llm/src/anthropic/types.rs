//! Anthropic API request/response types and conversion from core types.

use serde::Serialize;
use serde_json::Value;

use yi_agent_core::{ContentBlock, ImageSource, ProviderRequest, Role, ToolSchema};

/// Anthropic /v1/messages request body.
#[derive(Serialize)]
pub struct AnthropicRequest {
    pub model: String,
    pub messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tools: Vec<AnthropicTool>,
    #[serde(flatten)]
    pub params: AnthropicGenParams,
    /// Always true — we always stream.
    pub stream: bool,
}

#[derive(Serialize)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: Vec<AnthropicContentBlock>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicContentBlock {
    Text { text: String },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<AnthropicContentBlock>,
        is_error: bool,
    },
    Image { source: AnthropicImageSource },
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum AnthropicImageSource {
    Base64 {
        r#type: String, // "base64"
        media_type: String,
        data: String,
    },
    Url {
        r#type: String, // "url"
        url: String,
    },
}

#[derive(Serialize, Default)]
pub struct AnthropicGenParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
}

#[derive(Serialize)]
pub struct AnthropicTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl From<ToolSchema> for AnthropicTool {
    fn from(t: ToolSchema) -> Self {
        Self {
            name: t.name,
            description: t.description,
            input_schema: t.input_schema,
        }
    }
}

impl From<ImageSource> for AnthropicImageSource {
    fn from(s: ImageSource) -> Self {
        match s {
            ImageSource::Base64 { media_type, data } => AnthropicImageSource::Base64 {
                r#type: "base64".to_string(),
                media_type,
                data,
            },
            ImageSource::Url(url) => AnthropicImageSource::Url {
                r#type: "url".to_string(),
                url,
            },
        }
    }
}

impl From<ContentBlock> for AnthropicContentBlock {
    fn from(b: ContentBlock) -> Self {
        match b {
            ContentBlock::Text(text) => AnthropicContentBlock::Text { text },
            ContentBlock::ToolUse { id, name, input } => AnthropicContentBlock::ToolUse { id, name, input },
            ContentBlock::ToolResult { tool_use_id, content, is_error } => AnthropicContentBlock::ToolResult {
                tool_use_id,
                content: content.into_iter().map(Into::into).collect(),
                is_error,
            },
            ContentBlock::Image { source } => AnthropicContentBlock::Image { source: source.into() },
        }
    }
}

impl From<AnthropicGenParams> for yi_agent_core::GenParams {
    fn from(p: AnthropicGenParams) -> Self {
        Self {
            temperature: p.temperature,
            max_tokens: p.max_tokens,
            top_p: p.top_p,
            stop_sequences: p.stop_sequences,
        }
    }
}

impl From<yi_agent_core::GenParams> for AnthropicGenParams {
    fn from(p: yi_agent_core::GenParams) -> Self {
        Self {
            temperature: p.temperature,
            max_tokens: p.max_tokens,
            top_p: p.top_p,
            stop_sequences: p.stop_sequences,
        }
    }
}

/// Role label mapping. `Role::Tool` and `Role::System` are special-cased elsewhere.
fn role_label(role: Role) -> &'static str {
    match role {
        Role::User | Role::Tool => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
    }
}

impl From<ProviderRequest> for AnthropicRequest {
    fn from(req: ProviderRequest) -> Self {
        // System messages are pulled out to the top-level `system` field.
        // All other messages (including Role::Tool) become `role: "user"` entries.
        let mut system_parts: Vec<String> = Vec::new();
        let mut messages: Vec<AnthropicMessage> = Vec::new();

        for m in req.messages {
            match m.role {
                Role::System => {
                    for block in m.content {
                        if let ContentBlock::Text(t) = block {
                            system_parts.push(t);
                        }
                    }
                }
                _ => {
                    let role = role_label(m.role).to_string();
                    let content: Vec<AnthropicContentBlock> =
                        m.content.into_iter().map(Into::into).collect();
                    messages.push(AnthropicMessage { role, content });
                }
            }
        }

        // If ProviderRequest.system was explicitly set, prepend it.
        if let Some(s) = req.system {
            system_parts.insert(0, s);
        }

        let system = if system_parts.is_empty() {
            None
        } else {
            Some(system_parts.join("\n\n"))
        };

        Self {
            model: req.model,
            messages,
            system,
            tools: req.tools.into_iter().map(Into::into).collect(),
            params: req.params.into(),
            stream: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yi_agent_core::{GenParams, Message};

    #[test]
    fn converts_simple_user_text_request() {
        let req = ProviderRequest {
            model: "claude-sonnet-4-5".to_string(),
            system: None,
            messages: vec![Message::user("hello")],
            tools: vec![],
            params: GenParams::default(),
        };
        let a: AnthropicRequest = req.into();
        assert_eq!(a.model, "claude-sonnet-4-5");
        assert_eq!(a.messages.len(), 1);
        assert_eq!(a.messages[0].role, "user");
        assert!(a.system.is_none());
        assert_eq!(a.tools.len(), 0);
        assert!(a.stream);
    }

    #[test]
    fn pulls_system_message_to_top_level() {
        let req = ProviderRequest {
            model: "claude-sonnet-4-5".to_string(),
            system: None,
            messages: vec![
                Message::system("be helpful"),
                Message::user("hi"),
            ],
            tools: vec![],
            params: GenParams::default(),
        };
        let a: AnthropicRequest = req.into();
        assert_eq!(a.system.as_deref(), Some("be helpful"));
        assert_eq!(a.messages.len(), 1);
        assert_eq!(a.messages[0].role, "user");
    }

    #[test]
    fn merges_system_field_and_system_message() {
        let req = ProviderRequest {
            model: "claude-sonnet-4-5".to_string(),
            system: Some("base prompt".to_string()),
            messages: vec![
                Message::system("extra instructions"),
                Message::user("hi"),
            ],
            tools: vec![],
            params: GenParams::default(),
        };
        let a: AnthropicRequest = req.into();
        assert_eq!(a.system.as_deref(), Some("base prompt\n\nextra instructions"));
    }

    #[test]
    fn tool_role_serializes_as_user() {
        let result = ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: vec![ContentBlock::Text("ok".into())],
            is_error: false,
        };
        let req = ProviderRequest {
            model: "claude-sonnet-4-5".to_string(),
            system: None,
            messages: vec![Message::tool_results(vec![result])],
            tools: vec![],
            params: GenParams::default(),
        };
        let a: AnthropicRequest = req.into();
        assert_eq!(a.messages[0].role, "user");
    }

    #[test]
    fn serializes_request_json_correctly() {
        let req = ProviderRequest {
            model: "claude-sonnet-4-5".to_string(),
            system: None,
            messages: vec![Message::user("hi")],
            tools: vec![],
            params: GenParams {
                temperature: Some(0.5),
                max_tokens: Some(1024),
                ..Default::default()
            },
        };
        let a: AnthropicRequest = req.into();
        let json = serde_json::to_value(&a).unwrap();
        assert_eq!(json["model"], "claude-sonnet-4-5");
        assert_eq!(json["stream"], true);
        assert_eq!(json["temperature"], 0.5);
        assert_eq!(json["max_tokens"], 1024);
        // system should be absent (skip_serializing_if)
        assert!(json.get("system").is_none() || json["system"].is_null());
        // tools should be absent
        assert!(json.get("tools").is_none() || json["tools"].is_null());
    }
}
