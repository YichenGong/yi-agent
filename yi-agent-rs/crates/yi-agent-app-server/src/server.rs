//! app-server 主循环:读请求 → 分发 → 写响应/通知。
//!
//! 本模块实现协议主循环:`initialize` / `thread/start` / `config/read`,以及
//! `turn/start` / `turn/interrupt`。每个 thread 有一个独立的 driver task,
//! 串行消费 turn、驱动 `agent.run()` 的事件流,并经 `Translator` 写成协议通知。
//! 另有 `not_initialized` / `method_not_found` / 解析错误 / stdin EOF 优雅退出。

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use serde_json::json;
use tokio::sync::mpsc;

use yi_agent_runtime::config::RuntimeConfig;

use crate::protocol::{
    JSONRPC_VERSION, Notification, NotificationEnvelope, PROTOCOL_VERSION, RequestEnvelope,
    RequestId, ResponseEnvelope, RpcError,
};
use crate::session::{ThreadSession, TurnPrompt};
use crate::translate::Translator;
use crate::transport::{MessageReader, MessageWriter};

/// driver task → 主循环的完成事件。
enum TurnEvent {
    Finished { thread_id: String, turn_id: String },
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
    // channel 里携带 `Result`,区分「读到一行」「EOF(channel 关闭)」与
    // 「读/传输错误」。若不区分,超大帧或 broken pipe 会被误当成干净 EOF。
    let (req_tx, mut req_rx) = mpsc::channel::<anyhow::Result<String>>(64);
    tokio::spawn(async move {
        let mut reader = MessageReader::new(reader);
        loop {
            match reader.next_line().await {
                Ok(Some(line)) => {
                    if req_tx.send(Ok(line)).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break, // EOF → 丢弃 req_tx → 主循环优雅退出
                Err(e) => {
                    tracing::error!("app-server read error: {e}");
                    let _ = req_tx.send(Err(e)).await;
                    break;
                }
            }
        }
        // 丢弃 req_tx → 主循环的 req_rx.recv() 返回 None,触发优雅退出。
    });

    let writer = Arc::new(MessageWriter::new(writer));
    // driver task 会 clone 该 sender 上报 turn 完成事件;主循环持有它,
    // 保证 `turn_rx` 不会提前关闭。
    let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(64);

    let mut initialized = false;
    let mut threads: HashMap<String, ThreadSession> = HashMap::new();
    let mut next_thread: u64 = 1;
    // 主循环用该计数器分配 turn id(`turn-{n}`)。
    let mut next_turn: u64 = 1;

    loop {
        tokio::select! {
            line = req_rx.recv() => {
                let Some(item) = line else { break }; // EOF → graceful exit
                let line = match item {
                    Ok(l) => l,
                    Err(e) => {
                        // 尽力告知客户端我们为何退出,再把失败向上抛出,
                        // 避免传输错误被伪装成干净退出。
                        let _ = write_response(
                            &writer,
                            err_response(
                                RequestId::Num(0),
                                RpcError::invalid_request(format!("transport error: {e}")),
                            ),
                        )
                        .await;
                        return Err(e);
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                let req: RequestEnvelope = match serde_json::from_str(&line) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("app-server parse error: {e}");
                        // 畸形帧里没有可用的 id。JSON-RPC 2.0 要求此时响应
                        // 的 id 为 `null`,但 `RequestId` 目前没有 Null 变体,
                        // 故暂以 id 0 代替(见 docs/bug-list.md)。
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

                        let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
                        let (interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);

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

                        // 每个 thread 一个 driver task:独占 agent 与两个 receiver,
                        // 串行驱动 turn。
                        let driver_writer = Arc::clone(&writer);
                        let driver_turn_tx = turn_tx.clone();
                        let driver_thread_id = thread_id.clone();
                        tokio::spawn(run_thread_driver(
                            driver_thread_id,
                            agent,
                            prompt_rx,
                            interrupt_rx,
                            driver_writer,
                            driver_turn_tx,
                        ));

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
                    "turn/start" => {
                        // `id` 后续响应仍需使用,故传 clone。
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        let prompt = match extract_prompt(&req.params) {
                            Some(p) => p,
                            None => {
                                write_response(
                                    &writer,
                                    err_response(
                                        id,
                                        RpcError::invalid_params("missing or empty input text"),
                                    ),
                                )
                                .await?;
                                continue;
                            }
                        };

                        let turn_id = format!("turn-{next_turn}");
                        // 内层作用域:让 `&mut threads` 的借用先结束,后续错误
                        // 路径才能再次 `threads.get_mut`。
                        let prompt_tx = {
                            let Some(session) = threads.get_mut(&thread_id) else {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                                continue;
                            };
                            if session.active_turn_id.is_some() {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::turn_in_progress(&thread_id)),
                                )
                                .await?;
                                continue;
                            }
                            next_turn += 1;
                            session.active_turn_id = Some(turn_id.clone());
                            session.prompt_tx.clone()
                        };

                        // 顺序确定:先 turn/started 通知,再响应,最后投递 prompt。
                        write_notification(
                            &writer,
                            &Notification::TurnStarted {
                                thread_id: thread_id.clone(),
                                turn_id: turn_id.clone(),
                            },
                        )
                        .await?;
                        write_response(
                            &writer,
                            ok_response(id, json!({ "turn_id": turn_id.clone() })),
                        )
                        .await?;

                        if prompt_tx.send(TurnPrompt { turn_id, prompt }).await.is_err() {
                            // driver 已退出(理论上不会):清掉活跃标记,
                            // 避免后续 turn 永远报 turn_in_progress。
                            if let Some(s) = threads.get_mut(&thread_id) {
                                s.active_turn_id = None;
                            }
                        }
                    }
                    "turn/interrupt" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        let Some(session) = threads.get(&thread_id) else {
                            write_response(
                                &writer,
                                err_response(id, RpcError::unknown_thread(&thread_id)),
                            )
                            .await?;
                            continue;
                        };
                        if let Some(turn_id) = session.active_turn_id.clone() {
                            // 幂等 level 信号:用 try_send 避免主循环在满队列上阻塞。
                            let _ = session.interrupt_tx.try_send(turn_id);
                        }
                        write_response(&writer, ok_response(id, json!({}))).await?;
                    }
                    _ => {
                        write_response(&writer, err_response(id, RpcError::method_not_found(&method)))
                            .await?;
                    }
                }
            }
            ev = turn_rx.recv() => {
                if let Some(TurnEvent::Finished { thread_id, turn_id }) = ev {
                    if let Some(s) = threads.get_mut(&thread_id) {
                        // 仅当完成的正是当前活跃 turn 时才清除,避免迟到的
                        // 完成事件误清掉已开始的下一个 turn。
                        if s.active_turn_id.as_deref() == Some(turn_id.as_str()) {
                            s.active_turn_id = None;
                        }
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

/// 单个 thread 的 driver task:串行消费 turn,驱动 `agent.run()` 的 stream,
/// 经 `Translator` 写成协议通知。
///
/// **取消安全**:`Agent::run()` 每次都会重置 cancel token,因此必须在
/// `run().await` 返回**之后**再取 `cancel_token()`,否则 `turn/interrupt` 无效。
async fn run_thread_driver<W>(
    thread_id: String,
    mut agent: yi_agent_core::Agent,
    mut prompt_rx: mpsc::Receiver<TurnPrompt>,
    mut interrupt_rx: mpsc::Receiver<String>,
    writer: Arc<MessageWriter<W>>,
    turn_tx: mpsc::Sender<TurnEvent>,
) where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // 每个 thread 一个 translator:item 计数器跨 turn 单调递增,避免 id 重复。
    let mut translator = Translator::new(thread_id.clone());
    while let Some(TurnPrompt { turn_id, prompt }) = prompt_rx.recv().await {
        translator.set_turn(turn_id.clone());

        let mut stream = match agent.run(prompt).await {
            Ok(s) => s,
            Err(e) => {
                // run() 本身失败:翻译成 Error → turn/completed(failed)。
                for n in translator.on_event(yi_agent_core::AgentEvent::Error(e)) {
                    let _ = write_notification(&writer, &n).await;
                }
                let _ = turn_tx
                    .send(TurnEvent::Finished {
                        thread_id: thread_id.clone(),
                        turn_id: turn_id.clone(),
                    })
                    .await;
                continue;
            }
        };

        // 必须在 run() 之后捕获:run() 内部会重建 cancel token。
        let cancel_token = agent.cancel_token();
        let mut cancel_sent = false;

        loop {
            tokio::select! {
                ev = stream.next() => {
                    match ev {
                        Some(e) => {
                            for n in translator.on_event(e) {
                                if write_notification(&writer, &n).await.is_err() {
                                    // 客户端可能已断开;先上报 Finished,
                                    // 避免 active_turn_id 永久卡住。
                                    let _ = turn_tx
                                        .send(TurnEvent::Finished {
                                            thread_id: thread_id.clone(),
                                            turn_id: turn_id.clone(),
                                        })
                                        .await;
                                    return;
                                }
                            }
                        }
                        None => break,
                    }
                }
                Some(target) = interrupt_rx.recv(), if !cancel_sent => {
                    // 只接受针对当前 turn 的中断;忽略上一轮残留的信号。
                    if target == turn_id {
                        cancel_sent = true;
                        cancel_token.cancel();
                    }
                    // 继续消费 stream,直到 run loop 发 Cancelled 并结束。
                }
            }
        }

        let _ = turn_tx
            .send(TurnEvent::Finished {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
            })
            .await;
    }
}

/// 从 params 提取 `threadId`;缺失时写 `-32602` 并返回 `Ok(None)`。
async fn require_thread_id<W: tokio::io::AsyncWrite + Unpin>(
    writer: &MessageWriter<W>,
    params: &serde_json::Value,
    id: RequestId,
) -> anyhow::Result<Option<String>> {
    match params.get("threadId").and_then(|v| v.as_str()) {
        Some(s) => Ok(Some(s.to_string())),
        None => {
            write_response(
                writer,
                err_response(id, RpcError::invalid_params("missing threadId")),
            )
            .await?;
            Ok(None)
        }
    }
}

/// 从 `turn/start` 的 params 提取用户文本:`input:[{type:"text",text}]` 拼接。
fn extract_prompt(params: &serde_json::Value) -> Option<String> {
    let input = params.get("input")?.as_array()?;
    let mut text = String::new();
    for block in input {
        if block.get("type").and_then(|t| t.as_str()) != Some("text") {
            continue;
        }
        if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
            text.push_str(t);
        }
    }
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use async_trait::async_trait;
    use futures::StreamExt;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::*;

    /// 极简 provider:只回一段文本后结束。
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

    /// 永不自行结束的 provider:每 5ms 吐一个 delta,turn 会一直活跃,
    /// 直到被 `turn/interrupt` 取消。
    struct SlowProvider;

    #[async_trait]
    impl yi_agent_core::Provider for SlowProvider {
        async fn call_stream(
            &self,
            _req: yi_agent_core::provider::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
            yi_agent_core::provider::ProviderError,
        > {
            let events = futures::stream::unfold(0u64, |i| async move {
                tokio::time::sleep(Duration::from_millis(5)).await;
                Some((
                    yi_agent_core::provider::ProviderEvent::TextDelta("x".into()),
                    i + 1,
                ))
            });
            Ok(events.boxed())
        }
    }

    /// 首帧延迟 20ms 的 provider:保证 driver 的 interrupt 分支在 stream
    /// 首次 yield 之前已被轮询到(用于确定性地测试中断标记)。
    struct DelayedProvider;

    #[async_trait]
    impl yi_agent_core::Provider for DelayedProvider {
        async fn call_stream(
            &self,
            _req: yi_agent_core::provider::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
            yi_agent_core::provider::ProviderError,
        > {
            let events = futures::stream::once(async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                yi_agent_core::provider::ProviderEvent::TextDelta("hi".into())
            })
            .chain(futures::stream::once(async {
                yi_agent_core::provider::ProviderEvent::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                }
            }));
            Ok(events.boxed())
        }
    }

    fn build_test_agent() -> anyhow::Result<yi_agent_core::Agent> {
        Ok(yi_agent_core::Agent::new(
            Arc::new(MockProvider),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            yi_agent_core::AgentConfig::default(),
        ))
    }

    fn build_slow_agent() -> anyhow::Result<yi_agent_core::Agent> {
        Ok(yi_agent_core::Agent::new(
            Arc::new(SlowProvider),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            yi_agent_core::AgentConfig::default(),
        ))
    }

    fn build_delayed_agent() -> anyhow::Result<yi_agent_core::Agent> {
        Ok(yi_agent_core::Agent::new(
            Arc::new(DelayedProvider),
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
            Self::with_factory(build_test_agent)
        }

        /// 用自定义 agent 工厂搭建 harness(慢 provider / 中断测试需要)。
        fn with_factory<F>(build: F) -> Self
        where
            F: Fn() -> anyhow::Result<yi_agent_core::Agent> + Send + 'static,
        {
            let (client_w, server_r) = tokio::io::duplex(64 * 1024);
            let (server_w, client_r) = tokio::io::duplex(64 * 1024);
            let handle = tokio::spawn(run_with(server_r, server_w, test_config(), build));
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
            let res = tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .expect("server must shut down within the timeout")
                .expect("server task must not panic");
            assert!(res.is_ok(), "run_with should return Ok on EOF: {res:?}");
        }
    }

    async fn initialize(h: &mut Harness) {
        h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 1, "initialize must respond with the request id");
    }

    /// `initialize` + `thread/start`,返回新建 thread 的 id。
    async fn start_thread(h: &mut Harness) -> String {
        initialize(h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;
        // 依次读到 thread/started 通知与 thread/start 响应,取响应里的 thread_id。
        for _ in 0..4 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(2)) {
                return v["result"]["thread_id"].as_str().unwrap().to_string();
            }
        }
        panic!("no thread/start response");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn initialize_responds_with_server_info() {
        let mut h = Harness::new();
        h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 1);
        assert_eq!(v["jsonrpc"], "2.0");
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

    #[tokio::test(flavor = "multi_thread")]
    async fn malformed_json_returns_parse_error() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send("not json").await;
        let v = h.read_value().await;
        assert_eq!(v["error"]["code"], -32700, "expected parse error: {v}");
        assert_eq!(v["id"], 0, "malformed frame must be answered with id 0");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn blank_lines_are_ignored() {
        let mut h = Harness::new();
        // 先发一个空行,再发合法的 initialize;空行不应产生任何帧。
        h.send("").await;
        h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 1, "blank line must not produce a frame: {v}");
        assert_eq!(v["result"]["serverInfo"]["name"], "yi-agent-app-server");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_factory_failure_returns_internal_error() {
        let (mut client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let handle = tokio::spawn(run_with(server_r, server_w, test_config(), || {
            Err::<yi_agent_core::Agent, _>(anyhow::anyhow!("boom"))
        }));

        let mut client_r = BufReader::new(client_r);

        client_w
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .await
            .unwrap();
        client_w.flush().await.unwrap();
        let mut buf = String::new();
        client_r.read_line(&mut buf).await.unwrap();
        let init: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
        assert_eq!(init["id"], 1);

        client_w
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"thread/start\",\"params\":{}}\n",
            )
            .await
            .unwrap();
        client_w.flush().await.unwrap();
        let mut buf = String::new();
        client_r.read_line(&mut buf).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
        assert_eq!(v["id"], 2);
        assert_eq!(
            v["error"]["code"], -32603,
            "factory failure must map to internal error: {v}"
        );

        drop(client_w);
        let res = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("server must shut down within the timeout")
            .expect("server task must not panic");
        assert!(res.is_ok(), "run_with should return Ok on EOF: {res:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oversized_frame_returns_err() {
        let (mut client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, _client_r) = tokio::io::duplex(64 * 1024);
        let handle = tokio::spawn(run_with(
            server_r,
            server_w,
            test_config(),
            build_test_agent,
        ));

        let mut frame = vec![b'x'; crate::protocol::MAX_FRAME_BYTES + 16];
        frame.push(b'\n');
        // duplex 缓冲小于帧,server 读到超限即报错并退出读端;写侧随后可能
        // 因 broken pipe 失败,这里忽略写错误——真正断言的是 server 的结果。
        let _ = client_w.write_all(&frame).await;
        let _ = client_w.flush().await;

        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("server must exit within the timeout")
            .expect("server task must not panic");
        assert!(
            result.is_err(),
            "oversized frame must surface an error: {result:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_start_emits_full_notification_sequence() {
        let mut h = Harness::new();
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;

        let mut methods: Vec<String> = Vec::new();
        let mut resp_turn_id: Option<String> = None;
        let mut completed: Option<serde_json::Value> = None;
        for _ in 0..12 {
            let v = h.read_value().await;
            if let Some(m) = v.get("method").and_then(|m| m.as_str()) {
                methods.push(m.to_string());
                if m == "turn/completed" {
                    completed = Some(v);
                    break;
                }
            } else if v.get("id") == Some(&serde_json::json!(3)) {
                resp_turn_id = v["result"]["turn_id"].as_str().map(|s| s.to_string());
            }
        }

        assert_eq!(
            methods,
            vec![
                "turn/started",
                "item/started",
                "item/delta",
                // `Done` 会先 finalize 打开的 agentMessage,故 turn/completed
                // 之前必有一条 item/completed(见 Translator::finish_turn)。
                "item/completed",
                "turn/completed"
            ],
            "unexpected notification sequence"
        );
        let resp_turn_id = resp_turn_id.expect("turn/start response must carry turn_id");
        assert!(!resp_turn_id.is_empty(), "turn_id must be non-empty");
        let completed = completed.expect("expected a turn/completed notification");
        assert_eq!(completed["params"]["status"], "completed");
        assert_eq!(completed["params"]["turn_id"], resp_turn_id);
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_start_unknown_thread_returns_unknown_thread() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(
            r#"{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{"threadId":"nope","input":[{"type":"text","text":"hi"}]}}"#,
        )
        .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 3);
        assert_eq!(v["error"]["code"], -32011, "expected unknown thread: {v}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_start_missing_thread_id_returns_invalid_params() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(
            r#"{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{"input":[{"type":"text","text":"hi"}]}}"#,
        )
        .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 3);
        assert_eq!(v["error"]["code"], -32602, "expected invalid params: {v}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_start_empty_input_returns_invalid_params() {
        let mut h = Harness::new();
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[]}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 3);
        assert_eq!(v["error"]["code"], -32602, "expected invalid params: {v}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_start_while_turn_in_progress_returns_turn_in_progress() {
        let mut h = Harness::with_factory(build_slow_agent);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;

        // 读到 id==3 的响应(跳过 turn/started 通知与 item/* 帧)。
        loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(3)) {
                assert!(v["result"]["turn_id"].is_string(), "expected turn_id: {v}");
                break;
            }
        }

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"again"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(4)) {
                assert_eq!(v["error"]["code"], -32012, "expected turn in progress: {v}");
                break;
            }
        }
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_interrupt_cancels_active_turn() {
        let mut h = Harness::with_factory(build_slow_agent);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;

        // 等 turn/started,确认 turn 已活跃。
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/started") {
                break;
            }
        }

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"turn/interrupt","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;

        let mut completed: Option<serde_json::Value> = None;
        for _ in 0..40 {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
                completed = Some(v);
                break;
            }
        }
        let completed = completed.expect("expected a turn/completed notification");
        assert_eq!(completed["params"]["status"], "interrupted");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn driver_ignores_interrupt_tagged_with_other_turn() {
        let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
        let (interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
        let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let writer = Arc::new(MessageWriter::new(server_w));

        let handle = tokio::spawn(run_thread_driver(
            "thread-1".into(),
            build_delayed_agent().unwrap(),
            prompt_rx,
            interrupt_rx,
            writer,
            turn_tx,
        ));

        // 上一轮残留的中断(属于 turn-0)必须被忽略。
        interrupt_tx.try_send("turn-0".into()).unwrap();
        prompt_tx
            .send(TurnPrompt {
                turn_id: "turn-1".into(),
                prompt: "hi".into(),
            })
            .await
            .unwrap();

        let mut client_r = BufReader::new(client_r);
        let mut status = None;
        for _ in 0..10 {
            let mut buf = String::new();
            let n = tokio::time::timeout(Duration::from_secs(5), client_r.read_line(&mut buf))
                .await
                .expect("timed out waiting for a notification")
                .unwrap();
            if n == 0 {
                break;
            }
            let v: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
            if v["method"] == "turn/completed" {
                status = Some(v["params"]["status"].as_str().unwrap().to_string());
                break;
            }
        }
        assert_eq!(
            status.as_deref(),
            Some("completed"),
            "a stale interrupt must not cancel the new turn"
        );

        drop(prompt_tx);
        drop(interrupt_tx);
        let ev = tokio::time::timeout(Duration::from_secs(5), turn_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev, TurnEvent::Finished { .. }));
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn driver_reports_finished_when_writer_fails() {
        let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
        let (_interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
        let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        drop(client_r); // 断开读端 → 写通知失败
        let writer = Arc::new(MessageWriter::new(server_w));

        let handle = tokio::spawn(run_thread_driver(
            "thread-1".into(),
            build_test_agent().unwrap(),
            prompt_rx,
            interrupt_rx,
            writer,
            turn_tx,
        ));

        prompt_tx
            .send(TurnPrompt {
                turn_id: "turn-1".into(),
                prompt: "hi".into(),
            })
            .await
            .unwrap();

        let ev = tokio::time::timeout(Duration::from_secs(5), turn_rx.recv())
            .await
            .expect("driver must report Finished even when writes fail")
            .expect("turn channel must stay open");
        assert!(matches!(ev, TurnEvent::Finished { ref turn_id, .. } if turn_id == "turn-1"));

        drop(prompt_tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }
}
