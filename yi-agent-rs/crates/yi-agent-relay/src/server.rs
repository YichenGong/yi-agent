//! 中继服务端:hosts 两个出站 ws 端点,按 `session` 配对并转发帧。
//!
//! - 电脑侧(agent):`GET /connect?session=<id>`
//! - 手机侧(app):`GET /ws?session=<id>`
//!
//! 两端都是**出站连接**:电脑主动连 `/connect`,手机主动连 `/ws`;中继不主动连
//! 任何一方(spec §11.1「两端都出站」)。中继不解析 JSON-RPC 语义,只做搬运。

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::{ENDPOINT_QUEUE, INBOUND_QUEUE, Relay};

/// 两端共用的 `session` 查询参数。
#[derive(Deserialize)]
pub struct SessionQuery {
    pub session: Option<String>,
}

/// 构造中继路由(便于测试直接 `axum::serve` 一个已绑定的 listener)。
pub fn router(relay: Arc<Relay>) -> Router {
    Router::new()
        .route("/connect", get(agent_upgrade))
        .route("/ws", get(app_upgrade))
        .fallback(get(root))
        .with_state(relay)
}

/// 在一个已绑定的 listener 上跑中继,直到进程结束。
pub async fn serve(listener: tokio::net::TcpListener, relay: Arc<Relay>) -> Result<()> {
    let addr = listener.local_addr()?;
    tracing::info!(%addr, "yi-agent-relay listening");
    axum::serve(listener, router(relay)).await?;
    Ok(())
}

/// 解析 `--listen` 地址(默认 `127.0.0.1:8080`)。
pub fn parse_listen(raw: &str) -> Result<SocketAddr> {
    raw.parse()
        .map_err(|e| anyhow::anyhow!("invalid --listen address `{raw}`: {e}"))
}

/// 一段极简的运行说明(非 ws 请求)。
async fn root() -> &'static str {
    "yi-agent-relay: computers connect to /connect?session=<id>, phones to /ws?session=<id>\n"
}

/// 电脑侧升级:`/connect?session=<id>`。
async fn agent_upgrade(
    State(relay): State<Arc<Relay>>,
    Query(q): Query<SessionQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let session = q.session.unwrap_or_else(|| "default".to_string());
    upgrade.on_upgrade(move |socket| bridge_agent(relay, session, socket))
}

/// 手机侧升级:`/ws?session=<id>`。
async fn app_upgrade(
    State(relay): State<Arc<Relay>>,
    Query(q): Query<SessionQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let session = q.session.unwrap_or_else(|| "default".to_string());
    upgrade.on_upgrade(move |socket| bridge_app(relay, session, socket))
}

/// 电脑连接的生命周期:出口泵(中继 → 电脑)+ 入站泵(电脑 → 中继)。
async fn bridge_agent(relay: Arc<Relay>, session: String, socket: WebSocket) {
    tracing::info!(%session, "agent connected");
    let (out_tx, out_rx) = mpsc::channel::<Message>(ENDPOINT_QUEUE);
    let (in_tx, in_rx) = mpsc::channel::<Message>(INBOUND_QUEUE);
    let relay_task = tokio::spawn(async move { relay.attach_agent(session, out_tx, in_rx).await });
    let (read, write) = split_bridge(socket, in_tx, out_rx);
    let _ = tokio::join!(read, write);
    relay_task.abort();
}

/// 手机连接的生命周期。
async fn bridge_app(relay: Arc<Relay>, session: String, socket: WebSocket) {
    tracing::info!(%session, "app connected");
    let (out_tx, out_rx) = mpsc::channel::<Message>(ENDPOINT_QUEUE);
    let (in_tx, in_rx) = mpsc::channel::<Message>(INBOUND_QUEUE);
    let relay_task = tokio::spawn(async move { relay.attach_app(session, out_tx, in_rx).await });
    let (read, write) = split_bridge(socket, in_tx, out_rx);
    let _ = tokio::join!(read, write);
    relay_task.abort();
}

/// 把一条 socket 拆成「读 → `in_tx`」与「`out_rx` → 写」两半。
fn split_bridge(
    socket: WebSocket,
    in_tx: mpsc::Sender<Message>,
    mut out_rx: mpsc::Receiver<Message>,
) -> (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>) {
    let (mut sink, mut stream) = socket.split();
    let read = tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            match msg {
                Message::Text(_) | Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => {
                    if in_tx.send(msg).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
            }
        }
    });
    let write = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if sink.send(frame).await.is_err() {
                break;
            }
        }
    });
    (read, write)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_listen_defaults_are_parseable() {
        assert!(parse_listen("127.0.0.1:8080").is_ok());
        assert!(parse_listen("0.0.0.0:443").is_ok());
        assert!(parse_listen("not-an-address").is_err());
    }
}
