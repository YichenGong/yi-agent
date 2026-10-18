# MCP 设置面板（只读查看）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 桌面设置面板新增「MCP」Tab，按项目只读列出 MCP server（名字 / 传输 / 开关默认值 / 缓存中的工具清单）。

**Architecture:** 新增一条纯只读 RPC `mcp/list`：服务端复用 `yi-agent-mcp` 的配置解析与缓存指纹判定，读 `<项目>/.yi-agent/mcp.json` + `mcp-cache.json` 后返回扁平 JSON；不起进程、不发网络，档位 `Observe`。前端按既有「技能」Tab 的范式（项目选择器 + 折叠列表 + 空态帮助）新增一个面板。

**Tech Stack:** Rust（`yi-agent-app-server` 的 jsonrpc 域 handler 模式）、React 19 + TypeScript + vitest（`desktop/`）。

## Global Constraints

- 参考 spec：`docs/superpowers/specs/2026-10-18-mcp-settings-panel-design.md`
- 分支 `feat/mcp-settings-panel`，worktree `.worktrees/feat-mcp-settings-panel`（已存在）；**严禁在 `main` 上提交**
- 提交前在 `yi-agent-rs/` 下跑 `cargo fmt --all`（仓库 CLAUDE.md 要求）
- commit message 用 conventional commits，**不写** `Co-Authored-By`
- 不新增探测 RPC、不写回 `mcp.json`、不接 `McpManager`（本次范围只有「看得见」）
- 响应里**绝不**出现 stdio `env` 或 http `headers` 的**值**，也不出现 `${VAR}` 引用的原文；只回键名
- 已有测试必须保持绿：`desktop/src/components/SettingsDialog.test.tsx` 里断言 Tab 名列表的两个用例（桌面 6 项、手机 6 项）会因新增 Tab 而失败，属**预期更新**，随任务一起改
- 时间预算：每步 2-5 分钟；不要跨任务提前改文件

## File Structure

| 文件 | 职责 |
|------|------|
| `yi-agent-rs/crates/yi-agent-mcp/src/lib.rs` | 加 `pub struct McpCacheEntry` 视图类型 + `pub fn cached_tools_for`，不导出私有 `McpCache` |
| `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml` | 加 `yi-agent-mcp` 依赖 |
| `yi-agent-rs/crates/yi-agent-app-server/src/mcp_rpc.rs` | 新增：`mcp/list` 的纯函数实现 + 单测 |
| `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs` | 加 `Domain::Mcp` 与 `domain_for` 映射 |
| `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mcp.rs` | 新增：域 handler（`Observe`，不带臂内门禁） |
| `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` | dispatch 加 `Domain::Mcp` 分支 |
| `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs` | 方法表加 `("mcp/list", Domain::Mcp)` + 端到端用例 |
| `desktop/src/lib/mcp.ts` | 新增：`McpRpc`/`McpCall` 接缝 + 防御式解析 + `listMcpServers` |
| `desktop/src/components/SettingsMcpTab.tsx` | 新增：面板 |
| `desktop/src/components/SettingsDialog.tsx` | 加 Tab 项、三个 props、一支分派 |
| `desktop/src/App.tsx` | 加稳定身份的 `mcpCall` + projects/defaultProject 接线 |
| `docs/project-management/desktop.md`、`yi-agent-mcp.md`、`README.md` | 文档同步 |

---

### Task 1: 后端只读数据源（`mcp_rpc` + 依赖）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/lib.rs`（`pub use` 区之后）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml`
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/mcp_rpc.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`（加 `pub mod mcp_rpc;`，按字母序放在 `lead_memory` 与 `model_rpc` 之间）

**Interfaces:**
- Consumes: `yi_agent_mcp::{McpConfig, ServerConfig, ServerKind, cached_tools_for}`
- Produces:
  - `yi_agent_mcp::McpCacheEntry { pub name: String, pub description: Option<String>, pub read_only: bool }`
  - `yi_agent_mcp::cached_tools_for(workdir: &Path, name: &str, cfg: &ServerConfig) -> Option<Vec<McpCacheEntry>>`（`None` = 缓存缺失或指纹不匹配）
  - `crate::mcp_rpc::handle_mcp_request(host_workdir: &Path, method: &str, params: &Value) -> Result<Value, RpcError>`

- [ ] **Step 1: 先加 `yi-agent-mcp` 的公开视图（否则 app-server 拿不到缓存工具）**

`McpCache` 是私有模块（`lib.rs` 里 `mod cache;` 无 `pub`），app-server 不能直接用它。在 `yi-agent-rs/crates/yi-agent-mcp/src/lib.rs` 的 `pub use` 块之后加：

```rust
/// 缓存里一个远端工具的**只读视图**，供设置页展示。
///
/// 刻意不带 `input_schema`：那份 schema 可能有上千字，面板只展示名字与一行描述，
/// 把它下发既无用又撑大响应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCacheEntry {
    pub name: String,
    pub description: Option<String>,
    pub read_only: bool,
}

