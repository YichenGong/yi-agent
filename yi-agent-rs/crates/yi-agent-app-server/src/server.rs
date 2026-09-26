//! app-server 主循环:读请求 → 分发 → 写响应/通知。
//!
//! 本模块实现 C4a 范围内的协议骨架:`initialize` / `thread/start` /
//! `config/read`,以及 `not_initialized` / `method_not_found` / 解析错误 /
//! stdin EOF 优雅退出。`turn/start` / `turn/interrupt` 及其 driver task 属于
//! C4b,此处先返回 `method_not_found` 占位。

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::json;
use tokio::sync::mpsc;

use yi_agent_runtime::config::RuntimeConfig;

use crate::protocol::{
    JSONRPC_VERSION, Notification, NotificationEnvelope, PROTOCOL_VERSION, RequestEnvelope,
    RequestId, ResponseEnvelope, RpcError,
};
use crate::session::{ThreadSession, TurnPrompt};
use crate::transport::{MessageReader, MessageWriter};

/// driver task → 主循环的完成事件(C4b 使用)。
// TODO(C4b): 由 driver task 构造;C4a 仅保留类型与 select 分支骨架。
#[allow(dead_code)]
enum TurnEvent {
    Finished { thread_id: String },
}

/// app-server 入口:在 stdio(或任意读写流)上跑 JSON-RPC 主循环。
///
/// `cfg` 同时用于 `config/read` 响应与(每个 thread 的)`bootstrap_agent`。
pub async fn run<R, W>(reader: R, writer: W, cfg: RuntimeConfig) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let cfg_for_factory = cfg.clone();
    run_with(reader, writer, cfg, move || {
        yi_agent_runtime::bootstrap::bootstrap_agent(
            &cfg_for_factory,
            yi_agent_runtime::bootstrap::PermissionMode::Interactive,
        )
        .map(|b| b.agent)
    })
    .await
}

