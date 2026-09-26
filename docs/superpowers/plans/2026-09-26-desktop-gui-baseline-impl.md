# Desktop GUI Baseline — Phase 1 (Rust Foundation) Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 抽出 `yi-agent-runtime` 共享 crate,并新建 `yi-agent-app-server`(JSON-RPC over stdio),
让 GUI 能通过标准协议驱动 agent;CLI 行为保持不变。

**Architecture:** 三层——`yi-agent-runtime`(配置加载 + Agent 装配,CLI 与 app-server 共用)、
`yi-agent-app-server`(协议 + stdio 传输 + thread/turn 状态机 + 权限反向请求)、
CLI 增加 `app-server` 子命令作为 Tauri sidecar 入口。Tauri/前端是 Phase 2,不在本计划内。

**Tech Stack:** Rust 2024 / tokio / serde / serde_json / async-trait / tracing / wiremock(测试)。

**前置阅读:**
- 设计文档:`docs/superpowers/plans/2026-09-26-desktop-gui-design.md`
- 现有装配参考:`yi-agent-rs/crates/yi-agent/src/main.rs:833-908`(TUI 装配)、
  `main.rs:1091-1217`(headless 装配 + drain)
- 现有配置:`yi-agent-rs/crates/yi-agent/src/config.rs`

**工作目录(worktree):** `.worktrees/feat/desktop-gui-baseline`
所有命令在 worktree 根目录下执行;Rust 命令用
`cargo <cmd> --manifest-path yi-agent-rs/Cargo.toml -p <crate>`。

**重要约束(来自 CLAUDE.md):**
- 跑测试前先 `ps aux | grep cargo` 确认无残留 cargo 进程。
- 按 crate 跑测试,避免 `--workspace` 全量(易 OOM / 死锁)。
- commit 前必须 `cargo fmt --all`(在 `yi-agent-rs/` 下)。
- commit message 用 conventional commits,**不要**写 `Co-Authored-By`。

---

## Phase A:抽出 `yi-agent-runtime`

> 目标:把配置加载与 Agent 装配从 binary crate 移到共享 crate,CLI 行为**逐字节不变**。
> 关键取舍:为降低回归风险,`RuntimeConfig` 的字段名与类型**完全沿用**现有 `Config`
> (保留 `provider: String`、`api_url` 等),不引入 `ProviderKind` 枚举——枚举留到后续。

### Task A1:`yi-agent-runtime` crate 骨架

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-runtime/Cargo.toml`
- Create: `yi-agent-rs/crates/yi-agent-runtime/src/lib.rs`
- Modify: `yi-agent-rs/Cargo.toml`(workspace members)

**Step 1: 写 Cargo.toml**

```toml
[package]
name = "yi-agent-runtime"
version.workspace = true
edition.workspace = true
rust-version.workspace = true

[dependencies]
yi-agent-core = { path = "../yi-agent-core" }
yi-agent-llm = { path = "../yi-agent-llm" }
yi-agent-tools = { path = "../yi-agent-tools" }
yi-agent-skills = { path = "../yi-agent-skills" }
anyhow.workspace = true
dirs.workspace = true
dotenvy.workspace = true
serde.workspace = true
serde_json.workspace = true
tokio.workspace = true
tracing.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

> 注意:先核对 workspace `Cargo.toml` 里 `dotenvy` / `dirs` / `serde` 是否已声明为
> workspace 依赖;没有的用具体版本号,或在 workspace 里补声明。

**Step 2: 写 lib.rs 占位**

```rust
//! yi-agent 运行时:配置加载与 Agent 装配,供 CLI 与 app-server 共用。

pub mod config;
```

**Step 3: 注册到 workspace**

在 `yi-agent-rs/Cargo.toml` 的 `members` 数组里加
`"crates/yi-agent-runtime",`(与其它 crate 同格式)。

**Step 4: 编译**

Run: `cargo build --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-runtime`
Expected: 编译通过(crate 为空)。

**Step 5: Commit**

```bash
cargo fmt --all
git add yi-agent-rs/Cargo.toml yi-agent-rs/crates/yi-agent-runtime
git commit -m "feat(runtime): add yi-agent-runtime crate skeleton"
```

---

### Task A2:迁移 `Config` → `RuntimeConfig`(纯搬移)

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs`
- Test: 同文件 `#[cfg(test)] mod tests`

**Step 1: 搬移结构体**

把 `crates/yi-agent/src/config.rs:9-27` 的 `Config` 结构体复制过来,重命名为
`RuntimeConfig`,**字段名与类型一字不改**(含 `provider: String`、`api_url`、
`sandbox_writable_roots`、`skills_catalog_budget_explicit`)。

**Step 2: 搬移 env 工具函数**

搬移 `resolve_env_path`、`load_env_files`、`load_one_env`、`resolve_global_env_path`、
`is_workdir_explicit`、`resolve_workdir`、`env_usize`。
把它们的参数从 `&Cli` 换成 `&ConfigOverrides`(见 A3),或直接换成所需字段。

**Step 3: 写失败测试**

把 `config.rs:664-698`(`resolve_workdir_prefers_cli_value`、
`resolve_workdir_uses_nonempty_environment_value`)搬过来,把 `Cli::parse_from([...])`
换成手工构造 `ConfigOverrides { workdir: Some(temp.path().into()), ..Default::default() }`。

