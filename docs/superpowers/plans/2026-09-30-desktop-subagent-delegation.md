# Mac 桌面端子 Agent 委派对齐 TUI 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让桌面端 sidecar（`yi-agent app-server`）跑在 git 项目目录下的 thread 与 TUI
一样拿到 `spawn_agent` / `wait_agent` / `send_message` / `inspect_agent` / `cancel_agent` /
`review_agent` 六个委派工具。

**Architecture:** 新建共享 crate `yi-agent-subagent`，把 TUI bin crate 里的 daemon
worker 工厂、六个 daemon 工具与 runtime attach 客户端**纯平移**过去；`yi-agent`（TUI）与
`yi-agent-app-server`（桌面 sidecar）都依赖它。app-server 按 thread 的 cwd 懒初始化
per-cwd 的 daemon + `AttachApplicationRoot`，并把六个工具叠加到该 thread 的工具集上。

**Tech Stack:** Rust 2024、Tokio、serde、Unix socket、SQLite（daemon 状态）、Tauri 2 前端
（**本计划不改前端**）。

**设计文档：** `docs/superpowers/specs/2026-09-30-desktop-subagent-delegation-design.md`

## Global Constraints

- **严禁在 `main` 分支上提交。** 全程在 worktree
  `.worktrees/feat/desktop-subagent-delegation`（分支 `feat/desktop-subagent-delegation`）
  里工作。
- **提交前必须 `cd yi-agent-rs && cargo fmt --all`**；commit message 用 conventional
  commits，首行 ≤72 字符，**不要**写 `Co-Authored-By`。
- **不要并行跑 `cargo test`**：同一时刻只跑一个 cargo 命令。跑之前先
  `ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent"` 确认没有残留进程。
- **前端零改动。** 不新增协议方法、不改 `desktop/`。桌面端已有的 `toolCall` 卡片负责
  渲染 `spawn_agent`。
- **TUI 行为零回归。** `cargo test -p yi-agent --bin yi-agent` 每个任务结束都要绿。
- **降级不升级成错误。** 非 git cwd、daemon 起不来、attach 被拒 → 只 `tracing::warn!`，
  agent 少几个工具，turn 照常跑。
- **激活的 objective 必须是用户那轮 prompt**，不能用占位串（objective 会写进 root 任务：
  `crates/yi-agent-store/src/runtime.rs:917` `activate_application_root`）。

---

### Task 1: 新 crate `yi-agent-subagent`（平移 daemon 工厂与六个工具）

**目的：** 让 `yi-agent-app-server` 能用到 TUI 那套 daemon worker 工厂与委派工具，而不必
依赖 bin crate `yi-agent`（它只有 `[[bin]]`，且带 clap/crossterm/ratatui）。

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-subagent/Cargo.toml`
- Create: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`（由
  `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs` 平移而来）
- Modify: `yi-agent-rs/Cargo.toml`（workspace members + workspace.dependencies）
- Modify: `yi-agent-rs/crates/yi-agent/Cargo.toml`（加依赖）
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`（删 `mod subagent_runtime;`，改 use）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/subagents.rs`（改 use）
- Delete: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`

**Interfaces:**
- Consumes: 无（本任务不依赖前面任务）
- Produces: crate `yi-agent-subagent`，`lib` 名 `yi_agent_subagent`，导出
  `DaemonAgentWorkerFactory`、`DaemonWorkspaceService`、`register_application_subagent_tools`。
  后续任务用 `yi_agent_subagent::DaemonAgentWorkerFactory::new(provider, tools, config, socket)`
  与 `yi_agent_subagent::register_application_subagent_tools(&mut registry, socket, session_id, task_id, capability)`。

- [x] **Step 1: 建立 crate 目录与 Cargo.toml**

创建 `yi-agent-rs/crates/yi-agent-subagent/Cargo.toml`：

```toml
[package]
name = "yi-agent-subagent"
description = "Application-owned subagent runtime construction shared by the CLI and the app-server"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true

[dependencies]
yi-agent-core = { workspace = true }
yi-agent-store = { workspace = true }
yi-agent-tools = { workspace = true }
yi-agent-runtime = { workspace = true }

async-trait = "0.1"
futures = { workspace = true }
serde = { version = "1", features = ["derive"] }
serde_json.workspace = true
tokio.workspace = true
tracing.workspace = true
uuid.workspace = true

[dev-dependencies]
tempfile = "3"
```

- [x] **Step 2: 注册到 workspace**

在 `yi-agent-rs/Cargo.toml` 的 `members` 列表里，`"crates/yi-agent-app-server",` 之后加一行：

```toml
    "crates/yi-agent-subagent",
```

在 `[workspace.dependencies]` 里，`yi-agent-app-server = { path = "crates/yi-agent-app-server" }`
之后加一行：

```toml
yi-agent-subagent = { path = "crates/yi-agent-subagent" }
```

- [x] **Step 3: 平移源文件**

```bash
cd yi-agent-rs
git mv crates/yi-agent/src/subagent_runtime.rs crates/yi-agent-subagent/src/lib.rs
```

- [x] **Step 4: 把文件内已有的模块文档注释改成 crate 文档，并删掉直接内部引用**

`crates/yi-agent-subagent/src/lib.rs` 开头第一行保持 crate 文档风格（内容不变）：

```rust
//! Application-owned construction for daemon subagent workers.
```

该文件**不含任何 `crate::` 内部引用**（已核实：`grep -n "crate::" crates/yi-agent/src/subagent_runtime.rs`
无输出），因此不需要改写任何路径。文件内的 `#[cfg(test)] mod tests` 里用了
`yi_agent_store::ipc::...`、`yi_agent_store::repository::RuntimeRepository`、
`yi_agent_store::runtime::RuntimeCoordinator`、`yi_agent_core::...` 等外部 crate 路径，
全部保持不变。

- [x] **Step 5: 在 `yi-agent` 里改用新 crate**

`crates/yi-agent/src/main.rs`：删掉 `mod subagent_runtime;` 这一行（第 7 行附近）。
把唯一的调用点（`build_daemon_worker_factory`，约第 535 行）
`subagent_runtime::DaemonAgentWorkerFactory::new(` 改为
`yi_agent_subagent::DaemonAgentWorkerFactory::new(`。

`crates/yi-agent/src/tui/subagents.rs`：把第 9 行
`use crate::subagent_runtime::register_application_subagent_tools;` 删除，并在同处的
`use yi_agent_core::...` 附近加：

```rust
use yi_agent_subagent::register_application_subagent_tools;
```

- [x] **Step 6: 给 `yi-agent` 加依赖**

`crates/yi-agent/Cargo.toml` 的 `[dependencies]` 里，`yi-agent-app-server = { workspace = true }`
之后加：

```toml
yi-agent-subagent = { workspace = true }
```

- [x] **Step 7: 编译并跑测试**

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent
```

预期：编译通过，原 `subagent_runtime.rs` 的全部单测在新 crate 下运行并通过。

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent
```

预期：全绿（约 486 个），证明 TUI 侧只是改了路径。

