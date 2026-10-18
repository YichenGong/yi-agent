use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message as ClientMessage;
use yi_agent_runtime::config::RuntimeConfig;

/// 测试夹具的占位模型名。`test_config()` 用它作 `cfg.model`,`mod tests` 的
/// mock 工厂也用它作 `agent.config().model`,二者必须同值——`thread/start`/
/// `thread/resume` 以上报 agent 实际模型为准(任务 7 评审 Finding 1),夹具若
/// 不一致会凭空造出「上报 != 运行」的假象。
pub(crate) const TEST_MODEL: &str = "test-model";

/// 一份指向不存在文件的机器清单路径,供直接自建 runtime 的测试使用。
///
/// 这些测试不经过 `Harness`(后者的 `models_path` 已隔离),所以委派链路若从
/// 真实 `$HOME` 解析清单,就会依赖宿主机配了哪些模型——一旦存在
/// `subagent_model`/`default_model` 条目,解析出的 provider 会被悄悄改写、
/// 偏离测试的 `cfg`。传一个空清单让决策留在测试内。
pub(crate) fn isolated_models_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("models.json")
}

/// 等待一个**必然到来**的正面事件（一帧 JSONL、一条 turn 事件、server 任务结束）
/// 时使用的上限。
///
/// 这些等待的语义是「最终一定会来」；`5s` 只够空载机器，满载（CI 上并跑
/// 数十个测试）时正确实现也会错过它，于是把宿主机负载误报成产品 bug。放宽上限
/// 只是把「机器忙」从失败里排除掉，断言强度一字未动。
///
/// 不要拿它当「断言某事**不该**发生」的观察窗口——那种负等待要的是**短**窗口
/// （`Harness::try_read_value` 等）。
pub(crate) const EVENT_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// 对一条已连接的 ws 发 `initialize` 并读回响应。
///
/// 跨传输复用(`ws.rs` 的多客户端 E2E 与若干准入测试都要先握手),故放在
/// `test_support` 里;`#[cfg(test)]` 的 `mod tests` 对其它文件不可见。
pub(crate) async fn initialize<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    ws.send(ClientMessage::Text(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#.into(),
    ))
    .await
    .unwrap();
    let msg = ws.next().await.unwrap().unwrap();
    let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(v["id"], 1, "initialize must respond with the request id");
}

/// 经 stdio 的 `Harness` 发一条请求，读回**请求 id 相符**的那条响应。
///
/// 与 [`initialize`] 的分工：那条走 websocket 连接，这条走 `Harness` 的 JSONL
/// 双工流。两者都要跨文件复用（`server.rs` 的 `board_rpc_tests` 与
/// `resident_rpc_tests` 都要先发握手再发被测方法），故同住 `test_support`；
/// 名字带上 `over_stdio` 就是为了不与那条 ws 版混淆。
///
/// 读回时按 id 匹配并丢弃其它帧：server 会顺带推送通知（`broadcast` 的
/// 事件帧），不匹配的话断言会拿到通知而不是应答。
pub(crate) async fn rpc_over_stdio(
    h: &mut crate::server::tests::Harness,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    h.send(
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
        .to_string(),
    )
    .await;
    loop {
        let value = h.read_value().await;
        if value.get("id") == Some(&serde_json::json!(id)) {
            return value;
        }
    }
}

/// 一份无凭据、无网络的测试配置；provider 只是占位字符串。
pub(crate) fn test_config() -> RuntimeConfig {
    RuntimeConfig {
        provider: "anthropic".to_string(),
        api_url: "https://api.anthropic.com".to_string(),
        api_key: String::new(),
        model: TEST_MODEL.to_string(),
        api_style: yi_agent_core::ApiStyle::Auto,
        reasoning_effort: None,
        max_turns: 20,
        max_resident_subagents: yi_agent_runtime::config::RESIDENT_SUBAGENTS_DEFAULT,
        max_direct_children: yi_agent_runtime::config::DIRECT_CHILDREN_DEFAULT,
        workdir: std::path::PathBuf::from("/tmp/yi-agent-app-server-test"),
        system_prompt: None,
        compact_threshold: 160_000,
        compact_user_budget_tokens: 20_000,
        compact_tool_budget_tokens: 12_000,
        yolo: false,
        sandbox_promotable: true,
        sandbox: yi_agent_tools::SandboxMode::default(),
        sandbox_writable_roots: Vec::new(),
        skills_catalog_budget: 8192,
        skills_catalog_budget_explicit: false,
    }
}
