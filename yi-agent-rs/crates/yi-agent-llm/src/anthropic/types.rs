//! Anthropic API request/response types and conversion from core types.

use serde::Serialize;
use serde_json::Value;

use yi_agent_core::{ContentBlock, ImageSource, ProviderRequest, Role, ToolSchema};

/// Anthropic Messages API 的 `max_tokens` 是**必填**字段（缺失即 400）。
///
/// 取值需同时容纳 thinking token 与响应文本——thinking 计入 `max_tokens`。
/// 该值对当前新代模型（输出上限 ≥64k）留有充足余量。
pub const DEFAULT_ANTHROPIC_MAX_TOKENS: u32 = 32_000;

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
    /// 无条件发送（spec §5.1）：恒为自适应思考。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    /// 仅当 `reasoning_effort` 设置时存在；否则整个字段缺席（零回归硬要求）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    /// Always true — we always stream.
    pub stream: bool,
}

/// adaptive thinking 配置。新代模型拒绝 `budget_tokens`，故不带该字段。
#[derive(Serialize, Debug)]
pub struct ThinkingConfig {
    pub r#type: String,  // 恒为 "adaptive"
    pub display: String, // 恒为 "summarized"
}

#[derive(Serialize, Debug)]
pub struct OutputConfig {
    pub effort: String,
}

#[derive(Serialize)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: Vec<AnthropicContentBlock>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicContentBlock {
    Text {
        text: String,
    },
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
    Image {
        source: AnthropicImageSource,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
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
            ContentBlock::ToolUse { id, name, input } => {
                AnthropicContentBlock::ToolUse { id, name, input }
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => AnthropicContentBlock::ToolResult {
                tool_use_id,
                content: content.into_iter().map(Into::into).collect(),
                is_error,
            },
            ContentBlock::Image { source, .. } => AnthropicContentBlock::Image {
                source: source.into(),
            },
            ContentBlock::Thinking { text, token } => match token {
                yi_agent_core::ThinkingToken::Signature(signature) => {
                    AnthropicContentBlock::Thinking {
                        thinking: text,
                        signature,
                    }
                }
                yi_agent_core::ThinkingToken::Redacted(data) => {
                    AnthropicContentBlock::RedactedThinking { data }
                }
                // 非 Anthropic 形态**不会**到达这里：`From<ProviderRequest>` 已先过滤。
                // 保守起见用 unreachable 而非编造凭证——静默造出非法 wire 形状更糟。
                yi_agent_core::ThinkingToken::Encrypted(_) | yi_agent_core::ThinkingToken::None => {
                    unreachable!("non-Anthropic thinking tokens are filtered before conversion")
                }
            },
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
            reasoning_effort: None,
        }
    }
}

impl From<yi_agent_core::GenParams> for AnthropicGenParams {
    fn from(p: yi_agent_core::GenParams) -> Self {
        Self {
            temperature: p.temperature,
            // 必填字段：未指定时用默认，绝不序列化缺席。
            max_tokens: Some(p.max_tokens.unwrap_or(DEFAULT_ANTHROPIC_MAX_TOKENS)),
            top_p: p.top_p,
            stop_sequences: p.stop_sequences,
        }
    }
}