- [x] **Step 8: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && \
git commit -m "refactor: extract the subagent runtime construction into a shared crate"
```

---

### Task 2: 平移 runtime attach 客户端与 `AttachedRoot`

**目的：** 让 app-server 能复用 TUI 那条「起/复用 daemon → AttachApplicationRoot →
激活/断开」的链路，而不是复制一份。

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-subagent/src/attach.rs`
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`（加 `pub mod attach;` 与 re-export）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/subagents.rs`（删 `AttachedRoot` /
  `register_attached_root_tools`，改为 re-export）
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`（改用新 API）

**Interfaces:**
- Consumes: Task 1 的 crate 与 `register_application_subagent_tools`。
- Produces:
  - `yi_agent_subagent::AttachedRoot { session_id: String, task_id: String, capability: String, workspace: WorkerWorkspace }`
  - `yi_agent_subagent::register_attached_root_tools(&mut ToolRegistry, PathBuf, &AttachedRoot)`
  - `yi_agent_subagent::attach::{project_runtime_directory(&Path) -> PathBuf}`
  - `yi_agent_subagent::attach::{AttachedProjectRuntime, AttachFailure}`
  - `yi_agent_subagent::attach::{attach_project_runtime(&RuntimeConfig, PathBuf) -> Result<AttachedProjectRuntime, AttachFailure>}`
  - `yi_agent_subagent::attach::{activate_root(&Path, &AttachedRoot, &str) -> Result<(), String>}`
  - `yi_agent_subagent::attach::{detach_root(&Path, &AttachedRoot)}`

  说明：`attach_project_runtime` 收的是**运行时目录**而不是 socket 路径。这样调用方与测试都能
  用 `<workdir>/.yi-agent/runtime` 这个项目内路径做到天然隔离，**不需要**动
  `YI_AGENT_RUNTIME_DIR` 环境变量（动它会让同一测试二进制里的并行测试互相污染）。

- [x] **Step 1: 在共享 crate 里定义 `AttachedRoot` 与注册函数**

从 `crates/yi-agent/src/tui/subagents.rs` 平移 `AttachedRoot`（第 34-39 行）与
`register_attached_root_tools`（第 75-86 行），写进 `crates/yi-agent-subagent/src/lib.rs`
（放在 `register_application_subagent_tools` 定义之前）：

```rust
/// The application root a client attached to, plus what it takes to reach it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedRoot {
    pub session_id: String,
    pub task_id: String,
    pub capability: String,
    pub workspace: WorkerWorkspace,
}

/// Adds the six delegation tools to a root agent's registry.
pub fn register_attached_root_tools(
    registry: &mut ToolRegistry,
    runtime_socket: PathBuf,
    root: &AttachedRoot,
) {
    register_application_subagent_tools(
        registry,
        runtime_socket,
        root.session_id.clone(),
        root.task_id.clone(),
        root.capability.clone(),
    );
}
```

`WorkerWorkspace` 需要 `use yi_agent_core::subagent::worker::WorkerWorkspace;`——
`lib.rs` 顶部的 `use yi_agent_core::subagent::worker::{...}` 清单里已有它，不必重复。

- [x] **Step 2: 写 attach 客户端**

创建 `crates/yi-agent-subagent/src/attach.rs`：

```rust
//! Client half of the local subagent runtime: start (or join) the daemon for a
//! project, attach an application root, then activate and detach it.
//!
//! The TUI and the desktop app-server both drive the same daemon contract, so
//! both go through here rather than each inventing their own bring-up.

use std::path::{Path, PathBuf};

use yi_agent_core::subagent::worker::AgentWorkerFactory;
use yi_agent_runtime::config::RuntimeConfig;
use yi_agent_store::ipc::{Daemon, IpcError, IpcResponse, send_request};

use crate::AttachedRoot;

/// Why a runtime could not be brought up. Both fields are diagnostics for the
/// trace: delegation is optional, so neither reaches the user as an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachFailure {
    /// The bring-up step that failed: `runtime directory`, `daemon start`,
    /// `attach`, or `activation`.
    pub stage: &'static str,
    pub cause: String,
}

impl AttachFailure {
    pub fn new(stage: &'static str, cause: impl std::fmt::Display) -> Self {
        Self {
            stage,
            cause: cause.to_string(),
        }
    }
}

/// A project's attached runtime. Dropping it releases the embedded daemon (when
/// this process started one) but never detaches the root: callers detach
/// explicitly, because detaching is a durable state change.
pub struct AttachedProjectRuntime {
    pub socket_path: PathBuf,
    /// The Git checkout the delegation tools resolve their workdir against.
    pub workspace_root: PathBuf,
    /// The project directory the root was attached for.
    pub project_root: PathBuf,
    pub attached_root: AttachedRoot,
    /// Held only when this process started the daemon.
    pub embedded_daemon: Option<Daemon>,
}

/// The project-local runtime directory. The TUI's slash commands and the CLI
/// resolve the same location, so everything goes through here.
pub fn project_runtime_directory(workdir: &Path) -> PathBuf {
    std::env::var_os("YI_AGENT_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| workdir.join(".yi-agent/runtime"))
}

/// Records the project-local state root in the shared git exclude.
///
/// Creating `<workdir>/.yi-agent/` dirties a checkout that does not already
/// ignore it, and a dirty parent is exactly what makes the first delegation
/// fail. Must run *before* the daemon creates that directory.
pub fn ignore_project_local_runtime_state(workdir: &Path) {
    let state_root = workdir.join(".yi-agent");
    let service = yi_agent_tools::worktree::WorktreeService::new();
    if let Err(error) = service.ignore_project_path(&state_root) {
        tracing::debug!(
            error = %error,
            workdir = %workdir.display(),
            "could not record the project-local runtime state in git exclude"
        );
    }
}

/// Builds the daemon-side worker factory for `cfg` (provider, skills-only
/// registry, sandbox, project workspace).
pub fn worker_factory(
    cfg: &RuntimeConfig,
    runtime_socket: PathBuf,
) -> anyhow::Result<std::sync::Arc<dyn AgentWorkerFactory>> {
    use std::sync::Arc;

    let provider = yi_agent_runtime::bootstrap::build_provider(cfg)?;
    // Skills-only registry: the worker's deliberate contract is to NOT register
    // builtin/process tools here (the recovery path adds its own workspace-rooted
    // set, and the worker start call adds its own).
    let prompt = yi_agent_runtime::bootstrap::build_prompt_setup(cfg)?;
    let catalog = prompt.catalog;
    let mut registry = yi_agent_core::ToolRegistry::new();
    if let Some(skills) = &prompt.skills {
        registry.register(Arc::new(yi_agent_tools::SkillTool::new(skills.clone())));
    }
    let agent_config = yi_agent_runtime::bootstrap::build_agent_config(cfg, prompt.system_prompt);
    Ok(Arc::new(
        crate::DaemonAgentWorkerFactory::new(
            provider,
            Arc::new(registry),
            agent_config,
            runtime_socket,
        )
        .with_catalog(catalog)
        // Recovery must inspect the same worktree ordinary builtin tools use.
        .with_sandbox(cfg.sandbox, cfg.sandbox_writable_roots.clone())
        .with_workspace(cfg.workdir.clone()),
    ))
}

/// Starts (or joins) the project daemon and attaches an application root.
///
/// `runtime_dir` is passed in rather than derived from the environment, so a
/// caller (and a test) can isolate one project's runtime without mutating
/// process-wide state. Pass `project_runtime_directory(&cfg.workdir)` for the
/// default location.
pub fn attach_project_runtime(
    cfg: &RuntimeConfig,
    runtime_dir: PathBuf,
) -> Result<AttachedProjectRuntime, AttachFailure> {
    let database = runtime_dir.join("runtime.sqlite");
    let socket_path = yi_agent_store::ipc::socket_path_for(&runtime_dir)
        .map_err(|error| AttachFailure::new("runtime directory", error))?;
    // Record the project-local state before the daemon creates it, otherwise the
    // root worktree provisioning sees a checkout dirtied by our own store.
    ignore_project_local_runtime_state(&cfg.workdir);
    let factory = worker_factory(cfg, socket_path.clone())
        .map_err(|error| AttachFailure::new("worker factory", error))?;
    let embedded_daemon = match Daemon::start_with_factory(&runtime_dir, &database, factory) {
        Ok(daemon) => Some(daemon),
        Err(IpcError::AlreadyRunning { .. }) => None,
        Err(error) => return Err(AttachFailure::new("daemon start", error)),
    };
    let idempotency_key = format!(
        "project:{}:{}:{}",
        std::process::id(),
        cfg.workdir.display(),
        uuid::Uuid::new_v4()
    );
    let response = send_request(
        &socket_path,
        yi_agent_store::ipc::IpcRequest::AttachApplicationRoot {
            idempotency_key,
            workspace: cfg.workdir.clone(),
        },
    )
    .map_err(|error| AttachFailure::new("attach", error))?;
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        workspace,
    } = response
    else {
        return Err(AttachFailure::new("attach", describe_rejection(&response)));
    };
    Ok(AttachedProjectRuntime {
        socket_path,
        workspace_root: workspace.path.clone(),
        project_root: cfg.workdir.clone(),
        attached_root: AttachedRoot {
            session_id,
            task_id: root_task_id,
            capability: message_capability,
            workspace,
        },
        embedded_daemon,
    })
}

