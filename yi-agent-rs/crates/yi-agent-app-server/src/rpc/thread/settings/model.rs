//! `thread` 域的 settings 组：模型与权限、推理强度两类开关。

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
        "thread/setModel" => {
            // 与 `model/*` 写路径同档:切模型会改 provider/凭据,需 `Control`。
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
            // `name: null`(或缺失)= 清覆盖;字符串 = 选显示名。非字符串/非
            // null 一律 `-32602`,不静默当作清覆盖。
            let name = match req.params.get("name") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(n)) => Some(n.clone()),
                Some(_) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::invalid_params("name must be a string or null"),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            // 会话覆盖必须命中清单条目,否则 `model_not_found` 且零副作用。
            // 读清单路径走注入的 `models_path`(与 `model/*` 同一份),
            // 故 upsert 刚落盘的条目立刻可见。
            let catalog = yi_agent_runtime::models::load_catalog_from(state.models_path);
            if let Some(n) = &name
                && !catalog.models.iter().any(|m| &m.name == n)
            {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::model_not_found(n)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            // 生效串按 清单条目 → 全局默认 → cfg 解析;校验通过后再落盘。
            let effective =
                yi_agent_runtime::models::resolve_effective(state.cfg, &catalog, name.as_deref());
            // 先让在场 driver 用新覆盖重建 agent,成功后再落盘。铁律
            // 「上报/落盘 == 实际运行」:若 provider 重建失败(build_provider
            // 抛错),agent 仍跑旧模型,此时若已落盘就会让磁盘与响应谎报新模型
            // (任务 7 评审 Finding 3)。故先重建、后落盘;失败即回错误且零磁盘
            // 副作用,由客户端重试。
            //
            // 冷 thread(不在内存)没有 driver 可发:只能先落盘,resume 时会按
            // meta.model_ref 重建,故跳过本段。
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
                        .send(SessionCommand::SetModel {
                            effective: Box::new(effective.clone()),
                            model_ref: name.clone(),
                            reply: reply_tx,
                        })
                        .await
                };
                // reply 为 `Ok(Err(_))` 表示 driver 重建失败;发送失败(通道关闭)
                // 同样意味着无法保证生效,一并按失败处理。
                let switched = send_result.is_ok() && matches!(reply_rx.await, Ok(Ok(())));
                if !switched {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::internal(
                                "the live session could not switch to the requested \
                                 model; it is still running the previous one"
                                    .to_string(),
                            ),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            }
            // 重建成功后(冷 thread 则直接)落盘:一次原子写把 `model_ref` 与
            // 生效 `model` 同进同出,避免并发 `touch` 读到自相矛盾的组合。
            let store = store_lookup(state.threads, state.workspaces, state.cfg, &thread_id);
            if let Err(e) = store.set_model(&thread_id, name.as_deref(), &effective.model) {
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
            // 让内存里的 `ThreadSession.model` 与实际生效值保持一致。注意:
            // `thread/list(All)` 目前是从**盘上 meta**(`thread_summary_json`)
            // 渲染 model 的,`ThreadSession.model` 在当前 crate 内尚无读取方;
            // 这里仍同步刷新,是为了让内存视图与刚落盘的 meta 同源、不残留旧串
            // (供将来的读取方或排查时使用),而不是因为 list 会读它。
            if let Some(session) = state.threads.get_mut(&thread_id) {
                session.model = effective.model.clone();
            }
            write_response(
                state.hub,
                state.client,
                ok_response(id, json!({ "ok": true, "model": effective.model })),
            )
            .await?;
        }
        "thread/setPermissionMode" => {
            // 与 `thread/setModel` 同档:Control 起。Observe 拿到的是
            // 掩码级视图,不得放开 OS 沙箱。
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
            // mode 校验先于 thread 存在性:非法 mode 一律 `-32602`,
            // 不因 threadId 未知而改变错误码。
            let mode = match req.params.get("mode").and_then(|v| v.as_str()) {
                Some("normal") => crate::thread_store::ThreadMode::Normal,
                Some("yolo") => crate::thread_store::ThreadMode::Yolo,
                _ => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::invalid_params("mode must be \"normal\" or \"yolo\""),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            // 仅内存中的活跃线程可切换:UI 的 mode chip 只对当前打开的线程可见,而该
            // 线程必先经 thread/start 或 thread/resume 进入内存。冷线程没有运行期
            // switch 可翻转(其持久化模式在 resume 时被读取),故此处不为其落盘。
            let Some(session) = state.threads.get(&thread_id) else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::unknown_thread(&thread_id)),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            // 运行期开关立即生效:与权限层、沙箱共享同一 `Arc`,故
            // 无需重建 agent 本轮即可放行。
            session
                .yolo
                .set(mode == crate::thread_store::ThreadMode::Yolo);
            // 最佳努力落盘:运行期开关已生效,落盘失败不阻断本轮切换。
            match session.store.set_permission_mode(&thread_id, mode) {
                Ok(true) => {}
                Ok(false) => eprintln!(
                    "[app-server] permission_mode not persisted for {thread_id}: thread meta missing"
                ),
                Err(e) => {
                    eprintln!("[app-server] failed to persist permission_mode for {thread_id}: {e}")
                }
            }
            // 广播给所有客户端(列表层恒推):桌面改 → 手机 chip 同步,
            // 手机改 → 桌面 chip 同步。与 `setModel` 的 ModelChanged
            // 广播同一手法。
            let _ = write_notification(
                state.hub,
                &Notification::PermissionModeChanged {
                    thread_id: thread_id.clone(),
                    mode,
                },
            )
            .await;
            write_response(state.hub, state.client, ok_response(id, json!({}))).await?;
        }
        _ => return Ok(Flow::Unknown),
    }
    Ok(Flow::Continue)
}
