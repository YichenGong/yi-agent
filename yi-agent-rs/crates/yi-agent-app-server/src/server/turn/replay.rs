//! 轮次重放与收尾：分块回放、结算未完成的子代理回复、客户端响应路由。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 6）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

// 这些项仍由 `server.rs`（或其子模块经其 re-export）提供。
use crate::protocol::{ClientResponse, RequestId};
use crate::server::{TurnEvent, parse_client_decision};
use crate::session::ThreadSession;
use crate::workspace_index::WorkspaceIndex;
use tokio::sync::{Mutex, mpsc, oneshot};
use yi_agent_core::permission::Decision;

/// 回放分块的字节软预算（256 KiB）。单帧序列化后须**远小于**
/// `MAX_FRAME_BYTES = 1 MiB`,故留足余量。
pub(crate) const REPLAY_CHUNK_BYTES: usize = 256 * 1024;
/// 回放分块的条数上限，兜住「海量极小 item」把帧数压不下来的极端。
pub(crate) const REPLAY_CHUNK_MAX_ITEMS: usize = 200;
/// 把历史 items 切成回放帧的块：顺序保持；累计序列化字节超过
/// [`REPLAY_CHUNK_BYTES`] 或条数达到 [`REPLAY_CHUNK_MAX_ITEMS`] 即切块。
/// 单条自身超预算时它单独成块（容积为 1），不在此函数内再切分。
pub(crate) fn chunk_items_for_replay(
    items: Vec<crate::protocol::Item>,
) -> Vec<Vec<crate::protocol::Item>> {
    let mut chunks: Vec<Vec<crate::protocol::Item>> = Vec::new();
    let mut cur: Vec<crate::protocol::Item> = Vec::new();
    let mut cur_bytes = 0usize;
    for item in items {
        let approx = serde_json::to_vec(&item).map(|v| v.len()).unwrap_or(64);
        // 字节预算：当前块非空且再加一条会超预算 → 先结块。
        if !cur.is_empty() && cur_bytes + approx > REPLAY_CHUNK_BYTES {
            chunks.push(std::mem::take(&mut cur));
            cur_bytes = 0;
        }
        cur.push(item);
        cur_bytes += approx;
        // 条数上限：达到上限即结块（与字节预算互为兜底）。
        if cur.len() >= REPLAY_CHUNK_MAX_ITEMS {
            chunks.push(std::mem::take(&mut cur));
            cur_bytes = 0;
        }
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}
/// 把客户端对反向请求的响应路由到等待中的 driver。
///
/// 返回 `Some(decision)` 表示本次响应**首次**命中;`None` 表示该审批已被别的
/// 客户端处理(或根本不存在)。后者不是错误:双端同时点"允许"时,后到者是
/// no-op 成功,而不是报错——报错会让另一端弹出一个无意义的失败提示。
pub(crate) async fn route_client_response(
    resp: ClientResponse,
    pending: &Mutex<HashMap<String, oneshot::Sender<Decision>>>,
) -> Option<(String, Decision)> {
    let RequestId::Str(key) = resp.id else {
        tracing::warn!("ignoring client response with non-string id");
        return None;
    };
    let Some(tx) = pending.lock().await.remove(&key) else {
        // 已被先到的应答取走:安静地当作 no-op,不 warn(双答是预期场景)。
        tracing::debug!("approval {key} was already resolved");
        return None;
    };
    let decision = parse_client_decision(resp.result.as_ref());
    let _ = tx.send(decision.clone());
    Some((key, decision))
}
/// 构造一个 `TurnEvent::Finished`,避免在 driver 里重复拼字段。
///
/// `reply` 是本轮的助手回复文本(见 [`lead_reply_text`])。收尾路径传实际取出的
/// 文本;**中途放弃**的路径(run 失败、写帧失败、客户端断连)传 `None`:那些轮
/// 没有「组长说了什么」可言,伪造一条空回复会让等待方以为得到了答复。
///
/// `lead_request_id` 是本轮替哪条阻塞式 `ask_lead` 跑的。**只有**由阻塞投递产生的
/// 那一轮带得出来(见 `deliver_to_lead`),其余构造点一律 `None`:它们产出的轮不能
/// 回填任何等待方。
pub(crate) fn finished_event(
    thread_id: &str,
    turn_id: &str,
    reply: Option<String>,
    lead_request_id: Option<String>,
) -> TurnEvent {
    TurnEvent::Finished {
        thread_id: thread_id.to_string(),
        turn_id: turn_id.to_string(),
        reply,
        lead_request_id,
    }
}
/// 把某个 thread 上**这一轮**负责的那个等待方收口。
///
/// 只回填事件自己携带的那一个 `request_id`,并从待回填队列里**只**摘掉它(队列空
/// 了就把该 lead 的条目整个 drop)。
///
/// 为什么不能整条队列一次取走:`pending_lead_replies` 是一对多的(同一个组长可能
/// 同时被多个会话阻塞等待),而组长**一轮只回答一个提问**。整条取走就意味着把那
/// 一轮的文本同时回填给排队的每一个人——第二个提问人收到的是第一个的答案。用户
/// 直接往组长会话里打字也会产生一轮(`lead_request_id == None`),那种轮若参与
/// 回填,就是把用户的对话内容当成对某个等待方的答复。两种错配都在此排除:
/// **只**认本轮携带的身份,`None` 则谁都不回填。
///
/// 不变量:因本轮而产生的那个 id 已在投递**之前**入账(见主循环的 `lead_rx` 分支),
/// 故这里必然能摘到它;摘不到说明本轮没有任何等待方(或是用户轮),回填是 no-op。
///
/// `reply` 为 `None`(失败轮/纯工具轮/中途放弃)时只摘账、不回填:伪造一条空回复
/// 会让等待方以为组长答复了「空话」。
pub(crate) fn settle_pending_lead_replies(
    thread_id: &str,
    lead_request_id: Option<String>,
    reply: Option<String>,
    pending: &mut HashMap<String, Vec<String>>,
    replies: &crate::lead::LeadReplies,
) {
    // 本轮不替任何等待方跑(用户 turn / 看板卡 / 单向投递):队列**保持原样**,
    // 排队的人继续等它自己那一轮。
    let Some(request_id) = lead_request_id else {
        return;
    };
    if let Some(ids) = pending.get_mut(thread_id) {
        // 只摘这一个。该 id 一定在队列里(投递前入账),`retain` 比按位找更抗
        // 重复投递:即便同一 id 意外入账两次也一并摘干净。
        ids.retain(|id| id != &request_id);
        if ids.is_empty() {
            // 队列空了就把条目整个 drop,免得这张表随会话数无限长胖。
            pending.remove(thread_id);
        }
    }
    if let Some(reply) = reply {
        // `false` = 等待方已超时(drop 了接收端)或从未登记:不是错误,无需日志噪音。
        let _ = replies.resolve(&request_id, reply);
    }
}
/// A held pause prevents any old-driver write while lifecycle code touches storage.
/// Early returns resume the original session. Success explicitly stops its driver.
pub(crate) struct PausedWriter(Option<oneshot::Sender<bool>>);
impl PausedWriter {
    pub(crate) fn stop(mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(true);
        }
    }
}
impl Drop for PausedWriter {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(false);
        }
    }
}