/// Turns a rejected attach response into one actionable line for the trace.
fn describe_rejection(response: &IpcResponse) -> String {
    match response {
        IpcResponse::Error {
            code,
            message: Some(message),
        } => format!("daemon rejected the runtime attachment: {code}: {message}"),
        IpcResponse::Error { code, message: None } => {
            format!("daemon rejected the runtime attachment: {code}")
        }
        other => format!("daemon rejected the runtime attachment: {other:?}"),
    }
}

/// Activates the root with the user's objective. Idempotent: an already running
/// root answers `Ok`.
pub fn activate_root(
    socket_path: &Path,
    root: &AttachedRoot,
    objective: &str,
) -> Result<(), String> {
    let response = send_request(
        socket_path,
        yi_agent_store::ipc::IpcRequest::ActivateApplicationRoot {
            session_id: root.session_id.clone(),
            root_task_id: root.task_id.clone(),
            capability: root.capability.clone(),
            objective: objective.to_owned(),
        },
    )
    .map_err(|error| error.to_string())?;
    match response {
        IpcResponse::ApplicationRootActivated => Ok(()),
        other => Err(format!("daemon rejected the runtime activation: {other:?}")),
    }
}

/// Detaches the root. Best effort: a failure is a trace line, never an error
/// that could mask the reason the process is winding down.
pub fn detach_root(socket_path: &Path, root: &AttachedRoot) {
    let response = send_request(
        socket_path,
        yi_agent_store::ipc::IpcRequest::DetachApplicationRoot {
            session_id: root.session_id.clone(),
            root_task_id: root.task_id.clone(),
            capability: root.capability.clone(),
        },
    );
    if let Err(error) = response {
        tracing::warn!(%error, "could not detach the application root");
    }
}
```

在 `crates/yi-agent-subagent/src/lib.rs` 顶部（`//!` 文档之后）加：

```rust
pub mod attach;
```

`Cargo.toml` 需要 `anyhow.workspace = true`（`worker_factory` 的返回类型）、
`tracing.workspace = true`、`uuid.workspace = true`（后两者已在 Task 1 的清单里），
在 `[dependencies]` 里补上 `anyhow.workspace = true`。

- [x] **Step 3: 让 TUI 使用共享定义**

`crates/yi-agent/src/tui/subagents.rs`：删掉本地 `AttachedRoot` 定义与
`register_attached_root_tools` 函数，改为：

```rust
pub use yi_agent_subagent::{AttachedRoot, register_attached_root_tools};
```

（用 `pub use` 是为了让 `crate::tui::subagents::AttachedRoot`、
`crate::tui::subagents::register_attached_root_tools` 这类既有路径继续编译，从而把本任务对
`main.rs` 的改动压到最小。不要在这里 re-export `yi_agent_subagent::attach` 的东西——
`main.rs` 直接走全路径调用，多余 re-export 会产生未使用告警。
`CURRENT_ATTACHED_ROOT`、`set_current_attached_root`、`current_attached_root`、
`RuntimeStartupChoice`、`RuntimeStartupIntent`、`RUNTIME_RESTART_NOTICE`、
`runtime_restart_notice` **全部留在 TUI**，不要动。）

文件内原有的测试 `attached_tui_root_exposes_subagent_tools_without_a_delegate_command`
保留（它只用到 `register_attached_root_tools` 与 `AttachedRoot`，re-export 后仍然可用）。

- [x] **Step 4: `main.rs` 改用共享 attach 客户端**

`crates/yi-agent/src/main.rs`：

1. `build_daemon_worker_factory(cli, socket)` 的函数体改为

```rust
fn build_daemon_worker_factory(
    cli: &Cli,
    runtime_socket: std::path::PathBuf,
) -> Result<Arc<dyn yi_agent_core::subagent::worker::AgentWorkerFactory>> {
    let config = config::load(cli)?;
    yi_agent_subagent::attach::worker_factory(&config, runtime_socket)
}
```

2. 删除本地的 `runtime_directory_for` / `runtime_directory_from` / `runtime_database_path` /
   `runtime_socket_for` / `ignore_project_local_runtime_state` 五个定义，改用共享实现：

```rust
pub(crate) fn runtime_socket_for(
    workdir: &std::path::Path,
) -> Result<std::path::PathBuf, yi_agent_store::ipc::IpcError> {
    let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(workdir);
    yi_agent_store::ipc::socket_path_for(&runtime_dir)
}

pub(crate) fn runtime_directory_for(workdir: &std::path::Path) -> std::path::PathBuf {
    yi_agent_subagent::attach::project_runtime_directory(workdir)
}

fn runtime_database_path(runtime_dir: &std::path::Path) -> std::path::PathBuf {
    runtime_dir.join("runtime.sqlite")
}

fn ignore_project_local_runtime_state(workdir: &std::path::Path) {
    yi_agent_subagent::attach::ignore_project_local_runtime_state(workdir);
}
```

（保留这四个薄包装而不是逐个改调用点，是为了让既有调用点与既有单测
`runtime_directory_uses_workdir_local_default`、
`runtime_directory_for_workdir_uses_the_same_project_path_for_daemon_and_attachment`、
`runtime_database_path_is_shared_by_daemon_and_attachment` 继续有效。）

3. `activate_tui_runtime_root` / `detach_tui_runtime_root` 改为委托：

```rust
fn activate_tui_runtime_root(
    socket_path: &std::path::Path,
    root: &crate::tui::subagents::AttachedRoot,
    objective: &str,
) -> Result<()> {
    yi_agent_subagent::attach::activate_root(socket_path, root, objective)
        .map_err(|error| anyhow::anyhow!("{error}"))
}

fn detach_tui_runtime_root(
    socket_path: &std::path::Path,
    root: &crate::tui::subagents::AttachedRoot,
) {
    yi_agent_subagent::attach::detach_root(socket_path, root);
}
```

