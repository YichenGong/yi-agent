//! JSON-RPC 响应/通知的写出，以及 WS 设备登记表。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 2）。被 `rpc/*` 的 18 个文件与
//! `server/tests/` 的 3 个文件引用，故 `server.rs` 必须完整 re-export。

use crate::protocol::{
    JSONRPC_VERSION, Notification, NotificationEnvelope, RequestId, ResponseEnvelope, RpcError,
};
use std::sync::Arc;
use std::sync::OnceLock;

// 这些项仍由 `server.rs`（或其子模块经其 re-export）提供；走 `crate::server::`
// 路径引用，避免后续搬移改变 `super::` 的含义。
use crate::server::{WsDeviceRegistry, is_load_bearing};

/// `image/read` 每次回传的原始字节上限。传输帧上限 1 MiB,base64 膨胀 4/3 后
/// 512 KiB 的原始块约 683 KiB,连同其它字段仍在单帧之内。
pub(crate) const IMAGE_READ_CHUNK_BYTES: u64 = 512 * 1024;
/// `image/read` 允许服务的最大文件字节数。分片只是传输形态,不代表可以无上限地
/// 服务任意大文件;与 `image_prep::MAX_IMAGE_BYTES` 同值(20 MiB)。
pub(crate) const IMAGE_READ_MAX_BYTES: u64 = 20 * 1024 * 1024;
/// Snapshot all connections authenticated as a device.
#[cfg(test)]
pub(crate) fn ws_clients_for_device(device_id: &str) -> Vec<crate::broadcast::ClientId> {
    let Some(registry) = device_registry().get() else {
        return Vec::new();
    };
    let guard = registry.lock().unwrap_or_else(|p| p.into_inner());
    guard
        .get(device_id)
        .map(|ids| ids.iter().cloned().collect())
        .unwrap_or_default()
}
/// Atomically remove every registered connection of a revoked device.
pub(crate) fn forget_ws_device(device_id: &str) -> Vec<crate::broadcast::ClientId> {
    let Some(registry) = device_registry().get() else {
        return Vec::new();
    };
    registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(device_id)
        .map(|ids| ids.into_iter().collect())
        .unwrap_or_default()
}
/// 进程级「设备 id → ws `ClientId`」句柄。
///
/// 用 `OnceLock` 而非 `RuntimeAttachments` 字段:它天然是**每进程一张表**
/// (一个 app-server 进程只有一套 ws 连接),且只在 ws 传输启动时被设置一次。
/// ws 起不来时它保持未设置,`device/revoke` 于是退化为「只改设备表、不摘连接」,
/// 在纯 stdio 场景下本就没有 ws 连接可摘。
pub(crate) static DEVICE_REGISTRY: OnceLock<WsDeviceRegistry> = OnceLock::new();
pub(crate) fn device_registry() -> &'static OnceLock<WsDeviceRegistry> {
    &DEVICE_REGISTRY
}
/// 由 ws 传输在启动时登记它的「设备 id → `ClientId`」表(幂等;重复调用保留
/// 第一张)。返回该表,便于 ws 侧直接持有同一份 `Arc`。
pub(crate) fn install_device_registry(registry: WsDeviceRegistry) -> WsDeviceRegistry {
    device_registry().set(Arc::clone(&registry)).ok();
    device_registry().get().cloned().unwrap_or(registry)
}
pub(crate) fn ok_response(id: RequestId, result: serde_json::Value) -> ResponseEnvelope {
    ResponseEnvelope {
        jsonrpc: Some(JSONRPC_VERSION.to_string()),
        id,
        result: Some(result),
        error: None,
    }
}
pub(crate) fn err_response(id: RequestId, error: RpcError) -> ResponseEnvelope {
    ResponseEnvelope {
        jsonrpc: Some(JSONRPC_VERSION.to_string()),
        id,
        result: None,
        error: Some(error),
    }
}
/// 让**已经 detach 出去**的任务回一帧响应。
///
/// 为什么不是直接调 `write_response`:detached 任务手里不能有 `AppState<'a>`
/// (它按请求借用主循环的局部量),所以这里只持有写出所需的两样 owned:`Arc<hub>`
/// 与 `ClientId`。信号与主循环内联应答完全同一条路径,故连接关闭、大小校验、
/// 慢消费者摘除等行为不变。
///
/// `send` 返回 `()` 是刻意的:任务已经与请求方脱钩,写失败是那条连接自己的问题,
/// 没有人可回报。失败记一行 debug——重连后客户端会重新对账。
#[derive(Clone)]
pub(crate) struct SideResponseSender {
    hub: Arc<crate::broadcast::Broadcaster>,
    client: crate::broadcast::ClientId,
}

impl SideResponseSender {
    pub(crate) fn new(
        hub: Arc<crate::broadcast::Broadcaster>,
        client: crate::broadcast::ClientId,
    ) -> Self {
        Self { hub, client }
    }

