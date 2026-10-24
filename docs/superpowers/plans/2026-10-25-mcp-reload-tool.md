# MCP 配置自助管理 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让用户在对话里说一句「帮我加/关/删一个 MCP server」，模型能自己改配置、探测、并在下一轮生效；同时给桌面设置页补上与「开」对称的「关」。

**Architecture:** 在 `yi-agent-mcp` 里给 `McpManager` 加可重载能力（`servers` 改为可替换的 `Arc<HashMap<..>>` 快照），新增一个 `action` 多动作的 `mcp_reload` 工具负责校验与写盘；由于工具表是不可变快照（`ToolRegistry` 内部是 `BTreeMap`），生效点在回合之间，由 TUI 与桌面 driver 在每轮开跑前做一次廉价的配置指纹比对触发重建。

**Tech Stack:** Rust 2024、`tokio`、`rmcp 3.5`、`serde_json`、React + TypeScript（桌面设置页）、vitest。

**Design doc:** `docs/superpowers/specs/2026-10-25-mcp-reload-tool-design.md`

## Global Constraints

- 分支：所有改动在 `.worktrees/mcp-reload-tool`（分支 `feat/mcp-reload-tool`）内完成，禁止直接改 `main`。
- 测试命令一律按 crate 跑，禁止 `cargo test --workspace`：`cd yi-agent-rs && cargo test -p <crate>`。
- 每个 commit 前必须 `cd yi-agent-rs && cargo fmt --all`。
- commit message 用 conventional commits，**不写** `Co-Authored-By`。
- 工具名固定为 `mcp_reload`；server 名规则固定为 `^[a-z0-9]([a-z0-9-]*[a-z0-9])?$` 且 <=64 字节。
- 返回文案必须包含「下一条消息起可用」语义，禁止声称当轮生效。
- 探测失败不得静默：逐 server 收集 `Result` 并原文回报。
- 四个 action（`add`/`enable`/`disable`/`remove`）全部需要权限确认；**不提供**全局开关。

---

## File Structure

**创建**
- `yi-agent-rs/crates/yi-agent-mcp/src/reload.rs` — `mcp_reload` 工具的校验、合并、写盘逻辑（纯函数为主，便于单测）
- `yi-agent-rs/crates/yi-agent-mcp/src/reload_tool.rs` — `McpReloadTool` 的 `Tool` 实现（薄壳：解析入参 → 调 `reload.rs` → 格式化结果）
- `yi-agent-rs/crates/yi-agent-skills/src/assets/mcp-setup/SKILL.md` — 内置 skill
- `yi-agent-rs/crates/yi-agent-mcp/tests/reload_flow.rs` — 重载端到端（用 `TEST_CONNECTOR` 缝）

**修改**
- `yi-agent-mcp/src/manager.rs` — `servers` 变为可替换快照；新增 `reload()`、`config_fingerprint()`、`last_registered` 清理
- `yi-agent-mcp/src/config.rs` — 新增 `save()`（临时文件 + rename）
- `yi-agent-mcp/src/lib.rs` — 导出 `mcp_reload` 注册函数；无条件注册
- `yi-agent-core/src/permission.rs` — `mcp_reload` 始终 `NeedConfirm`
- `yi-agent-runtime/src/bootstrap.rs` — 装配处注册重载工具
- `yi-agent/src/main.rs` — 每轮开跑前指纹比对；`/mcp reload`
- `yi-agent/src/tui/slash.rs` — 解析 `reload`
- `yi-agent-app-server/src/mcp_rpc.rs` / `rpc/mcp.rs` — `mcp/set`
- `yi-agent-app-server/src/server.rs` — 每轮开跑前指纹比对 + 重建
- `desktop/src/components/SettingsMcpTab.tsx` — 开关
- `docs/project-management/*.md`、`README.md` — 文档同步

---

### Task 1: `McpConfig::save` 原子写盘

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/config.rs:145-175`
- Test: 同文件 `mod tests`

**Interfaces:**
- Produces: `McpConfig::save(&self, workdir: &Path) -> Result<()>`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn save_creates_agent_dir_and_round_trips() {
    let d = tempfile::TempDir::new().unwrap();
    let cfg = McpConfig {
        enabled: true,
        mcp_servers: BTreeMap::from([(
            "fs".to_string(),
            ServerConfig {
                kind: ServerKind::Stdio,
                command: Some("npx".into()),
                args: vec!["-y".into(), "pkg".into()],
                env: Default::default(),
                url: None,
                headers: Default::default(),
                enabled: true,
            },
        )]),
    };
    cfg.save(d.path()).unwrap();
    assert!(config_path(d.path()).is_file());
    let back = McpConfig::load(d.path()).unwrap().unwrap();
    assert_eq!(back.mcp_servers["fs"].args, vec!["-y", "pkg"]);
}

#[test]
fn save_leaves_no_temp_file_behind() {
    let d = tempfile::TempDir::new().unwrap();
    McpConfig::default().save(d.path()).unwrap();
    let leftovers: Vec<_> = std::fs::read_dir(d.path().join(".yi-agent"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.contains("tmp"))
        .collect();
    assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
}
```

注意：`McpConfig` 目前没有 `Default`，测试里用 `McpConfig { enabled: true, mcp_servers: BTreeMap::new() }` 构造；如需要，在 `config.rs` 给它加 `#[derive(Default)]`（`enabled` 默认为 false 不影响，因为 `save` 不读默认值）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp --lib config::tests::save_`
Expected: FAIL — `no method named 'save'`

- [ ] **Step 3: 实现**

在 `impl McpConfig` 内、`load` 之后加：

```rust
    /// 原子写入 `workdir` 下的 `.yi-agent/mcp.json`。
    ///
    /// 先写同目录临时文件再 `rename`：避免另一轮 driver 读到半写状态。
    /// 目录不存在时创建。
    pub fn save(&self, workdir: &Path) -> Result<()> {
        let path = config_path(workdir);
        let dir = path
            .parent()
            .ok_or_else(|| anyhow!("mcp.json has no parent directory"))?;
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create {}", dir.display()))?;
        // 缩进 2 空格 + 末尾换行：这份文件用户也会手改、进 code review。
        let mut body = serde_json::to_string_pretty(self)?;
        body.push('\n');
        let tmp = dir.join(format!(".mcp.json.tmp.{}", std::process::id()));
        std::fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename into {}", path.display()))?;
        Ok(())
    }
```

若 `rename` 失败，删掉临时文件再返回错误（避免残留）：

```rust
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("rename into {}", path.display()));
        }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp --lib config::tests`
Expected: PASS（全部 config 测试）

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-mcp/src/config.rs
git commit -m "feat(mcp): 原子写盘 McpConfig::save"
```

---

### Task 2: `McpManager` 改用可替换快照

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/manager.rs:50-360`
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: 无（首个改动 manager 的任务）
- Produces:
  - `McpManager { servers: RwLock<Arc<HashMap<String, Arc<ServerEntry>>>>, master_enabled: AtomicBool, last_registered: Mutex<HashSet<String>> }`
  - 既有公开方法签名**不变**：`is_effective`、`set_master`、`master`、`set_server`、`server_names`、`status`、`all_tool_names`、`enabled_tools`、`refresh_registry`、`call_tool`、`shutdown`

**关键理由（务必按此实现）**：读路径必须**先 clone 出 `Arc<HashMap<..>>` 再释放锁**，绝不跨 `.await` 持 `std::sync` 锁（否则 future 不是 `Send`，`call_tool` 编不过）。

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn refresh_registry_removes_tools_of_servers_dropped_from_the_snapshot() {
        let m = manager_with_fs_read();
        let mut registry = ToolRegistry::new();
        m.refresh_registry(&mut registry);
        assert!(registry.get("mcp__fs__read").is_some());

        // 模拟 reload 把 fs 整条删掉：新快照里没有它。
        m.replace_servers_for_test(HashMap::new());
        m.refresh_registry(&mut registry);

        assert!(
            registry.get("mcp__fs__read").is_none(),
            "tool of a removed server must be unregistered; got {:?}",
            registry.names()
        );
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp --lib refresh_registry_removes_tools_of_servers_dropped`
Expected: FAIL — `no method named 'replace_servers_for_test'`