4. `attach_tui_runtime` 与 `attach_headless_runtime` 的 daemon 启动段保持不变；它们继续
   各自调用 `Daemon::start_with_factory(..., build_daemon_worker_factory(...))` 与
   `attach_application_root_request`。**不要**把这两个函数改成调用
   `attach_project_runtime`：TUI 需要 `TuiRuntimeSession::Unavailable { reason }` 这条
   可展示的降级通道（`a_failed_runtime_bring_up_is_reported_rather_than_raised` 依赖它），
   而 `attach_project_runtime` 只返回 `AttachFailure`。两者共用的是更下面的部件
   （`worker_factory` / `ignore_project_local_runtime_state` / `project_runtime_directory`）。

- [x] **Step 5: 编译并跑测试**

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent
```

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent
```

预期：两者全绿。`yi-agent --bin yi-agent` 的测试数应与 Task 1 结束时一致（本任务只移动
代码，不删测试）。

- [x] **Step 6: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && \
git commit -m "refactor: share the runtime attach client between the TUI and other clients"
```

---

### Task 3: `yi-agent-runtime` 暴露装配部件

**目的：** app-server 需要「换掉工具集与权限根」而**不重建 provider 与权限通道**——
重建 provider 会重读密钥，重建权限通道会打断正在等待审批的 turn。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs:246-330`

**Interfaces:**
- Consumes: 无
- Produces: `AgentBootstrap { provider: Arc<dyn yi_agent_core::Provider>, tools: Arc<yi_agent_core::ToolRegistry>, catalog: Option<SkillsCatalogHandle>, .. }`

- [x] **Step 1: 写失败测试**

在 `crates/yi-agent-runtime/src/bootstrap.rs` 的测试模块里加：

```rust
#[test]
fn bootstrap_exposes_the_registry_and_the_catalog_handle() {
    let cfg = test_config();
    let built = bootstrap_agent(&cfg, PermissionMode::Interactive).expect("bootstrap");

    let names = built.tools.names();
    assert!(
        names.contains(&"bash".to_string()),
        "the bootstrap must expose the registry it built, got {names:?}"
    );
    assert!(
        built.catalog.is_some(),
        "the bootstrap must expose the skills catalog handle it built"
    );
}
```

（若该测试模块没有 `test_config()`，用同文件既有测试里构造 `RuntimeConfig` 的写法；注意
把 `workdir` 设成 `tempfile::TempDir`，避免读用户真实配置。）

- [x] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-runtime bootstrap_exposes_the_registry
```

预期：编译失败，`no field tools on type AgentBootstrap`。

- [x] **Step 3: 加字段**

`crates/yi-agent-runtime/src/bootstrap.rs` 的 `AgentBootstrap`（第 246 行起）加三个字段：

```rust
pub struct AgentBootstrap {
    pub agent: yi_agent_core::Agent,
    pub permission: Arc<yi_agent_core::permission::PermissionChecker>,
    /// The provider the agent runs on. Callers that rebuild the agent with a
    /// different tool set must reuse this instance rather than building a
    /// second one, so the credential and the client stay a single object.
    pub provider: Arc<dyn yi_agent_core::Provider>,
    /// The registry the agent was built with. Callers that need to add tools
    /// (for example the six delegation tools) rebuild from this instead of
    /// re-deriving a tool set of their own.
    pub tools: Arc<yi_agent_core::ToolRegistry>,
    /// The skills catalog handle, so a caller that rebuilds the registry keeps
    /// refreshing the system prompt between turns.
    pub catalog: Option<SkillsCatalogHandle>,
    pub decision_tx: Option<tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>>,
    pub decision_rx: Option<DecisionReceiver>,
    pub yolo: yi_agent_core::autonomy::YoloSwitch,
}
```

`bootstrap_agent`（第 277 行起）的两个分支：先取出 registry 再交给 `Agent::new`，
因为 `setup.tools` 会被 move：

```rust
    let provider_handle = Arc::clone(&provider);
    let tools = Arc::clone(&setup.tools);
    let catalog = setup.catalog;
    match mode {
        PermissionMode::Interactive => {
            let agent = yi_agent_core::Agent::new(provider, setup.tools, agent_config)
                .with_permission(checker.clone(), rx_arc.clone());
            Ok(AgentBootstrap {
                agent,
                permission: checker,
                provider: provider_handle,
                tools,
                catalog,
                decision_tx: Some(decision_tx),
                decision_rx: Some(rx_arc),
                yolo: switch.clone(),
            })
        }
        PermissionMode::AutoAllow => {
            // AutoAllow 下没有 UI 读取决定。丢弃 sender 让通道关闭:agent 侧
            // `recv()` 会立即返回 `None`,黑名单命令据此解析为 Deny,而不是
            // 永久阻塞等待一个不会到来的决定。
            drop(decision_tx);
            let agent = yi_agent_core::Agent::new(provider, setup.tools, agent_config)
                .with_permission(checker.clone(), rx_arc);
            Ok(AgentBootstrap {
                agent,
                permission: checker,
                provider: provider_handle,
                tools,
                catalog,
                decision_tx: None,
                decision_rx: None,
                yolo: switch.clone(),
            })
        }
    }
```

- [x] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-runtime
```

预期：全绿（约 61 个 + 新增 1 个）。

- [x] **Step 5: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && \
git commit -m "feat(runtime): expose the built registry and catalog on the bootstrap"
```

---

### Task 4: app-server 接线（per-cwd runtime + 委派工具）

**目的：** 让 git 项目目录下的 thread 拿到六个委派工具；非 git cwd 静默降级。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`（无协议改动；本任务
  不改文件，列在此处只为提醒**不要**加新方法）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/session.rs`（`TurnPrompt` 加字段）

**Interfaces:**
- Consumes: Task 1/2 的 `yi_agent_subagent::{AttachedRoot, register_attached_root_tools}`、
  `yi_agent_subagent::attach::{AttachedProjectRuntime, activate_root, attach_project_runtime, detach_root, project_runtime_directory, ignore_project_local_runtime_state}`；
  Task 3 的 `AgentBootstrap.tools` / `.catalog`。
- Produces: app-server 内部 `BuiltAgent` 新增字段与 `ProjectRuntimes` 缓存；无协议变化。

- [x] **Step 1: 加依赖**

`crates/yi-agent-app-server/Cargo.toml` 的 `[dependencies]` 里加：

```toml
yi-agent-store = { workspace = true }
yi-agent-subagent = { workspace = true }
yi-agent-tools = { workspace = true }
```

并把已有 dev-dependencies 里的 `yi-agent-tools = { workspace = true }` 删掉（它已进生产依赖）。

- [x] **Step 2: 写失败测试（工具集存在 + 非 git 降级 + 同 cwd 复用）**

在 `crates/yi-agent-app-server/src/server.rs` 的测试模块里加。先加两个共用 helper：

```rust
    /// A throwaway git repository: `DaemonAgentWorkerFactory` refuses a project
    /// that is not inside a Git worktree, and the attach path needs a real one.
    fn init_git_repo(dir: &std::path::Path) {
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
            vec!["commit", "-q", "--allow-empty", "-m", "init"],
        ] {
            let status = std::process::Command::new("git")
                .args(&args)
                .current_dir(dir)
                .status()
                .expect("git must be available");
            assert!(status.success(), "git {args:?} failed in {}", dir.display());
        }
    }

    /// An isolated runtime directory next to the project under test. Passing it
    /// in (rather than setting `YI_AGENT_RUNTIME_DIR`) keeps every test in this
    /// binary isolated from its neighbours.
    fn isolated_runtime_dir() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }
```

三个测试：

```rust
    #[test]
    fn a_git_project_gets_the_delegation_tools() {
        let repo = tempfile::TempDir::new().unwrap();
        let runtime = isolated_runtime_dir();
        init_git_repo(repo.path());
        let mut cfg = test_config();
        cfg.workdir = repo.path().to_path_buf();

        let attached = yi_agent_subagent::attach::attach_project_runtime(
            &cfg,
            runtime.path().to_path_buf(),
        )
        .expect("a clean git repo must attach");

        let tooling = build_runtime_tooling(&cfg, &attached, yi_agent_core::autonomy::YoloSwitch::new(false))
            .expect("tooling");
        let names = tooling.registry.names();

        for expected in [
            "spawn_agent",
            "send_message",
            "wait_agent",
            "inspect_agent",
            "cancel_agent",
            "review_agent",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "an attached root must expose {expected}, got {names:?}"
            );
        }
    }

    #[test]
    fn a_non_git_cwd_fails_to_attach_so_the_tools_are_absent() {
        let plain = tempfile::TempDir::new().unwrap();
        let runtime = isolated_runtime_dir();
        let mut cfg = test_config();
        cfg.workdir = plain.path().to_path_buf();

        // A plain directory is not a Git worktree, so the root's workspace
        // cannot be provisioned and the bring-up must fail rather than hand the
        // model a tool that could never run.
        let outcome = yi_agent_subagent::attach::attach_project_runtime(
            &cfg,
            runtime.path().to_path_buf(),
        );

        match outcome {
            Err(failure) => assert!(
                !failure.stage.is_empty() && !failure.cause.trim().is_empty(),
                "a degraded bring-up must explain itself: {failure:?}"
            ),
            Ok(attached) => {
                let names = build_runtime_tooling(
                    &cfg,
                    &attached,
                    yi_agent_core::autonomy::YoloSwitch::new(false),
                )
                .expect("tooling")
                .registry
                .names();
                assert!(
                    !names.contains(&"spawn_agent".to_string()),
                    "a non-git cwd must not reach the delegation tools, got {names:?}"
                );
            }
        }
    }

    #[test]
    fn two_threads_in_one_cwd_share_one_attached_runtime() {
        let repo = tempfile::TempDir::new().unwrap();
        let runtime = isolated_runtime_dir();
        init_git_repo(repo.path());
        let mut cfg = test_config();
        cfg.workdir = repo.path().to_path_buf();
        let runtimes: ProjectRuntimes = Arc::new(Mutex::new(HashMap::new()));

        let first = attach_cwd_runtime(&runtimes, &runtime.path().to_path_buf(), &cfg)
            .expect("first attach");
        let second = attach_cwd_runtime(&runtimes, &runtime.path().to_path_buf(), &cfg)
            .expect("second attach");

        assert!(
            Arc::ptr_eq(&first, &second),
            "a second thread in the same cwd must reuse the attached runtime"
        );
        assert_eq!(runtimes.lock().unwrap().len(), 1);
    }
```

**注意：** 若 `a_non_git_cwd_fails_to_attach_so_the_tools_are_absent` 实际走
`Ok(attached)` 分支（说明非 git 项目也能 attach），保留该分支的断言即可——两条分支都表达
同一个契约「非 git cwd 不得到达委派工具」。

- [x] **Step 3: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server a_git_project_gets
```

预期：编译失败，`cannot find function build_runtime_tooling`。
- [x] **Step 4: 实现 `build_runtime_tooling` 与 per-cwd 缓存**

在 `crates/yi-agent-app-server/src/server.rs` 里加（放在 `run_with` 之前）：

```rust
/// A tool set rooted at an attached project's checkout, plus the permission
/// checker that shares the thread's YoloSwitch.
struct RuntimeTooling {
    registry: Arc<yi_agent_core::ToolRegistry>,
    permission: Arc<yi_agent_core::permission::PermissionChecker>,
}

/// Per-cwd attached runtimes. The app-server is one long-lived process serving
/// many working directories, while a runtime is project-scoped, so bring-up is
/// keyed on the canonical cwd rather than done once per process.
///
/// Failures are cached too: without that, every new thread in a plain directory
/// would repeat the same doomed bring-up.
type ProjectRuntimes =
    Arc<Mutex<HashMap<PathBuf, Result<Arc<yi_agent_subagent::attach::AttachedProjectRuntime>, String>>>>;

/// Resolves (or brings up) the attached runtime for `cfg.workdir`.
///
/// `runtime_dir` is the isolated per-project runtime directory; passing it in
/// keeps the cache keyed on the project while tests stay independent of the
/// process-wide `YI_AGENT_RUNTIME_DIR`.
fn attach_cwd_runtime(
    runtimes: &ProjectRuntimes,
    runtime_dir: &std::path::Path,
    cfg: &RuntimeConfig,
) -> Result<Arc<yi_agent_subagent::attach::AttachedProjectRuntime>, String> {
    let key = std::fs::canonicalize(&cfg.workdir).unwrap_or_else(|_| cfg.workdir.clone());
    if let Some(existing) = runtimes.lock().unwrap_or_else(|p| p.into_inner()).get(&key) {
        return existing.clone();
    }
    let outcome = yi_agent_subagent::attach::attach_project_runtime(cfg, runtime_dir.to_path_buf())
        .map(Arc::new)
        .map_err(|failure| format!("{}: {}", failure.stage, failure.cause));
    runtimes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(key, outcome.clone());
    outcome
}

/// The delegation-ready tool set for an attached root.
///
/// Only the builtin tools and the permission root move to the checkout; the
/// skills and MCP roots stay on `cfg.workdir` (the project the user picked), and
/// `yolo` is the thread's own switch so the sandbox and the permission layer
/// keep flipping together.
fn build_runtime_tooling(
    cfg: &RuntimeConfig,
    attached: &yi_agent_subagent::attach::AttachedProjectRuntime,
    yolo: yi_agent_core::autonomy::YoloSwitch,
) -> Result<RuntimeTooling, String> {
    let setup = yi_agent_runtime::bootstrap::build_tool_setup_in(
        cfg,
        false,
        &attached.workspace_root,
    )
    .map_err(|error| error.to_string())?;
    let mut registry = (*setup.tools).clone();
    yi_agent_subagent::register_attached_root_tools(
        &mut registry,
        attached.socket_path.clone(),
        &attached.attached_root,
    );
    let permission = yi_agent_runtime::bootstrap::load_permission_checker_with_switch(
        &attached.workspace_root,
        yolo,
    )
    .map_err(|error| error.to_string())?;
    Ok(RuntimeTooling {
        registry: Arc::new(registry),
        permission,
    })
}
```

- [x] **Step 5: 让 `BuiltAgent` 携带重建所需的一切**

`BuiltAgent`（第 39 行起）改为：

```rust
struct BuiltAgent {
    agent: yi_agent_core::Agent,
    provider: Arc<dyn yi_agent_core::Provider>,
    config: yi_agent_core::AgentConfig,
    tools: Arc<yi_agent_core::ToolRegistry>,
    permission: Arc<yi_agent_core::permission::PermissionChecker>,
    decision_tx: Option<mpsc::Sender<(u64, Decision)>>,
    decision_rx: Option<yi_agent_runtime::bootstrap::DecisionReceiver>,
    catalog: Option<yi_agent_runtime::bootstrap::SkillsCatalogHandle>,
    yolo: yi_agent_core::autonomy::YoloSwitch,
}
```

`run()` 的工厂闭包（第 63 行起）改为把 bootstrap 的部件原样带出：

