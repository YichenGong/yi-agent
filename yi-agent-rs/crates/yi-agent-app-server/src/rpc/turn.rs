//! `turn/*` 域的 RPC 处理函数。
//!
//! 从 `server.rs` 的 `serve()` dispatch 逐字搬出(Task 3 batch 2)。轮次启动/中断/中途追加;`turn/start` 仍委托带帧的 `start_turn_core`(顺序与抽取前一致,故该 helper 留在 server.rs)。
//!
//! 臂体保持原样,只有两处改名:主循环局部量改走 [`AppState`] 字段(`hub` ->
//! `state.hub` 等;字段本身已是引用,故冗余的 `&` 一并去掉),以及 `continue`
//! 换成 `return Ok(Flow::Continue)`——后者的语义由 `serve()` 翻回 `continue`,
//! 与搬移前逐字一致。

use serde_json::json;

use crate::protocol::{RequestEnvelope, RequestId, RpcError};
use crate::rpc::{AppState, Flow};
use crate::server::{
    err_response, extract_prompt, ok_response, require_thread_id, start_turn_core, store_lookup,
    write_response,
};
use crate::session::InterjectionRequest;

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
        "turn/start" => {
            // 客户端可携带它以为的上下文锚点（spec §7.1）。仅当携带时校验：
            // 桌面/老客户端不带 ⇒ 逐字节零回归。
            if let Some(anchor) = req.params.get("contextAnchor") {
                if let Some(thread_id) = req.params.get("threadId").and_then(|v| v.as_str()) {
                    let store = store_lookup(state.threads, state.workspaces, state.cfg, thread_id);
                    let current_epoch = store
                        .load(thread_id)
                        .ok()
                        .flatten()
                        .map(|l| l.meta.content_epoch);
                    let claimed_epoch = anchor.get("contentEpoch").and_then(|v| v.as_u64());
                    // 世代不符 ⇒ 客户端持有的是服务端不存在的内容。
                    let epoch_stale = matches!(
                        (claimed_epoch, current_epoch),
                        (Some(c), Some(cur)) if c != cur
                    );
                    // 锚点已不在日志中 ⇒ 同样结构性不一致。
                    // **落后于末条不算失效**：那是两端并存的正常形状。
                    let anchor_gone = match anchor.get("lastItemId").and_then(|v| v.as_str()) {
                        Some(a) => !store.contains_item_id(thread_id, a).unwrap_or(false),
                        None => false,
                    };
                    if epoch_stale || anchor_gone {
                        write_response(
                            state.hub,
                            state.client,
                            err_response(id.clone(), RpcError::stale_context(thread_id)),
                        )
                        .await?;
                        return Ok(Flow::Continue);
                    }
                }
                // 缺 threadId：交给 `prepare_turn_core` 统一报错。
            }
            // 校验 / 占 `active_turn_id` / turn/started / Running 状态 /
            // 响应 / 投递 prompt 全在 start_turn_core 内,顺序与抽取前一致;
            // `Ok(None)` 表示已写过错误响应,这里 continue。
            if start_turn_core(
                state.threads,
                state.pending_activation,
                state.uploads,
                state.hub,
                state.client,
                &req.params,
                id,
            )
            .await?
            .is_none()
            {
                return Ok(Flow::Continue);
            }
        }
        "turn/interrupt" => {
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
            let already_idle = match session.active_turn_id.clone() {
                Some(turn_id) => {
                    // 幂等 level 信号:用 try_send 避免主循环在满队列上阻塞。
                    let _ = session.interrupt_tx.try_send(turn_id);
                    false
                }
                // 没有活跃轮 = 这一轮已经结束了,本地的 Stop 是**过期状态**。
                // 只回空 `ok {}` 的话,前端无从分辨「已取消」与「本来就没在跑」:
                // `turnActive` 只由 `turn/started` / `turn/completed` 翻转,一旦
                // 后者丢失(出站队列被逐字流灌满),那枚 Stop 就永久卡住,每次点击
                // 都在这里空转。显式回一个标志,让客户端能就地清掉它。
                None => true,
            };
            write_response(
                state.hub,
                state.client,
                ok_response(id, json!({ "alreadyIdle": already_idle })),
            )
            .await?;
        }
        "turn/interject" => {
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            let text = match extract_prompt(&req.params) {
                Some(p) => p,
                None => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::invalid_params("missing or empty input text")),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            // 单值 `active_turn_id` 决定了"往哪一轮追加":没有活跃 turn
            // 就没有可并入的上下文,返回 -32013 让客户端改走 turn/start。
            let (tx, turn_id) = {
                let Some(session) = state.threads.get(&thread_id) else {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::unknown_thread(&thread_id)),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                };
                match session.active_turn_id.clone() {
                    Some(turn_id) => (session.interject_tx.clone(), turn_id),
                    None => {
                        write_response(
                            state.hub,
                            state.client,
                            err_response(id, RpcError::not_running()),
                        )
                        .await?;
                        return Ok(Flow::Continue);
                    }
                }
            };
            let interjection_id = format!("interject-{}-{}", turn_id, uuid::Uuid::new_v4());
            if tx
                .send(InterjectionRequest {
                    turn_id: turn_id.clone(),
                    interjection_id: interjection_id.clone(),
                    text,
                })
                .await
                .is_err()
            {
                // driver 已退出:没有接收方,按"没有活跃 turn"报。
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::not_running()),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            write_response(
                state.hub,
                state.client,
                ok_response(
                    id,
                    json!({
                        "turn_id": turn_id,
                        "interjection_id": interjection_id,
                    }),
                ),
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