- [ ] **Step 3: 改结构体与所有读点**

结构体：

```rust
pub struct McpManager {
    /// 配置快照。读路径 clone 出 `Arc` 后立刻释放锁，绝不跨 `.await` 持锁。
    servers: std::sync::RwLock<Arc<HashMap<String, Arc<ServerEntry>>>>,
    master_enabled: AtomicBool,
    /// 上一次 `refresh_registry` 注册过的工具名。用于在 server 被删除后
    /// 把孤儿工具摘掉——`servers` 快照里已经查不到这些名字了。
    last_registered: Mutex<HashSet<String>>,
    /// 上一次 reload / 启动探测时读到的配置指纹；用于每轮开跑前的廉价变更检测。
    last_seen: Mutex<Option<String>>,
}
```

`ServerEntry` 的字段全部保持原样（`enabled: AtomicBool`、`client: Mutex<Option<..>>` 仍可共享），但整体包 `Arc`。读点改造示例：

```rust
    fn snapshot(&self) -> Arc<HashMap<String, Arc<ServerEntry>>> {
        Arc::clone(&self.servers.read().unwrap_or_else(|e| e.into_inner()))
    }

    fn entry(&self, server: &str) -> Option<Arc<ServerEntry>> {
        self.snapshot().get(server).cloned()
    }

    pub fn is_effective(&self, server: &str) -> bool {
        self.master_enabled.load(Ordering::Relaxed)
            && self
                .entry(server)
                .map(|e| e.enabled.load(Ordering::Relaxed))
                .unwrap_or(false)
    }

    pub fn set_server(&self, server: &str, on: bool) -> Result<()> {
        let entry = self
            .entry(server)
            .ok_or_else(|| anyhow!("unknown MCP server: {server}"))?;
        entry.enabled.store(on, Ordering::Relaxed);
        Ok(())
    }
```

`call_tool` 内改为先 `let Some(entry) = self.entry(server) else { ... }`，后续 `acquire_client(server, &entry).await` 传引用。`acquire_client` 与 `drop_client_if_same` 的形参从 `&ServerEntry` 改为 `&Arc<ServerEntry>`（或 `&ServerEntry`，解引用即可，不必改签名）。

`refresh_registry` 改为：

```rust
    pub fn refresh_registry(self: &Arc<Self>, registry: &mut ToolRegistry) {
        let mut registered = self.last_registered.lock().unwrap_or_else(|e| e.into_inner());
        for name in registered.drain() {
            registry.remove(&name);
        }
        for tool in self.enabled_tools() {
            registered.insert(tool.name().to_string());
            registry.register(tool);
        }
    }
```

`all_tool_names` 保留（仍用于展示/诊断），但**不再**是 `refresh_registry` 的清理依据。

测试缝改造：

```rust
    #[cfg(test)]
    pub(crate) fn replace_servers_for_test(&self, servers: HashMap<String, Arc<ServerEntry>>) {
        *self.servers.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(servers);
    }
```

`new_for_test` / `insert_server_*_for_test` 改为构造 `HashMap` 后写入 `RwLock<Arc<..>>`。

- [ ] **Step 4: 跑全部 mcp 测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp`
Expected: PASS（含既有 `call_tool`、`shutdown`、`refresh_registry_*` 全部用例）

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-mcp/src/manager.rs
git commit -m "refactor(mcp): servers 改为可替换的快照并清理孤儿工具"
```

---

### Task 3: `McpManager::reload` 只探变化项

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/manager.rs`
- Test: 同文件 `mod tests` + `tests/reload_flow.rs`

**Interfaces:**
- Consumes: Task 2 的 `snapshot()`、`replace_servers_for_test`
- Produces:
  - `pub struct ReloadReport { pub servers: Vec<ServerOutcome> }`
  - `pub struct ServerOutcome { pub name: String, pub added: bool, pub removed: bool, pub tool_count: usize, pub read_only_count: usize, pub error: Option<String>, pub probed: bool }`
  - `pub async fn reload(&self, workdir: &Path) -> ReloadReport`
  - `pub fn config_fingerprint(&self, workdir: &Path) -> Option<String>`
  - `pub fn last_seen_fingerprint(&self) -> Option<String>`
  - `McpManager` 新增字段 `last_seen: Mutex<Option<String>>`；`reload()` 末尾写入本次
    读到的指纹，`load_and_probe` 末尾同样写入一次（否则首次启动后的第一轮会把
    「和初值不同」误判成配置变了，白跑一次 reload）。

- [ ] **Step 1: 写失败测试（只探变化项）**

在 `tests/reload_flow.rs`（新建）里用 `TEST_CONNECTOR` 缝计次：

```rust
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[tokio::test]
async fn reload_does_not_reprobe_an_unchanged_server() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
    std::fs::write(
        dir.path().join(".yi-agent/mcp.json"),
        r#"{"mcpServers":{"fs":{"command":"npx","args":["-y","pkg"]}}}"#,
    )
    .unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    yi_agent_mcp::test_support::install_counting_connector(Arc::clone(&calls));

    let mgr = yi_agent_mcp::test_support::reload_from(dir.path()).await;
    let first = calls.load(Ordering::SeqCst);
    assert!(first >= 1, "first load must probe");

    // 配置没变，再 reload 一次
    let report = mgr.reload(dir.path()).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        first,
        "unchanged config must not re-probe; got {} -> {}",
        first,
        calls.load(Ordering::SeqCst)
    );
    assert!(report.servers.iter().all(|s| !s.probed));
}
```

在 `lib.rs` 的 `test_support` 模块补两个辅助（`#[cfg(test)]` 之外也需要的缝，放在 `pub mod test_support` 内、`#[doc(hidden)]`）：

```rust
    /// 安装一个只计次、不出工具的连接器：用来断言"没有再探测"。
    pub fn install_counting_connector(calls: Arc<std::sync::atomic::AtomicUsize>) {
        // 具体实现：TEST_CONNECTOR 存一个闭包，内部 calls.fetch_add(1) 后
        // 返回 Err(anyhow!("counting connector: no real server"))
        // 探测失败会被记录进 report，正好不干扰"次数"断言。
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp --test reload_flow`
Expected: FAIL — `reload` / `test_support::install_counting_connector` 不存在

- [ ] **Step 3: 实现 `reload`**

```rust
/// 一次 reload 里单个 server 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerOutcome {
    pub name: String,
    /// 本次 reload 前不存在、现在存在。
    pub added: bool,
    /// 之前存在、现在被删除。
    pub removed: bool,
    /// 本次是否真的连上去探测过。
    pub probed: bool,
    pub tool_count: usize,
    pub read_only_count: usize,
    /// 探测失败原因原文；`None` 表示成功（或未探测）。
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReloadReport {
    pub servers: Vec<ServerOutcome>,
}

impl ReloadReport {
    /// 任一 server 探测失败即为 `false`。写盘成功与探测成功是两件事，由调用方分别呈现。
    pub fn all_ok(&self) -> bool {
        self.servers.iter().all(|s| s.error.is_none())
    }
}
```