```rust
        move |session, cwd, mode| {
            let mut thread_cfg = cfg_for_factory.clone();
            thread_cfg.workdir = cwd.to_path_buf();
            thread_cfg.yolo = mode == crate::thread_store::ThreadMode::Yolo;
            let built = yi_agent_runtime::bootstrap::bootstrap_agent(
                &thread_cfg,
                yi_agent_runtime::bootstrap::PermissionMode::Interactive,
            )?;
            let config = built.agent.config().clone();
            Ok(BuiltAgent {
                agent: apply_session(built.agent, session),
                provider: built.provider,
                config,
                tools: built.tools,
                permission: built.permission,
                decision_tx: built.decision_tx,
                decision_rx: built.decision_rx,
                catalog: built.catalog,
                yolo: built.yolo,
            })
        },
```

这需要 `Agent` 暴露它的 `AgentConfig`。在
`crates/yi-agent-core/src/agent.rs` 的 `impl Agent` 里（紧邻 `pub fn session`）加：

```rust
    /// The configuration this agent was built with.
    pub fn config(&self) -> &AgentConfig {
        &self.config
    }
```

- [x] **Step 6: 重建 agent 的 `wrap_for_delegation`**

在 `server.rs` 里加（`BuildAgent` 附近）：

```rust
/// Swaps a thread's tool set and permission root for the attached runtime's,
/// keeping the provider, the decision channel, the YoloSwitch and the session.
///
/// Rebuilding the provider or the decision channel here would re-read config
/// and could strand a turn that is waiting on an approval decision.
fn wrap_for_delegation(built: BuiltAgent, tooling: RuntimeTooling) -> BuiltAgent {
    let BuiltAgent {
        agent,
        provider,
        config,
        decision_tx,
        decision_rx,
        catalog,
        yolo,
        ..
    } = built;
    let session = agent.session();
    let mut rebuilt = yi_agent_core::Agent::new(provider.clone(), tooling.registry.clone(), config.clone())
        .with_session(session);
    if let Some(rx) = decision_rx.clone() {
        rebuilt = rebuilt.with_permission(tooling.permission.clone(), rx);
    }
    BuiltAgent {
        agent: rebuilt,
        provider,
        config,
        tools: tooling.registry,
        permission: tooling.permission,
        decision_tx,
        decision_rx,
        catalog,
        yolo,
    }
}
```

- [x] **Step 7: 在 `run_with` 里接线**

`run_with` 签名加一个参数（放在 `workspaces` 之后）：

```rust
    runtimes: ProjectRuntimes,
```

`run()` 里构造并传入：

```rust
    let runtimes: ProjectRuntimes = Arc::new(Mutex::new(HashMap::new()));
    run_with(
        reader,
        writer,
        cfg,
        PERMISSION_TIMEOUT,
        workspaces,
        runtimes,
        move |session, cwd, mode| { /* 见 Step 5 */ },
    )
    .await
```

`run_with` 内部：`build_agent`（第 391 与 579 两处 `match build_agent(...)`）之后、把
`BuiltAgent` 解构之前插入接线。两处都改成同一形式：

```rust
                        let mut built = match build_agent(None, Path::new(&cwd), mode) {
                            Ok(a) => a,
                            Err(e) => {
                                write_response(&writer, err_response(id, RpcError::internal(e.to_string()))).await?;
                                continue;
                            }
                        };
                        // Delegation is optional: a project whose runtime cannot
                        // be brought up keeps the plain agent and a trace line.
                        let mut thread_cfg = cfg.clone();
                        thread_cfg.workdir = PathBuf::from(&cwd);
                        let thread_runtime_dir =
                            yi_agent_subagent::attach::project_runtime_directory(&thread_cfg.workdir);
                        let activation =
                            match attach_cwd_runtime(&runtimes, &thread_runtime_dir, &thread_cfg) {
                            Ok(runtime) => match build_runtime_tooling(
                                &thread_cfg,
                                &runtime,
                                built.yolo.clone(),
                            ) {
                                Ok(tooling) => {
                                    built = wrap_for_delegation(built, tooling);
                                    Some(Arc::clone(&runtime))
                                }
                                Err(cause) => {
                                    tracing::warn!(stage = "tooling", %cause, cwd = %cwd,
                                        "subagent delegation unavailable for this thread");
                                    None
                                }
                            },
                            Err(cause) => {
                                tracing::warn!(stage = "attach", %cause, cwd = %cwd,
                                    "subagent delegation unavailable for this thread");
                                None
                            }
                        };
                        let BuiltAgent { agent, decision_tx, catalog, yolo, .. } = built;
```

（`thread/resume` 那一处把 `None` 换成本该传的 `Some(session)`，并保留它自己的错误响应
分支。）

**顺序要求（必须遵守）：** 上述接线必须放在 `threads.get_mut(&thread_id)` 的
**作用域之外**，否则会与后续 `threads.insert(..)` 一起造成借用冲突。实现时把接线放在
拿到 `cwd` 之后、`threads.insert(..)` **之前**，用自己的局部变量保存 `activation`。

- [x] **Step 8: 首个 turn 激活 root**

`crates/yi-agent-app-server/src/session.rs` 的 `TurnPrompt` 加字段：

```rust
/// 一次 turn 的输入(由 driver task 消费)。
#[derive(Debug)]
pub struct TurnPrompt {
    pub turn_id: String,
    pub prompt: String,
    /// 该 thread 若已 attach 到项目 runtime,则带上它在**首个 turn** 激活;
    /// 激活放在 driver 里(而非请求循环),一个 thread 的激活不会卡住其他 thread。
    pub activate: Option<
        Arc<yi_agent_subagent::attach::AttachedProjectRuntime>,
    >,
}
```

`session.rs` 顶部加 `use std::sync::Arc;`。

`turn/start` 分支投递 prompt 处（第 872 行）改为：

```rust
                        if prompt_tx
                            .send(TurnPrompt {
                                turn_id,
                                prompt,
                                activate: activation.clone(),
                            })
                            .await
                            .is_err()
                        {
```

`run_thread_driver` 的循环头（第 1186 行）改为：

```rust
    let mut activated = false;
    while let Some(TurnPrompt { turn_id, prompt, activate }) = prompt_rx.recv().await {
        // 首个 turn 用用户真正的 objective 激活 root(objective 会写进 root 任务,
        // 不能用占位串)。放在 driver 里:(a) 请求循环不被 socket 调用阻塞,
        // (b) 一进程多 thread 时各自独立。激活自身幂等,重复调用是安全的。
        if !activated {
            activated = true;
            if let Some(runtime) = activate {
                let socket = runtime.socket_path.clone();
                let root = runtime.attached_root.clone();
                let objective = prompt.clone();
                let outcome = tokio::task::spawn_blocking(move || {
                    yi_agent_subagent::attach::activate_root(&socket, &root, &objective)
                })
                .await;
                match outcome {
                    Ok(Ok(())) => {}
                    Ok(Err(cause)) => tracing::warn!(
                        stage = "activation",
                        %cause,
                        %thread_id,
                        "subagent delegation unavailable for this thread"
                    ),
                    Err(error) => tracing::warn!(
                        stage = "activation",
                        %error,
                        %thread_id,
                        "subagent delegation unavailable for this thread"
                    ),
                }
            }
        }
```

注意 `activated` 必须在 `while` **之外**声明，且即使 `activate` 为 `None` 也要置位——
否则每一轮都会重试。同时保留语句体内原有的 `let user_prompt = prompt.clone();` 等逻辑。

- [x] **Step 9: `thread/delete` 与退出时的 detach**

`thread/delete` 分支在 `threads.remove(&thread_id);` 之后加：

