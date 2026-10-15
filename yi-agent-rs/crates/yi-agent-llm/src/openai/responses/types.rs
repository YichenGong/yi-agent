//! `/v1/responses` 请求体（无状态重放；实测免 previous_response_id 可做工具往返）。

use serde::Serialize;
use serde_json::Value;
use yi_agent_core::{ContentBlock, ImageSource, ProviderRequest, Role};

#[derive(Serialize)]
pub struct ResponsesRequest {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub input: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    pub stream: bool,
}

#[derive(Serialize)]
pub struct Reasoning {
    pub effort: String,
    /// 固定 auto —— 实测只有它能让 GPT-6 返回明文摘要。
    pub summary: String,
}

pub fn responses_request_from(req: ProviderRequest) -> ResponsesRequest {
    let mut input: Vec<Value> = Vec::new();
    for m in &req.messages {
        match m.role {
            Role::User => input.push(user_item(&m.content)),
            Role::Assistant => {
                // reasoning item 必须在 function_call 之前——与服务端返回顺序一致。
                for b in &m.content {
                    if let ContentBlock::Thinking {
                        token: yi_agent_core::ThinkingToken::Encrypted(enc),
                        ..
                    } = b
                    {
                        input.push(serde_json::json!({
                            "type": "reasoning",
                            "encrypted_content": enc
                        }));
                    }
                }
                let text: String = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect();
                if !text.is_empty() {
                    input.push(serde_json::json!({
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text}]
                    }));
                }
                for b in &m.content {
                    if let ContentBlock::ToolUse {
                        id,
                        name,
                        input: args,
                    } = b
                    {
                        input.push(serde_json::json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": args.to_string()
                        }));
                    }
                }
            }
            Role::Tool => {
                for b in &m.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } = b
                    {
                        let text = content
                            .iter()
                            .filter_map(|c| match c {
                                ContentBlock::Text(t) => Some(t.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("");
                        let has_image = content
                            .iter()
                            .any(|c| matches!(c, ContentBlock::Image { .. }));
                        let is_error = *is_error;

                        // The responses `function_call_output` item carries text
                        // only, so images travel in a following user item — the
                        // same shape the chat channel uses (marker + user turn).
                        let mut out = if is_error {
                            format!("error: {text}")
                        } else {
                            text
                        };
                        if has_image {
                            out.push_str("\n[image attached in the following message]");
                        }
                        input.push(serde_json::json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": out
                        }));
                        if has_image {
                            input.push(serde_json::json!({
                                "role": "user",
                                "content": image_parts(content)
                            }));
                        }
                    }
                }
            }
            Role::System => { /* system 走 instructions */ }
        }
    }
    let tools = req
        .tools
        .into_iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.input_schema
            })
        })
        .collect();
    let reasoning = req.params.reasoning_effort.map(|effort| Reasoning {
        effort,
        summary: "auto".into(),
    });
    ResponsesRequest {
        model: req.model,
        instructions: req.system,
        input,
        tools,
        reasoning,
        temperature: req.params.temperature,
        top_p: req.params.top_p,
        max_output_tokens: req.params.max_tokens,
        stream: true,
    }
}

fn user_item(blocks: &[ContentBlock]) -> Value {
    let has_image = blocks
        .iter()
        .any(|b| matches!(b, ContentBlock::Image { .. }));
    if !has_image {
        let text: String = blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        return serde_json::json!({"role": "user", "content": [{"type": "input_text", "text": text}]});
    }
    let mut parts: Vec<Value> = Vec::new();
    let text: String = blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    if !text.is_empty() {
        parts.push(serde_json::json!({"type": "input_text", "text": text}));
    }
    for b in blocks {
        if let ContentBlock::Image { source, .. } = b {
            parts.push(image_url_part(source));
        }
    }
    serde_json::json!({"role": "user", "content": parts})
}

/// Build the `input_image` parts for every image block in `blocks`.
fn image_parts(blocks: &[ContentBlock]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Image { source, .. } => Some(image_url_part(source)),
            _ => None,
        })
        .collect()
}

/// Map an image source to the responses `input_image` wire part.
fn image_url_part(source: &ImageSource) -> Value {
    let url = match source {
        ImageSource::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
        ImageSource::Url(u) => u.clone(),
    };
    serde_json::json!({"type": "input_image", "image_url": url})
}

#[cfg(test)]
mod tests {
    use super::*;
    use yi_agent_core::{GenParams, ImageDetail, Message, ThinkingToken, ToolSchema};

    const MARKER: &str = "\n[image attached in the following message]";

    fn base_request(messages: Vec<Message>) -> ProviderRequest {
        ProviderRequest {
            model: "m".into(),
            system: None,
            messages,
            tools: Vec::<ToolSchema>::new(),
            params: GenParams::default(),
        }
    }

