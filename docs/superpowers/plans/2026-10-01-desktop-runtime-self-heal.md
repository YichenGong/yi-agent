# 桌面端 Runtime 归属与自愈 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让桌面端（app-server sidecar）在项目 runtime daemon 死掉或被外部进程带走后能自行重建并重新 attach，而不是永久失去委派；并对健康的既有 daemon 保持"复用不抢占"。

**Architecture:** 在 `yi-agent-subagent` 里新增共享的 runtime 探活与接管层（`probe_runtime` / `ensure_owned_runtime`）和活绑定 `RuntimeBinding`（工具调用时解析"当届" socket + root，失败即单飞修复并重试一次）。app-server 的 `ProjectRuntimes` 由缓存 `Arc<AttachedProjectRuntime>` 升级为缓存 `Arc<RuntimeBinding>`；TUI 用 `RuntimeBinding::fixed` 保持行为不变。

**Tech Stack:** Rust（tokio / async_trait / rusqlite）、Unix domain socket JSONL IPC、`cargo test`。

**Spec:** `docs/superpowers/specs/2026-10-01-desktop-runtime-self-heal-design.md`

## Global Constraints

- 归属策略固定为 **B（不健康才接管）**：探测到 `Healthy` 就复用，只有 `Dead` / `Wedged` 才停掉并重建（spec §1.3）。
- `Unknown` 探活结果**不得**触发接管，也不得改动缓存，按原有降级路径返回失败（spec §3.1）。
- repair 一律用**新 uuid idempotency key** 建全新 root；**不**尝试唤醒 `recovery_required` 的旧 root（spec §3.3）。
- 探活/修复失败一律 `tracing::warn!` 降级，**不得** panic、不得返回 `AgentEvent::Error`（spec §3.7）。
- TUI 行为零变化：TUI 调用点改用 `RuntimeBinding::fixed`，语义等同于今日的冻结 socket + root。
- 只改**应用侧**六个委派工具（`DaemonApplication*Tool`）；worker 侧工具（`DaemonSpawnAgentTool` / `DaemonSendMessageTool` 等，`lib.rs:551` 附近注册的那组）**不得**改动。
- 所有改动在 worktree `.worktrees/feat/desktop-runtime-self-heal`（分支 `feat/desktop-runtime-self-heal`）内完成；提交前跑 `cargo fmt --all`。
- 不在 `main` 直接提交；commit message 用 conventional commits，不写 `Co-Authored-By`。

---

## File Structure

| 文件 | 职责 | 动作 |
|---|---|---|
| `yi-agent-rs/crates/yi-agent-subagent/src/attach.rs` | 探活（`RuntimeProbe` / `probe_runtime`）+ 接管（`ensure_owned_runtime`）+ 共享 `retire_if_wedged` | Modify |
| `yi-agent-rs/crates/yi-agent-subagent/src/binding.rs` | 活绑定 `RuntimeHandle` / `RuntimeBinding` / `is_liveness_error` / `send` | Create |
| `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs` | `mod binding` 导出；`register_attached_root_tools` / `register_application_subagent_tools` + 六个应用侧工具改收 `Arc<RuntimeBinding>` | Modify |
| `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` | `ProjectRuntimes` 改存 binding；`attach_cwd_runtime` → `binding_for`；`build_runtime_tooling` / `detach_unused_runtimes` 走 binding；driver 每轮探活 | Modify |
| `yi-agent-rs/crates/yi-agent-app-server/src/session.rs` | `TurnPrompt.activate` 类型改为 `Option<Arc<RuntimeBinding>>` | Modify |
| `yi-agent-rs/crates/yi-agent/src/main.rs` | `replace_wedged_daemon` 改调共享 `retire_if_wedged` | Modify |
| `yi-agent-rs/crates/yi-agent/src/tui/subagents.rs` | re-export 调整；TUI 调用点用 `RuntimeBinding::fixed` | Modify |
| `yi-agent-rs/crates/yi-agent-subagent/tests/attach_delegation.rs` | 自愈端到端契约 | Modify |
| `docs/project-management/*.md`、`docs/bug-list.md` | 文档同步（Task 7） | Modify |

**任务依赖顺序：** Task 1 → 2 → 3 → 4 → 5（app-server）/ 6（TUI）→ 7。Task 5 与 Task 6 在 Task 4 之后可并行。

---

### Task 1: 探活原语 `RuntimeProbe` / `probe_runtime`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/attach.rs`（在文件末尾 `#[cfg(test)] mod tests` 之前插入）

**Interfaces:**
- Consumes: `yi_agent_store::ipc::{send_request, IpcRequest, IpcResponse, IpcError, IpcErrorCode}`（已存在）
- Produces:
  - `pub enum RuntimeProbe { Healthy, Wedged, Dead, Unknown }`（`Debug + Clone + Copy + PartialEq + Eq`）
  - `pub fn probe_runtime(socket_path: &std::path::Path) -> RuntimeProbe`
  - `fn is_dead_io(error: &std::io::Error) -> bool`（private）

- [ ] **Step 1: 写失败测试**

在 `attach.rs` 末尾的 `mod tests` 内追加（若已有 `use super::...`，把新符号并进去）：

```rust
    use super::{RuntimeProbe, probe_runtime};
    use yi_agent_store::ipc::{Daemon, IpcErrorCode, IpcRequest, IpcResponse, send_request};

    #[test]
    fn probe_reports_dead_without_a_socket() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("runtime.sock");
        assert_eq!(probe_runtime(&socket), RuntimeProbe::Dead);
    }

    #[test]
    fn probe_reports_healthy_on_status() {
        let runtime = tempfile::TempDir::new().unwrap();
        let database = runtime.path().join("runtime.sqlite");
        let daemon = Daemon::start(runtime.path(), &database).expect("daemon starts");
        let socket = yi_agent_store::ipc::socket_path_for(runtime.path()).unwrap();
        assert_eq!(probe_runtime(&socket), RuntimeProbe::Healthy);
        drop(daemon);
    }
```

