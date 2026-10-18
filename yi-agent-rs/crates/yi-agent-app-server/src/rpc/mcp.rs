//! `mcp/*` 域的 RPC 处理函数。
//!
//! 与 `skill/*` 同构:臂体在域内应答后回主循环下一轮,`_` 交回 `serve()` 的原始
//! dispatch。权限档位 `Observe`——只读、不碰凭据值、不落盘,与 `skill/list`、
//! `plugins/list` 同档,故本域**不加**臂内门禁(门禁只落在 `Control`/`Admin`
//! 方法上,见 `serve()` 里的 `ADMIN_METHODS` 与各域内的 `client_scope` 判断)。

use crate::protocol::{RequestEnvelope, RequestId};
use crate::rpc::{AppState, Flow};
use crate::server::{err_response, ok_response, write_response};

/// 处理本域的一个 RPC 请求。
///
/// 返回 [`Flow::Continue`] 表示「本域已应答,主循环回到下一轮」;
/// [`Flow::Unknown`] 表示方法不在本域内,交回 `serve()` 的原始 dispatch。
pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        // 设置页「MCP」面板:按项目列出 server 与缓存中的工具清单。
        "mcp/list" => {
            match crate::mcp_rpc::handle_mcp_request(
                state.theme_handle.workdir(),
                req.method.as_str(),
                &req.params,
            ) {
                Ok(value) => {
                    write_response(state.hub, state.client, ok_response(id, value)).await?
                }
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?
                }
            }
        }
        // 域内兜底:本模块只实现上面列出的方法。
        _ => return Ok(Flow::Unknown),
    }
    // 臂体走完而**没有** `continue` 的路径,在搬移前同样落到本轮 dispatch 的
    // 末尾、回到主循环下一轮 —— 即与 `continue` 等价,故统一在此收束。
    Ok(Flow::Continue)
}
