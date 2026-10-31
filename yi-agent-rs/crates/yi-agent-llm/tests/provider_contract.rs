//! 跨 provider 的能力契约测试。
//!
//! 同一套**语义**用例，对 Anthropic 与 OpenAI 各跑一遍。报文形状由各 provider
//! 自己决定（真实 wire 格式），但断言的是能力结论：文本累积、工具调用、多工具、
//! 混排顺序、无参数工具、usage 合并、错误码分类。
//!
//! 存在理由：两条 provider 路径历史上出现过行为漂移——Anthropic 把
//! `stop_reason: tool_use` 映射成终止原因，导致工具**从不执行**，而 OpenAI 一直
//! 正确。单 provider 的单元测试无法发现这种漂移，只有同一契约下的横向对比可以。
//!
//! 全部默认运行，无需 API key。

use std::time::Duration;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use yi_agent_core::{
    ContentBlock, GenParams, Message, Provider, ProviderError, ProviderRequest, ProviderResponse,
    StopReason, ToolSchema,
};
use yi_agent_llm::{AnthropicProvider, AnthropicProviderOpts, OpenaiProvider, OpenaiProviderOpts};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Anthropic,
    Openai,
}

const KINDS: [Kind; 2] = [Kind::Anthropic, Kind::Openai];

fn provider(kind: Kind, base_url: &str) -> Box<dyn Provider> {
    match kind {
        Kind::Anthropic => Box::new(
            AnthropicProvider::new(AnthropicProviderOpts {
                base_url: Some(base_url.to_string()),
                api_key: Some("test-key".into()),
                timeout: Some(Duration::from_secs(5)),
                ..Default::default()
            })
            .expect("anthropic provider"),
        ),
        Kind::Openai => Box::new(
            OpenaiProvider::new(OpenaiProviderOpts {
                base_url: Some(format!("{base_url}/v1")),
                api_key: Some("test-key".into()),
                timeout: Some(Duration::from_secs(5)),
                // 显式 chat：避免 auto 探测多打一次 /responses。
                api_style: Some("chat".into()),
                ..Default::default()
            })
            .expect("openai provider"),
        ),
    }
}

fn request_path(kind: Kind) -> &'static str {
    match kind {
        Kind::Anthropic => "/v1/messages",
        Kind::Openai => "/v1/chat/completions",
    }
}

fn read_tool() -> ToolSchema {
    ToolSchema {
        name: "read".into(),
        description: "read a file".into(),
        input_schema: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
    }
}

fn request(tools: Vec<ToolSchema>) -> ProviderRequest {
    ProviderRequest {
        model: "test-model".into(),
        system: None,
        messages: vec![Message::user("go")],
        tools,
        params: GenParams::default(),
    }
}

/// 挂一个固定 SSE body 到该 provider 的真实路径。
async fn mount(server: &MockServer, kind: Kind, body: &str) {
    Mock::given(method("POST"))
        .and(path(request_path(kind)))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(server)
        .await;
}

/// 纯文本：累积出单个 Text 块，以 EndTurn 收尾。
#[tokio::test]
async fn contract_plain_text() {
    for kind in KINDS {
        let body = match kind {
            Kind::Anthropic => {
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
                 event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n\
                 event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n\
                 event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                 event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n\
                 event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
            }
            Kind::Openai => {
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n\
                 data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n\
                 data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
                 data: [DONE]\n\n"
            }
        };
        let server = MockServer::start().await;
        mount(&server, kind, body).await;
        let resp: ProviderResponse = provider(kind, &server.uri())
            .call(request(vec![]))
            .await
            .expect("call ok");

        assert_eq!(text_of_resp(&resp), "Hello world", "{kind:?}");
        assert_eq!(
            resp.content,
            vec![ContentBlock::Text("Hello world".into())],
            "{kind:?}: exactly one text block"
        );
        assert_eq!(resp.stop_reason, StopReason::EndTurn, "{kind:?}");
    }
}

