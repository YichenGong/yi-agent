//! 通道分派与 auto 透明回退的集成测试（wiremock）。

use std::time::Duration;

use futures::StreamExt;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use yi_agent_core::{
    Agent, AgentConfig, GenParams, Message, Provider, ProviderRequest, StopReason, ToolRegistry,
};
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

fn provider_with_style_and_effort(
    server: &MockServer,
    style: &str,
    effort: &str,
) -> OpenaiProvider {
    OpenaiProvider::new(OpenaiProviderOpts {
        base_url: Some(server.uri()),
        api_key: Some("k".into()),
        timeout: Some(Duration::from_secs(5)),
        api_style: Some(style.into()),
        reasoning_effort: Some(effort.into()),
    })
    .expect("provider construction")
}

async fn count_chat_requests(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/v1/chat/completions")
        .count()
}

async fn count_chat_requests_with_effort(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/v1/chat/completions")
        .filter(|r| String::from_utf8_lossy(&r.body).contains("reasoning_effort"))
        .count()
}

async fn count_chat_requests_without_effort(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/v1/chat/completions")
        .filter(|r| !String::from_utf8_lossy(&r.body).contains("reasoning_effort"))
        .count()
}

/// The wire-level end-to-end proof (spec §7.2): the tool loop's SECOND HTTP
/// request really carries the FIRST round's reasoning credential.
///
/// Everything below this test asserts on an in-memory `ProviderRequest` or a
/// serialized struct. Only this one drives a real `Agent` over a real
/// `OpenaiProvider` against an HTTP server, so only this one can prove the
/// credential survives the whole path: Responses SSE -> `ProviderEvent::
/// ThinkingBlock` -> accumulated content -> assistant message in the session ->
/// `ProviderRequest.messages` -> `responses_request_from` -> the actual POST body.
///
/// The server answers the FIRST POST with a `reasoning` item carrying
/// `encrypted_content` followed by a `function_call`; the loop then calls the
/// (unregistered) tool, gets a tool-not-found result, and issues a SECOND POST.
/// That second request body is what we assert on.
const SSE_RESPONSES_REASONING_THEN_TOOL_CALL: &str = "\
event: response.output_item.done\n\
data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"enc-round-one\",\"summary\":[]}}\n\n\
event: response.output_item.added\n\
data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"echo\",\"arguments\":\"\"}}\n\n\
event: response.function_call_arguments.delta\n\
data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{}\"}\n\n\
event: response.output_item.done\n\
data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"echo\",\"arguments\":\"{}\"}}\n\n\
event: response.completed\n\
data: {\"type\":\"response.completed\",\"response\":{}}\n\n";

