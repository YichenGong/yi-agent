//! 子 agent / 进程 / trace 三类观察流的轮询与汇总。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 2）。

use crate::protocol::{Notification, SubagentTotals};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

// 这些项仍由 `server.rs`（或其子模块经其 re-export）提供；走 `crate::server::`
// 路径引用，避免后续搬移改变 `super::` 的含义。
use crate::server::{agent_trace_row, write_notification};

pub(crate) fn trace_rows(
    rows: Vec<yi_agent_store::ipc::IpcTraceRow>,
) -> Vec<crate::protocol::AgentTraceRow> {
    rows.into_iter().map(agent_trace_row).collect()
}
/// Push the conversation's child list whenever it changes.
///
/// The first frame is skipped: the caller has just answered the same question
/// synchronously, so pushing it again would be a duplicate. Every later
/// difference is pushed whole, because the list is small and a client that
/// applies it wholesale cannot drift out of order.
pub(crate) async fn watch_children(
    hub: Arc<crate::broadcast::Broadcaster>,
    thread_id: String,
    socket: PathBuf,
    initial: Vec<crate::protocol::AgentChild>,
    // 用量汇总的去处。用 `watch` 而非 `mpsc`：这里只关心最新值，且写端是独立
    // 任务，`mpsc` 满队列会让它卡住。
    subagent_tx: tokio::sync::watch::Sender<SubagentTotals>,
) {
    let mut last = initial;
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let tasks = match yi_agent_store::ipc::send_request(
            &socket,
            yi_agent_store::ipc::IpcRequest::ListTaskSummaries {
                session_id: None,
                active_only: false,
            },
        ) {
            Ok(yi_agent_store::ipc::IpcResponse::TaskSummaries { tasks }) => tasks,
            // A daemon that is briefly unavailable is not a change; the next
            // poll will pick the list back up.
            _ => continue,
        };
        let current = children_for_thread(&tasks, &thread_id);
        if current == last {
            continue;
        }
        last = current.clone();
        // 用量与列表同源（都来自这次 ListTaskSummaries），故在同一次推送里一起更新，
        // 客户端不会看到"列表已变、数字未变"的中间态。
        //
        // `send_replace` 永不失败、也不阻塞：没有 receiver（driver 已退出）时值仍
        // 写进共享槽，下一个 driver 起来能立刻读到最新值。
        subagent_tx.send_replace(subagent_totals(&current));
        let notification = Notification::AgentChildrenUpdated {
            thread_id: thread_id.clone(),
            children: current,
        };
        if write_notification(&hub, &notification).await.is_err() {
            return;
        }
    }
}
/// 把 manager 的状态广播转成 `process/updated` 通知。
///
/// 只在状态变化时推送(`Output` 丢弃):高频输出走 `process/read` 按需拉取。
/// 关闭 thread 时由调用方 abort。
pub(crate) async fn watch_processes(
    hub: Arc<crate::broadcast::Broadcaster>,
    thread_id: String,
    manager: Arc<yi_agent_tools::ProcessManager>,
    mut rx: tokio::sync::broadcast::Receiver<yi_agent_tools::ProcessEvent>,
) {
    use yi_agent_tools::ProcessEvent;
    loop {
        match rx.recv().await {
            Ok(event) => {
                let (process_id, state) = match event {
                    ProcessEvent::Started { process_id } => (process_id, "starting"),
                    ProcessEvent::Ready { process_id } => (process_id, "ready"),
                    ProcessEvent::Exited { process_id, .. } => (process_id, "exited"),
                    ProcessEvent::Killed { process_id } => (process_id, "killed"),
                    // 输出是高频流:不转发,详情页自己按需增量拉。
                    ProcessEvent::Output { .. } => continue,
                };
                let _ = manager.list(); // 保证条目仍在(仅作存在性自检,结果丢弃)
                let _ = write_notification(
                    &hub,
                    &Notification::ProcessUpdated {
                        thread_id: thread_id.clone(),
                        process_id,
                        state: state.to_string(),
                    },
                )
                .await;
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}
/// Forward a watched task's rows until the stream ends or the task is aborted.
///
/// The subscription is non-blocking so the task yields between polls; it ends on
/// its own when the daemon closes the stream, and is aborted when the client
/// unwatches, watches another task, or closes the thread.
pub(crate) async fn stream_trace(
    hub: Arc<crate::broadcast::Broadcaster>,
    thread_id: String,
    task_id: String,
    socket: PathBuf,
    after_id: i64,
) {
    let subscription = match yi_agent_store::ipc::subscribe_trace(
        &socket,
        after_id,
        std::slice::from_ref(&task_id),
        &[],
    ) {
        Ok(subscription) => subscription,
        Err(error) => {
            tracing::warn!(%error, %thread_id, %task_id, "trace watch could not subscribe");
            return;
        }
    };
    if let Err(error) = subscription.set_nonblocking(true) {
        tracing::warn!(%error, %thread_id, %task_id, "trace watch could not poll");
        return;
    }
    let mut subscription = subscription;
    loop {
        match subscription.try_row() {
            Ok(Some(row)) => {
                let notification = Notification::AgentTraceEvent {
                    thread_id: thread_id.clone(),
                    task_id: task_id.clone(),
                    row: agent_trace_row(row),
                };
                if write_notification(&hub, &notification).await.is_err() {
                    return;
                }
            }
            // Nothing yet, or the stream ended: either way, yield and retry.
            // The stream ending is indistinguishable from a quiet moment by
            // design, so the loop is bounded by the abort that the caller does.
            Ok(None) => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(error) => {
                tracing::warn!(%error, %thread_id, %task_id, "trace watch stream failed");
                return;
            }
        }
    }
}
/// The children of one conversation, derived from a task-summary snapshot.
///
/// A conversation is scoped by the `thread_id` the spawner bound into its tools,
/// not by the directory: several conversations can share one directory and one
/// attached root, so the marker is the only thing that separates their children.
/// Root tasks are never children, and the `thread_id` filter is exact, so an
/// untagged task belongs to no conversation rather than to all of them.
pub(crate) fn children_for_thread(
    tasks: &[yi_agent_store::ipc::IpcTaskSummary],
    thread_id: &str,
) -> Vec<crate::protocol::AgentChild> {
    tasks
        .iter()
        .filter(|task| !task.is_root)
        .filter(|task| task.thread_id.as_deref() == Some(thread_id))
        .map(|task| crate::protocol::AgentChild {
            task_id: task.task_id.clone(),
            // `ListTaskSummaries` carries no objective; a later source can fill
            // it without changing this shape.
            objective: None,
            state: task.state.clone(),
            last_step: None,
            parent_task_id: task.parent_task_id.clone(),
            // 用量随列表一起给：客户端因此不需要为每个子任务再问一次。
            usage: crate::protocol::AgentChildUsage {
                input_tokens: task.usage.input_tokens,
                output_tokens: task.usage.output_tokens,
                cache_read_tokens: task.usage.cache_read_tokens,
            },
        })
        .collect()
}
/// 一个会话里所有子任务的用量之和。
///
/// 对**整份列表**求和而不是只算运行中的：已完成的子任务烧掉的 token 同样是这个
/// 会话的真实开销，累计值必须包含它们。
pub(crate) fn subagent_totals(children: &[crate::protocol::AgentChild]) -> SubagentTotals {
    children
        .iter()
        .fold(SubagentTotals::default(), |mut acc, child| {
            acc.input = acc.input.saturating_add(child.usage.input_tokens);
            acc.output = acc.output.saturating_add(child.usage.output_tokens);
            acc.cache_read = acc.cache_read.saturating_add(child.usage.cache_read_tokens);
            acc
        })
}
