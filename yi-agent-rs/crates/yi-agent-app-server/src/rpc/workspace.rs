//! `workspace/*` 域的 RPC 处理函数。
//!
//! 从 `server.rs` 的 `serve()` dispatch 逐字搬出(Task 3 batch 2)。工作区注册表(list/add/remove):add 存 canonical 路径,remove 同样规范化以保幂等;
//!
//! 臂体保持原样,只有两处改名:主循环局部量改走 [`AppState`] 字段(`hub` ->
//! `state.hub` 等;字段本身已是引用,故冗余的 `&` 一并去掉),以及 `continue`
//! 换成 `return Ok(Flow::Continue)`——后者的语义由 `serve()` 翻回 `continue`,
//! 与搬移前逐字一致。

use std::path::{Path, PathBuf};

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
        "workspace/list" => {
            let list: Vec<serde_json::Value> = state
                .workspaces
                .list()
                .into_iter()
                .map(|p| json!({ "path": p, "exists": Path::new(&p).is_dir() }))
                .collect();
            write_response(
                state.hub,
                state.client,
                ok_response(id, json!({ "workspaces": list })),
            )
            .await?;
        }
        "workspace/add" => {
            let raw = req
                .params
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match std::fs::canonicalize(raw) {
                Ok(p) if p.is_dir() => {
                    let value = p.to_string_lossy().to_string();
                    match state.workspaces.add(&p) {
                        Ok(()) => {
                            write_response(
                                state.hub,
                                state.client,
                                ok_response(id, json!({ "path": value })),
                            )
                            .await?
                        }
                        Err(e) => {
                            write_response(
                                state.hub,
                                state.client,
                                err_response(id, RpcError::internal(e.to_string())),
                            )
                            .await?
                        }
                    }
                }
                _ => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::invalid_params("path is not a directory")),
                    )
                    .await?;
                }
            }
        }
        "workspace/remove" => {
            let raw = req
                .params
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // add 存的是 canonical 路径;remove 也必须规范化,否则
            // 符号链接 / 相对路径 / 尾斜杠会静默 no-op 却仍回成功。
            // canonicalize 失败(路径已不存在)时回退原始串,保持幂等。
            let key = std::fs::canonicalize(raw).unwrap_or_else(|_| PathBuf::from(raw));
            match state.workspaces.remove(&key) {
                Ok(()) => {
                    write_response(state.hub, state.client, ok_response(id, json!({}))).await?
                }
                Err(e) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(e.to_string())),
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
