//! 拆除类会话操作：`thread/delete`、`thread/fork`、`thread/clear`（wave2 C3d）。
//!
//! 从 `rpc/thread/mod.rs` 的 `handle()` 逐字搬出。

use crate::protocol::{RequestEnvelope, RequestId, RpcError, Scope, ThreadStatus};
use crate::rpc::{AppState, Flow};
use crate::server::{
    ThreadCancelOutcome, cancel_thread_children, detach_unused_runtimes, err_response,
    interrupt_and_wait_for_persist, ok_response, require_thread_id, store_lookup, thread_cwd,
    worktree_json, write_response,
};
use crate::session::SessionCommand;
use serde_json::json;
use std::path::Path;
use tokio::sync::oneshot;

pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "thread/delete" => {
            // 与 `thread/rename` / `thread/setModel` 同档:Control 起。
            // Observe 是掩码级只读视图,不得删除会话。
            if state.client_scope < Scope::Control {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::insufficient_scope(Scope::Control)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            // 优先用内存中该 thread 的权威 store(与其 driver 共享 lock),
            // 冷 thread 才按全局索引 / cfg.workdir 定位;存在性与删除都
            // 作用在该 thread 的真实目录上。
            let thread_store = store_lookup(state.threads, state.workspaces, state.cfg, &thread_id);
            let in_memory = state.threads.contains_key(&thread_id);
            let on_disk = thread_store.exists(&thread_id);
            if !in_memory && !on_disk {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::unknown_thread(&thread_id)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            // 删除前先问一句:这个会话还有子代理在跑吗?要问在中断 turn
            // 之前——被拒的前提是不该动它分毫。子代理跑在项目 daemon 里,
            // 对话文件不是它的权威,只删文件会把它留在无人可见的地方继续跑。
            // force=true 才真的回收,由客户端在用户确认后携带。
            let force = req
                .params
                .get("force")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            // 卡片会话的 worktree 判定：只读会话自带 meta，不碰插件。
            // 顺手把 driver_project 拷到局部变量：会话文件随后即被删除，
            // 到执行删除时再读只会失败，故必须在此刻留下。
            //
            // 用 `read_meta` 而不是 `load`：这里只要 meta 的三个字段，
            // 而 `load` 会把整份对话 jsonl 解析一遍（主循环上的无谓阻塞 IO，
            // 与 `thread/diff/read` 同类）。计数语义不变：`load` 在 meta 缺失
            // 时走 `rebuild_meta`，其对 `driver_ref` / `driver_project` 都填
            // `None`，与 `read_meta` 返回 `None` 落到同一分支。
            let mut driver_project: Option<String> = None;
            let reclaim = match thread_store.read_meta(&thread_id) {
                Ok(Some(meta)) => {
                    driver_project = meta.driver_project.clone();
                    crate::worktree_reclaim::decide(
                        meta.driver_ref.as_deref(),
                        meta.driver_project.as_deref(),
                        Path::new(&meta.cwd),
                    )
                }
                _ => crate::worktree_reclaim::WorktreeReclaim::None,
            };
            // 「需确认」= 有活跃子代理（既有）或 worktree 处置为破坏性/硬保留（新）。
            let children = if force {
                None
            } else {
                match cancel_thread_children(state.runtimes, state.threads, &thread_id, false) {
                    Ok(ThreadCancelOutcome::NeedsConfirmation(count)) => Some(count),
                    // 无事可取消时按 0 处理，交给 worktree 判定决定是否确认。
                    Ok(ThreadCancelOutcome::Cancelled(_)) => Some(0),
                    // 查询失败（如会话不在内存）按 0 处理，但照旧记日志——与既有行为一致。
                    Err(cause) => {
                        eprintln!(
                            "[app-server] could not cancel subagents for {thread_id}: {cause}"
                        );
                        Some(0)
                    }
                }
            };
            let worktree_needs_confirm = matches!(
                reclaim,
                crate::worktree_reclaim::WorktreeReclaim::DestructiveDelete { .. }
                    | crate::worktree_reclaim::WorktreeReclaim::Keep { .. }
            );
            if !force && (children.unwrap_or(0) > 0 || worktree_needs_confirm) {
                let mut payload = json!({
                    "status": "needs_confirmation",
                    "active_children": children.unwrap_or(0),
                });
                if let Some(worktree) = worktree_json(&reclaim) {
                    payload["worktree"] = worktree;
                }
                write_response(state.hub, state.client, ok_response(id, payload)).await?;
                return Ok(Flow::Continue);
            }
            if force {
                // force 下真的回收子代理（既有语义）。
                match cancel_thread_children(state.runtimes, state.threads, &thread_id, true) {
                    Ok(ThreadCancelOutcome::Cancelled(cancelled)) if cancelled > 0 => {
                        eprintln!(
                            "[app-server] cancelled {cancelled} subagent task(s) for {thread_id}"
                        );
                    }
                    Err(cause) => eprintln!(
                        "[app-server] could not cancel subagents for {thread_id}: {cause}"
                    ),
                    _ => {}
                }
            }
            // 活跃 thread:先中断,再等 driver 落盘完成才删文件。若像旧
            // 实现那样在 driver 落盘前就删文件,driver 的 `append_turn`
            // (`create(true)`)会把 `<id>.jsonl` 复活,导致删除后
            // `store.exists` 仍为真、`thread/resume` 能把已删 thread 拉回。
            // 故复用 resume 的等待模式,等落盘后再删。
            interrupt_and_wait_for_persist(
                state.threads,
                state.turn_rx,
                &thread_id,
                state.pending_lead_replies,
                state.lead_replies,
            )
            .await;
            // 落盘已结束:现在从内存移除(drop prompt_tx 让 driver 收尾)并删文件。
            state.threads.remove(&thread_id);
            state.pending_activation.remove(&thread_id);
            // End this conversation's own root before the shared binding is
            // considered for detach: the root is per conversation, the
            // binding is per project and outlives any single conversation.
            if let Some(root) = state
                .thread_roots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&thread_id)
            {
                root.detach();
            }
            if let Some(watch) = state.trace_watches.remove(&thread_id) {
                watch.stop().await;
            }
            if let Some(watch) = state.children_watches.remove(&thread_id) {
                watch.stop().await;
            }
            if let Some(watch) = state.process_watches.remove(&thread_id) {
                watch.stop().await;
            }
            // 视图帧与 thread 同生命周期:帧表按 thread_id 索引,不清理就永久驻留。
            // `thread/view/read` 有 `require_known_thread` 前置检查,故此处不影响
            // 正确性,只是不让已删会话的帧继续占内存。
            state.view_frames.lock().await.remove(&thread_id);
            let live_cwds = state
                .threads
                .values()
                .map(|session| session.cwd.clone())
                .collect::<Vec<_>>();
            detach_unused_runtimes(state.runtimes, &live_cwds);
            if let Err(e) = thread_store.delete(&thread_id) {
                eprintln!("[app-server] failed to delete thread files for {thread_id}: {e}");
            }
            // 会话已删：成员表不能留悬空 id（spec「不变量 2」）。失败只记日志——
            // 会话删除本身已经成功，不该因为一次分组清理写失败而回滚。
            if let Some(cwd) = thread_store.root().parent().and_then(|p| p.parent()) {
                if let Err(e) = crate::group_store::remove_thread(cwd, &thread_id) {
                    eprintln!("[app-server] could not prune {thread_id} from groups: {e}");
                }
            }
            // 收走该会话的附件:与 thread 文件同一生命周期。放在 delete
            // 之后(driver 已结束落盘),避免竞态——工作区被挪走时
            // `remove_attachments` 尽力而为,不让删除失败。这里拿不到
            // cwd(见 `detach_unused_runtimes` 的注释:不为它改
            // `store_lookup` 签名),故由 store 的 root
            // (`<cwd>/.yi-agent/threads`)上溯两级得到工作区。
            if let Some(cwd) = thread_store.root().parent().and_then(|p| p.parent()) {
                crate::attachments::remove_attachments(cwd, &thread_id);
            }
            // 上传登记表也随 thread 一起 prune：否则一条 `Committed`
            // 记录会活到 10 分钟 TTL，让被删 thread 的 `uploadId` 仍能
            // 反查到已被删掉的附件路径（随后 `prepare_image_file` 读空），
            // 未 commit 的 staging 槽与进程级会话名额也被死 thread 白占。
            // `drop_thread` 只需内存与每条会话已记的 staging 路径，故这里
            // 拿不到 cwd 也无妨（已落盘的附件由上面 `remove_attachments` 收）。
            state.uploads.drop_thread(&thread_id);
            // 会话文件已摘除：按判定处置 worktree。删除失败只记日志，不阻断会话删除（D9）。
            // driver_project 已在进分支时留好——此处绝不能再 load（会话文件已不存在）。
            // 但失败这件事本身必须回报：用户刚被告知该 worktree 会被删掉，此刻会话
            // 已摘除、应用内再也删不掉它，又按 D10 让卡片上的 thread_id 悬空——
            // 若默默失败，worktree 就无声滞留、用户永远看不见。故除宿主日志外，
            // 把这一次"该删却没删成"如实回给客户端（失败才现身，成功仍为空）。
            let mut worktree_failure: Option<serde_json::Value> = None;
            if let (
                crate::worktree_reclaim::WorktreeReclaim::Delete { path }
                | crate::worktree_reclaim::WorktreeReclaim::DestructiveDelete { path, .. },
                Some(project),
            ) = (&reclaim, driver_project.as_deref())
            {
                if let Err(error) = crate::worktree_reclaim::remove(path, Path::new(project), force)
                {
                    eprintln!(
                        "[app-server] could not remove the worktree for {thread_id}: {error}"
                    );
                    worktree_failure = Some(json!({
                        "action": "remove",
                        "removed": false,
                        "path": path.to_string_lossy(),
                        "reason": error,
                    }));
                }
            }
            // 只有"被承诺会删、且确实没删成"才带字段；普通会话（reclaim=None）、
            // Keep、以及删除成功一律仍是空成功体，既有形状零变化。
            let body = match worktree_failure {
                Some(worktree) => json!({ "worktree": worktree }),
                None => json!({}),
            };
            write_response(state.hub, state.client, ok_response(id, body)).await?;
        }
        "thread/fork" => {
            // 与 `thread/rename` / `thread/delete` 同档：Control 本就能
            // `thread/start` + `turn/start`，单开此门不扩大能力面。
            if state.client_scope < Scope::Control {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::insufficient_scope(Scope::Control)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            // 冷会话必须能 fork：复制一个旧会话去分支正是主要用法，所以不能
            // 只看 `state.threads`，要走 store_lookup。
            let store = store_lookup(state.threads, state.workspaces, state.cfg, &thread_id);
            let in_memory = state.threads.contains_key(&thread_id);
            if !in_memory && !store.exists(&thread_id) {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::unknown_thread(&thread_id)),
                )
                .await?;
                return Ok(Flow::Continue);
            }

            // 运行中一律拒绝，且必须在任何磁盘动作之前：复制到半轮会产生一个
            // 结尾停在裸 tool_use 上的坏副本。判据与 `power_busy::local_busy`
            // 同一形状：有活跃 turn，或状态不是 Idle（含 awaiting_approval）。
            if let Some(session) = state.threads.get(&thread_id) {
                let status = *session
                    .status
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if session.active_turn_id.is_some() || !matches!(status, ThreadStatus::Idle) {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::turn_in_progress(&thread_id)),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            }

            // 目录没了就没法写出一个能打开的会话：明确报错，而不是让它落盘之后
            // 才在 resume 阶段失败。口径对齐 `thread/resume` 的同款检查。
            let Some(cwd) = thread_cwd(state.threads, state.workspaces, state.cfg, &thread_id)
            else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::unknown_thread(&thread_id)),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            if !cwd.is_dir() {
                write_response(
                    state.hub,
                    state.client,
                    err_response(
                        id,
                        RpcError::invalid_params(format!(
                            "working directory no longer exists: {}",
                            cwd.display()
                        )),
                    ),
                )
                .await?;
                return Ok(Flow::Continue);
            }

            let new_id = format!("thread-{}", uuid::Uuid::new_v4());
            match store.fork_thread(&thread_id, &new_id) {
                Ok(created) => {
                    // 标题从落盘的 meta 回读：`fork_thread` 已经算好（含无标题时的
                    // 兜底派生），这里再推导一遍只会两处规则漂移。
                    let title = store
                        .read_meta(&created)
                        .ok()
                        .flatten()
                        .and_then(|m| m.title);
                    write_response(
                        state.hub,
                        state.client,
                        ok_response(id, json!({ "thread_id": created, "title": title })),
                    )
                    .await?;
                }
                // `WouldBlock` 时 message 已是 `ThreadBusy` 的中文（「会话 X 正被
                // 另一个 yi-agent 实例使用」），原样上抛，客户端不必再翻译。
                Err(e) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(e.to_string())),
                    )
                    .await?;
                }
            }
        }
        "thread/clear" => {
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            let Some(session) = state.threads.get(&thread_id) else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::unknown_thread(&thread_id)),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            // 清空跑在半个 turn 上会产出不自洽的历史，直接拒绝。
            if session.active_turn_id.is_some() {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::turn_in_progress(&thread_id)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let (reply_tx, reply_rx) = oneshot::channel();
            if session
                .session_tx
                .send(SessionCommand::Clear { reply: reply_tx })
                .await
                .is_err()
            {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::internal("thread driver is gone")),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            match reply_rx.await {
                Ok(Ok(())) => {
                    write_response(state.hub, state.client, ok_response(id, json!({}))).await?
                }
                Ok(Err(message)) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(message)),
                    )
                    .await?
                }
                Err(_) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal("thread driver dropped")),
                    )
                    .await?
                }
            }
        }
        _ => return Ok(Flow::Unknown),
    }
    Ok(Flow::Continue)
}
