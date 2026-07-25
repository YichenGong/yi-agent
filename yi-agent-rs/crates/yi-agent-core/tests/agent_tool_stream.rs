use async_trait::async_trait;
use futures::stream::StreamExt;
use serde_json::json;
use std::sync::Arc;
use yi_agent_core::ToolResult;
use yi_agent_core::provider::{
    Provider, ProviderError, ProviderEvent, ProviderRequest, StopReason, TokenUsage,
};
use yi_agent_core::{Agent, AgentConfig, AgentEvent, OutputStream, Tool, ToolEvent, ToolRegistry};

struct DummyProvider;

#[async_trait]
impl Provider for DummyProvider {
    async fn call_stream(
        &self,
        _req: ProviderRequest,
    ) -> Result<futures::stream::BoxStream<'static, ProviderEvent>, ProviderError> {
        // emit one tool_use then stop with EndTurn (agent loop checks tool_uses, not stop_reason)
        let events = vec![
            ProviderEvent::ToolUseStart {
                id: "t1".into(),
                name: "stream_tool".into(),
            },
            ProviderEvent::ToolUseDelta {
                id: "t1".into(),
                partial_json: r#"{"command":"echo hi"}"#.into(),
            },
            ProviderEvent::ToolUseEnd { id: "t1".into() },
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
            ProviderEvent::Usage(TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            }),
        ];
        Ok(futures::stream::iter(events).boxed())
    }
}

struct StreamingTool;

#[async_trait]
impl Tool for StreamingTool {
    fn name(&self) -> &str {
        "stream_tool"
    }
    fn schema(&self) -> serde_json::Value {
        json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]})
    }
    fn description(&self) -> &str {
        "stream"
    }
    async fn call(&self, _args: serde_json::Value) -> ToolResult {
        ToolResult::text("ok")
    }
    async fn call_stream(
        &self,
        _args: serde_json::Value,
        tx: tokio::sync::mpsc::Sender<ToolEvent>,
    ) -> ToolResult {
        let _ = tx
            .send(ToolEvent::OutputDelta {
                stream: OutputStream::Stdout,
                text: "hi".into(),
            })
            .await;
        let _ = tx.send(ToolEvent::Exit { code: Some(0) }).await;
        ToolResult::text("ok")
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_agent_forwards_tool_output_delta() {
    let mut registry = ToolRegistry::default();
    registry.register(Arc::new(StreamingTool));
    let mut agent = Agent::new(
        Arc::new(DummyProvider),
        Arc::new(registry),
        AgentConfig::default(),
    );
    let mut stream = agent.run("test".into()).await.unwrap();
    let mut saw_output_delta = false;
    let mut saw_exit = false;
    while let Some(ev) = stream.next().await {
        match ev {
            AgentEvent::ToolOutputDelta { text, .. } if text.contains("hi") => {
                saw_output_delta = true;
            }
            AgentEvent::ToolExit { code: Some(0), .. } => {
                saw_exit = true;
            }
            _ => {}
        }
    }
    assert!(saw_output_delta, "expected ToolOutputDelta event");
    assert!(saw_exit, "expected ToolExit event");
}
