# MCP Streamable HTTP Transport Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给 `yi-agent-mcp` 增加 MCP Streamable HTTP transport（仅 POST + JSON），从而能把远端 HTTP MCP server（如 `horizon-websearch`）配进 `.yi-agent/mcp.json` 并暴露其工具给 agent。

**Architecture:** 配置层把 `ServerConfig` 扩展为"单结构体 + `type` 判别"（缺省 stdio，`http` 走 url/headers），新增 `transport()` 把配置降维成 `Transport::{Stdio, Http}` 并集中校验。接线层只改 `manager.rs` 的 `connect()`：按 `Transport` 分派到现有 `TokioChildProcess` 或新的 `StreamableHttpClientTransport`，两条路径都产出同一个 `RunningService<RoleClient, ()>`，因此懒连接/重连/超时/shutdown/结果映射全部复用。缓存指纹扩展为覆盖 `kind`/`url`/`headers`。

**Tech Stack:** Rust（edition 2024）、`rmcp 3.5`（新增 `transport-streamable-http-client-reqwest` + `reqwest` feature）、`http 1`（header 类型）、`wiremock 0.6`（集成测试 dev-dep）。

设计依据：`docs/superpowers/specs/2026-10-13-mcp-http-transport-design.md`。

## Global Constraints

- **不在 `main` 分支直接改动**：全部在 worktree `.worktrees/feat-mcp-http-transport`（分支 `feat/mcp-http-transport`）内进行。所有 `cargo` 命令在 `<worktree>/yi-agent-rs` 下执行。
- 提交前在 `<worktree>/yi-agent-rs` 跑 `cargo fmt --all`；commit message 用 conventional commits，首行 ≤72 字符，**不写** `Co-Authored-By`。
- 跑测试按 crate：`cargo test -p yi-agent-mcp`。跑前 `ps aux | grep cargo` 确认无残留 cargo 进程。
- **零回归硬要求**：现有 stdio 配置（无 `type` 字段、有 `command`）解析结果与行为**不变**；`cargo test -p yi-agent-mcp` 与 `cargo test -p yi-agent-runtime` 须全绿。
- `rmcp` feature 固定为：`["client", "transport-child-process", "transport-streamable-http-client-reqwest", "reqwest"]`（`reqwest` feature 启用 rustls，HTTPS 必需）。
- `${ENV_VAR}` 展开**只作用于 http 的 header 值**；未定义变量 → 丢弃该 header + warn。缓存指纹用**未展开的原文**。
- 端点为 stateless（无 `Mcp-Session-Id`），依赖 rmcp `allow_stateless` 默认 `true`。

---

## File Structure

**新增**
- `yi-agent-rs/crates/yi-agent-mcp/tests/http_transport.rs` — Streamable HTTP 传输的 wiremock 集成测试 + `#[ignore]` 真实端点冒烟测试

**修改**
- `yi-agent-rs/crates/yi-agent-mcp/Cargo.toml` — 新增 rmcp features、`http` 依赖、`wiremock` dev-dep
- `yi-agent-rs/crates/yi-agent-mcp/src/config.rs` — `ServerKind` / `ServerConfig` 扩展 / `Transport` / `transport()` / `expand_env` / `expand_headers`
- `yi-agent-rs/crates/yi-agent-mcp/src/lib.rs` — re-export 新类型 + 更新测试内的 `ServerConfig` 构造
- `yi-agent-rs/crates/yi-agent-mcp/src/cache.rs` — 指纹扩展 + 更新测试 helper
- `yi-agent-rs/crates/yi-agent-mcp/src/manager.rs` — `connect()` 分派 + 更新测试 helper
- `docs/project-management/yi-agent-mcp.md` — 记录 HTTP transport 能力

---

## Task 1: 扩展配置 schema + `Transport` 枚举 + env 展开

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/config.rs`
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/lib.rs`（re-export + 测试内构造点）
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/cache.rs`（测试 helper 构造点）
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/manager.rs`（测试 helper 构造点）
- Test: `yi-agent-rs/crates/yi-agent-mcp/src/config.rs`（`#[cfg(test)] mod tests`）

