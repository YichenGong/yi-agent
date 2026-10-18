//! `mcp/list` RPC：设置页「MCP」面板的只读接缝。
//!
//! 与 `skill/list` / `plugins/list` 同类：**按项目解析根目录**，缺省回落宿主
//! workdir。差别在于它读的是 `.yi-agent/mcp.json`（配置）与 `.yi-agent/mcp-cache.json`
//! （工具 schema），并把两者合成一份扁平视图。
//!
//! **纯只读**：不起 MCP 子进程、不发网络请求。工具清单来自磁盘缓存——首次调用
//! 某个 server 时才会被真正探测。缓存缺失或指纹不匹配时如实回 `tools: null`
//! （「尚未探测」），绝不假装清单是最新的。
//!
//! 权限档位：只需 `Observe`——只读盘、不碰凭据值、不落盘。iOS 设备默认
//! `Control`，故手机端可用。

use std::path::Path;

use serde_json::{Value, json};
use yi_agent_mcp::{McpConfig, ServerKind};

use crate::protocol::RpcError;

/// 生产入口：宿主 workdir。测试一律用 [`handle_mcp_request_at`]。
pub fn handle_mcp_request(
    host_workdir: &Path,
    method: &str,
    params: &Value,
) -> Result<Value, RpcError> {
    handle_mcp_request_at(host_workdir, method, params)
}

/// 可测核心：作用域由调用方注入（与 `skill_rpc::handle_skill_request_at` 同形），
/// 故测试不必改 `HOME` 之类的进程全局量。
pub fn handle_mcp_request_at(
    host_workdir: &Path,
    method: &str,
    params: &Value,
) -> Result<Value, RpcError> {
    match method {
        "mcp/list" => list(host_workdir, params),
        _ => Err(RpcError::method_not_found(method)),
    }
}