/// 主循环的可测核心:agent 工厂由调用方注入(测试用 mock provider)。
///
/// **取消安全**:`MessageReader::next_line` 基于 `read_line`,不是 cancel-safe,
/// 因此这里不直接在 `select!` 上轮询它;而是 spawn 一个独占 reader 的读取任务,
/// 把完整行转发到 channel,主循环只 select 两个 `recv`(均 cancel-safe)。
async fn run_with<R, W, F>(
    reader: R,
    writer: W,
    cfg: RuntimeConfig,
    build_agent: F,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    F: Fn() -> anyhow::Result<yi_agent_core::Agent> + Send + 'static,
{
    let (req_tx, mut req_rx) = mpsc::channel::<String>(64);
    tokio::spawn(async move {
        let mut reader = MessageReader::new(reader);
        loop {
            match reader.next_line().await {
                Ok(Some(line)) => {
                    if req_tx.send(line).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break, // EOF
                Err(e) => {
                    tracing::error!("app-server read error: {e}");
                    break;
                }
            }
        }
        // 丢弃 req_tx → 主循环的 req_rx.recv() 返回 None,触发优雅退出。
    });

    let writer = Arc::new(MessageWriter::new(writer));
    // C4b 的 driver task 会 clone 该 sender 上报 turn 完成事件;此处先持有,
    // 保证 `turn_rx` 不会提前关闭。
    let (_turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(64);

    let mut initialized = false;
    let mut threads: HashMap<String, ThreadSession> = HashMap::new();
    let mut next_thread: u64 = 1;
    // TODO(C4b): driver task 用该计数器分配 turn id。
    let next_turn: u64 = 1;
    let _ = next_turn;

    loop {
        tokio::select! {
            line = req_rx.recv() => {
                let Some(line) = line else { break }; // EOF → graceful exit
                if line.trim().is_empty() {
                    continue;
                }
                let req: RequestEnvelope = match serde_json::from_str(&line) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("app-server parse error: {e}");
                        // 畸形帧里没有可用的 id,按 JSON-RPC 惯例用 id 0 回错误。
                        write_response(
                            &writer,
                            err_response(RequestId::Num(0), RpcError::parse_error(e.to_string())),
                        )
                        .await?;
                        continue;
                    }
                };
                let id = req.id.clone();
                let method = req.method.clone();

                // 未 initialize 前,除 `initialize` 外的请求一律拒绝。
                if !initialized && method != "initialize" {
                    write_response(&writer, err_response(id, RpcError::not_initialized())).await?;
                    continue;
                }

                match method.as_str() {
                    "initialize" => {
                        initialized = true;
                        write_response(
                            &writer,
                            ok_response(
                                id,
                                json!({
                                    "serverInfo": {
                                        "name": "yi-agent-app-server",
                                        "version": env!("CARGO_PKG_VERSION"),
                                    },
                                    "protocolVersion": PROTOCOL_VERSION,
                                    "capabilities": {},
                                }),
                            ),
                        )
                        .await?;
                    }
                    "config/read" => {
                        write_response(&writer, ok_response(id, cfg.redacted_view())).await?;
                    }
                    "thread/start" => {
                        let thread_id = format!("thread-{next_thread}");
                        next_thread += 1;

                        let agent = match build_agent() {
                            Ok(a) => a,
                            Err(e) => {
                                write_response(&writer, err_response(id, RpcError::internal(e.to_string()))).await?;
                                continue;
                            }
                        };

                        // TODO(C4b): spawn driver task owning agent + prompt_rx + interrupt_rx.
                        // C4a 里 agent 与两个 receiver 暂未使用。
                        let _agent = agent;
                        let (prompt_tx, _prompt_rx) = mpsc::channel::<TurnPrompt>(8);
                        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(8);

                        let cwd = cfg.workdir.display().to_string();
                        let model = cfg.model.clone();
                        threads.insert(
                            thread_id.clone(),
                            ThreadSession {
                                thread_id: thread_id.clone(),
                                cwd: cwd.clone(),
                                model: model.clone(),
                                active_turn_id: None,
                                prompt_tx,
                                interrupt_tx,
                            },
                        );

                        write_notification(
                            &writer,
                            &Notification::ThreadStarted {
                                thread_id: thread_id.clone(),
                                cwd: cwd.clone(),
                                model: model.clone(),
                            },
                        )
                        .await?;
                        write_response(
                            &writer,
                            ok_response(
                                id,
                                json!({
                                    "thread_id": thread_id,
                                    "cwd": cwd,
                                    "model": model,
                                }),
                            ),
                        )
                        .await?;
                    }
                    // TODO(C4b): implement turn driver (turn/start + turn/interrupt).
                    "turn/start" | "turn/interrupt" => {
                        write_response(&writer, err_response(id, RpcError::method_not_found(&method)))
                            .await?;
                    }
                    _ => {
                        write_response(&writer, err_response(id, RpcError::method_not_found(&method)))
                            .await?;
                    }
                }
            }
            ev = turn_rx.recv() => {
                if let Some(TurnEvent::Finished { thread_id }) = ev {
                    if let Some(s) = threads.get_mut(&thread_id) {
                        s.active_turn_id = None;
                    }
                }
            }
        }
    }

    Ok(())
}

fn ok_response(id: RequestId, result: serde_json::Value) -> ResponseEnvelope {
    ResponseEnvelope {
        jsonrpc: Some(JSONRPC_VERSION.to_string()),
        id,
        result: Some(result),
        error: None,
    }
}

fn err_response(id: RequestId, error: RpcError) -> ResponseEnvelope {
    ResponseEnvelope {
        jsonrpc: Some(JSONRPC_VERSION.to_string()),
        id,
        result: None,
        error: Some(error),
    }
}

async fn write_response<W: tokio::io::AsyncWrite + Unpin>(
    writer: &MessageWriter<W>,
    resp: ResponseEnvelope,
) -> anyhow::Result<()> {
    writer.write_value(&resp).await
}

async fn write_notification<W: tokio::io::AsyncWrite + Unpin>(
    writer: &MessageWriter<W>,
    n: &Notification,
) -> anyhow::Result<()> {
    writer.write_value(&NotificationEnvelope::new(n)).await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use async_trait::async_trait;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::*;

    /// 极简 provider:只回一段文本后结束。C4a 不跑 turn,但 `thread/start`
    /// 需要装配出一个真实 `Agent`,故这里提供一个可用的 provider。
    struct MockProvider;

    #[async_trait]
    impl yi_agent_core::Provider for MockProvider {
        async fn call_stream(
            &self,
            _req: yi_agent_core::provider::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
            yi_agent_core::provider::ProviderError,
        > {
            let events = vec![
                yi_agent_core::provider::ProviderEvent::TextDelta("hi".into()),
                yi_agent_core::provider::ProviderEvent::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                },
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    fn build_test_agent() -> anyhow::Result<yi_agent_core::Agent> {
        Ok(yi_agent_core::Agent::new(
            Arc::new(MockProvider),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            yi_agent_core::AgentConfig::default(),
        ))
    }

    fn test_config() -> RuntimeConfig {
        RuntimeConfig {
            provider: "anthropic".to_string(),
            api_url: "https://api.anthropic.com".to_string(),
            api_key: String::new(),
            model: "test-model".to_string(),
            max_turns: 20,
            workdir: std::path::PathBuf::from("/tmp/yi-agent-app-server-test"),
            system_prompt: None,
            compact_threshold: 160_000,
            compact_user_budget_tokens: 20_000,
            compact_tool_budget_tokens: 12_000,
            yolo: false,
            sandbox: yi_agent_tools::SandboxMode::default(),
            sandbox_writable_roots: Vec::new(),
            skills_catalog_budget: 8192,
            skills_catalog_budget_explicit: false,
        }
    }

    /// 用两条 `duplex` 管道把 server 与测试客户端对接。
    struct Harness {
        client_w: tokio::io::DuplexStream,
        client_r: BufReader<tokio::io::DuplexStream>,
        handle: tokio::task::JoinHandle<anyhow::Result<()>>,
    }

    impl Harness {
        fn new() -> Self {
            let (client_w, server_r) = tokio::io::duplex(64 * 1024);
            let (server_w, client_r) = tokio::io::duplex(64 * 1024);
            let handle = tokio::spawn(run_with(
                server_r,
                server_w,
                test_config(),
                build_test_agent,
            ));
            Self {
                client_w,
                client_r: BufReader::new(client_r),
                handle,
            }
        }

        async fn send(&mut self, line: &str) {
            self.client_w.write_all(line.as_bytes()).await.unwrap();
            self.client_w.write_all(b"\n").await.unwrap();
            self.client_w.flush().await.unwrap();
        }

        async fn read_value(&mut self) -> serde_json::Value {
            let mut buf = String::new();
            let n = tokio::time::timeout(Duration::from_secs(5), self.client_r.read_line(&mut buf))
                .await
                .expect("timed out waiting for a message")
                .expect("read_line failed");
            assert!(n > 0, "unexpected EOF while waiting for a message");
            serde_json::from_str(buf.trim()).expect("server wrote invalid JSON")
        }

        /// 关掉客户端写端(触发 EOF)并等待 server 任务结束。
        async fn shutdown(self) {
            let Harness {
                client_w, handle, ..
            } = self;
            drop(client_w);
            let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        }
    }

    async fn initialize(h: &mut Harness) {
        h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 1, "initialize must respond with the request id");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn initialize_responds_with_server_info() {
        let mut h = Harness::new();
        h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["serverInfo"]["name"], "yi-agent-app-server");
        assert_eq!(v["result"]["protocolVersion"], 1);
        assert!(
            v.get("error").is_none(),
            "success response must omit `error`: {v}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn request_before_initialize_returns_not_initialized() {
        let mut h = Harness::new();
        h.send(r#"{"jsonrpc":"2.0","id":1,"method":"config/read","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 1);
        assert_eq!(v["error"]["code"], -32010);
        assert!(
            v.get("result").is_none(),
            "error response must omit `result`: {v}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_method_returns_method_not_found() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"bogus","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 2);
        assert_eq!(v["error"]["code"], -32601);
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_start_emits_thread_started_and_responds() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;

        let mut notif = None;
        let mut resp = None;
        for _ in 0..4 {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("thread/started") {
                notif = Some(v);
            } else if v.get("id") == Some(&serde_json::json!(2)) {
                resp = Some(v);
            }
            if notif.is_some() && resp.is_some() {
                break;
            }
        }

        let notif = notif.expect("expected a thread/started notification");
        let resp = resp.expect("expected a thread/start response");
        let notif_thread_id = notif["params"]["thread_id"]
            .as_str()
            .expect("notification thread_id must be a string");
        assert!(!notif_thread_id.is_empty(), "thread_id must be non-empty");
        assert_eq!(notif["params"]["model"], "test-model");
        assert_eq!(resp["result"]["thread_id"], notif_thread_id);
        assert_eq!(resp["result"]["model"], "test-model");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn config_read_returns_redacted_view() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"config/read","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 2);
        assert_eq!(v["result"]["api_key"], "");
        assert_eq!(v["result"]["model"], "test-model");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn eof_exits_gracefully() {
        let (client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, _client_r) = tokio::io::duplex(64 * 1024);
        let handle = tokio::spawn(run_with(
            server_r,
            server_w,
            test_config(),
            build_test_agent,
        ));

        // 什么都不发,直接关掉写端 → server 应看到 EOF 并 Ok(()) 退出。
        drop(client_w);
        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("server must exit within the timeout")
            .expect("server task must not panic");
        assert!(
            result.is_ok(),
            "run_with should return Ok on EOF: {result:?}"
        );
    }
}