#[tokio::test(flavor = "multi_thread")]
async fn tool_loop_second_request_body_replays_the_first_round_reasoning_item() {
    let server = MockServer::start().await;

    // The first POST carries no conversation history yet (messages == [user "hi"]).
    // The extra predicate keeps this mock from also swallowing later rounds:
    // `body_partial_json` compares arrays by index inclusively, so the `input[0]`
    // user item alone would match every round.
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(body_partial_json(serde_json::json!({
            "model": "gpt-4o",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
        })))
        .and(|r: &wiremock::Request| {
            let body: serde_json::Value =
                serde_json::from_slice(&r.body).expect("request body must be JSON");
            body["input"]
                .as_array()
                .is_some_and(|input| !input.iter().any(|i| i["type"] == "reasoning"))
        })
        .respond_with(
            ResponseTemplate::new(200).set_body_string(SSE_RESPONSES_REASONING_THEN_TOOL_CALL),
        )
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;

    // The second POST must already carry the reasoning item AND the function_call,
    // in that order. Index-aligned expectations pin both the content and the
    // ordering; if either were missing this mock would not match and the loop
    // would surface a 404 instead.
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(body_partial_json(serde_json::json!({
            "model": "gpt-4o",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "reasoning", "encrypted_content": "enc-round-one"},
                {"type": "function_call", "call_id": "call_1", "name": "echo"}
            ]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_string(SSE_RESPONSES_TEXT_ONLY))
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;

    // `echo` is deliberately unregistered: the loop gets a tool-not-found result
    // and still advances to a second round.
    let provider = provider_with_style(&server, "responses");
    // Pin the model the agent sends (`AgentConfig::default()` is a Claude model)
    // so the mock can assert on a realistic body.
    let config = AgentConfig {
        model: "gpt-4o".into(),
        ..AgentConfig::default()
    };
    let mut agent = Agent::new(
        std::sync::Arc::new(provider),
        std::sync::Arc::new(ToolRegistry::new()),
        config,
    );

    let mut stream = agent.run("hi".into()).await.expect("agent run");
    // Drain the stream to completion. The bound is a guard, not an expectation:
    // a healthy loop ends on its own, and the bound stops a hypothetical
    // duplicate-`Done` regression from hanging the suite.
    let mut done = 0usize;
    let mut seen = 0usize;
    while let Some(event) = stream.next().await {
        seen += 1;
        assert!(seen < 1000, "the agent event stream must terminate");
        if matches!(event, yi_agent_core::AgentEvent::Done { .. }) {
            done += 1;
        }
    }
    let requests = server.received_requests().await.expect("received requests");
    assert_eq!(done, 1, "the tool loop must finish with exactly one Done");

    let bodies: Vec<serde_json::Value> = requests
        .iter()
        .filter(|r| r.url.path() == "/v1/responses")
        .map(|r| serde_json::from_slice(&r.body).expect("request body must be JSON"))
        .collect();
    assert_eq!(
        bodies.len(),
        2,
        "the tool loop must issue exactly two HTTP requests, got {}",
        bodies.len()
    );

    // The assertion that matters: the REAL second-round HTTP body replays the
    // credential the first round produced.
    let input = bodies[1]["input"]
        .as_array()
        .expect("the second request must carry an input array");
    let reasoning_at = input
        .iter()
        .position(|i| i["type"] == "reasoning")
        .unwrap_or_else(|| {
            panic!("the second request must replay a reasoning item, got input: {input:?}")
        });
    assert_eq!(
        input[reasoning_at]["encrypted_content"], "enc-round-one",
        "the credential must be replayed byte-for-byte"
    );
    let call_at = input
        .iter()
        .position(|i| i["type"] == "function_call")
        .expect("the function_call must be replayed alongside it");
    assert!(
        reasoning_at < call_at,
        "the reasoning item must precede its function_call (server returns them in that order)"
    );
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

#[tokio::test]
async fn unsupported_effort_is_stripped_and_retried_with_a_notice() {
    let server = MockServer::start().await;
    // 第一次：带 reasoning_effort 的 chat 请求 → 400（模拟 Sol 拒 none/minimal）
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(
            serde_json::json!({"reasoning_effort":"minimal"}),
        ))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"message":"Unsupported value","param":"reasoning_effort","code":"unsupported_value"}}"#,
        ))
        .mount(&server)
        .await;
    // 第二次：不带该字段 → 200
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(|r: &wiremock::Request| !String::from_utf8_lossy(&r.body).contains("reasoning_effort"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SSE_TEXT_ONLY))
        .mount(&server)
        .await;

    let provider = provider_with_style_and_effort(&server, "chat", "minimal");
    let resp = provider
        .call(simple_request())
        .await
        .expect("strip+retry must succeed");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    assert!(resp.notices.iter().any(|n| n.text.contains("推理档")));
    // 恰好一次带 effort（被拒）+ 一次不带 effort（成功）。
    assert_eq!(count_chat_requests_with_effort(&server).await, 1);
    assert_eq!(count_chat_requests_without_effort(&server).await, 1);
}