> `Wedged` 需要"活着但回 Internal"的假 daemon，本任务不构造（`yi-agent` bin 的既有测试已有一个 `InternalErrorDaemon` 辅助，但它是 bin 私有）。`Wedged` 的构造测试放在 Task 6 的 bin 侧回归里，用 `replace_wedged_daemon` 走共享 `retire_if_wedged` 的既有用例（`main.rs:2812`）覆盖。

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib probe_`
Expected: 编译失败（`RuntimeProbe` / `probe_runtime` 未定义）。

- [ ] **Step 3: 实现**

在 `attach.rs` 的 `impl AttachFailure` 之后插入：

```rust
/// How reachable and healthy the project runtime is.
///
/// `Unknown` is deliberately separate: a permission error or an exhausted file
/// descriptor table is not "the daemon died", and must never trigger a takeover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeProbe {
    /// Answered a read-only `Status` normally.
    Healthy,
    /// Listening, but answers `internal` -- alive yet unusable (see the TUI's
    /// `replace_wedged_daemon`), so it must be retired and replaced.
    Wedged,
    /// The socket is gone or nobody is accepting on it.
    Dead,
    /// Any other outcome. Never taken over; the caller degrades as before.
    Unknown,
}

/// Probes a runtime socket with the cheapest read-only request a daemon serves.
///
/// `Status` creates no session and writes no attachment, so probing is free of
/// side effects. A wedged daemon fails it exactly as it fails everything else.
pub fn probe_runtime(socket_path: &std::path::Path) -> RuntimeProbe {
    match send_request(socket_path, yi_agent_store::ipc::IpcRequest::Status) {
        Ok(yi_agent_store::ipc::IpcResponse::Status { .. }) => RuntimeProbe::Healthy,
        Ok(yi_agent_store::ipc::IpcResponse::Error {
            code: yi_agent_store::ipc::IpcErrorCode::Internal,
            ..
        }) => RuntimeProbe::Wedged,
        Ok(_) => RuntimeProbe::Unknown,
        Err(yi_agent_store::ipc::IpcError::Io(error)) if is_dead_io(&error) => RuntimeProbe::Dead,
        Err(_) => RuntimeProbe::Unknown,
    }
}

/// Whether a connect failure means "no live daemon", as opposed to "we were not
/// allowed to find out".
fn is_dead_io(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        error.kind(),
        ErrorKind::NotFound
            | ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::NotConnected
            | ErrorKind::UnexpectedEof
            | ErrorKind::TimedOut
    )
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib probe_`
Expected: PASS（2 个）。

- [ ] **Step 5: fmt + 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-subagent/src/attach.rs
git commit -m "feat(subagent): add runtime liveness probe"
```

---

### Task 2: 接管 `ensure_owned_runtime` + 共享退休 `retire_if_wedged`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/attach.rs`

**Interfaces:**
- Consumes: Task 1 的 `RuntimeProbe` / `probe_runtime`；既有 `attach_project_runtime` / `worker_factory` / `ignore_project_local_runtime_state`（`attach.rs:115` / `:78` / `:64`）
- Produces:
  - `pub fn ensure_owned_runtime(cfg: &RuntimeConfig, runtime_dir: PathBuf) -> Result<AttachedProjectRuntime, AttachFailure>`
  - `pub fn retire_if_wedged(socket_path: &std::path::Path) -> bool`（供 bin 的 `replace_wedged_daemon` 复用）

- [ ] **Step 1: 写失败测试**

```rust
    use super::{RuntimeProbe, ensure_owned_runtime, probe_runtime};
    use std::path::PathBuf;
    use yi_agent_runtime::config::RuntimeConfig;
    use yi_agent_store::ipc::{Daemon, socket_path_for};

    fn test_config(workdir: &std::path::Path) -> RuntimeConfig {
        // Mirrors the app-server test helper: the provider is never called by
        // attach/daemon bring-up, so a minimal config is enough.
        let mut cfg = RuntimeConfig::default();
        cfg.workdir = workdir.to_path_buf();
        cfg
    }

    #[test]
    fn ensure_owned_starts_a_daemon_when_none_is_listening() {
        let project = tempfile::TempDir::new().unwrap();
        let runtime = tempfile::TempDir::new().unwrap();
        let cfg = test_config(project.path());

        let owned = ensure_owned_runtime(&cfg, runtime.path().to_path_buf())
            .expect("a dead runtime must be replaced with a fresh one");

        assert!(owned.embedded_daemon.is_some(), "this process must own the new daemon");
        let socket = socket_path_for(runtime.path()).unwrap();
        assert_eq!(probe_runtime(&socket), RuntimeProbe::Healthy);
    }

    #[test]
    fn ensure_owned_reuses_a_healthy_daemon() {
        let project = tempfile::TempDir::new().unwrap();
        let runtime = tempfile::TempDir::new().unwrap();
        let cfg = test_config(project.path());
        let database = runtime.path().join("runtime.sqlite");
        let _existing = Daemon::start(runtime.path(), &database).expect("daemon starts");

        let joined = ensure_owned_runtime(&cfg, runtime.path().to_path_buf())
            .expect("a healthy runtime must be adopted, not replaced");

        assert!(joined.embedded_daemon.is_none(), "must not steal a healthy daemon");
    }
```