    fn base64_image() -> ContentBlock {
        ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "aGk=".into(),
            },
            detail: ImageDetail::High,
            path: None,
        }
    }

    #[test]
    fn tool_result_with_image_marks_output_and_appends_user_image_item() {
        let req = base_request(vec![
            Message::assistant(vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "view_image".into(),
                input: serde_json::json!({}),
            }]),
            Message::tool_results(vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: vec![ContentBlock::Text("ok".into()), base64_image()],
                is_error: false,
            }]),
        ]);
        let body = serde_json::to_value(responses_request_from(req)).unwrap();
        let input = body["input"].as_array().unwrap();

        // function_call_output keeps text and gains the chat marker.
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[1]["output"], format!("ok{MARKER}"));

        // Image travels in a following user item with no text part.
        assert_eq!(input[2]["role"], "user");
        assert_eq!(input[2]["content"][0]["type"], "input_image");
        assert_eq!(
            input[2]["content"][0]["image_url"],
            "data:image/png;base64,aGk="
        );
        assert_eq!(input[2]["content"].as_array().unwrap().len(), 1);
        assert_eq!(input.len(), 3, "no extra items emitted");
    }

    #[test]
    fn tool_result_with_url_image_appends_user_image_item() {
        let req = base_request(vec![Message::tool_results(vec![
            ContentBlock::ToolResult {
                tool_use_id: "call_9".into(),
                content: vec![
                    ContentBlock::Text("see".into()),
                    ContentBlock::Image {
                        source: ImageSource::Url("https://example.com/a.png".into()),
                        detail: ImageDetail::High,
                        path: None,
                    },
                ],
                is_error: false,
            },
        ])]);
        let body = serde_json::to_value(responses_request_from(req)).unwrap();
        let input = body["input"].as_array().unwrap();

        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(input[0]["output"], format!("see{MARKER}"));
        assert_eq!(input[1]["role"], "user");
        assert_eq!(
            input[1]["content"][0]["image_url"],
            "https://example.com/a.png"
        );
    }

    #[test]
    fn tool_result_without_image_is_unchanged() {
        let req = base_request(vec![Message::tool_results(vec![
            ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: vec![ContentBlock::Text("plain".into())],
                is_error: false,
            },
        ])]);
        let body = serde_json::to_value(responses_request_from(req)).unwrap();
        let input = body["input"].as_array().unwrap();

        assert_eq!(input.len(), 1, "no image item");
        assert_eq!(input[0]["output"], "plain");
        assert!(
            !input[0]["output"]
                .as_str()
                .unwrap()
                .contains("[image attached"),
            "no marker for text-only tool result"
        );
    }

    #[test]
    fn error_tool_result_with_image_keeps_error_prefix_and_marker() {
        let req = base_request(vec![Message::tool_results(vec![
            ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: vec![ContentBlock::Text("boom".into()), base64_image()],
                is_error: true,
            },
        ])]);
        let body = serde_json::to_value(responses_request_from(req)).unwrap();
        let input = body["input"].as_array().unwrap();

        assert_eq!(input[0]["output"], format!("error: boom{MARKER}"));
        assert_eq!(input[1]["role"], "user");
        assert_eq!(input[1]["content"][0]["type"], "input_image");
    }

    #[test]
    fn reasoning_item_is_replayed_before_function_call() {
        use yi_agent_core::{ContentBlock, ThinkingToken};
        let req = ProviderRequest {
            model: "gpt-6-sol".into(),
            system: None,
            messages: vec![Message::assistant(vec![
                ContentBlock::Thinking {
                    text: "".into(),
                    token: ThinkingToken::Encrypted("enc-blob".into()),
                },
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "read".into(),
                    input: serde_json::json!({}),
                },
            ])],
            tools: vec![],
            params: GenParams::default(),
        };
        let body = serde_json::to_value(responses_request_from(req)).unwrap();
        let input = body["input"].as_array().unwrap();
        let reasoning_at = input
            .iter()
            .position(|i| i["type"] == "reasoning")
            .expect("reasoning item must be present");
        let call_at = input
            .iter()
            .position(|i| i["type"] == "function_call")
            .expect("function_call must be present");
        assert!(
            reasoning_at < call_at,
            "reasoning must precede function_call"
        );
        assert_eq!(input[reasoning_at]["encrypted_content"], "enc-blob");
    }

    // ---- 以下为超出 brief 的防御性用例 ----

    #[test]
    fn non_encrypted_thinking_tokens_yield_no_reasoning_item() {
        // Signature/Redacted/None 都是非本通道凭证，不得伪装成 reasoning item。
        let tokens = vec![
            ThinkingToken::Signature("sig".into()),
            ThinkingToken::Redacted("red".into()),
            ThinkingToken::None,
        ];
        for token in tokens {
            let req = base_request(vec![Message::assistant(vec![ContentBlock::Thinking {
                text: "summary".into(),
                token: token.clone(),
            }])]);
            let body = serde_json::to_value(responses_request_from(req)).unwrap();
            let input = body["input"].as_array().unwrap();
            assert!(
                !input.iter().any(|i| i["type"] == "reasoning"),
                "token {token:?} must not produce a reasoning item: {input:?}"
            );
        }
    }
}
