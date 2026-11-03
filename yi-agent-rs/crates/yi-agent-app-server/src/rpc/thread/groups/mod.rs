//! `thread` 域的 groups 组：组长会话与四个分组写操作。
//!
//! 臂体从 `rpc/thread/mod.rs` 的 `handle()` 逐字搬出（wave2 C3e）。

use crate::protocol::{Notification, RequestEnvelope, RequestId, RpcError, Scope};
use crate::rpc::{AppState, Flow};
use crate::server::{
    err_response, ok_response, resolve_thread_cwd, write_notification, write_response,
};
use serde_json::json;
use std::collections::HashSet;
use std::path::Path;

pub(crate) fn owns(method: &str) -> bool {
    matches!(
        method,
        "thread/lead"
            | "thread/group/create"
            | "thread/group/update"
            | "thread/group/setMembers"
            | "thread/group/delete"
    )
}

pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "thread/lead" => {
            // 关掉开关时这个方法**不存在**（-32601），而不是「存在但拒绝」：
            // 前端只需看 `build/info` + `flags/list`，不必再判断权限。
            //
            // 放在参数校验之前：否则关掉时传错参数会得到 `invalid_params`，
            // 让人以为是调用方的问题，而不是「这个能力没有」。
            if !state.lead_agent_enabled {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::method_not_found("thread/lead")),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            // 组长归属当前请求的项目根。与 `thread/start` 同源,保证
            // 「用户在哪个项目里问组长」与「组长管哪个项目」一致。
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
            let store = crate::thread_store::ThreadStore::new(Path::new(&cwd));
            match crate::lead::ensure_lead_meta(&store, Path::new(&cwd)) {
                Ok((meta, created)) => {
                    write_response(
                        state.hub,
                        state.client,
                        ok_response(
                            id,
                            json!({
                                "thread_id": meta.thread_id,
                                "created": created,
                            }),
                        ),
                    )
                    .await?;
                }
                Err(e) => {
                    // 看板 worktree 之类的「不是项目」是调用方问错了目录(InvalidInput),
                    // 按参数错误回;真正的 I/O 故障仍是 internal。
                    let rpc_error = if e.kind() == std::io::ErrorKind::InvalidInput {
                        RpcError::invalid_params(e.to_string())
                    } else {
                        RpcError::internal(e.to_string())
                    };
                    write_response(state.hub, state.client, err_response(id, rpc_error)).await?;
                }
            }
        }
        "thread/group/create"
        | "thread/group/update"
        | "thread/group/setMembers"
        | "thread/group/delete" => {
            if state.client_scope < Scope::Control {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::insufficient_scope(Scope::Control)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let dirs = state.workspaces.list();
            match apply_group_mutation(&req.method, &req.params, &dirs) {
                Ok((workspace, result)) => {
                    write_response(state.hub, state.client, ok_response(id, result)).await?;
                    // 广播给所有客户端（含本端）：列表层恒推，各自重拉
                    // `thread/listAll` 才能看到新组——通知本身不带组表。
                    let _ =
                        write_notification(state.hub, &Notification::GroupsChanged { workspace })
                            .await;
                }
                Err(e) => {
                    write_response(state.hub, state.client, err_response(id, e)).await?;
                }
            }
        }
        _ => return Ok(Flow::Unknown),
    }
    Ok(Flow::Continue)
}