> 注：`RuntimeConfig::default()` 若不存在，用 app-server 测试里的 `test_config()` 同款构造（provider 缺失不影响 attach 链路）——见 `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` 的 `fn test_config()`。实现时以该文件为准复制一份最小构造。

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib ensure_owned_`
Expected: 编译失败（`ensure_owned_runtime` 未定义）。

- [ ] **Step 3: 实现**

把 `attach.rs` 里现有 `attach_project_runtime` 的 daemon 启动段抽出为独立函数，并新增两个公开函数：

```rust
/// Starts, joins, or replaces the project daemon so *this* process ends up
/// owning a usable runtime.
///
/// Strategy B: a healthy daemon is adopted (its owner keeps it, we never steal
/// it). Only a `Dead` socket (nobody listening) or a `Wedged` daemon (alive but
/// answering `internal`) is replaced -- those cannot serve us and would only
/// keep failing.
///
/// `Unknown` is not a takeover signal: it is returned as an `AttachFailure` so
/// the caller degrades exactly as it did before.
pub fn ensure_owned_runtime(
    cfg: &RuntimeConfig,
    runtime_dir: PathBuf,
) -> Result<AttachedProjectRuntime, AttachFailure> {
    let socket_path = yi_agent_store::ipc::socket_path_for(&runtime_dir)
        .map_err(|error| AttachFailure::new("runtime directory", error))?;
    match probe_runtime(&socket_path) {
        RuntimeProbe::Healthy => {}
        RuntimeProbe::Dead => {}
        RuntimeProbe::Wedged => {
            if !retire_if_wedged(&socket_path) {
                return Err(AttachFailure::new(
                    "daemon start",
                    "the local runtime answered `internal` and could not be retired",
                ));
            }
        }
        RuntimeProbe::Unknown => {
            return Err(AttachFailure::new(
                "daemon start",
                "the local runtime could not be probed",
            ));
        }
    }
    attach_project_runtime(cfg, runtime_dir)
}

/// Retires a *wedged* daemon (listening, but every request answers `internal`)
/// so a fresh `Daemon::start` can take over.
///
/// Returns `true` only when a `Stop` was accepted. Every other outcome --
/// including a healthy daemon or a `Stop` that could not be delivered -- returns
/// `false`, because such a daemon is not ours to replace. Shared by the TUI's
/// bring-up and the desktop's self-heal so both apply one rule.
pub fn retire_if_wedged(socket_path: &std::path::Path) -> bool {
    if probe_runtime(socket_path) != RuntimeProbe::Wedged {
        return false;
    }
    tracing::warn!(
        socket = %socket_path.display(),
        "local runtime answered `internal`; retiring it so this session can start a working one"
    );
    matches!(
        send_request(socket_path, yi_agent_store::ipc::IpcRequest::Stop),
        Ok(yi_agent_store::ipc::IpcResponse::Stopping)
    )
}
```

> 实现说明：`attach_project_runtime` 内部已有 `Daemon::start_with_factory` → `AlreadyRunning => None` 的分支（`attach.rs:127`），因此 `Healthy` 分支直接落到它即可得到"复用"；`Dead` 分支 `start_with_factory` 会持锁并清理残留 socket 节点（`ipc.rs:702`）。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib ensure_owned_`
Expected: PASS（2 个）。

- [ ] **Step 5: 回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent`
Expected: 全绿（含既有的 `attach_delegation.rs`）。

- [ ] **Step 6: fmt + 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-subagent/src/attach.rs
git commit -m "feat(subagent): own or replace a project runtime on demand"
```

---

### Task 3: 活绑定 `RuntimeHandle` / `RuntimeBinding`（新文件 `binding.rs`）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-subagent/src/binding.rs`
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`（加 `pub mod binding;`，紧邻 `pub mod attach;`）

**Interfaces:**
- Consumes: Task 2 的 `ensure_owned_runtime`；`AttachedProjectRuntime`（`attach.rs:37`）
- Produces:
  - `pub struct RuntimeHandle { pub socket_path: PathBuf, pub workspace_root: PathBuf, pub session_id: String, pub task_id: String, pub capability: String }`（`Clone + Debug`）
  - `pub struct RuntimeBinding`（不 derive Debug）
  - `impl RuntimeBinding`:
    - `pub fn managed(cfg: &RuntimeConfig, runtime_dir: PathBuf, initial: Arc<AttachedProjectRuntime>) -> Arc<Self>`
    - `pub fn fixed(handle: RuntimeHandle) -> Arc<Self>`
    - `pub fn from_runtime(rt: &AttachedProjectRuntime) -> RuntimeHandle`
    - `pub fn current(&self) -> Result<RuntimeHandle, String>`
    - `pub fn repair(&self) -> Result<RuntimeHandle, String>`
    - `pub fn send<F>(&self, build: F) -> Result<IpcResponse, String> where F: Fn(&RuntimeHandle) -> IpcRequest`
    - `pub fn activate(&self, objective: &str) -> Result<(), String>`
    - `pub fn detach(&self)`
  - `pub fn is_liveness_error(error: &IpcError) -> bool`

- [ ] **Step 1: 写失败测试**

在 `binding.rs` 末尾：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error as IoError, ErrorKind};
    use std::path::PathBuf;
    use yi_agent_store::ipc::IpcError;

    fn handle(socket: &str) -> RuntimeHandle {
        RuntimeHandle {
            socket_path: socket.into(),
            workspace_root: "/tmp/ws".into(),
            session_id: "s".into(),
            task_id: "t".into(),
            capability: "c".into(),
        }
    }

    #[test]
    fn a_fixed_binding_hands_out_its_frozen_handle() {
        let binding = RuntimeBinding::fixed(handle("/tmp/frozen.sock"));
        assert_eq!(binding.current().unwrap().socket_path, PathBuf::from("/tmp/frozen.sock"));
    }

    #[test]
    fn a_fixed_binding_cannot_repair() {
        let binding = RuntimeBinding::fixed(handle("/tmp/frozen.sock"));
        assert!(binding.repair().is_err(), "TUI bindings have no repair plan");
    }

    #[test]
    fn a_dead_socket_is_a_liveness_error() {
        let dead = IpcError::Io(IoError::new(ErrorKind::ConnectionRefused, "refused"));
        assert!(is_liveness_error(&dead));
    }

    #[test]
    fn a_frame_too_large_is_not_a_liveness_error() {
        assert!(!is_liveness_error(&IpcError::FrameTooLarge));
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib binding::`
Expected: 编译失败（模块不存在）。

