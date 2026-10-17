//! `plugin/*` 与 `plugins/*` 域的 RPC 处理函数。
//!
//! 从 `server.rs` 的 `serve()` dispatch 逐字搬出(Task 3 batch 1)。臂体保持
//! 原样,只有三处改名:主循环局部量改走 [`AppState`] 字段(`hub` -> `state.hub`
//! 等),臂内的 `method.as_str()` 改成 `req.method.as_str()`(本模块的形参不再
//! 叫 `method`),以及 `continue` 换成 `return Ok(Flow::Continue)`——后者的语义由
//! `serve()` 翻回 `continue`,与搬移前逐字一致。

use serde_json::json;

use crate::protocol::{RequestEnvelope, RequestId, RpcError};
use crate::rpc::{AppState, Flow};
use crate::server::{err_response, list_installed_plugins, ok_response, plugin_query, plugin_query_self, plugin_scope, project_arg, write_response};

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
        "plugin/query" => {
            // `project` names the project being queried, not the
            // server's cwd: the whole point of the RPC is to reach
            // a board that lives under another project's daemon.
            let Some(project) = project_arg(&req.params) else {
                write_response(
                    state.hub, state.client,
                    err_response(id, RpcError::invalid_params("missing or empty project")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            let plugin = req
                .params
                .get("plugin")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let method = req
                .params
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let params = req
                .params
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            match plugin_query(&project, state.board_dir, method, plugin, params) {
                Ok(value) => {
                    write_response(state.hub, state.client, ok_response(id, value)).await?
                }
                Err(error) => {
                    write_response(
                        state.hub, state.client,
                        err_response(
                            id,
                            RpcError::board_query(error.code, error.message),
                        ),
                    )
                    .await?
                }
            }
        }
        "plugins/list" => {
            let workdir = state.theme_handle.workdir();
            // 清单按项目安装，故作用域可由调用方的 `project` 指定；
            // 缺省回落到宿主 workdir（TUI 与旧客户端）。
            let scope = plugin_scope(workdir, &req.params);
            let plugins: Vec<serde_json::Value> = list_installed_plugins(&scope)
                .into_iter()
                .map(|plugin| {
                    json!({
                        "name": plugin.name,
                        "queryable": plugin.queryable,
                        "switch_key": plugin.switch_key,
                    })
                })
                .collect();
            write_response(state.hub, state.client, ok_response(id, json!({ "plugins": plugins })))
                .await?;
        }
        "plugin/settings/read" => {
            let workdir = state.theme_handle.workdir().to_path_buf();
            let scope = plugin_scope(&workdir, &req.params);
            let plugin = req
                .params
                .get("plugin")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            match plugin_query_self(&scope, &plugin, "settings.read", json!({})) {
                Ok(value) => write_response(state.hub, state.client, ok_response(id, value)).await?,
                Err(error) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::board_query(error.code, error.message)),
                    )
                    .await?
                }
            }
        }
        "plugin/settings/write" => {
            let workdir = state.theme_handle.workdir().to_path_buf();
            let scope = plugin_scope(&workdir, &req.params);
            let plugin = req
                .params
                .get("plugin")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let settings = req.params.get("settings").cloned().unwrap_or(json!({}));
            match plugin_query_self(
                &scope,
                &plugin,
                "settings.write",
                json!({ "settings": settings }),
            ) {
                Ok(value) => write_response(state.hub, state.client, ok_response(id, value)).await?,
                Err(error) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::board_query(error.code, error.message)),
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