/// 读 `workdir` 下 `.yi-agent/mcp-cache.json` 里 `name` 的工具清单。
///
/// 指纹与 `cfg` 不匹配或缓存缺失时返回 `None`——调用方据此区分「尚未探测」与
/// 「探测过但没有工具」（后者是 `Some(vec![])`）。判定逻辑复用同源的
/// [`cache::McpCache::get_for`]，故面板看到的清单与真正注册进工具集的那份一致。
pub fn cached_tools_for(workdir: &Path, name: &str, cfg: &ServerConfig) -> Option<Vec<McpCacheEntry>> {
    let cache = cache::McpCache::load(workdir);
    cache.get_for(name, cfg).map(|tools| {
        tools
            .iter()
            .map(|t| McpCacheEntry {
                name: t.name.clone(),
                description: t.description.clone(),
                read_only: t.read_only,
            })
            .collect()
    })
}
```

- [ ] **Step 2: 编译验证**

Run: `cd yi-agent-rs && cargo check -p yi-agent-mcp`
Expected: 通过（`cache` 与 `config` 在本 crate 内可见；`Path` 已 import）

- [ ] **Step 3: 加 app-server 依赖**

在 `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml` 的 `yi-agent-tools` 一行附近加：

```toml
# `mcp/list` 读项目级 `.yi-agent/mcp.json` 与工具 schema 缓存。
yi-agent-mcp = { workspace = true }
```

- [ ] **Step 4: 写失败的测试**

创建 `yi-agent-rs/crates/yi-agent-app-server/src/mcp_rpc.rs`，**先只写测试模块**：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 造一个项目目录并写入 mcp.json；返回项目根。
    fn project_with_config(raw: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join(".yi-agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(agent.join("mcp.json"), raw).unwrap();
        dir
    }

    fn list(project: &std::path::Path) -> serde_json::Value {
        handle_mcp_request_at(project, "mcp/list", &json!({})).unwrap()
    }

    #[test]
    fn reports_not_configured_when_the_file_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let out = list(dir.path());
        assert_eq!(out["configured"], false);
        assert_eq!(out["servers"].as_array().unwrap().len(), 0);
        // 路径照实回项目下的解析值，供「如何添加」文案用。
        assert!(out["path"].as_str().unwrap().ends_with(".yi-agent/mcp.json"));
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
        assert_eq!(s["url"], serde_json::Value::Null);
        // 没有缓存 ⇒ 尚未探测（null），而不是「没有工具」（[]）。
        assert_eq!(s["tools"], serde_json::Value::Null);
        assert_eq!(s["tool_count"], 0);
    }

    #[test]
    fn lists_an_http_server_with_url() {
        let dir = project_with_config(
            r#"{"mcpServers":{"ws":{"type":"http","url":"https://x/mcp"}}}"#,
        );
        let out = list(dir.path());
        let s = &out["servers"][0];
        assert_eq!(s["kind"], "http");
        assert_eq!(s["url"], "https://x/mcp");
        assert_eq!(s["command"], serde_json::Value::Null);
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
        // 直接用 crate 的缓存写入 API 造命中缓存（指纹必须由同一份实现算）。
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
        cache.put("fs", &other, vec![yi_agent_mcp::test_support::cached_tool("x", true)]);
        cache.save(dir.path()).unwrap();

        let out = list(dir.path());
        assert_eq!(
            out["servers"][0]["tools"],
            serde_json::Value::Null,
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
```

同一 Task 内要给 `yi-agent-mcp` 加 crate 内测试支撑（`#[cfg(any(test, feature = ...))]` 太重，直接用 `#[doc(hidden)]` 的 pub 模块即可，app-server 的 dev 依赖也能用）：

在 `yi-agent-rs/crates/yi-agent-mcp/src/lib.rs` 末尾加：

```rust
/// 测试支撑：让下游 crate 的测试能写入真实的缓存文件，而不必复刻 SHA-256 指纹
/// 格式（那会让下游测试对私有实现脆弱）。
#[doc(hidden)]
pub mod test_support {
    use super::*;

    /// 一个挂在 `workdir` 上的空缓存（尚未 `save`）。
    pub fn cache_with(workdir: &Path) -> cache::McpCache {
        cache::McpCache::load(workdir)
    }

    /// 造一条缓存工具记录。
    pub fn cached_tool(name: &str, read_only: bool) -> cache::CachedTool {
        cache::CachedTool {
            name: name.to_string(),
            description: Some(format!("{name} 描述")),
            input_schema: serde_json::json!({"type": "object"}),
            read_only,
        }
    }
}
```

- [ ] **Step 5: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server mcp_rpc 2>&1 | tail -20`
Expected: 编译失败 —— `handle_mcp_request_at` / `handle_mcp_request` 未定义（`module mcp_rpc` 还没挂上，或函数体还没写）

- [ ] **Step 6: 写最小实现**

在 `mcp_rpc.rs` 的测试模块**之前**写实现：

```rust
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
```

在 `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs` 的模块声明区加：

```rust
mod mcp_rpc;
```

（若 `lib.rs` 里模块是 `pub mod`，则用 `pub mod mcp_rpc;`，与该文件既有风格保持一致。）

**注意**：`yi_agent_mcp::config_path` 目前是 `pub fn config_path(workdir: &Path) -> PathBuf`，
但未在 `lib.rs` 的 `pub use` 里导出。在 `lib.rs` 的导出区补一行：

```rust
pub use config::{McpConfig, ServerConfig, ServerKind, Transport, config_path};
```

- [ ] **Step 7: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib mcp_rpc 2>&1 | tail -20`
Expected: 9 个用例全 PASS

- [ ] **Step 8: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-mcp/src/lib.rs \
  yi-agent-rs/crates/yi-agent-app-server/Cargo.toml \
  yi-agent-rs/crates/yi-agent-app-server/src/lib.rs \
  yi-agent-rs/crates/yi-agent-app-server/src/mcp_rpc.rs
git commit -m "feat(app-server): add the read-only mcp/list RPC"
```

---

### Task 2: 把 `mcp/list` 接进 RPC 域与主循环

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs:155-210`
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mcp.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`Domain::Skill` 分支之后，约 :2799）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`（方法表 + 新用例）

**Interfaces:**
- Consumes: `crate::mcp_rpc::handle_mcp_request(host_workdir, method, params)`（Task 1）、`server::{err_response, ok_response, write_response}`、`rpc::{AppState, Flow}`
- Produces: `crate::rpc::Domain::Mcp`、`crate::rpc::mcp::handle(state, req, id) -> anyhow::Result<Flow>`

- [ ] **Step 1: 写失败的测试（方法表 + 端到端）**

在 `rpc_tests.rs` 的 `unknown_method_returns_method_not_found` 的方法表里，`// ---- Skill` 那段之后加：

```rust
        // ---- Mcp(设置页 MCP 面板:项目级 mcp.json + schema 缓存) ----
        ("mcp/list", Domain::Mcp),
```

在同文件的 `mod` 测试区内（与 `skill_list_is_available_at_observe_scope` 相邻）加：