#[tokio::test]
async fn rejected_effort_is_remembered_so_later_calls_omit_it() {
    let server = MockServer::start().await;
    // 仅在带 reasoning_effort 时 400。
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(|r: &wiremock::Request| String::from_utf8_lossy(&r.body).contains("reasoning_effort"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"message":"reasoning_effort are not supported for gpt-6-luna","param":"reasoning_effort"}}"#,
        ))
        .mount(&server)
        .await;
    // 不带该字段 → 200。
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(|r: &wiremock::Request| !String::from_utf8_lossy(&r.body).contains("reasoning_effort"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SSE_TEXT_ONLY))
        .mount(&server)
        .await;

    let provider = provider_with_style_and_effort(&server, "chat", "minimal");

    // 第一次：带 effort 被拒 → 剥离重投成功。
    let first = provider.call(simple_request()).await.expect("first call");
    assert!(first.notices.iter().any(|n| n.text.contains("推理档")));

    // 第二次：命中拒绝缓存，首次 POST 就不带 effort。
    let second = provider
        .call(simple_request())
        .await
        .expect("second call must skip the rejected effort");
    assert_eq!(second.stop_reason, StopReason::EndTurn);

    assert_eq!(
        count_chat_requests_with_effort(&server).await,
        1,
        "only the very first POST should ever carry the rejected effort"
    );
    assert_eq!(count_chat_requests_without_effort(&server).await, 2);
    assert_eq!(count_chat_requests(&server).await, 3);
}

#[tokio::test]
async fn non_effort_400_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"message":"Unsupported value for temperature","param":"temperature"}}"#,
        ))
        .mount(&server)
        .await;

    let provider = provider_with_style_and_effort(&server, "chat", "minimal");
    provider
        .call(simple_request())
        .await
        .expect_err("a non-effort 400 must stay an error");
    assert_eq!(
        count_chat_requests(&server).await,
        1,
        "a non-effort rejection must not be retried"
    );
}

/// 线上真实形态的「网关内部组件瞬时拒绝」响应体（外层内层均为 JSON，内层被转义）。
const TRANSIENT_MAAS_BODY: &str = concat!(
    r#"{"error":{"message":"{\"error\":{\"type\":\"invalid_request_error\",\"code\":\"400001\","#,
    r#"\"message\":\"The request is invalid: the request was rejected by an internal MaaS component. "#,
    r#"Please check the request body, required fields, and request format.\",\"source\":\"client\","#,
    r#"\"request_id\":\"5f5338e2-5c52-46b3-99b2-6c0780e2dcc4\"}}\n","type":"upstream_error"}}"#
);

async fn count_responses_requests(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/v1/responses")
        .count()
}

#[tokio::test]
async fn a_transient_maas_rejection_is_retried_unchanged_until_it_succeeds() {
    let server = MockServer::start().await;
    // 前两次：网关内部组件瞬时拒绝（body 逐字节相同，无 param 可指认）→ 400。
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(400).set_body_string(TRANSIENT_MAAS_BODY))
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&server)
        .await;
    // 之后：恢复正常流式。
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SSE_RESPONSES_TEXT_ONLY))
        .with_priority(2)
        .mount(&server)
        .await;

    let provider = provider_with_style(&server, "responses");
    let resp = provider
        .call(simple_request())
        .await
        .expect("a transient gateway rejection must be retried and then succeed");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    assert_eq!(
        count_responses_requests(&server).await,
        3,
        "two rejected attempts plus one success"
    );
}

#[tokio::test]
async fn a_transient_rejection_that_never_clears_stays_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(400).set_body_string(TRANSIENT_MAAS_BODY))
        .mount(&server)
        .await;

    let provider = provider_with_style(&server, "responses");
    provider
        .call(simple_request())
        .await
        .expect_err("an unrelenting rejection must surface, not loop forever");
    assert_eq!(
        count_responses_requests(&server).await,
        3,
        "the retry budget is bounded (initial attempt + 2 retries)"
    );
}

#[tokio::test]
async fn a_maas_rejection_naming_a_field_is_not_retried() {
    let server = MockServer::start().await;
    // 同一个网关错误码，但指名了字段 —— 这是确定性问题，重投必然再失败。
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"message":"x","type":"upstream_error","code":"400001","param":"reasoning_effort"}}"#,
        ))
        .mount(&server)
        .await;

    let provider = provider_with_style(&server, "responses");
    provider
        .call(simple_request())
        .await
        .expect_err("a field-bearing rejection must stay an error");
    assert_eq!(
        count_responses_requests(&server).await,
        1,
        "a deterministic rejection must not be retried"
    );
}