`reload` 主体：

```rust
    pub async fn reload(&self, workdir: &Path) -> ReloadReport {
        let cfg = match McpConfig::load(workdir) {
            Ok(Some(c)) => c,
            Ok(None) => McpConfig { enabled: true, mcp_servers: Default::default() },
            Err(e) => {
                return ReloadReport {
                    servers: vec![ServerOutcome {
                        name: "<config>".into(),
                        added: false,
                        removed: false,
                        probed: false,
                        tool_count: 0,
                        read_only_count: 0,
                        error: Some(e.to_string()),
                    }],
                }
            }
        };

        let old = self.snapshot();
        let mut cache = McpCache::load(workdir);
        let mut next: HashMap<String, Arc<ServerEntry>> = HashMap::new();
        let mut out = Vec::new();

        for (name, sc) in &cfg.mcp_servers {
            let previous = old.get(name);
            // 命中旧快照且配置指纹未变 ⇒ 沿用工具清单，不重探。
            let reusable = previous.filter(|e| e.config == *sc);
            let (tools, probed, error) = match reusable {
                Some(e) => (e.tools.clone(), false, None),
                None => match cache.get_for(name, sc) {
                    Some(cached) => (cached.to_vec(), false, None),
                    None => match probe_tools(sc).await {
                        Ok(probed_tools) => {
                            cache.put(name, sc, probed_tools.clone());
                            (probed_tools, true, None)
                        }
                        Err(e) => (Vec::new(), true, Some(e.to_string())),
                    },
                },
            };
            let enabled = previous.map(|e| e.enabled.load(Ordering::Relaxed)).unwrap_or(sc.enabled);
            let read_only_count = tools.iter().filter(|t| t.read_only).count();
            out.push(ServerOutcome {
                name: name.clone(),
                added: previous.is_none(),
                removed: false,
                probed,
                tool_count: tools.len(),
                read_only_count,
                error,
            });
            next.insert(
                name.clone(),
                Arc::new(ServerEntry {
                    config: sc.clone(),
                    enabled: AtomicBool::new(enabled),
                    tools,
                    client: Mutex::new(None),
                }),
            );
        }

        for name in old.keys() {
            if !cfg.mcp_servers.contains_key(name) {
                out.push(ServerOutcome {
                    name: name.clone(),
                    added: false,
                    removed: true,
                    probed: false,
                    tool_count: 0,
                    read_only_count: 0,
                    error: None,
                });
            }
        }

        if let Err(e) = cache.save(workdir) {
            warn!(error = %e, "failed to persist MCP cache after reload");
        }
        self.master_enabled.store(cfg.enabled, Ordering::Relaxed);
        *self.servers.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(next);
        if let Some(fp) = self.config_fingerprint(workdir) {
            *self.last_seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(fp);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        ReloadReport { servers: out }
    }

    /// 上一次 `reload` / `load_and_probe` 时读到的配置指纹。
    pub fn last_seen_fingerprint(&self) -> Option<String> {
        self.last_seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 当前磁盘配置的指纹；文件缺失或坏 JSON 时返回 `None`（调用方据此决定是否 reload）。
    pub fn config_fingerprint(&self, workdir: &Path) -> Option<String> {
        let text = std::fs::read_to_string(crate::config::config_path(workdir)).ok()?;
        let cfg: McpConfig = serde_json::from_str(&text).ok()?;
        Some(crate::cache::fingerprint_for_manager(&cfg))
    }
```

`cache::fingerprint` 目前是私有函数，新增一个 pub(crate) 包装 `fingerprint_for_manager(&McpConfig)`，对整个配置（含 `enabled`、按 server 名排序）算一个 SHA-256。

注意：`ServerEntry` 没有 `Clone`，`reusable` 分支用 `e.tools.clone()` 即可，不必克隆整个 entry。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-mcp/src
git commit -m "feat(mcp): McpManager::reload 只探测新增或变化的 server"
```

---

### Task 4: 校验与合并（纯函数）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-mcp/src/reload.rs`
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/lib.rs`（`mod reload;`）
- Test: `reload.rs` 内 `mod tests`

**Interfaces:**
- Produces:
  - `pub enum ReloadAction { Add, Enable, Disable, Remove }`（`FromStr` 接受 `add`/`enable`/`disable`/`remove`）
  - `pub struct ReloadRequest { pub action: ReloadAction, pub name: String, pub spec: Option<ServerConfig>, pub overwrite: bool }`
  - `pub fn apply(cfg: &mut McpConfig, req: &ReloadRequest) -> Result<(), String>`
  - `pub fn validate_name(name: &str) -> Result<(), String>`
  - `pub fn validate_spec(sc: &ServerConfig) -> Result<(), String>`

- [ ] **Step 1: 写失败测试**

```rust
    fn stdio(cmd: &str) -> ServerConfig {
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

    #[test]
    fn rejects_invalid_names() {
        for bad in ["GitHub", "-gh", "gh-", "gh--x", "", "a".repeat(65).as_str()] {
            assert!(validate_name(bad).is_err(), "{bad:?} must be rejected");
        }
        for good in ["gh", "github-mcp", "a1"] {
            assert!(validate_name(good).is_ok(), "{good:?} must be accepted");
        }
    }

    #[test]
    fn rejects_sse_kind() {
        let raw = r#"{"type":"sse","url":"https://x/mcp"}"#;
        let sc: ServerConfig = serde_json::from_str(raw).unwrap_err();
        // 若 serde 允许了，则 transport() 必须拒绝；两条路径都要覆盖。
        let _ = sc;
    }

    #[test]
    fn rejects_undefined_env_reference() {
        let mut sc = stdio("npx");
        sc.env.insert("TOKEN".into(), "${MCP_TEST_UNSET_VAR}".into());
        let err = validate_spec(&sc).unwrap_err();
        assert!(err.contains("MCP_TEST_UNSET_VAR"), "got {err}");
    }

    #[test]
    fn add_refuses_existing_name_without_overwrite() {
        let mut cfg = McpConfig { enabled: true, mcp_servers: Default::default() };
        cfg.mcp_servers.insert("gh".into(), stdio("npx"));
        let req = ReloadRequest {
            action: ReloadAction::Add,
            name: "gh".into(),
            spec: Some(stdio("uvx")),
            overwrite: false,
        };
        let err = apply(&mut cfg, &req).unwrap_err();
        assert!(err.contains("已存在"), "got {err}");
    }

    #[test]
    fn disable_sets_flag_without_touching_other_fields() {
        let mut cfg = McpConfig { enabled: true, mcp_servers: Default::default() };
        let mut sc = stdio("npx");
        sc.args = vec!["-y".into(), "pkg".into()];
        cfg.mcp_servers.insert("gh".into(), sc);
        let req = ReloadRequest {
            action: ReloadAction::Disable,
            name: "gh".into(),
            spec: None,
            overwrite: false,
        };
        apply(&mut cfg, &req).unwrap();
        let got = &cfg.mcp_servers["gh"];
        assert!(!got.enabled);
        assert_eq!(got.args, vec!["-y", "pkg"]);
    }

    #[test]
    fn remove_deletes_the_entry() {
        let mut cfg = McpConfig { enabled: true, mcp_servers: Default::default() };
        cfg.mcp_servers.insert("gh".into(), stdio("npx"));
        let req = ReloadRequest {
            action: ReloadAction::Remove,
            name: "gh".into(),
            spec: None,
            overwrite: false,
        };
        apply(&mut cfg, &req).unwrap();
        assert!(cfg.mcp_servers.is_empty());
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp --lib reload::tests`
Expected: FAIL — 模块不存在

- [ ] **Step 3: 实现**

```rust
//! `mcp_reload` 的纯逻辑：校验入参、把一次动作合并进 `McpConfig`。
//!
//! 与 IO 分离：本模块不读盘、不探测，只操作内存中的配置，便于逐条单测。

use std::collections::BTreeMap;

use crate::config::{McpConfig, ServerConfig, Transport};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadAction {
    Add,
    Enable,
    Disable,
    Remove,
}

impl std::str::FromStr for ReloadAction {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "add" => Ok(Self::Add),
            "enable" => Ok(Self::Enable),
            "disable" => Ok(Self::Disable),
            "remove" => Ok(Self::Remove),
            other => Err(format!(
                "不支持的 action: {other}；只支持 add / enable / disable / remove"
            )),
        }
    }
}