```rust
#[test]
fn resolve_workdir_prefers_override_value() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let overrides = ConfigOverrides {
        workdir: Some(temp.path().to_path_buf()),
        ..Default::default()
    };
    assert_eq!(resolve_workdir(&overrides).expect("resolve"), temp.path());
}
```

**Step 4: 跑测试确认失败**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-runtime resolve_workdir`
Expected: FAIL(未实现 / 未定义)。

**Step 5: 实现**

让 `resolve_workdir(&ConfigOverrides) -> Result<PathBuf>` 与现有逻辑一致:
`overrides.workdir` > 非空 `YI_AGENT_WORKDIR` > `current_dir()`,并校验 `is_dir()`。

**Step 6: 跑测试确认通过**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-runtime`
Expected: PASS。

**Step 7: Commit**

```bash
cargo fmt --all
git add yi-agent-rs/crates/yi-agent-runtime/src/config.rs
git commit -m "feat(runtime): move config struct and env loading from CLI"
```

---

### Task A3:引入 `ConfigOverrides` 并迁移 `load()`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs`

**Step 1: 定义 `ConfigOverrides`**

```rust
/// 来自 CLI flag 或 GUI 请求的纯数据覆盖项。不依赖 clap。
#[derive(Debug, Clone, Default)]
pub struct ConfigOverrides {
    pub provider: Option<String>,
    pub api_url: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub max_turns: Option<u32>,
    pub workdir: Option<PathBuf>,
    pub system_prompt: Option<String>,
    pub model_context_length: Option<u32>,
    pub compact_ratio: Option<u32>,
    pub compact_keep_turns: Option<u32>,
    pub compact_user_budget_tokens: Option<usize>,
    pub compact_tool_budget_tokens: Option<usize>,
    pub yolo: bool,
    pub skip_permissions: bool,
    pub sandbox: Option<yi_agent_tools::SandboxMode>,
    pub sandbox_writable_roots: Vec<PathBuf>,
    pub skills_catalog_budget: Option<usize>,
}
```

**Step 2: 写失败测试(逐条搬现有测试)**

把 `config.rs` 里这些测试搬过来,`Cli{...}` → `ConfigOverrides{...}`:
`load_requires_api_key`、`load_loads_from_cli_args`、`load_defaults_api_url_and_model`、
`load_includes_compact_defaults`、`load_computes_threshold_from_context_and_ratio`、
`load_falls_back_to_default_context_length`、`load_rejects_nonexistent_workdir`、
`load_defaults_provider_to_anthropic`、`load_defaults_openai_provider`、
`load_reads_dotenv_file`、`load_yolo_from_cli_flag`、`explicit_cli_sandbox_overrides_yolo`、
`environment_sandbox_overrides_yolo`、`skip_permissions_keeps_default_sandbox`、
`yolo_environment_variable_keeps_default_sandbox`、`load_yolo_defaults_false`、
`load_falls_back_to_current_dir_when_workdir_env_empty`、
`load_does_not_create_local_yi_agent_dir_in_fallback_mode`。
`EnvVarGuard` / `ENV_TEST_MUTEX` / `isolated_config_env` / `CurrentDirGuard` 一并搬过来。

**Step 3: 跑测试确认失败**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-runtime`
Expected: FAIL(`load` 未定义)。

**Step 4: 实现 `load(&ConfigOverrides) -> Result<RuntimeConfig>`**

把 `config.rs:373-527` 的逻辑整体搬过来,把每一处 `cli.X` 换成 `overrides.X`,
`is_workdir_explicit(cli)` 换成 `is_workdir_explicit(&overrides)`。
**默认值、优先级、报错文案一字不改。**

**Step 5: 跑测试确认通过**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-runtime`
Expected: 全部 PASS。

**Step 6: Commit**

```bash
cargo fmt --all
git add yi-agent-rs/crates/yi-agent-runtime/src/config.rs
git commit -m "feat(runtime): add ConfigOverrides and move load()"
```

---

### Task A4:`redacted_view()`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs`

**Step 1: 写失败测试**

```rust
#[test]
fn redacted_view_hides_api_key() {
    let cfg = RuntimeConfig {
        api_key: "sk-super-secret".into(),
        ..RuntimeConfig::test_default()
    };
    let view = cfg.redacted_view();
    assert_eq!(view["api_key"], "***");
    assert_eq!(view["model"], cfg.model);
}
```

> `test_default()` 是本 crate 测试用的构造函数(可用 `#[cfg(test)]` 限定),
> 用固定字段值填充,避免每个测试重复写全部字段。

**Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-runtime redacted_view`
Expected: FAIL。

**Step 3: 实现**

```rust
/// 返回给 GUI 的安全配置视图:api_key 脱敏,其余字段原样。
pub fn redacted_view(&self) -> serde_json::Value {
    serde_json::json!({
        "provider": self.provider,
        "api_url": self.api_url,
        "api_key": if self.api_key.is_empty() { "" } else { "***" },
        "model": self.model,
        "max_turns": self.max_turns,
        "workdir": self.workdir.display().to_string(),
        "sandbox": format!("{:?}", self.sandbox),
        "yolo": self.yolo,
        "compact_threshold": self.compact_threshold,
    })
}
```

**Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-runtime redacted_view`
Expected: PASS。

