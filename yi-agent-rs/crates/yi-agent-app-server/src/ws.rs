//! WebSocket 传输(Tier 0,单连接)。

use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;

use crate::broadcast::{Broadcaster, ClientId};
use crate::protocol::MAX_FRAME_BYTES;
use crate::server::{PERMISSION_TIMEOUT, RuntimeAttachments, production_factory, serve};
use crate::workspace_index::WorkspaceIndex;
use yi_agent_runtime::config::RuntimeConfig;

/// 用一个已绑定的 listener 跑 ws 传输,直到进程退出。
///
/// 单个 ws server 共享**一套**主循环(thread 会话、审批表都在其中),因此所有
/// 连接都往同一个入站 channel 灌帧,并共享同一个 `Broadcaster`。
pub async fn serve_ws(
    listener: tokio::net::TcpListener,
    cfg: RuntimeConfig,
    workspaces: Arc<WorkspaceIndex>,
) -> anyhow::Result<()> {
    let hub = Arc::new(Broadcaster::new());
    let (inbound_tx, inbound_rx) = mpsc::channel::<(ClientId, anyhow::Result<String>)>(64);
    let serve_hub = Arc::clone(&hub);
    // 先克隆出主循环要用的 config,再把 cfg 丢给下面的 router 闭包。
    let serve_cfg = cfg.clone();
    let serve_task = tokio::spawn(async move {
        use std::collections::HashMap;
        use std::sync::Mutex as StdMutex;
        serve(
            inbound_rx,
            serve_hub,
            serve_cfg.clone(),
            PERMISSION_TIMEOUT,
            workspaces,
            RuntimeAttachments {
                runtimes: Arc::new(StdMutex::new(HashMap::new())),
                thread_roots: Arc::new(StdMutex::new(HashMap::new())),
                board_dir: yi_agent_boards::global_dir().unwrap_or_default(),
                launcher: Arc::new(yi_agent_boards::lifecycle::launch_if_absent),
            },
            production_factory(serve_cfg),
        )
        .await
    });

    let app = Router::new().route(
        "/ws",
        get(move |upgrade: WebSocketUpgrade| {
            let hub = Arc::clone(&hub);
            let tx = inbound_tx.clone();
            async move {
                // Tier 0:只接受一个客户端。
                if hub.client_count() > 0 {
                    return upgrade.on_upgrade(|mut socket: WebSocket| async move {
                        let _ = socket.send(Message::Close(None)).await;
                    });
                }
                let id = ClientId::ws(uuid::Uuid::new_v4());
                upgrade.on_upgrade(move |socket| handle_ws(socket, hub, tx, id))
            }
        }),
    );

    tracing::info!(addr = %listener.local_addr()?, "app-server ws listening");
    axum::serve(listener, app).await?;
    serve_task.abort();
    Ok(())
}