pub struct ReloadRequest {
    pub action: ReloadAction,
    pub name: String,
    pub spec: Option<ServerConfig>,
    pub overwrite: bool,
}

const MAX_NAME_LEN: usize = 64;

pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return Err(format!("name 长度必须在 1..={MAX_NAME_LEN} 字节之间"));
    }
    let b = name.as_bytes();
    let ok_char = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-';
    if !b.iter().all(|c| ok_char(*c)) {
        return Err("name 只能用小写字母、数字与连字符".into());
    }
    if b[0] == b'-' || b[b.len() - 1] == b'-' || name.contains("--") {
        return Err("name 不能以连字符开头/结尾，也不能有连续连字符".into());
    }
    Ok(())
}

pub fn validate_spec(sc: &ServerConfig) -> Result<(), String> {
    // 传输必填项
    sc.transport().map_err(|e| e.to_string())?;
    // http header 名合法性 + 变量已定义
    for (k, v) in &sc.headers {
        if k.is_empty() || !k.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
            return Err(format!("header 名不合法: {k:?}（只允许字母/数字/连字符）"));
        }
        check_env_refs(&format!("headers.{k}"), v)?;
    }
    for (k, v) in &sc.env {
        check_env_refs(&format!("env.{k}"), v)?;
    }
    if let Transport::Http { .. } = sc.transport().map_err(|e| e.to_string())? {
        // 已在 transport() 校验 url 非空；这里补协议白名单。
        let url = sc.url.as_deref().unwrap_or_default();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err("http 需要以 http:// 或 https:// 开头的 url".into());
        }
    }
    Ok(())
}

/// `${VAR}` 引用必须已定义。未定义直接拒绝：运行时 `expand_headers` 会静默丢弃
/// 该 header（只 warn），用户最终面对 401 却查不到原因。
fn check_env_refs(field: &str, value: &str) -> Result<(), String> {
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else { break };
        let name = &after[..end];
        if std::env::var(name).is_err() {
            return Err(format!("{field} 引用的环境变量 {name} 未定义"));
        }
        rest = &after[end + 1..];
    }
    Ok(())
}

pub fn apply(cfg: &mut McpConfig, req: &ReloadRequest) -> Result<(), String> {
    validate_name(&req.name)?;
    match req.action {
        ReloadAction::Add => {
            let spec = req
                .spec
                .as_ref()
                .ok_or_else(|| "add 需要提供 server 配置".to_string())?;
            validate_spec(spec)?;
            if cfg.mcp_servers.contains_key(&req.name) && !req.overwrite {
                return Err(format!(
                    "server {:?} 已存在；要覆盖请传 overwrite: true",
                    req.name
                ));
            }
            cfg.mcp_servers.insert(req.name.clone(), spec.clone());
            Ok(())
        }
        ReloadAction::Enable | ReloadAction::Disable => {
            let on = req.action == ReloadAction::Enable;
            let entry = cfg
                .mcp_servers
                .get_mut(&req.name)
                .ok_or_else(|| format!("没有名为 {:?} 的 server", req.name))?;
            entry.enabled = on;
            Ok(())
        }
        ReloadAction::Remove => {
            if cfg.mcp_servers.remove(&req.name).is_none() {
                return Err(format!("没有名为 {:?} 的 server", req.name));
            }
            Ok(())
        }
    }
}
```

`McpConfig` 需要 `Default` 或测试里手工构造；若加 `#[derive(Default)]`，注意 `enabled` 会变 `false`，测试里显式赋值即可。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp --lib reload::tests`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-mcp/src/reload.rs crates/yi-agent-mcp/src/lib.rs
git commit -m "feat(mcp): mcp_reload 的校验与合并逻辑"
```

---

### Task 5: `McpReloadTool` 工具实现 + 无条件注册

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-mcp/src/reload_tool.rs`
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/lib.rs`
- Test: `reload_tool.rs` 内 `mod tests`、`tests/reload_flow.rs`

**Interfaces:**
- Consumes: Task 3 `reload()`、Task 4 `apply()` / `ReloadRequest`
- Produces:
  - `pub struct McpReloadTool { manager: Arc<McpManager>, workdir: PathBuf, blocklist: Option<BlocklistFn> }`
  - `pub fn register_mcp_reload_tool(registry: &mut ToolRegistry, manager: Arc<McpManager>, workdir: &Path, blocklist: Option<BlocklistFn>)`
  - `pub type BlocklistFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;`（与 `yi-agent-core::permission::BlocklistFn` 同形，直接复用后者）

- [ ] **Step 1: 写失败测试**

```rust
    #[tokio::test]
    async fn reports_each_server_outcome_and_the_effect_timing() {
        // 用坏 command 制造一次失败的探测（不会真的起进程：进程起不来即报错）。
        let dir = tempfile::TempDir::new().unwrap();
        let mgr = yi_agent_mcp::test_support::reload_from(dir.path()).await;
        let tool = McpReloadTool::new(Arc::clone(&mgr), dir.path().to_path_buf(), None);

        let out = tool
            .call(serde_json::json!({
                "action": "add",
                "name": "broken",
                "command": "definitely-not-a-real-binary-xyz",
                "args": ["-y", "pkg"]
            }))
            .await;

        let text = match &out.content[0] {
            yi_agent_core::message::ContentBlock::Text(t) => t.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("下一条消息起可用"), "got {text}");
        assert!(text.contains("失败"), "probe failure must not be swallowed; got {text}");
        // 配置本身仍然写盘成功
        assert!(dir.path().join(".yi-agent/mcp.json").is_file());
    }

    #[tokio::test]
    async fn disable_is_fast_and_does_not_probe() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        std::fs::write(
            dir.path().join(".yi-agent/mcp.json"),
            r#"{"mcpServers":{"gh":{"command":"npx","enabled":true}}}"#,
        )
        .unwrap();
        let mgr = yi_agent_mcp::test_support::reload_from(dir.path()).await;
        let tool = McpReloadTool::new(Arc::clone(&mgr), dir.path().to_path_buf(), None);

        let out = tool
            .call(serde_json::json!({"action": "disable", "name": "gh"}))
            .await;
        assert!(!out.is_error, "disable must succeed");
        let saved = std::fs::read_to_string(dir.path().join(".yi-agent/mcp.json")).unwrap();
        assert!(saved.contains("\"enabled\": false"), "got {saved}");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp --lib reload_tool::tests`
Expected: FAIL — 模块不存在

- [ ] **Step 3: 实现工具**

```rust
//! `mcp_reload`：让模型自己改 MCP 配置。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use yi_agent_core::Tool;
use yi_agent_core::permission::BlocklistFn;
use yi_agent_core::tool::{ToolMetadata, ToolResult, ToolSource};

