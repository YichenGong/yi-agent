//! Real-API smoke tests for AnthropicProvider and OpenaiProvider.
//! All tests are #[ignore]'d; run with:
//!   cargo test -p yi-agent-llm --test real_integration -- --ignored

use std::time::Duration;

use futures::stream::StreamExt;
use yi_agent_core::{
    ContentBlock, GenParams, Message, Provider, ProviderEvent, ProviderRequest, ProviderResponse,
    ToolSchema,
};
use yi_agent_llm::{AnthropicProvider, AnthropicProviderOpts};

fn skip_if_no_key(env_var: &str) -> Option<String> {
    match std::env::var(env_var) {
        Ok(k) if !k.is_empty() => Some(k),
        _ => {
            eprintln!("skip: no {env_var}");
            None
        }
    }
}

fn simple_request(model: &str) -> ProviderRequest {
    ProviderRequest {
        model: model.to_string(),
        system: None,
        messages: vec![Message::user("Reply with exactly: hello world")],
        tools: vec![],
        params: GenParams::default(),
    }
}

async fn collect_events(
    stream: futures::stream::BoxStream<'static, ProviderEvent>,
) -> Vec<ProviderEvent> {
    let mut s = stream;
    let mut out = Vec::new();
    while let Some(e) = s.next().await {
        out.push(e);
    }
    out
}

#[tokio::test]
#[ignore]
async fn real_anthropic_text_stream() {
    let key = match skip_if_no_key("ANTHROPIC_API_KEY") {
        Some(k) => k,
        None => return,
    };
    let provider = AnthropicProvider::new(AnthropicProviderOpts {
        api_key: Some(key),
        timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    })
    .expect("provider construction");

    let stream = provider
        .call_stream(simple_request("claude-sonnet-4-5"))
        .await
        .expect("stream ok");
    let events = collect_events(stream).await;

    let text: String = events
        .iter()
        .filter_map(|e| {
            if let ProviderEvent::TextDelta(t) = e {
                Some(t.clone())
            } else {
                None
            }
        })
        .collect();
    assert!(!text.is_empty(), "should have text, events: {events:?}");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProviderEvent::Stop { .. })),
        "should have Stop event"
    );
}

#[tokio::test]
#[ignore]
async fn real_anthropic_tool_use() {
    let key = match skip_if_no_key("ANTHROPIC_API_KEY") {
        Some(k) => k,
        None => return,
    };
    let provider = AnthropicProvider::new(AnthropicProviderOpts {
        api_key: Some(key),
        timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    })
    .expect("provider");

    let req = ProviderRequest {
        model: "claude-sonnet-4-5".to_string(),
        system: None,
        messages: vec![Message::user("What is 2+2? Use the calculator tool.")],
        tools: vec![ToolSchema {
            name: "calculator".to_string(),
            description: "Basic calculator".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "expr": {"type": "string"}
                },
                "required": ["expr"]
            }),
        }],
        params: GenParams::default(),
    };

    let stream = provider.call_stream(req).await.expect("stream ok");
    let events = collect_events(stream).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProviderEvent::ToolUseStart { .. })),
        "should have ToolUseStart, events: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProviderEvent::ToolUseEnd { .. })),
        "should have ToolUseEnd"
    );
}

#[tokio::test]
#[ignore]
async fn real_anthropic_call_accumulate() {
    let key = match skip_if_no_key("ANTHROPIC_API_KEY") {
        Some(k) => k,
        None => return,
    };
    let provider = AnthropicProvider::new(AnthropicProviderOpts {
        api_key: Some(key),
        timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    })
    .expect("provider");

    let resp: ProviderResponse = provider
        .call(simple_request("claude-sonnet-4-5"))
        .await
        .expect("call ok");

    assert!(!resp.content.is_empty(), "should have content");
    assert!(
        resp.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text(_))),
        "should have text block, got: {:?}",
        resp.content
    );
}

// === OpenAI provider real-API tests ===

use yi_agent_llm::{OpenaiProvider, OpenaiProviderOpts};

#[tokio::test]
#[ignore]
async fn real_openai_text_stream() {
    let key = match skip_if_no_key("OPENAI_API_KEY") {
        Some(k) => k,
        None => return,
    };
    let provider = OpenaiProvider::new(OpenaiProviderOpts {
        api_key: Some(key),
        timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    })
    .expect("provider");

    let stream = provider
        .call_stream(simple_request("gpt-4o"))
        .await
        .expect("stream ok");
    let events = collect_events(stream).await;

    let text: String = events
        .iter()
        .filter_map(|e| {
            if let ProviderEvent::TextDelta(t) = e {
                Some(t.clone())
            } else {
                None
            }
        })
        .collect();
    assert!(!text.is_empty(), "should have text");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProviderEvent::Stop { .. }))
    );
}

#[tokio::test]
#[ignore]
async fn real_openai_tool_use() {
    let key = match skip_if_no_key("OPENAI_API_KEY") {
        Some(k) => k,
        None => return,
    };
    let provider = OpenaiProvider::new(OpenaiProviderOpts {
        api_key: Some(key),
        timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    })
    .expect("provider");

    let req = ProviderRequest {
        model: "gpt-4o".to_string(),
        system: None,
        messages: vec![Message::user("What is 2+2? Use the calculator tool.")],
        tools: vec![ToolSchema {
            name: "calculator".to_string(),
            description: "Basic calculator".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"expr": {"type": "string"}},
                "required": ["expr"]
            }),
        }],
        params: GenParams::default(),
    };

    let events = collect_events(provider.call_stream(req).await.expect("stream ok")).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ProviderEvent::ToolUseStart { .. })),
        "should have ToolUseStart, events: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ProviderEvent::ToolUseEnd { .. })),
        "should have ToolUseEnd, events: {events:?}"
    );
}

#[tokio::test]
#[ignore]
async fn real_openai_call_accumulate() {
    let key = match skip_if_no_key("OPENAI_API_KEY") {
        Some(k) => k,
        None => return,
    };
    let provider = OpenaiProvider::new(OpenaiProviderOpts {
        api_key: Some(key),
        timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    })
    .expect("provider");

    let resp: ProviderResponse = provider
        .call(simple_request("gpt-4o"))
        .await
        .expect("call ok");

    assert!(!resp.content.is_empty());
    assert!(
        resp.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text(_)))
    );
}