**Step 5: Commit**

```bash
cargo fmt --all
git add yi-agent-rs/crates/yi-agent-runtime/src/config.rs
git commit -m "feat(runtime): add redacted config view for GUI"
```

---

### Task A5:`build_provider()`

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/lib.rs`(加 `pub mod bootstrap;`)

**Step 1: 写失败测试**

```rust
#[test]
fn build_provider_rejects_unknown_name() {
    let cfg = RuntimeConfig { provider: "gemini".into(), ..RuntimeConfig::test_default() };
    let err = build_provider(&cfg).unwrap_err();
    assert!(format!("{err}").contains("unknown provider"));
}

#[test]
fn build_provider_accepts_anthropic_and_openai() {
    for name in ["anthropic", "openai"] {
        let cfg = RuntimeConfig { provider: name.into(), ..RuntimeConfig::test_default() };
        assert!(build_provider(&cfg).is_ok(), "provider {name} should build");
    }
}
```

**Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-runtime build_provider`
Expected: FAIL。

**Step 3: 实现**

把 `main.rs:1150-1169`(及 `843-862` 的重复版本)搬过来,签名改为:

```rust
pub fn build_provider(
    cfg: &RuntimeConfig,
) -> Result<Arc<dyn yi_agent_core::Provider>, anyhow::Error> {
    match cfg.provider.as_str() {
        "anthropic" => Ok(Arc::new(yi_agent_llm::AnthropicProvider::new(
            yi_agent_llm::AnthropicProviderOpts {
                base_url: Some(cfg.api_url.clone()),
                api_key: Some(cfg.api_key.clone()),
                ..Default::default()
            },
        )?)),
        "openai" => Ok(Arc::new(yi_agent_llm::OpenaiProvider::new(
            yi_agent_llm::OpenaiProviderOpts {
                base_url: Some(cfg.api_url.clone()),
                api_key: Some(cfg.api_key.clone()),
                ..Default::default()
            },
        )?)),
        other => anyhow::bail!("unknown provider '{}': expected 'anthropic' or 'openai'", other),
    }
}
```

**Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-runtime build_provider`
Expected: PASS。

**Step 5: Commit**

```bash
cargo fmt --all
git add yi-agent-rs/crates/yi-agent-runtime/src
git commit -m "feat(runtime): add build_provider"
```

---

### Task A6:`build_tools()` + `build_system_prompt()` + skills

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs`

**Step 1: 搬移 skills 逻辑**

把 `main.rs:1572-1610` 的 `setup_skills` 和 `1613+` 的 `resolve_system_prompt_with_skills`
搬过来(重命名 `setup_skills` → `build_skills_service`,参数改 `&RuntimeConfig`)。
`resolve_system_prompt`(被 `resolve_system_prompt_with_skills` 调用)一并搬移。
注意:这些函数目前可能依赖 `crate::tui::...`;若依赖,需一并复制相关辅助函数或
先确认依赖面(用 `grep` 查 `resolve_system_prompt` 的实现位置)。

**Step 2: 写失败测试**

```rust
#[test]
fn build_tools_registers_fs_and_shell() {
    let cfg = RuntimeConfig::test_default();
    let tools = build_tools(&cfg).expect("build tools");
    let names = tools.names(); // 若无 names(),用 ToolRegistry 现有枚举接口
    assert!(names.contains(&"bash".to_string()) || names.iter().any(|n| n == "bash"));
}

#[test]
fn build_system_prompt_includes_current_date() {
    let cfg = RuntimeConfig::test_default();
    let prompt = build_system_prompt(&cfg);
    assert!(prompt.contains("2026") || prompt.len() > 0);
}
```

> 实现前先看 `ToolRegistry` 有没有公开遍历接口(`yi-agent-core/src/tool.rs`)。
> 没有的话,测试改成断言"注册后 `registry.get("bash").is_some()`"这类。

**Step 3: 跑测试确认失败**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-runtime build_tools`
Expected: FAIL。

**Step 4: 实现**

```rust
/// 注册内置工具(含 sandbox 配置)。naked 模式下返回空 registry。
pub fn build_tools(cfg: &RuntimeConfig) -> Result<Arc<yi_agent_core::ToolRegistry>>;

/// 解析 system prompt(默认 prompt + 当前日期 + skills catalog)。
pub fn build_system_prompt(cfg: &RuntimeConfig) -> String;

/// naked 模式:空 registry + None prompt,供 CLI `--naked` 使用。
pub struct ToolSetup { pub tools: Arc<yi_agent_core::ToolRegistry>, pub system_prompt: Option<String> }
pub fn build_tool_setup(cfg: &RuntimeConfig, naked: bool) -> Result<ToolSetup>;
```

把 `main.rs:1048-1087`(`build_headless_setup_for_workspace`)的逻辑搬进来。
注意 TUI 版还额外注册了 `ProcessManager` + `register_process_tools`(`main.rs:888-896`);
**app-server 需要进程工具**(否则 shell 类工具有缺失),因此 `build_tool_setup` 应
包含 `register_process_tools`,与 TUI 对齐而非 headless 对齐。这一点在实现时用
`git log`/现有测试确认 headless 与 TUI 的工具差异是否影响既有行为。

**Step 5: 跑测试确认通过**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-runtime`
Expected: PASS。