- [ ] **Step 3: 实现**

创建 `binding.rs`：

```rust
//! A live handle onto a project's runtime.
//!
//! The application-side delegation tools must not freeze a socket path and a
//! root triple into themselves: a daemon can die and be replaced, and when it is
//! the old root is swept away. They hold a `RuntimeBinding` instead and resolve
//! the current handle per call, so a replacement is picked up automatically.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use yi_agent_runtime::config::RuntimeConfig;
use yi_agent_store::ipc::{IpcError, IpcRequest, IpcResponse, send_request};

use crate::attach::{self, AttachedProjectRuntime};

/// Everything one delegation call needs to reach the runtime: where to connect
/// and which root to act as.
#[derive(Clone, Debug)]
pub struct RuntimeHandle {
    pub socket_path: PathBuf,
    pub workspace_root: PathBuf,
    pub session_id: String,
    pub task_id: String,
    pub capability: String,
}

/// What is needed to bring the runtime back up when it is gone.
struct RepairPlan {
    cfg: RuntimeConfig,
    runtime_dir: PathBuf,
}

/// The mutable half: which handle is current, and (for managed bindings) the
/// attached runtime that keeps the embedded daemon alive.
struct BindingState {
    handle: RuntimeHandle,
    /// `None` for fixed bindings; `Some` for managed ones.
    runtime: Option<Arc<AttachedProjectRuntime>>,
}

/// A shared, self-healing reference to a project runtime.
///
/// `managed` bindings repair themselves; `fixed` bindings (the TUI, which owns
/// its daemon for the whole session) only ever hand out the handle they were
/// built with, preserving today's behaviour exactly.
pub struct RuntimeBinding {
    state: Mutex<BindingState>,
    repair: Option<RepairPlan>,
    /// Serialises repair so one thread starts the daemon while the others wait
    /// and then adopt its result instead of racing into a second start.
    flight: Mutex<()>,
}

impl RuntimeBinding {
    pub fn managed(
        cfg: &RuntimeConfig,
        runtime_dir: PathBuf,
        initial: Arc<AttachedProjectRuntime>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(BindingState {
                handle: Self::handle_of(&initial),
                runtime: Some(initial),
            }),
            repair: Some(RepairPlan { cfg: cfg.clone(), runtime_dir }),
            flight: Mutex::new(()),
        })
    }

    pub fn fixed(handle: RuntimeHandle) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(BindingState { handle, runtime: None }),
            repair: None,
            flight: Mutex::new(()),
        })
    }

    fn handle_of(rt: &AttachedProjectRuntime) -> RuntimeHandle {
        RuntimeHandle {
            socket_path: rt.socket_path.clone(),
            workspace_root: rt.workspace_root.clone(),
            session_id: rt.attached_root.session_id.clone(),
            task_id: rt.attached_root.task_id.clone(),
            capability: rt.attached_root.capability.clone(),
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, BindingState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The current handle, without any repair attempt.
    pub fn current(&self) -> Result<RuntimeHandle, String> {
        Ok(self.lock_state().handle.clone())
    }

    /// The project directory this binding was created for. Used to decide
    /// whether any live thread still needs it.
    pub fn project_root(&self) -> PathBuf {
        self.lock_state()
            .runtime
            .as_ref()
            .map(|rt| rt.project_root.clone())
            .unwrap_or_else(|| {
                let state = self.lock_state();
                state.handle.workspace_root.clone()
            })
    }

    /// Brings the runtime back up (single-flight) and adopts the result.
    pub fn repair(&self) -> Result<RuntimeHandle, String> {
        let Some(plan) = &self.repair else {
            return Err("this runtime binding cannot be repaired".to_string());
        };
        let _flight = self.flight.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // Another thread may have finished repairing while we waited for the
        // flight lock; if so, adopt its result instead of starting a second daemon.
        if attach::probe_runtime(&self.current()?.socket_path) == attach::RuntimeProbe::Healthy {
            return self.current();
        }
        let fresh = attach::ensure_owned_runtime(&plan.cfg, plan.runtime_dir.clone())
            .map_err(|failure| format!("{}: {}", failure.stage, failure.cause))?;
        let handle = Self::handle_of(&fresh);
        let mut state = self.lock_state();
        state.handle = handle.clone();
        state.runtime = Some(Arc::new(fresh));
        Ok(handle)
    }

    /// Current if usable, else repair.
    pub fn current_or_repair(&self) -> Result<RuntimeHandle, String> {
        match self.current() {
            Ok(handle) => Ok(handle),
            Err(_) => self.repair(),
        }
    }

    /// Sends one request, repairing and retrying once if the runtime turns out
    /// to be unreachable. `build` is called with whichever handle is current.
    pub fn send<F>(&self, build: F) -> Result<IpcResponse, String>
    where
        F: Fn(&RuntimeHandle) -> IpcRequest,
    {
        let handle = self.current_or_repair()?;
        match send_request(&handle.socket_path, build(&handle)) {
            Ok(response) => Ok(response),
            Err(error) if is_liveness_error(&error) => {
                let handle = self.repair()?;
                send_request(&handle.socket_path, build(&handle)).map_err(|e| e.to_string())
            }
            Err(error) => Err(error.to_string()),
        }
    }

    /// Activates the current root. Repairs first only when unreachable.
    pub fn activate(&self, objective: &str) -> Result<(), String> {
        match self.send(|h| IpcRequest::ActivateApplicationRoot {
            session_id: h.session_id.clone(),
            root_task_id: h.task_id.clone(),
            capability: h.capability.clone(),
            objective: objective.to_owned(),
        }) {
            Ok(IpcResponse::ApplicationRootActivated) => Ok(()),
            Ok(other) => Err(format!("daemon rejected the runtime activation: {other:?}")),
            Err(error) => Err(error),
        }
    }

    /// Detaches the current root. Best effort: a failure is a trace line, never
    /// an error that could mask why the process is winding down.
    pub fn detach(&self) {
        let Ok(handle) = self.current() else {
            return;
        };
        // No repair here: a detach against a dead daemon has nothing to undo.
        if let Err(error) = send_request(
            &handle.socket_path,
            IpcRequest::DetachApplicationRoot {
                session_id: handle.session_id.clone(),
                root_task_id: handle.task_id.clone(),
                capability: handle.capability.clone(),
            },
        ) {
            tracing::warn!(%error, "could not detach the application root");
        }
    }
}

/// Whether an IPC failure means "the runtime is unreachable", which is the only
/// class worth repairing. Business rejections (a frame too large, a validation
/// error) arrive as `Ok(Error { .. })` or as non-transport errors and must not
/// trigger a takeover.
pub fn is_liveness_error(error: &IpcError) -> bool {
    match error {
        IpcError::Io(io) => matches!(
            io.kind(),
            std::io::ErrorKind::NotFound
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::TimedOut
        ),
        IpcError::TruncatedFrame { .. } => true,
        _ => false,
    }
}
```

