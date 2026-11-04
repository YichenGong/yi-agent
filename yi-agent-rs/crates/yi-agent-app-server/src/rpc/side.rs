//! `side/*` 域:旁支会话的六个方法(spec §8.1)。
//!
//! 分派的两条硬规则:
//!
//! 1. **慢活不在请求路径上。** `open` 要读父会话的持久状态、复制附件、装工具集,
//!    `close` 要等核心任务真正收敛。这些都放 `side_tasks` 里跑,主循环只做参数
//!    校验与派发。主循环是单 task,内联 `await` 会把所有连接(含桌面 GUI)一起
//!    堵住——`side_creating_open_does_not_block_the_loop` 钉的就是这一点。
//! 2. **不借 `AppState<'a>` 跨 spawn。** 那个结构按请求借用主循环的局部量,
//!    move 进 task 就是悬空引用。派发前把它需要的东西克隆成 owned:
//!    `Arc<SideRegistry>` + `SideResponseSender`。
//!
//! 权限档:`list`/`readItems` 只读(`Observe`);`open`/`turnStart`/`turnInterrupt`/
//! `close` 会花真钱或改状态(`Control`)。认证先于 scope——`serve()` 在 dispatch
//! 之前就拒绝了未 `initialize` 的连接。
//!
//! 日志只记 method / request id / 稳定 reason。**不记**正文、item、配置凭据:
//! 旁支的正文与父会话一样是用户内容。

use std::sync::Arc;

use crate::protocol::{
    RequestEnvelope, RequestId, RpcError, Scope, SideCloseParams, SideListParams, SideListResult,
    SideOpenParams, SideReadItemsParams, SideReadItemsResult, SideSummaryWire,
    SideTurnInterruptParams, SideTurnStartParams, SideTurnStartResult,
};
use crate::rpc::{AppState, Flow};
use crate::server::{SideResponseSender, err_response, ok_response, write_response};
use crate::side_conversation::{ReplayBaseline, SideRegistry};
use yi_agent_runtime::side_conversation::{SideError, SideErrorReason};

/// 本域认领的方法(与 `side_methods_baseline.txt` 一一对应)。
pub(crate) fn owns(method: &str) -> bool {
    matches!(
        method,
        "side/open"
            | "side/list"
            | "side/readItems"
            | "side/turnStart"
            | "side/turnInterrupt"
            | "side/close"
    )
}

/// 该方法要求的最低权限档。
///
/// `open` 会创建 runner 并**读父会话内容**,所以它不是 Observe 能做的事;只读
/// 发现(`list`)与阅读(`readItems`)才是。
fn required_scope(method: &str) -> Scope {
    match method {
        "side/list" | "side/readItems" => Scope::Observe,
        _ => Scope::Control,
    }
}

pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    let method = req.method.as_str();
    if !owns(method) {
        return Ok(Flow::Unknown);
    }
    if state.client_scope < required_scope(method) {
        write_response(
            state.hub,
            state.client,
            err_response(id, RpcError::insufficient_scope(required_scope(method))),
        )
        .await?;
        return Ok(Flow::Continue);
    }

    // 参数在请求路径上解,因为「形状不对」必须立刻回给调用方,且不能占一个任务
    // 名额。读盘、装 runner、等收敛都在后面 spawn 出去。
    let registry = Arc::clone(state.side_registry);
    let responder = SideResponseSender::new(Arc::clone(state.hub), state.client.clone());
    let tasks = &mut state.side_tasks;

    match method {
        "side/open" => {
            let params: SideOpenParams = match parse(&req.params) {
                Ok(params) => params,
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?;
                    return Ok(Flow::Continue);
                }
            };
            let id_for_task = id.clone();
            tasks.spawn(async move {
                let result = registry
                    .open(&params.parent_thread_id)
                    .await
                    .map(|summary| SideSummaryWire::from_registry(&summary));
                responder.send(id_for_task, result).await;
            });
        }
        "side/list" => {
            let params: SideListParams = match parse(&req.params) {
                Ok(params) => params,
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?;
                    return Ok(Flow::Continue);
                }
            };
            // 列表是纯内存读,且客户端每 5 秒会对账一次;为它排一个任务只会让
            // 「旁支开了没有」这个判断多绕一次调度。
            let sides = registry
                .list(&params.parent_thread_id)
                .await
                .into_iter()
                .map(|summary| SideSummaryWire::from_registry(&summary))
                .collect();
            write_response(
                state.hub,
                state.client,
                ok_response(id, serde_json::json!(SideListResult { sides })),
            )
            .await?;
        }
        "side/readItems" => {
            let params: SideReadItemsParams = match parse(&req.params) {
                Ok(params) => params,
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?;
                    return Ok(Flow::Continue);
                }
            };
            // cursor 必须绑定 token:没有 token 的 cursor 看不出属于哪个快照,
            // 拼页就会把两个时刻的内容接在一起。
            if params.cursor.is_some() && params.snapshot_token.is_none() {
                write_response(
                    state.hub,
                    state.client,
                    err_response(
                        id,
                        RpcError::side(&SideError::new(
                            SideErrorReason::InvalidParams,
                            "cursor 必须与 snapshotToken 一起使用",
                        )),
                    ),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let id_for_task = id.clone();
            let replay = Arc::clone(state.side_replay);
            let connection = state.client.as_str().to_string();
            tasks.spawn(async move {
                // 分页表按连接记账,所以连接名必须在请求路径上取好再带走——
                // spawn 出去之后拿不到 `AppState`。
                let mut store = replay.lock().await;
                let result = read_items(&registry, &mut store, &connection, &params).await;
                drop(store);
                responder.send(id_for_task, result).await;
            });
        }
        "side/turnStart" => {
            let params: SideTurnStartParams = match parse(&req.params) {
                Ok(params) => params,
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?;
                    return Ok(Flow::Continue);
                }
            };
            let id_for_task = id.clone();
            tasks.spawn(async move {
                let result = registry
                    .turn_start(&params.side_id, params.text, params.client_request_id)
                    .await
                    .map(|turn_id| SideTurnStartResult { turn_id });
                responder.send(id_for_task, result).await;
            });
        }
        "side/turnInterrupt" => {
            let params: SideTurnInterruptParams = match parse(&req.params) {
                Ok(params) => params,
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?;
                    return Ok(Flow::Continue);
                }
            };
            let id_for_task = id.clone();
            tasks.spawn(async move {
                let result = registry
                    .interrupt(&params.side_id, &params.turn_id)
                    .await
                    .map(|()| serde_json::json!({}));
                responder.send(id_for_task, result).await;
            });
        }
        "side/close" => {
            let params: SideCloseParams = match parse(&req.params) {
                Ok(params) => params,
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?;
                    return Ok(Flow::Continue);
                }
            };
            let id_for_task = id.clone();
            tasks.spawn(async move {
                let result = registry
                    .close(&params.side_id, params.expected_active_turn_id.as_deref())
                    .await
                    .map(|()| serde_json::json!({}));
                responder.send(id_for_task, result).await;
            });
        }
        _ => return Ok(Flow::Unknown),
    }
    Ok(Flow::Continue)
}

/// 解请求参数,把 serde 的报错压成 `invalid_params`。
///
/// serde 的原文会带上字段名与类型,足以让客户端定位问题,且不含用户正文;
/// `deny_unknown_fields` 触发的「未知字段」也走这里。
fn parse<T: serde::de::DeserializeOwned>(params: &serde_json::Value) -> Result<T, RpcError> {
    serde_json::from_value(params.clone()).map_err(|error| {
        RpcError::side(&SideError::new(
            SideErrorReason::InvalidParams,
            format!("参数不合法：{error}"),
        ))
    })
}

/// 读一页旁支正文。
///
/// 一致性来自**一次**注册表快照:基线在锁内捕获,锁外按页序列化。`ReplayStore`
/// 只做「同一快照的下一页」,所以拼页不可能跨时刻。
async fn read_items(
    registry: &SideRegistry,
    replay: &mut crate::side_conversation::ReplayStore,
    connection: &str,
    params: &SideReadItemsParams,
) -> Result<SideReadItemsResult, SideError> {
    // 首读才捕获基线:续页必须复用已签发的快照,重读当前状态就等于拼页。
    let baseline = if params.snapshot_token.is_none() {
        let snapshot = registry.snapshot(&params.side_id).await?;
        Some(ReplayBaseline::from_snapshot(&snapshot))
    } else {
        None
    };
    let page = replay.read_page(
        connection,
        &params.side_id,
        params.snapshot_token.as_deref(),
        params.cursor.as_deref(),
        baseline,
    )?;
    // 一页的形状可以进日志,正文不行。
    crate::side_conversation::log_page(&params.side_id, page.page_len, page.finished);
    Ok(page.result)
}

#[cfg(test)]
mod tests;