/// Role label mapping for the Anthropic `role` field.
///
/// `Role::System` is handled specially in `From<ProviderRequest>` (pulled out
/// to the top-level `system` field) and never reaches this function in
/// practice; the arm exists only for exhaustiveness. `Role::Tool` maps to
/// `"user"` because Anthropic's API requires tool results to be sent in a
/// `user`-role message.
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
                    // The Anthropic API's top-level `system` field accepts only
                    // text, so non-text blocks (e.g. Image) are silently dropped.
                    // `Message::system()` only produces text blocks in practice.
                    for block in m.content {
                        if let ContentBlock::Text(t) = block {
                            system_parts.push(t);
                        }
                    }
                }
                _ => {
                    let role = role_label(m.role).to_string();
                    let content: Vec<AnthropicContentBlock> = m
                        .content
                        .into_iter()
                        // Anthropic 只认自己的两种凭证形态；其他 provider 的
                        // 思考块在转换前丢弃，避免编造出非法 wire 形状。
                        .filter(|b| {
                            !matches!(
                                b,
                                ContentBlock::Thinking {
                                    token: yi_agent_core::ThinkingToken::Encrypted(_)
                                        | yi_agent_core::ThinkingToken::None,
                                    ..
                                }
                            )
                        })
                        .map(Into::into)
                        .collect();
                    // 全空的 assistant 消息（只有被过滤的块）不发送。
                    if !content.is_empty() {
                        messages.push(AnthropicMessage { role, content });
                    }
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

        let thinking = Some(ThinkingConfig {
            r#type: "adaptive".into(),
            display: "summarized".into(),
        });
        // `reasoning_effort` 只被读取（clone），故 `req.params.into()` 可在其后移动。
        let output_config = req
            .params
            .reasoning_effort
            .clone()
            .map(|effort| OutputConfig { effort });

        Self {
            model: req.model,
            messages,
            system,
            tools: req.tools.into_iter().map(Into::into).collect(),
            params: req.params.into(),
            thinking,
            output_config,
            stream: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yi_agent_core::{GenParams, ImageDetail, ImageSource, Message};

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
            messages: vec![Message::system("be helpful"), Message::user("hi")],
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
            messages: vec![Message::system("extra instructions"), Message::user("hi")],
            tools: vec![],
            params: GenParams::default(),
        };
        let a: AnthropicRequest = req.into();
        assert_eq!(
            a.system.as_deref(),
            Some("base prompt\n\nextra instructions")
        );
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
    fn compacted_tool_history_preserves_anthropic_roles_and_pairing() {
        let req = ProviderRequest {
            model: "claude-sonnet-4-5".into(),
            system: None,
            messages: vec![
                Message::user("[用户消息]\nfix the test"),
                Message::assistant(vec![
                    ContentBlock::Text("[对话摘要]\nprevious work".into()),
                    ContentBlock::ToolUse {
                        id: "call_1".into(),
                        name: "read".into(),
                        input: serde_json::json!({"path":"src/lib.rs"}),
                    },
                ]),
                Message::tool_results(vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: vec![ContentBlock::Text("contents".into())],
                    is_error: false,
                }]),
            ],
            tools: vec![],
            params: GenParams::default(),
        };
        let request: AnthropicRequest = req.into();
        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json["messages"][0]["role"], "user");
        assert_eq!(json["messages"][1]["role"], "assistant");
        assert_eq!(json["messages"][2]["role"], "user");
        assert_eq!(json["messages"][1]["content"][1]["id"], "call_1");
        assert_eq!(json["messages"][2]["content"][0]["tool_use_id"], "call_1");
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

    #[test]
    fn tool_result_image_is_preserved() {
        let result = ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: vec![
                ContentBlock::Text("viewed logo.png (8x8, image/png)".into()),
                ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "AAA".into(),
                    },
                    detail: ImageDetail::High,
                    path: None,
                },
            ],
            is_error: false,
        };
        let req = ProviderRequest {
            model: "claude-sonnet-4-5".into(),
            system: None,
            messages: vec![Message::tool_results(vec![result])],
            tools: vec![],
            params: GenParams::default(),
        };
        let a: AnthropicRequest = req.into();
        let json = serde_json::to_value(&a).unwrap();
        // Tool role -> "user" message whose content[0] is the tool_result.
        let content = json["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "tool_result");
        let inner = content[0]["content"].as_array().unwrap();
        assert_eq!(inner[0]["type"], "text");
        assert_eq!(inner[1]["type"], "image");
        assert_eq!(inner[1]["source"]["type"], "base64");
        assert_eq!(inner[1]["source"]["media_type"], "image/png");
        assert_eq!(inner[1]["source"]["data"], "AAA");
    }

    #[test]
    fn thinking_block_serializes_before_tool_use_with_verbatim_signature() {
        use yi_agent_core::{ContentBlock, ThinkingToken};
        let req = ProviderRequest {
            model: "claude-opus-5-5".into(),
            system: None,
            messages: vec![Message::assistant(vec![
                ContentBlock::Thinking {
                    text: "".into(), // display:"omitted"
                    token: ThinkingToken::Signature("sig-part-1.sig-part-2".into()),
                },
                ContentBlock::ToolUse {
                    id: "toolu_1".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path": "a.rs"}),
                },
            ])],
            tools: vec![],
            params: GenParams::default(),
        };
        let json = serde_json::to_value(AnthropicRequest::from(req)).unwrap();
        let blocks = json["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "thinking", "thinking must be first");
        assert_eq!(blocks[0]["signature"], "sig-part-1.sig-part-2", "verbatim");
        assert_eq!(blocks[0]["thinking"], "");
        assert_eq!(blocks[1]["type"], "tool_use", "tool_use must follow");
    }

    #[test]
    fn redacted_thinking_serializes_as_its_own_type() {
        use yi_agent_core::{ContentBlock, ThinkingToken};
        let req = ProviderRequest {
            model: "claude-opus-5-5".into(),
            system: None,
            messages: vec![Message::assistant(vec![ContentBlock::Thinking {
                text: "".into(),
                token: ThinkingToken::Redacted("enc-blob".into()),
            }])],
            tools: vec![],
            params: GenParams::default(),
        };
        let json = serde_json::to_value(AnthropicRequest::from(req)).unwrap();
        let block = &json["messages"][0]["content"][0];
        assert_eq!(block["type"], "redacted_thinking");
        assert_eq!(block["data"], "enc-blob");
        assert!(block.get("signature").is_none());
    }

    #[test]
    fn deepseek_and_responses_tokens_are_not_sent_to_anthropic() {
        use yi_agent_core::{ContentBlock, ThinkingToken};
        for token in [ThinkingToken::None, ThinkingToken::Encrypted("x".into())] {
            let req = ProviderRequest {
                model: "claude-opus-5-5".into(),
                system: None,
                messages: vec![Message::assistant(vec![
                    ContentBlock::Thinking {
                        text: "t".into(),
                        token,
                    },
                    ContentBlock::Text("visible".into()),
                ])],
                tools: vec![],
                params: GenParams::default(),
            };
            let json = serde_json::to_value(AnthropicRequest::from(req)).unwrap();
            let blocks = json["messages"][0]["content"].as_array().unwrap();
            assert_eq!(blocks.len(), 1, "non-Anthropic token must be dropped");
            assert_eq!(blocks[0]["type"], "text");
        }
    }

    #[test]
    fn thinking_and_output_config_are_emitted_when_effort_set() {
        use yi_agent_core::GenParams;
        let req = ProviderRequest {
            model: "claude-opus-5-5".into(),
            system: None,
            messages: vec![Message::user("hi")],
            tools: vec![],
            params: GenParams {
                reasoning_effort: Some("high".into()),
                ..Default::default()
            },
        };
        let json = serde_json::to_value(AnthropicRequest::from(req)).unwrap();
        assert_eq!(json["thinking"]["type"], "adaptive");
        assert_eq!(json["thinking"]["display"], "summarized");
        assert!(
            json["thinking"].get("budget_tokens").is_none(),
            "no manual budget"
        );
        assert_eq!(json["output_config"]["effort"], "high");
    }

    #[test]
    fn max_tokens_always_present_even_when_unset() {
        let req = ProviderRequest {
            model: "claude-opus-5-5".into(),
            system: None,
            messages: vec![Message::user("hi")],
            tools: vec![],
            params: GenParams::default(),
        };
        let json = serde_json::to_value(AnthropicRequest::from(req)).unwrap();
        assert!(
            json["max_tokens"].as_u64().is_some(),
            "max_tokens is required by the Anthropic API; must never be absent"
        );
    }
}