> 关键点：`activate` / `detach` 直接按 `RuntimeHandle` 组 IPC 请求（照抄 `attach.rs:206` / `:229` 的请求体），**不伪造 `AttachedRoot`**；因此不需要给 `AttachedRoot` 加 `Default`，也不需要额外构造函数。`current()` 对 managed 与 fixed 两种绑定都恒定成功（handle 总在）；"能否修复"由 `repair: Option<..>` 区分，故 `current_or_repair()` 对 fixed 绑定等价于 `current()`，TUI 语义与今日完全一致。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib binding::`
Expected: PASS（4 个）。

- [ ] **Step 5: fmt + 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-subagent/src/lib.rs crates/yi-agent-subagent/src/binding.rs
git commit -m "feat(subagent): add a self-healing runtime binding"
```

---

### Task 4: 六个应用侧工具改收 `Arc<RuntimeBinding>`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`

**Interfaces:**
- Consumes: Task 3 的 `RuntimeBinding` / `RuntimeHandle`
- Produces:
  - `pub fn register_attached_root_tools(registry: &mut ToolRegistry, binding: Arc<RuntimeBinding>)`（签名变更）
  - `pub fn register_application_subagent_tools(registry: &mut ToolRegistry, binding: Arc<RuntimeBinding>)`（签名变更）

- [ ] **Step 1: 写失败测试**

在 `lib.rs` 的 `mod tests` 内追加（沿用该文件既有的 `attach_delegation` 测试风格；`tests/attach_delegation.rs` 已有可复用的 helper）：

```rust
    #[test]
    fn application_tools_share_one_binding() {
        use crate::binding::{RuntimeBinding, RuntimeHandle};
        use std::sync::Arc;

        let binding = RuntimeBinding::fixed(RuntimeHandle {
            socket_path: "/tmp/unused.sock".into(),
            workspace_root: "/tmp/ws".into(),
            session_id: "s".into(),
            task_id: "t".into(),
            capability: "c".into(),
        });
        let mut registry = yi_agent_core::ToolRegistry::new();
        crate::register_attached_root_tools(&mut registry, Arc::clone(&binding));

        for name in ["spawn_agent", "wait_agent", "send_message", "inspect_agent", "cancel_agent", "review_agent"] {
            assert!(registry.get(name).is_some(), "{name} must be registered");
        }
    }
```

