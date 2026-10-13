//! Streamable HTTP transport 端到端测试:用 wiremock 充当远端 MCP server,
//! 走真实 HTTP 往返完成 initialize / notifications/initialized / tools/list /
//! tools/call。

use std::collections::BTreeMap;

use serde_json::{Value, json};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use yi_agent_mcp::{McpConfig, McpManager, ServerConfig, ServerKind};

/// 一个最小的 Streamable HTTP MCP server:按 JSON-RPC method 分派响应。
struct McpHttpMock;

impl Respond for McpHttpMock {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = match request.body_json() {
            Ok(v) => v,
            Err(_) => return ResponseTemplate::new(400),
        };
        let method = body.get("method").and_then(Value::as_str).unwrap_or("");
        let id = body.get("id").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "mock", "version": "0.1" }
                    }
                })),
            // Notification:无 id,返回 202 Accepted。
            "notifications/initialized" => ResponseTemplate::new(202),
            "tools/list" => ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": [{
                            "name": "SearchPro",
                            "description": "web search",
                            "inputSchema": {
                                "type": "object",
                                "properties": { "Query": { "type": "string" } },
                                "required": ["Query"]
                            }
                        }]
                    }
                })),
            "tools/call" => ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": "mock-search-result" }],
                        "isError": false
                    }
                })),
            other => ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("method not found: {other}") }
                })),
        }
    }
}

fn http_server(url: &str) -> ServerConfig {
    ServerConfig {
        kind: ServerKind::Http,
        command: None,
        args: vec![],
        env: BTreeMap::new(),
        url: Some(url.to_string()),
        headers: BTreeMap::new(),
        enabled: true,
    }
}

#[tokio::test]
async fn probes_and_calls_over_streamable_http() {
    let server = MockServer::start().await;

    // 任何 POST 由 mock 按 method 分派;GET 不支持(405),与真实端点一致。
    Mock::given(wiremock::matchers::method("POST"))
        .respond_with(McpHttpMock)
        .mount(&server)
        .await;
    Mock::given(wiremock::matchers::method("GET"))
        .respond_with(ResponseTemplate::new(405))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let sc = http_server(&server.uri());

    let mut servers = BTreeMap::new();
    servers.insert("websearch".to_string(), sc.clone());
    let cfg = McpConfig {
        enabled: true,
        mcp_servers: servers,
    };

    let mgr = McpManager::load_and_probe(cfg, dir.path()).await.unwrap();

    // probe 通过 HTTP 发现了 SearchPro,并注册为限定名。
    let names: Vec<String> = mgr
        .enabled_tools()
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    assert!(
        names.contains(&"mcp__websearch__SearchPro".to_string()),
        "expected SearchPro to be discovered via http; got {names:?}"
    );

    // 调用走懒连接 + HTTP 往返。
    let res = mgr
        .call_tool("websearch", "SearchPro", json!({ "Query": "hello" }))
        .await;
    assert!(!res.is_error, "call should succeed: {res:?}");
    let text = match &res.content[0] {
        yi_agent_core::message::ContentBlock::Text(t) => t.clone(),
        other => panic!("expected text block, got {other:?}"),
    };
    assert_eq!(text, "mock-search-result");

    mgr.shutdown().await;
}

/// 真实 horizon-websearch 端点冒烟。默认跳过(CI 不跑);手动运行:
/// `cargo test -p yi-agent-mcp --test http_transport -- --ignored`
///
/// 无外网时该测试会失败,故用 `#[ignore]` gate 隔离。
#[tokio::test]
#[ignore]
async fn real_horizon_websearch_smoke() {
    const URL: &str = "https://llmapi.horizon.auto/mcp-servers/horizon-websearch/Mcp";

    let dir = tempfile::tempdir().unwrap();
    let mut servers = BTreeMap::new();
    servers.insert("horizon-websearch".to_string(), http_server(URL));
    let cfg = McpConfig {
        enabled: true,
        mcp_servers: servers,
    };

    let mgr = McpManager::load_and_probe(cfg, dir.path()).await.unwrap();
    let names: Vec<String> = mgr
        .enabled_tools()
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    assert!(
        names.contains(&"mcp__horizon-websearch__SearchPro".to_string()),
        "expected real endpoint to expose SearchPro; got {names:?}"
    );

    let res = mgr
        .call_tool(
            "horizon-websearch",
            "SearchPro",
            json!({ "Query": "今天北京天气", "Cnt": "10" }),
        )
        .await;
    assert!(!res.is_error, "real SearchPro call should succeed: {res:?}");

    mgr.shutdown().await;
}
