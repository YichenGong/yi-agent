//! Provider trait for LLM backends.

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use crate::message::ContentBlock;
use crate::message::Message;
use crate::tool::ToolSchema;

/// Request to a provider.
#[derive(Debug, Clone)]
pub struct ProviderRequest {
    /// Model identifier (e.g. "claude-sonnet-4-5"). Interpreted by the provider.
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub params: GenParams,
}

/// Generation parameters (all optional, provider uses its defaults if None).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GenParams {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub top_p: Option<f32>,
    pub stop_sequences: Option<Vec<String>>,
}

/// Token usage from a provider response.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_creation_input_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
}

/// Streaming event from a provider.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    TextDelta(String),
    ToolUseStart { id: String, name: String },
    ToolUseDelta { id: String, partial_json: String },
    ToolUseEnd { id: String },
    Stop { reason: StopReason },
    Usage(TokenUsage),
}

/// Why generation stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    Other(String),
}

/// Accumulated provider response.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
}

/// Errors from a provider.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ProviderError {
    #[error("network error: {0}")]
    Network(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("rate limited")]
    RateLimited,
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("server error: {0}")]
    Server(String),
    #[error("stream error: {0}")]
    Stream(String),
}

/// Accumulate a provider stream into content blocks + stop reason.
/// `on_text` is called for each text delta (used by agent to emit AgentEvent::AssistantText).
pub async fn accumulate_stream<F>(
    mut stream: BoxStream<'static, ProviderEvent>,
    mut on_text: F,
) -> Result<(Vec<ContentBlock>, StopReason), ProviderError>
where
    F: FnMut(String),
{
    let mut content = Vec::new();
    let mut current_text = String::new();
    let mut tool_uses: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    let mut stop_reason = StopReason::EndTurn;

    while let Some(event) = stream.next().await {
        match event {
            ProviderEvent::TextDelta(s) => {
                current_text.push_str(&s);
                on_text(s);
            }
            ProviderEvent::ToolUseStart { id, name } => {
                if !current_text.is_empty() {
                    content.push(ContentBlock::Text(std::mem::take(&mut current_text)));
                }
                tool_uses.insert(id, (name, String::new()));
            }
            ProviderEvent::ToolUseDelta { id, partial_json } => {
                if let Some((_, json)) = tool_uses.get_mut(&id) {
                    json.push_str(&partial_json);
                }
            }
            ProviderEvent::ToolUseEnd { id } => {
                if let Some((name, json)) = tool_uses.remove(&id) {
                    let input: serde_json::Value = serde_json::from_str(&json).map_err(|e| {
                        ProviderError::Stream(format!("malformed tool use JSON for id {id}: {e}"))
                    })?;
                    content.push(ContentBlock::ToolUse { id, name, input });
                }
            }
            ProviderEvent::Stop { reason } => {
                stop_reason = reason;
            }
            ProviderEvent::Usage(_) => {
                // Usage events are ignored by accumulate_stream.
            }
        }
    }
    if !current_text.is_empty() {
        content.push(ContentBlock::Text(current_text));
    }
    Ok((content, stop_reason))
}

/// LLM provider trait.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Main method: stream events.
    async fn call_stream(
        &self,
        req: ProviderRequest,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError>;

    /// Convenience: accumulate stream into full response.
    async fn call(&self, req: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let stream = self.call_stream(req).await?;
        let (content, stop_reason) = accumulate_stream(stream, |_| {}).await?;
        Ok(ProviderResponse {
            content,
            stop_reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::StreamExt;

    /// Mock provider that emits a predefined sequence of events.
    struct MockProvider {
        events: Vec<ProviderEvent>,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            let events = self.events.clone();
            Ok(futures::stream::iter(events).boxed())
        }
    }

    fn text_event(s: &str) -> ProviderEvent {
        ProviderEvent::TextDelta(s.to_string())
    }

    #[tokio::test]
    async fn call_accumulates_text_only() {
        let provider = MockProvider {
            events: vec![
                text_event("Hello"),
                text_event(" world"),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        };
        let resp = provider
            .call(ProviderRequest {
                model: "claude-sonnet-4-5".to_string(),
                system: None,
                messages: vec![],
                tools: vec![],
                params: GenParams::default(),
            })
            .await
            .unwrap();
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert_eq!(resp.content, vec![ContentBlock::Text("Hello world".into())]);
    }

    #[tokio::test]
    async fn call_accumulates_text_and_tool_use() {
        let provider = MockProvider {
            events: vec![
                text_event("I'll read the file"),
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "read".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#"{"path":"#.to_string(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#""main.rs"}"#.to_string(),
                },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        };
        let resp = provider
            .call(ProviderRequest {
                model: "claude-sonnet-4-5".to_string(),
                system: None,
                messages: vec![],
                tools: vec![],
                params: GenParams::default(),
            })
            .await
            .unwrap();
        assert_eq!(resp.content.len(), 2);
        match &resp.content[0] {
            ContentBlock::Text(s) => assert_eq!(s, "I'll read the file"),
            _ => panic!("expected Text"),
        }
        match &resp.content[1] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "t1");
                assert_eq!(name, "read");
                assert_eq!(input, &serde_json::json!({"path": "main.rs"}));
            }
            _ => panic!("expected ToolUse"),
        }
    }

    #[tokio::test]
    async fn call_handles_stop_reason() {
        let provider = MockProvider {
            events: vec![
                text_event("truncated"),
                ProviderEvent::Stop {
                    reason: StopReason::MaxTokens,
                },
            ],
        };
        let resp = provider
            .call(ProviderRequest {
                model: "claude-sonnet-4-5".to_string(),
                system: None,
                messages: vec![],
                tools: vec![],
                params: GenParams::default(),
            })
            .await
            .unwrap();
        assert_eq!(resp.stop_reason, StopReason::MaxTokens);
    }

    #[tokio::test]
    async fn call_stream_yields_events() {
        let provider = MockProvider {
            events: vec![
                text_event("a"),
                text_event("b"),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        };
        let mut stream = provider
            .call_stream(ProviderRequest {
                model: "claude-sonnet-4-5".to_string(),
                system: None,
                messages: vec![],
                tools: vec![],
                params: GenParams::default(),
            })
            .await
            .unwrap();
        let mut collected = Vec::new();
        while let Some(e) = stream.next().await {
            collected.push(e);
        }
        assert_eq!(collected.len(), 3);
    }

    #[test]
    fn gen_params_default() {
        let p = GenParams::default();
        assert!(p.temperature.is_none());
        assert!(p.max_tokens.is_none());
    }

    #[test]
    fn token_usage_default_has_no_cache() {
        let u = TokenUsage::default();
        assert_eq!(u.input_tokens, 0);
        assert_eq!(u.output_tokens, 0);
        assert_eq!(u.cache_creation_input_tokens, None);
        assert_eq!(u.cache_read_input_tokens, None);
    }

    #[test]
    fn provider_event_usage_variant_exists() {
        let u = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_input_tokens: Some(10),
            cache_read_input_tokens: None,
        };
        let e = ProviderEvent::Usage(u.clone());
        assert!(matches!(e, ProviderEvent::Usage(_)));
    }
}