**Interfaces:**
- Produces:
  - `pub enum ServerKind { Stdio, Http }`（`Default = Stdio`，serde `rename_all = "lowercase"`）
  - `pub struct ServerConfig { kind: ServerKind, command: Option<String>, args: Vec<String>, env: BTreeMap<String,String>, url: Option<String>, headers: BTreeMap<String,String>, enabled: bool }`
  - `pub enum Transport { Stdio { command: String, args: Vec<String>, env: BTreeMap<String,String> }, Http { url: String, headers: BTreeMap<String,String> } }`
  - `impl ServerConfig { pub fn transport(&self) -> Result<Transport> }`
  - `pub(crate) fn expand_env(value: &str) -> Option<String>`
  - `pub(crate) fn expand_headers(headers: &BTreeMap<String,String>) -> BTreeMap<String,String>`

- [ ] **Step 1: 写失败测试** — 在 `config.rs` 的 `#[cfg(test)] mod tests` 内追加以下测试（同时**替换**现有 `rejects_missing_command`，见 Step 3）：

```rust
    #[test]
    fn stdio_config_without_type_still_parses_as_stdio() {
        let raw = r#"{"mcpServers":{"fs":{"command":"npx","args":["-y","pkg"]}}}"#;
        let cfg: McpConfig = serde_json::from_str(raw).unwrap();
        let fs = cfg.mcp_servers.get("fs").unwrap();
        assert_eq!(fs.kind, ServerKind::Stdio);
        assert_eq!(fs.command.as_deref(), Some("npx"));
        match fs.transport().unwrap() {
            Transport::Stdio { command, args, .. } => {
                assert_eq!(command, "npx");
                assert_eq!(args, vec!["-y", "pkg"]);
            }
            other => panic!("expected Stdio, got {other:?}"),
        }
    }

    #[test]
    fn http_config_parses_with_url_and_headers() {
        let raw = r#"{"mcpServers":{"ws":{"type":"http","url":"https://x/mcp","headers":{"Authorization":"Bearer t"}}}}"#;
        let cfg: McpConfig = serde_json::from_str(raw).unwrap();
        let ws = cfg.mcp_servers.get("ws").unwrap();
        assert_eq!(ws.kind, ServerKind::Http);
        match ws.transport().unwrap() {
            Transport::Http { url, headers } => {
                assert_eq!(url, "https://x/mcp");
                assert_eq!(headers.get("Authorization").map(String::as_str), Some("Bearer t"));
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn http_config_without_url_is_rejected_by_transport() {
        let raw = r#"{"mcpServers":{"ws":{"type":"http"}}}"#;
        let cfg: McpConfig = serde_json::from_str(raw).unwrap();
        assert!(cfg.mcp_servers.get("ws").unwrap().transport().is_err());
    }

    #[test]
    fn stdio_config_without_command_is_rejected_by_transport() {
        // 解析期通过(命令改为 Option),校验期报错。
        let raw = r#"{"mcpServers":{"bad":{"args":[]}}}"#;
        let cfg: McpConfig = serde_json::from_str(raw).unwrap();
        assert!(cfg.mcp_servers.get("bad").unwrap().transport().is_err());
    }

    #[test]
    fn expand_env_replaces_defined_and_rejects_undefined() {
        // PATH 在任何环境都存在;用超长随机名保证未定义。
        assert!(expand_env("${PATH}").is_some());
        assert_eq!(expand_env("plain"), Some("plain".into()));
        assert_eq!(expand_env("${DEFINITELY_UNSET_YI_AGENT_VAR_XYZ}"), None);
        assert_eq!(expand_env("pre/${NOPE_XYZ_ABC}"), None);
    }

    #[test]
    fn expand_headers_drops_only_undefined() {
        let mut h = BTreeMap::new();
        h.insert("A".to_string(), "plain".to_string());
        h.insert("B".to_string(), "${DEFINITELY_UNSET_YI_AGENT_VAR_XYZ}".to_string());
        let out = expand_headers(&h);
        assert_eq!(out.get("A").map(String::as_str), Some("plain"));
        assert!(out.get("B").is_none(), "undefined env header must be dropped");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd <worktree>/yi-agent-rs && cargo test -p yi-agent-mcp --lib config:: 2>&1 | tail -30`
Expected: 编译失败（`ServerKind`/`Transport`/`transport`/`expand_env` 未定义）。