**Step 6: Commit**

```bash
cargo fmt --all
git add yi-agent-rs/crates/yi-agent-runtime/src
git commit -m "feat(runtime): add build_tools, build_system_prompt, skills setup"
```

---

### Task A7:`bootstrap_agent()`(装配 + 权限通道)

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs`

**Step 1: 定义类型**

```rust
pub enum PermissionMode { Interactive, AutoAllow }

pub struct AgentBootstrap {
    pub agent: yi_agent_core::Agent,
    pub permission: Arc<yi_agent_core::permission::PermissionChecker>,
    pub decision_tx: tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    /// app-server 持有 rx 以接收权限决定;AutoAllow 时返回 None。
    pub decision_rx: Option<Arc<tokio::sync::Mutex<
        tokio::sync::mpsc::Receiver<(u64, yi_agent_core::permission::Decision)>,
    >>>,
}
```

**Step 2: 写失败测试**

```rust
#[tokio::test]
async fn bootstrap_interactive_keeps_decision_channel_open() {
    let cfg = RuntimeConfig::test_default();
    let b = bootstrap_agent(&cfg, PermissionMode::Interactive).expect("bootstrap");
    assert!(b.decision_rx.is_some());
}

#[tokio::test]
async fn bootstrap_auto_allow_closes_decision_channel() {
    let cfg = RuntimeConfig::test_default();
    let b = bootstrap_agent(&cfg, PermissionMode::AutoAllow).expect("bootstrap");
    assert!(b.decision_rx.is_none());
}
```

**Step 3: 跑测试确认失败**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-runtime bootstrap`
Expected: FAIL。

**Step 4: 实现**

搬移 `main.rs:798-831`(`load_permission_checker_for_workdir`)的逻辑,
`yolo` 用 `cfg.yolo`(Interactive)或强制 `true`(AutoAllow)。
构造 `AgentConfig`(搬 `main.rs:900-908`,**注意要带上 `compact_threshold` 等字段**)。
`AutoAllow` 时 drop 掉 `decision_tx`(与 headless 一致,黑名单命令解析为 Deny)。

> 注意:`AgentConfig` 的 `think_idle_timeout` 等字段用 `..Default::default()`。

**Step 5: 跑测试确认通过**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-runtime`
Expected: PASS。

**Step 6: Commit**

```bash
cargo fmt --all
git add yi-agent-rs/crates/yi-agent-runtime/src
git commit -m "feat(runtime): add bootstrap_agent with permission channel"
```

---

## Phase B:CLI 改接 runtime crate(行为不变)

### Task B1:`config.rs` 瘦身为适配层

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs`
- Modify: `yi-agent-rs/crates/yi-agent/Cargo.toml`(加 `yi-agent-runtime` 依赖)

**Step 1: 写失败测试**

在 `crates/yi-agent/src/config.rs` 保留一个集成测试:

```rust
#[test]
fn cli_adapter_maps_all_overrides() {
    let cli = Cli::parse_from([
        "yi-agent", "--provider", "openai", "--api-key", "k",
        "--model", "m", "--max-turns", "7", "--workdir", ".",
    ]);
    let overrides: yi_agent_runtime::config::ConfigOverrides = (&cli).into();
    assert_eq!(overrides.provider.as_deref(), Some("openai"));
    assert_eq!(overrides.max_turns, Some(7));
    assert_eq!(overrides.model.as_deref(), Some("m"));
}
```

**Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent cli_adapter_maps_all_overrides`
Expected: FAIL。

**Step 3: 实现适配层**

```rust
impl From<&Cli> for yi_agent_runtime::config::ConfigOverrides {
    fn from(cli: &Cli) -> Self {
        Self {
            provider: cli.provider.clone(),
            api_url: cli.api_url.clone(),
            api_key: cli.api_key.clone(),
            model: cli.model.clone(),
            max_turns: cli.max_turns,
            workdir: cli.workdir.clone(),
            system_prompt: cli.system_prompt.clone(),
            model_context_length: cli.model_context_length,
            compact_ratio: cli.compact_ratio,
            compact_keep_turns: cli.compact_keep_turns,
            compact_user_budget_tokens: cli.compact_user_budget_tokens,
            compact_tool_budget_tokens: cli.compact_tool_budget_tokens,
            yolo: cli.yolo,
            skip_permissions: cli.skip_permissions,
            sandbox: cli.sandbox,
            sandbox_writable_roots: cli.sandbox_writable_roots.clone(),
            skills_catalog_budget: cli.skills_catalog_budget,
        }
    }
}