fn list(host_workdir: &Path, params: &Value) -> Result<Value, RpcError> {
    // 项目作用域**复用** `plugins/list` 的那份实现（`server.rs::plugin_scope`）：
    // MCP / 插件 / 技能三处指的都是「项目目录下的 .yi-agent」，再抄第三份会让
    // 三者在「空白 project 怎么办」「要不要 trim」这类细节上悄悄分叉。
    let project = crate::server::plugin_scope(host_workdir, params);
    let path = yi_agent_mcp::config_path(&project);

    let cfg = match McpConfig::load(&project) {
        Ok(Some(cfg)) => cfg,
        // 没配 MCP 是正常态，不是错误：面板据 `configured: false` 显示空态。
        Ok(None) => {
            return Ok(json!({
                "configured": false,
                "path": path.to_string_lossy(),
                "enabled": false,
                "servers": Vec::<Value>::new(),
            }));
        }
        Err(error) => {
            // 报错必须点名文件，用户才知道去改哪一个。
            return Err(RpcError::invalid_params(format!(
                "cannot read {}: {error}",
                path.display()
            )));
        }
    };

    let mut servers: Vec<Value> = Vec::new();
    for (name, sc) in &cfg.mcp_servers {
        let kind = match sc.kind {
            ServerKind::Stdio => "stdio",
            ServerKind::Http => "http",
        };
        // 缓存命中 ⇒ Some(tools)；缺失/指纹不匹配 ⇒ None（尚未探测）。
        let cached = yi_agent_mcp::cached_tools_for(&project, name, sc);
        let (tools, tool_count, read_only_count) = match &cached {
            None => (Value::Null, 0usize, 0usize),
            Some(entries) => {
                let read_only = entries.iter().filter(|t| t.read_only).count();
                let rows: Vec<Value> = entries
                    .iter()
                    .map(|t| {
                        json!({
                            "name": t.name,
                            "description": t.description.clone().unwrap_or_default(),
                            "read_only": t.read_only,
                        })
                    })
                    .collect();
                (Value::Array(rows), entries.len(), read_only)
            }
        };
        servers.push(json!({
            "name": name,
            "kind": kind,
            "enabled": sc.enabled,
            // stdio 的可执行文件与参数；http 下为 null/空。
            "command": sc.command,
            "args": sc.args,
            // http 的 endpoint；stdio 下为 null。
            "url": sc.url,
            // 只给键名：这些值常是 token，面板没必要把明文摊出来（见 spec 决策 #6）。
            "env_keys": sc.env.keys().collect::<Vec<_>>(),
            "header_keys": sc.headers.keys().collect::<Vec<_>>(),
            "tools": tools,
            "tool_count": tool_count,
            "read_only_count": read_only_count,
        }));
    }

    Ok(json!({
        "configured": true,
        "path": path.to_string_lossy(),
        "enabled": cfg.enabled,
        "servers": servers,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个项目目录并写入 mcp.json；返回项目根。
    fn project_with_config(raw: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join(".yi-agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(agent.join("mcp.json"), raw).unwrap();
        dir
    }

    fn list(project: &Path) -> Value {
        handle_mcp_request_at(project, "mcp/list", &json!({})).unwrap()
    }

    #[test]
    fn reports_not_configured_when_the_file_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let out = list(dir.path());
        assert_eq!(out["configured"], false);
        assert_eq!(out["servers"].as_array().unwrap().len(), 0);
        // 路径照实回项目下的解析值，供「如何添加」文案用。
        assert!(
            out["path"]
                .as_str()
                .unwrap()
                .ends_with(".yi-agent/mcp.json")
        );
    }

    #[test]
    fn lists_a_stdio_server_with_command_and_args() {
        let dir = project_with_config(
            r#"{"enabled":true,"mcpServers":{"fs":{"command":"npx","args":["-y","pkg"]}}}"#,
        );
        let out = list(dir.path());
        assert_eq!(out["configured"], true);
        assert_eq!(out["enabled"], true);
        let s = &out["servers"][0];
        assert_eq!(s["name"], "fs");
        assert_eq!(s["kind"], "stdio");
        assert_eq!(s["enabled"], true);
        assert_eq!(s["command"], "npx");
        assert_eq!(s["args"], json!(["-y", "pkg"]));
        assert_eq!(s["url"], Value::Null);
        // 没有缓存 ⇒ 尚未探测（null），而不是「没有工具」（[]）。
        assert_eq!(s["tools"], Value::Null);
        assert_eq!(s["tool_count"], 0);
    }

    #[test]
    fn lists_an_http_server_with_url() {
        let dir =
            project_with_config(r#"{"mcpServers":{"ws":{"type":"http","url":"https://x/mcp"}}}"#);
        let out = list(dir.path());
        let s = &out["servers"][0];
        assert_eq!(s["kind"], "http");
        assert_eq!(s["url"], "https://x/mcp");
        assert_eq!(s["command"], Value::Null);
    }

    #[test]
    fn reports_only_key_names_never_values() {
        let dir = project_with_config(
            r#"{"mcpServers":{"fs":{"command":"npx","env":{"TOKEN":"secret-token"}},"ws":{"type":"http","url":"https://x/mcp","headers":{"Authorization":"Bearer secret-token"}}}}"#,
        );
        let out = list(dir.path());
        // `mcpServers` 是 BTreeMap ⇒ 顺序按名字：fs 在前，ws 在后。
        let by_name = |n: &str| {
            out["servers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["name"] == n)
                .unwrap()
                .clone()
        };
        assert_eq!(by_name("fs")["env_keys"], json!(["TOKEN"]));
        assert_eq!(by_name("ws")["header_keys"], json!(["Authorization"]));
        let text = out.to_string();
        assert!(
            !text.contains("secret-token"),
            "env/header values must never leave the server: {text}"
        );
    }

    #[test]
    fn reports_per_server_disabled_state() {
        let dir = project_with_config(
            r#"{"enabled":false,"mcpServers":{"fs":{"command":"npx","enabled":false}}}"#,
        );
        let out = list(dir.path());
        assert_eq!(out["enabled"], false);
        assert_eq!(out["servers"][0]["enabled"], false);
    }

    #[test]
    fn surfaces_cached_tools_and_counts() {
        let dir = project_with_config(r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        // 直接用 crate 的缓存写入 API 造命中缓存（指纹由同一份实现算）。
        let sc = yi_agent_mcp::ServerConfig {
            kind: yi_agent_mcp::ServerKind::Stdio,
            command: Some("npx".into()),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            url: None,
            headers: std::collections::BTreeMap::new(),
            enabled: true,
        };
        let mut cache = yi_agent_mcp::test_support::cache_with(dir.path());
        cache.put(
            "fs",
            &sc,
            vec![
                yi_agent_mcp::test_support::cached_tool("read_file", true),
                yi_agent_mcp::test_support::cached_tool("write_file", false),
            ],
        );
        cache.save(dir.path()).unwrap();

        let out = list(dir.path());
        let s = &out["servers"][0];
        assert_eq!(s["tool_count"], 2);
        assert_eq!(s["read_only_count"], 1);
        assert_eq!(s["tools"][0]["name"], "read_file");
        assert_eq!(s["tools"][0]["read_only"], true);
        // schema 不下发。
        assert!(s["tools"][0].get("input_schema").is_none());
    }

    #[test]
    fn stale_cache_fingerprint_reports_unprobed_instead_of_stale_tools() {
        let dir = project_with_config(r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        let other = yi_agent_mcp::ServerConfig {
            kind: yi_agent_mcp::ServerKind::Stdio,
            command: Some("uvx".into()), // 与文件里的 command 不同 ⇒ 指纹不匹配
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            url: None,
            headers: std::collections::BTreeMap::new(),
            enabled: true,
        };
        let mut cache = yi_agent_mcp::test_support::cache_with(dir.path());
        cache.put(
            "fs",
            &other,
            vec![yi_agent_mcp::test_support::cached_tool("x", true)],
        );
        cache.save(dir.path()).unwrap();

        let out = list(dir.path());
        assert_eq!(
            out["servers"][0]["tools"],
            Value::Null,
            "a stale cache must show as unprobed, never as a fresh tool list"
        );
    }

    #[test]
    fn invalid_json_reports_an_error_naming_the_file() {
        let dir = project_with_config("{ not json");
        let err = handle_mcp_request_at(dir.path(), "mcp/list", &json!({})).unwrap_err();
        assert_eq!(err.code, -32602);
        assert!(
            err.message.contains("mcp.json"),
            "the error must name the file to fix: {}",
            err.message
        );
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = handle_mcp_request_at(dir.path(), "mcp/bogus", &json!({})).unwrap_err();
        assert_eq!(err.code, -32601);
    }
}