/// 四个分组写操作的公共实现。
///
/// 成功返回 `(workspace, 该方法的应答体)`：`create` 需要回传新建的组（客户端
/// 用它做后续编辑），其余三个回空对象。失败一律 `RpcError`——参数类错误走
/// `invalid_params`（-32602），含 `unknown group: {id}`；IO 失败走 `internal`。
fn apply_group_mutation(
    method: &str,
    params: &serde_json::Value,
    dirs: &[String],
) -> Result<(String, serde_json::Value), RpcError> {
    let workspace = params
        .get("workspace")
        .and_then(|v| v.as_str())
        .ok_or_else(|| RpcError::invalid_params("missing workspace"))?;
    // workspace 必须是已索引目录：分组是目录级概念，写进未知目录等于制造一个
    // 永远显示不出来的组。
    if !dirs.iter().any(|d| d == workspace) {
        return Err(RpcError::invalid_params(format!(
            "workspace is not indexed: {workspace}"
        )));
    }
    let ws = Path::new(workspace);

    // `threadIds` 必须是字符串数组且无重复。重复 id 在成员表里没有意义，
    // 且会让 `prune` 之后的计数看起来像两条会话。
    let thread_ids = |params: &serde_json::Value| -> Result<Vec<String>, RpcError> {
        let raw = params
            .get("threadIds")
            .cloned()
            .unwrap_or(serde_json::Value::Array(Vec::new()));
        let ids: Vec<String> = serde_json::from_value(raw)
            .map_err(|e| RpcError::invalid_params(format!("threadIds must be strings: {e}")))?;
        let mut seen = HashSet::new();
        for id in &ids {
            if !seen.insert(id.clone()) {
                return Err(RpcError::invalid_params(format!(
                    "duplicate threadId: {id}"
                )));
            }
        }
        Ok(ids)
    };

    // 成员必须真的在这个目录里。用目录的 meta 列表判定，与
    // `build_list_all_response` 的过滤口径一致（同一个 `ThreadStore::list`）。
    let members_ok = |ids: &[String]| -> Result<(), RpcError> {
        let live: HashSet<String> = crate::thread_store::ThreadStore::new(ws)
            .list()
            .map_err(|e| RpcError::internal(e.to_string()))?
            .into_iter()
            .map(|m| m.thread_id)
            .collect();
        if let Some(bad) = ids.iter().find(|id| !live.contains(*id)) {
            return Err(RpcError::invalid_params(format!(
                "thread is not in this workspace: {bad}"
            )));
        }
        Ok(())
    };

    match method {
        "thread/group/create" => {
            let name = crate::group_store::validate_name(
                params.get("name").and_then(|v| v.as_str()).unwrap_or(""),
            )
            .map_err(RpcError::invalid_params)?;
            let description = crate::group_store::validate_description(
                params
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
            )
            .map_err(RpcError::invalid_params)?;
            let ids = thread_ids(params)?;
            if ids.is_empty() {
                return Err(RpcError::invalid_params("threadIds must not be empty"));
            }
            members_ok(&ids)?;
            let group = crate::group_store::Group {
                id: crate::group_store::new_id(),
                name,
                description,
                thread_ids: ids,
            };
            let created = group.clone();
            crate::group_store::mutate(ws, |groups| groups.push(group))
                .map_err(|e| RpcError::internal(e.to_string()))?;
            Ok((
                workspace.to_string(),
                json!({
                    "group": {
                        "id": created.id,
                        "name": created.name,
                        "description": created.description,
                        "threadIds": created.thread_ids,
                    }
                }),
            ))
        }
        "thread/group/update" => {
            let group_id = require_group_id(params)?;
            let name = params
                .get("name")
                .and_then(|v| v.as_str())
                .map(crate::group_store::validate_name)
                .transpose()
                .map_err(RpcError::invalid_params)?;
            let description = params
                .get("description")
                .and_then(|v| v.as_str())
                .map(crate::group_store::validate_description)
                .transpose()
                .map_err(RpcError::invalid_params)?;
            let mut found = false;
            crate::group_store::mutate(ws, |groups| {
                if let Some(group) = groups.iter_mut().find(|g| g.id == group_id) {
                    found = true;
                    if let Some(name) = &name {
                        group.name = name.clone();
                    }
                    if let Some(description) = &description {
                        group.description = description.clone();
                    }
                }
            })
            .map_err(|e| RpcError::internal(e.to_string()))?;
            if !found {
                return Err(RpcError::invalid_params(format!(
                    "unknown group: {group_id}"
                )));
            }
            Ok((workspace.to_string(), json!({})))
        }
        "thread/group/setMembers" => {
            let group_id = require_group_id(params)?;
            let ids = thread_ids(params)?;
            members_ok(&ids)?;
            let mut found = false;
            crate::group_store::mutate(ws, |groups| {
                if let Some(group) = groups.iter_mut().find(|g| g.id == group_id) {
                    found = true;
                    group.thread_ids = ids.clone();
                }
                // 空成员表 = 解散（spec「不变量 3」）。这一步必须在写路径做：
                // 读路径只过滤不下发，磁盘条目要等一次写才真正消失。
                groups.retain(|g| !g.thread_ids.is_empty());
            })
            .map_err(|e| RpcError::internal(e.to_string()))?;
            if !found {
                return Err(RpcError::invalid_params(format!(
                    "unknown group: {group_id}"
                )));
            }
            Ok((workspace.to_string(), json!({})))
        }
        "thread/group/delete" => {
            let group_id = require_group_id(params)?;
            let mut found = false;
            crate::group_store::mutate(ws, |groups| {
                let before = groups.len();
                groups.retain(|g| g.id != group_id);
                found = groups.len() != before;
            })
            .map_err(|e| RpcError::internal(e.to_string()))?;
            if !found {
                return Err(RpcError::invalid_params(format!(
                    "unknown group: {group_id}"
                )));
            }
            Ok((workspace.to_string(), json!({})))
        }
        other => Err(RpcError::internal(format!(
            "unhandled group mutation: {other}"
        ))),
    }
}

/// `groupId` 是 update / setMembers / delete 的必填项。
fn require_group_id(params: &serde_json::Value) -> Result<String, RpcError> {
    params
        .get("groupId")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| RpcError::invalid_params("missing groupId"))
}