```rust
        #[tokio::test(flavor = "multi_thread")]
        async fn mcp_list_is_available_at_observe_scope() {
            let mut h = (Harness::with_scope(Scope::Observe)).await;
            initialize(&mut h).await;
            let resp = call(&mut h, 1, "mcp/list", "{}").await;
            assert!(
                resp.get("error").is_none(),
                "observe must list mcp servers: {resp}"
            );
            assert!(resp["result"]["servers"].is_array());
            h.shutdown().await;
        }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server mcp_list_is_available 2>&1 | tail -20`
Expected: 编译失败（`Domain::Mcp` 不存在）；方法表那条也会因 `Domain::Mcp` 未定义而编译不过

- [ ] **Step 3: 加 `Domain::Mcp` 与映射**

在 `rpc/mod.rs` 的 `Domain` 枚举里，`Skill` 变体之后加：

```rust
    /// `mcp/*`(设置页 MCP 面板:项目级 mcp.json + 工具 schema 缓存的只读视图)。
    Mcp,
```

在 `domain_for` 里，`Some("skill") => Domain::Skill,` 之后加：

```rust
        Some("mcp") => Domain::Mcp,
```

- [ ] **Step 4: 写域 handler**

创建 `rpc/mcp.rs`（形状逐字照 `rpc/skill.rs`）：

```rust
//! `mcp/*` 域的 RPC 处理函数。
//!
//! 与 `skill/*` 同构：臂体在域内应答后回主循环下一轮，`_` 交回 `serve()` 的原始
//! dispatch。权限档位 `Observe`——只读、不碰凭据值、不落盘，与 `skill/list`、
//! `plugins/list` 同档，故本域**不加**臂内门禁（门禁只落在 `Control`/`Admin`
//! 方法上，见 `serve()` 里的 `ADMIN_METHODS` 与各域内的 `client_scope` 判断）。

use crate::protocol::{RequestEnvelope, RequestId};
use crate::rpc::{AppState, Flow};
use crate::server::{err_response, ok_response, write_response};

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
        // 设置页「MCP」面板：按项目列出 server 与缓存中的工具清单。
        "mcp/list" => {
            match crate::mcp_rpc::handle_mcp_request(
                state.theme_handle.workdir(),
                req.method.as_str(),
                &req.params,
            ) {
                Ok(value) => {
                    write_response(state.hub, state.client, ok_response(id, value)).await?
                }
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?
                }
            }
        }
        // 域内兜底：本模块只实现上面列出的方法。
        _ => return Ok(Flow::Unknown),
    }
    Ok(Flow::Continue)
}
```

- [ ] **Step 5: 挂进 `rpc/mod.rs` 的模块列表**

在 `rpc/mod.rs` 的 `pub(crate) mod` 列表里按字母序加：

```rust
pub(crate) mod mcp;
```

- [ ] **Step 6: 在主循环 dispatch 里加分支**

在 `server.rs` 的 `crate::rpc::Domain::Skill => { ... }` 整块之后、`Domain::Thread` 之前加：

```rust
                    crate::rpc::Domain::Mcp => {
                        let mut state = app_state!();
                        match crate::rpc::mcp::handle(&mut state, &req, id.clone()).await? {
                            // 已搬移的域在应答后一律回下一轮。
                            crate::rpc::Flow::Continue => continue,
                            // 域内 `_` 才可能到这里:交回下面的原始 dispatch。
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
```

- [ ] **Step 7: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server 2>&1 | tail -25`
Expected: 全 PASS（含方法表回归网与新用例）

- [ ] **Step 8: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs \
  yi-agent-rs/crates/yi-agent-app-server/src/rpc/mcp.rs \
  yi-agent-rs/crates/yi-agent-app-server/src/server.rs \
  yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs
git commit -m "feat(app-server): route mcp/list through its own RPC domain"
```

---

### Task 3: 前端 RPC 接缝与防御式解析

**Files:**
- Create: `desktop/src/lib/mcp.ts`
- Test: `desktop/src/lib/mcp.test.ts`

**Interfaces:**
- Produces:
  - `export type McpRpc`、`export type McpCall`
  - `export interface McpToolSummary { name: string; description: string; read_only: boolean }`
  - `export interface McpServerView { name; kind: "stdio" | "http"; enabled; command: string | null; args: string[]; url: string | null; envKeys: string[]; headerKeys: string[]; tools: McpToolSummary[] | null; toolCount: number; readOnlyCount: number }`
  - `export interface McpList { configured: boolean; path: string; enabled: boolean; servers: McpServerView[] }`
  - `export async function listMcpServers(rpc: McpRpc, project?: string): Promise<McpList>`

- [ ] **Step 1: 写失败的测试**

创建 `desktop/src/lib/mcp.test.ts`：

```ts
import { describe, expect, it } from "vitest";
import { listMcpServers, type McpRpc } from "./mcp";

const rpc = (result: unknown): McpRpc => (async () => result) as McpRpc;

describe("listMcpServers", () => {
  it("sends the project when one is given", async () => {
    const calls: unknown[] = [];
    const call: McpRpc = (async (method: string, params: unknown) => {
      calls.push([method, params]);
      return { configured: false, path: "/p/.yi-agent/mcp.json", enabled: false, servers: [] };
    }) as McpRpc;
    await listMcpServers(call, "/p");
    expect(calls).toEqual([["mcp/list", { project: "/p" }]]);
  });

  it("omits the project when none is given", async () => {
    const calls: unknown[] = [];
    const call: McpRpc = (async (method: string, params: unknown) => {
      calls.push([method, params]);
      return { configured: false, path: "/p/.yi-agent/mcp.json", enabled: false, servers: [] };
    }) as McpRpc;
    await listMcpServers(call);
    expect(calls).toEqual([["mcp/list", {}]]);
  });

  it("keeps unprobed tools as null instead of an empty list", async () => {
    const out = await listMcpServers(
      rpc({
        configured: true,
        path: "/p/.yi-agent/mcp.json",
        enabled: true,
        servers: [{ name: "fs", kind: "stdio", enabled: true, command: "npx", args: ["-y"], tools: null, tool_count: 0, read_only_count: 0 }],
      }),
    );
    expect(out.servers[0].tools).toBeNull();
    expect(out.servers[0].command).toBe("npx");
  });

  it("drops rows with an unknown transport instead of guessing", async () => {
    const out = await listMcpServers(
      rpc({
        configured: true,
        path: "/p/.yi-agent/mcp.json",
        enabled: true,
        servers: [
          { name: "bad", kind: "carrier-pigeon" },
          { name: "fs", kind: "stdio", command: "npx" },
        ],
      }),
    );
    expect(out.servers.map((s) => s.name)).toEqual(["fs"]);
  });

  it("fills missing fields with safe empties", async () => {
    const out = await listMcpServers(rpc({ servers: [{ name: "fs", kind: "stdio" }] }));
    expect(out.configured).toBe(false);
    expect(out.servers[0]).toMatchObject({
      enabled: true,
      args: [],
      envKeys: [],
      headerKeys: [],
      tools: null,
      toolCount: 0,
      readOnlyCount: 0,
    });
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/mcp.test.ts 2>&1 | tail -20`
Expected: FAIL —— 无法解析 `./mcp`（模块不存在）

- [ ] **Step 3: 写实现**

创建 `desktop/src/lib/mcp.ts`：

```ts
/**
 * 桌面与宿主之间的 MCP 通道。形状与 `skills.ts` / `pluginSettings.ts` 一致。
 *
 * 服务端返回的是手写 `json!`（不走 ts-rs DTO），故本模块做一次形状收敛：坏行丢掉、
 * 缺失字段补安全空值，绝不把半成品往下传——面板宁可少列，也不能因为一条脏数据
 * 整页崩掉。
 */

