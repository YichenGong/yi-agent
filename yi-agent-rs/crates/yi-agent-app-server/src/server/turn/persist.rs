//! 轮次持久化：把这一轮的结果落盘、收尾、写 checkpoint。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 6）。

use std::sync::Arc;

// 这些项仍由 `server.rs`（或其子模块经其 re-export）提供。
use crate::protocol::ThreadStatus;
use crate::server::{
    PartialTurnInput, TurnEvent, completed_items_agent_text, finished_event, opening_user_item,
    record_last_item_id, update_status,
};
use tokio::sync::mpsc;

/// 组装当前 turn 的 checkpoint：已 finalize 的 item（含开头的用户提问）+ 当前
/// 上下文快照。进行中的流式文本不在 `completed_items` 里，故天然不入内。
pub(crate) fn build_partial(
    input: PartialTurnInput<'_>,
    completed_items: &[crate::protocol::Item],
    messages: Vec<yi_agent_core::Message>,
    usage: Option<crate::thread_store::TurnUsage>,
    usage_total: Option<crate::thread_store::TurnUsage>,
) -> crate::thread_store::PartialTurn {
    let mut items = Vec::with_capacity(completed_items.len() + 1);
    items.push(opening_user_item(
        input.turn_id,
        input.user_prompt,
        input.user_attachments.to_vec(),
        input.user_image_refs.to_vec(),
    ));
    items.extend(completed_items.iter().cloned());
    crate::thread_store::PartialTurn {
        turn_id: input.turn_id.to_string(),
        items,
        messages,
        usage,
        usage_total,
    }
}
/// 尽力写一次 checkpoint：失败只记 stderr，绝不打断 turn。
pub(crate) fn checkpoint(
    store: &crate::thread_store::ThreadStore,
    thread_id: &str,
    partial: crate::thread_store::PartialTurn,
) {
    if let Err(e) = store.write_partial(thread_id, &partial) {
        eprintln!("[app-server] failed to write turn checkpoint ({thread_id}): {e}");
    }
}
/// 一个 turn 结束后（或被 clear / compact 改动后）的收尾：把当前 session 快照
/// 落盘、置 Idle，并在 `turn_id` 为 `Some` 时上报 Finished。
///
/// clear / compact 之后必须调用它：`/clear` 要落一条空快照（否则 resume 回放的是
/// 旧日志），`/compact` 要把压缩结果写回 `.jsonl`。此时**没有 turn 在收尾**，
/// 传 `turn_id = None`：不发 `TurnEvent::Finished`，也不 touch meta。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn persist_and_finish_turn(
    thread_id: &str,
    turn_id: Option<&str>,
    // 随 `turn/completed` 广播出去的那个完成时刻。落盘必须复用它而不是重新取一次
    // 墙钟：广播发生在落盘之前，各取一次会让落盘值更晚，另一端的客户端收到广播
    // 立刻 markRead 时写下的已读水位就盖不住它，重连后蓝点复活。
    completed_at: i64,
    user_prompt: Option<&str>,
    user_attachments: &[crate::protocol::Attachment],
    user_image_refs: &[crate::protocol::ImageRef],
    // 本轮若替某条阻塞式 `ask_lead` 跑,这里是它的 request_id;随 Finished 上报,
    // 主循环据此只回填那一个等待方(见 `settle_pending_lead_replies`)。
    lead_request_id: Option<String>,
    agent: &yi_agent_core::Agent,
    completed_items: Vec<crate::protocol::Item>,
    last_usage: Option<crate::thread_store::TurnUsage>,
    // 本轮所有 provider 调用的累加值（`last_usage` 只是最后一次调用的快照）。
    usage_total: Option<crate::thread_store::TurnUsage>,
    store: &crate::thread_store::ThreadStore,
    hub: &crate::broadcast::Broadcaster,
    turn_tx: &mpsc::Sender<TurnEvent>,
    status: &Arc<std::sync::Mutex<ThreadStatus>>,
) {
    let mut items = Vec::with_capacity(completed_items.len() + 1);
    if let Some(prompt) = user_prompt {
        // 基线 server 不 emit userMessage，必须在落盘时补齐，否则 resume 会丢用户提问。
        items.push(crate::protocol::Item::UserMessage {
            id: format!("user-{}", turn_id.unwrap_or("session-command")),
            text: prompt.to_string(),
            attachments: user_attachments.to_vec(),
            images: user_image_refs.to_vec(),
        });
    }
    items.extend(completed_items);

    // 先取助手侧材料：`items` 接下来会 move 进 `record`，此后无法再借用。
    let assistant_text = completed_items_agent_text(&items);
    // 落盘副本：关闭跨轮保留（`preserve_thinking == false`）时剥离思考块，使磁盘
    // 与「已放弃跨轮保留」的会话状态一致——否则每轮追加的日志行仍携带刚产出的
    // 凭证，重开会话即「复活」（spec §4.1/§7.4）。session 本体（展示与请求的权威）
    // 不受影响：这里只剥**落盘副本**，与请求路径的剥离互不干扰。
    // 默认 `true` 时逐字取会话内容 ⇒ 与今日落盘完全一致（零回归）。
    let persisted_messages = if agent.config().preserve_thinking {
        agent.session().messages().to_vec()
    } else {
        yi_agent_core::strip_thinking(agent.session().messages())
    };
    let record = crate::thread_store::TurnLine::Turn {
        items,
        usage: last_usage,
        usage_total,
        messages: persisted_messages,
    };
    // append 失败则跳过 meta 维护：避免出现"幽灵" thread。
    if let Err(e) = store.append_turn(thread_id, &record) {
        eprintln!("[app-server] failed to persist turn {thread_id}: {e}");
    } else {
        // `last_item_id` 供 `thread/listAll` 报告"客户端是否落后"的**保守下界**；
        // 「锚点是否仍在」的权威判据是读真实 jsonl 的 `contains_item_id`（spec §3）。
        //
        // 只有本次落盘确实产出了 item 才更新游标。空 items 的落盘（`compact` /
        // `SetPreserveThinking`）不改变可见内容集合，写 `None` 会把「有 items」
        // 误报成「从未有过 item」，从而永久削弱 `listAll` 的「是否落后」信号。
        // 真正清空内容的 `clear` 路径在它的调用点显式清游标（见下）。
        if let Some(last) = record_last_item_id(&record)
            && let Err(e) = store.set_last_item_id(thread_id, Some(&last))
        {
            eprintln!("[app-server] failed to record last_item_id for {thread_id}: {e}");
        }
        if let Some(prompt) = user_prompt {
            // 标题已在 turn 开始时（`run_thread_driver` 收到输入后）由
            // `title_on_first_input` 写入并尝试用 LLM 标题覆盖；这里只负责刷新
            // `updated_at`，返回值不再有意义。
            if let Err(e) = store.touch(thread_id, Some(prompt)) {
                eprintln!("[app-server] failed to update meta for {thread_id}: {e}");
            }
        }
    }

    // 记下**回合结束**时刻：未读由 `last_completed_at > last_read_at` 派生，而
    // `updated_at` 在回合**开始**时也会被 `touch` 刷新，不能兼任这个职责（否则
    // 另一端刚发问、还在跑的会话会立刻亮蓝点）。
    //
    // 刻意放在 append 成功/失败**之外**：两者是独立失败。日志写不进去只说明内容
    // 没存下来，回合本身确确实实结束了、用户也在流里看到了输出。把这条记账绑在
    // append 成功上，这一轮就会在未读口径里凭空消失——另一端重连后毫无「有新内容」
    // 的提示，直到下一轮跑完。
    //
    // 只在 `turn_id.is_some()` 时写：`clear` / `compact` / `SetPreserveThinking`
    // 收尾传 `None`，它们不是回合结束，记下去会让刚清空上下文的会话凭空亮蓝点。
    if turn_id.is_some()
        && let Err(e) = store.mark_turn_completed(thread_id, completed_at)
    {
        eprintln!("[app-server] failed to record turn completion for {thread_id}: {e}");
    }

    // 广播「本轮已落盘」：delete/interrupt 路径靠这枚 Finished 判定落盘完成。
    // 标题生成已前移到 turn 开始处，收尾路径不再有任何可等待的模型调用。
    let _ = update_status(hub, status, thread_id, ThreadStatus::Idle).await;
    if let Some(turn_id) = turn_id {
        // 回填给等待方的回复:与标题材料同一份文本,故这里复用它而不是再抽一次
        // (extraction 只有一条口径,见 `lead_reply_text`)。身份与文本一起上报:
        // 收口方只认这一个 request_id(见 `settle_pending_lead_replies`)。
        let _ = turn_tx
            .send(finished_event(
                thread_id,
                turn_id,
                assistant_text.clone(),
                lead_request_id,
            ))
            .await;
    }
}
