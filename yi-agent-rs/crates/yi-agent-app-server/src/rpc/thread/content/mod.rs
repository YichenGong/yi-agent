//! `thread` 域的 content 组：list / listAll / diff/read / view/read /
//! subscribe / readItems。
//!
//! 臂体从 `rpc/thread/mod.rs` 的 `handle()` 逐字搬出（wave2 C3b），只做允许的
//! 三处改写：`continue` → `return Ok(Flow::Continue)`、`method.as_str()` →
//! `req.method.as_str()`、裸局部量 → `state.<字段>`。

use crate::protocol::{RequestEnvelope, RequestId, RpcError};
use crate::rpc::{AppState, Flow};
use crate::server::{
    err_response, item_id, ok_response, require_known_thread, require_thread_id, store_lookup,
    thread_summary_json, write_response,
};
use serde_json::json;
use std::sync::Arc;

pub(crate) mod diff_read;
pub(crate) mod list_all;

/// 本组认领的 method。`threads` 是 `thread/list` 的旧名，同一个臂兼收二者，
/// 故调用方按 `thread/list` 分派、归属判断要把它算进来。
pub(crate) fn owns(method: &str) -> bool {
    matches!(
        method,
        "thread/list"
            | "threads"
            | "thread/listAll"
            | "thread/diff/read"
            | "thread/view/read"
            | "thread/subscribe"
            | "thread/readItems"
    )
}

pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    // 两个臂已各自成片，先转交；其余四个在本文件的第二个 match 里。
    match req.method.as_str() {
        "thread/listAll" => return list_all::handle(state, req, id).await,
        "thread/diff/read" => return diff_read::handle(state, req, id).await,
        _ => {}
    }
    match req.method.as_str() {
        "thread/list" | "threads" => {
            // 单目录列表:仍是 `cfg.workdir` 的 store。跨目录分组见下方
            // `thread/listAll`。
            let store = crate::thread_store::ThreadStore::new(&state.cfg.workdir);
            match store.list() {
                Ok(metas) => {
                    // 显式映射而非直接序列化 ThreadMeta:wire 契约与存储结构解耦,
                    // 存储字段重命名不会悄悄改变 RPC 输出。
                    let threads: Vec<serde_json::Value> = metas
                        .into_iter()
                        .map(|m| thread_summary_json(&m, state.threads))
                        .collect();
                    write_response(
                        state.hub,
                        state.client,
                        ok_response(id, json!({ "threads": threads })),
                    )
                    .await?;
                }
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
        "thread/view/read" => {
            // 读回**帧**（模型给命令 / 结构化默认），与 `thread/diff/read` 的
            // 「结构化取数」是两条不同的路径，故是两个方法而不是一个方法的开关。
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            if !require_known_thread(
                state.hub,
                state.client,
                state.threads,
                &thread_id,
                id.clone(),
            )
            .await?
            {
                return Ok(Flow::Continue);
            }
            let refresh = req
                .params
                .get("refresh")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if refresh {
                // 重算这一帧：**分叉**在「这一帧原本是命令帧还是结构化默认」上。
                // 命令帧重跑同一条命令（模型可重复观察同一视角）；结构化默认则
                // 重算锚点 diff（工作区在这期间变了，重读必须反映新内容）。
                //
                // 与 `thread/diff/read` 同一理由搬进独立 task：结构化分支同步跑
                // thread_diff(5+ 次 git),命令分支虽 async 但开头同步
                // `Path::canonicalize`(挂载点失效时会卡住),都不该压在主循环上。
                let cwd = state
                    .threads
                    .get(&thread_id)
                    .map(|t| t.cwd.clone())
                    .filter(|c| !c.is_empty())
                    .unwrap_or_else(|| state.cfg.workdir.display().to_string());
                let repo = std::path::PathBuf::from(&cwd);
                // 只要 meta 的 `anchor_sha`：`load` 会把整份对话 jsonl 解析一遍,
                // 那是本任务要消除的主循环阻塞类 IO。同 `rpc/mod.rs:70` 的
                // `store.read_meta(thread_id)` 取法。
                let anchor = state
                    .threads
                    .get(&thread_id)
                    .and_then(|t| t.store.read_meta(&thread_id).ok().flatten())
                    .and_then(|m| m.anchor_sha);
                // 分两次短暂持锁，**不**把 guard 跨过 await：`serve()` 跑在
                // `tokio::spawn` 里，guard 过 await 会让整个 future 非 `Send`。
                // 快照连**基准来历**一起取：结构化帧可能是「模型指定的基准」
                // (`base_kind == "explicit"`)，重算时必须原样再用那个 base——
                // 只传 `explicit=None` 会让它退化成会话锚点。
                let snapshot = state
                    .view_frames
                    .lock()
                    .await
                    .get(&thread_id)
                    .map(|f| (f.command.clone(), f.base.clone(), f.base_kind.clone()));
                let command = snapshot.as_ref().and_then(|(c, _, _)| c.clone());
                let explicit = snapshot.as_ref().and_then(|(_, base, kind)| {
                    (kind == "explicit").then(|| base.clone()).flatten()
                });
                let hub = Arc::clone(state.hub);
                let client = state.client.clone();
                let view_frames = Arc::clone(state.view_frames);
                let tid_for_task = thread_id.clone();
                tokio::spawn(async move {
                    let next = match command.as_deref() {
                        // 命令帧：`command_frame` 是 async(tokio::process + 超时)，
                        // 放进本 task 即已脱离主循环。
                        Some(command) => {
                            crate::git_diff::command_frame(&repo, command, None, None).await
                        }
                        // 结构化帧：全同步(5+ 次 git_ok)，必须进 blocking 池。
                        None => {
                            let repo = repo.clone();
                            let explicit = explicit.clone();
                            let anchor = anchor.clone();
                            match tokio::task::spawn_blocking(move || {
                                crate::git_diff::structured_frame(
                                    &repo,
                                    explicit.as_deref(),
                                    anchor.as_deref(),
                                )
                            })
                            .await
                            {
                                Ok(frame) => frame,
                                Err(error) => {
                                    eprintln!("[app-server] view refresh task failed: {error}");
                                    // 算不出来就回读已有帧（CAS 阶段会读到它），
                                    // 不写坏帧、不丢响应。
                                    let existing = view_frames.lock().await;
                                    match existing.get(&tid_for_task) {
                                        Some(f) => f.clone(),
                                        None => {
                                            // 没有既有帧可回读时**也必须应答**：直接
                                            // return 会把这个响应的 promise 永远吊着，
                                            // 桌面 RPC 客户端没有单请求超时。空值与
                                            // 非 refresh 路径「无帧」的应答同形。
                                            eprintln!(
                                                "[app-server] view refresh has no frame to fall back on"
                                            );
                                            drop(existing);
                                            let _ = write_response(
                                                &hub,
                                                &client,
                                                ok_response(id, json!(null)),
                                            )
                                            .await;
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                    };
                    // 写回用 CAS，不是无条件覆盖：「读缓存命令」与「写回新帧」之间
                    // 隔了一次 await（最多 VIEW_TIMEOUT）。期间别的写入者（另一次
                    // refresh、或 Task 4 的 watcher）可能已经写了更新的帧；若无条件
                    // 覆盖，面板就会显示与「刚刚请求的命令」不符的内容。
                    // 判定：槽里的命令仍等于我们读到的快照才写。
                    // 边界：判据只有 `command`，故它只对**命令帧**有区分力——两条
                    // 都是结构化帧时区分不出先后，后果仅是多算一遍，不产生错标。
                    let mut frames = view_frames.lock().await;
                    let unchanged = frames.get(&tid_for_task).and_then(|f| f.command.as_deref())
                        == command.as_deref();
                    if unchanged {
                        frames.insert(tid_for_task.clone(), next);
                    }
                    let result = frames
                        .get(&tid_for_task)
                        .map(|f| serde_json::to_value(f).unwrap_or(json!(null)))
                        .unwrap_or(json!(null));
                    drop(frames);
                    let _ = write_response(&hub, &client, ok_response(id, result)).await;
                });
                return Ok(Flow::Continue);
            }
            let result = state
                .view_frames
                .lock()
                .await
                .get(&thread_id)
                .map(|f| serde_json::to_value(f).unwrap_or(json!(null)))
                .unwrap_or(json!(null));
            write_response(state.hub, state.client, ok_response(id, result)).await?;
        }
        "thread/subscribe" => {
            // 整体替换该客户端的订阅集合。**不在 ADMIN_METHODS 内**:
            // 任何已初始化客户端都能收窄自己的 feed。未调用过的客户端
            // 保持 `Feed::All`(桌面行为不变)。
            let ids: Option<Vec<String>> = req
                .params
                .get("threadIds")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                });
            let Some(ids) = ids else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("missing threadIds")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            if ids.len() > crate::broadcast::MAX_SUBSCRIPTIONS {
                write_response(
                    state.hub,
                    state.client,
                    err_response(
                        id,
                        RpcError::invalid_params(format!(
                            "at most {} threadIds",
                            crate::broadcast::MAX_SUBSCRIPTIONS
                        )),
                    ),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            state.hub.subscribe(state.client, ids.clone());
            write_response(
                state.hub,
                state.client,
                ok_response(id, json!({ "subscribed": ids })),
            )
            .await?;
        }
        "thread/readItems" => {
            // 只读补齐：读已落盘的 items，**不 resume、不重建 agent、
            // 不中断正在跑的回合**（这正是它相对 thread/resume 的意义）。
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            let after = req
                .params
                .get("afterItemId")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let store = store_lookup(state.threads, state.workspaces, state.cfg, &thread_id);
            let loaded = match store.load(&thread_id) {
                Ok(Some(l)) => l,
                Ok(None) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::unknown_thread(&thread_id)),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
                Err(e) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(e.to_string())),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            // 该 thread **最后一轮**的用量快照，与 `thread/resume` 回放的是同一份
            // （`ThreadStore::read_log` 只保留最后一条非空 usage）。回传它的理由：
            // 运行中的会话按既有约束**绝不 resume**、只走这里，不回传就等于让客户端
            // 在那条路径上永远拿不到这份已经读出来的数据。
            let loaded_usage = loaded.usage;
            // 同一份 `load` 已经算出的累计量：客户端拿到它才能显示「会话累计」，
            // 而不是只能拿最后一轮快照近似。
            let loaded_totals = loaded.totals;
            let items: Vec<crate::protocol::Item> = match after {
                Some(aid) => match loaded
                    .items
                    .iter()
                    .position(|it| item_id(it) == Some(aid.as_str()))
                {
                    // 找到锚点：只给它之后的部分。
                    Some(i) => loaded.items.into_iter().skip(i + 1).collect(),
                    // 锚点缺失（可能被 compact 丢弃）：返回全部，由客户端按 id 去重。
                    None => loaded.items,
                },
                None => loaded.items,
            };
            write_response(
                state.hub,
                state.client,
                ok_response(
                    id,
                    json!({ "items": items, "usage": loaded_usage, "totals": loaded_totals }),
                ),
            )
            .await?;
        }
        _ => return Ok(Flow::Unknown),
    }
    Ok(Flow::Continue)
}