> `registry.get(name)` 是示意；若 `ToolRegistry` 无 `get`，用该文件既有测试所用的等价访问方式（如 `registry.tools()` 或按名 `find`）。以文件现状为准。

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib application_tools_share_one_binding`
Expected: 编译失败（`register_attached_root_tools` 仍收 `PathBuf` + `&AttachedRoot`）。

- [ ] **Step 3: 实现**

改动点（逐项）：

1. 六个应用侧结构体（`lib.rs:965` `DaemonApplicationSendMessageTool`、`:1213` `DaemonApplicationSpawnAgentTool`，以及 `inspect` / `cancel` / `review` / `wait` 的 Application 变体）把
   ```rust
   runtime_socket: PathBuf,
   session_id: String,
   caller_task_id: String,
   application_capability: String,
   ```
   替换为单个
   ```rust
   binding: Arc<crate::binding::RuntimeBinding>,
   ```
2. 每个 `call` 里，把
   `yi_agent_store::ipc::send_request(&self.runtime_socket, IpcRequest::X { session_id: self.session_id.clone(), caller_task_id: ..., capability: self.application_capability.clone(), .. })`
   改为
   ```rust
   let response = match self.binding.send(|h| yi_agent_store::ipc::IpcRequest::X {
       session_id: h.session_id.clone(),
       sender_task_id: h.task_id.clone(),
       application_capability: h.capability.clone(),
       .. /* 其余入参照旧 */
   }) {
       Ok(response) => response,
       Err(error) => return ToolResult::error(error),
   };
   ```
   （各工具的字段名以既有请求体为准，逐一对齐；`wait_agent` 的轮询超时逻辑不变。）
3. `register_application_subagent_tools`（`:1111`）与 `register_attached_root_tools`（`:1097`）签名改为收 `binding: Arc<RuntimeBinding>`，内部把 `Arc::clone(&binding)` 传给六个工具。
4. `DaemonApplicationSendMessageTool` 里 `self.runtime_socket` 的另一处用法（`:1012`）与其 `:1066`、`spawn` 的 `:1312` / `:1371` / `:1394`、`inspect` / `cancel` / `review` / `wait` 的对应处一并改为经 binding。

**worker 侧六个工具（`DaemonSpawnAgentTool` / `DaemonSendMessageTool` 等，`:551` 附近）保持不动。**

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib application_tools_share_one_binding`
Expected: PASS。

- [ ] **Step 5: 回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent`
Expected: 除因签名变更需要同步的调用点外全绿（调用点在 Task 5/6 修）。

- [ ] **Step 6: fmt + 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-subagent/src/lib.rs
git commit -m "refactor(subagent): resolve delegation targets through a live binding"
```

---

### Task 5: app-server 接线（bindings + 每轮探活）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/session.rs`

**Interfaces:**
- Consumes: Task 3/4 的 `RuntimeBinding`、`register_attached_root_tools(binding)`
- Produces: `type ProjectRuntimes = Arc<StdMutex<HashMap<PathBuf, Arc<RuntimeBinding>>>>`；`TurnPrompt.activate: Option<Arc<RuntimeBinding>>`

- [ ] **Step 1: 写失败测试**

在 `server.rs` 的 `mod tests` 内追加：

```rust
    #[tokio::test]
    async fn a_dead_runtime_is_repaired_on_the_next_tool_call() {
        // Arrange a git project + isolated runtime directory, exactly like
        // `a_git_project_gets_the_delegation_tools` (server.rs:~1800).
        let repo = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());
        let runtime = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = repo.path().to_path_buf();

        let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));
        let binding = attach_cwd_runtime(&runtimes, runtime.path(), &cfg)
            .expect("a git project must attach");

        // Kill the daemon this process started: drop the runtime the binding holds.
        // (Simulates the Terminal closing: the process that owned the daemon exits.)
        let socket = binding.current().unwrap().socket_path.clone();
        drop(binding);
        *runtimes.lock().unwrap() = HashMap::new(); // forget the cached binding
        assert_eq!(
            yi_agent_subagent::attach::probe_runtime(&socket),
            yi_agent_subagent::attach::RuntimeProbe::Dead,
            "the daemon must be gone before we test recovery"
        );

        // Act: asking for the binding again must bring a new runtime up.
        let repaired = attach_cwd_runtime(&runtimes, runtime.path(), &cfg)
            .expect("a dead runtime must be replaced, not cached as a failure");
        assert_eq!(
            yi_agent_subagent::attach::probe_runtime(&repaired.current().unwrap().socket_path),
            yi_agent_subagent::attach::RuntimeProbe::Healthy,
            "the repaired runtime must be reachable"
        );
    }
```

> 说明：`drop(binding)` 只释放 `Arc`，若 map 仍持有引用则 daemon 不死；因此测试先清空 map。这模拟"持有 daemon 的进程退出"。实现时若 `Daemon` 由 binding 内部持有，确保 `Arc<AttachedProjectRuntime>` 是**唯一**持有者，否则换一种可确定停掉 daemon 的方式（例如先记下 `embedded_daemon` 并在测试里显式 stop）。

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib a_dead_runtime_is_repaired`
Expected: 编译失败（`ProjectRuntimes` 仍是旧类型 / `binding.current()` 不存在）。

- [ ] **Step 3: 实现**

1. `ProjectRuntimes`（`server.rs:69`）改为：
   ```rust
   type ProjectRuntimes = Arc<StdMutex<HashMap<PathBuf, Arc<yi_agent_subagent::binding::RuntimeBinding>>>>;
   ```
2. `attach_cwd_runtime`（`:131`）改为返回 `Result<Arc<RuntimeBinding>, String>`：
   - 命中缓存直接返回；
   - 未命中调 `yi_agent_subagent::attach::ensure_owned_runtime(&cfg, runtime_dir.to_path_buf())`，
     成功则 `RuntimeBinding::managed(&cfg, runtime_dir.to_path_buf(), Arc::new(runtime))`。
   - **失败仍写入缓存**（存 `?` 无法表达，故把 map 的值类型改为 `Result<Arc<RuntimeBinding>, String>`，与今日"失败也缓存"语义一致）。
3. `attach_delegation`（`:86`）：`build_runtime_tooling` 现在需要 binding 与其 workspace_root；改为
   ```rust
   let handle = match runtime.current() {
       Ok(handle) => handle,
       Err(cause) => return Activation { built, runtime: None }, // 理论上不会发生
   };
   match build_runtime_tooling(&thread_cfg, &runtime, handle.workspace_root.clone(), built.yolo.clone()) { .. }
   ```
   并把 `build_runtime_tooling`（`:185`）的签名加上 `binding: Arc<RuntimeBinding>`，内部 `:194` 的
   `register_attached_root_tools(&mut registry, attached.socket_path.clone(), &attached.attached_root)`
   改为 `register_attached_root_tools(&mut registry, Arc::clone(&binding))`；`:191` 的 `&attached.workspace_root` 改用传入的 `workspace_root`。
4. `detach_unused_runtimes`（`:156`）：`yi_agent_subagent::attach::detach_root(&runtime.socket_path, &runtime.attached_root)` → `binding.detach()`；`live_cwds` 的匹配改用 Task 3 已提供的 `binding.project_root()`。
5. `session.rs`：`TurnPrompt.activate: Option<Arc<yi_agent_subagent::binding::RuntimeBinding>>`，注释同步。
6. `pending_activation.insert(thread_id, activation.runtime)` 的类型随之变为 binding；driver 里激活改为：**先探活**，不健康则 `binding.repair()`，再 `binding.activate(objective)`：
   ```rust
   if let Some(binding) = &prompt.activate {
       let unhealthy = match binding.current() {
           Ok(handle) => matches!(
               yi_agent_subagent::attach::probe_runtime(&handle.socket_path),
               RuntimeProbe::Dead | RuntimeProbe::Wedged
           ),
           Err(_) => true,
       };
       if unhealthy {
           let _ = binding.repair();
       }
       if let Err(error) = binding.activate(&prompt.prompt) {
           tracing::warn!(%error, "could not activate the subagent runtime root");
       }
   }
   ```
   （`current()` 对 managed binding 恒定成功，`Err` 分支只为防御；不写 `unwrap`。）

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib a_dead_runtime_is_repaired`
Expected: PASS。

