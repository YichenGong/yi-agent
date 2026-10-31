//! `remote/*` 域的 RPC 处理函数。
//!
//! **本域只有 `remote/relayTargets` 一个方法**（spec §4.6）：其余 `remote/*`
//! 是 Tauri 命令（增删项目、连/断），它们必须改本机进程表，只有 desktop 能做。
//! 手机经中继访问的是本机 app-server，链路上没有 Tauri 桥接层，所以「这个项目
//! 能不能连、该连哪个 session」只能由 app-server 回答。
//!
//! 它**只读**：读 `remote.json`（用户配置）与 `remote-status.json`（桥接层写的
//! 运行时状态），返回几个字符串，不碰任何连接逻辑。
//!
//! `session` 是**凭据**：拿到它就能连上该项目的远端进程。`Observe` 级的只读设备
//! 不得拿到它（spec §4.6、§8.9）——否则「只读」形同虚设。它们仍能拿到
//! `label` 与 `available`，UI 才能显示「看得见但点不动」。

use std::collections::HashMap;
use std::path::Path;

use serde_json::{Value, json};

use crate::protocol::{RequestEnvelope, RequestId, RpcError, Scope};
use crate::rpc::{AppState, Flow};
use crate::server::{err_response, ok_response, write_response};

pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "remote/relayTargets" => {
            // flag 的结论在**每请求**读一次（不是启动快照）：用户在设置页关掉
            // 开关后，已连着的手机下一次轮询就再也拿不到清单。
            let enabled = yi_agent_flags::is_enabled("remote_ssh");
            let answer = handle_relay_targets(
                state.remote_dir,
                state.relay_url,
                state.client_scope,
                enabled,
            );
            let response = match answer {
                Ok(value) => ok_response(id, value),
                Err(error) => err_response(id, error),
            };
            write_response(state.hub, state.client, response).await?;
        }
        _ => return Ok(Flow::Unknown),
    }
    Ok(Flow::Continue)
}

/// `remote/relayTargets` 的全部判定：开关 + 读两份文件 + 按 scope 裁剪。
///
/// 独立成可注入的纯函数，是为了让「flag 关时方法不存在」这条**也能被测试钉住**
/// ——`yi_agent_flags::is_enabled` 会去读开发者真实的 `~/.yi-agent/flags.json`，
/// 且测试二进制的 build kind 恒为 `release`（开关一律 fail-closed），直接调它
/// 无法覆盖「开」的那条分支。
fn handle_relay_targets(
    dir: &Path,
    relay_url: Option<&str>,
    scope: Scope,
    enabled: bool,
) -> Result<Value, RpcError> {
    if !enabled {
        // 方法**不存在**：与 `flags_rpc` 对 stable 的处理同构。前端据此判断
        // 「这个版本没有该功能」，而不是渲染一个必然失败的面板。
        return Err(RpcError::method_not_found("remote/relayTargets"));
    }
    Ok(read_targets(dir, relay_url, scope))
}

/// 读两份文件并组装清单。任何读取失败都退化成「空清单」——手机看到的是「暂时
/// 没有可用项目」，重连后自然恢复；让方法报错只会让手机的整块 UI 崩掉。
fn read_targets(dir: &Path, relay_url: Option<&str>, scope: Scope) -> Value {
    let config = read_json(&dir.join("remote.json"));
    let status = read_json(&dir.join("remote-status.json"));
    build_targets(&config, &status, relay_url, scope)
}

fn read_json(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null)
}

