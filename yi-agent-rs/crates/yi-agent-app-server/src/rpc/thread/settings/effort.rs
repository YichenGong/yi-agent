//! `thread` 域的 settings 组：推理强度与思考保留开关。

use crate::protocol::{Notification, RequestEnvelope, RequestId, RpcError, Scope};
use crate::rpc::{AppState, Flow};
use crate::server::{
    err_response, ok_response, require_thread_id, store_lookup, write_notification, write_response,
};
use crate::session::SessionCommand;
use serde_json::json;
use tokio::sync::oneshot;

pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "thread/setReasoningEffort" => {
            // 与 `thread/setModel` 同档:effort 会改推理行为(能力面/计费),
            // 需 `Control`。
            if state.client_scope < Scope::Control {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::insufficient_scope(Scope::Control)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            // `effort: null`(或缺失)= 清覆盖;字符串必须在七档白名单内,
            // 否则 `-32602`;其余 JSON 类型同样 `-32602`,不静默当作清覆盖。
            // 白名单与 `model_rpc::upsert` 的条目校验逐字一致。
            let effort = match req.params.get("effort") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(s)) => {
                    if !matches!(
                        s.as_str(),
                        "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
                    ) {
                        write_response(
                            state.hub,
                            state.client,
                            err_response(
                                id,
                                RpcError::invalid_params(
                                    "effort must be one of \
                                     none|minimal|low|medium|high|xhigh|max",
                                ),
                            ),
                        )
                        .await?;
                        return Ok(Flow::Continue);
                    }
                    Some(s.clone())
                }
                Some(_) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::invalid_params("effort must be a string or null"),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            // 条目有能力数据时只接受它支持的档位。能力未知（未探测/探测失败）
            // 时保持七档白名单——与探测功能上线前的行为一致，不能因为「还没测」
            // 就把用户挡在门外。
            //
            // `model_ref` 取该会话自己的覆盖：`thread/setModel` 落了什么，这里
            // 就校验什么。拿不到 ref（冷 thread 的手写 meta、或跟随全局默认）
            // 时回退到全局默认条目——那正是这次设置会生效到的那一条。
            if let Some(effort) = effort.as_deref() {
                let store = store_lookup(state.threads, state.workspaces, state.cfg, &thread_id);
                let model_ref = store
                    .load(&thread_id)
                    .ok()
                    .flatten()
                    .and_then(|loaded| loaded.meta.model_ref);
                let catalog = yi_agent_runtime::models::load_catalog_from(state.models_path);
                if let Some(entry) = yi_agent_runtime::models::effective_entry_for(
                    state.cfg,
                    &catalog,
                    model_ref.as_deref(),
                ) {
                    if let Some(allowed) = yi_agent_runtime::models::supported_efforts(entry) {
                        if !allowed.iter().any(|s| s == effort) {
                            let supported = if allowed.is_empty() {
                                "(none)".to_string()
                            } else {
                                allowed.join(", ")
                            };
                            write_response(
                                state.hub,
                                state.client,
                                err_response(
                                    id,
                                    RpcError::invalid_params(format!(
                                        "reasoning effort '{effort}' is not supported by this \
                                         model; supported: {supported}"
                                    )),
                                ),
                            )
                            .await?;
                            return Ok(Flow::Continue);
                        }
                    }
                }
            }
            // 先让在场 driver 用新 effort 重建 agent（复用当前 provider——
            // effort 不烤进 URL/key），成功后再落盘。铁律「上报/落盘 == 实际
            // 运行」同 setModel:若重建失败,agent 仍跑旧档位,此时落盘就会让
            // 磁盘与响应谎报。故先重建、后落盘;失败即回错误且零磁盘副作用。
            //
            // 冷 thread(不在内存)没有 driver 可发:只能先落盘,resume 时会按
            // meta.reasoning_effort 重建,故跳过本段。
            if state.threads.contains_key(&thread_id) {
                let (reply_tx, reply_rx) = oneshot::channel();
                // 借用只活在下面这个块内(send 的 future 借 `session`),
                // 出块即释放,后续 `threads.get_mut` 才不被占用。
                let send_result = {
                    let session = state
                        .threads
                        .get(&thread_id)
                        .expect("contains_key was just checked");
                    session
                        .session_tx
                        .send(SessionCommand::SetReasoningEffort {
                            effort: effort.clone(),
                            reply: reply_tx,
                        })
                        .await
                };
                // reply 为 `Ok(Err(_))` 表示 driver 重建失败;发送失败(通道关闭)
                // 同样意味着无法保证生效,一并按失败处理。
                let applied = send_result.is_ok() && matches!(reply_rx.await, Ok(Ok(())));
                if !applied {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::internal(
                                "the live session could not switch to the requested \
                                 reasoning effort; it is still running the previous one"
                                    .to_string(),
                            ),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            }
            // 重建成功后(冷 thread 则直接)落盘:读-改-写 + 原子写,与
            // `set_model` 同锁,避免并发 `touch` / `rename` 互相覆盖。
            let store = store_lookup(state.threads, state.workspaces, state.cfg, &thread_id);
            if let Err(e) = store.set_reasoning_effort(&thread_id, effort.as_deref()) {
                if e.kind() == std::io::ErrorKind::NotFound {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::unknown_thread(&thread_id)),
                    )
                    .await?;
                } else {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(e.to_string())),
                    )
                    .await?;
                }
                return Ok(Flow::Continue);
            }
            // 列表层恒推(同 `thread/permissionModeChanged`):桌面改 → 手机
            // effort 控件同步,反之亦然。
            let _ = write_notification(
                state.hub,
                &Notification::ReasoningEffortChanged {
                    thread_id: thread_id.clone(),
                    effort: effort.clone(),
                },
            )
            .await;
            write_response(
                state.hub,
                state.client,
                ok_response(id, json!({ "ok": true, "effort": effort })),
            )
            .await?;
        }
        "thread/setPreserveThinking" => {
            // 与 `thread/setReasoningEffort` 同档:开关会改请求体(是否回传
            // 历史轮的推理凭证),需 `Control`。
            if state.client_scope < Scope::Control {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::insufficient_scope(Scope::Control)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            // `preserve` 必须是 JSON 布尔;缺失/null/字符串/数字一律
            // `-32602`,**不**静默取默认值——静默默认会让调用方以为开关
            // 已按其所愿设置,而实际可能停在相反的值。
            let preserve = match req.params.get("preserve") {
                Some(serde_json::Value::Bool(b)) => *b,
                _ => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::invalid_params("preserve must be a boolean")),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            // 先让在场 driver 用新开关重建 agent(复用当前 provider——
            // 开关不烤进 URL/key),成功后再落盘。铁律「上报/落盘 == 实际
            // 运行」同 setReasoningEffort:若重建失败,agent 仍跑旧值,此时
            // 落盘就会让磁盘与响应谎报。故先重建、后落盘;失败即回错误且
            // 零磁盘副作用。
            //
            // 冷 thread(不在内存)没有 driver 可发:只能先落盘,resume 时会按
            // meta.preserve_thinking 重建,故跳过本段。
            if state.threads.contains_key(&thread_id) {
                let (reply_tx, reply_rx) = oneshot::channel();
                // 借用只活在下面这个块内(send 的 future 借 `session`),
                // 出块即释放,后续 `threads.get_mut` 才不被占用。
                let send_result = {
                    let session = state
                        .threads
                        .get(&thread_id)
                        .expect("contains_key was just checked");
                    session
                        .session_tx
                        .send(SessionCommand::SetPreserveThinking {
                            preserve,
                            reply: reply_tx,
                        })
                        .await
                };
                // reply 为 `Ok(Err(_))` 表示 driver 重建失败;发送失败(通道关闭)
                // 同样意味着无法保证生效,一并按失败处理。
                let applied = send_result.is_ok() && matches!(reply_rx.await, Ok(Ok(())));
                if !applied {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::internal(
                                "the live session could not switch the \
                                 preserve-thinking flag; it is still running the \
                                 previous value"
                                    .to_string(),
                            ),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            }
            // 重建成功后(冷 thread 则直接)落盘:读-改-写 + 原子写,与
            // `set_reasoning_effort` 同锁,避免并发 `touch` / `rename` 互相覆盖。
            let store = store_lookup(state.threads, state.workspaces, state.cfg, &thread_id);
            if let Err(e) = store.set_preserve_thinking(&thread_id, preserve) {
                if e.kind() == std::io::ErrorKind::NotFound {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::unknown_thread(&thread_id)),
                    )
                    .await?;
                } else {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(e.to_string())),
                    )
                    .await?;
                }
                return Ok(Flow::Continue);
            }
            // 列表层恒推(同 `thread/reasoningEffortChanged`):桌面改 → 手机
            // 控件同步,反之亦然。
            let _ = write_notification(
                state.hub,
                &Notification::PreserveThinkingChanged {
                    thread_id: thread_id.clone(),
                    preserve,
                },
            )
            .await;
            write_response(
                state.hub,
                state.client,
                ok_response(id, json!({ "ok": true, "preserve": preserve })),
            )
            .await?;
        }
        _ => return Ok(Flow::Unknown),
    }
    Ok(Flow::Continue)
}
