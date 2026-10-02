//! WebSocket 传输(Tier 1:多客户端 + token 认证 + 每连接 scope)。
//!
//! 与 stdio 共用**一套**主循环(thread 会话、审批表都在其中):所有连接都往
//! 同一个入站 channel 灌帧,并共享同一个 `Broadcaster`。每个连接一个
//! `ClientId`,准入不再限一个——第二台设备可以并行连接。

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, Uri};
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use tokio::sync::{Mutex, mpsc};

use crate::broadcast::{Broadcaster, ClientId};
use crate::pairing::PairingState;
use crate::protocol::MAX_FRAME_BYTES;
use crate::server::{
    ClientScopes, PERMISSION_TIMEOUT, RuntimeAttachments, install_device_registry,
    production_factory, serve,
};
use crate::workspace_index::WorkspaceIndex;
use yi_agent_runtime::config::RuntimeConfig;

/// 认证失败时用的 ws close code(RFC 6455 的 4000-4999 私有段):未提供或无效 token。
const UNAUTHORIZED_CLOSE_CODE: u16 = 4401;

/// 用一个已绑定的 listener 跑 ws 传输(生产:注入 `production_factory`)。
///
/// `pairing` 由调用方注入,必须与桌面 stdio 主循环是**同一个** `Arc`:桌面铸的
/// 配对码、手机兑现出的设备 token,都要落在同一张设备表上,ws 侧才能用它认证。
pub async fn serve_ws(
    listener: tokio::net::TcpListener,
    cfg: RuntimeConfig,
    workspaces: Arc<WorkspaceIndex>,
    pairing: Arc<PairingState>,
) -> anyhow::Result<()> {
    let build_agent = production_factory(cfg.clone());
    serve_ws_inner(listener, cfg, workspaces, pairing, build_agent).await
}

/// [`serve_ws`] 的可注入 agent 工厂版本:测试用 mock provider(如审批 E2E)。
pub(crate) async fn serve_ws_inner<F>(
    listener: tokio::net::TcpListener,
    cfg: RuntimeConfig,
    workspaces: Arc<WorkspaceIndex>,
    pairing: Arc<PairingState>,
    build_agent: F,
) -> anyhow::Result<()>
where
    F: Fn(
            Option<yi_agent_core::Session>,
            &std::path::Path,
            crate::thread_store::ThreadMode,
        ) -> anyhow::Result<crate::server::BuiltAgent>
        + Send
        + 'static,
{
    let hub = Arc::new(Broadcaster::new());
    let (inbound_tx, inbound_rx) = mpsc::channel::<(ClientId, anyhow::Result<String>)>(64);
    // 每连接一次登记的 scope(共享、可增长):ws 在握手时插入、断连时摘除,主
    // 循环按 `ClientId` 查它做门禁。缺省 = 未登记,主循环按 fail-closed 的
    // `Observe` 处理。
    let client_scopes: ClientScopes = Arc::new(Mutex::new(HashMap::new()));
    // 设备 id → 连接:供 `device/revoke` 摘掉被踢设备的连接。经进程级句柄暴露给
    // 主循环(见 `server::install_device_registry`),避免 `ws → server → ws` 的
    // 反向依赖成环。
    let device_clients = install_device_registry(Arc::new(std::sync::Mutex::new(HashMap::new())));

    let serve_hub = Arc::clone(&hub);
    // 先克隆出主循环要用的 config,再把 cfg 丢给下面的 router 闭包。
    let serve_cfg = cfg.clone();
    let serve_scopes = Arc::clone(&client_scopes);
    let serve_pairing = Arc::clone(&pairing);
    let serve_task = tokio::spawn(async move {
        use std::sync::Mutex as StdMutex;
        serve(
            inbound_rx,
            serve_hub,
            serve_cfg,
            PERMISSION_TIMEOUT,
            workspaces,
            serve_pairing,
            RuntimeAttachments {
                runtimes: Arc::new(StdMutex::new(HashMap::new())),
                thread_roots: Arc::new(StdMutex::new(HashMap::new())),
                board_dir: yi_agent_boards::global_dir().unwrap_or_default(),
                launcher: Arc::new(yi_agent_boards::lifecycle::launch_if_absent),
            },
            build_agent,
            serve_scopes,
        )
        .await
    });

    let router_hub = Arc::clone(&hub);
    let router_scopes = Arc::clone(&client_scopes);
    let router_devices = Arc::clone(&device_clients);
    let app = Router::new().route(
        "/ws",
        get(
            move |headers: HeaderMap, uri: Uri, upgrade: WebSocketUpgrade| {
                let hub = Arc::clone(&router_hub);
                let tx = inbound_tx.clone();
                let scopes = Arc::clone(&router_scopes);
                let devices = Arc::clone(&router_devices);
                let pairing = Arc::clone(&pairing);
                async move {
                    // 认证:token 取自 `Authorization: Bearer <token>`,或退回
                    // `?token=<token>`(无法设 header 的客户端)。缺 token 或校验失败
                    // 一律 4401 关闭——准入即认证,没有匿名连接。
                    let Some(token) = extract_token(&headers, &uri) else {
                        return close_unauthorized(upgrade, "missing token").await;
                    };
                    let Some(device) = pairing.authenticate(&token) else {
                        return close_unauthorized(upgrade, "invalid token").await;
                    };
                    let id = ClientId::ws(uuid::Uuid::new_v4());
                    let scope = device.scope;
                    let device_id = device.id;
                    upgrade.on_upgrade(move |socket| {
                        handle_ws(socket, hub, tx, id, scope, scopes, device_id, devices)
                    })
                }
            },
        ),
    );

    tracing::info!(addr = %listener.local_addr()?, "app-server ws listening");
    axum::serve(listener, app).await?;
    serve_task.abort();
    Ok(())
}