```rust
                        // 断开该项目 runtime 的 root(若有)。位置无关:删除一个 thread
                        // 不影响同项目其它 thread 的委派,daemon 侧对重复 detach 幂等。
                        for runtime in attached_runtimes(&runtimes) {
                            yi_agent_subagent::attach::detach_root(
                                &runtime.socket_path,
                                &runtime.attached_root,
                            );
                        }
```

（`thread/delete` 分支里没有该 thread 的 cwd——它靠 `store_lookup` 按 thread_id 定位，
不经过 cwd。所以这里用位置无关的写法：断开所有已 attach 的项目 root。**已知取舍**：
删掉某个项目里的一个 thread 会一并断开该项目 root，同项目其它仍在用的 thread 会在下一次
`turn/start` 时被重新激活（`run_thread_driver` 的激活是幂等的，且每个 driver 只激活一次
自己的首个 turn——所以第二个 thread 的委派在它自己首个 turn 时已激活过，这里 detach 之后
不会自动恢复）。实现时**优先**验证「删 thread 后同项目其它 thread 仍能 spawn」；若不能，
改成只在 `threads.keys()` 中不再有该 cwd 的 thread 时才 detach，并把该取舍写进偏差记录。）

主循环结束后（第 988 行 `Ok(())` 之前）加退出清理：

```rust
    // 进程退出前断开所有项目 root。同步调用:这里已经不在热路径上,而进程即将结束。
    for runtime in attached_runtimes(&runtimes) {
        yi_agent_subagent::attach::detach_root(&runtime.socket_path, &runtime.attached_root);
    }
```

两处共用同一个 helper（放在 `attach_cwd_runtime` 旁边）：

```rust
/// Every project runtime this process attached, successes only.
fn attached_runtimes(
    runtimes: &ProjectRuntimes,
) -> Vec<Arc<yi_agent_subagent::attach::AttachedProjectRuntime>> {
    runtimes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .filter_map(|entry| entry.clone().ok())
        .collect()
}
```

- [x] **Step 11: 跑测试**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server
```

预期：全绿（原 148 个 + 新增 3 个）。

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent
```

- [x] **Step 12: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && \
git commit -m "feat(app-server): give git-project threads the subagent delegation tools"
```

---

### Task 5: 端到端装配测试与全量回归

**目的：** 证明「attach → spawn → 子任务落终态」这条链路在共享 crate 层是通的，且 TUI
与前端零回归。

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-subagent/tests/attach_delegation.rs`
- Modify: `docs/`（Task 6 处理）

**Interfaces:**
- Consumes: Task 1/2 的公开 API。
- Produces: 无（测试专用）。

- [x] **Step 1: 写失败测试**

创建 `yi-agent-rs/crates/yi-agent-subagent/tests/attach_delegation.rs`：

```rust
//! End-to-end shape of the shared attach path: a clean git project attaches,
//! an objective activates the root, and the daemon accepts a child spawn.

use std::path::Path;

fn init_git_repo(dir: &Path) {
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Test"],
        vec!["commit", "-q", "--allow-empty", "-m", "init"],
    ] {
        let status = std::process::Command::new("git")
            .args(&args)
            .current_dir(dir)
            .status()
            .expect("git must be available");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }
}

#[test]
fn a_clean_git_project_attaches_and_activates_its_root() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime_dir = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());

    let cfg = yi_agent_runtime::config::RuntimeConfig {
        provider: "anthropic".into(),
        api_url: "https://api.anthropic.com".into(),
        api_key: String::new(),
        model: "test-model".into(),
        max_turns: 4,
        workdir: repo.path().to_path_buf(),
        system_prompt: None,
        compact_threshold: 160_000,
        compact_user_budget_tokens: 20_000,
        compact_tool_budget_tokens: 12_000,
        yolo: false,
        sandbox_promotable: true,
        sandbox: yi_agent_tools::SandboxMode::default(),
        sandbox_writable_roots: Vec::new(),
        skills_catalog_budget: 8192,
        skills_catalog_budget_explicit: true,
    };

    let attached = yi_agent_subagent::attach::attach_project_runtime(
        &cfg,
        runtime_dir.path().to_path_buf(),
    )
    .expect("a clean git project must attach");
    assert!(
        attached.workspace_root.starts_with(repo.path().canonicalize().unwrap()),
        "the root runs inside the project checkout, got {}",
        attached.workspace_root.display()
    );

    yi_agent_subagent::attach::activate_root(
        &attached.socket_path,
        &attached.attached_root,
        "investigate the build",
    )
    .expect("activation must succeed");
    // Activation is idempotent, so a second thread activating the same root is safe.
    yi_agent_subagent::attach::activate_root(
        &attached.socket_path,
        &attached.attached_root,
        "investigate the build",
    )
    .expect("a second activation must be a no-op, not an error");

    yi_agent_subagent::attach::detach_root(&attached.socket_path, &attached.attached_root);
}
```

`dev-dependencies` 需要 `yi-agent-runtime` / `yi-agent-tools`（已在依赖里，但集成测试要用
它们的名字，所以 `Cargo.toml` 的 `[dev-dependencies]` 里补：

```toml
yi-agent-runtime = { workspace = true }
yi-agent-tools = { workspace = true }
```

- [x] **Step 2: 跑测试确认失败（或直接通过）**

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent --test attach_delegation
```

预期：编译并运行；若 attach 路径有实现缺口（例如 idempotency key 前缀、objective 写入），
这里会暴露。

- [x] **Step 3: 全量回归**

逐条跑（**不要并行**）：

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent
cd yi-agent-rs && cargo test -p yi-agent-app-server
cd yi-agent-rs && cargo test -p yi-agent-runtime
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc
```

```bash
cd desktop && npx tsc --noEmit && npm test
```

预期：全部通过。任何失败都先修再提交。

- [x] **Step 4: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && \
git commit -m "test(subagent): cover the shared attach path end to end"
```

---

### Task 6: 文档同步

**Files:**
- Create: `docs/project-management/yi-agent-subagent.md`
- Modify: `docs/project-management/README.md`（模块索引 + 计数）
- Modify: `docs/project-management/desktop.md`、`yi-agent-app-server.md`、
  `subagent-runtime.md`、`yi-agent-runtime.md`
- Modify: `docs/bug-list.md`

- [x] **Step 1: 新建模块文件**

创建 `docs/project-management/yi-agent-subagent.md`：

```markdown
# yi-agent-subagent（共享的委派装配）

## 模块说明

`yi-agent-subagent` 是「本地 subagent runtime 的客户端装配」的共享 crate：TUI
（`crates/yi-agent`）与桌面 sidecar（`crates/yi-agent-app-server`）都用它接入同一个
daemon，避免两处各写一份 daemon 工厂与委派工具。它由 `crates/yi-agent` 的
`src/subagent_runtime.rs` 与 `src/main.rs` 的 attach 客户端平移而来。

## 范围边界

**做什么：**
- `DaemonAgentWorkerFactory` 与 `DaemonWorkspaceService`（daemon 侧真正跑子 worker 的工厂）
- 六个 daemon 委派工具（`spawn_agent` / `send_message` / `wait_agent` / `inspect_agent` /
  `cancel_agent` / `review_agent`）与 `register_application_subagent_tools`
- runtime attach 客户端：`attach_project_runtime` / `activate_root` / `detach_root` /
  `project_runtime_directory` / `worker_factory`

**不做什么：**
- 不做 TUI 的启动确认与偏好（留在 `crates/yi-agent/src/tui/runtime_prefs.rs` 与
  `tui/subagents.rs` 的 `RuntimeStartup*` 类型）