/** 泛型形状：调用方可自选返回类型。 */
export type McpRpc = <T = unknown>(method: string, params: unknown) => Promise<T>;

/** 非泛型接缝（宿主与测试用）：`vi.fn` 推断不出泛型函数，直接赋给 `McpRpc` 会 TS2322。 */
export type McpCall = (method: string, params: unknown) => Promise<unknown>;

/** 工具清单里的一个远端工具（不含参数 schema）。 */
export interface McpToolSummary {
  name: string;
  description: string;
  read_only: boolean;
}

export interface McpServerView {
  name: string;
  kind: "stdio" | "http";
  /** 文件里的该 server 开关默认值（只读展示，面板不写回）。 */
  enabled: boolean;
  /** stdio：可执行文件；http 下为 null。 */
  command: string | null;
  args: string[];
  /** http：endpoint；stdio 下为 null。 */
  url: string | null;
  /** env 的**键名**（值永不下发）。 */
  envKeys: string[];
  /** headers 的**键名**（值永不下发）。 */
  headerKeys: string[];
  /** `null` = 尚未探测（缓存缺失或指纹不匹配）；`[]` = 探测过但没有工具。 */
  tools: McpToolSummary[] | null;
  toolCount: number;
  readOnlyCount: number;
}

export interface McpList {
  /** `false` = 该项目没有 `.yi-agent/mcp.json`，是正常态而非错误。 */
  configured: boolean;
  /** 配置文件的真实解析路径（供「如何添加」文案用，不硬编码）。 */
  path: string;
  /** 文件里的顶层总开关默认值。 */
  enabled: boolean;
  servers: McpServerView[];
}

function asStringArray(value: unknown): string[] {
  return Array.isArray(value) ? value.filter((v): v is string => typeof v === "string") : [];
}

function asOptString(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}

function asTool(row: unknown): McpToolSummary | undefined {
  if (typeof row !== "object" || row === null) return undefined;
  const r = row as Record<string, unknown>;
  if (typeof r.name !== "string" || r.name === "") return undefined;
  return {
    name: r.name,
    description: typeof r.description === "string" ? r.description : "",
    read_only: r.read_only === true,
  };
}

function asServer(row: unknown): McpServerView | undefined {
  if (typeof row !== "object" || row === null) return undefined;
  const r = row as Record<string, unknown>;
  if (typeof r.name !== "string" || r.name === "") return undefined;
  // 传输只认两种；未知值整行丢弃，而不是猜一个默认——猜错会把 http 的 endpoint
  // 当成 stdio 命令显示。
  if (r.kind !== "stdio" && r.kind !== "http") return undefined;
  const rawTools = r.tools;
  const tools =
    rawTools === null || rawTools === undefined
      ? null
      : (Array.isArray(rawTools) ? rawTools : [])
          .map(asTool)
          .filter((t): t is McpToolSummary => t !== undefined);
  return {
    name: r.name,
    kind: r.kind,
    enabled: r.enabled !== false,
    command: asOptString(r.command),
    args: asStringArray(r.args),
    url: asOptString(r.url),
    envKeys: asStringArray(r.env_keys),
    headerKeys: asStringArray(r.header_keys),
    tools,
    toolCount: typeof r.tool_count === "number" ? r.tool_count : 0,
    readOnlyCount: typeof r.read_only_count === "number" ? r.read_only_count : 0,
  };
}

