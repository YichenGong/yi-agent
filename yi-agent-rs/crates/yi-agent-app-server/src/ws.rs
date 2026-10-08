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
    ClientInitialized, ClientScopes, PERMISSION_TIMEOUT, RuntimeAttachments, WsDeviceRegistry,
    install_device_registry, production_factory, serve,
};
use crate::workspace_index::WorkspaceIndex;
use yi_agent_runtime::config::RuntimeConfig;

/// 认证失败时用的 ws close code(RFC 6455 的 4000-4999 私有段):未提供或无效 token。
const UNAUTHORIZED_CLOSE_CODE: u16 = 4401;

/// 一次性配对兑现连接交付 token 后用的 close code。
///
/// 兑现连接**不做通用传输**:把 token 交给手机后即关闭,手机须带 token 重连
/// (见 [`redeem_pair_code`])。用 4403(而非 4401)与「未认证」区分开,便于客户端
/// 分辨「配对失败」与「配对成功、请用 token 重连」。
const PAIRING_DELIVERED_CLOSE_CODE: u16 = 4403;

/// `pair/redeemed` 通知的方法名。
const PAIR_REDEEMED_METHOD: &str = "pair/redeemed";

/// 未提供 `device_name` 时的默认设备名。
const DEFAULT_DEVICE_NAME: &str = "iPhone";

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
    // 主题句柄:`ui/settings/read|write` 与每个 thread 的 `set_theme` 工具共用,
    // 与 stdio 路径同构——工厂闭包是 `'static`,拿不到 `serve_ws_inner` 里的
    // `theme`,故先克隆一份专供工厂。
    let theme = crate::theme_tool::ThemeHandle::new(cfg.workdir.clone());
    let theme_for_factory = theme.clone();
    // 清单启动读一次;工厂闭包持有它,逐会话按 `model_ref` 解析 provider。
    let catalog = Arc::new(yi_agent_runtime::models::load_catalog());
    let build_agent = production_factory(cfg.clone(), theme_for_factory, catalog);
    serve_ws_inner(listener, cfg, workspaces, pairing, theme, build_agent).await
}

