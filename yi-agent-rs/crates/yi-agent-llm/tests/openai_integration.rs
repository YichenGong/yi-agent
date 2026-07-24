//! Integration tests for OpenaiProvider against a wiremock mock server.

use std::sync::Mutex;
use std::time::Duration;

use futures::stream::StreamExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use yi_agent_core::{
    ContentBlock, GenParams, Message, Provider, ProviderError, ProviderEvent, ProviderRequest,
    ProviderResponse, StopReason,
};
use yi_agent_llm::{OpenaiProvider, OpenaiProviderOpts};

/// Serialize tests that touch the `OPENAI_API_KEY` env var.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const SSE_TEXT_STREAM: &str = "\
data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n\
data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
data: [DONE]\n\n";

const SSE_TOOL_USE_STREAM: &str = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_01\",\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"main.rs\\\"}\"}}]}}]}\n\n\
data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n\
data: [DONE]\n\n";

const SSE_MAX_TOKENS_STREAM: &str = "\
data: {\"choices\":[{\"delta\":{\"content\":\"truncated\"}}]}\n\n\
data: {\"choices\":[{\"finish_reason\":\"length\"}]}\n\n\
data: [DONE]\n\n";

fn provider_for(server: &MockServer) -> OpenaiProvider {
    OpenaiProvider::new(OpenaiProviderOpts {
        base_url: Some(server.uri()),
        api_key: Some("test-key".to_string()),
        timeout: Some(Duration::from_secs(5)),
    })
    .expect("provider construction")
}

fn simple_request() -> ProviderRequest {
    ProviderRequest {
        model: "gpt-4o".to_string(),
        system: None,
        messages: vec![Message::user("hi")],
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
async fn streams_text_deltas_correctly() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("Authorization", "Bearer test-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(SSE_TEXT_STREAM),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let stream = provider.call_stream(simple_request()).await.expect("stream ok");
    let events = collect_events(stream).await;

    let text: String = events.iter().filter_map(|e| {
        if let ProviderEvent::TextDelta(t) = e { Some(t.clone()) } else { None }
    }).collect();
    assert_eq!(text, "Hello world");
    assert!(events.iter().any(|e| matches!(e, ProviderEvent::Stop { .. })));
}

#[tokio::test]
async fn streams_tool_use_deltas_correctly() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(SSE_TOOL_USE_STREAM),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let stream = provider.call_stream(simple_request()).await.expect("stream ok");
    let events = collect_events(stream).await;

    assert!(events.iter().any(|e| matches!(
        e, ProviderEvent::ToolUseStart { id, name } if id == "call_01" && name == "read"
    )));
    assert_eq!(
        events.iter().filter(|e| matches!(e, ProviderEvent::ToolUseDelta { .. })).count(),
        2
    );
    assert!(events.iter().any(|e| matches!(
        e, ProviderEvent::ToolUseEnd { id } if id == "call_01"
    )));
    assert!(events.iter().any(|e| matches!(
        e, ProviderEvent::Stop { reason: StopReason::EndTurn }
    )));
}

#[tokio::test]
async fn maps_stop_reason_max_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(SSE_MAX_TOKENS_STREAM),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let stream = provider.call_stream(simple_request()).await.expect("stream ok");
    let events = collect_events(stream).await;
    assert!(events.iter().any(|e| matches!(
        e, ProviderEvent::Stop { reason: StopReason::MaxTokens }
    )));
}

#[tokio::test]
async fn returns_auth_error_on_401() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let result = provider.call_stream(simple_request()).await;
    assert!(matches!(result, Err(ProviderError::Auth(_))));
}

#[tokio::test]
async fn returns_rate_limited_on_429() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let result = provider.call_stream(simple_request()).await;
    assert!(matches!(result, Err(ProviderError::RateLimited)));
}

#[tokio::test]
async fn returns_server_error_on_500() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let result = provider.call_stream(simple_request()).await;
    assert!(matches!(result, Err(ProviderError::Server(_))));
}

#[tokio::test]
async fn returns_auth_error_when_no_api_key() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::remove_var("OPENAI_API_KEY");
    }
    let result = OpenaiProvider::new(OpenaiProviderOpts {
        api_key: None,
        ..Default::default()
    });
    assert!(matches!(result, Err(ProviderError::Auth(_))));
}

#[tokio::test]
async fn reads_api_key_from_env() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var("OPENAI_API_KEY", "env-key");
    }
    let provider = OpenaiProvider::new(OpenaiProviderOpts::default()).expect("env key picked up");
    unsafe {
        std::env::remove_var("OPENAI_API_KEY");
    }
    let _ = provider;
}

#[tokio::test]
async fn returns_invalid_request_error_on_400() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let result = provider.call_stream(simple_request()).await;
    assert!(matches!(result, Err(ProviderError::InvalidRequest(_))));
}

#[tokio::test]
async fn trims_trailing_slash_in_base_url() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("data: [DONE]\n\n"),
        )
        .mount(&server)
        .await;

    let provider = OpenaiProvider::new(OpenaiProviderOpts {
        base_url: Some(format!("{}/", server.uri())),
        api_key: Some("test-key".to_string()),
        timeout: Some(Duration::from_secs(5)),
    })
    .expect("provider construction");
    let _ = provider.call_stream(simple_request()).await.expect("stream ok");
}

#[tokio::test]
async fn call_accumulates_full_response_end_to_end() {
    let server = MockServer::start().await;
    let body = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hello \"}}]}\n\n\
                data: {\"choices\":[{\"delta\":{\"content\":\"world\"}}]}\n\n\
                data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_42\",\"function\":{\"name\":\"search\",\"arguments\":\"{\\\"q\\\":\\\"rust\\\"}\"}}]}}]}\n\n\
                data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n\
                data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let resp: ProviderResponse = provider.call(simple_request()).await.expect("call ok");

    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    assert_eq!(resp.content.len(), 2, "expected text + tool_use");
    match &resp.content[0] {
        ContentBlock::Text(t) => assert_eq!(t, "Hello world"),
        other => panic!("expected Text, got {other:?}"),
    }
    match &resp.content[1] {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "call_42");
            assert_eq!(name, "search");
            assert_eq!(input, &serde_json::json!({"q":"rust"}));
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

#[tokio::test]
async fn sends_system_message_as_role_system() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(wiremock::matchers::body_string_contains("\"role\":\"system\""))
        .and(wiremock::matchers::body_string_contains("be helpful"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("data: [DONE]\n\n"),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let req = ProviderRequest {
        model: "gpt-4o".to_string(),
        system: None,
        messages: vec![Message::system("be helpful"), Message::user("hi")],
        tools: vec![],
        params: GenParams::default(),
    };
    let _ = provider.call_stream(req).await.expect("stream ok");
}

#[tokio::test]
async fn mid_stream_error_becomes_terminal_stop() {
    let server = MockServer::start().await;
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n\
                data: not valid json\n\n\
                data: {\"choices\":[{\"delta\":{\"content\":\"should-not-arrive\"}}]}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let stream = provider.call_stream(simple_request()).await.expect("stream ok");
    let events = collect_events(stream).await;

    assert!(matches!(&events[0], ProviderEvent::TextDelta(t) if t == "partial"));
    match &events[1] {
        ProviderEvent::Stop { reason: StopReason::Other(msg) } => {
            assert!(msg.contains("invalid SSE JSON") || msg.contains("stream error"));
        }
        _ => panic!("expected Stop{{Other}}, got: {:?}", events[1]),
    }
    assert_eq!(events.len(), 2, "stream should terminate after Stop");
}