fn text_of_resp(resp: &ProviderResponse) -> String {
    resp.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

/// 单工具调用：累积出 ToolUse 块，且以 EndTurn 收尾（不是终止）。
///
/// 这是缺陷 A 的契约护栏：Anthropic 曾把 tool_use 映射成 Other，agent 循环因此
/// 直接中断，工具从不执行。
#[tokio::test]
async fn contract_single_tool_call_ends_with_end_turn() {
    for kind in KINDS {
        let body = match kind {
            Kind::Anthropic => {
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read\",\"input\":{}}}\n\n\
                 event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n\
                 event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"a.rs\\\"}\"}}\n\n\
                 event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                 event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n\
                 event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
            }
            Kind::Openai => {
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]}}]}\n\n\
                 data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n\
                 data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"a.rs\\\"}\"}}]}}]}\n\n\
                 data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n\
                 data: [DONE]\n\n"
            }
        };
        let server = MockServer::start().await;
        mount(&server, kind, body).await;
        let resp = provider(kind, &server.uri())
            .call(request(vec![read_tool()]))
            .await
            .expect("call ok");

        assert_eq!(resp.stop_reason, StopReason::EndTurn, "{kind:?}");
        let tool = resp
            .content
            .iter()
            .find_map(|b| match b {
                ContentBlock::ToolUse { name, input, .. } => Some((name.clone(), input.clone())),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{kind:?}: a ToolUse block must be accumulated"));
        assert_eq!(tool.0, "read", "{kind:?}");
        assert_eq!(
            tool.1,
            serde_json::json!({"path": "a.rs"}),
            "{kind:?}: argument fragments must be joined"
        );
    }
}

/// 两个工具一次返回：两个 ToolUse 块，顺序与报文一致。
#[tokio::test]
async fn contract_multiple_tool_calls_keep_order() {
    for kind in KINDS {
        let body = match kind {
            Kind::Anthropic => {
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_a\",\"name\":\"read\",\"input\":{}}}\n\n\
                 event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"p\\\":1}\"}}\n\n\
                 event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                 event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_b\",\"name\":\"write\",\"input\":{}}}\n\n\
                 event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"p\\\":2}\"}}\n\n\
                 event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
                 event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n\
                 event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
            }
            Kind::Openai => {
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"p\\\":1}\"}}]}}]}\n\n\
                 data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_b\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"p\\\":2}\"}}]}}]}\n\n\
                 data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n\
                 data: [DONE]\n\n"
            }
        };
        let server = MockServer::start().await;
        mount(&server, kind, body).await;
        let resp = provider(kind, &server.uri())
            .call(request(vec![read_tool()]))
            .await
            .expect("call ok");

        let names: Vec<String> = resp
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            names,
            vec!["read".to_string(), "write".to_string()],
            "{kind:?}"
        );
    }
}

/// 文本 + 工具混排：content 顺序为 [Text, ToolUse]。
#[tokio::test]
async fn contract_text_before_tool_keeps_wire_order() {
    for kind in KINDS {
        let body = match kind {
            Kind::Anthropic => {
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
                 event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"reading\"}}\n\n\
                 event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                 event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read\",\"input\":{}}}\n\n\
                 event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n\
                 event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
                 event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n\
                 event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
            }
            Kind::Openai => {
                "data: {\"choices\":[{\"delta\":{\"content\":\"reading\"}}]}\n\n\
                 data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]}}]}\n\n\
                 data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n\
                 data: [DONE]\n\n"
            }
        };
        let server = MockServer::start().await;
        mount(&server, kind, body).await;
        let resp = provider(kind, &server.uri())
            .call(request(vec![read_tool()]))
            .await
            .expect("call ok");

        assert_eq!(
            resp.content.len(),
            2,
            "{kind:?}: text + tool, got {:?}",
            resp.content
        );
        assert!(matches!(resp.content[0], ContentBlock::Text(_)), "{kind:?}");
        assert!(
            matches!(resp.content[1], ContentBlock::ToolUse { .. }),
            "{kind:?}"
        );
    }
}

