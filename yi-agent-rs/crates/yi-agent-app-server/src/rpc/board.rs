//! `board/*` 域的 RPC 处理函数。
//!
//! 从 `server.rs` 的 `serve()` dispatch 逐字搬出(Task 3 batch 1)。臂体保持
//! 原样,只有三处改名:主循环局部量改走 [`AppState`] 字段(`hub` -> `state.hub`
//! 等),臂内的 `method.as_str()` 改成 `req.method.as_str()`(本模块的形参不再
//! 叫 `method`),以及 `continue` 换成 `return Ok(Flow::Continue)`——后者的语义由
//! `serve()` 翻回 `continue`,与搬移前逐字一致。

use std::path::Path;

use serde_json::json;

use crate::protocol::{RequestEnvelope, RequestId, RpcError};
use crate::rpc::{AppState, Flow};
use crate::server::{
    ensure_watchman_installed, err_response, ok_response, project_arg, to_json, write_response,
};

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
        "board/create" => {
            let Some(project) = project_arg(&req.params) else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("missing or empty project")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            let mut launcher = |project: &Path| (state.board_launcher)(project);
            match yi_agent_boards::lifecycle::create_with_project(
                &project,
                state.board_dir,
                state.resident_dir,
                state.plugin_command,
                &mut launcher,
            ) {
                Ok(status) => {
                    // 看板建好后确保值守在装：开关为开且 plist 未指向
                    // 当前 exe 时装上。best-effort——装不上值守不该让
                    // 「创建成功」变成失败（前端已拿到看板）。
                    ensure_watchman_installed(
                        state.theme_handle.workdir(),
                        state.watchman_home,
                        state.watchman_install,
                    );
                    write_response(state.hub, state.client, ok_response(id, to_json(&status)))
                        .await?
                }
                Err(message) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(message)),
                    )
                    .await?
                }
            }
        }
        "board/remove" => {
            let Some(project) = project_arg(&req.params) else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("missing or empty project")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            match yi_agent_boards::lifecycle::remove_in(
                &project,
                state.board_dir,
                state.resident_dir,
            ) {
                Ok(()) => {
                    write_response(state.hub, state.client, ok_response(id, json!({}))).await?
                }
                Err(message) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(message)),
                    )
                    .await?
                }
            }
        }
        "board/list" => {
            // The registry is the whole answer: the sidebar decides
            // which projects get a board entry from exactly this.
            match yi_agent_boards::registry::list(state.board_dir) {
                Ok(boards) => {
                    let boards: Vec<serde_json::Value> = boards
                        .into_iter()
                        .map(|board| {
                            json!({
                                "project": board.project.to_string_lossy(),
                                "created_at": board.created_at,
                                "status": to_json(
                                    &yi_agent_boards::lifecycle::status_with(
                                        &board.project,
                                        state.board_dir,
                                    ),
                                ),
                            })
                        })
                        .collect();
                    write_response(
                        state.hub,
                        state.client,
                        ok_response(id, json!({ "boards": boards })),
                    )
                    .await?
                }
                Err(message) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(message.to_string())),
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