/// [`serve_ws`] 的可注入 agent 工厂版本:测试用 mock provider(如审批 E2E)。
pub(crate) async fn serve_ws_inner<F>(
    listener: tokio::net::TcpListener,
    cfg: RuntimeConfig,
    workspaces: Arc<WorkspaceIndex>,
    pairing: Arc<PairingState>,
    theme: crate::theme_tool::ThemeHandle,
    build_agent: F,
) -> anyhow::Result<()>
where
    F: Fn(
            Option<yi_agent_core::Session>,
            &std::path::Path,
            crate::thread_store::ThreadMode,
        ) -> anyhow::Result<crate::server::BuiltAgent>
        + Send
        + Sync
        + 'static,
{
    let hub = Arc::new(Broadcaster::new());
    let (inbound_tx, inbound_rx) = mpsc::channel::<(ClientId, anyhow::Result<String>)>(64);
    // 每连接一次登记的 scope(共享、可增长):ws 在握手时插入、断连时摘除,主
    // 循环按 `ClientId` 查它做门禁。缺省 = 未登记,主循环按 fail-closed 的
    // `Observe` 处理。
    let client_scopes: ClientScopes = Arc::new(Mutex::new(HashMap::new()));
    // 每连接的 `initialize` 状态:与 scope 表同型、同生命周期。主循环持有同一个
    // `Arc`,在收到 `initialize` 时置位;ws 断连时摘键——否则连接可来可去、这张
    // 表只增不减(每台设备都永久留一个 `ClientId`)。
    let client_initialized: ClientInitialized = Arc::new(Mutex::new(HashMap::new()));
    // 设备 id → 连接:供 `device/revoke` 摘掉被踢设备的连接。经进程级句柄暴露给
    // 主循环(见 `server::install_device_registry`),避免 `ws → server → ws` 的
    // 反向依赖成环。
    let device_clients = install_device_registry(Arc::new(std::sync::Mutex::new(HashMap::new())));

    let serve_hub = Arc::clone(&hub);
    // 主循环用克隆出的 config(抽取后 router 不再需要 cfg)。
    let serve_cfg = cfg.clone();
    let serve_scopes = Arc::clone(&client_scopes);
    let serve_initialized = Arc::clone(&client_initialized);
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
                models_path: yi_agent_runtime::models::models_path(),
                resident_dir: yi_agent_store::resident::default_dir().unwrap_or_default(),
                launcher: Arc::new(yi_agent_boards::lifecycle::launch_if_absent),
                theme,
                git_diff: crate::git_diff_tool::GitDiffHandle::new(),
                watchman_install: crate::server::production_watchman_install(),
                watchman_uninstall: crate::server::production_watchman_uninstall(),
                watchman_home: crate::server::home_dir(),
            },
            build_agent,
            serve_scopes,
            serve_initialized,
            // 纯 ws 路径无 stdio 读端,故无 EOF 观察者。
            None,
        )
        .await
    });

    // ws 前端跑在自己的任务里;主任务在此等待它,与抽取前内联
    // `axum::serve(...).await?` 同形(仅在服务失败时返回 `Err`)。
    //
    // 为什么在这里等、而不是把 `JoinHandle` 直接返回给调用方:本函数一返回,外层
    // `serve_ws` 任务就当场结束——`a_client_leaving_does_not_kill_the_shared_loop`
    // (见本文件测试模块,约 ws.rs:952)用 `!handle.is_finished()`(约 ws.rs:977-980)
    // 断言共享主循环在客户端离开后仍存活,直接返回会打破该断言,同时丢掉「前端失败
    // → `Err`」的传播。
    let frontend = attach_ws_frontend(
        listener,
        hub,
        inbound_tx,
        client_scopes,
        client_initialized,
        device_clients,
        pairing,
    );
    // 守卫必须在 await **之前**取:本函数唯一的 `?` 就在下面的 `frontend.await??`,
    // 它一旦传播就直接 return,走不到末尾的 `serve_task.abort()`(生产上 `axum::serve`
    // 内部是 `pending()`,永不返回,故 `Ok(())` 分支不可达)——也就是说那句 abort 在
    // **唯一可达**路径上是死代码。把前端的取消柄交给 `AbortOnDrop`,才能在「`?` 传播」
    // 与「本任务被 abort 拍掉」两条真实可达的离开路径上,都同步停掉 spawn 出的前端
    // 监听任务(否则它会脱离外层任务、继续 accept 连接)。
    let _frontend_abort = AbortOnDrop(frontend.abort_handle());
    frontend.await??;
    // 以下两句只在不可达的 `Ok(())` 分支上有机会执行,保留以对抽取前的内联形态
    // 逐字一致;teardown 已由上面的 `AbortOnDrop` 守卫覆盖。
    serve_task.abort();
    Ok(())
}