/// 拒绝一条未认证的升级:发一帧 4401 关闭帧。
///
/// axum 没有「在 upgrade 前直接关」的钩子,故先完成 upgrade 再立刻关闭——对
/// 客户端而言仍是「连接建立即被以 4401 关闭」,客户端据此重新配对。
async fn close_unauthorized(
    upgrade: WebSocketUpgrade,
    reason: &'static str,
) -> axum::response::Response {
    tracing::warn!("ws connection rejected: {reason}");
    upgrade.on_upgrade(|mut socket: WebSocket| async move {
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code: UNAUTHORIZED_CLOSE_CODE,
                reason: "unauthorized".into(),
            })))
            .await;
    })
}

/// 从握手请求里取设备 token:`Authorization: Bearer <token>` 优先,其次
/// `?token=<token>`。空 token 视同没有。
fn extract_token(headers: &HeaderMap, uri: &Uri) -> Option<String> {
    if let Some(value) = headers.get(axum::http::header::AUTHORIZATION) {
        if let Ok(value) = value.to_str() {
            if let Some(token) = value.strip_prefix("Bearer ") {
                let token = token.trim();
                if !token.is_empty() {
                    return Some(token.to_string());
                }
            }
        }
    }
    let query = uri.query()?;
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            if key == "token" && !value.is_empty() {
                return Some(percent_decode(value));
            }
        }
    }
    None
}