- [ ] **Step 5: 回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: 全绿（含 `a_git_project_gets_the_delegation_tools`、`a_non_git_cwd_attaches_in_place_with_the_delegation_tools`、`two_threads_in_one_cwd_share_one_attached_runtime`、`set_permission_mode`、`turn_` 系列）。

- [ ] **Step 6: fmt + 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/server.rs crates/yi-agent-app-server/src/session.rs
git commit -m "feat(app-server): self-heal a dead project runtime"
```

---

### Task 6: TUI 保全 + bin 共享退休逻辑

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/subagents.rs`

**Interfaces:**
- Consumes: Task 2 的 `retire_if_wedged`；Task 3 的 `RuntimeBinding::fixed`
- Produces: TUI 调用点用 `RuntimeBinding::fixed`；`replace_wedged_daemon`（`main.rs:789`）改为薄封装。

- [ ] **Step 1: 写失败测试**

在 `main.rs` 的既有 `replace_wedged_daemon` 测试（`:2812` 起）之后追加一条，确认行为不变但走共享实现：

```rust
    #[test]
    fn retiring_a_wedged_daemon_still_reports_true() {
        let runtime_dir = tempfile::TempDir::new().unwrap();
        let probe = InternalErrorDaemon::start(&runtime_dir.path().join("runtime.sock"));
        assert!(replace_wedged_daemon(probe.socket_path()));
        probe.join();
    }

    #[test]
    fn a_healthy_daemon_is_never_retired() {
        let runtime_dir = tempfile::TempDir::new().unwrap();
        let database = runtime_dir.path().join("runtime.sqlite");
        let daemon = yi_agent_store::ipc::Daemon::start(runtime_dir.path(), &database)
            .expect("daemon starts");
        let socket = yi_agent_store::ipc::socket_path_for(runtime_dir.path()).unwrap();
        assert!(!replace_wedged_daemon(&socket));
        drop(daemon);
    }
```

> `InternalErrorDaemon` 是 `main.rs` 既有测试辅助；沿用即可。

- [ ] **Step 2: 运行测试确认失败/通过判定**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent retiring_a_wedged`
Expected: 先跑一次记录现状；实现后必须 PASS。

- [ ] **Step 3: 实现**

1. `main.rs:789` 的 `replace_wedged_daemon` 整体替换为：
   ```rust
   fn replace_wedged_daemon(socket_path: &std::path::Path) -> bool {
       yi_agent_subagent::attach::retire_if_wedged(socket_path)
   }
   ```
   （删掉其内部那份与共享实现重复的 probe + Stop 逻辑。）
2. `tui/subagents.rs:28` 的 `pub use` 增加 `RuntimeBinding` / `RuntimeHandle`：
   ```rust
   pub use yi_agent_subagent::binding::{RuntimeBinding, RuntimeHandle};
   ```
3. TUI 构造 registry 的两处（`main.rs:616` / `:721`）与 `tui/subagents.rs:116` 的调用点，改为先建 `RuntimeHandle` 再 `RuntimeBinding::fixed(...)`：
   ```rust
   let binding = yi_agent_subagent::binding::RuntimeBinding::fixed(
       yi_agent_subagent::binding::RuntimeHandle {
           socket_path: attached.socket_path.clone(),
           workspace_root: attached.workspace_root.clone(),
           session_id: attached.attached_root.session_id.clone(),
           task_id: attached.attached_root.task_id.clone(),
           capability: attached.attached_root.capability.clone(),
       },
   );
   register_attached_root_tools(&mut registry, binding);
   ```
4. `tui/subagents.rs` 里 `CURRENT_ATTACHED_ROOT` 单例与 TUI 专有交互不动。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent`
Expected: 全绿（含既有的 4 个 `restart_notice`、`runtime_start_prompt`、`slash_commands_resolve_...`、以及 `replace_wedged_daemon` 系列）。

