//! 电脑侧**出站**中继客户端:把本地环回 app-server 与远端中继桥接起来。
//!
//! 拓扑(spec §2.4/§11.1):电脑不开放任何入站端口。`yi-agent app-server --relay`
//! 先在本机 `127.0.0.1:0` 起一个**普通** ws app-server,再由本模块:
//!
//! ```text
//!  手机 ──wss──▶ 中继 ◀──wss(出站)── 本模块 ──ws(环回)──▶ 本地 app-server
//! ```
//!
//! 两个连接**都是出站**:本模块作为 ws 客户端分别连中继与本地 app-server,在中继
//! 连接上定时发 Ping 保活(NAT/切网),断开后按指数退避重连。
//!
//! 本地 app-server **不弱化准入**:它仍是「无 token 即 4401」。`local_token` 由
//! `main.rs` 用与本地 server **共享**的 `PairingState` 现铸(等价于一台本机设备
//! 走一次正常配对),本模块只把它带进 `?token=`。
//!
//! 已知 v1 限制(诚实记录,见任务报告):桥接是**单条**本地 ws 连接,故本地
//! app-server 看到的 scope 恒为该 `local_token` 的 scope;多台手机经同一条中继
//! session 进来时,per-device scope 在本地这一层**不区分**。本机铸的 token 取
//! `Control`(与 spec §5.4「新配对设备默认 control」一致),因此经中继路径的
//! admin 类操作会被本地 app-server 正常拒绝——与手机直连时的行为一致。

use std::time::Duration;

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use url::Url;

use crate::backoff::{backoff_start, next_delay};

/// 中继连接上的保活 ping 间隔。要显著小于常见 NAT 的空闲超时(通常 60s+)。
pub const PING_INTERVAL: Duration = Duration::from_secs(30);

/// 把本地 app-server 与远端中继对接,直到进程退出。
///
/// - `relay_url`:中继的电脑侧端点,如 `wss://relay.example/connect`(本函数会补
///   `?session=<session_id>`);
/// - `app_server_ws`:本地环回 app-server 的 ws 基址,如 `ws://127.0.0.1:PORT/ws`
///   (本函数会补 `?token=<local_token>`);
/// - `session_id`:与手机侧约定的配对会话 id;
/// - `local_token`:`main.rs` 用共享 `PairingState` 现铸的本地设备 token。
///
/// 返回 `Err` 仅在**初始连接**失败时(如中继/本地 ws 不可达);连上之后的断线
/// 一律退避重连,不返回。
pub async fn run_client(
    relay_url: Url,
    app_server_ws: Url,
    session_id: String,
    local_token: String,
) -> Result<()> {
    let relay = with_query(relay_url, "session", &session_id);
    let local = with_query(app_server_ws, "token", &local_token);

    let mut delay = backoff_start();
    loop {
        match bridge_once(&relay, &local).await {
            Ok(()) => tracing::info!("relay bridge closed; reconnecting"),
            Err(e) => tracing::warn!(error = %e, "relay bridge error; reconnecting"),
        }
        tokio::time::sleep(delay).await;
        delay = next_delay(delay);
    }
}

/// 追加一个查询参数(保留已有查询串)。
fn with_query(mut url: Url, key: &str, value: &str) -> Url {
    url.query_pairs_mut().append_pair(key, value);
    url
}

/// 一次桥接:两端都连上后双向转发,任一方向结束即收尾。
async fn bridge_once(relay: &Url, local: &Url) -> Result<()> {
    let (relay_ws, _) = tokio_tungstenite::connect_async(relay.as_str())
        .await
        .context("connect relay")?;
    let (local_ws, _) = tokio_tungstenite::connect_async(local.as_str())
        .await
        .context("connect local app-server")?;
    tracing::info!(relay = %relay, "relay bridge up");

    let (mut relay_tx, mut relay_rx) = relay_ws.split();
    let (mut local_tx, mut local_rx) = local_ws.split();

    // 下行:中继 → 本地(手机发来的请求帧喂给 app-server)。
    let mut down = tokio::spawn(async move {
        while let Some(Ok(frame)) = relay_rx.next().await {
            if local_tx.send(frame).await.is_err() {
                break;
            }
        }
    });

    // 上行:本地 → 中继(app-server 的响应/通知发给手机);与保活 ping 交替。
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await; // interval 的首个 tick 立即触发,丢弃它。
    loop {
        tokio::select! {
            frame = local_rx.next() => match frame {
                Some(Ok(msg)) => {
                    if relay_tx.send(msg).await.is_err() { break; }
                }
                _ => break, // 本地 ws 关闭
            },
            _ = ping.tick() => {
                // 保活:手机切网、电脑睡眠唤醒后,靠它尽早发现死连接。
                if relay_tx.send(Message::Ping(Vec::new().into())).await.is_err() { break; }
            }
            _ = &mut down => break, // 中继侧读循环结束
        }
    }

    down.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_query_appends_and_preserves_existing_params() {
        let url = Url::parse("wss://relay.example/connect?foo=bar").unwrap();
        let url = with_query(url, "session", "s-1");
        assert_eq!(url.path(), "/connect");
        assert_eq!(url.query(), Some("foo=bar&session=s-1"));

        let url = Url::parse("ws://127.0.0.1:8790/ws").unwrap();
        let url = with_query(url, "token", "yia_x");
        assert_eq!(url.query(), Some("token=yia_x"));
    }
}
