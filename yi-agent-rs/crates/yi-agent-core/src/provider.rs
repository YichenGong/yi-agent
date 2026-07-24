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
/// `on_event` is called for each provider event (text delta, usage, etc.).
pub async fn accumulate_stream<F>(
    mut stream: BoxStream<'static, ProviderEvent>,
    mut on_event: F,
) -> Result<(Vec<ContentBlock>, StopReason), ProviderError>
where
    F: FnMut(ProviderEvent),
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
                on_event(ProviderEvent::TextDelta(s));
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
            ProviderEvent::Usage(u) => {
                on_event(ProviderEvent::Usage(u));
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

    #[tokio::test]
    async fn accumulate_stream_forwards_usage_via_callback() {
        let events = vec![
            text_event("hi"),
            ProviderEvent::Usage(TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            }),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ];
        let provider = MockProvider { events };
        let stream = provider
            .call_stream(ProviderRequest {
                model: "test".into(),
                system: None,
                messages: vec![],
                tools: vec![],
                params: GenParams::default(),
            })
            .await
            .unwrap();

        let mut received_text = Vec::new();
        let mut received_usage = Vec::new();
        let (content, stop) = accumulate_stream(stream, |ev| match ev {
            ProviderEvent::TextDelta(s) => received_text.push(s),
            ProviderEvent::Usage(u) => received_usage.push(u),
            _ => {}
        })
        .await
        .unwrap();

        assert_eq!(content, vec![ContentBlock::Text("hi".into())]);
        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(received_text, vec!["hi".to_string()]);
        assert_eq!(received_usage.len(), 1);
        assert_eq!(received_usage[0].input_tokens, 10);
    }

    #[tokio::test]
    async fn accumulate_stream_multiple_tool_uses_in_order() {
        let events = vec![
            ProviderEvent::ToolUseStart {
                id: "t1".into(),
                name: "read".into(),
            },
            ProviderEvent::ToolUseDelta {
                id: "t1".into(),
                partial_json: r#"{"p":"a"}"#.into(),
            },
            ProviderEvent::ToolUseEnd { id: "t1".into() },
            ProviderEvent::ToolUseStart {
                id: "t2".into(),
                name: "write".into(),
            },
            ProviderEvent::ToolUseDelta {
                id: "t2".into(),
                partial_json: r#"{"p":"b"}"#.into(),
            },
            ProviderEvent::ToolUseEnd { id: "t2".into() },
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ];
        let provider = MockProvider { events };
        let stream = provider
            .call_stream(ProviderRequest {
                model: "test".into(),
                system: None,
                messages: vec![],
                tools: vec![],
                params: GenParams::default(),
            })
            .await
            .unwrap();
        let (content, _) = accumulate_stream(stream, |_| {}).await.unwrap();
        assert_eq!(content.len(), 2);
        match &content[0] {
            ContentBlock::ToolUse { id, name, .. } => {
                assert_eq!(id, "t1");
                assert_eq!(name, "read");
            }
            _ => panic!("expected ToolUse t1"),
        }
        match &content[1] {
            ContentBlock::ToolUse { id, name, .. } => {
                assert_eq!(id, "t2");
                assert_eq!(name, "write");
            }
            _ => panic!("expected ToolUse t2"),
        }
    }

    #[tokio::test]
    async fn accumulate_stream_malformed_json_returns_error() {
        let events = vec![
            ProviderEvent::ToolUseStart {
                id: "t1".into(),
                name: "read".into(),
            },
            ProviderEvent::ToolUseDelta {
                id: "t1".into(),
                partial_json: "not valid json".into(),
            },
            ProviderEvent::ToolUseEnd { id: "t1".into() },
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ];
        let provider = MockProvider { events };
        let stream = provider
            .call_stream(ProviderRequest {
                model: "test".into(),
                system: None,
                messages: vec![],
                tools: vec![],
                params: GenParams::default(),
            })
            .await
            .unwrap();
        let result = accumulate_stream(stream, |_| {}).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ProviderError::Stream(msg) => assert!(msg.contains("malformed tool use JSON")),
            other => panic!("expected Stream error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn accumulate_stream_text_after_tool_use() {
        let events = vec![
            ProviderEvent::ToolUseStart {
                id: "t1".into(),
                name: "read".into(),
            },
            ProviderEvent::ToolUseDelta {
                id: "t1".into(),
                partial_json: "{}".into(),
            },
            ProviderEvent::ToolUseEnd { id: "t1".into() },
            ProviderEvent::TextDelta("after tool".into()),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ];
        let provider = MockProvider { events };
        let stream = provider
            .call_stream(ProviderRequest {
                model: "test".into(),
                system: None,
                messages: vec![],
                tools: vec![],
                params: GenParams::default(),
            })
            .await
            .unwrap();
        let (content, _) = accumulate_stream(stream, |_| {}).await.unwrap();
        assert_eq!(content.len(), 2);
        assert!(matches!(content[0], ContentBlock::ToolUse { .. }));
        match &content[1] {
            ContentBlock::Text(s) => assert_eq!(s, "after tool"),
            _ => panic!("expected Text after tool"),
        }
    }

    #[tokio::test]
    async fn accumulate_stream_orphan_delta_no_panic() {
        // Delta without a preceding Start — should be silently ignored, no panic.
        let events = vec![
            ProviderEvent::ToolUseDelta {
                id: "ghost".into(),
                partial_json: "{}".into(),
            },
            ProviderEvent::ToolUseEnd { id: "ghost".into() },
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ];
        let provider = MockProvider { events };
        let stream = provider
            .call_stream(ProviderRequest {
                model: "test".into(),
                system: None,
                messages: vec![],
                tools: vec![],
                params: GenParams::default(),
            })
            .await
            .unwrap();
        let (content, _) = accumulate_stream(stream, |_| {}).await.unwrap();
        assert!(content.is_empty());
    }

    #[tokio::test]
    async fn accumulate_stream_empty_yields_default() {
        let provider = MockProvider { events: vec![] };
        let stream = provider
            .call_stream(ProviderRequest {
                model: "test".into(),
                system: None,
                messages: vec![],
                tools: vec![],
                params: GenParams::default(),
            })
            .await
            .unwrap();
        let (content, stop) = accumulate_stream(stream, |_| {}).await.unwrap();
        assert!(content.is_empty());
        assert_eq!(stop, StopReason::EndTurn);
    }

    #[tokio::test]
    async fn accumulate_stream_stop_sequence_reason() {
        let events = vec![
            text_event("stopped"),
            ProviderEvent::Stop {
                reason: StopReason::StopSequence,
            },
        ];
        let provider = MockProvider { events };
        let stream = provider
            .call_stream(ProviderRequest {
                model: "test".into(),
                system: None,
                messages: vec![],
                tools: vec![],
                params: GenParams::default(),
            })
            .await
            .unwrap();
        let (_, stop) = accumulate_stream(stream, |_| {}).await.unwrap();
        assert_eq!(stop, StopReason::StopSequence);
    }

    #[tokio::test]
    async fn accumulate_stream_other_stop_reason() {
        let events = vec![
            text_event("content"),
            ProviderEvent::Stop {
                reason: StopReason::Other("custom".into()),
            },
        ];
        let provider = MockProvider { events };
        let stream = provider
            .call_stream(ProviderRequest {
                model: "test".into(),
                system: None,
                messages: vec![],
                tools: vec![],
                params: GenParams::default(),
            })
            .await
            .unwrap();
        let (_, stop) = accumulate_stream(stream, |_| {}).await.unwrap();
        assert_eq!(stop, StopReason::Other("custom".into()));
    }

    #[test]
    fn provider_error_display_messages() {
        assert_eq!(
            ProviderError::Network("conn refused".into()).to_string(),
            "network error: conn refused"
        );
        assert_eq!(
            ProviderError::Auth("bad key".into()).to_string(),
            "authentication failed: bad key"
        );
        assert_eq!(ProviderError::RateLimited.to_string(), "rate limited");
        assert_eq!(
            ProviderError::InvalidRequest("bad".into()).to_string(),
            "invalid request: bad"
        );
        assert_eq!(
            ProviderError::Server("500".into()).to_string(),
            "server error: 500"
        );
        assert_eq!(
            ProviderError::Stream("broken".into()).to_string(),
            "stream error: broken"
        );
    }

    #[test]
    fn stop_reason_eq_variants() {
        assert_eq!(StopReason::EndTurn, StopReason::EndTurn);
        assert_ne!(StopReason::EndTurn, StopReason::MaxTokens);
        assert_eq!(StopReason::Other("x".into()), StopReason::Other("x".into()));
        assert_ne!(StopReason::Other("x".into()), StopReason::Other("y".into()));
    }

    #[test]
    fn token_usage_with_cache_fields() {
        let u = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_input_tokens: Some(20),
            cache_read_input_tokens: Some(10),
        };
        assert_eq!(u.cache_creation_input_tokens, Some(20));
        assert_eq!(u.cache_read_input_tokens, Some(10));
        assert_eq!(u.input_tokens + u.output_tokens, 150);
    }
}