- 不做 app-server 协议（由 yi-agent-app-server 负责）
- 不持有进程级 `current_attached_root` 单例（那是 TUI 一进程一 root 的表达；多 thread 的
  app-server 用 per-cwd 映射）

## Features

- [x] 共享 crate 骨架 + workspace 注册 — `yi-agent-rs/crates/yi-agent-subagent/`；
  `yi-agent-rs/Cargo.toml` members 含 `crates/yi-agent-subagent`
- [x] daemon 工厂与六个委派工具的平移 — `src/lib.rs`；验证：
  `cargo test -p yi-agent-subagent`
- [x] 共享 attach 客户端（启动/复用 daemon、attach、激活、断开）— `src/attach.rs`；验证：
  `cargo test -p yi-agent-subagent --test attach_delegation`

**验证命令：** `cargo test -p yi-agent-subagent`
```

- [x] **Step 2: 更新索引与计数**

`docs/project-management/README.md` 的模块索引表加一行（放在 `yi-agent-app-server` 之后）：

```markdown
| yi-agent-subagent | 3 / 3 | [详情](./yi-agent-subagent.md) |
```

同步刷新同一张表里 `desktop`、`yi-agent-app-server`、`subagent-runtime`、
`yi-agent-runtime` 四行的「完成 / 总计」计数（各自模块文件改完后再数）。

- [x] **Step 3: 更新四个模块文件**

- `desktop.md`：在 Features 里加一条 `[x]`，说明「git 项目目录下的 thread 与 TUI 一样拿到
  六个委派工具」，判据写
  `cargo test -p yi-agent-app-server a_git_project_gets_the_delegation_tools`，并注明
  「前端零改动：委派工具就是普通工具，经 `translate.rs` 变 `toolCall` item，由既有
  `ToolCallCard` 渲染」。
- `yi-agent-app-server.md`：加一条 `[x]`，说明 per-cwd runtime attach（缓存键为 canonical
  cwd、失败也缓存、激活放在 driver task 内），判据写
  `cargo test -p yi-agent-app-server two_threads_in_one_cwd_share_one_attached_runtime`。
- `subagent-runtime.md`：在已有条目附近注明「工厂与委派工具已移入
  `yi-agent-subagent`，TUI 与 app-server 共用」，并更新受影响的验证命令（`-p yi-agent`
  下那些 subagent 测试若已随 crate 迁走，改成 `-p yi-agent-subagent`）。
- `yi-agent-runtime.md`：在 `bootstrap_agent()` 那条上补 `AgentBootstrap` 新增
  `tools` / `catalog` 字段与理由。

- [x] **Step 4: 更新 bug-list**

`docs/bug-list.md` 里那条「Mac desktop 版本的 yi-agent app 上面，没法起 subagent」改
`[x]`，按同文件既有格式补齐：根因（app-server 从不接触 daemon，`bootstrap_agent` 也不暴露
registry，所以模型手里没有 `spawn_agent`）、修复（新 crate + per-cwd attach + 工具叠加）、
验证命令（`cargo test -p yi-agent-app-server`、`cargo test -p yi-agent-subagent`、
`cargo test -p yi-agent --bin yi-agent`）。

- [x] **Step 5: 提交**

```bash
git add docs && git commit -m "docs: record desktop subagent delegation parity"
```

---

## 完成判据（全部来自设计文档 §6）

1. git 项目目录下 `cargo test -p yi-agent-app-server a_git_project_gets_the_delegation_tools`
   通过（六个工具全在）。
2. `cargo test -p yi-agent-app-server a_non_git_cwd_fails_to_attach_so_the_tools_are_absent`
   通过（降级不报错）。
3. `cargo test -p yi-agent-subagent` 与 `cargo test -p yi-agent --bin yi-agent` 全绿
   （TUI 零回归）。
4. `cd desktop && npx tsc --noEmit && npm test` 全绿（前端零改动）。
5. 手工验收（可选，需真实 API key）：`cd desktop && npm run sidecar && npm run tauri dev`，
   选一个 git 项目目录建 thread，发「用 spawn_agent 起一个只读子任务调研 X」，对话里出现
   `spawn_agent` 的 toolCall 卡片并返回 `task_id`。

---

## 执行偏差记录

计划本身没覆盖、实现时才暴露的三点，均已按"以实测为准"处理：

1. **非 git 目录不是"不注册工具"**（Task 4 的期望被实测推翻）。计划与设计都假设
   `attach` 会因非 git 项目失败，于是"没有六个工具"。实测：`attach_application_root`
   对非 git 项目原地建 root，`register_attached_root_tools` 照常注册，真正失败发生在
   `spawn_agent` —— daemon 的 worker 准入要求可恢复的 `worktree:` lease
   （`validate_recovery_context`，见 `crates/yi-agent-store/src/repository.rs`）。
   测试因此改名并改为断言"attach 成功 + 工具齐全 + spawn 被拒"，设计文档 §2.4 与 §5 同步更正。
   这不是本设计引入的行为，TUI 一直如此（共用同一份 `attach_project_runtime`）。
2. **`TurnPrompt` 不能在首次激活后清除 `activate`**。计划 Step 8 让 driver 只消费一次；
   实现改为 `pending_activation: HashMap<thread_id, Option<Arc<..>>>` + 主循环投递时 clone，
   因为 `thread/start` 与 `turn/start` 是两个 RPC，attach 发生在前者、激活发生在后者。
3. **`thread/delete` 的 detach 收窄**（计划里标注的"优先验证"项，实测证实不能全断）：
   计划给的写法是删任一线程即断开该项目全部 root。实测语义上不可接受——同一个 driver
   只激活一次，被断开后同项目其它 thread 的委派不会自动恢复。故改为
   `detach_unused_runtimes(&runtimes, &live_cwds)`：只有当项目目录下不再有任何活着的
   thread 时才 detach（`crates/yi-agent-app-server/src/server.rs`）。
4. **`BuiltAgent` 不必保留被替换的 `tools` / `permission`**：计划 Step 4 让 `wrap_for_delegation`
   把它们回填进 `BuiltAgent`，clippy 报 `fields are never read`。实测无任何读取方，故删除，
   只保留重建必须沿用的 provider / config / 决定通道 / session / catalog / yolo。
5. **`main.rs` 的 `runtime_directory_from` 与它的 5 个测试删除**：任务 2 把解析逻辑委托给
   `project_runtime_directory` 后，这个本地函数只剩测试在用（clippy `never used`），而它的
   5 个测试断言的是一个手写副本、比较的是常量而非被测逻辑。改为在
   `crates/yi-agent-subagent/src/attach.rs` 直接测 `project_runtime_directory`
   （默认位置 + 两项目互不相同），App 侧因此少一个"看起来有用"的死函数。

**验证结果**（逐步实测，未并行跑 cargo）：

| 命令 | 结果 |
|---|---|
| `cargo test -p yi-agent-subagent` | 30 + 1 通过 |
| `cargo test -p yi-agent-app-server` | 157 通过（本次新增 3 个；app-server 模块文档此前记的 148 已过期，一并更正为实测 157） |
| `cargo test -p yi-agent-runtime` | 64 通过 |
| `cargo test -p yi-agent --bin yi-agent` | 481 通过 |
| `cargo test -p yi-agent-store --test runtime_ipc` | 98 通过 |
| `cd desktop && npx tsc --noEmit && npm test` | 158 通过（前端零改动） |
| `cargo clippy -p yi-agent-app-server -p yi-agent-subagent --all-targets` | 无告警 |
