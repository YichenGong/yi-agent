//! `process/*` 域的 RPC 处理函数。
//!
//! 从 `server.rs` 的 `serve()` dispatch 逐字搬出(Task 3 batch 2)。按 thread 的进程管理器(list/read/kill);kill 仍是 admin 门禁(门禁在主循环、未随域搬移)。
//!
//! 臂体保持原样,只有两处改名:主循环局部量改走 [`AppState`] 字段(`hub` ->
//! `state.hub` 等;字段本身已是引用,故冗余的 `&` 一并去掉),以及 `continue`
//! 换成 `return Ok(Flow::Continue)`——后者的语义由 `serve()` 翻回 `continue`,
//! 与搬移前逐字一致。

use serde_json::json;

use crate::protocol::{RequestEnvelope, RequestId, RpcError};
use crate::rpc::{AppState, Flow};
use crate::server::{err_response, ok_response, write_response};

/// 处理本域的一个 RPC 请求。
///
/// 返回 [`Flow::Continue`] 表示「本域已应答,主循环回到下一轮」(等价于搬移前臂
/// 里的 `continue`);[`Flow::Unknown`] 表示方法不在本域内,交回 `serve()` 的原始
/// dispatch(走既有的 `method_not_found`),响应形状与搬移前一致。
pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "process/list" => {
            let Some(thread_id) = req
                .params
                .get("thread_id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
            else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("missing thread_id")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            // 未知 thread 返回空表而非错误:切走再切回、thread 已删除
            // 都是正常路径,报错只会弹一条无意义的红条。
            let processes = state
                .threads
                .get(&thread_id)
                .map(|s| s.process_manager.list())
                .unwrap_or_default();
            write_response(
                state.hub,
                state.client,
                ok_response(id, json!({ "processes": processes })),
            )
            .await?;
        }
        "process/read" => {
            let Some(thread_id) = req
                .params
                .get("thread_id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
            else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("missing thread_id")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            let Some(process_id) = req
                .params
                .get("process_id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
            else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("missing process_id")),
                )
                .await?;
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
            let cursor = req.params.get("cursor").and_then(|v| v.as_u64());
            let max_bytes = req
                .params
                .get("max_bytes")
                .and_then(|v| v.as_u64())
                .unwrap_or(64 * 1024) as usize;
            match session
                .process_manager
                .read(
                    yi_agent_tools::ProcessSelector::Id(process_id),
                    cursor,
                    max_bytes,
                )
                .await
            {
                Ok(result) => {
                    write_response(
                        state.hub,
                        state.client,
                        ok_response(
                            id,
                            serde_json::to_value(result).unwrap_or(serde_json::Value::Null),
                        ),
                    )
                    .await?
                }
                Err(message) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::invalid_params(message)),
                    )
                    .await?
                }
            }
        }
        "process/kill" => {
            let Some(thread_id) = req
                .params
                .get("thread_id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
            else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("missing thread_id")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            let Some(process_id) = req
                .params
                .get("process_id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
            else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("missing process_id")),
                )
                .await?;
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
            match session
                .process_manager
                .kill(yi_agent_tools::ProcessSelector::Id(process_id))
                .await
            {
                Ok(()) => {
                    write_response(
                        state.hub,
                        state.client,
                        ok_response(id, json!({ "ok": true })),
                    )
                    .await?
                }
                Err(message) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::invalid_params(message)),
                    )
                    .await?
                }
            }
        }
        // 域内兜底:本模块只实现上面列出的方法。若 `domain_for` 把别的同前缀
        // 方法也划给本域,交回 `serve()` 原始 dispatch,而不是在这里自行编造响应。
        _ => return Ok(Flow::Unknown),
    }
    // 臂体走完而**没有** `continue` 的路径,在搬移前同样落到本轮 dispatch 的
    // 末尾、回到主循环下一轮 —— 即与 `continue` 等价,故统一在此收束。
    Ok(Flow::Continue)
}
