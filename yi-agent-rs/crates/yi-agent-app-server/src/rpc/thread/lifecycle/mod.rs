//! `thread` 域的 lifecycle 组：新建、恢复、删除、分叉、清空。
//!
//! 臂体从 `rpc/thread/mod.rs` 的 `handle()` 逐字搬出（wave2 C3d）。`resume` 与
//! `delete`（含 `fork` / `clear`）各自成片，`start` 留在本文件。

use crate::protocol::{Notification, RequestEnvelope, RequestId, RpcError};
use crate::rpc::{AppState, Flow};
use crate::server::{
    err_response, linked_extra_roots, ok_response, resolve_thread_cwd, start_thread_core,
    with_session_extra_roots, with_session_model_ref, with_session_preserve_thinking,
    with_session_reasoning_effort, write_notification, write_response,
};
use serde_json::json;
use std::path::Path;

pub(crate) mod delete;
pub(crate) mod resume;

pub(crate) fn owns(method: &str) -> bool {
    matches!(
        method,
        "thread/start" | "thread/resume" | "thread/fork" | "thread/delete" | "thread/clear"
    )
}

pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "thread/resume" => return resume::handle(state, req, id).await,
        "thread/fork" | "thread/delete" | "thread/clear" => {
            return delete::handle(state, req, id).await;
        }
        _ => {}
    }
    match req.method.as_str() {
        "thread/start" => {
            let thread_id = format!("thread-{}", uuid::Uuid::new_v4());

            // 目录决定 agent / store / 权限 / 沙箱 / skills 的根。
            let cwd = match resolve_thread_cwd(
                &req.params,
                state.cfg,
                state.hub,
                state.client,
                id.clone(),
            )
            .await?
            {
                Some(c) => c,
                None => return Ok(Flow::Continue),
            };

            // 新线程一律以 Normal 起步;同一值既用于建 agent,也落盘 meta,
            // 抽成局部量避免两处字面量漂移。
            let mode = crate::thread_store::ThreadMode::Normal;

            // 可选的会话级模型覆盖(清单显示名)。wire 用 camelCase
            // `modelRef`,兼容 snake_case `model_ref`;二者都缺省即
            // `None`(跟随清单全局默认,再回退 cfg)。
            let model_ref = req
                .params
                .get("modelRef")
                .or_else(|| req.params.get("model_ref"))
                .and_then(|v| v.as_str())
                .map(str::to_string);

            // 本次会话的关联根：`thread/start` 从索引快照一次，之后**只**用这个局部量
            // ——既进工厂槽位（决定沙箱/工具/权限的根集），也进 meta（落盘快照）与
            // `attach_delegation`（委派路径的根集）。三处必须同源，故不各自重读索引。
            let extra_roots = linked_extra_roots(state.workspaces, &cwd);

            // 模型可用性在**建 agent 之前**判:建 agent 会做大量装配,失败信息会被
            // 压成 internal;这里直接给 UI 一个可分支的错误码。
            //
            // 判据是「解析后的生效 model 是否为空」,而不是「有没有清单条目」:
            // `effective_entry_for` 在整包注入时返回 `None`(表示注入压过清单),
            // 那条路径的 model 来自 `cfg.model`,非空就是合法配置。Task 4 已保证
            // 「无条目且无注入」时 `cfg.model == ""`,所以一个判据覆盖三种情况:
            //   有条目 -> 条目.model;无条目+注入 -> cfg.model;两者都无 -> 空串。
            if let Some(error) = missing_model_error(state, model_ref.as_deref()) {
                write_response(state.hub, state.client, err_response(id, error)).await?;
                return Ok(Flow::Continue);
            }

            // 本次会话的 `model_ref` 经 thread-local 交给工厂解析 provider /
            // api_url / api_key / model(命中清单默认则用条目,否则回退 cfg)。
            // 关联根同路经 thread-local 传入:工厂是按目录构建、被同目录多个
            // 会话共享的,把根集写进共享 `cfg` 会让它泄漏给别的会话。
            let built = match with_session_model_ref(model_ref.clone(), || {
                with_session_extra_roots(extra_roots.clone(), || {
                    with_session_reasoning_effort(None, || {
                        with_session_preserve_thinking(true, || {
                            (state.build_agent)(None, Path::new(&cwd), mode)
                        })
                    })
                })
            }) {
                Ok(a) => a,
                Err(e) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(e.to_string())),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };

            // 落盘/上报的生效串取自**实际建出的 agent**,而非另按清单解析一次。
            // `production_factory` 只在进程启动时快照一次清单,运行期经
            // `model/upsert` 新增的条目不在其快照内;此处若再读一次磁盘,就可能
            // 上报一个工厂实际未采用的模型(任务 7 评审 Finding 1)。用
            // `agent.config().model` 才能保证「上报 == 实际运行」。
            let model = built.agent.config().model.clone();

            // 与 `model` 同源:上报**实际建出**的 agent 的生效档位。
            // 用硬编码 null 会在条目带默认档时谎报(本会话其实跑着该档),
            // 且与 resume 的口径不一致。
            let reasoning_effort = built.agent.config().gen_params.reasoning_effort.clone();

            // 与 `model` 同源:上报**实际建出**的 agent 的开关值。
            let preserve_thinking = built.agent.config().preserve_thinking;

            // 内核负责「attach_delegation → 建 driver 通道 → insert 到
            // `threads` → spawn 守望者与 driver」;RPC 层的通知与响应仍在
            // 本分支完成,与抽取前逐字一致。
            start_thread_core(
                state.threads,
                state.pending_activation,
                state.process_watches,
                state.runtimes,
                state.thread_roots,
                state.managed,
                state.models_path,
                state.bringup,
                state.cfg,
                &cwd,
                mode,
                thread_id.clone(),
                // 本会话的关联根：上面已捕获的同一个局部量，内核据此写
                // meta 并传给 `attach_delegation`（不在那里重读槽位）。
                extra_roots,
                built,
                state.hub,
                state.client,
                state.turn_tx,
                state.pending,
                state.permission_timeout,
                state.perm_seq,
                state.theme_handle,
                state.git_diff_handle,
                state.workspaces,
                model.clone(),
                model_ref.clone(),
                None,
                None,
                state.lead_inbox,
                state.lead_replies,
                state.lead_control,
                state.lead_control_replies,
                Some(state.lead_agent_enabled),
                // RPC 起的会话没有驱动方句柄，不额外注入环境变量。
                Vec::new(),
                // RPC 起的会话不绑插件（绑定由驱动方按已提交激活算好传入）。
                Vec::new(),
                state.live_settings,
                state.live_side_parents,
            )
            .await?;

            write_notification(
                state.hub,
                &Notification::ThreadStarted {
                    thread_id: thread_id.clone(),
                    cwd: cwd.clone(),
                    model: model.clone(),
                    model_ref: model_ref.clone(),
                },
            )
            .await?;
            write_response(
                state.hub,
                state.client,
                ok_response(
                    id,
                    json!({
                        "thread_id": thread_id,
                        "cwd": cwd,
                        "model": model,
                        "model_ref": model_ref,
                        "permission_mode": mode,
                        // 新会话无覆盖 ⇒ 上报实际运行值(解析出的条目默认
                        // 或 cfg;都没有则 null),与实际建出的 agent 同源。
                        "reasoning_effort": reasoning_effort,
                        "preserve_thinking": preserve_thinking,
                    }),
                ),
            )
            .await?;
        }
        _ => return Ok(Flow::Unknown),
    }
    Ok(Flow::Continue)
}