/// 从两份 JSON 组装清单。纯函数，可穷举测试。
///
/// `relay_url` 是本机 app-server 自己收到的中继原始串（含 `?session=`）。`None`
/// 时 `session` 一律为 `null`：没有中继就无从派生，报一个猜的值只会更糟。
pub fn build_targets(
    config: &Value,
    status: &Value,
    relay_url: Option<&str>,
    scope: Scope,
) -> Value {
    let statuses: Option<&serde_json::Map<String, Value>> =
        status.get("statuses").and_then(|v| v.as_object());
    let targets: Vec<Value> = config
        .get("profiles")
        .and_then(|v| v.as_array())
        .map(|profiles| {
            profiles
                .iter()
                .filter_map(|profile| {
                    let id = profile.get("id")?.as_str()?;
                    let label = profile
                        .get("label")
                        .and_then(|v| v.as_str())
                        .filter(|v| !v.is_empty())
                        .unwrap_or(id);
                    // 只有明确写着 `connected` 才算可用：`connecting` 与缺失都
                    // 不是。手机据此决定要不要为这个 target 建连接。
                    let available = statuses
                        .and_then(|map| map.get(id))
                        .and_then(|v| v.as_str())
                        .map(|v| v == "connected")
                        .unwrap_or(false);

                    // Observe 只拿到「看得见」，拿不到凭据。
                    let session = match (scope >= Scope::Control, relay_url) {
                        (true, Some(base)) => {
                            // **手机侧端点**：必须是 `/ws`，不是电脑侧的 `/connect`。
                            // 见 `remote_session::client_endpoint` 的注释。
                            json!(crate::remote_session::client_endpoint(base, id))
                        }
                        _ => Value::Null,
                    };
                    Some(json!({
                        "profileId": id,
                        "label": label,
                        "session": session,
                        "available": available,
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    json!({ "targets": targets })
}

/// 把状态文件读成 map，供别处（如排障命令）复用。
#[allow(dead_code)]
pub fn status_map(status: &Value) -> HashMap<String, String> {
    status
        .get("statuses")
        .and_then(|v| v.as_object())
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str, label: &str) -> Value {
        json!({
            "id": id,
            "label": label,
            "host": "h",
            "local_path": "/a",
            "remote_path": "/b",
            "instance_root": "/c/.yi-agent",
            "binary_version": "1",
            "remote_arch": "x86_64-unknown-linux-gnu"
        })
    }

    fn one_target(scope: Scope) -> Value {
        build_targets(
            &json!({ "version": 1, "profiles": [profile("proj-foo", "foo")] }),
            &json!({ "statuses": { "proj-foo": "connected" } }),
            Some("wss://relay/?session=base"),
            scope,
        )
    }

    #[test]
    fn control_sees_the_derived_session() {
        let value = one_target(Scope::Control);
        let target = &value["targets"][0];
        assert_eq!(target["profileId"], "proj-foo");
        assert_eq!(target["available"], true);
        // `/ws` 而不是 `/connect`：手机连的是转发端点。见 `client_endpoint`。
        assert_eq!(target["session"], "wss://relay/ws?session=base-proj-foo");
    }

    /// session 就是凭据，只读设备拿到它等于拿到「动」的能力。
    #[test]
    fn observe_never_sees_the_session() {
        let value = one_target(Scope::Observe);
        let target = &value["targets"][0];
        assert!(
            target["session"].is_null(),
            "Observe must not receive session: {target}"
        );
        // 但 label 与 available 仍然给，UI 才能显示「可见但只读」。
        assert_eq!(target["label"], "foo");
        assert_eq!(target["available"], true);
    }

    #[test]
    fn admin_sees_the_session_too() {
        assert_eq!(
            one_target(Scope::Admin)["targets"][0]["session"],
            "wss://relay/ws?session=base-proj-foo"
        );
    }

    #[test]
    fn a_disconnected_project_reports_unavailable() {
        let value = build_targets(
            &json!({ "version": 1, "profiles": [profile("p", "p")] }),
            &json!({ "statuses": {} }),
            Some("wss://r/?session=s"),
            Scope::Control,
        );
        assert_eq!(value["targets"][0]["available"], false);
    }

    /// `connecting` 不是可用：手机不该对一个还没握手的槽位建连接。
    #[test]
    fn only_connected_counts_as_available() {
        for raw in ["connecting", "disconnected", "", "weird"] {
            let value = build_targets(
                &json!({ "version": 1, "profiles": [profile("p", "p")] }),
                &json!({ "statuses": { "p": raw } }),
                Some("wss://r/?session=s"),
                Scope::Control,
            );
            assert_eq!(value["targets"][0]["available"], false, "status `{raw}`");
        }
    }

    #[test]
    fn an_empty_config_yields_an_empty_list() {
        let value = build_targets(
            &json!({ "version": 1, "profiles": [] }),
            &json!({}),
            Some("wss://r/?session=s"),
            Scope::Control,
        );
        assert!(value["targets"].as_array().unwrap().is_empty());
    }

    /// 两份文件都读不到（全新机器 / 坏文件）时给空清单，不报错。
    #[test]
    fn a_missing_or_broken_config_yields_an_empty_list() {
        for config in [Value::Null, json!({}), json!("garbage")] {
            let value = build_targets(&config, &Value::Null, Some("wss://r/?s=1"), Scope::Admin);
            assert!(value["targets"].as_array().unwrap().is_empty(), "{config}");
        }
    }

    #[test]
    fn a_profile_without_an_id_is_skipped() {
        let value = build_targets(
            &json!({ "version": 1, "profiles": [{ "label": "no id" }] }),
            &json!({}),
            Some("wss://r/?session=s"),
            Scope::Control,
        );
        assert!(value["targets"].as_array().unwrap().is_empty());
    }

    /// 没有中继就派生不出 session：给 `null` 而不是猜一个。
    #[test]
    fn no_relay_url_yields_null_sessions() {
        let value = build_targets(
            &json!({ "version": 1, "profiles": [profile("p", "p")] }),
            &json!({ "statuses": { "p": "connected" } }),
            None,
            Scope::Admin,
        );
        assert!(value["targets"][0]["session"].is_null());
        assert_eq!(value["targets"][0]["available"], true);
    }

    /// 空 label 退回 id：手机上不能出现一张没有标题的卡片。
    #[test]
    fn a_blank_label_falls_back_to_the_id() {
        let value = build_targets(
            &json!({ "version": 1, "profiles": [profile("proj-x", "")] }),
            &json!({ "statuses": {} }),
            Some("wss://r/?session=s"),
            Scope::Control,
        );
        assert_eq!(value["targets"][0]["label"], "proj-x");
    }

    /// `Observe < Control < Admin` 的排序是上面所有裁剪的**唯一依据**：一旦
    /// 派生顺序反了，`scope >= Scope::Control` 会把凭据交给只读设备。
    #[test]
    fn scope_ranks_observe_below_control_below_admin() {
        assert!(Scope::Observe < Scope::Control);
        assert!(Scope::Control < Scope::Admin);
        assert!(Scope::Observe < Scope::Admin);
    }

    /// flag 关时方法**不存在**（`-32601`），而不是「存在但返回空」：后者会让
    /// 前端以为功能在、只是暂时没项目，从而一直渲染一个空面板。
    #[test]
    fn the_flag_gate_reports_method_not_found() {
        let dir = tempfile::TempDir::new().unwrap();
        let error = handle_relay_targets(dir.path(), Some("wss://r/?s=1"), Scope::Admin, false)
            .unwrap_err();
        assert_eq!(error.code, -32601);
    }

    #[test]
    fn the_gate_reads_the_two_files_when_enabled() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("remote.json"),
            serde_json::to_string(
                &json!({ "version": 1, "profiles": [profile("proj-foo", "foo")] }),
            )
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("remote-status.json"),
            r#"{"statuses":{"proj-foo":"connected"}}"#,
        )
        .unwrap();

        let value =
            handle_relay_targets(dir.path(), Some("wss://r/?session=s"), Scope::Control, true)
                .unwrap();
        assert_eq!(value["targets"][0]["profileId"], "proj-foo");
        assert_eq!(value["targets"][0]["available"], true);
        assert_eq!(
            value["targets"][0]["session"],
            "wss://r/ws?session=s-proj-foo"
        );
    }

    /// 两个项目各拿各的 session：共用一个会让手机把两边的对话混在一起。
    #[test]
    fn each_project_gets_its_own_session() {
        let value = build_targets(
            &json!({
                "version": 1,
                "profiles": [profile("a", "A"), profile("b", "B")]
            }),
            &json!({ "statuses": { "a": "connected", "b": "connected" } }),
            Some("wss://r/?session=s"),
            Scope::Control,
        );
        let targets = value["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 2);
        assert_ne!(targets[0]["session"], targets[1]["session"]);
    }
}
