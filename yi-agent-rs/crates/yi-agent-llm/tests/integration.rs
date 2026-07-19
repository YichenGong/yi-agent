//! Integration tests for AnthropicProvider against a wiremock mock server.

use std::sync::Mutex;
use std::time::Duration;

use futures::stream::StreamExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use yi_agent_core::{
    GenParams, Message, Provider, ProviderError, ProviderEvent, ProviderRequest, StopReason,
};
use yi_agent_llm::{AnthropicProvider, AnthropicProviderOpts};

/// Serialize tests that touch the `ANTHROPIC_API_KEY` env var to prevent cross-test interference.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const SSE_TEXT_STREAM: &str = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\"}}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";

const SSE_TOOL_USE_STREAM: &str = "\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"read\",\"input\":{}}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"main.rs\\\"}\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n";

const SSE_MAX_TOKENS_STREAM: &str = "\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"truncated\"}}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"}}\n\n";

fn provider_for(server: &MockServer) -> AnthropicProvider {
    AnthropicProvider::new(AnthropicProviderOpts {
        base_url: Some(server.uri()),
        api_key: Some("test-key".to_string()),
        api_version: None,
        timeout: Some(Duration::from_secs(5)),
    })
    .expect("provider construction")
}

fn simple_request() -> ProviderRequest {
    ProviderRequest {
        model: "claude-sonnet-4-5".to_string(),
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
        .and(path("/v1/messages"))
        .and(header("x-api-key", "test-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(SSE_TEXT_STREAM),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let stream = provider
        .call_stream(simple_request())
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
    assert_eq!(text, "Hello world");
    assert!(events
        .iter()
        .any(|e| matches!(e, ProviderEvent::Stop { .. })));
}

#[tokio::test]
async fn streams_tool_use_deltas_correctly() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(SSE_TOOL_USE_STREAM),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let stream = provider
        .call_stream(simple_request())
        .await
        .expect("stream ok");
    let events = collect_events(stream).await;

    assert!(events.iter().any(|e| matches!(
        e,
        ProviderEvent::ToolUseStart { id, name } if id == "toolu_01" && name == "read"
    )));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ProviderEvent::ToolUseDelta { .. }))
            .count(),
        2
    );
    assert!(events.iter().any(|e| matches!(
        e,
        ProviderEvent::ToolUseEnd { id } if id == "toolu_01"
    )));
}

#[tokio::test]
async fn maps_stop_reason_max_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(SSE_MAX_TOKENS_STREAM),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let stream = provider
        .call_stream(simple_request())
        .await
        .expect("stream ok");
    let events = collect_events(stream).await;
    assert!(events.iter().any(|e| matches!(
        e,
        ProviderEvent::Stop {
            reason: StopReason::MaxTokens
        }
    )));
}

#[tokio::test]
async fn returns_auth_error_on_401() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
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
        .and(path("/v1/messages"))
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
        .and(path("/v1/messages"))
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
    // Make sure the env var isn't picked up by accident.
    unsafe {
        std::env::remove_var("ANTHROPIC_API_KEY");
    }
    let result = AnthropicProvider::new(AnthropicProviderOpts {
        api_key: None,
        ..Default::default()
    });
    assert!(matches!(result, Err(ProviderError::Auth(_))));
}

#[tokio::test]
async fn reads_api_key_from_env() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var("ANTHROPIC_API_KEY", "env-key");
    }
    let provider =
        AnthropicProvider::new(AnthropicProviderOpts::default()).expect("env key picked up");
    unsafe {
        std::env::remove_var("ANTHROPIC_API_KEY");
    }
    // Sanity check: provider was constructed successfully with the env key.
    let _ = provider;
}

#[tokio::test]
async fn sends_system_message_as_top_level_system() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(wiremock::matchers::body_string_contains("\"system\":\"be helpful\""))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                ),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let req = ProviderRequest {
        model: "claude-sonnet-4-5".to_string(),
        system: None,
        messages: vec![Message::system("be helpful"), Message::user("hi")],
        tools: vec![],
        params: GenParams::default(),
    };
    let _ = provider.call_stream(req).await.expect("stream ok");
    // The mock only responds 200 if the body contained `"system":"be helpful"`.
}