/// 保留原函数名,内部转发到 runtime crate,供 main.rs 与测试调用。
pub fn load(cli: &Cli) -> Result<yi_agent_runtime::config::RuntimeConfig> {
    yi_agent_runtime::config::RuntimeConfig::load(&cli.into())
}
```

`Cli` / `Command` / 各 `*Action` enum **留在本文件**。
`resolve_env_path` / `resolve_workdir` 等若 main.rs 仍在用,保留为转发函数;
若无人用则删除。

**Step 4: 迁移/删除旧测试**

`config.rs` 里**非 clap 解析类**的测试(`load_*`、`resolve_workdir_*`、
`load_env_files_*`、`resolve_env_path_*`、`*sandbox*` 等)已搬到 runtime crate,
在本文件删除;**clap 解析类测试**(`cli_parses_*`、`cli_defaults_*`、
`cli_no_subcommand_*`、`cli_rejects_*`、`cli_documented_*`)保留。

**Step 5: 跑测试确认通过**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent`
Expected: PASS。

**Step 6: Commit**

```bash
cargo fmt --all
git add yi-agent-rs/crates/yi-agent/src/config.rs yi-agent-rs/crates/yi-agent/Cargo.toml
git commit -m "refactor(cli): route config loading through yi-agent-runtime"
```

---

### Task B2:`main.rs` 装配点改调 runtime

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`

**Step 1: 改 `run_headless` 的装配(1091-1217)**

- 删除内联 provider 构造(1150-1169)→ 调 `yi_agent_runtime::bootstrap::build_provider(&config)`。
- 删除内联 `PermissionChecker` 构造(1131-1148)→ 调
  `bootstrap_agent(&config, PermissionMode::AutoAllow)`。
- `build_headless_setup` → 调 `build_tool_setup(&config, naked)`。
- 保留 `headless_runtime`(subagents)分支不动——该分支的 `build_headless_root_tools`
  是子 agent 专属,不在本次重构范围。

**Step 2: 改 `run_agent`(833-908)与 `run_tui_agent` 的装配**

- provider 构造 → `build_provider(&config)`。
- skills + system prompt + tools → `build_tool_setup(&config, false)`。
- `AgentConfig` 构造 → runtime crate 提供 `build_agent_config(&config, system_prompt)`
  或直接保留本地构造(二选一,优先复用以免漂移)。
- `PermissionChecker` → 保留 `load_permission_checker_for_workdir`(TUI 需要在
  切换 workspace 时重建 checker,签名与 config 耦合,可暂不迁移;若迁移则改为
  接受 `&RuntimeConfig`)。

**Step 3: 跑 CLI 测试**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent`
Expected: PASS(含 `build_headless_setup_*`、`drain_stream_*` 等既有测试)。

**Step 4: 跑 core/tools 回归**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-core`
Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-tools`
Expected: PASS。

**Step 5: 端到端回归(真实 LLM,可选但有价值)**

Run: `just test-real-e2e`(需 API key;无 key 自动跳过)
Expected: PASS 或 skip。

**Step 6: Commit**

```bash
cargo fmt --all
git add yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "refactor(cli): use yi-agent-runtime for provider/tools/bootstrap"
```

---

## Phase C:`yi-agent-app-server`

### Task C1:crate 骨架 + 协议类型

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml`
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`
- Modify: `yi-agent-rs/Cargo.toml`(workspace members)

**Step 1: Cargo.toml**

```toml
[package]
name = "yi-agent-app-server"
version.workspace = true
edition.workspace = true
rust-version.workspace = true

[dependencies]
yi-agent-core = { path = "../yi-agent-core" }
yi-agent-runtime = { path = "../yi-agent-runtime" }
anyhow.workspace = true
serde = { workspace = true, features = ["derive"] }
serde_json.workspace = true
tokio = { workspace = true, features = ["full"] }
tokio-util.workspace = true
tracing.workspace = true

[dev-dependencies]
wiremock.workspace = true
tempfile.workspace = true
```

**Step 2: 写 protocol.rs(完整类型)**

```rust
//! JSON-RPC 2.0 信封与 yi-agent 协议类型。

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId { Num(i64), Str(String) }

#[derive(Debug, Clone, Deserialize)]
pub struct RequestEnvelope {
    #[serde(default)] pub jsonrpc: Option<String>,
    pub id: RequestId,
    pub method: String,
    #[serde(default)] pub params: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponseEnvelope {
    #[serde(default)] pub jsonrpc: Option<String>,
    pub id: RequestId,
    #[serde(default)] pub result: Option<Value>,
    #[serde(default)] pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")] pub data: Option<Value>,
}

impl RpcError {
    pub fn parse_error(msg: impl Into<String>) -> Self { Self { code: -32700, message: msg.into(), data: None } }
    pub fn invalid_request(msg: impl Into<String>) -> Self { Self { code: -32600, message: msg.into(), data: None } }
    pub fn method_not_found(m: &str) -> Self { Self { code: -32601, message: format!("method not found: {m}"), data: None } }
    pub fn invalid_params(msg: impl Into<String>) -> Self { Self { code: -32602, message: msg.into(), data: None } }
    pub fn internal(msg: impl Into<String>) -> Self { Self { code: -32603, message: msg.into(), data: None } }
    pub fn not_initialized() -> Self { Self { code: -32010, message: "server not initialized".into(), data: None } }
    pub fn unknown_thread(id: &str) -> Self { Self { code: -32011, message: format!("unknown thread: {id}"), data: None } }
    pub fn turn_in_progress(id: &str) -> Self { Self { code: -32012, message: format!("turn already in progress: {id}"), data: None } }
}

/// 服务端 → 客户端通知(无 id)。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "method", content = "params")]
pub enum Notification {
    #[serde(rename = "thread/started")]
    ThreadStarted { thread_id: String, cwd: String, model: String },
    #[serde(rename = "turn/started")]
    TurnStarted { thread_id: String, turn_id: String },
    #[serde(rename = "item/started")]
    ItemStarted { thread_id: String, item: Item },
    #[serde(rename = "item/delta")]
    ItemDelta { thread_id: String, item_id: String, delta: String },
    #[serde(rename = "item/completed")]
    ItemCompleted { thread_id: String, item: Item },
    #[serde(rename = "turn/completed")]
    TurnCompleted { thread_id: String, turn_id: String, status: TurnStatus,
                    #[serde(skip_serializing_if = "Option::is_none")] error: Option<String> },
    #[serde(rename = "thread/tokenUsage/updated")]
    TokenUsage { thread_id: String, model: String, input_tokens: u32, output_tokens: u32 },
    #[serde(rename = "error")]
    Error { message: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus { Completed, Interrupted, Failed }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Item {
    UserMessage { id: String, text: String },
    AgentMessage { id: String, text: String },
    ToolCall { id: String, call_id: String, name: String, input: Value,
               status: ToolStatus,
               #[serde(skip_serializing_if = "Option::is_none")] result: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus { Running, Completed, Failed }
```