use crate::McpManager;
use crate::config::{McpConfig, ServerConfig, ServerKind};
use crate::reload::{ReloadRequest, apply};

pub struct McpReloadTool {
    manager: Arc<McpManager>,
    workdir: PathBuf,
    blocklist: Option<BlocklistFn>,
}

impl McpReloadTool {
    pub fn new(manager: Arc<McpManager>, workdir: PathBuf, blocklist: Option<BlocklistFn>) -> Self {
        Self { manager, workdir, blocklist }
    }

    /// 把入参拼成 `ServerConfig`；只对 `add` 有意义。
    fn spec_from_args(&self, args: &Value) -> Result<ServerConfig, String> {
        let kind = match args.get("kind").and_then(|v| v.as_str()).unwrap_or("stdio") {
            "stdio" => ServerKind::Stdio,
            "http" => ServerKind::Http,
            other => return Err(format!("不支持的 type: {other:?}；只支持 stdio / http")),
        };
        let str_map = |key: &str| -> BTreeMap<String, String> {
            args.get(key)
                .and_then(|v| v.as_object())
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default()
        };
        Ok(ServerConfig {
            kind,
            command: args.get("command").and_then(|v| v.as_str()).map(str::to_string),
            args: args
                .get("args")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
            env: str_map("env"),
            url: args.get("url").and_then(|v| v.as_str()).map(str::to_string),
            headers: str_map("headers"),
            enabled: args.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
        })
    }

    /// 危险命令拦截：`command` 走黑名单；含 `/` 时必须是存在的绝对路径。
    fn check_command(&self, sc: &ServerConfig) -> Result<(), String> {
        let Some(cmd) = sc.command.as_deref() else { return Ok(()) };
        if let Some(reason) = self.blocklist.as_ref().and_then(|f| f(cmd)) {
            return Err(format!("命令被黑名单拒绝: {reason}"));
        }
        if cmd.contains('/') {
            let p = Path::new(cmd);
            if !p.is_absolute() {
                return Err(format!("command 含路径时必须是绝对路径: {cmd}"));
            }
            if !p.is_file() {
                return Err(format!("command 指向的文件不存在: {cmd}"));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Tool for McpReloadTool {
    fn name(&self) -> &str {
        "mcp_reload"
    }

    fn description(&self) -> &str {
        "添加、启用、停用或删除一个 MCP server。改动写入 <workdir>/.yi-agent/mcp.json，\
         从下一条消息起生效（本轮不可调用新工具）。"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["add", "enable", "disable", "remove"] },
                "name": { "type": "string", "description": "server 名：小写字母/数字/连字符，<=64 字节" },
                "kind": { "type": "string", "enum": ["stdio", "http"], "description": "仅 add，默认 stdio" },
                "command": { "type": "string", "description": "仅 stdio add" },
                "args": { "type": "array", "items": { "type": "string" } },
                "env": { "type": "object", "additionalProperties": { "type": "string" } },
                "url": { "type": "string", "description": "仅 http add" },
                "headers": { "type": "object", "additionalProperties": { "type": "string" } },
                "overwrite": { "type": "boolean", "description": "同名已存在时是否覆盖，默认 false" }
            },
            "required": ["action", "name"]
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            source: ToolSource::Builtin,
            requires_confirmation: true,
            read_only: false,
            version: None,
        }
    }

    async fn call(&self, args: Value) -> ToolResult {
        let action = match args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "缺少 action".to_string())
            .and_then(str::parse::<ReloadAction>)
        {
            Ok(a) => a,
            Err(e) => return ToolResult::error(e),
        };
        let name = match args.get("name").and_then(|v| v.as_str()) {
            Some(n) => n.to_string(),
            None => return ToolResult::error("缺少 name"),
        };

        let mut cfg = match McpConfig::load(&self.workdir) {
            Ok(Some(c)) => c,
            Ok(None) => McpConfig { enabled: true, mcp_servers: Default::default() },
            Err(e) => return ToolResult::error(format!("读取现有配置失败: {e:#}")),
        };

        let spec = if action == ReloadAction::Add {
            match self.spec_from_args(&args) {
                Ok(s) => {
                    if let Err(e) = self.check_command(&s) {
                        return ToolResult::error(e);
                    }
                    Some(s)
                }
                Err(e) => return ToolResult::error(e),
            }
        } else {
            None
        };

        let req = ReloadRequest {
            action,
            name: name.clone(),
            spec,
            overwrite: args.get("overwrite").and_then(|v| v.as_bool()).unwrap_or(false),
        };
        if let Err(e) = apply(&mut cfg, &req) {
            return ToolResult::error(e);
        }
        if let Err(e) = cfg.save(&self.workdir) {
            return ToolResult::error(format!("写入 mcp.json 失败: {e:#}"));
        }

        // disable / remove 无需探测；add / enable 交给 reload（缓存命中则不会重探）。
        let report = match action {
            ReloadAction::Disable | ReloadAction::Remove => None,
            _ => Some(self.manager.reload(&self.workdir).await),
        };

        ToolResult::text(render(&action, &name, &self.workdir, report.as_ref()))
    }
}