    pub(crate) async fn send<T: serde::Serialize>(
        &self,
        id: RequestId,
        result: Result<T, yi_agent_runtime::side_conversation::SideError>,
    ) {
        let response = match result {
            Ok(value) => match serde_json::to_value(&value) {
                Ok(value) => ok_response(id, value),
                Err(error) => err_response(
                    id,
                    RpcError::internal(format!("failed to serialize side response: {error}")),
                ),
            },
            Err(error) => err_response(id, RpcError::side(&error)),
        };
        if let Err(error) = write_response(&self.hub, &self.client, response).await {
            tracing::debug!(%error, "side response write failed");
        }
    }
}

pub(crate) async fn write_response(
    hub: &crate::broadcast::Broadcaster,
    client: &crate::broadcast::ClientId,
    resp: ResponseEnvelope,
) -> anyhow::Result<()> {
    let frame = serde_json::to_value(&resp)
        .map_err(|e| anyhow::anyhow!("failed to serialize response: {e}"))?;
    // 定向回复:`reply` 会 await 到入队成功,保留 stdio 改造前
    // 「写阻塞直到对端读」的语义。
    match hub.reply(client, frame).await {
        Ok(()) => Ok(()),
        // 写失败到**非本地**客户端(ws 手机)不得掀翻共享主循环:那是**多客户端
        // 共享**的一台服务器,一个被 `device/revoke` 踢掉、或被广播背压摘除
        // (慢消费者)的连接,不能让所有其它客户端(以及后续连接)一起陪葬。
        // 摘掉它、让本轮请求就此了结即可(它的读循环靠 `is_connected` 票 tick
        // 自行收尾)。
        Err(_) if *client != crate::broadcast::ClientId::local() => {
            tracing::warn!(
                client = client.as_str(),
                "app-server response write failed; dropping the ws client"
            );
            hub.unregister(client);
            Ok(())
        }
        // stdio 的 `local` 仍保留改造前的语义:出口泵已死,再写就是错,把错误
        // 抛出以结束会话(`driver_reports_finished_when_writer_fails` 钉死这条)。
        Err(_) => Err(anyhow::anyhow!("client {} disconnected", client.as_str())),
    }
}
pub(crate) async fn write_notification(
    hub: &crate::broadcast::Broadcaster,
    n: &Notification,
) -> anyhow::Result<()> {
    let frame = serde_json::to_value(NotificationEnvelope::new(n))
        .map_err(|e| anyhow::anyhow!("failed to serialize notification: {e}"))?;
    // S2：列表层/全局帧恒推（无键）；内容层按 thread 过滤。无订阅者时等价于全广播。
    let delivery = n.delivery();
    let key = match delivery {
        crate::protocol::Delivery::Content => n.thread_key(),
        crate::protocol::Delivery::List | crate::protocol::Delivery::Global => None,
    };
    if is_load_bearing(delivery) {
        // 状态机帧**绝不丢**：`turn/completed` 是前端 `turnActive` 唯一的清除来源，
        // 丢了就永久停在 Stop（服务端 `active_turn_id` 已清，之后每次点 Stop 都是
        // 空转，只能重启 App）。逐字流（内容层）恰恰会把出站队列灌满——bash 每
        // 4 KiB 输出一帧、桌面端又不订阅（见 `has_subscribed_clients`），所以这条
        // 路径上的 `try_send` 在真实使用中一定会失败。改为背压投递：队列满时等
        // 出口泵抽干，而不是丢掉这一帧。列表/全局层每个 turn 只有寥寥数帧，等待
        // 成本可以忽略；内容层仍走非阻塞路径，否则 bash 大量输出会把 driver 堵死。
        //
        // `Err(Closed)` 表示可靠客户端的出口泵已真死（`pump_stdout` 写失败并注销）。
        // 这不是本帧的问题：主循环随后的 `reply` 会拿到 `Closed` 并按既有致命语义
        // 收尾，故此处忽略即可。
        let _ = hub.broadcast_await(key, frame).await;
    } else if matches!(
        n,
        Notification::ItemStarted { .. }
            | Notification::ItemCompleted { .. }
            | Notification::ItemsCompleted { .. }
            | Notification::TurnStarted { .. }
            | Notification::InterjectionsReturned { .. }
            | Notification::TurnRetry { .. }
    ) {
        hub.broadcast_required(key, frame);
    } else {
        hub.broadcast_for(key, frame);
    }
    Ok(())
}
/// 与 [`write_notification`] 同款序列化与投递键，但走**带背压**的
/// [`crate::broadcast::Broadcaster::broadcast_await`]：本地 reliable 客户端等待
/// 入队，WebSocket 对短时满队列有界等待，持续停滞则断开重同步。
/// 用于历史回放这类「必须完整送达」的批量下发。
pub(crate) async fn write_notification_await(
    hub: &crate::broadcast::Broadcaster,
    n: &Notification,
) -> anyhow::Result<()> {
    let frame = serde_json::to_value(NotificationEnvelope::new(n))
        .map_err(|e| anyhow::anyhow!("failed to serialize notification: {e}"))?;
    let key = match n.delivery() {
        crate::protocol::Delivery::Content => n.thread_key(),
        crate::protocol::Delivery::List | crate::protocol::Delivery::Global => None,
    };
    hub.broadcast_await(key, frame)
        .await
        .map_err(|_| anyhow::anyhow!("replay client closed"))?;
    Ok(())
}
