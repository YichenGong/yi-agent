//! `agent/*` 域的 RPC 处理函数。
//!
//! 从 `server.rs` 的 `serve()` dispatch 逐字搬出(Task 3 batch 3)。子代理的列表 /
//! 轨迹读取与订阅 / 消息投递 / 两段式取消(先预览拿 token,确认才真取消),全部经
//! 项目 daemon 的 IPC 转发给该会话的 runtime。
//!
//! 臂体保持原样,只有两处改名:主循环局部量改走 [`AppState`] 字段(`hub` ->
//! `state.hub` 等;字段本身已是引用,故冗余的 `&` 一并去掉),以及 `continue` 换成
//! `return Ok(Flow::Continue)`——后者的语义由 `serve()` 翻回 `continue`,与搬移前
//! 逐字一致。`break` 无此问题:本域的臂里没有 `break`。

use std::sync::Arc;

use serde_json::json;

use crate::protocol::{RequestEnvelope, RequestId, RpcError};
use crate::rpc::{AppState, Flow};
use crate::server::{
    ChildrenWatch, TraceWatch, children_for_thread, err_response, forward_to_daemon,
    list_task_summaries, ok_response, read_task_trace, require_known_thread, require_thread_id,
    snapshot_for_watch, socket_for_thread, stream_trace, watch_children, write_response,
};
/// 处理本域的一个 RPC 请求。
///
/// 返回 [`Flow::Continue`] 表示「本域已应答,主循环回到下一轮」(等价于搬移前臂里
/// 的 `continue`);[`Flow::Unknown`] 表示方法不在本域内,交回 `serve()` 的原始
/// dispatch(走既有的 `method_not_found`),响应形状与搬移前一致。
pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "agent/children/list" => {
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
            // 列表是尽力而为的只读视图:attach 未就绪或 daemon 不可达时返回空表,
            // 而不是报错。桌面端的暂留区因此永远能渲染,turn 也照常跑。
            let snapshot = list_task_summaries(state.runtimes, state.threads, &thread_id);
            let children = children_for_thread(&snapshot.unwrap_or_default(), &thread_id);
            // 首次拉取时建立守望者:此后列表变化由 agent/children/updated 推出,
            // 客户端不必轮询。daemon 不可达时不建立(没有可观察的变化)。
            if !state.children_watches.contains_key(&thread_id) {
                if let Some(socket) = socket_for_thread(state.runtimes, state.threads, &thread_id) {
                    let task = tokio::spawn(watch_children(
                        Arc::clone(state.hub),
                        thread_id.clone(),
                        socket,
                        children.clone(),
                    ));
                    state
                        .children_watches
                        .insert(thread_id.clone(), ChildrenWatch { task });
                }
            }
            write_response(
                state.hub,
                state.client,
                ok_response(id, json!({ "children": children })),
            )
            .await?;
        }
        "agent/trace/read" => {
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
            let Some(task_id) = req
                .params
                .get("taskId")
                .and_then(|v| v.as_str())
                .map(str::to_string)
            else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("missing taskId")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            match read_task_trace(state.runtimes, state.threads, &thread_id, &task_id) {
                Ok((rows, high_water_id)) => {
                    write_response(
                        state.hub,
                        state.client,
                        ok_response(id, json!({ "rows": rows, "highWaterId": high_water_id })),
                    )
                    .await?;
                }
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?;
                }
            }
        }
        "agent/trace/watch" => {
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
            let Some(task_id) = req
                .params
                .get("taskId")
                .and_then(|v| v.as_str())
                .map(str::to_string)
            else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("missing taskId")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            // 替换而不是叠加:一个 thread 只关注一个任务,旧流必须先停,
            // 否则换任务后两个任务的行使会同时到达客户端。
            if let Some(previous) = state.trace_watches.remove(&thread_id) {
                previous.stop().await;
            }
            match snapshot_for_watch(state.runtimes, state.threads, &thread_id, &task_id) {
                Ok((rows, high_water_id)) => {
                    let handle = tokio::spawn(stream_trace(
                        Arc::clone(state.hub),
                        thread_id.clone(),
                        task_id.clone(),
                        socket_for_thread(state.runtimes, state.threads, &thread_id)
                            .unwrap_or_default(),
                        high_water_id,
                    ));
                    state
                        .trace_watches
                        .insert(thread_id.clone(), TraceWatch { task: handle });
                    write_response(
                        state.hub,
                        state.client,
                        ok_response(id, json!({ "rows": rows, "highWaterId": high_water_id })),
                    )
                    .await?;
                }
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?;
                }
            }
        }
        "agent/message" => {
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
            let task_id = req
                .params
                .get("taskId")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let message = req
                .params
                .get("message")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let (Some(task_id), Some(message)) = (task_id, message) else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("missing taskId or message")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            match forward_to_daemon(
                state.runtimes,
                state.threads,
                &thread_id,
                yi_agent_store::ipc::IpcRequest::SendUserMessage {
                    task_id: task_id.clone(),
                    message,
                },
            ) {
                Ok(_) => {
                    write_response(
                        state.hub,
                        state.client,
                        ok_response(id, json!({ "queued": true })),
                    )
                    .await?;
                }
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?;
                }
            }
        }
        "agent/cancel/preview" => {
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
            let Some(task_id) = req
                .params
                .get("taskId")
                .and_then(|v| v.as_str())
                .map(str::to_string)
            else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("missing taskId")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            // 取消必须两步:预览拿 token,确认才真取消。这里只做第一步,
            // 绝不代客户端跳过确认。
            match forward_to_daemon(
                state.runtimes,
                state.threads,
                &thread_id,
                yi_agent_store::ipc::IpcRequest::PreviewCancel {
                    task_id: task_id.clone(),
                    recursive: false,
                },
            ) {
                Ok(yi_agent_store::ipc::IpcResponse::CancelPreview {
                    confirmation_token,
                    task_ids,
                    expires_in_secs,
                    ..
                }) => {
                    write_response(
                        state.hub,
                        state.client,
                        ok_response(
                            id,
                            json!({
                                "confirmationToken": confirmation_token,
                                "taskIds": task_ids,
                                "expiresInSecs": expires_in_secs,
                            }),
                        ),
                    )
                    .await?;
                }
                Ok(other) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::internal(format!(
                                "daemon returned a non-preview response: {other:?}"
                            )),
                        ),
                    )
                    .await?;
                }
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?;
                }
            }
        }
        "agent/cancel" => {
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
            let task_id = req
                .params
                .get("taskId")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let confirmation_token = req
                .params
                .get("confirmationToken")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let (Some(task_id), Some(confirmation_token)) = (task_id, confirmation_token) else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(
                        id,
                        RpcError::invalid_params(
                            "missing taskId or confirmationToken; preview first",
                        ),
                    ),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            match forward_to_daemon(
                state.runtimes,
                state.threads,
                &thread_id,
                yi_agent_store::ipc::IpcRequest::ConfirmCancel {
                    task_id: task_id.clone(),
                    recursive: false,
                    confirmation_token,
                },
            ) {
                Ok(_) => {
                    write_response(
                        state.hub,
                        state.client,
                        ok_response(id, json!({ "cancelled": true })),
                    )
                    .await?;
                }
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?;
                }
            }
        }
        "agent/trace/unwatch" => {
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
            if let Some(watch) = state.trace_watches.remove(&thread_id) {
                watch.stop().await;
            }
            if let Some(watch) = state.children_watches.remove(&thread_id) {
                watch.stop().await;
            }
            if let Some(watch) = state.process_watches.remove(&thread_id) {
                watch.stop().await;
            }
            write_response(
                state.hub,
                state.client,
                ok_response(id, json!({ "stopped": true })),
            )
            .await?;
        }
        // 域内兜底:本模块只实现上面列出的方法。若 `domain_for` 把别的同前缀
        // 方法也划给本域,交回 `serve()` 原始 dispatch,而不是在这里自行编造响应。
        _ => return Ok(Flow::Unknown),
    }
    // 臂体走完而**没有** `continue` 的路径,在搬移前同样落到本轮 dispatch 的
    // 末尾、回到主循环下一轮 —— 即与 `continue` 等价,故统一在此收束。
    Ok(Flow::Continue)
}
