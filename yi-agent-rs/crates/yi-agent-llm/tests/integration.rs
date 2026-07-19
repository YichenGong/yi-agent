//! Integration tests for AnthropicProvider against a wiremock mock server.

use std::sync::Mutex;
use std::time::Duration;

use futures::stream::StreamExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use yi_agent_core::{
    ContentBlock, GenParams, Message, Provider, ProviderError, ProviderEvent, ProviderRequest,
    ProviderResponse, StopReason,
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
    // The SSE fixture includes a `message_delta` with `stop_reason: "end_turn"`,
    // so the stream should also contain a `Stop` event.
    assert!(events.iter().any(|e| matches!(
        e,
        ProviderEvent::Stop { reason: StopReason::EndTurn }
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

#[tokio::test]
async fn returns_invalid_request_error_on_400() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let result = provider.call_stream(simple_request()).await;
    assert!(matches!(result, Err(ProviderError::InvalidRequest(_))));
}

#[tokio::test]
async fn mid_stream_sse_error_becomes_terminal_stop() {
    // Verify that an SSE `error` event mid-stream is mapped to a terminal
    // `ProviderEvent::Stop { reason: StopReason::Other(_) }` and that no
    // further events arrive after it (the stream terminates).
    let server = MockServer::start().await;
    let body = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n\
                event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"overloaded\"}}\n\n\
                event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"should-not-arrive\"}}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
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

    // First event: the text delta before the error.
    assert!(matches!(&events[0], ProviderEvent::TextDelta(t) if t == "partial"));
    // Second event: the error converted to a terminal Stop.
    match &events[1] {
        ProviderEvent::Stop { reason: StopReason::Other(msg) } => {
            assert!(msg.contains("overloaded"), "unexpected message: {msg}");
        }
        _ => panic!("expected Stop{{Other}} for mid-stream error, got: {:?}", events[1]),
    }
    // No further events after the terminal Stop.
    assert_eq!(events.len(), 2, "stream should terminate after Stop");
}

#[tokio::test]
async fn trims_trailing_slash_in_base_url() {
    // A base_url with a trailing slash should not produce `//v1/messages`.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
        )
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(AnthropicProviderOpts {
        // Intentionally add a trailing slash.
        base_url: Some(format!("{}/", server.uri())),
        api_key: Some("test-key".to_string()),
        api_version: None,
        timeout: Some(Duration::from_secs(5)),
    })
    .expect("provider construction");
    // If trailing slash weren't trimmed, the path would become `//v1/messages`
    // and wiremock would not match the `path("/v1/messages")` matcher — the
    // request would 404. The mock only responds 200 if the path is exactly
    // `/v1/messages`, so this passing verifies the trim works.
    let _ = provider.call_stream(simple_request()).await.expect("stream ok");
}

#[tokio::test]
async fn call_accumulates_full_response_end_to_end() {
    // Verify the full `Provider::call` path: the provider streams events,
    // core's `accumulate_stream` stitches them into a `ProviderResponse`
    // with `content: Vec<ContentBlock>` and `stop_reason`. This exercises
    // the integration between yi-agent-llm (streaming) and yi-agent-core
    // (accumulation), which no other test covers.
    let server = MockServer::start().await;
    let body = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n\
                event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
                event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello \"}}\n\n\
                event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"world\"}}\n\n\
                event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_42\",\"name\":\"search\",\"input\":{}}}\n\n\
                event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"q\\\":\\\"rust\\\"}\"}}\n\n\
                event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
                event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n\
                event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    // Use `call` (not `call_stream`) — this drives `accumulate_stream` in core.
    let resp: ProviderResponse = provider
        .call(simple_request())
        .await
        .expect("call ok");

    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    assert_eq!(resp.content.len(), 2, "expected text + tool_use");
    match &resp.content[0] {
        ContentBlock::Text(t) => assert_eq!(t, "Hello world"),
        other => panic!("expected Text, got {other:?}"),
    }
    match &resp.content[1] {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "toolu_42");
            assert_eq!(name, "search");
            assert_eq!(input, &serde_json::json!({"q":"rust"}));
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

#[tokio::test]
async fn mixed_text_and_tool_use_in_one_response() {
    // A single SSE response containing both a text block (index 0) and a
    // tool_use block (index 1). Verify events arrive in order and the
    // text/tool boundaries are correctly tracked.
    let server = MockServer::start().await;
    let body = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
                event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"thinking\"}}\n\n\
                event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_99\",\"name\":\"write\",\"input\":{}}}\n\n\
                event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n\
                event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
                event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
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

    // Expected order:
    //   TextDelta("thinking"), ToolUseStart{id:"toolu_99", name:"write"},
    //   ToolUseDelta{id:"toolu_99", partial_json:"{}"},
    //   ToolUseEnd{id:"toolu_99"}, Stop{EndTurn}
    assert_eq!(events.len(), 5, "events: {events:?}");
    assert!(matches!(&events[0], ProviderEvent::TextDelta(t) if t == "thinking"));
    assert!(matches!(
        &events[1],
        ProviderEvent::ToolUseStart { id, name } if id == "toolu_99" && name == "write"
    ));
    assert!(matches!(
        &events[2],
        ProviderEvent::ToolUseDelta { id, partial_json } if id == "toolu_99" && partial_json == "{}"
    ));
    assert!(matches!(
        &events[3],
        ProviderEvent::ToolUseEnd { id } if id == "toolu_99"
    ));
    assert!(matches!(
        &events[4],
        ProviderEvent::Stop { reason: StopReason::EndTurn }
    ));
}

#[tokio::test]
async fn multiple_tool_use_blocks_in_one_response() {
    // Two separate tool_use blocks (index 0 and index 1) in a single
    // response. Verifies `block_ids` cache correctly isolates ids across
    // blocks and both ToolUseEnd events fire with the right id.
    let server = MockServer::start().await;
    let body = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_a\",\"name\":\"read\",\"input\":{}}}\n\n\
                event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"p\\\":1}\"}}\n\n\
                event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_b\",\"name\":\"write\",\"input\":{}}}\n\n\
                event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"p\\\":2}\"}}\n\n\
                event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
                event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
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

    // Expected: StartA, DeltaA, EndA, StartB, DeltaB, EndB, Stop
    assert_eq!(events.len(), 7, "events: {events:?}");
    assert!(matches!(
        &events[0],
        ProviderEvent::ToolUseStart { id, name } if id == "toolu_a" && name == "read"
    ));
    assert!(matches!(
        &events[1],
        ProviderEvent::ToolUseDelta { id, partial_json } if id == "toolu_a" && partial_json == "{\"p\":1}"
    ));
    assert!(matches!(
        &events[2],
        ProviderEvent::ToolUseEnd { id } if id == "toolu_a"
    ));
    assert!(matches!(
        &events[3],
        ProviderEvent::ToolUseStart { id, name } if id == "toolu_b" && name == "write"
    ));
    assert!(matches!(
        &events[4],
        ProviderEvent::ToolUseDelta { id, partial_json } if id == "toolu_b" && partial_json == "{\"p\":2}"
    ));
    assert!(matches!(
        &events[5],
        ProviderEvent::ToolUseEnd { id } if id == "toolu_b"
    ));
    assert!(matches!(
        &events[6],
        ProviderEvent::Stop { reason: StopReason::EndTurn }
    ));
}

#[tokio::test]
async fn stop_sequence_stop_reason_end_to_end() {
    // The unit test `maps_stop_reasons_correctly` covers the string mapping in
    // isolation; this test verifies the full provider path emits
    // `Stop { reason: StopReason::StopSequence }` when the SSE stream contains
    // a `stop_reason: "stop_sequence"` in `message_delta`.
    let server = MockServer::start().await;
    let body = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
                event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"stopped early\"}}\n\n\
                event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"stop_sequence\",\"stop_sequence\":\"\\nUser:\"}}\n\n\
                event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
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

    // Last event should be Stop{StopSequence} (not EndTurn or Other).
    let stop = events.iter().find(|e| matches!(e, ProviderEvent::Stop { .. }));
    assert!(stop.is_some(), "no Stop event in: {events:?}");
    match stop.unwrap() {
        ProviderEvent::Stop { reason } => {
            assert_eq!(*reason, StopReason::StopSequence, "events: {events:?}");
        }
        _ => unreachable!(),
    }
}