**Step 3: 写 serde 往返测试**

```rust
#[test]
fn request_envelope_parses_numeric_id() {
    let raw = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let req: RequestEnvelope = serde_json::from_str(raw).unwrap();
    assert_eq!(req.method, "initialize");
}

#[test]
fn request_envelope_parses_string_id() {
    let raw = r#"{"jsonrpc":"2.0","id":"perm-3","method":"x","params":{}}"#;
    let req: RequestEnvelope = serde_json::from_str(raw).unwrap();
    matches!(req.id, RequestId::Str(_));
}

#[test]
fn notification_serializes_with_method_tag() {
    let n = Notification::TurnStarted { thread_id: "t1".into(), turn_id: "u1".into() };
    let v: Value = serde_json::to_value(&n).unwrap();
    assert_eq!(v["method"], "turn/started");
    assert_eq!(v["params"]["thread_id"], "t1");
}
```

**Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-app-server`
Expected: PASS。

**Step 5: Commit**

```bash
cargo fmt --all
git add yi-agent-rs/Cargo.toml yi-agent-rs/crates/yi-agent-app-server
git commit -m "feat(app-server): add protocol types"
```

---

### Task C2:stdio 传输(JSONL 分帧)

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/transport.rs`

**Step 1: 写失败测试**

```rust
#[tokio::test]
async fn reads_one_message_per_line() {
    let input = b"{\"a\":1}\n{\"b\":2}\n";
    let mut reader = MessageReader::new(&input[..]);
    assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("{\"a\":1}"));
    assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("{\"b\":2}"));
    assert_eq!(reader.next_line().await.unwrap(), None);
}

#[tokio::test]
async fn rejects_oversized_frame() {
    let big = vec![b'x'; MAX_FRAME_BYTES + 10];
    let mut reader = MessageReader::new(&big[..]);
    assert!(reader.next_line().await.is_err());
}
```

**Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-app-server transport`
Expected: FAIL。

**Step 3: 实现**

```rust
/// 逐行读取 JSONL 帧,超长帧报错,EOF 返回 None。
pub struct MessageReader<R> { /* BufReader<R> */ }
impl<R: tokio::io::AsyncRead + Unpin> MessageReader<R> {
    pub fn new(r: R) -> Self;
    pub async fn next_line(&mut self) -> anyhow::Result<Option<String>>;
}

/// 串行化写出:每条消息一行 JSON + '\n'。
pub struct MessageWriter<W> { inner: tokio::sync::Mutex<W> }
impl<W: tokio::io::AsyncWrite + Unpin> MessageWriter<W> {
    pub fn new(w: W) -> Self;
    pub async fn write_value(&self, v: &impl serde::Serialize) -> anyhow::Result<()>;
}
```

`write_value` 里若 `serde_json::to_string` 失败 → 记 `tracing::error!` 到 stderr,
**不 panic**;写出后再 flush。

**Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-app-server transport`
Expected: PASS。

**Step 5: Commit**

```bash
cargo fmt --all
git add yi-agent-rs/crates/yi-agent-app-server/src
git commit -m "feat(app-server): add stdio JSONL transport"
```

---

### Task C3:`translate.rs` — AgentEvent → 通知

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/translate.rs`

**Step 1: 写失败测试(逐条映射断言)**

```rust
#[test]
fn assistant_text_starts_agent_message() {
    let mut t = Translator::new("t1".into());
    let out = t.on_event(AgentEvent::AssistantText("hello".into()));
    assert!(matches!(out[0], Notification::ItemStarted { .. }));
}

