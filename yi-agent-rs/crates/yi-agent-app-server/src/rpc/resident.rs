//! `resident/list` 域的 RPC 处理函数。
//!
//! 「哪些项目要常驻 daemon」的**通用**回答口。宿主不认识任何具体插件/看板：
//! 数据源是 `yi-agent-store` 的通用常驻登记（`$HOME/.yi-agent/resident-daemons.json`，
//! 字段只有 `project` 与 `required_by`），故这一域不含任何插件语义。
//!
//! 纯文件读取：不需要 daemon 在跑，也不需要先建过看板——登记表本身就是答案。
//! 读侧与 `yi_agent_store::resident` 同口径地宽容（缺失/损坏 → 空表），
//! 因为一个坏文件不该让侧栏起不来。

use std::path::Path;

use serde_json::json;

use crate::protocol::{RequestEnvelope, RequestId};
use crate::rpc::{AppState, Flow};
use crate::server::{ok_response, write_response};

/// 已登记项目，形状即线格式：`{"projects":[{"project":..,"required_by":[..]}]}`。
///
/// 包装成对象（而不是裸数组）与登记表文件本身一致：两者日后都能加键而不破坏
/// 既有读者。`required_by` 如实带出「谁在需要它」——它是通用的，故不解释这些
/// 名字的含义。
pub(crate) fn projects_payload(resident_dir: &Path) -> serde_json::Value {
    let projects: Vec<serde_json::Value> = yi_agent_store::resident::entries(resident_dir)
        .into_iter()
        .map(|entry| {
            json!({
                "project": entry.project.to_string_lossy(),
                "required_by": entry.required_by,
            })
        })
        .collect();
    json!({ "projects": projects })
}

/// 处理本域的一个 RPC 请求。
///
/// 返回 [`Flow::Continue`] 表示「本域已应答，主循环回到下一轮」；
/// [`Flow::Unknown`] 表示方法不在本域内，交回 `serve()` 的原始 dispatch。
pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "resident/list" => {
            let payload = projects_payload(state.resident_dir);
            write_response(state.hub, state.client, ok_response(id, payload)).await?;
        }
        // 域内兜底：本模块只实现上面的方法。若 `domain_for` 把别的同前缀方法也
        // 划给本域，交回 `serve()` 原始 dispatch，而不是在这里自行编造响应。
        _ => return Ok(Flow::Unknown),
    }
    Ok(Flow::Continue)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 通用登记列举：不依赖任何看板概念，也不依赖 daemon。
    #[test]
    fn resident_list_reports_registered_projects() {
        let dir = tempfile::tempdir().unwrap();
        yi_agent_store::resident::require(dir.path(), Path::new("/p"), "some-plugin").unwrap();

        let value = projects_payload(dir.path());

        assert_eq!(value["projects"][0]["project"], "/p");
        assert_eq!(value["projects"][0]["required_by"][0], "some-plugin");
    }

    /// 多请求者如实并列：宿主不解释这些名字，只转发事实。
    #[test]
    fn every_requester_of_a_project_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        yi_agent_store::resident::require(dir.path(), Path::new("/p"), "plug-one").unwrap();
        yi_agent_store::resident::require(dir.path(), Path::new("/p"), "plug-two").unwrap();

        let value = projects_payload(dir.path());

        assert_eq!(value["projects"].as_array().unwrap().len(), 1);
        assert_eq!(
            value["projects"][0]["required_by"],
            json!(["plug-one", "plug-two"])
        );
    }

    /// 空的或损坏的登记表一律回空表：读侧宽容，绝不因一份坏文件让侧栏起不来。
    #[test]
    fn an_absent_or_corrupt_registry_lists_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            projects_payload(dir.path()),
            json!({ "projects": [] }),
            "absent"
        );

        std::fs::write(
            yi_agent_store::resident::registry_path(dir.path()),
            "{ not json",
        )
        .unwrap();
        assert_eq!(
            projects_payload(dir.path()),
            json!({ "projects": [] }),
            "corrupt"
        );
    }

    /// 登记的路径原样带出（不做 canonicalize）：宿主不替登记表解释项目身份，
    /// 「同一个项目两种写法」该由写入方负责。
    #[test]
    fn the_registered_path_is_reported_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let project = PathBuf::from("/some/odd/../path");
        yi_agent_store::resident::require(dir.path(), &project, "plug").unwrap();

        let value = projects_payload(dir.path());

        assert_eq!(value["projects"][0]["project"], "/some/odd/../path");
    }
}