/// `Drop` 即 [`tokio::task::AbortHandle::abort`]:把 [`attach_ws_frontend`] spawn 出的
/// 监听任务绑到持有者(此处 [`serve_ws_inner`])的生命周期上。
///
/// 需要的理由:tokio 的 `JoinHandle` 在 drop 时**不会**取消任务,而 `serve_ws_inner`
/// 里的 `serve_task.abort()` 处在唯一可达路径之外(`frontend.await??` 的 `?` 会先
/// 传播,且 `axum::serve` 的 `Ok(())` 分支不可达)。裸等 `?` 传播、或外层任务被 abort
/// 时,若不额外持有取消句柄,`attach_ws_frontend` spawn 出的前端会脱离外层任务、继续
/// 接受连接——正是抽取后新出现的那条「abort 不停前端」路径。这个守卫在**所有**离开
/// 作用域的路径(drop)上 abort 它,把 teardown 钉回可达路径。
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// 把一个 ws 前端挂到一个**已构建**的 hub / 入站 channel / scope 表上。
///
/// 抽取自 [`serve_ws_inner`],负责:构造 axum `Router` 的 `/ws` 路由(握手准入
/// ——配对码兑现、token 认证、升级后登记 scope/设备并交给 [`handle_ws`])并用
/// `axum::serve` 服务该 listener。主循环 `serve(...)` 不在此函数内:调用方用
/// 同一个 `inbound_rx` 驱动它,故本函数可复用来给已有主循环再挂一个前端。
///
/// 返回值与生命周期契约(**重要**,与抽取前内联 `axum::serve(...).await?` 的语义
/// 有一处真实差异):
/// - 返回的 `JoinHandle<anyhow::Result<()>>` 已 spawn 在**独立任务**上。监听错误
///   (如 `listener.local_addr()` 失败)落在 `JoinHandle` 的 `Result` 里,**不会**
///   自动冒泡:调用方必须 `await` 并展平(如 `handle.await??`)才能观测到它;
///   不 await 就完全丢弃该错误。
/// - 该任务**脱离调用方的生命周期**:tokio 在 `JoinHandle` 被 drop 时**不取消**
///   任务。因此调用方被 abort 或提前返回,并**不会**停掉已 spawn 的前端——它会
///   继续 accept 连接。
/// - 要停掉前端,调用方必须持有取消柄并 abort:直接用返回的 `JoinHandle::abort()`,
///   或先 `let h = handle.abort_handle();`(再用 `handle.await??` 观测错误)后 abort
///   `h`。若只是 `await` 到结束,则只在监听器自身出错(罕见)时才收尾。
pub(crate) fn attach_ws_frontend(
    listener: tokio::net::TcpListener,
    hub: Arc<Broadcaster>,
    inbound_tx: mpsc::Sender<(ClientId, anyhow::Result<String>)>,
    client_scopes: ClientScopes,
    client_initialized: ClientInitialized,
    device_clients: WsDeviceRegistry,
    pairing: Arc<PairingState>,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    let app = Router::new().route(
        "/ws",
        get(
            move |headers: HeaderMap, uri: Uri, upgrade: WebSocketUpgrade| {
                let hub = Arc::clone(&hub);
                let tx = inbound_tx.clone();
                let scopes = Arc::clone(&client_scopes);
                let initialized = Arc::clone(&client_initialized);
                let devices = Arc::clone(&device_clients);
                let pairing = Arc::clone(&pairing);
                async move {
                    // Flow A(spec §5.1/§4.3):手机扫到二维码里的是一次性配对码,
                    // 带外没有 token,必须在握手时用 `?pair=<code>` 兑换。这条路径
                    // **在准入认证之前**处理,但它自己不做任何鉴权放行——成功只把这
                    // 条一次性连接的 token 交回手机并立即关闭;失败则原样落到下面的
                    // 4401 路径(与「无 token」等价,不泄露码是否存在)。
                    if let Some(code) = query_param(&uri, "pair") {
                        let name = query_param(&uri, "device_name")
                            .unwrap_or_else(|| DEFAULT_DEVICE_NAME.to_string());
                        return redeem_pair_code(upgrade, pairing, code, name).await;
                    }
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
                        handle_ws(
                            socket,
                            hub,
                            tx,
                            id,
                            scope,
                            scopes,
                            initialized,
                            device_id,
                            devices,
                        )
                    })
                }
            },
        ),
    );

    tokio::spawn(async move {
        tracing::info!(addr = %listener.local_addr()?, "app-server ws listening");
        axum::serve(listener, app).await?;
        Ok(())
    })
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