/// 模型未配置时返回可分支的错误；配置可用时返回 `None`。
///
/// 判据只看「解析后的生效 model 是否为空」，不看清单是否为空：整包注入
/// （`--model` + `--api-key`）绕开清单，`cfg.model` 非空就是合法配置。
/// `cfg.model` 在"既无条目又无注入"时由 `RuntimeConfig::load` 留空，
/// 于是这一个判据同时覆盖两条来源。
// 被 `start`（同文件）与 `resume`（`resume.rs`）共用，故放开到 `pub(super)`。
pub(super) fn missing_model_error(
    state: &AppState<'_>,
    session_model_ref: Option<&str>,
) -> Option<RpcError> {
    let catalog = yi_agent_runtime::models::load_catalog_from(state.models_path);
    let effective =
        yi_agent_runtime::models::resolve_effective(state.cfg, &catalog, session_model_ref);
    if !effective.model.trim().is_empty() {
        return None;
    }
    // 原因列表留给 UI 展示"还差哪几项"，与 `onboarding/status` 同源。
    let reasons = match yi_agent_runtime::onboarding::assess(Some(&catalog)) {
        yi_agent_runtime::onboarding::Assessment::Needed { reasons } => reasons
            .into_iter()
            .map(|m| crate::onboarding_rpc::missing_name(m).to_string())
            .collect(),
        yi_agent_runtime::onboarding::Assessment::Ready => Vec::new(),
    };
    Some(RpcError::model_not_configured(&reasons))
}
