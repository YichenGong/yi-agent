//! `model/*` 域的 RPC 处理函数。
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
        // 机器级模型清单（`~/.yi-agent/models.json`），不带 project：
        // 与 `ui/settings/*` 同类。`model/list` 只需 `Observe`（回掩码，
        // 永不含明文 key）；写操作与既有写路径同档，需 `Control`。
        "model/list" => {
            match crate::model_rpc::handle_model_request_at(
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
        "model/upsert" | "model/delete" | "model/setDefault" | "model/setSubagent"
        | "model/importEnv" => {
            if state.client_scope < Scope::Control {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::insufficient_scope(Scope::Control)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            // upsert 可能让旧能力失效（连接字段变了 → 清空），需要在**写之前**
            // 拿一份旧清单做对比。其它方法不碰能力，不必付这次读盘。
            let is_upsert = req.method == "model/upsert";
            let before =
                is_upsert.then(|| yi_agent_runtime::models::load_catalog_from(state.models_path));
            match crate::model_rpc::handle_model_request_at(
                state.models_path,
                req.method.as_str(),
                &req.params,
                state.cfg,
            ) {
                Ok(value) => {
                    write_response(state.hub, state.client, ok_response(id, value)).await?;
                    if let Some(before) = before {
                        maybe_probe_after_upsert(state, &req.params, &before);
                    }
                }
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?
                }
            }
        }
        // 能力探测：后台跑（一轮最长 30s），先回 `started` 再广播结果。
        // 与 `model/test` 同档（触及密钥，需 `Control`），因 async 而单独开分支。
        "model/probe" => {
            if state.client_scope < Scope::Control {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::insufficient_scope(Scope::Control)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let name = req
                .params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let catalog = yi_agent_runtime::models::load_catalog_from(state.models_path);
            let Some(entry) = catalog.models.iter().find(|m| m.name == name) else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::model_not_found(&name)),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            // 已在探测中：直接回 false，不打第二轮请求。
            if !state.probes.try_begin(&name) {
                write_response(
                    state.hub,
                    state.client,
                    ok_response(id, serde_json::json!({ "started": false })),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            spawn_probe(
                state.hub,
                state.probes,
                state.models_path,
                name,
                entry.clone(),
            );
            write_response(
                state.hub,
                state.client,
                ok_response(id, serde_json::json!({ "started": true })),
            )
            .await?;
        }
        // 探测只发一次最小请求、**不落盘**：与 `onboarding/test` 同档
        // （触及密钥，需 Control），且因探测是 async 而单独开分支。
        "model/test" => {
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
                match crate::model_rpc::resolve_test_settings(state.models_path, &req.params) {
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

/// 探测后写回并广播。**不阻塞**调用方：一轮探测最长 30s，放在 RPC 的
/// await 链上会把整个连接（以及同一 task 上的其它请求）堵死。
///
/// `ProbeRegistry` 用 clone 而非引用移进 task：`AppState` 里的 `&ProbeRegistry`
/// 活不过这个函数，而 `tokio::spawn` 要求 `'static`。内部是 `Arc<Mutex<..>>`，
/// clone 仍指向同一张表。
pub(crate) fn spawn_probe(
    hub: &std::sync::Arc<crate::broadcast::Broadcaster>,
    probes: &crate::model_rpc::ProbeRegistry,
    path: &std::path::Path,
    name: String,
    entry: yi_agent_runtime::models::ModelEntry,
) {
    let hub = std::sync::Arc::clone(hub);
    let probes = probes.clone();
    let path = path.to_path_buf();
    let fingerprint = crate::model_rpc::entry_fingerprint(&entry);
    tokio::spawn(async move {
        let outcome = yi_agent_runtime::reasoning_probe::probe_entry(
            entry.provider,
            &entry.api_url,
            &entry.api_key,
            &entry.model,
            entry.api_style.as_deref(),
        )
        .await;
        let applied = crate::model_rpc::apply_probe_result(&path, &name, &fingerprint, outcome);
        probes.finish(&name);
        if !applied {
            // 条目删了、或探测期间连接字段被改：旧结果作废，不广播。
            return;
        }
        let caps = yi_agent_runtime::models::load_catalog_from(&path)
            .models
            .into_iter()
            .find(|m| m.name == name)
            .and_then(|m| m.reasoning_capabilities);
        let _ = crate::server::write_notification(
            &hub,
            &crate::protocol::Notification::ModelCapabilitiesChanged {
                name,
                reasoning_capabilities: caps,
            },
        )
        .await;
    });
}

/// upsert 成功后的探测判定。
///
/// 触发条件只有两种：**新建条目**（能力必为空），或**连接字段变了**（旧能力
/// 已被 upsert 清空）。只改密钥、只改 `reasoning_effort` 都不该重测——前者不
/// 影响档位支持，后者是用户偏好而非模型能力。
fn maybe_probe_after_upsert(
    state: &crate::rpc::AppState<'_>,
    params: &serde_json::Value,
    before: &yi_agent_runtime::models::ModelCatalog,
) {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if name.is_empty() {
        return;
    }
    let after = yi_agent_runtime::models::load_catalog_from(state.models_path);
    let Some(entry) = after.models.iter().find(|m| m.name == name) else {
        return;
    };
    let fingerprint_changed = match before.models.iter().find(|m| m.name == name) {
        Some(old) => {
            crate::model_rpc::entry_fingerprint(old) != crate::model_rpc::entry_fingerprint(entry)
        }
        None => true,
    };
    let no_capabilities = entry.reasoning_capabilities.is_none();
    if !(fingerprint_changed || no_capabilities) {
        return;
    }
    // 已经在探测（比如用户刚点了「重新探测」）：别插队，那一轮结果对本条目的
    // 连接字段仍然有效，upsert 没有改动它们。
    if !state.probes.try_begin(name) {
        return;
    }
    spawn_probe(
        state.hub,
        state.probes,
        state.models_path,
        name.to_string(),
        entry.clone(),
    );
}