/// 最小 percent-decoding:token 是 `yia_<hex>`,实践中不含需转义的字符,但
/// 客户端可能按 URL 规则转义,故解出 `%XX` 与 `+`→空格。
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 一个 WS 连接的生命周期:登记客户端与 scope → 出口泵转发帧 → 入站帧喂主循环。
#[allow(clippy::too_many_arguments)]
async fn handle_ws(
    socket: WebSocket,
    hub: Arc<Broadcaster>,
    inbound_tx: mpsc::Sender<(ClientId, anyhow::Result<String>)>,
    id: ClientId,
    scope: crate::protocol::Scope,
    scopes: ClientScopes,
    device_id: String,
    devices: crate::server::WsDeviceRegistry,
) {
    let outbound = hub.register(id.clone());
    scopes.lock().await.insert(id.clone(), scope);
    devices
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(device_id.clone(), id.clone());
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
    scopes.lock().await.remove(&id);
    {
        let mut guard = devices.lock().unwrap_or_else(|p| p.into_inner());
        // 只有仍指向本连接的条目才摘除:同一设备可能已重连(新 ClientId),
        // 迟到关闭的旧连接不得把新连接的映射抹掉。
        if guard.get(&device_id) == Some(&id) {
            guard.remove(&device_id);
        }
    }
    pump.abort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    // 客户端侧用的是 tungstenite 自己的 `Message`;`super::*` 带进来的 `Message`
    // 是 axum 的(服务端侧),二者是不同的类型,故此处按客户端角色改名。
    use serde_json::Value;
    use tokio_tungstenite::tungstenite::ClientRequestBuilder;
    use tokio_tungstenite::tungstenite::Message as ClientMessage;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    /// 起一个只监听 127.0.0.1 的 ws server,返回它的地址、配对的 token 与后台句柄。
    ///
    /// 用隔离的 tempdir `DeviceStore` 铸一枚配对码换 token:`serve_ws` 收的是这个
    /// `PairingState`,故这台"手机"的 token 正好能认证它。**绝不**碰用户真实的
    /// `~/.yi-agent/devices.json`。
    async fn spawn_ws_with_token(
        cfg: RuntimeConfig,
    ) -> (
        SocketAddr,
        String,
        tokio::task::JoinHandle<anyhow::Result<()>>,
    ) {
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
        let pairing = Arc::new(PairingState::new(crate::device_store::DeviceStore::new(
            std::env::temp_dir().join(format!(
                "yi-agent-test-devices-{}.json",
                uuid::Uuid::new_v4()
            )),
        )));
        let code = pairing.create_code();
        let (_device, token) = pairing.redeem(&code.code, "test-phone").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(serve_ws(listener, cfg, workspaces, pairing));
        (addr, token, handle)
    }

    /// 连一条已认证的 ws;token 经 `Authorization: Bearer` 头带上。
    async fn connect_authed(
        addr: SocketAddr,
        token: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        let uri: axum::http::Uri = format!("ws://{addr}/ws").parse().unwrap();
        let request = ClientRequestBuilder::new(uri)
            .with_header("Authorization", format!("Bearer {token}"))
            .into_client_request()
            .unwrap();
        let (ws, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("authenticated ws connect");
        ws
    }

    /// 读一帧并解析为 JSON;超时即视为失败。
    async fn recv_json<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> Value
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for a ws frame")
            .expect("ws stream ended before a frame arrived")
            .expect("ws transport error");
        serde_json::from_str(msg.to_text().expect("server sent a non-text frame"))
            .expect("server sent invalid JSON")
    }

    async fn send_json<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>, line: &str)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        ws.send(ClientMessage::Text(line.into())).await.unwrap();
    }

    async fn initialize<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        crate::server::tests_support::initialize(ws).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ws_client_completes_a_jsonrpc_handshake() {
        let (addr, token, handle) =
            spawn_ws_with_token(crate::server::tests_support::test_config()).await;
        let mut ws = connect_authed(addr, &token).await;
        initialize(&mut ws).await;
        // config/read
        send_json(
            &mut ws,
            r#"{"jsonrpc":"2.0","id":2,"method":"config/read","params":{}}"#,
        )
        .await;
        let v = recv_json(&mut ws).await;
        assert_eq!(v["id"], 2);
        handle.abort();
    }

    /// 查询串形式的 token 也必须被接受(无法设 header 的客户端)。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_query_string_token_is_accepted() {
        let (addr, token, handle) =
            spawn_ws_with_token(crate::server::tests_support::test_config()).await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?token={token}"))
            .await
            .expect("query-string token connect");
        initialize(&mut ws).await;
        handle.abort();
    }

    /// 没有 token 的连接必须被以 4401 关闭,且不得进入主循环(连 initialize 都别想)。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_connection_without_a_token_is_closed_with_4401() {
        let (addr, _token, handle) =
            spawn_ws_with_token(crate::server::tests_support::test_config()).await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .expect("handshake completes; the close follows");
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("server must close an unauthenticated connection")
            .expect("stream must not end without a close frame")
            .expect("transport error");
        match msg {
            ClientMessage::Close(Some(frame)) => {
                assert_eq!(u16::from(frame.code), 4401, "close code must be 4401");
            }
            other => panic!("expected a 4401 close, got {other:?}"),
        }
        handle.abort();
    }

    /// 无效 token 与无 token 同等对待:4401。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_connection_with_an_invalid_token_is_closed_with_4401() {
        let (addr, _token, handle) =
            spawn_ws_with_token(crate::server::tests_support::test_config()).await;
        let uri: axum::http::Uri = format!("ws://{addr}/ws").parse().unwrap();
        let request = ClientRequestBuilder::new(uri)
            .with_header("Authorization", "Bearer not-a-real-token")
            .into_client_request()
            .unwrap();
        let (mut ws, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("handshake completes; the close follows");
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("server must close an invalid-token connection")
            .expect("stream must not end without a close frame")
            .expect("transport error");
        match msg {
            ClientMessage::Close(Some(frame)) => {
                assert_eq!(u16::from(frame.code), 4401, "close code must be 4401");
            }
            other => panic!("expected a 4401 close, got {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_connection_is_accepted_alongside_the_first() {
        let (addr, token, handle) =
            spawn_ws_with_token(crate::server::tests_support::test_config()).await;
        let mut first = connect_authed(addr, &token).await;
        initialize(&mut first).await;

        // Tier 1:第二条连接必须被**接受**,并完成自己的握手。
        let mut second = connect_authed(addr, &token).await;
        initialize(&mut second).await;

        // 第一条连接仍然存活(第二台的握手没有把它顶掉)。
        send_json(
            &mut first,
            r#"{"jsonrpc":"2.0","id":2,"method":"config/read","params":{}}"#,
        )
        .await;
        let v = recv_json(&mut first).await;
        assert_eq!(v["id"], 2);
        handle.abort();
    }

    /// 单个连接的传输错误(超大帧)只应摘除该 ClientId 并关闭该连接,
    /// 不得掀翻共享的服务器/主循环(spec §6):随后一个全新连接仍能完成
    /// `initialize`。回归 `serve` 的 inbound `Err` 分支曾无条件 `return Err`。
    #[tokio::test(flavor = "multi_thread")]
    async fn oversized_frame_closes_only_the_offending_connection() {
        let (addr, token, handle) =
            spawn_ws_with_token(crate::server::tests_support::test_config()).await;

        // 第一个连接:推一个超过 MAX_FRAME_BYTES 的帧。
        let mut first = connect_authed(addr, &token).await;
        initialize(&mut first).await;

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
        let mut second = connect_authed(addr, &token).await;
        initialize(&mut second).await;
        handle.abort();
    }

    /// 多客户端 + 审批先到先得(Tier 1 的端到端闭环)。
    ///
    /// A 与 B 都认证连接、都 initialize;A 发起的 turn 触发一次需审批的工具调用。
    /// 反向 `item/toolCall/requestApproval` 必须**广播**给**两端**(不再只发给发起
    /// 方);A 先答 allow 即生效,B 后答是 no-op 成功——B 不该收到任何 error,且
    /// 两端都应收到 `item/toolCall/approvalResolved`,turn 正常结束。
    #[tokio::test(flavor = "multi_thread")]
    async fn two_clients_both_see_the_turn_and_only_the_first_approval_counts() {
        let (addr, token, handle) =
            spawn_ws_permission(crate::server::tests_support::test_config()).await;
        let mut a = connect_authed(addr, &token).await;
        let mut b = connect_authed(addr, &token).await;
        crate::server::tests_support::initialize(&mut a).await;
        crate::server::tests_support::initialize(&mut b).await;

        // A 建 thread 并起一个会触发 bash 审批的 turn。
        send_json(
            &mut a,
            r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#,
        )
        .await;

        // A 收自己的请求响应拿 thread_id;B 只旁观,二者的响应各自到达。
        let thread_id = loop {
            let v = recv_json(&mut a).await;
            if v.get("id") == Some(&serde_json::json!(2)) {
                break v["result"]["thread_id"].as_str().unwrap().to_string();
            }
        };
        send_json(
            &mut a,
            &format!(
                r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"hi"}}]}}}}"#
            ),
        )
        .await;

        // 两端都必须收到 requestApproval(广播),取到同一个 perm id。
        let (perm_a, perm_b) = tokio::join!(recv_approval_id(&mut a), recv_approval_id(&mut b),);
        assert_eq!(
            perm_a, perm_b,
            "both clients must observe the same approval request"
        );

        // A 先答 allow:它应收到 approvalResolved(由 A 触发)。
        send_json(
            &mut a,
            &format!(r#"{{"jsonrpc":"2.0","id":"{perm_a}","result":{{"decision":"allow_once"}}}}"#),
        )
        .await;
        let resolved = recv_until_method(&mut a, "item/toolCall/approvalResolved").await;
        assert_eq!(resolved["params"]["perm_id"], perm_a.as_str());
        assert_eq!(resolved["params"]["decision"], "allow_once");

        // B 后答(同一 perm):必须是 no-op 成功——B 不得收到任何 error,
        // 且 B 也应收到那次 approvalResolved 广播。
        send_json(
            &mut b,
            &format!(r#"{{"jsonrpc":"2.0","id":"{perm_b}","result":{{"decision":"deny"}}}}"#),
        )
        .await;
        let resolved_b = recv_until_method(&mut b, "item/toolCall/approvalResolved").await;
        assert_eq!(resolved_b["params"]["perm_id"], perm_b.as_str());

        // B 继续读:不得出现任何针对 perm_b 的 error 帧。
        let mut b_errors = Vec::new();
        for _ in 0..6 {
            match tokio::time::timeout(std::time::Duration::from_millis(300), b.next()).await {
                Ok(Some(Ok(ClientMessage::Text(t)))) => {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    if v.get("error").is_some() {
                        b_errors.push(v);
                    }
                }
                _ => break,
            }
        }
        assert!(
            b_errors.is_empty(),
            "the late answer must not error: {b_errors:?}"
        );

        // turn 正常结束:A 应看到 turn/completed。
        let completed = recv_until_method(&mut a, "turn/completed").await;
        assert_eq!(
            completed["params"]["thread_id"],
            thread_id.as_str(),
            "the turn must complete after the approval: {completed}"
        );
        handle.abort();
    }

    /// 启动一个 ws server,其 agent 工厂会触发一次 bash 审批(与
    /// `server::tests::build_permission_agent` 同源),供多客户端 E2E 用。
    async fn spawn_ws_permission(
        cfg: RuntimeConfig,
    ) -> (
        SocketAddr,
        String,
        tokio::task::JoinHandle<anyhow::Result<()>>,
    ) {
        let dir = tempfile::TempDir::new().unwrap();
        let workdir = dir.path().to_path_buf();
        let index_path = dir.path().join("workspaces.json");
        std::mem::forget(dir);
        let mut cfg = cfg;
        cfg.workdir = workdir;
        let workspaces = Arc::new(WorkspaceIndex::new(index_path));
        let pairing = Arc::new(PairingState::new(crate::device_store::DeviceStore::new(
            std::env::temp_dir().join(format!(
                "yi-agent-test-devices-{}.json",
                uuid::Uuid::new_v4()
            )),
        )));
        let code = pairing.create_code();
        let (_device, token) = pairing.redeem(&code.code, "test-phone").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(crate::server::tests::serve_ws_with_permission_agent(
            listener, cfg, workspaces, pairing,
        ));
        (addr, token, handle)
    }

    /// 读到 `item/toolCall/requestApproval`,返回其 id。
    async fn recv_approval_id<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> String
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        loop {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
                .await
                .expect("timed out waiting for requestApproval")
                .expect("ws stream ended before requestApproval")
                .expect("ws transport error");
            let ClientMessage::Text(t) = frame else {
                continue;
            };
            let v: Value = serde_json::from_str(&t).unwrap();
            if v.get("method").and_then(Value::as_str) == Some("item/toolCall/requestApproval") {
                return v["id"]
                    .as_str()
                    .expect("perm id must be a string")
                    .to_string();
            }
        }
    }

    /// 读到指定 method 的通知帧。
    async fn recv_until_method<S>(
        ws: &mut tokio_tungstenite::WebSocketStream<S>,
        method: &str,
    ) -> Value
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        loop {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {method}"))
                .expect("ws stream ended before the expected frame")
                .expect("ws transport error");
            let ClientMessage::Text(t) = frame else {
                continue;
            };
            let v: Value = serde_json::from_str(&t).unwrap();
            if v.get("method").and_then(Value::as_str) == Some(method) {
                return v;
            }
        }
    }
}
