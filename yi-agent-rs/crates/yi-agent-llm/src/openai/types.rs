//! OpenAI Chat Completions API request types and conversion from core types.

use serde::Serialize;
use serde_json::Value;

use yi_agent_core::{ContentBlock, ProviderRequest, Role, ToolSchema};

/// OpenAI /v1/chat/completions request body.
#[derive(Serialize)]
pub struct OpenaiRequest {
    pub model: String,
    pub messages: Vec<OpenaiMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<OpenaiTool>,
    /// Always true — we always stream.
    pub stream: bool,
    /// Request usage in the final stream chunk.
    pub stream_options: OpenaiStreamOptions,
    #[serde(flatten)]
    pub params: OpenaiGenParams,
}

#[derive(Serialize)]
pub struct OpenaiStreamOptions {
    pub include_usage: bool,
}

#[derive(Serialize)]
pub struct OpenaiMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<OpenaiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OpenaiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Serialize, Debug)]
#[serde(untagged)]
pub enum OpenaiContent {
    Text(String),
    ToolCalls(Vec<OpenaiToolCall>),
}

#[derive(Serialize, Debug)]
pub struct OpenaiToolCall {
    pub id: String,
    pub r#type: String,
    pub function: OpenaiToolCallFunction,
}

#[derive(Serialize, Debug)]
pub struct OpenaiToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Serialize, Default)]
pub struct OpenaiGenParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "stop")]
    pub stop_sequences: Option<Vec<String>>,
}

#[derive(Serialize)]
pub struct OpenaiTool {
    pub r#type: String,
    pub function: OpenaiToolFunction,
}

#[derive(Serialize)]
pub struct OpenaiToolFunction {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl From<ToolSchema> for OpenaiTool {
    fn from(t: ToolSchema) -> Self {
        Self {
            r#type: "function".to_string(),
            function: OpenaiToolFunction {
                name: t.name,
                description: t.description,
                parameters: t.input_schema,
            },
        }
    }
}

fn extract_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

impl From<ProviderRequest> for OpenaiRequest {
    fn from(req: ProviderRequest) -> Self {
        let mut messages: Vec<OpenaiMessage> = Vec::new();

        if let Some(s) = req.system {
            messages.push(OpenaiMessage {
                role: "system".to_string(),
                name: None,
                content: Some(OpenaiContent::Text(s)),
                tool_calls: None,
                tool_call_id: None,
            });
        }

        for m in req.messages {
            match m.role {
                Role::System => {
                    for block in &m.content {
                        if let ContentBlock::Text(t) = block {
                            messages.push(OpenaiMessage {
                                role: "system".to_string(),
                                name: None,
                                content: Some(OpenaiContent::Text(t.clone())),
                                tool_calls: None,
                                tool_call_id: None,
                            });
                        }
                    }
                }
                Role::User => {
                    let text = extract_text(&m.content);
                    messages.push(OpenaiMessage {
                        role: "user".to_string(),
                        name: None,
                        content: Some(OpenaiContent::Text(text)),
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
                Role::Assistant => {
                    let text_parts: Vec<String> = m
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text(t) => Some(t.clone()),
                            _ => None,
                        })
                        .collect();
                    let tool_calls: Vec<OpenaiToolCall> = m
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::ToolUse { id, name, input } => Some(OpenaiToolCall {
                                id: id.clone(),
                                r#type: "function".to_string(),
                                function: OpenaiToolCallFunction {
                                    name: name.clone(),
                                    arguments: input.to_string(),
                                },
                            }),
                            _ => None,
                        })
                        .collect();

                    let content = if !text_parts.is_empty() {
                        Some(OpenaiContent::Text(text_parts.join("")))
                    } else {
                        None
                    };

                    messages.push(OpenaiMessage {
                        role: "assistant".to_string(),
                        name: None,
                        content,
                        tool_calls: if tool_calls.is_empty() {
                            None
                        } else {
                            Some(tool_calls)
                        },
                        tool_call_id: None,
                    });
                }
                Role::Tool => {
                    for block in m.content {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } = block
                        {
                            let text = extract_text(&content);
                            let body = if is_error {
                                format!("error: {}", text)
                            } else {
                                text
                            };
                            messages.push(OpenaiMessage {
                                role: "tool".to_string(),
                                name: None,
                                content: Some(OpenaiContent::Text(body)),
                                tool_calls: None,
                                tool_call_id: Some(tool_use_id),
                            });
                        }
                    }
                }
            }
        }

