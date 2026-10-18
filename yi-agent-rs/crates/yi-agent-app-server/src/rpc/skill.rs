//! `skill/*` 域的 RPC 处理函数。
//!
//! 从 `server.rs` 的 `serve()` dispatch 逐字搬出(Task 3 之后的 `batch 4`,与
//! 合并 `main` 时一并落位)。臂体保持原样,只有三处语言强制的改名:主循环局部量
//! 改走 [`AppState`] 字段(`hub` -> `state.hub`、`skills_home` -> `state.skills_home`
//! 等),臂内的 `method.as_str()` 改成 `req.method.as_str()`(本模块的形参不再叫
//! `method`),以及 `continue` 换成 `return Ok(Flow::Continue)`——后者的语义由
//! `serve()` 翻回 `continue`,与搬移前逐字一致。

use crate::protocol::{RequestEnvelope, RequestId};
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
        // 设置页「技能」面板：枚举三层 skill 根 + 按需读正文。只读，
        // 故与 `config/read` / `plugins/list` 同档只需 `Observe`——
        // iOS 设备默认 Control，加闸即手机端结构性不可用。
        "skill/list" | "skill/read" => {
            match crate::skill_rpc::handle_skill_request(
                state.theme_handle.workdir(),
                state.skills_home,
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
        // 域内兜底:本模块只实现上面列出的方法。若 `domain_for` 把别的同前缀
        // 方法也划给本域,交回 `serve()` 原始 dispatch,而不是在这里自行编造响应。
        _ => return Ok(Flow::Unknown),
    }
    // 臂体走完而**没有** `continue` 的路径,在搬移前同样落到本轮 dispatch 的
    // 末尾、回到主循环下一轮 —— 即与 `continue` 等价,故统一在此收束。
    Ok(Flow::Continue)
}
