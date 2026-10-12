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
                        let out = if *is_error {
                            format!("error: {text}")
                        } else {
                            text
                        };
                        input.push(serde_json::json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": out
                        }));
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
            let url = match source {
                ImageSource::Base64 { media_type, data } => {
                    format!("data:{media_type};base64,{data}")
                }
                ImageSource::Url(u) => u.clone(),
            };
            parts.push(serde_json::json!({"type": "input_image", "image_url": url}));
        }
    }
    serde_json::json!({"role": "user", "content": parts})
}