/// 渲染给模型的回执。**必须**说明生效时机（见设计约束 3.2）。
fn render(
    action: &ReloadAction,
    name: &str,
    workdir: &Path,
    report: Option<&crate::manager::ReloadReport>,
) -> String {
    let mut out = String::new();
    let verb = match action {
        ReloadAction::Add => "已添加",
        ReloadAction::Enable => "已启用",
        ReloadAction::Disable => "已停用",
        ReloadAction::Remove => "已删除",
    };
    out.push_str(&format!("{verb} MCP server {name:?}。配置已保存到 {}\n", workdir.join(".yi-agent/mcp.json").display()));
    if let Some(r) = report {
        for s in &r.servers {
            if let Some(err) = &s.error {
                out.push_str(&format!("  {}  失败：{}\n", s.name, err));
            } else if s.tool_count > 0 || s.added {
                out.push_str(&format!(
                    "  {}  ✓ {} 个工具（{} 只读）{}\n",
                    s.name,
                    s.tool_count,
                    s.read_only_count,
                    if s.probed { "" } else { "（缓存命中，未重新探测）" }
                ));
            }
        }
    }
    out.push_str("新工具从下一条消息起可用；本轮不要尝试调用它们。");
    out
}
```

`lib.rs` 增加无条件注册：

```rust
/// 注册 `mcp_reload`。**无条件**注册：首次添加 MCP 时 `mcp.json` 尚不存在，
/// 若沿用 `register_mcp_tools` 的「无配置即不注册」语义，模型就永远没有入口。
pub fn register_mcp_reload_tool(
    registry: &mut ToolRegistry,
    manager: Arc<McpManager>,
    workdir: &Path,
    blocklist: Option<BlocklistFn>,
) {
    registry.register(Arc::new(reload_tool::McpReloadTool::new(
        manager,
        workdir.to_path_buf(),
        blocklist,
    )));
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-mcp/src
git commit -m "feat(mcp): mcp_reload 工具与无条件注册"
```

---

### Task 6: 权限接线（始终确认）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/permission.rs:130-160`
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: 无
- Produces: `check("mcp_reload", ..)` 恒为 `CheckResult::NeedConfirm`，`prefix_suggestion` 恒为 `None`

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn check_mcp_reload_always_needs_confirmation_even_under_yolo() {
        let blocklist: BlocklistFn = Arc::new(|_| None);
        for yolo in [true, false] {
            let checker = PermissionChecker::new(
                PermissionsConfig::default(),
                crate::autonomy::YoloSwitch::new(yolo),
                std::path::PathBuf::from("/tmp"),
                Arc::clone(&blocklist),
            );
            match checker.check("mcp_reload", &serde_json::json!({"action": "disable", "name": "gh"})) {
                CheckResult::NeedConfirm(req) => {
                    assert_eq!(req.tool_name, "mcp_reload");
                    assert!(
                        req.prefix_suggestion.is_none(),
                        "mcp_reload must not offer an always-allow prefix"
                    );
                }
                other => panic!("yolo={yolo}: expected NeedConfirm, got {other:?}"),
            }
        }
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib check_mcp_reload_always`
Expected: FAIL — 拿到 `Allow`

- [ ] **Step 3: 实现**

在 `check()` 的受控工具判断之后、`yolo.get()` **之前**插入：

```rust
        // `mcp_reload` 会写入「下次启动自动执行」的配置，且能启动任意子进程。
        // 因此始终确认：不受 yolo 豁免，也不提供 AlwaysAllowPrefix（免得变成
        // 永久静默口子）。四个 action 一视同仁，见设计 §4.6。
        if tool_name == "mcp_reload" {
            return CheckResult::NeedConfirm(self.build_request(
                tool_name,
                tool_input,
                PermissionKind::Normal,
            ));
        }
```

同时把 `"mcp_reload"` 加进前面的 `matches!` 列表（纯粹为了可读性对称；逻辑上已由上面的分支兜住）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib permission`
Expected: PASS（含既有 20+ 个 permission 用例）

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src/permission.rs
git commit -m "feat(core): mcp_reload 始终需要权限确认"
```

---

### Task 7: TUI 接线（每轮指纹比对 + `/mcp reload`）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:2353`、`:2075`（`McpRefresh` 分支）、`:1295`（注册）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs:391-460`
- Test: `slash.rs` 内 `mod tests`、`main.rs` 内 `mod tests`

**Interfaces:**
- Consumes: Task 3 `reload()` / `config_fingerprint()`、Task 5 `register_mcp_reload_tool`
- Produces: `McpAction::Reload`

- [ ] **Step 1: 写失败测试（slash 解析）**

```rust
    #[test]
    fn parses_mcp_reload() {
        assert_eq!(parse_mcp_args("reload").unwrap(), McpAction::Reload);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib parses_mcp_reload`
Expected: FAIL — 无 `Reload` 变体

- [ ] **Step 3: 实现 slash 解析**

`McpAction` 加 `Reload` 变体；`parse_mcp_args` 加 `["reload"] => Ok(McpAction::Reload)`；用法串改为
`"用法: /mcp [on|off|reload|enable <server>|disable <server>|status]"`（两处错误文案都要改）。

`app.rs` 的 `SlashCommand::Mcp` 分支加：

```rust
                Ok(McpAction::Reload) => {
                    // 强制刷新一次：重读配置 + 只探变化项 + 重建 agent。
                    let _ = control_tx.blocking_send(crate::DriverControl::McpReload);
                    history.push(
                        HistoryCell::Separator { label: Some("MCP 配置已重载".into()) },
                        width,
                    );
                }
```

- [ ] **Step 4: main.rs 接线**

`DriverControl` 加 `McpReload`；在 `:2075` 的 `McpRefresh` 分支**之前**加一个分支：

```rust
                        DriverControl::McpReload => {
                            let report = mcp_for_driver.reload(&config.workdir).await;
                            tracing::info!(?report, "MCP config reloaded via /mcp reload");
                            let mut refreshed = (*current_tools).clone();
                            mcp_for_driver.refresh_registry(&mut refreshed);
                            current_tools = Arc::new(refreshed);
                            let handle = agent.session_handle();
                            agent = live.rebuild(
                                Arc::clone(&current_tools),
                                Arc::clone(&current_checker),
                                Arc::clone(&rebuild_decision_rx),
                                handle,
                            );
                        }
```

在 `:2353` 之前（`DriverInput::Prompt` 分支内、catalog 刷新旁边）加自动检测：

```rust
                // MCP 配置热重载：只比指纹，未变则零开销跳过。
                if let Some(fp) = mcp_for_driver.config_fingerprint(&config.workdir) {
                    if mcp_for_driver.last_seen_fingerprint().as_deref() != Some(fp.as_str()) {
                        let report = mcp_for_driver.reload(&config.workdir).await;
                        tracing::info!(?report, "MCP config changed on disk; reloaded");
                        let mut refreshed = (*current_tools).clone();
                        mcp_for_driver.refresh_registry(&mut refreshed);
                        current_tools = Arc::new(refreshed);
                        let handle = agent.session_handle();
                        agent = live.rebuild(
                            Arc::clone(&current_tools),
                            Arc::clone(&current_checker),
                            Arc::clone(&rebuild_decision_rx),
                            handle,
                        );
                    }
                }
```

`McpManager` 需要 `last_seen_fingerprint()`（返回 `Option<String>`）并在 `reload()` 末尾更新它。

`:1295` 的注册处，在 `register_mcp_tools` 之后追加：

```rust
    yi_agent_mcp::register_mcp_reload_tool(
        &mut registry,
        Arc::clone(&mcp_for_reload),
        &config.workdir,
        Some(Arc::clone(&blocklist_fn)),
    );
```

注意空 manager 分支：`register_mcp_tools` 返回 `Ok(None)` 时用的是 `McpManager::empty()`。
必须把**同一个** Arc 交给工具，否则 reload 改的不是 registry 用的那个 manager。
把 `let mcp = match ...` 的结果先绑定成变量，再克隆给工具。

`blocklist_fn` 的获取：TUI 构造 `PermissionChecker` 时用的是
`yi_agent_runtime::bootstrap::load_permission_checker`，它内部持有 `BlocklistFn`；
为不改变该函数的签名，本期在 `main.rs` 里直接构造同一份：
`yi_agent_tools::blocklist::as_fn()`（如不存在，则在 `shell/blocklist.rs` 加一个
`pub fn as_fn() -> BlocklistFn` 把 `is_blocked` 包成 `Arc`）。

- [ ] **Step 5: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib`
Expected: PASS

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src
git commit -m "feat(tui): MCP 配置每轮热重载与 /mcp reload"
```

---

### Task 8: 桌面端 `mcp/set` + 每轮热重载

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/mcp_rpc.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mcp.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:5019`（空闲路径执行 SessionCommand）、`:5160`（每轮开跑前）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/session.rs:73`（`SessionCommand`）
- Test: `mcp_rpc.rs` 内 `mod tests`、`server.rs` 内 `mod tests`

**Interfaces:**
- Consumes: Task 1 `save()`、Task 3 `reload()`
- Produces: RPC `mcp/set`，参数 `{ project?, scope: "master"|"server", name?, enabled: bool }`

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn set_requires_control_scope() {
        // 在 server.rs 的 RPC 测试里：Observe 档调用 mcp/set 必须 insufficient_scope。
        // 仿 server.rs:7885 的 mcp_list_is_available_at_observe_scope 写法。
    }

    #[test]
    fn set_server_toggles_enabled_in_the_file() {
        let project = project_with_config(r#"{"mcpServers":{"gh":{"command":"npx"}}}"#);
        let params = json!({"scope": "server", "name": "gh", "enabled": false});
        handle_mcp_request_at(project.path(), "mcp/set", &params).unwrap();
        let saved = std::fs::read_to_string(project.path().join(".yi-agent/mcp.json")).unwrap();
        assert!(saved.contains("\"enabled\": false"), "got {saved}");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib mcp_rpc`
Expected: FAIL — `mcp/set` 走到 `unknown method`

- [ ] **Step 3: 实现 `mcp/set`**

`mcp_rpc.rs` 加分支与处理函数（与 `list` 同形，读取 `plugin_scope` 得到项目根）：

```rust
        "mcp/set" => set(host_workdir, params),

fn set(host_workdir: &Path, params: &Value) -> Result<Value, RpcError> {
    let project = crate::server::plugin_scope(host_workdir, params);
    let scope = params.get("scope").and_then(|v| v.as_str()).unwrap_or("server");
    let enabled = params
        .get("enabled")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| RpcError::invalid_params("缺少 enabled"))?;
    let mut cfg = McpConfig::load(&project)
        .map_err(|e| RpcError::invalid_params(format!("读取 mcp.json 失败: {e:#}")))?
        .unwrap_or(McpConfig { enabled: true, mcp_servers: Default::default() });
    match scope {
        "master" => cfg.enabled = enabled,
        "server" => {
            let name = params
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| RpcError::invalid_params("scope=server 时缺少 name"))?;
            let entry = cfg
                .mcp_servers
                .get_mut(name)
                .ok_or_else(|| RpcError::invalid_params(format!("没有名为 {name:?} 的 server")))?;
            entry.enabled = enabled;
        }
        other => {
            return Err(RpcError::invalid_params(format!(
                "不支持的 scope: {other:?}；只支持 master / server"
            )))
        }
    }
    cfg.save(&project)
        .map_err(|e| RpcError::invalid_params(format!("写入 mcp.json 失败: {e:#}")))?;
    Ok(json!({"ok": true}))
}
```

`yi-agent-mcp` 需要 `McpConfig::default()` 或上面的手工构造；`Default` 的 `enabled=false` 会写坏新文件，所以**必须**用 `McpConfig { enabled: true, mcp_servers: Default::default() }`。

`rpc/mcp.rs` 加 `"mcp/set"` 分支，明确要求 Control 档（照 `rpc/flags.rs:23-45`）：

```rust
        "mcp/set" => {
            if state.client_scope < crate::protocol::Scope::Control {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, crate::protocol::RpcError::insufficient_scope(crate::protocol::Scope::Control)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            // …同 list 的调用与应答写法
        }
```

- [ ] **Step 4: 每轮热重载**

`SessionCommand` 加变体：

```rust
    /// 重读项目 `mcp.json` 并重建 registry。`/mcp reload` 与配置变更检测共用。
    ReloadMcp { reply: oneshot::Sender<Result<(), String>> },
```

`apply_session_command` 处理它：`mcp.reload(&workdir)` → `refresh_registry` → 重建 agent。
`run_thread_driver` 需要新增一个 `mcp: Arc<yi_agent_mcp::McpManager>` 形参（从
`build_thread_tooling` 的 `setup.mcp` 取；`ToolSetup.mcp` 是 `Option`，`None` 时传
`McpManager::empty()`）。

在 `:5160`（catalog 刷新旁、`agent.run_blocks` 之前）加：

```rust
        // MCP 配置热重载：指纹未变则零开销。
        if let Some(fp) = mcp.config_fingerprint(&config.workdir) {
            if mcp.last_seen_fingerprint().as_deref() != Some(fp.as_str()) {
                let _ = mcp.reload(&config.workdir).await;
                let mut next = (*agent.tools()).clone();
                mcp.refresh_registry(&mut next);
                agent = rebuild_thread_agent_with_tools(agent, Arc::new(next), theme.clone());
            }
        }
```

`rebuild_thread_agent_with_tools` 若不存在，按 `rebuild_thread_agent_with_theme`
（`server.rs:1373`）的形状新增一个只换 registry 的版本，保留 `permission_checker`、
`decision_rx`、`session_handle`。

- [ ] **Step 5: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib mcp`
Expected: PASS

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src
git commit -m "feat(app-server): mcp/set 与每轮 MCP 配置热重载"
```

---

### Task 9: 桌面设置页开关

**Files:**
- Modify: `desktop/src/components/SettingsMcpTab.tsx:120-215`
- Modify: `desktop/src/lib/mcp.ts`
- Test: `desktop/src/components/SettingsMcpTab.test.tsx`

**Interfaces:**
- Consumes: Task 8 的 `mcp/set`
- Produces: 无（UI 终端）

- [ ] **Step 1: 写失败测试**

```tsx
it("toggles a server through mcp/set", async () => {
  const call = vi.fn(async (method: string) => {
    if (method === "mcp/list") return {
      configured: true, path: "/p/.yi-agent/mcp.json", enabled: true,
      servers: [{ name: "gh", kind: "stdio", enabled: true, command: "npx", args: [], url: null, env_keys: [], header_keys: [], tools: null, tool_count: 0, read_only_count: 0 }],
    };
    return { ok: true };
  });
  render(<SettingsMcpTab call={call as never} projects={["/p"]} defaultProject="/p" />);
  const toggle = await screen.findByRole("switch", { name: /gh/i });
  await userEvent.click(toggle);
  expect(call).toHaveBeenCalledWith("mcp/set", { project: "/p", scope: "server", name: "gh", enabled: false });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/SettingsMcpTab.test.tsx`
Expected: FAIL — 没有 switch 角色

- [ ] **Step 3: 实现**

- `lib/mcp.ts`：把 `call` 类型扩到含 `"mcp/set"`；
- `SettingsMcpTab.tsx`：把 `<span>` 徽章换成按钮式开关（`role="switch"`、`aria-checked`），
  `onClick` 调 `call("mcp/set", { project: selected, scope: "server", name: server.name, enabled: !server.enabled })`，
  成功后就地更新本地 `listed`（乐观更新，失败则回滚并显示错误）；
- 顶部再加一个全局开关（`scope: "master"`）；
- 文案：`AddMcpHelp` 里「重启侧车后生效」改为「下一轮对话生效」，并补一句
  「也可以直接让 Yi-Agent 帮你加：说『帮我加一个 xxx MCP』」。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/SettingsMcpTab.test.tsx src/lib/mcp.test.ts && npx tsc --noEmit`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add desktop/src
git commit -m "feat(desktop): MCP 设置页启停开关"
```

---

### Task 10: 内置 `mcp-setup` skill

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-skills/src/assets/mcp-setup/SKILL.md`
- Test: `yi-agent-rs/crates/yi-agent-skills/src/system.rs` 既有 `installs_to_empty_dir`

**Interfaces:**
- Consumes: Task 5 的工具契约
- Produces: 无（模型侧指导）

- [ ] **Step 1: 先加断言**

在 `system.rs` 的 `installs_to_empty_dir` 里追加：

```rust
        assert!(tmp.path().join("mcp-setup/SKILL.md").is_file());
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-skills --lib installs_to_empty_dir`
Expected: FAIL

- [ ] **Step 3: 写 skill**

```markdown
---
name: mcp-setup
description: "Use when the user wants to add, enable, disable, remove, or troubleshoot an MCP server in this project (e.g. '帮我加一个 GitHub MCP', '关掉那个 fs MCP', 'MCP 工具怎么没出现'). Covers choosing stdio vs http, filling mcp_reload arguments, and reading probe failures."
---

# 管理本项目的 MCP server

MCP 配置是**项目级**文件：`<workdir>/.yi-agent/mcp.json`。
只有项目级一层——**没有**用户级配置，所以不要承诺「以后所有项目都能用」。

## 用工具，不要手写 JSON

加/启用/停用/删除 server 一律用 `mcp_reload`，不要用 `write` 直接改 `mcp.json`：
工具会做校验、合并、原子写盘，并在失败时把原因原文给你。

```json
{"action": "add", "name": "github", "command": "npx",
 "args": ["-y", "@modelcontextprotocol/server-github"],
 "env": {"GITHUB_PERSONAL_ACCESS_TOKEN": "${GITHUB_TOKEN}"}}
```

- `action` 取 `add` / `enable` / `disable` / `remove`。
- `name`：小写字母、数字、连字符；不能首尾或连续连字符；<=64 字节。
- 覆盖已存在的同名 server 要显式传 `"overwrite": true`。

## 选 stdio 还是 http

| 拿到的是什么 | 用哪种 |
| --- | --- |
| 一条启动命令（`npx` / `uvx` / 本地二进制） | `stdio`（默认，不用写 `kind`） |
| 一个以 `http://` / `https://` 开头的 MCP endpoint | `http`，填 `url`（+ 可选 `headers`） |

没有 `sse` 这种 type，传了会被拒绝。

## 生效时机（重要）

配置改完后，**新工具从下一条消息起才可用**，本轮不要尝试调用 `mcp__*`。
工具的返回里会写这句，照它说给用户听。

## 探测失败怎么读

工具会逐个 server 回报：

```
github  ✓ 7 个工具（4 只读）
fs      失败：MCP connect timed out after 30s
```

- **超时 / connect 失败**：多半是包名写错或首次 `npx -y` 下载太慢。核对包名，
  必要时让用户先手动装一次。
- **401 / 403**：`headers` / `env` 里的 token 不对或没设。工具会在写入前拒绝
  引用了未定义环境变量的配置——所以如果你看到那条错误，让用户先 `export` 该变量。
- **配置写盘成功但探测失败**：配置本身是对的，server 只是暂时起不来。如实告诉用户。

## 不要做的事

- 不要关闭全局 MCP（`mcp.json` 顶层 `enabled`）：用户要全关时，让他用
  `/mcp off`（终端）或设置页开关。这是用户的账户级偏好，不由你代决。
- 不要在同一轮里反复 reload 试探，一次到位。
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-skills`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-skills/src
git commit -m "feat(skills): 内置 mcp-setup skill"
```

---

### Task 11: 文档同步

**Files:**
- Modify: `docs/project-management/yi-agent-mcp.md`
- Modify: `docs/project-management/yi-agent-skills.md`
- Modify: `docs/project-management/desktop.md`
- Modify: `README.md`（「它能做什么」之后加「接入 MCP」小节）

- [ ] **Step 1: 更新 `yi-agent-mcp.md`**

在 Features 追加：

```markdown
- [x] 运行时自助重载（`mcp_reload` 工具）
  - 工具：`crates/yi-agent-mcp/src/reload_tool.rs`（`action: add|enable|disable|remove`，无条件注册）
  - 校验/合并纯逻辑：`crates/yi-agent-mcp/src/reload.rs`
  - 重载：`manager.rs::reload`（只探新增/变化项，指纹复用 `cache.rs`）+ `last_registered` 清理孤儿工具
  - 原子写盘：`config.rs::McpConfig::save`（临时文件 + rename）
  - 权限：`yi-agent-core/src/permission.rs` 中 `mcp_reload` 始终 `NeedConfirm`（不受 yolo 豁免）
  - 生效：TUI `crates/yi-agent/src/main.rs:2353` 与桌面 `crates/yi-agent-app-server/src/server.rs:5160`
    每轮开跑前比指纹；兜底 `/mcp reload` 与 `mcp/set`
  - 验证：`cargo test -p yi-agent-mcp`、`cargo test -p yi-agent-core --lib permission`、
    `cargo test -p yi-agent --lib parses_mcp_reload`、`cargo test -p yi-agent-app-server --lib mcp`
```

并把模块计数在 `README.md` 的索引表加一。

- [ ] **Step 2: 更新 `yi-agent-skills.md` 与 `desktop.md`**

- skills：追加一行「内置 `mcp-setup` skill — `src/assets/mcp-setup/SKILL.md`」
- desktop：把 `SettingsMcpTab` 从「只读接缝」改为「只读清单 + 启停开关」，注明
  `mcp/set` 需 Control 档

- [ ] **Step 3: 更新 README**

在「它能做什么」列表后加：

```markdown
### 接入 MCP server

在对话里直接说就行：「帮我加一个 GitHub MCP」。模型会调用 `mcp_reload` 改写
`<项目>/.yi-agent/mcp.json`（Claude Desktop 兼容 schema），**下一轮对话生效**，
不需要重启。也可以手工编辑该文件，或用桌面端「设置 → MCP」启停。

支持 stdio（`command` + `args`）与 Streamable HTTP（`url` + `headers`）两种传输；
配置为项目级，每个项目一份。
```

- [ ] **Step 4: 跑全量分 crate 验证**

```bash
cd yi-agent-rs
cargo test -p yi-agent-mcp
cargo test -p yi-agent-core
cargo test -p yi-agent-skills
cargo test -p yi-agent --lib
cargo test -p yi-agent-app-server --lib
cargo fmt --all -- --check
```

Expected: 全绿。（不要跑 `--workspace`。）

- [ ] **Step 5: 提交**

```bash
git add docs README.md
git commit -m "docs: MCP 自助重载与设置页开关"
```

---

## 自检结果

**Spec 覆盖检查**

| Spec 章节 | 对应任务 |
| --- | --- |
| §4.1 生效模型（回合之间重建 + 两端检测点） | Task 7、Task 8 |
| §4.2 `mcp_reload` action 多动作 + 无条件注册 + 不给全局开关 | Task 4、Task 5 |
| §4.3 `McpManager::reload` + `last_registered` | Task 2、Task 3 |
| §4.4 校验规则表 | Task 4 |
| §4.5 危险命令拦截 | Task 5 |
| §4.6 权限（四动作全确认、无 Prefix） | Task 6 |
| §4.7 设置页开关 + 文案修正 | Task 8、Task 9 |
| §4.8 内置 skill | Task 10 |
| §5 错误处理（不吞 / 原子写 / 坏 JSON 行号） | Task 1、Task 3、Task 5 |
| §6 测试 | 各任务内 + Task 11 Step 4 |
| §7 文档同步 | Task 11 |

**类型一致性**：`ReloadAction` / `ReloadRequest` / `ReloadReport` / `ServerOutcome`
在 Task 3、4、5 之间命名一致；`McpReloadTool::new(manager, workdir, blocklist)`
三参在 Task 5 定义、Task 7 调用一致；`refresh_registry(self: &Arc<Self>, ...)` 签名未变。

**已知待补的实现细节（执行时按注释处理，不影响任务边界）**
- `test_support::install_counting_connector` 与 `reload_from` 需在 `lib.rs` 的
  `pub mod test_support` 内实现（Task 3 Step 1 给了契约）。
- `blocklist::as_fn()` 若不存在则在 `shell/blocklist.rs` 补，属 Task 7 范围内。
- `rebuild_thread_agent_with_tools` 若不存在则按 `server.rs:1373` 形状新增，属 Task 8 范围内。