/// Interrupt, settle matching execution, then pause the writer at its idle boundary.
/// Timeouts, closed channels and unsaved content refuse mutation, retaining the session.
pub(crate) async fn interrupt_and_wait_for_persist(
    threads: &mut HashMap<String, ThreadSession>,
    turn_rx: &mut mpsc::Receiver<TurnEvent>,
    thread_id: &str,
    pending_lead_replies: &mut HashMap<String, Vec<String>>,
    lead_replies: &crate::lead::LeadReplies,
) -> Result<Option<PausedWriter>, String> {
    if !threads.contains_key(thread_id) {
        return Ok(None);
    }
    let barrier = async {
        if let Some(tid) = threads
            .get(thread_id)
            .and_then(|s| s.active_turn_id.clone())
        {
            if let Some(s) = threads.get(thread_id) {
                let _ = s.interrupt_tx.try_send(tid.clone());
            }
            loop {
                let Some(TurnEvent::Finished {
                    thread_id: done_id,
                    turn_id: done_turn,
                    reply,
                    lead_request_id,
                }) = turn_rx.recv().await
                else {
                    return Err(
                        "completion stream closed before the writer settled; retry later".into(),
                    );
                };
                if let Some(s) = threads.get_mut(&done_id) {
                    s.finish_turn(&done_turn);
                }
                settle_pending_lead_replies(
                    &done_id,
                    lead_request_id,
                    reply,
                    pending_lead_replies,
                    lead_replies,
                );
                if done_id == thread_id && done_turn == tid {
                    break;
                }
            }
        }
        let session = threads.get(thread_id).ok_or("session disappeared")?;
        let (reply, answer) = oneshot::channel();
        session
            .session_tx
            .send(crate::session::SessionCommand::PauseWriter { reply })
            .await
            .map_err(|_| "writer unavailable; session retained".to_string())?;
        let control = answer
            .await
            .map_err(|_| "writer did not acknowledge pause; retry later".to_string())??;
        Ok(Some(PausedWriter(Some(control))))
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), barrier)
        .await
        .map_err(|_| {
            "timed out waiting for safe persistence; session retained, retry later".to_string()
        })?
}
/// 从本轮 items 里取第一条助手文本：作为回填给组长等待方的回复。
///
/// 空文本的 `AgentMessage` 视为不存在，避免把一段空回复当成组长真的说了话。
///
/// 标题生成已不再使用它：标题在用户输入落地时就生成，此刻助手还没有输出
/// （见 [`generate_title_for_first_input`]）。
pub(crate) fn completed_items_agent_text(items: &[crate::protocol::Item]) -> Option<String> {
    items.iter().find_map(|item| match item {
        crate::protocol::Item::AgentMessage { text, .. } if !text.trim().is_empty() => {
            Some(text.clone())
        }
        _ => None,
    })
}
/// 组长那轮要回填给等待方 thread 的文本。
///
/// 与落盘/标题生成**同一条口径**:直接复用 [`completed_items_agent_text`],不另立
/// 一套抽取逻辑。两处若各写一遍,「标题取的文本」与「回填给等待方的文本」会在
/// 今后任何一次口径变更(例如改成取最后一条)里悄悄分叉,而后者分叉的表现是
/// 等待方收到一段与组长实际回复无关的文字——比拿不到回复更难排查。
///
/// `None` = 本轮没有可回填的助手文本(失败轮、纯工具轮、写帧失败而中断的轮):
/// 调用方**不得**把它当成一条空回复回填,那会让等待方以为组长回了话。
pub(crate) fn lead_reply_text(items: &[crate::protocol::Item]) -> Option<String> {
    completed_items_agent_text(items)
}
/// 解析某主根当前关联的附属根：取索引里的记录，丢掉已不存在的目录。
///
/// 静默丢弃（而非报错）是有意的：附属根是能力增量，它消失不该让会话打不开；
/// `cwd` 本身已删仍照既有行为报错（`resolve_thread_cwd` / resume 的守卫），
/// 因为那是权威基点。
///
/// `WorkspaceIndex::children` 返回的已是 canonical 绝对路径（`link` 写入前
/// canonicalize 过），故此处无需再规范化。
///
/// 可见性:`thread/start` 的臂已搬进 [`crate::rpc::thread`],故与
/// [`resolve_thread_cwd`] 同样放宽到 `pub(crate)`(只放宽可见性,值/行为不变)。
pub(crate) fn linked_extra_roots(workspaces: &WorkspaceIndex, cwd: &str) -> Vec<PathBuf> {
    workspaces
        .children(Path::new(cwd))
        .into_iter()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .collect()
}
