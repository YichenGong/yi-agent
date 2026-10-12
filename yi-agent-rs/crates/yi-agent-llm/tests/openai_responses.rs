//! 通道分派与 auto 透明回退的集成测试（wiremock）。

use std::time::Duration;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use yi_agent_core::{GenParams, Message, Provider, ProviderRequest, StopReason};
use yi_agent_llm::{OpenaiProvider, OpenaiProviderOpts};

/// 最小 chat SSE：一段文本 + `stop` + `[DONE]`。
const SSE_TEXT_ONLY: &str = "\
data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n\
data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
data: [DONE]\n\n";

/// 最小 responses SSE：一段 output text + `response.completed`。
const SSE_RESPONSES_TEXT_ONLY: &str = "\
event: response.output_text.delta\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n\
event: response.completed\n\
data: {\"type\":\"response.completed\",\"response\":{}}\n\n";

fn simple_request() -> ProviderRequest {
    ProviderRequest {
        model: "gpt-4o".to_string(),
        system: None,
        messages: vec![Message::user("hi")],
        tools: vec![],
        params: GenParams::default(),
    }
}

fn provider_with_style(server: &MockServer, style: &str) -> OpenaiProvider {
    OpenaiProvider::new(OpenaiProviderOpts {
        base_url: Some(server.uri()),
        api_key: Some("k".into()),
        timeout: Some(Duration::from_secs(5)),
        api_style: Some(style.into()),
        reasoning_effort: None,
    })
    .expect("provider construction")
}

#[tokio::test]
async fn auto_probe_falls_back_to_chat_when_responses_404s_empty() {
    let server = MockServer::start().await;
    // /v1/responses → 404 空 body（网关不支持）
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(404).set_body_string(""))
        .mount(&server)
        .await;
    // chat 正常
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SSE_TEXT_ONLY))
        .mount(&server)
        .await;

    let provider = provider_with_style(&server, "auto");
    let resp = provider
        .call(simple_request())
        .await
        .expect("auto fallback must succeed");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    // 至少收到一条 Notice 告知已回退
    assert!(resp.notices.iter().any(|n| n.text.contains("Chat")));
}

#[tokio::test]
async fn auto_probe_uses_responses_when_available() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SSE_RESPONSES_TEXT_ONLY))
        .mount(&server)
        .await;
    let provider = provider_with_style(&server, "auto");
    let resp = provider
        .call(simple_request())
        .await
        .expect("responses must work");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn explicit_chat_style_never_probes_responses() {
    let server = MockServer::start().await;
    // 只挂 chat；若代码探测 responses 会收到 wiremock 未预期请求（或 404）
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SSE_TEXT_ONLY))
        .mount(&server)
        .await;
    let provider = provider_with_style(&server, "chat");
    let _ = provider
        .call(simple_request())
        .await
        .expect("chat style ok");
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/v1/responses")
            .count(),
        0
    );
}

#[tokio::test]
async fn auto_does_not_fall_back_on_404_with_json_body() {
    // 带 JSON body 的 404 = 端点存在但请求有问题（如 model_not_found），
    // 绝不能当成「路径不存在」而回退。
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_string(r#"{"error":{"message":"model not found"}}"#),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SSE_TEXT_ONLY))
        .mount(&server)
        .await;

    let provider = provider_with_style(&server, "auto");
    let result = provider.call(simple_request()).await;
    assert!(
        result.is_err(),
        "404 with a JSON body must surface as an error, got {result:?}"
    );
    // 未回退：chat 端点一次都没被调用。
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/v1/chat/completions")
            .count(),
        0
    );
}

#[tokio::test]
async fn auto_caches_chat_so_later_calls_skip_responses() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(404).set_body_string(""))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SSE_TEXT_ONLY))
        .mount(&server)
        .await;

    let provider = provider_with_style(&server, "auto");
    let first = provider.call(simple_request()).await.expect("first call");
    assert!(first.notices.iter().any(|n| n.text.contains("Chat")));

    // 第二次请求命中缓存，直接走 chat，不再 POST responses、也不再补 Notice。
    let second = provider.call(simple_request()).await.expect("second call");
    assert!(
        second.notices.is_empty(),
        "second call must reuse the cached channel: {:?}",
        second.notices
    );

    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/v1/responses")
            .count(),
        1,
        "the missing-route probe must happen at most once"
    );
}