/// 一个 WS 连接的生命周期:注册客户端 → 出口泵转发帧 → 入站帧喂主循环。
async fn handle_ws(
    socket: WebSocket,
    hub: Arc<Broadcaster>,
    inbound_tx: mpsc::Sender<(ClientId, anyhow::Result<String>)>,
    id: ClientId,
) {
    let outbound = hub.register(id.clone());
    let (mut sink, mut stream) = socket.split();

    // 出口泵:把该客户端的出站帧写成 Text 帧。
    let pump = tokio::spawn(async move {
        let mut outbound = outbound;
        while let Some(frame) = outbound.recv().await {
            let text = frame.to_string();
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    // 入站:每个 Text/Binary 帧当作一行 JSONL。
    while let Some(Ok(msg)) = stream.next().await {
        let line = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => match String::from_utf8(b.to_vec()) {
                Ok(s) => s,
                Err(_) => continue, // 非 UTF-8 帧丢弃,不打断连接
            },
            Message::Close(_) => break,
            _ => continue, // Ping/Pong 由 axum 处理
        };
        if line.len() > MAX_FRAME_BYTES {
            tracing::warn!("ws frame exceeds max size; closing");
            break;
        }
        if inbound_tx.send((id.clone(), Ok(line))).await.is_err() {
            break;
        }
    }

    hub.unregister(&id);
    pump.abort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    // 客户端侧用的是 tungstenite 自己的 `Message`;`super::*` 带进来的 `Message`
    // 是 axum 的(服务端侧),二者是不同的类型,故此处按客户端角色改名。
    use serde_json::Value;
    use tokio_tungstenite::tungstenite::Message as ClientMessage;

    /// 起一个只监听 127.0.0.1 的 ws server,返回它的地址与后台句柄。
    async fn spawn_ws(
        cfg: RuntimeConfig,
    ) -> (SocketAddr, tokio::task::JoinHandle<anyhow::Result<()>>) {
        // workdir 与 workspace 索引都落在同一个临时目录里,并刻意让它活到进程
        // 结束(`mem::forget`):索引必须隔离,否则会读写用户真实的
        // `~/.yi-agent/workspaces.json`。
        let dir = tempfile::TempDir::new().unwrap();
        let workdir = dir.path().to_path_buf();
        let index_path = dir.path().join("workspaces.json");
        std::mem::forget(dir);
        let mut cfg = cfg;
        cfg.workdir = workdir;
        let workspaces = Arc::new(WorkspaceIndex::new(index_path));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(serve_ws(listener, cfg, workspaces));
        (addr, handle)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ws_client_completes_a_jsonrpc_handshake() {
        let (addr, handle) = spawn_ws(crate::server::tests_support::test_config()).await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .expect("ws connect");
        // initialize
        ws.send(ClientMessage::Text(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#.into(),
        ))
        .await
        .unwrap();
        let msg = ws.next().await.unwrap().unwrap();
        let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["serverInfo"]["name"], "yi-agent-app-server");
        // config/read
        ws.send(ClientMessage::Text(
            r#"{"jsonrpc":"2.0","id":2,"method":"config/read","params":{}}"#.into(),
        ))
        .await
        .unwrap();
        let msg = ws.next().await.unwrap().unwrap();
        let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert_eq!(v["id"], 2);
        handle.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_connection_is_rejected_while_the_first_is_open() {
        let (addr, handle) = spawn_ws(crate::server::tests_support::test_config()).await;
        let (mut first, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .expect("first connect");
        first
            .send(ClientMessage::Text(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#.into(),
            ))
            .await
            .unwrap();
        let _ = first.next().await; // 等它注册完成
        let second = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await;
        match second {
            Ok((mut s, _)) => {
                // 服务端应随即关闭这条连接。
                let closed =
                    tokio::time::timeout(std::time::Duration::from_secs(2), s.next()).await;
                assert!(
                    matches!(closed, Ok(Some(Ok(ClientMessage::Close(_)))) | Ok(None)),
                    "second connection must be closed by the server"
                );
            }
            Err(_) => {} // 握手阶段被拒也可接受
        }
        handle.abort();
    }

    /// 单个连接的传输错误(超大帧)只应摘除该 ClientId 并关闭该连接,
    /// 不得掀翻共享的服务器/主循环(spec §6):随后一个全新连接仍能完成
    /// `initialize`。回归 `serve` 的 inbound `Err` 分支曾无条件 `return Err`。
    #[tokio::test(flavor = "multi_thread")]
    async fn oversized_frame_closes_only_the_offending_connection() {
        let (addr, handle) = spawn_ws(crate::server::tests_support::test_config()).await;

        // 第一个连接:推一个超过 MAX_FRAME_BYTES 的帧。
        let (mut first, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .expect("first connect");
        first
            .send(ClientMessage::Text(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#.into(),
            ))
            .await
            .unwrap();
        let _ = first.next().await; // 等它注册完成

        let big = "x".repeat(MAX_FRAME_BYTES + 16);
        let _ = first.send(ClientMessage::Text(big.into())).await;

        // 服务端读到超限帧即 break 并摘除该客户端,连接随之关闭。
        let closed = tokio::time::timeout(std::time::Duration::from_secs(2), first.next()).await;
        assert!(
            matches!(
                closed,
                Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(ClientMessage::Close(_))))
            ),
            "the offending connection must be closed, got {closed:?}"
        );

        // 服务器必须仍然存活:一个全新连接仍能完成 initialize。
        let (mut second, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .expect("server must still accept connections after a peer transport error");
        second
            .send(ClientMessage::Text(
                r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{}}"#.into(),
            ))
            .await
            .unwrap();
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), second.next())
            .await
            .expect("server must still answer after a peer transport error")
            .unwrap()
            .unwrap();
        let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert_eq!(v["id"], 2);
        assert_eq!(v["result"]["serverInfo"]["name"], "yi-agent-app-server");
        handle.abort();
    }
}
