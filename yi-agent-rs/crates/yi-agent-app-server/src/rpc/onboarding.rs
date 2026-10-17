//! `onboarding/*` 域的 RPC 处理函数。
//!
//! 从 `server.rs` 的 `serve()` dispatch 逐字搬出(Task 3 batch 1)。臂体保持
//! 原样,只有三处改名:主循环局部量改走 [`AppState`] 字段(`hub` -> `state.hub`
//! 等),臂内的 `method.as_str()` 改成 `req.method.as_str()`(本模块的形参不再
//! 叫 `method`),以及 `continue` 换成 `return Ok(Flow::Continue)`——后者的语义由
//! `serve()` 翻回 `continue`,与搬移前逐字一致。

use crate::protocol::{RequestEnvelope, RequestId, RpcError, Scope};
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
        // 首次安装引导。与 `model/*` 同档:`onboarding/status` 只读
        // (Observe);apply/dismiss/test 都触及密钥或落盘,需 `Control`。
        "onboarding/status" => {
            match crate::onboarding_rpc::handle_onboarding_request_at(
                state.onboarding_env_path,
                state.onboarding_preferences_path,
                state.models_path,
                req.method.as_str(),
                &req.params,
                state.cfg,
            ) {
                Ok(value) => {
                    write_response(state.hub, state.client, ok_response(id, value)).await?
                }
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?
                }
            }
        }
        "onboarding/apply" | "onboarding/dismiss" => {
            if state.client_scope < Scope::Control {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::insufficient_scope(Scope::Control)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            match crate::onboarding_rpc::handle_onboarding_request_at(
                state.onboarding_env_path,
                state.onboarding_preferences_path,
                state.models_path,
                req.method.as_str(),
                &req.params,
                state.cfg,
            ) {
                Ok(value) => {
                    write_response(state.hub, state.client, ok_response(id, value)).await?
                }
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?
                }
            }
        }
        // 探测只发一次最小请求,**不落盘**:故不归 `onboarding_rpc`
        // (它管路径),而是直接调 runtime 的 `test_connection`(async)。
        "onboarding/test" => {
            if state.client_scope < Scope::Control {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::insufficient_scope(Scope::Control)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let settings =
                match crate::onboarding_rpc::settings_from_params(&req.params) {
                    Ok(s) => s,
                    Err(error) => {
                        write_response(state.hub, state.client, err_response(id, error)).await?;
                        return Ok(Flow::Continue);
                    }
                };
            let outcome = yi_agent_runtime::onboarding::test_connection(&settings).await;
            let value = serde_json::json!({
                "ok": outcome.ok,
                "reason": outcome.reason,
            });
            write_response(state.hub, state.client, ok_response(id, value)).await?;
        }
        // 域内兜底:本模块只实现上面列出的方法。若 `domain_for` 把别的同前缀
        // 方法也划给本域,交回 `serve()` 原始 dispatch,而不是在这里自行编造响应。
        _ => return Ok(Flow::Unknown),
    }
    // 臂体走完而**没有** `continue` 的路径,在搬移前同样落到本轮 dispatch 的
    // 末尾、回到主循环下一轮 —— 即与 `continue` 等价,故统一在此收束。
    Ok(Flow::Continue)
}