- [ ] **Step 5: fmt + 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/main.rs crates/yi-agent/src/tui/subagents.rs
git commit -m "refactor(tui): share the runtime retire rule and keep a fixed binding"
```

---

### Task 7: 文档同步 + 全量回归

**Files:**
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/yi-agent-app-server.md`
- Modify: `docs/project-management/subagent-runtime.md`
- Modify: `docs/project-management/yi-agent-subagent.md`
- Modify: `docs/bug-list.md`

- [ ] **Step 1: 登记 bug-list 条目**

在 `docs/bug-list.md` 新增一条 `[x]`（带根因、修复位置、验证命令）：

```
- [x] 桌面端委派在"启动该 runtime 的进程（Terminal 里的 CLI/TUI）退出"后永久失效：app-server 复用他人的 daemon 却不持有句柄（`attach.rs:127`），并把 socket 与 root 三元组冻结进六个工具（`lib.rs:1213`），缓存只插不删（`server.rs:137`），无探活无重连。修复：新增共享探活/接管（`probe_runtime`/`ensure_owned_runtime`/`retire_if_wedged`，`yi-agent-subagent/src/attach.rs`）与活绑定（`RuntimeBinding`，`yi-agent-subagent/src/binding.rs`），应用侧六工具改经 binding 解析当届 socket+root，连接失效时单飞修复并重试一次；app 启动复用既有"自动恢复最近对话 → resume → attach"路径。策略为 B：健康复用、死/卡死才接管。验证：`cargo test -p yi-agent-subagent`、`cargo test -p yi-agent-app-server --lib a_dead_runtime_is_repaired`、`cargo test -p yi-agent --bin yi-agent`。
```

- [ ] **Step 2: 更新模块文档**

- `yi-agent-subagent.md`：新增 `binding.rs` 与 `probe_runtime`/`ensure_owned_runtime`/`retire_if_wedged`，附判据。
- `yi-agent-app-server.md`：`ProjectRuntimes` 改述为 binding；补"死 runtime 自愈"条目与验证命令。
- `desktop.md`：在既有"子 Agent 委派"条目补"归属与自愈"说明，更新验证命令。
- `subagent-runtime.md`：补共享探活/接管契约。
- 每个新增/更新条目遵循 `docs/project-management/README.md` 的维护规则（三态、可验证判据、同步 README 计数）。

- [ ] **Step 3: 全量回归**

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent
cd yi-agent-rs && cargo test -p yi-agent-app-server
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent
cd yi-agent-rs && cargo test -p yi-agent-runtime
cd desktop && npx tsc --noEmit && npm test
```

Expected: 全部通过。真实 LLM 测试（`#[ignore]`）不跑；手动冒烟见 Step 4。

- [ ] **Step 4: 手动冒烟（复现原故障）**

1. 选定一个 git 项目目录，`npm run sidecar && npm run tauri dev` 起桌面端，新建 thread 并委派一次（确认 `spawn_agent` 可用）。
2. 找到该项目 runtime 的 socket（短路径 `<项目>/.yi-agent/runtime/runtime.sock`，或 `$TMPDIR/yi-agent-<hash>.sock`），用 `yi-agent` 侧命令或直接停掉持有它的进程，使探测结果为 `Dead`。
3. **不重启 App**，在同一 thread 再发一次委派。
4. Expected：该次调用自愈成功（D 自愈生效）；若仍失败，记录 trace 中 `stage`/`cause`，回到 Task 3/5 排查。

- [ ] **Step 5: fmt + 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add docs/
git commit -m "docs: record desktop runtime ownership and self-heal"
```

- [ ] **Step 6: 合并回 main（按项目规范）**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
git checkout main
git merge --no-ff feat/desktop-runtime-self-heal
git worktree remove .worktrees/feat/desktop-runtime-self-heal
git branch -d feat/desktop-runtime-self-heal
```

---

## Self-Review

**1. Spec coverage：**
- §1 现象/根因 → Task 1–6（探活、接管、活绑定、接线）。
- §1.3 策略 B → Global Constraints + Task 2 `ensure_owned_runtime`。
- §3.1 探活/接管 → Task 1、2。
- §3.2 活绑定 + 单飞 → Task 3、4。
- §3.3 旧 root 不复用稳定 key → Task 2（`ensure_owned_runtime` 走 `attach_project_runtime` 的新 uuid key）。
- §3.4 数据流（失败→修复→重试一次）与每轮探活 → Task 3 `send`/`current_or_repair` + Task 5 driver 探活。
- §3.5 触发点 → Task 5（thread/start、resume、detach）+ Task 3（工具调用）。
- §3.6 归属闸门不新增阶段 → Task 5（复用 resume 触发的 attach）。
- §3.7 降级 → Global Constraints + Task 3 `repair` 返回 `Err`、Task 5 `warn!`。
- §4 测试 → Task 1–6 各自测试 + Task 7 回归。
- §5 判据 → Task 5/6/7。
- §6 代价 → 已写入 spec，无实现任务（正确）。
- §7 文档 → Task 7。

**2. Placeholder scan：** 无 `TBD/TODO`；所有代码步骤给出可编译意图的具体代码。两处"以文件现状为准"的注解（`registry.get`、`RuntimeConfig::default`）都给了明确的替代方案，不构成占位。

**3. Type consistency：** `RuntimeHandle` 字段（`socket_path/workspace_root/session_id/task_id/capability`）在 Task 3 定义，Task 4/6 使用一致；`RuntimeBinding` 方法（`current/repair/current_or_repair/send/activate/detach`）在 Task 3 定义，Task 4/5 使用；`ProjectRuntimes` 新类型在 Task 5 定义并使用；`probe_runtime`/`RuntimeProbe`/`ensure_owned_runtime`/`retire_if_wedged` 在 Task 1/2 定义，Task 5/6 使用。