/// 一条 `?pair=<code>` 握手:**一次性**兑换配对码并把这台新设备的 token 交回。
///
/// 形态(手机侧唯一需要认识的一帧):
///
/// ```json
/// {"jsonrpc":"2.0","method":"pair/redeemed",
///  "params":{"device_id":"dev-…","token":"yia_…","scope":"control"}}
/// ```
///
/// 为什么是「送完即关」(close after delivery)而不是把这条连接直接当成已认证:
/// - 配对是**一次性**事件,不是会话:手机拿到 token 后本来就要持久化并以
///   `?token=`/`Bearer` 走正常连接路径(spec §5.2 流程 B)。把兑现连接并入通用
///   传输,等于给一条"没有 device scope 登记、也没有走标准认证"的 socket 开一个
///   特例,徒增状态与风险面。
/// - 少一层状态 = 少一个绕过点:兑现连接从不注册进 `Broadcaster`、不写 scope 表、
///   不喂主循环,因此即使逻辑出错也不可能以某台设备的身份消费帧。
/// - 关帧用 4403,客户端可据此区分「配对失败(4401)」与「配对成功、请带 token
///   重连(4403)」。
///
/// 失败(码不存在/已用过/已过期,或设备表写失败)**一律落到 4401**,与「无 token」
/// 不可区分——不泄露某个码是否曾经存在。
async fn redeem_pair_code(
    upgrade: WebSocketUpgrade,
    pairing: Arc<PairingState>,
    code: String,
    device_name: String,
) -> axum::response::Response {
    let redeemed = pairing.redeem(&code, &device_name);
    match redeemed {
        Ok((device, token)) => {
            tracing::info!(device_id = %device.id, "ws pair code redeemed");
            upgrade.on_upgrade(move |mut socket: WebSocket| async move {
                let frame = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": PAIR_REDEEMED_METHOD,
                    "params": {
                        "device_id": device.id,
                        "token": token,
                        "scope": device.scope,
                    }
                });
                let _ = socket.send(Message::Text(frame.to_string().into())).await;
                // 交付即关:这是一次性兑现通道,不是可用的会话。
                let _ = socket
                    .send(Message::Close(Some(CloseFrame {
                        code: PAIRING_DELIVERED_CLOSE_CODE,
                        reason: "paired".into(),
                    })))
                    .await;
            })
        }
        Err(_) => close_unauthorized(upgrade, "invalid pair code").await,
    }
}

