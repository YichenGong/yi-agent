use serde_json::json;
use yi_agent_core::{OutputStream, Tool, ToolEvent, ToolResult};

struct EchoTool;

#[async_trait::async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn schema(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}})
    }
    fn description(&self) -> &str {
        "echo"
    }
    async fn call(&self, _args: serde_json::Value) -> ToolResult {
        ToolResult::text("done")
    }
}

#[tokio::test]
async fn test_default_call_stream_no_events() {
    let tool = EchoTool;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ToolEvent>(16);
    let result = tool.call_stream(json!({}), tx).await;
    assert_eq!(result.content.len(), 1);
    // default impl sends no events
    assert!(rx.try_recv().is_err());
}

#[test]
fn test_tool_event_variants() {
    let e = ToolEvent::OutputDelta {
        stream: OutputStream::Stdout,
        text: "hi".into(),
    };
    assert!(matches!(
        e,
        ToolEvent::OutputDelta {
            stream: OutputStream::Stdout,
            ..
        }
    ));
    let e = ToolEvent::Exit { code: Some(0) };
    assert!(matches!(e, ToolEvent::Exit { code: Some(0) }));
    let e = ToolEvent::Timeout;
    assert!(matches!(e, ToolEvent::Timeout));
    let e = ToolEvent::Truncated {
        stream: OutputStream::Stderr,
        skipped_bytes: 100,
    };
    assert!(matches!(
        e,
        ToolEvent::Truncated {
            skipped_bytes: 100,
            ..
        }
    ));
}