/** 列出某个项目的 MCP server。`project` 缺省时由服务端回落宿主 workdir。 */
export async function listMcpServers(rpc: McpRpc, project?: string): Promise<McpList> {
  const result = await rpc<{ configured?: unknown; path?: unknown; enabled?: unknown; servers?: unknown }>(
    "mcp/list",
    { ...(project ? { project } : {}) },
  );
  const rows = Array.isArray(result?.servers) ? result.servers : [];
  return {
    configured: result?.configured === true,
    path: typeof result?.path === "string" ? result.path : "",
    enabled: result?.enabled === true,
    servers: rows
      .map(asServer)
      .filter((s): s is McpServerView => s !== undefined)
      // 服务端按名字排序（BTreeMap）；这里不重排，保持与服务端一致。
  };
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/mcp.test.ts 2>&1 | tail -20`
Expected: 5 个用例全 PASS

- [ ] **Step 5: 类型检查并提交**

```bash
cd desktop && npx tsc --noEmit
cd .. && git add desktop/src/lib/mcp.ts desktop/src/lib/mcp.test.ts
git commit -m "feat(desktop): add the MCP RPC seam and defensive parsing"
```

---

### Task 4: 「MCP」Tab 面板组件

**Files:**
- Create: `desktop/src/components/SettingsMcpTab.tsx`
- Test: `desktop/src/components/SettingsMcpTab.test.tsx`

**Interfaces:**
- Consumes: `listMcpServers` / `McpCall` / `McpList` / `McpServerView`（Task 3）、`SettingsProjectPicker`（既有）
- Produces: `export function SettingsMcpTab({ call, projects, defaultProject }: { call?: McpCall; projects: string[]; defaultProject?: string })`

- [ ] **Step 1: 写失败的测试**

创建 `desktop/src/components/SettingsMcpTab.test.tsx`：

```tsx
/** @vitest-environment jsdom */
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { SettingsMcpTab } from "./SettingsMcpTab";

afterEach(cleanup);

const empty = { configured: false, path: "/p/.yi-agent/mcp.json", enabled: false, servers: [] };

describe("SettingsMcpTab", () => {
  it("explains how to add one when nothing is installed", async () => {
    const call = async () => empty;
    render(<SettingsMcpTab call={call} projects={[]} />);
    expect(await screen.findByText("未安装任何 MCP server")).toBeTruthy();
    // 路径取服务端回的真实值，不硬编码 ~/.yi-agent。
    expect(screen.getByText(/\/p\/\.yi-agent\/mcp\.json/)).toBeTruthy();
  });

  it("renders a stdio server with its command and a disabled badge", async () => {
    const call = async () => ({
      configured: true,
      path: "/p/.yi-agent/mcp.json",
      enabled: true,
      servers: [
        { name: "fs", kind: "stdio", enabled: false, command: "npx", args: ["-y", "pkg"], tools: null },
      ],
    });
    render(<SettingsMcpTab call={call} projects={[]} />);
    expect(await screen.findByText("fs")).toBeTruthy();
    expect(screen.getByText("已停用")).toBeTruthy();
    expect(screen.getByText(/npx -y pkg/)).toBeTruthy();
  });

  it("distinguishes unprobed from a probe that found no tools", async () => {
    const call = async () => ({
      configured: true,
      path: "/p/.yi-agent/mcp.json",
      enabled: true,
      servers: [
        { name: "unprobed", kind: "stdio", command: "npx", tools: null },
        { name: "empty", kind: "stdio", command: "npx", tools: [] },
      ],
    });
    render(<SettingsMcpTab call={call} projects={[]} />);
    expect(await screen.findByText(/尚未探测/)).toBeTruthy();
    expect(screen.getByText(/未提供任何工具/)).toBeTruthy();
  });

  it("expands a server to list its tools", async () => {
    const call = async () => ({
      configured: true,
      path: "/p/.yi-agent/mcp.json",
      enabled: true,
      servers: [
        {
          name: "fs",
          kind: "stdio",
          command: "npx",
          tools: [{ name: "read_file", description: "读文件", read_only: true }],
          tool_count: 1,
          read_only_count: 1,
        },
      ],
    });
    render(<SettingsMcpTab call={call} projects={[]} />);
    fireEvent.click(await screen.findByRole("button", { name: "展开 fs 的工具清单" }));
    expect(screen.getByText("read_file")).toBeTruthy();
    expect(screen.getByText("读文件")).toBeTruthy();
  });

  it("scopes mcp/list to the selected project and re-lists on switch", async () => {
    const calls: unknown[] = [];
    const call = async (method: string, params: unknown) => {
      calls.push(params);
      return empty;
    };
    render(<SettingsMcpTab call={call} projects={["/p/first", "/p/second"]} />);
    await screen.findByText("未安装任何 MCP server");
    expect(calls[0]).toEqual({ project: "/p/first" });
    fireEvent.change(screen.getByLabelText("MCP 设置项目"), { target: { value: "/p/second" } });
    await screen.findByText("未安装任何 MCP server");
    expect(calls[1]).toEqual({ project: "/p/second" });
  });

  it("never renders header or env values", async () => {
    const call = async () => ({
      configured: true,
      path: "/p/.yi-agent/mcp.json",
      enabled: true,
      servers: [
        { name: "ws", kind: "http", url: "https://x/mcp", header_keys: ["Authorization"] },
      ],
    });
    const { container } = render(<SettingsMcpTab call={call} projects={[]} />);
    await screen.findByText("ws");
    // 键名可见、值不可见（值根本没进前端：服务端只下发键名）。
    expect(screen.getByText(/Authorization/)).toBeTruthy();
    expect(container.textContent).not.toContain("secret-token");
  });

  it("reports a read failure instead of rendering a blank pane", async () => {
    const call = async () => {
      throw new Error("boom");
    };
    render(<SettingsMcpTab call={call} projects={[]} />);
    expect(await screen.findByRole("alert")).toBeTruthy();
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/SettingsMcpTab.test.tsx 2>&1 | tail -20`
Expected: FAIL —— 无法解析 `./SettingsMcpTab`

- [ ] **Step 3: 写实现**

创建 `desktop/src/components/SettingsMcpTab.tsx`：

```tsx
import { useEffect, useRef, useState } from "react";
import { listMcpServers, type McpCall, type McpList, type McpRpc, type McpServerView } from "../lib/mcp";
import { SettingsProjectPicker } from "./SettingsProjectPicker";

const EMPTY: McpList = { configured: false, path: "", enabled: false, servers: [] };

/** stdio 的一行命令摘要（等宽显示、单行截断）。 */
function commandSummary(s: McpServerView): string {
  return [s.command ?? "", ...s.args].join(" ").trim();
}

/**
 * 「MCP」Tab。
 *
 * MCP 配置是**项目级文件**（`<项目>/.yi-agent/mcp.json`），而桌面侧车的 app-server
 * 以自己的 workdir（用户 home）为作用域，所以必须让用户明确「在看哪个项目」，并把它
 * 随每次 `mcp/list` 传下去——理由与「插件」「技能」两个 Tab 完全相同。
 *
 * **只读**：工具清单来自磁盘缓存（首次调用某个 server 时才探测），缓存缺失或指纹
 * 不匹配时显示「尚未探测」，而不是假装清单是最新的。不提供启停开关。
 */
export function SettingsMcpTab({
  call,
  projects,
  defaultProject,
}: {
  call?: McpCall;
  /** 可选项目（绝对路径）。空表时面板回落宿主 workdir。 */
  projects: string[];
  /** 首选项目（当前对话的工作目录）。不在 `projects` 里时回落到第一个。 */
  defaultProject?: string;
}) {
  const [listed, setListed] = useState<McpList>(EMPTY);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<string>(() => defaultProject ?? projects[0] ?? "");
  /** server 名 → 是否展开工具清单。折叠状态不丢，避免反复展开时重复拉取。 */
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});

  const rpc = call as McpRpc | undefined;

  // 选项 = 候选 ∪ 默认项目（当前对话目录可能不在候选里，只列候选就无法选中）。
  const options = (() => {
    const seen = new Set<string>();
    const out: string[] = [];
    for (const path of [defaultProject, ...projects]) {
      const trimmed = path?.trim();
      if (!trimmed || seen.has(trimmed)) continue;
      seen.add(trimmed);
      out.push(trimmed);
    }
    return out;
  })();

  // `project` 进 ref，理由同插件/技能面板：宿主每次重渲染可能换新接缝身份，但我们
  // 只在「项目真的换了」时重新枚举。
  const projectRef = useRef(selected);
  projectRef.current = selected;

  // 项目迟到（workspaces 异步到达）时补一个默认值；用户已选定的不覆盖。
  useEffect(() => {
    if (selected !== "") return;
    const next = defaultProject ?? projects[0] ?? "";
    if (next !== "") setSelected(next);
  }, [defaultProject, projects, selected]);

  useEffect(() => {
    if (!rpc) {
      setLoading(false);
      return;
    }
    let cancelled = false;
    setLoading(true);
    setError(null);
    void (async () => {
      try {
        const result = await listMcpServers(rpc, projectRef.current || undefined);
        if (!cancelled) setListed(result);
      } catch {
        if (!cancelled) setError("无法读取 MCP 配置");
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [rpc, selected]);

  if (!call) {
    return <p className="p-4 text-xs text-fg-subtle">当前连接无法读取 MCP 配置</p>;
  }
  if (error) {
    return (
      <p role="alert" className="p-4 text-xs text-amber-500">
        {error}
      </p>
    );
  }

  const visible = listed.servers;

  return (
    <div>
      <SettingsProjectPicker
        options={options}
        selected={selected}
        onSelect={setSelected}
        ariaLabel="MCP 设置项目"
      />
      {loading ? (
        <p className="p-4 text-xs text-fg-subtle">正在读取 MCP 配置…</p>
      ) : !listed.configured || visible.length === 0 ? (
        <div>
          <p className="p-4 text-sm text-fg-muted">未安装任何 MCP server</p>
          <AddMcpHelp path={listed.path} />
        </div>
      ) : (
        <>
          <p className="border-b border-line p-4 text-xs text-fg-subtle">
            MCP：{listed.enabled ? "已启用" : "已停用"}
          </p>
          <div className="divide-y divide-line">
            {visible.map((server) => {
              const open = expanded[server.name] === true;
              return (
                <section key={server.name} className="p-4">
                  <div className="flex items-center gap-2">
                    <h2 className="min-w-0 truncate text-sm font-medium text-fg">{server.name}</h2>
                    <span
                      className="shrink-0 rounded bg-raised px-1.5 py-0.5 text-[10px] text-fg-subtle"
                    >
                      {server.enabled ? "已启用" : "已停用"}
                    </span>
                  </div>
                  <p className="mt-1 truncate font-mono text-xs text-fg-subtle">
                    {server.kind === "http" ? (server.url ?? "") : commandSummary(server)}
                  </p>
                  {(server.envKeys.length > 0 || server.headerKeys.length > 0) && (
                    <p className="mt-1 truncate text-xs text-fg-subtle">
                      {server.envKeys.length > 0 && <>env: {server.envKeys.join(", ")}</>}
                      {server.envKeys.length > 0 && server.headerKeys.length > 0 && " · "}
                      {server.headerKeys.length > 0 && <>header: {server.headerKeys.join(", ")}</>}
                    </p>
                  )}
                  {/* 折叠范式与技能 Tab 一致：aria-expanded + ▸/▾。 */}
                  <button
                    type="button"
                    aria-expanded={open}
                    aria-label={`展开 ${server.name} 的工具清单`}
                    onClick={() => setExpanded((e) => ({ ...e, [server.name]: !open }))}
                    className="mt-2 flex w-full items-start gap-2 px-1 py-1 text-left"
                  >
                    <span aria-hidden="true" className="text-fg-subtle">
                      {open ? "▾" : "▸"}
                    </span>
                    <span className="text-xs text-fg-subtle">
                      {server.tools === null
                        ? "工具清单尚未探测（首次调用该 server 时自动拉取）"
                        : server.tools.length === 0
                          ? "该 server 未提供任何工具"
                          : `${server.toolCount} 个工具（${server.readOnlyCount} 个只读）`}
                    </span>
                  </button>
                  {open && server.tools !== null && server.tools.length > 0 && (
                    <ul className="mt-1 pl-6">
                      {server.tools.map((tool) => (
                        <li key={tool.name} className="py-1">
                          <span className="block text-sm text-fg">
                            {tool.name}
                            {tool.read_only && <span className="ml-2 text-xs text-fg-subtle">只读</span>}
                          </span>
                          <span className="block text-xs text-fg-subtle">{tool.description}</span>
                        </li>
                      ))}
                    </ul>
                  )}
                </section>
              );
            })}
          </div>
        </>
      )}
    </div>
  );
}

/**
 * 「如何添加 MCP server」说明。
 *
 * 路径取服务端返回的**真实解析值**，不硬编码 `~/.yi-agent`：路径规则若变动，
 * 硬编码的文案会与事实脱节（与技能 Tab 的 `AddSkillHelp` 同一理由）。
 */
function AddMcpHelp({ path }: { path: string }) {
  return (
    <section className="border-t border-line p-4">
      <h3 className="text-sm font-medium text-fg">如何添加 MCP server</h3>
      <p className="mt-2 text-xs text-fg-muted">
        把 server 写进下面这个文件（Claude Desktop 兼容的 schema），重启侧车后生效：
      </p>
      {path !== "" && (
        <p className="mt-1 break-all font-mono text-xs text-fg-subtle">{path}</p>
      )}
      <pre className="mt-3 overflow-x-auto rounded border border-line bg-surface p-2 font-mono text-xs text-fg-muted">
{`{
  "mcpServers": {
    "fs": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path"] },
    "ws": { "type": "http", "url": "https://example.com/mcp" }
  }
}`}
      </pre>
      <p className="mt-2 text-xs text-fg-subtle">
        工具清单在首次调用该 server 时自动探测并缓存，本页的清单因此可能滞后于配置改动。
      </p>
    </section>
  );
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/SettingsMcpTab.test.tsx 2>&1 | tail -25`
Expected: 7 个用例全 PASS

- [ ] **Step 5: 类型检查并提交**

```bash
cd desktop && npx tsc --noEmit
cd .. && git add desktop/src/components/SettingsMcpTab.tsx desktop/src/components/SettingsMcpTab.test.tsx
git commit -m "feat(desktop): add the read-only MCP settings panel"
```

---

### Task 5: 接进设置对话框与 App

**Files:**
- Modify: `desktop/src/components/SettingsDialog.tsx`（`TABS`、props、分派）
- Modify: `desktop/src/components/SettingsDialog.test.tsx`（Tab 名列表两处 + 一个新用例）
- Modify: `desktop/src/App.tsx`（`mcpCall` + 三个 props）

**Interfaces:**
- Consumes: `SettingsMcpTab`（Task 4）
- Produces: `SettingsDialog` 新增 props `mcpCall?: McpCall`、`mcpProjects?: string[]`、`mcpDefaultProject?: string`

- [ ] **Step 1: 写失败的测试**

在 `desktop/src/components/SettingsDialog.test.tsx` 中：

(a) 在 `shows a 技能 tab backed by the injected skillsCall` 之后加：

```tsx
  it("shows a MCP tab backed by the injected mcpCall", async () => {
    const mcpCall = async () => ({
      configured: true,
      path: "/p/.yi-agent/mcp.json",
      enabled: true,
      servers: [{ name: "horizon-websearch", kind: "http", url: "https://x/mcp", tools: null }],
    });
    render(
      <SettingsDialog
        open
        theme="dark"
        onThemeChange={() => {}}
        onClose={() => {}}
        mcpCall={mcpCall}
      />,
    );
    fireEvent.click(screen.getByRole("tab", { name: "MCP" }));
    expect(await screen.findByText("horizon-websearch")).toBeTruthy();
  });
```

(b) 断言桌面 Tab 名列表的用例（`"lists all six categories on the desktop"`）改为：

```tsx
  it("lists all seven categories on the desktop", () => {
    render(<SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={() => {}} />);
    const names = screen.getAllByRole("tab").map((t) => t.textContent);
    expect(names).toEqual(["通用", "远程访问", "插件", "模型", "技能", "MCP", "标签"]);
  });
```

(c) 手机分类列表的用例（`"renders a category list, not a tablist, on the phone"`）期望数组改为：

```tsx
    expect(rows.map((r) => r.getAttribute("data-settings-category"))).toEqual([
      "general",
      "remote",
      "plugins",
      "models",
      "skills",
      "mcp",
      "tags",
    ]);
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/SettingsDialog.test.tsx 2>&1 | tail -25`
Expected: 3 个用例 FAIL（找不到 `MCP` tab / 分类 `mcp`）

- [ ] **Step 3: 改 `SettingsDialog.tsx`**

(a) `TABS`（第 15-22 行）在 `skills` 与 `tags` 之间插入：

```tsx
  { id: "mcp", label: "MCP" },
```

(b) 文件顶部 import 区加：

```tsx
import { SettingsMcpTab } from "./SettingsMcpTab";
import type { McpCall } from "../lib/mcp";
```

(c) 组件 props 解构与类型里加三个可选 prop（与 `skillsProjects`/`skillsDefaultProject` 相邻）：

```tsx
  mcpCall,
  mcpProjects,
  mcpDefaultProject,
```

```tsx
  /** 「MCP」Tab 的宿主 RPC 接缝（mcp/list）。 */
  mcpCall?: McpCall;
  /** 「MCP」Tab 可选的配置项目（绝对路径）。 */
  mcpProjects?: string[];
  /** 「MCP」Tab 首选项目（当前对话的工作目录）。 */
  mcpDefaultProject?: string;
```

(d) 面板分派里，`active === "skills"` 那一支之后加：

```tsx
            ) : active === "mcp" ? (
              <SettingsMcpTab
                call={mcpCall}
                projects={mcpProjects ?? []}
                defaultProject={mcpDefaultProject}
              />
```

- [ ] **Step 4: 改 `App.tsx`**

(a) import 区（`SettingsDialog` 附近）加：

```tsx
import type { McpCall } from "./lib/mcp";
```

（若 App.tsx 不直接需要类型，可省略 import，仅靠 props 传给 SettingsDialog；本仓库既有做法是显式引入接缝类型。）

(b) 在 `skillsCall` 之后加一个稳定身份的接缝（照既有注释风格）：

```tsx
  /**
   * 「MCP」Tab 的宿主接缝。理由同 `pluginCall`：只闭合 `clientRef`，空依赖即稳定；
   * 若每次渲染换新身份，MCP 面板的读取 effect 会跟着重跑，把已展开的工具清单
   * 折叠状态冲掉。
   */
  const mcpCall: McpCall = useCallback(
    (method: string, params: unknown): Promise<unknown> => {
      const c = clientRef.current;
      if (!c) return Promise.reject(new Error("not connected"));
      return c.request(method, params);
    },
    [],
  );
```

(c) `SettingsDialog` 的 props 里，`skillsProjects` / `skillsDefaultProject` 之后加：

```tsx
        // MCP 的配置也是项目级文件（<项目>/.yi-agent/mcp.json），候选与默认值直接
        // 复用插件/技能那份——三者指的都是「项目目录下的 .yi-agent」，来源分叉会让
        // 用户在三个面板里看到不同的项目列表。
        mcpCall={mcpCall}
        mcpProjects={pluginProjectPaths(sess.workspaces, board.boards)}
        mcpDefaultProject={current?.info?.cwd}
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/SettingsDialog.test.tsx src/App.test.tsx 2>&1 | tail -25`
Expected: 全 PASS

- [ ] **Step 6: 全量前端验证并提交**

```bash
cd desktop && npx tsc --noEmit && npx vitest run 2>&1 | tail -15
```

Expected: 全 PASS；若 `App.test.tsx` 有断言 Tab 数量的用例，同样按「多一项 MCP」更新

```bash
cd .. && git add desktop/src/components/SettingsDialog.tsx \
  desktop/src/components/SettingsDialog.test.tsx desktop/src/App.tsx
git commit -m "feat(desktop): expose the MCP panel from the settings dialog"
```

---

### Task 6: 文档同步

**Files:**
- Modify: `docs/project-management/desktop.md`（P3 那条 `[ ] MCP 集成` 拆两条）
- Modify: `docs/project-management/yi-agent-mcp.md`（补设置页接缝一行）
- Modify: `docs/project-management/README.md`（若计数受影响）

**Interfaces:**
- Consumes: 前面全部任务的成果
- Produces: 无代码接口

- [ ] **Step 1: 改 `desktop.md`**

把 P3 里的：

```markdown
- [ ] MCP 集成 — 后端 `yi-agent-mcp` 已完成（配置/懒连接/调用/`/mcp` 开关，见 [yi-agent-mcp](./yi-agent-mcp.md)）；判据：GUI 可查看 / 启停 MCP server
```

替换为两条（第一条的测试命令与代码位置必须与实现一致）：

```markdown
- [x] MCP 面板（只读查看）— 设置页新增「MCP」Tab，按项目列出 server 名字 / 传输（stdio 的 `command args`、http 的 `url`）/ 文件里的开关默认值 / 缓存中的工具清单（可展开，显示名字 + 一行描述 + 只读标记）；`env`/`headers` **只显示键名**，值不下发；缓存缺失或指纹不匹配时显示「工具清单尚未探测」而不是假装最新。后端 `mcp/list`（`Observe` 档，纯读盘、不起进程、不发网络）：`yi-agent-rs/crates/yi-agent-app-server/src/mcp_rpc.rs`、域 handler `.../src/rpc/mcp.rs`、缓存视图 `yi-agent-rs/crates/yi-agent-mcp/src/lib.rs:McpCacheEntry`/`cached_tools_for`；前端 `desktop/src/components/SettingsMcpTab.tsx`、`desktop/src/lib/mcp.ts`。判据：`cd desktop && npx vitest run src/components/SettingsMcpTab.test.tsx src/lib/mcp.test.ts src/components/SettingsDialog.test.tsx && npx tsc --noEmit` + `cd yi-agent-rs && cargo test -p yi-agent-app-server mcp` — [设计](../superpowers/specs/2026-10-18-mcp-settings-panel-design.md)
- [ ] MCP 启停 — 在面板上启用/停用 server（需先让 app-server 留存 `McpManager` 并接通 `refresh_registry` + agent 重建，令改动对活着的会话生效）；判据：GUI 可启停 MCP server，且新开的 turn 里工具集随之变化
```

- [ ] **Step 2: 改 `yi-agent-mcp.md`**

在「Features」的 `[x] Streamable HTTP transport` 条目之后加：

```markdown
- [x] 设置页只读接缝
  - 公开视图：`yi-agent-rs/crates/yi-agent-mcp/src/lib.rs` 的 `McpCacheEntry` 与 `cached_tools_for`（只回名字/描述/read_only，不带 `input_schema`；指纹不匹配回 `None`）
  - RPC：`mcp/list`（`yi-agent-rs/crates/yi-agent-app-server/src/mcp_rpc.rs`），`Observe` 档，读 `.yi-agent/mcp.json` + `.yi-agent/mcp-cache.json`
  - 验证：`cargo test -p yi-agent-app-server --lib mcp_rpc`、`cargo test -p yi-agent-app-server mcp_list_is_available_at_observe_scope`
```

- [ ] **Step 3: 检查 `README.md` 计数**

Run: `grep -n "desktop\|yi-agent-mcp" docs/project-management/README.md`
若有「完成 / 总计」计数行且本次改变了完成数，同步更新；若无相关计数，本步不改文件。

- [ ] **Step 4: 提交**

```bash
git add docs/project-management/desktop.md docs/project-management/yi-agent-mcp.md docs/project-management/README.md
git commit -m "docs(mcp): record the read-only settings panel"
```

---

### Task 7: 合并回 main

**Files:** 无文件改动

**Interfaces:**
- Consumes: 分支上全部 commit
- Produces: `main` 上的合并提交

- [ ] **Step 1: 合并前完整验证**

```bash
# 先确认没有残留 cargo 进程（仓库 CLAUDE.md：多个 cargo 并发会锁竞争 / OOM）
ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent" || true
cd yi-agent-rs && cargo fmt --all --check && cargo test -p yi-agent-app-server -p yi-agent-mcp 2>&1 | tail -20
cd ../desktop && npx tsc --noEmit && npx vitest run 2>&1 | tail -15
```

Expected: 全 PASS（`cargo fmt --check` 无输出即通过）

- [ ] **Step 2: 合并回 main**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
git checkout main
git merge --no-ff feat/mcp-settings-panel -m "merge: 设置面板新增只读 MCP 视图"
```

- [ ] **Step 3: 清理 worktree 与分支**

```bash
git worktree remove .worktrees/feat-mcp-settings-panel
git branch -d feat/mcp-settings-panel
git worktree list | grep mcp-settings-panel || echo "worktree removed"
```

- [ ] **Step 4: 合并后在 main 上复跑关键验证**

```bash
git log --oneline -3
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib mcp_rpc 2>&1 | tail -10
cd ../desktop && npx vitest run src/components/SettingsMcpTab.test.tsx 2>&1 | tail -10
```

Expected: 合并提交在 `main` 顶部；测试 PASS