/// 取查询串里的第一个 `key=value`(已 percent-decode)。空值视同不存在。
fn query_param(uri: &Uri, key: &str) -> Option<String> {
    let query = uri.query()?;
    for pair in query.split('&') {
        if let Some((k, value)) = pair.split_once('=') {
            if k == key && !value.is_empty() {
                return Some(percent_decode(value));
            }
        }
    }
    None
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
    query_param(uri, "token")
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
///
/// 另起一个低频 `interval` tick 与入站流 `select!`:被 `device/revoke` 踢掉、或
/// 被广播背压摘除的连接,其读循环仍在等入站帧,不会自己醒;每 tick 检一次
/// `hub.is_connected` 才能及时 `break`,进而收尾(摘 scope/initialized/device、
/// abort 出口泵、drop socket)——这就是「撤销即掉线」的实现。
#[allow(clippy::too_many_arguments)]
async fn handle_ws(
    socket: WebSocket,
    hub: Arc<Broadcaster>,
    inbound_tx: mpsc::Sender<(ClientId, anyhow::Result<String>)>,
    id: ClientId,
    scope: crate::protocol::Scope,
    scopes: ClientScopes,
    initialized: ClientInitialized,
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

    // 入站:每个 Text/Binary 帧当作一行 JSONL。低频 tick 用于发现「已被踢」。
    let mut kick_check = tokio::time::interval(std::time::Duration::from_millis(200));
    loop {
        let msg = tokio::select! {
            _ = kick_check.tick() => {
                if !hub.is_connected(&id) {
                    break; // 被 `device/revoke` 踢掉(或被背压摘除):收尾关闭。
                }
                continue;
            }
            next = stream.next() => match next {
                Some(Ok(msg)) => msg,
                _ => break, // 对端关闭或传输错误
            },
        };
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
    teardown_ws_client(&hub, &id, &scopes, &initialized, &devices, &device_id).await;
    pump.abort();
}

/// 连接收尾:把该 `ClientId` 从**所有**共享表里摘除。
///
/// 与 `prepare`(登记)成对:登记了 scope、`device_id → ClientId`、以及主循环懒
/// 插入的 `initialized`,收尾时都要摘掉。`initialized` 尤其容易漏——它由主循环在
/// 收到 `initialize` 时才插入,连接可来可去,不摘就是只增不减的表。
///
/// 抽成独立函数是为了可测:不需要真 socket,即可验证三张表都被清干净。
async fn teardown_ws_client(
    hub: &Broadcaster,
    id: &ClientId,
    scopes: &ClientScopes,
    initialized: &ClientInitialized,
    devices: &crate::server::WsDeviceRegistry,
    device_id: &str,
) {
    hub.unregister(id);
    scopes.lock().await.remove(id);
    initialized.lock().await.remove(id);
    let mut guard = devices.lock().unwrap_or_else(|p| p.into_inner());
    // 只有仍指向本连接的条目才摘除:同一设备可能已重连(新 ClientId),
    // 迟到关闭的旧连接不得把新连接的映射抹掉。
    if guard.get(device_id) == Some(id) {
        guard.remove(device_id);
    }
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

    /// 一台已配对设备(其 token 能认证本 server)的句柄:地址、明文 token、
    /// 以及**与 server 共享**的那个 `PairingState`。
    ///
    /// `pairing` 返回出来,是因为「撤销后掉线」这类测试既要拿 `device_id` 去
    /// `device/revoke`,又要在撤销后复查 `pairing.authenticate` 已失败(证明 token
    /// 真的废了,而不只是 socket 关了)。
    struct WsFixture {
        addr: SocketAddr,
        token: String,
        pairing: Arc<PairingState>,
        handle: tokio::task::JoinHandle<anyhow::Result<()>>,
    }

    /// 起一个只监听 127.0.0.1 的 ws server,铸一台默认 `Control` 设备,返回夹具。
    ///
    /// 用隔离的 tempdir `DeviceStore` 铸设备:`serve_ws` 收的是这个
    /// `PairingState`,故这台"手机"的 token 正好能认证它。**绝不**碰用户真实的
    /// `~/.yi-agent/devices.json`。
    async fn spawn_ws_with_token(cfg: RuntimeConfig) -> WsFixture {
        spawn_ws_with_token_for(cfg, crate::protocol::Scope::Control).await
    }

    /// 同 [`spawn_ws_with_token`],但可指定设备 scope:`device/revoke` 是 Admin
    /// 门禁内的 RPC,要经 ws 驱动它,连接就必须是一台 Admin 设备(不能用配对码
    /// 换——新配对设备恒为 Control)。
    async fn spawn_ws_with_token_for(
        cfg: RuntimeConfig,
        scope: crate::protocol::Scope,
    ) -> WsFixture {
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
        let (_device, token) = pairing.seed_device("test-phone", scope);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(serve_ws(listener, cfg, workspaces, Arc::clone(&pairing)));
        WsFixture {
            addr,
            token,
            pairing,
            handle,
        }
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
        let WsFixture {
            addr,
            token,
            handle,
            ..
        } = spawn_ws_with_token(crate::server::tests_support::test_config()).await;
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
        let WsFixture {
            addr,
            token,
            handle,
            ..
        } = spawn_ws_with_token(crate::server::tests_support::test_config()).await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?token={token}"))
            .await
            .expect("query-string token connect");
        initialize(&mut ws).await;
        handle.abort();
    }

    /// 没有 token 的连接必须被以 4401 关闭,且不得进入主循环(连 initialize 都别想)。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_connection_without_a_token_is_closed_with_4401() {
        let WsFixture { addr, handle, .. } =
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
        let WsFixture { addr, handle, .. } =
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
        let WsFixture {
            addr,
            token,
            handle,
            ..
        } = spawn_ws_with_token(crate::server::tests_support::test_config()).await;
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
        let WsFixture {
            addr,
            token,
            handle,
            ..
        } = spawn_ws_with_token(crate::server::tests_support::test_config()).await;

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

    /// 断言携带 `token` 的连接会被以 4401 关闭。
    async fn assert_closed_with_4401(addr: SocketAddr, token: &str) {
        let uri: axum::http::Uri = format!("ws://{addr}/ws").parse().unwrap();
        let request = ClientRequestBuilder::new(uri)
            .with_header("Authorization", format!("Bearer {token}"))
            .into_client_request()
            .unwrap();
        let (mut ws, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("handshake completes; the close follows");
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("server must close the connection")
            .expect("stream must not end without a close frame")
            .expect("transport error");
        match msg {
            ClientMessage::Close(Some(frame)) => {
                assert_eq!(u16::from(frame.code), 4401, "close code must be 4401");
            }
            other => panic!("expected a 4401 close, got {other:?}"),
        }
    }

    /// 连一条**未带 token**的 ws(可带任意查询串),断言服务端以 `code` 关闭。
    ///
    /// 用于「配对失败一律落到普通 4401 路径」与「一次性兑现连接送完即关」两类断言。
    async fn assert_closed_with(addr: SocketAddr, query: &str, code: u16) {
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?{query}"))
            .await
            .expect("handshake completes; the close follows");
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("server must close the connection")
            .expect("stream must not end without a close frame")
            .expect("transport error");
        match msg {
            ClientMessage::Close(Some(frame)) => {
                assert_eq!(u16::from(frame.code), code, "unexpected close code");
            }
            other => panic!("expected a close frame, got {other:?}"),
        }
    }

    /// Flow A(spec §5.1/§4.3):手机扫到的是一次性配对码,带外拿不到 token,
    /// 必须在 ws 握手时用 `?pair=<code>` 兑换。本测试覆盖完整往返:
    /// 兑现 → 服务端把 token 回给该连接并以 4403 关闭(一次性,simpler/safer);
    /// 用该 token 重连能 `initialize`;同一个码再兑必须失败(落到 4401)。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_pair_code_is_redeemed_over_the_ws_and_yields_a_usable_token() {
        let WsFixture {
            addr,
            pairing,
            handle,
            ..
        } = spawn_ws_with_token(crate::server::tests_support::test_config()).await;
        let code = pairing.create_code();

        // 1. 未认证连接带 `?pair=<code>` 与 `device_name`,服务端必须回 `pair/redeemed`。
        let (mut ws, _) = tokio_tungstenite::connect_async(format!(
            "ws://{addr}/ws?pair={}&device_name=iPhone%2015",
            code.code
        ))
        .await
        .expect("pairing ws connect");
        let v = recv_json(&mut ws).await;
        assert_eq!(
            v["method"], "pair/redeemed",
            "the server must deliver the redeemed token on this connection: {v}"
        );
        let token = v["params"]["token"]
            .as_str()
            .expect("pair/redeemed must carry a token")
            .to_string();
        assert!(token.starts_with("yia_"), "unexpected token form: {token}");
        assert_eq!(
            v["params"]["scope"], "control",
            "a newly paired device must default to control scope"
        );
        assert!(
            v["params"]["device_id"].as_str().is_some(),
            "pair/redeemed must carry the new device id: {v}"
        );

        // 2. 兑现连接是一次性的:送完 token 即由服务端关闭(4403)。
        assert_closed_after_delivery(&mut ws).await;

        // 3. 用换来的 token 重连,正常完成 initialize。
        let mut authed = connect_authed(addr, &token).await;
        initialize(&mut authed).await;

        // 4. 同一个码不能二次兑现:失败必须落到普通 4401 路径。
        assert_closed_with(addr, &format!("pair={}", code.code), 4401).await;
        handle.abort();
    }

    /// Tier 1.5 关键证明(跨进程拓扑):桌面 stdio 进程铸码,`--relay`/`ws://`
    /// 进程兑换——两个进程、两个 `PairingState` 实例,只共享 `devices.json` 与
    /// `pairing.json`。这里让 ws server 的 `PairingState` 与"桌面"的实例使用**同一
    /// 组文件**,但实例不同:桌面实例铸码,ws 侧必须能兑换。
    ///
    /// 回归:码未落盘时(仅进程内 `HashMap`),ws 侧读不到该码,兑换恒 4401。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_pair_code_minted_by_another_instance_redeems_over_the_ws() {
        let dir = tempfile::TempDir::new().unwrap();
        let devices = dir.path().join("devices.json");

        // "桌面 stdio 进程":自己的 PairingState 实例,铸码。
        let desktop = Arc::new(PairingState::new(crate::device_store::DeviceStore::new(
            devices.clone(),
        )));
        let code = desktop.create_code();

        // "relay/ws 进程":另一个实例,同一组文件。
        let workspaces = Arc::new(WorkspaceIndex::new(dir.path().join("workspaces.json")));
        let mut cfg = crate::server::tests_support::test_config();
        cfg.workdir = dir.path().to_path_buf();
        // 这个进程的 PairingState 与桌面实例共享同一组文件,但实例不同。
        // 本测试只走 `?pair=`,不需要本机凭据。
        let relay = Arc::new(PairingState::new(crate::device_store::DeviceStore::new(
            devices.clone(),
        )));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(serve_ws(listener, cfg, workspaces, Arc::clone(&relay)));

        // 手机用桌面铸出的码连接 ws 兑换,必须成功拿到 token。
        let (mut ws, _) = tokio_tungstenite::connect_async(format!(
            "ws://{addr}/ws?pair={}&device_name=iPhone%2015",
            code.code
        ))
        .await
        .expect("pairing ws connect");
        let v = recv_json(&mut ws).await;
        assert_eq!(
            v["method"], "pair/redeemed",
            "a code from another process must redeem over the ws: {v}"
        );
        let token = v["params"]["token"].as_str().expect("token").to_string();
        assert_closed_after_delivery(&mut ws).await;

        // token 落在共享设备表上:两个实例都能认证。
        assert!(relay.authenticate(&token).is_some());
        assert!(
            desktop.authenticate(&token).is_some(),
            "the minting process must also authenticate the token"
        );
        let mut authed = connect_authed(addr, &token).await;
        initialize(&mut authed).await;
        handle.abort();
    }

    /// 断言一条已送出兑现 token 的连接会被服务端以 4403 关闭(或直接 EOF)。
    async fn assert_closed_after_delivery<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let closed = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("the server must close a one-shot pairing connection");
        match closed {
            Some(Ok(ClientMessage::Close(Some(frame)))) => {
                assert_eq!(u16::from(frame.code), 4403, "expected a 4403 close");
            }
            None | Some(Err(_)) | Some(Ok(ClientMessage::Close(None))) => {}
            other => panic!("expected the pairing connection to close, got {other:?}"),
        }
    }

    /// Critical 1:`device/revoke` 必须**真的**断开活着的 socket,并让 token 立即
    /// 失效(spec §3/§6)。
    ///
    /// `device/revoke` 是 Admin 门禁内的 RPC,而新配对设备恒为 `Control`,故本测试
    /// 用 `seed_device` 直接铸一台 Admin 设备来驱动它。回归:在此之前 `revoke` 只
    /// 摘 hub 注册,读循环与之无关、照旧吃帧,被撤销的手机仍能以全 scope 发
    /// `turn/start`。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_revoked_device_is_disconnected_and_its_token_invalidated() {
        let WsFixture {
            addr,
            token: admin_token,
            pairing,
            handle,
            ..
        } = spawn_ws_with_token_for(
            crate::server::tests_support::test_config(),
            crate::protocol::Scope::Admin,
        )
        .await;
        // 桌面侧:一台 Admin 连接,经它发起 revoke。
        let mut admin = connect_authed(addr, &admin_token).await;
        initialize(&mut admin).await;

        // 被撤销的手机:一台 Control 设备,先正常连上并握手。
        let (victim_device, victim_token) =
            pairing.seed_device("victim-phone", crate::protocol::Scope::Control);
        let mut victim = connect_authed(addr, &victim_token).await;
        initialize(&mut victim).await;

        // 撤销它。
        send_json(
            &mut admin,
            &format!(
                r#"{{"jsonrpc":"2.0","id":9,"method":"device/revoke","params":{{"device_id":"{}"}}}}"#,
                victim_device.id
            ),
        )
        .await;
        let ack = loop {
            let v = recv_json(&mut admin).await;
            if v.get("id") == Some(&serde_json::json!(9)) {
                break v;
            }
        };
        assert_eq!(ack["result"]["revoked"], true, "revoke must ack: {ack}");

        // 被撤销设备的 socket 必须在有界时间内关闭(读循环的 disconnected tick)。
        let closed = tokio::time::timeout(std::time::Duration::from_secs(3), victim.next()).await;
        assert!(
            matches!(
                closed,
                Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(ClientMessage::Close(_))))
            ),
            "a revoked device must be disconnected, got {closed:?}"
        );

        // token 立即失效:拿旧 token 重连会被 4401 拒绝。
        assert_closed_with_4401(addr, &victim_token).await;
        handle.abort();
    }

    /// Critical 2:一个已离开的客户端(断连、被撤销或被广播背压丢弃)不得掀翻
    /// **共享**主循环。
    ///
    /// 回归:`serve` 的入站循环对任何帧都先为它写响应,`write_response(..)?` 曾把
    /// 对已注销客户端的 `Closed` 变成致命错误——一个坏掉的同伴会连带杀死所有其它
    /// 客户端(以及后续连接)。本测试经 `assert!(handle.is_finished())` 断言 run
    /// 循环**没有**在客户端离开后终止;写响应的非致命语义本身由
    /// `server::tests::write_response_to_a_gone_ws_client_is_not_fatal` 精确钉死。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_leaving_does_not_kill_the_shared_loop() {
        let WsFixture {
            addr,
            token,
            handle,
            ..
        } = spawn_ws_with_token(crate::server::tests_support::test_config()).await;
        let mut gone = connect_authed(addr, &token).await;
        initialize(&mut gone).await;
        let mut live = connect_authed(addr, &token).await;
        initialize(&mut live).await;

        // `gone` 关闭连接:服务端读到 Close/EOF 即摘除它。此刻它可能仍有一帧在
        // 共享 channel 里排队——那正是早退守卫要丢掉的。
        gone.close(None).await.ok();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // 服务器必须仍然存活:`live` 仍被正常服务,且 main loop 没结束。
        send_json(
            &mut live,
            r#"{"jsonrpc":"2.0","id":7,"method":"config/read","params":{}}"#,
        )
        .await;
        let v = recv_json(&mut live).await;
        assert_eq!(v["id"], 7, "the surviving client must still be served: {v}");
        assert!(
            !handle.is_finished(),
            "the shared main loop must survive one client leaving"
        );
        handle.abort();
    }

    /// `teardown_ws_client` 必须把该连接从**所有**共享表里摘干净:hub 注册、scope、
    /// 以及主循环在 `initialize` 时懒插入的 `initialized`,还有 `device_id → ClientId`
    /// 映射。回归:`initialized` 曾由主循环私有持有,连接可来可去、表只增不减。
    #[tokio::test]
    async fn teardown_clears_scope_initialized_and_device_registries() {
        let hub = Arc::new(Broadcaster::new());
        let id = ClientId::ws(uuid::Uuid::new_v4());
        let _rx = hub.register(id.clone());
        let scopes: ClientScopes = Arc::new(Mutex::new(HashMap::new()));
        let initialized: ClientInitialized = Arc::new(Mutex::new(HashMap::new()));
        let devices: crate::server::WsDeviceRegistry =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        scopes
            .lock()
            .await
            .insert(id.clone(), crate::protocol::Scope::Control);
        initialized.lock().await.insert(id.clone(), true);
        devices
            .lock()
            .unwrap()
            .insert("dev-x".to_string(), id.clone());

        teardown_ws_client(&hub, &id, &scopes, &initialized, &devices, "dev-x").await;

        assert!(!hub.is_connected(&id), "hub registration must be gone");
        assert!(
            scopes.lock().await.get(&id).is_none(),
            "scope entry must be gone"
        );
        assert!(
            initialized.lock().await.get(&id).is_none(),
            "initialized entry must be gone"
        );
        assert!(
            devices.lock().unwrap().get("dev-x").is_none(),
            "device mapping must be gone"
        );
    }

    /// 多客户端 + 审批先到先得(Tier 1 的端到端闭环)。
    ///
    /// A 与 B 都认证连接、都 initialize;A 发起的 turn 触发一次需审批的工具调用。
    /// 反向 `item/toolCall/requestApproval` 必须**广播**给**两端**(不再只发给发起
    /// 方);A 先答 allow 即生效,B 后答是 no-op 成功——B 不该收到任何 error,且
    /// 两端都应收到 `item/toolCall/approvalResolved`,turn 正常结束。
    #[tokio::test(flavor = "multi_thread")]
    async fn two_clients_both_see_the_turn_and_only_the_first_approval_counts() {
        let WsFixture {
            addr,
            token,
            handle,
            ..
        } = spawn_ws_permission(crate::server::tests_support::test_config()).await;
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
    async fn spawn_ws_permission(cfg: RuntimeConfig) -> WsFixture {
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
        let (_device, token) = pairing.seed_device("test-phone", crate::protocol::Scope::Control);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(crate::server::tests::serve_ws_with_permission_agent(
            listener,
            cfg,
            workspaces,
            Arc::clone(&pairing),
        ));
        WsFixture {
            addr,
            token,
            pairing,
            handle,
        }
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
