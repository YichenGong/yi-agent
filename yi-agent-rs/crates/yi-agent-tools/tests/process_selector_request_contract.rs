//! 传输边界护栏：真实的 `process_read` / `process_kill` 工具 schema 穿过三条
//! provider 路径后，实际发出的 HTTP 请求里两个选择器**都不得**变成必填。
//!
//! 这里断言的是**请求体**，不是转换 helper，也不是自造的 fixture：schema 由真实
//! `ToolRegistry` 从真实工具取得，provider 由真实构造器建立。mock 只负责回一条
//! 最简 SSE 让 `Provider::call` 正常收尾，成功本身不是断言目标。

use std::time::Duration;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use yi_agent_core::{GenParams, Message, Provider, ProviderRequest, ToolRegistry};
use yi_agent_llm::{AnthropicProvider, AnthropicProviderOpts, OpenaiProvider, OpenaiProviderOpts};
use yi_agent_tools::{ProcessKillTool, ProcessManager, ProcessReadTool};

#[tokio::test]
async fn process_selector_request_contract() {
    for style in ["anthropic", "chat", "responses"] {
        let server = MockServer::start().await;
        let temp = tempfile::TempDir::new().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());
        let mut registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(ProcessReadTool::new(manager.clone())));
        registry.register(std::sync::Arc::new(ProcessKillTool::new(manager)));

        let (request_path, sse) = match style {
            "anthropic" => (
                "/v1/messages",
                concat!(
                    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
                    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
                    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                ),
            ),
            "chat" => (
                "/v1/chat/completions",
                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            ),
            "responses" => (
                "/v1/responses",
                "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n",
            ),
            _ => unreachable!(),
        };

        Mock::given(method("POST"))
            .and(path(request_path))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .expect(1)
            .mount(&server)
            .await;

        let provider: Box<dyn Provider> = if style == "anthropic" {
            Box::new(
                AnthropicProvider::new(AnthropicProviderOpts {
                    base_url: Some(server.uri()),
                    api_key: Some("test-key".into()),
                    timeout: Some(Duration::from_secs(5)),
                    ..Default::default()
                })
                .unwrap(),
            )
        } else {
            Box::new(
                OpenaiProvider::new(OpenaiProviderOpts {
                    base_url: Some(format!("{}/v1", server.uri())),
                    api_key: Some("test-key".into()),
                    timeout: Some(Duration::from_secs(5)),
                    api_style: Some(style.into()),
                    ..Default::default()
                })
                .unwrap(),
            )
        };

        provider
            .call(ProviderRequest {
                model: "test-model".into(),
                system: None,
                messages: vec![Message::user("check tool schemas")],
                tools: registry.schemas(),
                params: GenParams::default(),
            })
            .await
            .expect("mock provider call");

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1, "{style}: unexpected probe/retry");

        let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        let tools = body["tools"].as_array().expect("transmitted tools");
        assert_eq!(tools.len(), 2, "{style}");

        for name in ["process_read", "process_kill"] {
            // chat 把 schema 包在 `function` 下；anthropic / responses 是平铺。
            let tool = tools
                .iter()
                .find(|tool| {
                    let definition = if style == "chat" {
                        &tool["function"]
                    } else {
                        tool
                    };
                    definition["name"] == name
                })
                .expect("both tools transmitted");
            let definition = if style == "chat" {
                &tool["function"]
            } else {
                tool
            };
            let schema = if style == "anthropic" {
                &definition["input_schema"]
            } else {
                &definition["parameters"]
            };

            assert_eq!(schema["type"], "object", "{style}/{name}");
            assert_eq!(schema["required"], serde_json::json!([]), "{style}/{name}");
            assert_ne!(definition["strict"], true, "strict unexpectedly enabled");

            let description = definition["description"].as_str().unwrap();
            assert!(description.contains("exactly one"), "{style}/{name}");
            assert!(description.contains("Omit"), "{style}/{name}");
            assert!(description.contains("empty string"), "{style}/{name}");

            for (field, other) in [("process_id", "name"), ("name", "process_id")] {
                assert_eq!(schema["properties"][field]["type"], "string");
                let description = schema["properties"][field]["description"].as_str().unwrap();
                assert!(description.contains("Optional"), "{style}/{name}.{field}");
                assert!(description.contains("omit"), "{style}/{name}.{field}");
                assert!(description.contains(other), "{style}/{name}.{field}");
            }
            for key in ["oneOf", "anyOf", "allOf", "not", "enum", "const"] {
                assert!(schema.get(key).is_none(), "{style}/{name}: {key}");
            }
        }
    }
}
