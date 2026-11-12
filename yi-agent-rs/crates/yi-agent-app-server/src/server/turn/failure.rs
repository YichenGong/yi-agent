//! 启动期 turn 失败的**共享**收口。
//!
//! RPC（`start_turn_core`）、插件启动（`ServeLauncher::launch_inner`）与组长冷
//! 投递（`deliver_to_lead`）在 prepare 之后都可能失败。三处的判据必须逐字相同：
//!
//! - **回滚按身份**：只有 `active_turn_id` 仍然是**本次**的 turn_id 才清占位并
//!   恢复 Idle。无条件清零会把另一个正在跑的 turn 变成无主（`turn/cancel` 静默
//!   失效、`is_idle` 谎报空闲）。
//! - **已宣告才补完成通知**：`turn/started` 发出之后失败，必须补一条
//!   `turn/completed`（failed），否则界面永久停在「运行中」。`turn/started` 之前
//!   的失败不能编造开始/完成事件。
//! - **通知与落盘失败不阻止内存回滚**：广播失败是真实故障，但界面卡在运行中
//!   比丢一条日志更糟；原始错误仍向调用方传播。
//!
//! 这里**不**追加任何助手内容、不删除用户上传的文件、不把 Finished 投回队列。
//! 未用的 pending activation 留给后续首次成功投递。

use std::collections::HashMap;
use std::sync::Arc;

use crate::broadcast::Broadcaster;
use crate::protocol::{Notification, ThreadStatus};
use crate::server::{update_status, write_notification};
use crate::session::ThreadSession;

/// 若 `active_turn_id` 仍是 `turn_id`，清掉占位并把状态恢复为 `Idle`。
///
/// 返回 `true` 表示**本次**调用真正收回了自己的占位；`false` 表示该 turn 已被
/// 别的 turn 取代（或 thread 已不在表里），此时绝不能动它的状态。
pub(crate) async fn rollback_turn(
    threads: &mut HashMap<String, ThreadSession>,
    hub: &Broadcaster,
    thread_id: &str,
    turn_id: &str,
) -> bool {
    // 先改内存、拿到「属于本次」的判断，再从 session 里 clone 状态句柄：
    // `update_status` 是 async，不能持着对 `threads` 的借用等它。
    let Some(status) = threads.get_mut(thread_id).and_then(|session| {
        if session.finish_turn(turn_id) {
            Some(Arc::clone(&session.status))
        } else {
            None
        }
    }) else {
        return false;
    };
    // 状态句柄取自**这一个** session，而不是调用方手里可能已过期的副本。
    let _ = update_status(hub, &status, thread_id, ThreadStatus::Idle).await;
    true
}

/// 已经宣告过 `turn/started` 的这一轮失败了：一次 `completed_at` 同时用于
/// 完成水位与 `turn/completed`，再按身份回滚。
///
/// 顺序是刻意的：先落完成水位（best effort）→ 广播 failed → 回滚。广播失败或
/// 落盘失败都只记日志，**不跳过**回滚。
pub(crate) async fn fail_announced_turn(
    threads: &mut HashMap<String, ThreadSession>,
    hub: &Broadcaster,
    thread_id: &str,
    turn_id: &str,
    error: &str,
) {
    // 与 `persist.rs` 同口径：广播自带权威时间戳，落盘取同一个值，客户端按 max
    // 合入才收敛（另取一次 now 会让已读水位低于完成水位，蓝点复活）。
    let completed_at = crate::thread_store::now_millis();
    if let Some(store) = threads
        .get(thread_id)
        .map(|session| Arc::clone(&session.store))
        && let Err(error) = store.mark_turn_completed(thread_id, completed_at)
    {
        tracing::warn!(
            %thread_id,
            %turn_id,
            error = %error,
            "failed to record the completion watermark for a failed turn",
        );
    }
    write_notification(
        hub,
        &Notification::TurnCompleted {
            thread_id: thread_id.to_string(),
            turn_id: turn_id.to_string(),
            status: crate::protocol::TurnStatus::Failed,
            error: Some(error.to_string()),
            completed_at: Some(completed_at),
        },
    )
    .await
    .map_or_else(
        |write_error| {
            tracing::warn!(
                %thread_id,
                %turn_id,
                error = %write_error,
                "failed to announce the failed turn; rolling the placeholder back anyway",
            );
        },
        |()| {},
    );
    rollback_turn(threads, hub, thread_id, turn_id).await;
}