- [ ] **Step 3: 实现** — 把 `config.rs` 整体替换为下述内容（保留原有 doc comment 风格；`rejects_missing_command` 已被上面的新测试取代，删掉它）：

```rust
//! `.yi-agent/mcp.json` 配置解析(兼容 Claude Desktop 的 schema)。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use tracing::warn;

/// `.yi-agent/mcp.json` 的根结构。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpConfig {
    /// 总开关,缺省为 `true`。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 各 MCP server 的配置,键为 server 名。
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: BTreeMap<String, ServerConfig>,
}

/// 传输类型。缺省为 `Stdio`(向后兼容没有 `type` 的老配置)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerKind {
    #[default]
    Stdio,
    Http,
}

/// `mcpServers` 中的单个 server 配置。
///
/// 单结构体 + `type` 判别(而非 serde tagged enum):对无 `type`、有 `command`
/// 的老配置零破坏,校验集中在 [`ServerConfig::transport`] 一处。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ServerConfig {
    /// 传输类型:缺省 stdio。
    #[serde(rename = "type", default)]
    pub kind: ServerKind,
    /// stdio:启动 MCP server 的可执行文件。`kind = Stdio` 时必填(校验期)。
    #[serde(default)]
    pub command: Option<String>,
    /// stdio:传给可执行文件的参数。
    #[serde(default)]
    pub args: Vec<String>,
    /// stdio:追加到子进程的环境变量。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// http:远端 MCP endpoint 的 URL。`kind = Http` 时必填(校验期)。
    #[serde(default)]
    pub url: Option<String>,
    /// http:每个请求附带的 header。值支持 `${ENV_VAR}` 展开。
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// 单个 server 的开关,缺省为 `true`。
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// 校验后的传输形态:把 [`ServerConfig`] 降维成两个明确变体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

fn default_true() -> bool {
    true
}

impl ServerConfig {
    /// 校验并降维成 [`Transport`]。缺必填项时返回带原因的 `Err`。
    pub fn transport(&self) -> Result<Transport> {
        match self.kind {
            ServerKind::Stdio => {
                let command = self
                    .command
                    .clone()
                    .filter(|c| !c.is_empty())
                    .ok_or_else(|| anyhow!("stdio MCP server requires a non-empty `command`"))?;
                Ok(Transport::Stdio {
                    command,
                    args: self.args.clone(),
                    env: self.env.clone(),
                })
            }
            ServerKind::Http => {
                let url = self
                    .url
                    .clone()
                    .filter(|u| !u.is_empty())
                    .ok_or_else(|| anyhow!("http MCP server requires a non-empty `url`"))?;
                Ok(Transport::Http {
                    url,
                    headers: self.headers.clone(),
                })
            }
        }
    }
}

/// 展开字符串中的 `${NAME}` 引用。
///
/// 任一所引用的环境变量未定义时返回 `None`(调用方据此丢弃整个 header,避免
/// 发出空值导致下游 401 难排查)。未闭合的 `${` 按字面量保留。
pub(crate) fn expand_env(value: &str) -> Option<String> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return Some(out);
        };
        let name = &after[..end];
        let val = std::env::var(name).ok()?;
        out.push_str(&val);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// 对一组 header 做 [`expand_env`];未定义变量的 header 被丢弃并 warn。
pub(crate) fn expand_headers(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (k, v) in headers {
        match expand_env(v) {
            Some(expanded) => {
                out.insert(k.clone(), expanded);
            }
            None => warn!(header = %k, "dropping MCP http header: referenced env var is unset"),
        }
    }
    out
}

/// 给定工作目录下的配置文件路径。
pub fn config_path(workdir: &Path) -> PathBuf {
    workdir.join(".yi-agent").join("mcp.json")
}

impl McpConfig {
    /// 加载 `workdir` 下的 `.yi-agent/mcp.json`。
    ///
    /// 文件不存在时返回 `Ok(None)`(即未配置 MCP)。
    pub fn load(workdir: &Path) -> Result<Option<Self>> {
        let path = config_path(workdir);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let cfg =
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        Ok(Some(cfg))
    }
}
```

注意 `with_context` 需要 `use anyhow::Context;`——把 import 行改为：

```rust
use anyhow::{Context, Result, anyhow};
```

保留文件中原有的 `load_*` 测试（`load_returns_none_when_absent` 等），它们不受影响。

- [ ] **Step 4: 修所有 `ServerConfig { .. }` 构造点** — `command` 现为 `Option`，且新增 `kind`/`url`/`headers` 字段。逐处修改：

`config.rs` 测试内（`parses_minimal_config_with_defaults`）：把 `assert_eq!(fs.command, "npx")` 改为 `assert_eq!(fs.command.as_deref(), Some("npx"))`。

`cache.rs` 测试 helper（约 112 行，保留其原有 `args` 值）：

```rust
    fn server(cmd: &str) -> ServerConfig {
        ServerConfig {
            kind: ServerKind::Stdio,
            command: Some(cmd.into()),
            args: vec!["--x".into()],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
        }
    }
```

并把 `cache.rs` 顶部 `use crate::config::ServerConfig;` 改为 `use crate::config::{ServerConfig, ServerKind};`。

`manager.rs` 测试 helper `cfg`（约 543 行）：

```rust
    fn cfg(cmd: &str) -> ServerConfig {
        ServerConfig {
            kind: ServerKind::Stdio,
            command: Some(cmd.into()),
            args: vec![],
            env: Default::default(),
            url: None,
            headers: Default::default(),
            enabled: true,
        }
    }
```

`manager.rs` 内联构造（约 800 行 `probe_then_call_over_duplex`）：同样补齐 `kind: ServerKind::Stdio, command: Some("echo".into()), ..` 与 `url: None, headers: BTreeMap::new(),`。

`lib.rs` 测试（约 98 行）：同样补齐字段。

`manager.rs` 顶部把 `use crate::config::{McpConfig, ServerConfig};` 改为 `use crate::config::{McpConfig, ServerConfig, ServerKind, Transport};`（`Transport` 在 Task 3 用到，先加上）。
`cache.rs` 顶部加 `ServerKind` import（见上）。

- [ ] **Step 5: re-export 新类型** — `lib.rs` 中：

```rust
pub use config::{McpConfig, ServerConfig, ServerKind, Transport};
```

（替换原 `pub use config::{McpConfig, ServerConfig};`）

- [ ] **Step 6: 运行测试确认通过**

Run: `cd <worktree>/yi-agent-rs && cargo test -p yi-agent-mcp --lib config:: 2>&1 | tail -30`
Expected: PASS（新 config 测试全绿）。

再跑整包确认无回归：

Run: `cd <worktree>/yi-agent-rs && cargo test -p yi-agent-mcp 2>&1 | tail -30`
Expected: 全部 PASS。

- [ ] **Step 7: 格式化并提交**

```bash
cd <worktree>/yi-agent-rs && cargo fmt --all
cd <worktree> && git add yi-agent-rs/crates/yi-agent-mcp/src/config.rs yi-agent-rs/crates/yi-agent-mcp/src/lib.rs yi-agent-rs/crates/yi-agent-mcp/src/cache.rs yi-agent-rs/crates/yi-agent-mcp/src/manager.rs
git commit -m "feat(mcp): add http transport kind to server config schema"
```

---

## Task 2: 缓存指纹覆盖 `kind`/`url`/`headers`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/cache.rs`
- Test: `yi-agent-rs/crates/yi-agent-mcp/src/cache.rs`（`#[cfg(test)] mod tests`）

**Interfaces:**
- Consumes: `ServerConfig`（Task 1）、`ServerKind`（Task 1）
- Produces: `fn fingerprint(cfg: &ServerConfig) -> String`（签名不变，覆盖范围扩大）

- [ ] **Step 1: 写失败测试** — 在 `cache.rs` 的 `#[cfg(test)] mod tests` 内追加：

```rust
    #[test]
    fn fingerprint_changes_when_url_changes() {
        let a = http_server("https://x/one");
        let b = http_server("https://x/two");
        let mut c = McpCache::default();
        c.put("ws", &a, vec![]);
        assert!(c.get_for("ws", &a).is_some(), "same config must hit");
        assert!(c.get_for("ws", &b).is_none(), "changed url must miss");
    }

    fn http_server(url: &str) -> ServerConfig {
        ServerConfig {
            kind: ServerKind::Http,
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            url: Some(url.to_string()),
            headers: BTreeMap::new(),
            enabled: true,
        }
    }
```

（`BTreeMap` 已在该文件的测试模块 import 中；若没有则加 `use std::collections::BTreeMap;`。）

- [ ] **Step 2: 运行测试确认失败**

Run: `cd <worktree>/yi-agent-rs && cargo test -p yi-agent-mcp --lib cache:: 2>&1 | tail -30`
Expected: FAIL —— `changed url must miss`（当前指纹不含 url，改 url 仍命中）。

- [ ] **Step 3: 实现** — 把 `fingerprint` 函数替换为：

```rust
/// 计算 `ServerConfig` 的确定性指纹(SHA-256 十六进制摘要)。
///
/// 覆盖传输相关的全部字段(kind+command+args+env+url+headers);`enabled` 是
/// 运行时开关,不影响 schema,故意排除以免切换开关时无谓地让缓存失效。摘要
/// 而非原文,避免把 `env`/`headers` 里的 token/密钥明文落到 `.yi-agent/mcp-cache.json`。
/// header 值里的 `${ENV_VAR}` 以**未展开原文**参与指纹,轮换 token 不失效缓存。
fn fingerprint(cfg: &ServerConfig) -> String {
    let key = serde_json::json!({
        "kind": cfg.kind,
        "command": cfg.command,
        "args": cfg.args,
        "env": cfg.env,
        "url": cfg.url,
        "headers": cfg.headers,
    });
    // 序列化 `serde_json::Value` 不会失败;显式 panic 好过静默回退成空串
    // (空串会让所有配置都判定为命中,从而一直提供过期 schema)。
    let json = serde_json::to_string(&key).expect("serializing a serde_json::Value is infallible");
    let digest = Sha256::digest(json.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd <worktree>/yi-agent-rs && cargo test -p yi-agent-mcp --lib cache:: 2>&1 | tail -30`
Expected: PASS（含新增两条 + 原有指纹测试）。

- [ ] **Step 5: 提交**

```bash
cd <worktree>/yi-agent-rs && cargo fmt --all
cd <worktree> && git add yi-agent-rs/crates/yi-agent-mcp/src/cache.rs
git commit -m "feat(mcp): include transport fields in the cache fingerprint"
```

---

## Task 3: 在 `connect()` 分派 stdio / http + 启用依赖

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-mcp/Cargo.toml`
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/manager.rs`
- Test: 由 Task 4 的集成测试覆盖（本任务先让包能编译且既有测试全绿）

**Interfaces:**
- Consumes: `ServerConfig::transport()` → `Transport`（Task 1）、`expand_headers`（Task 1）
- Produces: `async fn connect(cfg: &ServerConfig) -> Result<Client>`（签名不变；内部按 transport 分派）

- [ ] **Step 1: 改 `Cargo.toml` 依赖**

把 `[dependencies]` 的 rmcp 行改为：

```toml
rmcp = { version = "3.5", default-features = false, features = ["client", "transport-child-process", "transport-streamable-http-client-reqwest", "reqwest"] }
```

在同一 `[dependencies]` 段新增：

```toml
http = "1"
```

在 `[dev-dependencies]` 段新增：

```toml
wiremock = "0.6"
```

- [ ] **Step 2: 确认依赖可解析**

Run: `cd <worktree>/yi-agent-rs && cargo build -p yi-agent-mcp 2>&1 | tail -20`
Expected: 编译成功（HTTP transport 类型可用；尚未接线所以无行为变化）。

- [ ] **Step 3: 替换 `connect()`** — 把 `manager.rs` 中的 `async fn connect(cfg: &ServerConfig) -> Result<Client>` 整个替换为：

```rust
/// 按 `cfg.transport()` 分派到 stdio 子进程或 Streamable HTTP,完成 MCP 握手。
///
/// 两条路径都产出同一个 `Client`,因此懒连接/重连/超时/shutdown 逻辑无需分支。
async fn connect(cfg: &ServerConfig) -> Result<Client> {
    #[cfg(test)]
    {
        if let Some(factory) = test_connector() {
            return factory().await;
        }
    }

    use rmcp::ServiceExt;

    match cfg.transport()? {
        Transport::Stdio { command, args, env } => {
            use rmcp::transport::TokioChildProcess;

            let mut cmd = tokio::process::Command::new(&command);
            cmd.args(&args);
            for (k, v) in &env {
                cmd.env(k, v);
            }
            let transport = TokioChildProcess::new(cmd)?;
            Ok(().serve(transport).await?)
        }
        Transport::Http { url, headers } => {
            use rmcp::transport::StreamableHttpClientTransport;
            use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

            let expanded = crate::config::expand_headers(&headers);
            let mut http_cfg = StreamableHttpClientTransportConfig::with_uri(url);
            if !expanded.is_empty() {
                let mut map = std::collections::HashMap::new();
                for (k, v) in &expanded {
                    let name = http::HeaderName::from_bytes(k.as_bytes())
                        .map_err(|e| anyhow!("invalid http header name {k:?}: {e}"))?;
                    let value = http::HeaderValue::from_str(v)
                        .map_err(|e| anyhow!("invalid http header value for {k:?}: {e}"))?;
                    map.insert(name, value);
                }
                http_cfg = http_cfg.custom_headers(map);
            }
            let transport = StreamableHttpClientTransport::from_config(http_cfg);
            Ok(().serve(transport).await?)
        }
    }
}
```

- [ ] **Step 4: 运行包测试确认无回归**

Run: `cd <worktree>/yi-agent-rs && cargo test -p yi-agent-mcp 2>&1 | tail -30`
Expected: 全部 PASS（原有 stdio 与 duplex 测试不受影响）。

- [ ] **Step 5: 提交**

```bash
cd <worktree>/yi-agent-rs && cargo fmt --all
cd <worktree> && git add yi-agent-rs/crates/yi-agent-mcp/Cargo.toml yi-agent-rs/Cargo.lock yi-agent-rs/crates/yi-agent-mcp/src/manager.rs
git commit -m "feat(mcp): dispatch stdio and streamable http in connect"
```

---

## Task 4: wiremock 集成测试（HTTP 协议往返）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-mcp/tests/http_transport.rs`

**Interfaces:**
- Consumes: `yi_agent_mcp::McpManager::load_and_probe(cfg, workdir)`、`McpManager::call_tool(server, tool, args)`、`McpManager::enabled_tools()`、`yi_agent_mcp::{McpConfig, ServerConfig, ServerKind}`

- [ ] **Step 1: 写集成测试** — 新建 `tests/http_transport.rs`：

```rust
//! Streamable HTTP transport 端到端测试:用 wiremock 充当远端 MCP server,
//! 走真实 HTTP 往返完成 initialize / notifications/initialized / tools/list /
//! tools/call。

use std::collections::BTreeMap;

use serde_json::{Value, json};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use yi_agent_mcp::{McpConfig, McpManager, ServerConfig, ServerKind};

/// 一个最小的 Streamable HTTP MCP server:按 JSON-RPC method 分派响应。
struct McpHttpMock;

impl Respond for McpHttpMock {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = match request.body_json() {
            Ok(v) => v,
            Err(_) => return ResponseTemplate::new(400),
        };
        let method = body.get("method").and_then(Value::as_str).unwrap_or("");
        let id = body.get("id").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "mock", "version": "0.1" }
                    }
                })),
            // Notification:无 id,返回 202 Accepted。
            "notifications/initialized" => ResponseTemplate::new(202),
            "tools/list" => ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": [{
                            "name": "SearchPro",
                            "description": "web search",
                            "inputSchema": {
                                "type": "object",
                                "properties": { "Query": { "type": "string" } },
                                "required": ["Query"]
                            }
                        }]
                    }
                })),
            "tools/call" => ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": "mock-search-result" }],
                        "isError": false
                    }
                })),
            other => ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("method not found: {other}") }
                })),
        }
    }
}

fn http_server(url: &str) -> ServerConfig {
    ServerConfig {
        kind: ServerKind::Http,
        command: None,
        args: vec![],
        env: BTreeMap::new(),
        url: Some(url.to_string()),
        headers: BTreeMap::new(),
        enabled: true,
    }
}

#[tokio::test]
async fn probes_and_calls_over_streamable_http() {
    let server = MockServer::start().await;

    // 任何 POST 由 mock 按 method 分派;GET 不支持(405),与真实端点一致。
    Mock::given(wiremock::matchers::method("POST"))
        .respond_with(McpHttpMock)
        .mount(&server)
        .await;
    Mock::given(wiremock::matchers::method("GET"))
        .respond_with(ResponseTemplate::new(405))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let sc = http_server(&server.uri());

    let mut servers = BTreeMap::new();
    servers.insert("websearch".to_string(), sc.clone());
    let cfg = McpConfig {
        enabled: true,
        mcp_servers: servers,
    };

    let mgr = McpManager::load_and_probe(cfg, dir.path()).await.unwrap();

    // probe 通过 HTTP 发现了 SearchPro,并注册为限定名。
    let names: Vec<String> = mgr.enabled_tools().iter().map(|t| t.name().to_string()).collect();
    assert!(
        names.contains(&"mcp__websearch__SearchPro".to_string()),
        "expected SearchPro to be discovered via http; got {names:?}"
    );

    // 调用走懒连接 + HTTP 往返。
    let res = mgr
        .call_tool("websearch", "SearchPro", json!({ "Query": "hello" }))
        .await;
    assert!(!res.is_error, "call should succeed: {res:?}");
    let text = match &res.content[0] {
        yi_agent_core::message::ContentBlock::Text(t) => t.clone(),
        other => panic!("expected text block, got {other:?}"),
    };
    assert_eq!(text, "mock-search-result");

    mgr.shutdown().await;
}
```

> `yi_agent_core` 在 dev 上下文可见(它是本 crate 的普通依赖,integration test 可直接 `use yi_agent_core::...`)。

- [ ] **Step 2: 运行集成测试确认通过**

Run: `cd <worktree>/yi-agent-rs && cargo test -p yi-agent-mcp --test http_transport 2>&1 | tail -40`
Expected: PASS（`probes_and_calls_over_streamable_http` 绿）。

若失败并报 SSE/内容类型相关错误：给 POST mock 的响应补 `insert_header("content-type", "application/json")`（本 mock 已设），并**不要**返回 `text/event-stream` —— 端点实测回 JSON。

- [ ] **Step 3: 提交**

```bash
cd <worktree>/yi-agent-rs && cargo fmt --all
cd <worktree> && git add yi-agent-rs/crates/yi-agent-mcp/tests/http_transport.rs
git commit -m "test(mcp): streamable http round-trip via wiremock"
```

---

## Task 5: 真实端点冒烟测试（`#[ignore]` gate）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-mcp/tests/http_transport.rs`（追加一个 `#[ignore]` 测试）

**Interfaces:**
- Consumes: 同 Task 4

- [ ] **Step 1: 追加 `#[ignore]` 测试** — 在 `tests/http_transport.rs` 末尾追加：

```rust
/// 真实 horizon-websearch 端点冒烟。默认跳过(CI 不跑);手动运行:
/// `cargo test -p yi-agent-mcp --test http_transport -- --ignored`
///
/// 无外网时该测试会失败,故用 `#[ignore]` gate 隔离。
#[tokio::test]
#[ignore]
async fn real_horizon_websearch_smoke() {
    const URL: &str = "https://llmapi.horizon.auto/mcp-servers/horizon-websearch/Mcp";

    let dir = tempfile::tempdir().unwrap();
    let mut servers = BTreeMap::new();
    servers.insert("horizon-websearch".to_string(), http_server(URL));
    let cfg = McpConfig {
        enabled: true,
        mcp_servers: servers,
    };

    let mgr = McpManager::load_and_probe(cfg, dir.path()).await.unwrap();
    let names: Vec<String> = mgr.enabled_tools().iter().map(|t| t.name().to_string()).collect();
    assert!(
        names.contains(&"mcp__horizon-websearch__SearchPro".to_string()),
        "expected real endpoint to expose SearchPro; got {names:?}"
    );

    let res = mgr
        .call_tool(
            "horizon-websearch",
            "SearchPro",
            json!({ "Query": "今天北京天气", "Cnt": "10" }),
        )
        .await;
    assert!(!res.is_error, "real SearchPro call should succeed: {res:?}");

    mgr.shutdown().await;
}
```

- [ ] **Step 2: 运行（手动，需外网）**

Run: `cd <worktree>/yi-agent-rs && cargo test -p yi-agent-mcp --test http_transport -- --ignored 2>&1 | tail -40`
Expected: PASS。默认 `cargo test` 会自动跳过它。

- [ ] **Step 3: 提交**

```bash
cd <worktree>/yi-agent-rs && cargo fmt --all
cd <worktree> && git add yi-agent-rs/crates/yi-agent-mcp/tests/http_transport.rs
git commit -m "test(mcp): ignored smoke against the real horizon websearch endpoint"
```

---

## Task 6: 落地配置 + 更新模块文档

**Files:**
- Modify: `docs/project-management/yi-agent-mcp.md`
- Local (gitignored, 不提交): `<worktree>/.yi-agent/mcp.json`

**Interfaces:** 无代码接口。

- [ ] **Step 1: 写本地配置** — 创建 `<worktree>/.yi-agent/mcp.json`（`.yi-agent/` 已被 `.gitignore` 忽略，仅本地生效）：

```json
{
  "enabled": true,
  "mcpServers": {
    "horizon-websearch": {
      "type": "http",
      "url": "https://llmapi.horizon.auto/mcp-servers/horizon-websearch/Mcp"
    }
  }
}
```

- [ ] **Step 2: 验证配置被解析并注册工具**（无需真实网络，用 Task 4 的 mock 已覆盖链路；此处只确认文件形状可解析）

Run: `cd <worktree>/yi-agent-rs && cargo test -p yi-agent-mcp --lib config::http_config_parses_with_url_and_headers 2>&1 | tail -10`
Expected: PASS。

- [ ] **Step 3: 更新模块文档** — 在 `docs/project-management/yi-agent-mcp.md` 的 "Features" 小节，`MCP client 与远端 Tool 适配` 项下追加子条目：

```markdown
- [x] Streamable HTTP transport（仅 POST + JSON）
  - 配置：`ServerConfig` 增加 `type`（`"stdio"` 默认 / `"http"`）、`url`、`headers`；`transport()` 集中校验
  - 展开：http header 值支持 `${ENV_VAR}`，未定义变量丢弃该 header 并 warn；缓存指纹以未展开原文参与
  - 接线：`manager.rs` 的 `connect()` 按 `Transport` 分派 `TokioChildProcess` / `StreamableHttpClientTransport`
  - 指纹：`cache.rs` 缓存指纹扩展为覆盖 `kind`/`url`/`headers`（排除 `enabled`）
  - 验证：`cargo test -p yi-agent-mcp --test http_transport`（wiremock 往返）+ `-- --ignored`（真实 horizon 端点）
```

- [ ] **Step 4: 提交文档**

```bash
cd <worktree> && git add docs/project-management/yi-agent-mcp.md
git commit -m "docs(mcp): record streamable http transport"
```

---

## 收尾（整个计划完成后）

- [ ] 在 `<worktree>/yi-agent-rs` 跑 `cargo fmt --all -- --check` 与 `cargo test -p yi-agent-mcp`，全绿。
- [ ] 回 `main` 执行 `git merge --no-ff feat/mcp-http-transport`；合并后删分支 + `git worktree remove .worktrees/feat-mcp-http-transport`。
- [ ] 合并后，在 `main` 的项目根写一份同样的 `.yi-agent/mcp.json`（本地文件，不提交），使 yi-agent 实际启用该 MCP。

## Self-Review 记录

- **Spec 覆盖**：§3 schema → Task 1；§3.4 env 展开 → Task 1；§3.5 enabled 不变 → Task 2 指纹排除 enabled 测试 + 复用既有开关；§4 指纹 → Task 2；§5 接线 → Task 3；§5.1 依赖 → Task 3 Step 1；§6 落地配置 → Task 6；§7.1 单元 → Task 1/2；§7.2 集成 → Task 4；§7.3 冒烟 → Task 5；§8 流程 → 收尾。全部有对应任务。
- **类型一致性**：`ServerKind`/`Transport`/`transport()`/`expand_env`/`expand_headers`/`fingerprint` 在后续任务中的签名与本任务定义一致；测试 helper `http_server`/`cfg`/`server` 命名统一。
- **无占位符**：每个代码步骤均含完整可编译代码与确切命令。