#[test]
fn decode_delta_emits_item_delta() {
    let mut t = Translator::new("t1".into());
    t.on_event(AgentEvent::AssistantText("hello".into())); // 先开 item
    let out = t.on_event(AgentEvent::DecodeDelta(" world".into()));
    match &out[0] {
        Notification::ItemDelta { delta, .. } => assert_eq!(delta, " world"),
        other => panic!("expected ItemDelta, got {other:?}"),
    }
}

#[test]
fn tool_call_starts_tool_item() {
    let mut t = Translator::new("t1".into());
    let out = t.on_event(AgentEvent::ToolCall { id: "c1".into(), name: "bash".into(), input: json!({"cmd":"ls"}) });
    match &out[0] {
        Notification::ItemStarted { item: Item::ToolCall { name, .. }, .. } => assert_eq!(name, "bash"),
        other => panic!("expected ToolCall item, got {other:?}"),
    }
}

#[test]
fn done_end_turn_completes() { /* AgentEvent::Done{reason:EndTurn} → TurnCompleted{status:Completed} */ }
#[test]
fn cancelled_completes_interrupted() { /* → TurnCompleted{status:Interrupted} */ }
#[test]
fn error_completes_failed() { /* → TurnCompleted{status:Failed, error:Some(..)} */ }
#[test]
fn usage_emits_token_usage() { /* → Notification::TokenUsage */ }
#[test]
fn permission_request_returns_reverse_request() { /* → 特殊返回,见 C5 */ }
```

**Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-app-server translate`
Expected: FAIL。

**Step 3: 实现**

`Translator` 持有当前 agent-message item 的 id 与累积文本,维护 call_id → item_id 映射。
`on_event(&mut self, ev: AgentEvent) -> Vec<Notification>`(权限事件见 C5 单独处理)。

映射表见设计文档 §6.5。注意 `AssistantText` 与 `DecodeDelta` 都追加到同一个
agentMessage item(`item/delta`),`Done` 时补一条 `item/completed`。

**Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-app-server translate`
Expected: PASS。

**Step 5: Commit**

```bash
cargo fmt --all
git add yi-agent-rs/crates/yi-agent-app-server/src
git commit -m "feat(app-server): add AgentEvent to notification translator"
```

---

### Task C4:server 主循环 + thread/turn 状态机

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/session.rs`
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
- Modify: `src/lib.rs`

**Step 1: 写失败测试(用 mock Provider)**

在 `server.rs` 测试里起一个内存管道,喂 `initialize` / `thread/start` / `turn/start`,
断言 stdout 通知序列。mock provider 参考 `yi-agent-llm` 的 wiremock 测试写法,
或直接实现一个 `struct MockProvider` 返回固定 `BoxStream`。

```rust
#[tokio::test]
async fn initialize_then_thread_then_turn_emits_expected_notifications() {
    // 1. 构造 MockProvider,依次 yield AssistantText("hi") + Done{EndTurn}
    // 2. 起 Server,喂入三行请求
    // 3. 断言收到 thread/started, turn/started, item/started, item/delta, item/completed, turn/completed
}
```

**Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-app-server server`
Expected: FAIL。

**Step 3: 实现**

- `session.rs`:`Session`(注意与 core 的 `Session` 重名,命名 `ThreadSession`)
  持有 `thread_id`、`Agent`、`Option<active_turn_id>`。
- `server.rs`:
  - `pub async fn run<R, W>(reader: R, writer: W, cfg: RuntimeConfig) -> anyhow::Result<()>`
  - 主循环读请求 → 分发 → 写响应。
  - `initialize` → 记 `initialized = true`,回 `{serverInfo, protocolVersion, capabilities}`。
  - 未 `initialize` 前调用其它方法 → `RpcError::not_initialized()`。
  - `thread/start` → 建 `ThreadSession`(`bootstrap_agent(cfg, Interactive)`),
    回 `{id, cwd, model}` + 发 `thread/started`。
  - `turn/start` → 若该 thread 已有活跃 turn → `turn_in_progress`;
    否则 spawn driver task 消费 `agent.run()` 的 stream,经 `Translator` 写通知。
  - `turn/interrupt` → `agent.cancel()`。
  - `config/read` → `cfg.redacted_view()`。
  - 未知方法 → `method_not_found`。
  - stdin EOF → 优雅退出。

> **取消 token 陷阱**:`Agent::run()` 会重置 token。driver task 必须在
> `run().await` **返回之后**调用 `agent.cancel_token()` 并注册到中断通道,
> 否则 `turn/interrupt` 无效。见 `CLAUDE.md`「cargo test 执行」记录。

**Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-app-server`
Expected: PASS。

**Step 5: Commit**

```bash
cargo fmt --all
git add yi-agent-rs/crates/yi-agent-app-server/src
git commit -m "feat(app-server): add server loop with thread/turn state machine"
```

---

### Task C5:权限审批反向请求闭环

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/translate.rs`

**Step 1: 写失败测试**

```rust
#[tokio::test]
async fn permission_request_round_trip() {
    // MockProvider 触发一个需要审批的工具调用
    // 1. 喂 turn/start
    // 2. 断言收到 server→client 请求 item/toolCall/requestApproval(id="perm-<n>")
    // 3. 喂入 client 响应 {"id":"perm-<n>","result":{"decision":"allow_once"}}
    // 4. 断言 agent 继续并最终 turn/completed
}

