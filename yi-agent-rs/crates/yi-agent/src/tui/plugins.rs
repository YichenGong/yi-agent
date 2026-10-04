//! `/plugins`——列出本机（当前工作目录）已装插件，并读/写插件设置。
//!
//! 与看板一样走 daemon 的通用 `plugin/query` 通道；列表则直接读
//! `<workdir>/.yi-agent/supervisors/`，因为「装了没装」是文件事实，不需要 daemon。

use std::path::Path;

use serde_json::{Value, json};

/// 经 daemon 问插件一个问题，返回原始应答。失败给一句人话。
fn query_plugin(
    workdir: &Path,
    plugin: &str,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let socket =
        crate::runtime_socket_for(workdir).map_err(|error| format!("no daemon socket: {error}"))?;
    let response = yi_agent_store::ipc::send_request(
        &socket,
        yi_agent_store::ipc::IpcRequest::PluginQuery {
            plugin: plugin.to_string(),
            method: method.to_string(),
            params,
        },
    )
    .map_err(|error| format!("daemon is unavailable: {error}"))?;
    match response {
        yi_agent_store::ipc::IpcResponse::PluginResult { value } => Ok(value),
        yi_agent_store::ipc::IpcResponse::Error { message, .. } => {
            Err(message.unwrap_or_else(|| "the plugin rejected the query".to_string()))
        }
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// 已装插件名（按清单）。
fn installed(workdir: &Path) -> Vec<String> {
    let dir = workdir.join(".yi-agent").join("supervisors");
    yi_agent_supervisors::manifest::load_manifests(&dir)
        .into_iter()
        .map(|manifest| manifest.name)
        .collect()
}

/// `/plugins [<name> [set <key> <value>]]`。
pub fn handle_plugins(workdir: &Path, args: &str) -> Vec<String> {
    let mut words = args.split_whitespace();
    let first = words.next();
    let rest: Vec<&str> = words.collect();

    match (first, rest.as_slice()) {
        (None, _) => {
            let names = installed(workdir);
            if names.is_empty() {
                return vec!["未安装任何插件".to_string()];
            }
            let mut lines = vec!["已安装插件：".to_string()];
            lines.extend(names.into_iter().map(|name| format!("- {name}")));
            lines
        }
        (Some(name), []) => match query_plugin(workdir, name, "settings.read", json!({})) {
            Ok(value) => render_settings(name, &value),
            Err(message) => vec![format!("无法读取 {name} 的设置: {message}")],
        },
        (Some(name), ["set", key, value]) => {
            let current = match query_plugin(workdir, name, "settings.read", json!({})) {
                Ok(value) => value,
                Err(message) => return vec![format!("无法读取 {name} 的设置: {message}")],
            };
            let mut settings = current
                .get("settings")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let parsed: serde_json::Value = match value.parse() {
                Ok(parsed) => parsed,
                Err(_) => json!(value),
            };
            if let Some(object) = settings.as_object_mut() {
                object.insert((*key).to_string(), parsed);
            } else {
                return vec![format!("{name} 的设置不是对象，无法设置 {key}")];
            }
            match query_plugin(
                workdir,
                name,
                "settings.write",
                json!({ "settings": settings }),
            ) {
                Ok(_) => vec![format!("{name}: 已设置 {key} = {value}")],
                Err(message) => vec![format!("{name}: 设置失败: {message}")],
            }
        }
        _ => vec!["usage: /plugins [<name> [set <key> <value>]]".to_string()],
    }
}

/// 把 `settings.read` 的载荷渲染成几行。窗口表只读展示，编辑请去桌面端。
fn render_settings(name: &str, value: &Value) -> Vec<String> {
    let settings = value.get("settings").unwrap_or(value);
    let mut lines = vec![format!("{name} 设置：")];
    if let Some(default_max_tasks) = settings.get("default_max_tasks") {
        lines.push(format!("  默认并发上限: {default_max_tasks}"));
    }
    if let Some(interval) = settings.get("interval_secs") {
        lines.push(format!("  推进间隔秒数: {interval}"));
    }
    if let Some(windows) = settings.get("windows").and_then(Value::as_array) {
        lines.push("  时段窗口（只读；编辑请用桌面端）：".to_string());
        for window in windows {
            let days = window.get("days").and_then(Value::as_str).unwrap_or("");
            let start = window.get("start").and_then(Value::as_str).unwrap_or("");
            let end = window.get("end").and_then(Value::as_str).unwrap_or("");
            let max = window
                .get("max_tasks")
                .map(|v| v.to_string())
                .unwrap_or_default();
            lines.push(format!("    {days} {start}-{end} → 并发 {max}"));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_plugins_prints_a_neutral_line() {
        let dir = tempfile::tempdir().unwrap();
        let lines = handle_plugins(dir.path(), "");
        assert!(
            lines.iter().any(|line| line.contains("未安装任何插件")),
            "{lines:?}"
        );
    }

    #[test]
    fn an_installed_plugin_is_listed() {
        let dir = tempfile::tempdir().unwrap();
        let sup = dir.path().join(".yi-agent/supervisors");
        std::fs::create_dir_all(&sup).unwrap();
        std::fs::write(
            sup.join("superpowers-kanban.json"),
            r#"{"name":"superpowers-kanban","command":"x","switch_key":"k","query_socket":"{state_dir}/superpowers-kanban.sock"}"#,
        )
        .unwrap();
        let lines = handle_plugins(dir.path(), "");
        assert!(
            lines.iter().any(|line| line.contains("superpowers-kanban")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_bad_subcommand_prints_usage() {
        let dir = tempfile::tempdir().unwrap();
        let lines = handle_plugins(dir.path(), "a b c d");
        assert!(
            lines.iter().any(|line| line.contains("usage: /plugins")),
            "{lines:?}"
        );
    }

    /// 全局约束：面对坏 JSON 绝不 panic。坏的清单文件被跳过，好的仍然列出。
    #[test]
    fn a_malformed_manifest_is_skipped_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let sup = dir.path().join(".yi-agent/supervisors");
        std::fs::create_dir_all(&sup).unwrap();
        std::fs::write(sup.join("broken.json"), "{ not json").unwrap();
        std::fs::write(
            sup.join("superpowers-kanban.json"),
            r#"{"name":"superpowers-kanban","command":"x","switch_key":"k","query_socket":"{state_dir}/superpowers-kanban.sock"}"#,
        )
        .unwrap();
        let lines = handle_plugins(dir.path(), "");
        assert!(
            lines.iter().any(|line| line.contains("superpowers-kanban")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("broken")),
            "{lines:?}"
        );
    }

    /// `settings.read` 回了非对象载荷时，渲染只丢字段、不 panic。
    #[test]
    fn rendering_a_malformed_settings_payload_does_not_panic() {
        let lines = render_settings("demo", &json!({ "settings": 42 }));
        assert_eq!(lines, vec!["demo 设置：".to_string()]);
    }
}
