//! `thread` 域的 settings 组：重命名、置顶、标签、已读、排序、压缩，
//! 以及转发给 `model/` 与 `effort/` 的两个开关子片。
//!
//! 臂体从 `rpc/thread/mod.rs` 的 `handle()` 逐字搬出（wave2 C3c）。

use crate::protocol::{Notification, RequestEnvelope, RequestId, RpcError, Scope};
use crate::rpc::{AppState, Flow};
use crate::server::{
    collect_pinned, err_response, ok_response, require_thread_id, store_lookup, write_notification,
    write_response,
};
use crate::session::{CompactOutcome, SessionCommand};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use tokio::sync::oneshot;

pub(crate) mod effort;
pub(crate) mod model;

pub(crate) fn owns(method: &str) -> bool {
    matches!(
        method,
        "thread/rename"
            | "thread/setModel"
            | "thread/setReasoningEffort"
            | "thread/setPreserveThinking"
            | "thread/setPermissionMode"
            | "thread/setPinned"
            | "thread/setTag"
            | "thread/markRead"
            | "thread/reorderPinned"
            | "thread/compact"
    )
}

pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "thread/setModel" | "thread/setPermissionMode" => {
            return model::handle(state, req, id).await;
        }
        "thread/setReasoningEffort" | "thread/setPreserveThinking" => {
            return effort::handle(state, req, id).await;
        }
        _ => {}
    }
    match req.method.as_str() {
        "thread/rename" => {
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            let title = req
                .params
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if title.is_empty() {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("title must not be empty")),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            match store_lookup(state.threads, state.workspaces, state.cfg, &thread_id)
                .rename(&thread_id, &title)
            {
                Ok(true) => {
                    write_response(state.hub, state.client, ok_response(id, json!({}))).await?;
                }
                Ok(false) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::unknown_thread(&thread_id)),
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
        "thread/setPinned" => {
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            // 参数校验先于 thread 存在性:非布尔一律 `-32602`。
            let Some(pinned) = req.params.get("pinned").and_then(|v| v.as_bool()) else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("pinned must be a boolean")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            // 降序契约:now_millis 是当前最大值 → 新置顶项天然在最顶。
            let seq = if pinned {
                Some(crate::thread_store::now_millis())
            } else {
                None
            };
            match store_lookup(state.threads, state.workspaces, state.cfg, &thread_id)
                .set_pin_seq(&thread_id, seq)
            {
                Ok(true) => {
                    write_response(state.hub, state.client, ok_response(id, json!({}))).await?;
                }
                Ok(false) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::unknown_thread(&thread_id)),
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
        "thread/setTag" => {
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            // 参数校验先于 thread 存在性：非法颜色一律 `-32602`，
            // 与 setPinned 的「非布尔先报错」同一顺序。
            let raw = req
                .params
                .get("tag")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let tag: Option<String> = match raw {
                serde_json::Value::Null => None,
                serde_json::Value::String(s) if crate::thread_store::is_valid_tag(&s) => Some(s),
                other => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::invalid_params(format!(
                                "tag must be null or one of the 6 palette ids, got {other}"
                            )),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            match store_lookup(state.threads, state.workspaces, state.cfg, &thread_id)
                .set_tag(&thread_id, tag.as_deref())
            {
                Ok(true) => {
                    write_response(state.hub, state.client, ok_response(id, json!({}))).await?;
                    // 广播给所有客户端（含本端）：列表层恒推，就在地
                    // 更新色点，无需各自重拉 thread/listAll。
                    let _ = write_notification(
                        state.hub,
                        &Notification::TagChanged {
                            thread_id: thread_id.clone(),
                            tag: tag.clone(),
                        },
                    )
                    .await;
                }
                Ok(false) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::unknown_thread(&thread_id)),
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
        "thread/markRead" => {
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
            let store = store_lookup(state.threads, state.workspaces, state.cfg, &thread_id);
            match store.mark_read(&thread_id) {
                Ok(Some((last_read_at, last_completed_at))) => {
                    write_response(
                        state.hub,
                        state.client,
                        ok_response(id, json!({ "last_read_at": last_read_at })),
                    )
                    .await?;
                    // 广播给所有客户端（含本端）：列表层恒推。这正是「手机点开、
                    // 桌面蓝点同时消失」的通道——若归内容层，未订阅该会话的远程
                    // 客户端就收不到，另一端会一直亮着。
                    //
                    // 即便水位没推进（重复点开同一会话）也照发：它是一次廉价的
                    // 收敛信号，能让刚重连、还停在旧水位的另一端就地补齐。
                    let _ = write_notification(
                        state.hub,
                        &Notification::ReadStateChanged {
                            thread_id: thread_id.clone(),
                            last_read_at,
                            last_completed_at,
                        },
                    )
                    .await;
                }
                Ok(None) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::unknown_thread(&thread_id)),
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
        "thread/reorderPinned" => {
            let arr = match req.params.get("threadIds").and_then(|v| v.as_array()) {
                Some(a) => a,
                None => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::invalid_params("threadIds must be an array of strings"),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            // 全部必须是字符串,否则拒绝(不允许静默丢弃非字符串项)。
            let ids: Vec<String> = match arr
                .iter()
                .map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Option<Vec<_>>>()
            {
                Some(v) => v,
                None => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::invalid_params("threadIds must be an array of strings"),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            // 校验:与「当前全部置顶集合」完全一致,且无重复。
            let pinned_now = collect_pinned(state.workspaces);
            let current: HashSet<&str> = pinned_now.iter().map(|m| m.thread_id.as_str()).collect();
            let unique: HashSet<&str> = ids.iter().map(|s| s.as_str()).collect();
            let valid = unique.len() == ids.len()
                && ids.len() == current.len()
                && ids.iter().all(|i| current.contains(i.as_str()));
            if !valid {
                write_response(
                    state.hub,
                    state.client,
                    err_response(
                        id,
                        RpcError::invalid_params(
                            "threadIds must list exactly the pinned threads, no duplicates",
                        ),
                    ),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let current_seq: HashMap<String, Option<i64>> = pinned_now
                .iter()
                .map(|m| (m.thread_id.clone(), m.pin_seq))
                .collect();
            let assignments = crate::thread_store::assign_pin_seqs(&ids, &current_seq);
            let mut err: Option<RpcError> = None;
            for (tid, seq) in assignments {
                let store = store_lookup(state.threads, state.workspaces, state.cfg, &tid);
                match store.set_pin_seq(&tid, Some(seq)) {
                    Ok(true) => {}
                    Ok(false) => {
                        err = Some(RpcError::unknown_thread(&tid));
                        break;
                    }
                    Err(e) => {
                        err = Some(RpcError::internal(e.to_string()));
                        break;
                    }
                }
            }
            match err {
                None => {
                    write_response(state.hub, state.client, ok_response(id, json!({}))).await?;
                }
                Some(e) => {
                    write_response(state.hub, state.client, err_response(id, e)).await?;
                }
            }
        }
        "thread/compact" => {
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
            // 压缩会重写整段历史,turn 进行中不接受。
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
                .send(SessionCommand::Compact { reply: reply_tx })
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
            // compact 要调一次 provider 生成摘要,属于长请求;不设短超时。
            let result = match reply_rx.await {
                Ok(CompactOutcome::Compacted) => json!({"status": "compacted"}),
                Ok(CompactOutcome::NotReduced) => json!({"status": "not_reduced"}),
                Ok(CompactOutcome::Failed(message)) => {
                    json!({"status": "failed", "error": message})
                }
                Err(_) => json!({"status": "failed", "error": "thread driver dropped"}),
            };
            write_response(state.hub, state.client, ok_response(id, result)).await?;
        }
        _ => return Ok(Flow::Unknown),
    }
    Ok(Flow::Continue)
}
