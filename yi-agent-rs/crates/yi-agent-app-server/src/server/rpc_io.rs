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
/// 把一个设备 id 映射到它当前的 ws `ClientId`——设备被撤销时据此摘除连接。
///
/// 这张表在 **ws 传输内部**维护(`ws.rs`):每个已认证的连接把自己
/// `device_id → ClientId` 登记进去,断连即摘除。主循环(本文件)不能直接持有
/// 它——`ws.rs` 依赖 `server.rs`,反向依赖会成环——故 ws 层在启动时把
/// `Arc<Mutex<HashMap<String, ClientId>>>` 交给本函数要读的进程级句柄
/// ([`install_device_registry`])。表为空(纯 stdio、无 ws,或尚未起 ws)时
/// 安全返回 `None`。
pub(crate) fn ws_client_for_device(device_id: &str) -> Option<crate::broadcast::ClientId> {
    let registry = device_registry().get()?;
    let guard = registry.lock().unwrap_or_else(|p| p.into_inner());
    guard.get(device_id).cloned()
}
/// 取出并移除某设备 id 的连接映射(撤销时用)。
///
/// 与 [`ws_client_for_device`] 同一张表,但顺带摘掉条目:设备已撤销,它的
/// `device_id → ClientId` 映射不该留在全局表里等下一个同名设备(或迟到的清理)
/// 撞上。真正的连接关闭由调用方 `hub.unregister` + ws 读循环的 disconnected
/// tick 完成;这里只负责让主循环侧不再记得它。
pub(crate) fn forget_ws_device(device_id: &str) -> Option<crate::broadcast::ClientId> {
    let registry = device_registry().get()?;
    let mut guard = registry.lock().unwrap_or_else(|p| p.into_inner());
    guard.remove(device_id)
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
    } else {
        hub.broadcast_for(key, frame);
    }
    Ok(())
}
/// 与 [`write_notification`] 同款序列化与投递键，但走**带背压**的
/// [`crate::broadcast::Broadcaster::broadcast_await`]：本地 reliable 客户端队列
/// 满时等待而非丢帧。用于历史回放这类「必须完整送达」的批量下发。
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