        Self {
            model: req.model,
            messages,
            tools: req.tools.into_iter().map(Into::into).collect(),
            stream: true,
            stream_options: OpenaiStreamOptions {
                include_usage: true,
            },
            params: OpenaiGenParams {
                temperature: req.params.temperature,
                max_tokens: req.params.max_tokens,
                top_p: req.params.top_p,
                stop_sequences: req.params.stop_sequences,
            },
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
            model: "gpt-4o".to_string(),
            system: None,
            messages: vec![Message::user("hello")],
            tools: vec![],
            params: GenParams::default(),
        };
        let o: OpenaiRequest = req.into();
        assert_eq!(o.model, "gpt-4o");
        assert_eq!(o.messages.len(), 1);
        assert_eq!(o.messages[0].role, "user");
        assert!(o.tools.is_empty());
        assert!(o.stream);
    }

    #[test]
    fn system_message_becomes_role_system() {
        let req = ProviderRequest {
            model: "gpt-4o".to_string(),
            system: None,
            messages: vec![Message::system("be helpful"), Message::user("hi")],
            tools: vec![],
            params: GenParams::default(),
        };
        let o: OpenaiRequest = req.into();
        assert_eq!(o.messages.len(), 2);
        assert_eq!(o.messages[0].role, "system");
        match &o.messages[0].content {
            Some(OpenaiContent::Text(t)) => assert_eq!(t, "be helpful"),
            _ => panic!("expected Text content"),
        }
    }

    #[test]
    fn merges_provider_request_system_field() {
        let req = ProviderRequest {
            model: "gpt-4o".to_string(),
            system: Some("base prompt".to_string()),
            messages: vec![Message::user("hi")],
            tools: vec![],
            params: GenParams::default(),
        };
        let o: OpenaiRequest = req.into();
        assert_eq!(o.messages.len(), 2);
        assert_eq!(o.messages[0].role, "system");
        match &o.messages[0].content {
            Some(OpenaiContent::Text(t)) => assert_eq!(t, "base prompt"),
            _ => panic!("expected Text content"),
        }
    }

    #[test]
    fn tool_use_block_maps_to_tool_call() {
        let req = ProviderRequest {
            model: "gpt-4o".to_string(),
            system: None,
            messages: vec![Message::assistant(vec![ContentBlock::ToolUse {
                id: "call_01".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "/a"}),
            }])],
            tools: vec![],
            params: GenParams::default(),
        };
        let o: OpenaiRequest = req.into();
        assert_eq!(o.messages.len(), 1);
        assert_eq!(o.messages[0].role, "assistant");
        match &o.messages[0].tool_calls {
            Some(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "call_01");
                assert_eq!(calls[0].function.name, "read");
                assert_eq!(calls[0].function.arguments, r#"{"path":"/a"}"#);
            }
            _ => panic!("expected tool_calls"),
        }
    }

    #[test]
    fn tool_result_maps_to_role_tool_message() {
        let result = ContentBlock::ToolResult {
            tool_use_id: "call_01".into(),
            content: vec![ContentBlock::Text("ok".into())],
            is_error: false,
        };
        let req = ProviderRequest {
            model: "gpt-4o".to_string(),
            system: None,
            messages: vec![Message::tool_results(vec![result])],
            tools: vec![],
            params: GenParams::default(),
        };
        let o: OpenaiRequest = req.into();
        assert_eq!(o.messages.len(), 1);
        assert_eq!(o.messages[0].role, "tool");
        // OpenAI requires tool message content to be a string, with
        // tool_call_id as a top-level field (not nested in content).
        match &o.messages[0].content {
            Some(OpenaiContent::Text(t)) => assert_eq!(t, "ok"),
            other => panic!("expected Text content, got {other:?}"),
        }
        assert_eq!(o.messages[0].tool_call_id.as_deref(), Some("call_01"));
    }

    #[test]
    fn tool_result_message_serializes_content_as_string() {
        let result = ContentBlock::ToolResult {
            tool_use_id: "call_01".into(),
            content: vec![ContentBlock::Text("ok".into())],
            is_error: false,
        };
        let req = ProviderRequest {
            model: "gpt-4o".to_string(),
            system: None,
            messages: vec![Message::tool_results(vec![result])],
            tools: vec![],
            params: GenParams::default(),
        };
        let o: OpenaiRequest = req.into();
        let json = serde_json::to_value(&o).unwrap();
        let msg = &json["messages"][0];
        assert_eq!(msg["role"], "tool");
        assert_eq!(msg["content"], "ok", "content must be a string");
        assert_eq!(msg["tool_call_id"], "call_01");
    }

    #[test]
    fn tool_result_error_message_serializes_with_error_prefix() {
        let result = ContentBlock::ToolResult {
            tool_use_id: "call_02".into(),
            content: vec![ContentBlock::Text("boom".into())],
            is_error: true,
        };
        let req = ProviderRequest {
            model: "gpt-4o".to_string(),
            system: None,
            messages: vec![Message::tool_results(vec![result])],
            tools: vec![],
            params: GenParams::default(),
        };
        let o: OpenaiRequest = req.into();
        let json = serde_json::to_value(&o).unwrap();
        let msg = &json["messages"][0];
        assert_eq!(msg["role"], "tool");
        assert_eq!(msg["content"], "error: boom");
        assert_eq!(msg["tool_call_id"], "call_02");
    }

    #[test]
    fn serializes_request_json_correctly() {
        let req = ProviderRequest {
            model: "gpt-4o".to_string(),
            system: None,
            messages: vec![Message::user("hi")],
            tools: vec![ToolSchema {
                name: "read".into(),
                description: "Read a file".into(),
                input_schema: serde_json::json!({"type":"object"}),
            }],
            params: GenParams {
                temperature: Some(0.5),
                max_tokens: Some(1024),
                ..Default::default()
            },
        };
        let o: OpenaiRequest = req.into();
        let json = serde_json::to_value(&o).unwrap();
        assert_eq!(json["model"], "gpt-4o");
        assert_eq!(json["stream"], true);
        assert_eq!(json["temperature"], 0.5);
        assert_eq!(json["max_tokens"], 1024);
        assert!(json.get("stop").is_none() || json["stop"].is_null());
        assert_eq!(json["tools"][0]["type"], "function");
        assert_eq!(json["tools"][0]["function"]["name"], "read");
        assert_eq!(json["stream_options"]["include_usage"], true);
    }

    #[test]
    fn stop_sequences_serialize_as_stop() {
        let req = ProviderRequest {
            model: "gpt-4o".to_string(),
            system: None,
            messages: vec![Message::user("hi")],
            tools: vec![],
            params: GenParams {
                stop_sequences: Some(vec!["END".into()]),
                ..Default::default()
            },
        };
        let o: OpenaiRequest = req.into();
        let json = serde_json::to_value(&o).unwrap();
        assert_eq!(json["stop"], serde_json::json!(["END"]));
    }
}