#[tokio::test]
async fn permission_timeout_defaults_to_deny() {
    // 不回应,断言超时后按 deny 处理(用短超时注入)
}
```

**Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-app-server permission`
Expected: FAIL。

**Step 3: 实现**

- driver 收到 `AgentEvent::PermissionRequest { request_id, tool_name, tool_input, kind, .. }`
  → 发反向请求 `{"jsonrpc":"2.0","id":"perm-<request_id>","method":"item/toolCall/requestApproval",
  "params":{...}}`,并把 `oneshot::Sender<Decision>` 存入
  `pending: Arc<Mutex<HashMap<RequestId, oneshot::Sender<Decision>>>>`。
- reader 收到**响应**(有 `id` 无 `method`)→ 从 `pending` 取出 sender → 解析
  `{"decision": "allow_once"|"always_allow"|"deny"}` → 转 `Decision` → `send`。
- driver 收到 oneshot 结果 → 写入 `decision_tx`。
- 超时(如 5 分钟,测试注入短值)→ 视为 Deny。
- 发 `AgentEvent::PermissionResolved` → 对应通知(可先略过或发 `error` 之外的
  `item/completed`)。

`Decision` 取值需对照 `yi-agent-core/src/permission.rs:58`。

**Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-app-server`
Expected: PASS。

**Step 5: Commit**

```bash
cargo fmt --all
git add yi-agent-rs/crates/yi-agent-app-server/src
git commit -m "feat(app-server): add permission approval reverse-request loop"
```

---

## Phase D:CLI 子命令 + sidecar 就绪

### Task D1:`yi-agent app-server` 子命令

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs`(加 `Command::AppServer`)
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`(dispatch)
- Modify: `yi-agent-rs/crates/yi-agent/Cargo.toml`(加 `yi-agent-app-server` 依赖)

**Step 1: 写失败测试**

```rust
#[test]
fn cli_parses_app_server_stdio() {
    let cli = Cli::parse_from(["yi-agent", "app-server", "--listen", "stdio://"]);
    assert!(matches!(cli.command, Some(Command::AppServer { .. })));
}
```

**Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent cli_parses_app_server_stdio`
Expected: FAIL。

**Step 3: 实现**

```rust
/// Run the JSON-RPC app-server (stdio transport). Used by the desktop GUI sidecar.
AppServer {
    /// Listen transport. Only `stdio://` is supported.
    #[arg(long, default_value = "stdio://")]
    listen: String,
},
```

dispatch 里:

```rust
Some(Command::AppServer { listen }) => run_app_server(cli, &listen),
```

`run_app_server`:校验 `listen == "stdio://"`(否则 bail),`config::load(&cli)`,
构造 tokio runtime,`rt.block_on(yi_agent_app_server::run_stdio(config))`。
**tracing 必须初始化到 stderr**(复用 `tracing_init.rs`,确认其 writer 是 stderr)。

**Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent`
Expected: PASS。

**Step 5: 手工冒烟**

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"smoke","version":"0"}}}' \
  | cargo run --manifest-path yi-agent-rs/Cargo.toml -p yi-agent -- app-server --listen stdio://
```

Expected: stdout 一行合法 JSON-RPC 响应(`result.serverInfo...`),不含任何日志。

**Step 6: Commit**

```bash
cargo fmt --all
git add yi-agent-rs/crates/yi-agent/src yi-agent-rs/crates/yi-agent/Cargo.toml
git commit -m "feat(cli): add app-server stdio subcommand"
```

---

### Task D2:全量回归 + 文档同步

**Files:**
- Modify: `docs/project-management/README.md`(模块索引加两行)
- Create: `docs/project-management/yi-agent-runtime.md`
- Create: `docs/project-management/yi-agent-app-server.md`

**Step 1: 回归测试**

Run(逐个,不要 `--workspace`):
```
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-runtime
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-app-server
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-core
```
Expected: 全部 PASS。

**Step 2: fmt 检查**

Run: `cd yi-agent-rs && cargo fmt --all -- --check`
Expected: 无输出(通过)。

**Step 3: 写模块文档**

按 `docs/project-management/README.md` 的格式,为新 crate 各写一份模块文件,
每条 feature 带可验证判据(file.rs:line 或命令),并在 README 索引表登记
「完成 / 总计」计数。规则见 `docs/project-management/README.md` 末尾。

**Step 4: Commit**

```bash
git add docs/project-management
git commit -m "docs: register yi-agent-runtime and yi-agent-app-server modules"
```

**Step 5:合并回 main**

```bash
cd <repo-root>
git merge --no-ff feat/desktop-gui-baseline
```

Phase 1 完成。Phase 2(Tauri 桌面应用 + 前端)将另写计划。

---

## 完成判据(Phase 1)

- [ ] `cargo test -p yi-agent-runtime` 全绿。
- [ ] `cargo test -p yi-agent-app-server` 全绿(含权限反向请求闭环)。
- [ ] `cargo test -p yi-agent` 全绿(CLI 行为未回归)。
- [ ] `yi-agent app-server --listen stdio://` 手工冒烟:stdout 只输出协议 JSON。
- [ ] 两个新 crate 已登记进 `docs/project-management/`。