/// 无参数工具：解析为 `{}`，不报错。
///
/// 缺陷 C 的契约护栏：Anthropic 对无参数工具可以完全不发参数增量。
#[tokio::test]
async fn contract_zero_arg_tool_becomes_empty_object() {
    for kind in KINDS {
        let body = match kind {
            Kind::Anthropic => {
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_z\",\"name\":\"list_files\",\"input\":{}}}\n\n\
                 event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                 event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n\
                 event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
            }
            Kind::Openai => {
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_z\",\"function\":{\"name\":\"list_files\",\"arguments\":\"\"}}]}}]}\n\n\
                 data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n\
                 data: [DONE]\n\n"
            }
        };
        let server = MockServer::start().await;
        mount(&server, kind, body).await;
        let resp = provider(kind, &server.uri())
            .call(request(vec![read_tool()]))
            .await
            .unwrap_or_else(|e| panic!("{kind:?}: a zero-arg tool must not error, got {e}"));

        let input = resp
            .content
            .iter()
            .find_map(|b| match b {
                ContentBlock::ToolUse { name, input, .. } if name == "list_files" => {
                    Some(input.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("{kind:?}: list_files tool must be accumulated"));
        assert_eq!(input, serde_json::json!({}), "{kind:?}");
    }
}

/// usage 分片合并：最终 input 与 output 都保留，互不覆盖。
///
/// 缺陷 B 的契约护栏：Anthropic 的 message_delta 只带 output_tokens，整体覆盖会
/// 把 input_tokens 抹成 0，自动压缩因此永不触发。OpenAI 只在流尾发一次完整
/// usage，本用例对它同样成立（合并是幂等的）。
#[tokio::test]
async fn contract_usage_keeps_input_and_output() {
    for kind in KINDS {
        let body = match kind {
            Kind::Anthropic => {
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100000,\"cache_read_input_tokens\":90000}}}\n\n\
                 event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
                 event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n\
                 event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                 event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":42}}\n\n\
                 event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
            }
            Kind::Openai => {
                "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                 data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
                 data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100000,\"completion_tokens\":42,\"prompt_tokens_details\":{\"cached_tokens\":90000}}}\n\n\
                 data: [DONE]\n\n"
            }
        };
        // 必须走 `accumulate_stream` —— 合并逻辑住在 core，测试侧自己合并会
        // 绕过被测代码，让「整体覆盖」的回归漏网。
        let server = MockServer::start().await;
        mount(&server, kind, body).await;
        let stream = provider(kind, &server.uri())
            .call_stream(request(vec![]))
            .await
            .expect("stream ok");
        let (_content, _end, usage) =
            yi_agent_core::provider::accumulate_stream(stream, |_| {}, None)
                .await
                .expect("accumulate ok");
        let usage = usage.unwrap_or_else(|| panic!("{kind:?}: usage must be reported"));
        assert_eq!(usage.input_tokens, 100000, "{kind:?}: input must survive");
        assert_eq!(usage.output_tokens, 42, "{kind:?}: output must survive");
    }
}

/// 错误码分类：401 → Auth，429 → RateLimited，500 → Server。
#[tokio::test]
async fn contract_error_status_mapping() {
    let cases = [(401u16, "auth"), (429, "rate"), (500, "server")];
    for kind in KINDS {
        for (status, label) in cases {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path(request_path(kind)))
                .respond_with(ResponseTemplate::new(status).set_body_string("nope"))
                .mount(&server)
                .await;
            let err = provider(kind, &server.uri())
                .call(request(vec![]))
                .await
                .expect_err("must fail");
            let ok = match (label, &err) {
                ("auth", ProviderError::Auth(_)) => true,
                ("rate", ProviderError::RateLimited) => true,
                ("server", ProviderError::Server(_)) => true,
                _ => false,
            };
            assert!(ok, "{kind:?} {status}: unexpected {err:?}");
        }
    }
}
