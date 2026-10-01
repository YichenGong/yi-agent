# 每会话独立 root + 子任务容量可配 — 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 桌面端每个会话拥有独立的 application root（各自 4 个直接子任务名额），同时继续共享一个 daemon；并把 `resident:global` 子任务并发容量接成配置、默认从 16 提到 64。

**Architecture:** 把 app-server 现有的「一个 cwd 一个 root」拆成两层：`ProjectRuntimes` 继续按 cwd 缓存**共享的 daemon/binding**，新增按会话的 `ThreadRoot` 持有该会话自己的 root 三元组（attach 键 `thread:<thread_id>`）。委派工具由绑定 binding 改为绑定 `ThreadRoot`。容量侧把硬编码常量接成 `RuntimeConfig` 字段，经既有的 `AgentWorkerFactory` trait 流入 `RuntimeCoordinator::open`。

**Tech Stack:** Rust（workspace 多 crate）、tokio、serde、SQLite（runtime.sqlite）、Unix socket IPC、Tauri + React（前端零改动）。

## Global Constraints

- 设计依据：`docs/superpowers/specs/2026-10-01-per-session-subagent-root-design.md`（下称 spec）。
- **不改** `MAX_DIRECT_CHILDREN = 4`（`yi-agent-core/src/subagent/supervisor.rs:28`）。
- **不逐会话隔离** `llm:{profile}`：保持 8（含 1 协调预留）。
- **不接线** `coding:global` / `build:host`：它们在 spec §1.3 已被证实是惰性参数（只有容量定义、无申请方）。本期只记录，不实现。
- **前端零改动**：`desktop/` 下不得有任何代码变更。
- `yi-agent-store` 目前**不依赖** `yi-agent-runtime`；不得为传配置而新增该依赖（改用既有 `AgentWorkerFactory` trait，见 Task 2）。
- IPC 请求形状**不变**；`SpawnApplicationChild { thread_id: Option<String>, .. }` 保持可选。
- 默认值：`resident:global` = **64**，env 覆盖名 **`YI_AGENT_MAX_RESIDENT_SUBAGENTS`**。
- 提交信息沿用仓库风格（`feat(scope): ...` / `fix(scope): ...` / `docs: ...`）。

**本机环境限制（影响所有 Rust 任务，务必先读）：**

1. `cargo` 不在 PATH，需用完整路径：`export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`。
2. 本机 `/tmp` 与 `$TMPDIR` 对 clang **不可写**，C 依赖（`ring`）编译会失败。**必须**设一个既短又可写的 TMPDIR：
   `REPO=/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent`，
   `export TMPDIR="$REPO/.t1"`（已验证可写，且 socket 路径 85 字节 < 103）。
3. 部分 store/app-server 集成测试在本机长路径下会撞 `socket path ... exceeds this platform's 103-byte limit`（`ipc.rs:860`）。**跑不起来 ≠ 通过**；遇到时记录并换短路径环境验证，不得谎报绿灯。

**统一测试前置（每个 Rust 任务的 Run 步骤都以此为前缀）：**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.t1
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/yi-agent-rs
```

---

## File Structure

| 文件 | 职责 | 动作 |
|---|---|---|
| `yi-agent-rs/crates/yi-agent-core/src/subagent/scheduler.rs` | 资源容量定义 | 改：默认常量 16→64，新增带容量的构造入口 |
| `yi-agent-rs/crates/yi-agent-runtime/src/config.rs` | 配置加载 | 改：新增 `max_resident_subagents` 字段 + env + `redacted_view` |
| `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs` | 工厂 trait | 改：新增 `max_resident_subagents()` 访问器（默认 64） |
| `yi-agent-rs/crates/yi-agent-store/src/runtime.rs` | runtime 协调器 | 改：`open` 从 factory 取容量并设置 |
| `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` | daemon / IPC | 改：`UnavailableWorkerFactory` 沿用默认（无逻辑变更，仅确认编译） |
| `yi-agent-rs/crates/yi-agent-subagent/src/attach.rs` | attach 客户端 | 改：抽出可传入 idempotency key 的 attach |
| `yi-agent-rs/crates/yi-agent-subagent/src/thread_root.rs` | **新建** | 每会话 root 句柄 |
| `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs` | 委派工具与注册 | 改：工具持有 `ThreadRoot` |
| `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` | RPC 主循环 | 改：per-thread root 的 attach/activate/detach |
| `docs/project-management/subagent-runtime.md` | 项目管理文档 | 改：记录偏差与本次变更 |

---

## Task 1: 核心容量：常量提到 64 + 带容量的构造入口

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/scheduler.rs:111-148`
- Test: 同文件 `#[cfg(test)] mod tests`（文件内联测试）或 `yi-agent-rs/crates/yi-agent-core/tests/subagent_scheduler.rs`

**Interfaces:**
- Consumes: 无（第一个任务）
- Produces:
  - `ResourceCoordinator::DEFAULT_GLOBAL_RESIDENT_SUBAGENTS: u16 = 64`
  - `ResourceCoordinator::with_resident_capacity(units: u16) -> Self`
  - `ResourceCoordinator::default()` 的 `resident:global` 取该常量

- [ ] **Step 1: 写失败测试**

追加到 `yi-agent-rs/crates/yi-agent-core/tests/subagent_scheduler.rs` 末尾：

```rust
#[test]
fn the_default_resident_capacity_is_sixty_four() {
    let coordinator = ResourceCoordinator::new();
    assert_eq!(coordinator.capacity("resident:global"), Some(64));
    assert_eq!(
        coordinator.capacity("resident:global"),
        Some(ResourceCoordinator::DEFAULT_GLOBAL_RESIDENT_SUBAGENTS)
    );
}

#[test]
fn a_configured_resident_capacity_replaces_the_default() {
    let coordinator = ResourceCoordinator::with_resident_capacity(128);
    assert_eq!(coordinator.capacity("resident:global"), Some(128));
}
```

若该测试文件顶部未导入 `ResourceCoordinator`，按既有文件写法补 `use yi_agent_core::subagent::scheduler::ResourceCoordinator;`。

- [ ] **Step 2: 运行测试确认失败**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.t1
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/yi-agent-rs
cargo test -p yi-agent-core --test subagent_scheduler the_default_resident_capacity_is_sixty_four
```

预期：编译失败，`with_resident_capacity` 不存在，且 `capacity("resident:global")` 返回 `Some(16)`。

- [ ] **Step 3: 改常量与 `default()`**

`yi-agent-rs/crates/yi-agent-core/src/subagent/scheduler.rs`：

把 `:114-117` 的字面量改用常量（顺序：常量定义在前，`Default` 在后不影响，因为是关联常量）：

```rust
impl Default for ResourceCoordinator {
    fn default() -> Self {
        let mut capacities = HashMap::new();
        capacities.insert(
            "resident:global".into(),
            ResourceCoordinator::DEFAULT_GLOBAL_RESIDENT_SUBAGENTS,
        );
        capacities.insert("coding:global".into(), 6);
        capacities.insert("build:host".into(), 2);
        Self {
            capacities,
            in_use: HashMap::new(),
            queues: HashMap::new(),
            active: HashMap::new(),
            last_grant_root: HashMap::new(),
            last_grant_parent: HashMap::new(),
            next_sequence: 0,
            queue_capacity: 64,
        }
    }
}
```

把 `:134` 的值改掉：

```rust
    /// Resident subagent capacity for one daemon. Roots do not consume it; only
    /// subagents do (`runtime.rs` requests `resident:global` for subagents only).
    pub const DEFAULT_GLOBAL_RESIDENT_SUBAGENTS: u16 = 64;
```

在 `:138` 的 `new()` 旁新增构造入口：

```rust
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a coordinator whose resident capacity is `units` instead of
    /// [`Self::DEFAULT_GLOBAL_RESIDENT_SUBAGENTS`]. `units` of 0 is allowed and
    /// admits nothing, which callers use to park all subagent work.
    pub fn with_resident_capacity(units: u16) -> Self {
        let mut coordinator = Self::default();
        coordinator.set_capacity("resident:global", units);
        coordinator
    }
```

- [ ] **Step 4: 运行测试确认通过**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.t1
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/yi-agent-rs
cargo test -p yi-agent-core --test subagent_scheduler
```

预期：全绿。**注意**：既有用例可能断言过 `16`；若有失败，逐个检查——那是既有断言需要同步更新（改测试，不改回常量）。

再跑核心库全量，确认没有其它地方依赖 16：

```bash
cargo test -p yi-agent-core --lib subagent::
```

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/subagent/scheduler.rs \
        yi-agent-rs/crates/yi-agent-core/tests/subagent_scheduler.rs
git commit -m "feat(core): raise the default resident subagent capacity to 64 and make it settable"
```

---

## Task 2: 配置接线：`max_resident_subagents` 从配置流到 coordinator

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs`（struct `:13-34`、`load` `:173+`、`redacted_view` `:344`、`sample_config` `:360`）
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs:613-670`（trait 新增访问器）
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs:327-346`（`open` 取容量）
- Test: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs` 的 `mod tests`；`yi-agent-rs/crates/yi-agent-store/src/runtime.rs` 的 `mod tests`（`ProfileFactory` 在 `:4295`）

**Interfaces:**
- Consumes: Task 1 的 `ResourceCoordinator::with_resident_capacity(u16)`
- Produces:
  - `RuntimeConfig.max_resident_subagents: u16`（默认 64，env `YI_AGENT_MAX_RESIDENT_SUBAGENTS`）
  - `AgentWorkerFactory::max_resident_subagents(&self) -> u16`（默认实现返回 64）
  - `RuntimeCoordinator::open` 用该值建 coordinator

- [ ] **Step 1: 写失败测试（配置加载）**

加到 `yi-agent-rs/crates/yi-agent-runtime/src/config.rs` 的 `mod tests` 中（该模块已有清理环境变量的既有辅助；沿用它的模式，不要新造）：

```rust
#[test]
fn max_resident_subagents_defaults_to_sixty_four() {
    let overrides = ConfigOverrides {
        api_key: Some("sk-test".into()),
        workdir: Some(std::path::PathBuf::from("/tmp/yi-agent-cap-test")),
        ..ConfigOverrides::default()
    };
    let config = RuntimeConfig::load(&overrides).expect("config loads");
    assert_eq!(config.max_resident_subagents, 64);
}

#[test]
fn the_environment_can_lower_the_resident_capacity() {
    // SAFETY: this test module already serialises env mutation through its own
    // lock helper; reuse it rather than adding a second one.
    unsafe { std::env::set_var("YI_AGENT_MAX_RESIDENT_SUBAGENTS", "8") };
    let overrides = ConfigOverrides {
        api_key: Some("sk-test".into()),
        workdir: Some(std::path::PathBuf::from("/tmp/yi-agent-cap-test")),
        ..ConfigOverrides::default()
    };
    let config = RuntimeConfig::load(&overrides).expect("config loads");
    unsafe { std::env::remove_var("YI_AGENT_MAX_RESIDENT_SUBAGENTS") };
    assert_eq!(config.max_resident_subagents, 8);
}
```

若该模块没有 env 串行化辅助，先查看既有 env 相关用例怎么写的（`grep -n "set_var" yi-agent-rs/crates/yi-agent-runtime/src/config.rs`），完全照抄其模式。

- [ ] **Step 2: 运行确认失败**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.t1
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/yi-agent-rs
cargo test -p yi-agent-runtime --lib config::tests::max_resident_subagents_defaults_to_sixty_four
```

预期：编译失败，`RuntimeConfig` 无 `max_resident_subagents` 字段。

- [ ] **Step 3: 加字段、env 与展示**

`config.rs` struct（在 `pub max_turns: u32,` `:18` 之后插入）：

```rust
    /// Resident subagent capacity for the daemon this config starts. Roots do not
    /// consume it. Defaults to [`RESIDENT_SUBAGENTS_DEFAULT`].
    pub max_resident_subagents: u16,
```

在文件顶部常量区（`use` 之后）新增，供 config 与 trait 共享同一默认值：

```rust
/// Default resident subagent capacity, mirrored by
/// `AgentWorkerFactory::max_resident_subagents`'s default so a factory that does
/// not override it and a config that does not set it agree.
pub const RESIDENT_SUBAGENTS_DEFAULT: u16 = 64;
```

在 `load()` 中读 `max_turns` 之后（`:218-226` 之后）加：

```rust
        let max_resident_subagents = std::env::var("YI_AGENT_MAX_RESIDENT_SUBAGENTS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(RESIDENT_SUBAGENTS_DEFAULT);
```

在 `load()` 的返回结构体初始化里（`max_turns,` `:324` 附近）加：

```rust
            max_resident_subagents,
```

在 `redacted_view()`（`:344-355`）的 json 里加一行：

```rust
            "max_resident_subagents": self.max_resident_subagents,
```

在 `sample_config()`（`:360-383`）里加：

```rust
        max_resident_subagents: RESIDENT_SUBAGENTS_DEFAULT,
```

- [ ] **Step 4: 加 trait 访问器并在 `open` 中接线**

`yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs:613` 的 trait，在 `provider_profile_id`（`:621-624`）附近新增：

```rust
    /// Resident subagent capacity this factory's runtime should admit. A factory
    /// that does not care inherits the shared default.
    fn max_resident_subagents(&self) -> u16 {
        64
    }
```

（用字面量 64 而非跨 crate 引用常量：`worker.rs` 属于 `yi-agent-core`，不能依赖 `yi-agent-runtime`。两侧默认值必须都是 64——Task 1 的常量、config 的 `RESIDENT_SUBAGENTS_DEFAULT`、这里的默认，三处一致。）

`yi-agent-rs/crates/yi-agent-store/src/runtime.rs:336` 改为：

```rust
        let mut resource_coordinator =
            ResourceCoordinator::with_resident_capacity(factory.max_resident_subagents());
```

- [ ] **Step 5: 写接线测试（决定性用例）**

加到 `yi-agent-rs/crates/yi-agent-store/src/runtime.rs` 的 `mod tests`（`ProfileFactory` 定义在 `:4295`）。先给它加一个可配字段与 trait 实现：

```rust
    /// A factory that reports a specific resident capacity, so the test can
    /// prove the value survives the trip into the coordinator.
    struct CapacityFactory {
        units: u16,
    }

    impl AgentWorkerFactory for CapacityFactory {
        fn is_available(&self) -> bool {
            false
        }
        fn max_resident_subagents(&self) -> u16 {
            self.units
        }
        fn start(
            &self,
            _request: yi_agent_core::subagent::worker::WorkerStart,
        ) -> futures::future::BoxFuture<
            'static,
            Result<WorkerHandle, yi_agent_core::subagent::worker::WorkerError>,
        > {
            Box::pin(async { Err(yi_agent_core::subagent::worker::WorkerError::Startup("unused".into())) })
        }
    }

    #[test]
    fn the_factorys_resident_capacity_reaches_the_coordinator() {
        let directory = tempfile::TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let coordinator =
            RuntimeCoordinator::open(&database, Arc::new(CapacityFactory { units: 128 })).unwrap();
        assert_eq!(
            coordinator.resident_capacity(),
            Some(128),
            "the configured capacity must reach the coordinator, not just the constant"
        );
    }
```

- [ ] **Step 6: 加 `resident_capacity()` 观察器**

在 `RuntimeCoordinator` 上（`runtime.rs` 的 `impl` 内，临近 `root_task_id` `:1036`）新增，仅用于断言与诊断：

```rust
    /// The live resident subagent capacity. Exists so tests assert the value
    /// actually reached the coordinator instead of re-asserting the constant.
    pub fn resident_capacity(&self) -> Option<u16> {
        self.resources
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .capacity("resident:global")
    }
```

（字段名以 `runtime.rs:644` 实际的 `resources` 字段为准；若字段名不同，按实际改。）

- [ ] **Step 7: 运行测试**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.t1
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/yi-agent-rs
cargo test -p yi-agent-runtime --lib config::tests
cargo test -p yi-agent-core --lib subagent::
cargo test -p yi-agent-store --lib the_factorys_resident_capacity_reaches_the_coordinator
```

预期：三条全绿。

- [ ] **Step 8: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-runtime/src/config.rs \
        yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs \
        yi-agent-rs/crates/yi-agent-store/src/runtime.rs
git commit -m "feat(config): plumb the resident subagent capacity from configuration into the coordinator"
```

---

## Task 3: `ThreadRoot` — 每会话一个 root 句柄

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-subagent/src/thread_root.rs`
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`（模块声明）、`attach.rs`（可传入 key 的 attach）
- Test: `thread_root.rs` 内联 `mod tests` + 端到端见 Task 5

**Interfaces:**
- Consumes: `RuntimeBinding`（`binding.rs:46`）、`AttachedRoot`（`lib.rs:1241`）、`attach_project_runtime`（`attach.rs:115`）
- Produces:
  - `attach::application_root_idempotency_key(thread_id: &str) -> String` → `"thread:<id>"`
  - `ThreadRoot::{new, attach, handle, activate, detach, root_task_id}`

- [ ] **Step 1: 写失败测试**

新建 `yi-agent-rs/crates/yi-agent-subagent/src/thread_root.rs`，先只放测试与最小骨架（先让测试红）：

```rust
//! One conversation's own application root on a shared project daemon.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_conversation_key_is_stable_and_distinct_per_thread() {
        assert_eq!(application_root_key("thread-a"), "thread:thread-a");
        assert_eq!(application_root_key("thread-b"), "thread:thread-b");
        assert_eq!(
            application_root_key("thread-a"),
            application_root_key("thread-a"),
            "the same conversation must key the same root on every attempt"
        );
        assert_ne!(
            application_root_key("thread-a"),
            application_root_key("thread-b"),
            "two conversations must not share one root"
        );
    }
}
```

- [ ] **Step 2: 运行确认失败**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.t1
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/yi-agent-rs
cargo test -p yi-agent-subagent --lib thread_root
```

预期：编译失败（`application_root_key` 未定义）。

- [ ] **Step 3: 实现 `ThreadRoot`**

在 `thread_root.rs` 中，测试模块之上写入：

```rust
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use yi_agent_store::ipc::{IpcRequest, IpcResponse, send_request};

use crate::attach::{AttachedRoot, project_runtime_directory};
use crate::binding::{RuntimeBinding, RuntimeHandle};

/// The stable attach key for one conversation's root.
///
/// The project key cannot be used: two conversations in one directory must get
/// two roots, or they share one `MAX_DIRECT_CHILDREN` budget. The thread id is
/// stable across retries and across a daemon restart, so re-attaching the same
/// conversation adopts its own root instead of minting a new one.
pub fn application_root_key(thread_id: &str) -> String {
    format!("thread:{thread_id}")
}

/// One conversation's own root on a shared project runtime.
///
/// The `RuntimeBinding` is shared (one daemon per project, self-healing); this
/// type owns only the root triple, so two conversations in one directory have
/// two independent root tasks and two independent child budgets.
pub struct ThreadRoot {
    binding: Arc<RuntimeBinding>,
    thread_id: String,
    project_dir: PathBuf,
    root: Mutex<Option<AttachedRoot>>,
}

impl ThreadRoot {
    pub fn new(
        binding: Arc<RuntimeBinding>,
        thread_id: impl Into<String>,
        project_dir: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            binding,
            thread_id: thread_id.into(),
            project_dir,
            root: Mutex::new(None),
        })
    }

    fn lock_root(&self) -> std::sync::MutexGuard<'_, Option<AttachedRoot>> {
        self.root
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Attaches (or adopts) this conversation's root on the shared daemon.
    ///
    /// Idempotent: the stable key means a second call adopts the root the first
    /// one created, including after a daemon restart.
    pub fn attach(&self) -> Result<(), String> {
        if self.lock_root().is_some() {
            return Ok(());
        }
        let handle = self.binding.current_or_repair()?;
        let response = send_request(
            &handle.socket_path,
            IpcRequest::AttachApplicationRoot {
                idempotency_key: application_root_key(&self.thread_id),
                workspace: self.project_dir.clone(),
            },
        )
        .map_err(|error| error.to_string())?;
        let IpcResponse::ApplicationRootAttached {
            session_id,
            root_task_id,
            message_capability,
            workspace,
        } = response
        else {
            return Err(format!("daemon rejected the conversation root: {response:?}"));
        };
        *self.lock_root() = Some(AttachedRoot {
            session_id,
            task_id: root_task_id,
            capability: message_capability,
            workspace,
        });
        Ok(())
    }

    /// The handle a delegation call should act as: the shared socket plus this
    /// conversation's own root triple.
    pub fn handle(&self) -> Result<RuntimeHandle, String> {
        self.attach()?;
        let shared = self.binding.current_or_repair()?;
        let root = self
            .lock_root()
            .clone()
            .ok_or_else(|| "the conversation root is not attached".to_string())?;
        Ok(RuntimeHandle {
            socket_path: shared.socket_path,
            workspace_root: shared.workspace_root,
            session_id: root.session_id,
            task_id: root.task_id,
            capability: root.capability,
        })
    }

    /// This conversation's root task id, once attached.
    pub fn root_task_id(&self) -> Option<String> {
        self.lock_root().as_ref().map(|root| root.task_id.clone())
    }

    /// Activates this conversation's root with its first objective. Idempotent
    /// at the daemon, so a re-run after a repair is safe.
    pub fn activate(&self, objective: &str) -> Result<(), String> {
        let handle = self.handle()?;
        match send_request(
            &handle.socket_path,
            IpcRequest::ActivateApplicationRoot {
                session_id: handle.session_id.clone(),
                root_task_id: handle.task_id.clone(),
                capability: handle.capability.clone(),
                objective: objective.to_owned(),
            },
        ) {
            Ok(IpcResponse::ApplicationRootActivated) => Ok(()),
            Ok(other) => Err(format!("daemon rejected the conversation activation: {other:?}")),
            Err(error) => Err(error.to_string()),
        }
    }

    /// Detaches this conversation's root. Best effort: a failure is a trace
    /// line, never an error that masks why the conversation is winding down.
    pub fn detach(&self) {
        let handle = match self.handle() {
            Ok(handle) => handle,
            Err(_) => return,
        };
        if let Err(error) = send_request(
            &handle.socket_path,
            IpcRequest::DetachApplicationRoot {
                session_id: handle.session_id.clone(),
                root_task_id: handle.task_id.clone(),
                capability: handle.capability.clone(),
            },
        ) {
            tracing::warn!(%error, "could not detach the conversation root");
        }
        *self.lock_root() = None;
    }
}
```

在 `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs` 的模块声明处（与 `pub mod binding;` / `pub mod attach;` 同列）加：

```rust
pub mod thread_root;
```

若 `lib.rs` 未导出 `project_runtime_directory` 的使用点，检查 `attach.rs:50` 该函数是 `pub`；`thread_root.rs` 里未用到它则删掉该 `use` 项（保持 `cargo clippy -D warnings` 干净）。

- [ ] **Step 4: 运行测试确认通过**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.t1
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/yi-agent-rs
cargo test -p yi-agent-subagent --lib thread_root
```

预期：`a_conversation_key_is_stable_and_distinct_per_thread` 通过。

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-subagent/src/thread_root.rs \
        yi-agent-rs/crates/yi-agent-subagent/src/lib.rs
git commit -m "feat(subagent): add a per-conversation application root handle"
```

---

## Task 4: 委派工具绑定 `ThreadRoot`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:1121-1411`（六个工具的字段）、`:1264-1310`（注册函数）、`:1163`、`:1474`（`binding.send` 调用点）
- Test: `yi-agent-rs/crates/yi-agent-subagent/tests/attach_delegation.rs`

**Interfaces:**
- Consumes: Task 3 的 `ThreadRoot`
- Produces: `register_attached_root_tools_in_thread(registry, root: Arc<ThreadRoot>, controller, thread_id)` —— 第二个参数由 `Arc<RuntimeBinding>` 改为 `Arc<ThreadRoot>`

- [ ] **Step 1: 写失败测试**

加到 `yi-agent-rs/crates/yi-agent-subagent/tests/attach_delegation.rs`：

```rust
#[test]
fn delegation_tools_act_as_the_conversations_own_root() {
    // Two roots on one shared binding must hand out two different root task ids;
    // that is the whole point of this change.
    let root = Arc::new(ThreadRoot::new(binding, "thread-a", project_dir.clone()));
    root.attach().expect("a conversation root attaches");
    let handle = root.handle().expect("a handle resolves");
    assert_eq!(handle.task_id, root.root_task_id().expect("attached"));
    assert!(
        handle.task_id != other_root_task_id,
        "conversation A must not act as conversation B's root"
    );
}
```

该测试需要真 daemon；若既有文件已有起临时 daemon 的辅助（`grep -n "Daemon::start" yi-agent-rs/crates/yi-agent-subagent/tests/attach_delegation.rs`），完全复用它建两个 `ThreadRoot` 指向同一个 binding。

- [ ] **Step 2: 运行确认失败**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.t1
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/yi-agent-rs
cargo test -p yi-agent-subagent --test attach_delegation delegation_tools_act_as_the_conversations_own_root
```

预期：编译失败（`register_attached_root_tools_in_thread` 仍要 binding，工具结构体字段是 binding）。

- [ ] **Step 3: 改工具字段与调用点**

在 `lib.rs`：

1. 六个工具结构体（`:1121`、`:1377`、`:1397`、`:1401`、`:1405`、`:1409`）的
   `binding: Arc<crate::binding::RuntimeBinding>` 全部改为
   `root: Arc<crate::thread_root::ThreadRoot>`。
2. 所有 `self.binding.send(|h| ...)` / `self.binding.current()`（`:1163`、`:1474` 及其余同类）
   改为 `self.root.handle()?` 后用该 handle 直接 `send_request`，例如：

```rust
        let handle = match self.root.handle() {
            Ok(handle) => handle,
            Err(error) => return ToolResult::error(format!("daemon is unavailable: {error}")),
        };
        let response = yi_agent_store::ipc::send_request(
            &handle.socket_path,
            yi_agent_store::ipc::IpcRequest::SpawnApplicationChild {
                session_id: handle.session_id.clone(),
                parent_task_id: handle.task_id.clone(),
                capability: handle.capability.clone(),
                objective: task.to_string(),
                mode: Some(mode.as_str().to_string()),
                model,
                workdir,
                thread_id: self.thread_id.clone(),
                sandbox: None,
            },
        );
```

（`SpawnApplicationChild` 的字段以 `ipc.rs:190-205` 实际定义为准，逐一填齐，不得少字段。）

3. 注册函数（`:1264`、`:1281`）：

```rust
pub fn register_attached_root_tools_in_thread(
    registry: &mut ToolRegistry,
    root: Arc<crate::thread_root::ThreadRoot>,
    controller: yi_agent_tools::SandboxController,
    thread_id: Option<String>,
) {
    register_application_subagent_tools_in_thread(registry, root, controller, thread_id);
}

pub fn register_attached_root_tools(
    registry: &mut ToolRegistry,
    root: Arc<crate::thread_root::ThreadRoot>,
    controller: yi_agent_tools::SandboxController,
) {
    register_application_subagent_tools_in_thread(registry, root, controller, None);
}
```

4. `register_application_subagent_tools_in_thread` 内部六个 `registry.register(Arc::new(...))`
   的字段名同步改为 `root: Arc::clone(&root)`。

- [ ] **Step 4: 运行测试与全量编译**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.t1
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/yi-agent-rs
cargo test -p yi-agent-subagent
cargo clippy -p yi-agent-subagent --all-targets -- -D warnings
```

预期：测试绿、clippy 无告警。**注意**：此步之后 `yi-agent-app-server` 会编译失败（它还在传 binding）——这是预期的，Task 5 修复；不要把 app-server 的失败当成 Task 4 未完成。

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-subagent/src/lib.rs \
        yi-agent-rs/crates/yi-agent-subagent/tests/attach_delegation.rs
git commit -m "feat(subagent): bind the delegation tools to a conversation root"
```

---

## Task 5: app-server 接线：per-thread root + 语义修订的核心测试

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`:72` 类型、`:121-164` attach_delegation、`:462-483` attach_cwd_runtime、`:491-501` detach、`:518-555` build_runtime_tooling、`:1093`/`:1293` 调用点、`:1487-1530` thread/delete、`:2455-2490` activate、`:3143` 核心测试）
- Test: `server.rs` 内联 `mod tests`

**Interfaces:**
- Consumes: Task 3 的 `ThreadRoot`、Task 4 的 `register_attached_root_tools_in_thread(registry, Arc<ThreadRoot>, controller, Option<String>)`
- Produces: `ProjectRuntimes` 值类型不变（`Arc<RuntimeBinding>`），配套新增 per-thread 的 `ThreadRoot` 登记表

- [ ] **Step 1: 修订核心测试（先红）**

改 `server.rs:3143` 的 `two_threads_in_one_cwd_share_one_attached_runtime` 为下面这个。
**关键**：它走的是 `attach_delegation`——真实接线函数——而不是自己 new 两个 `ThreadRoot`。
后者会在「接线仍然共享 root」时也变绿（假绿）；前者在改之前必然红（两次调用拿到同一个
root task id），改之后才绿，这才是本次改动的真回归守卫。

```rust
    /// Two threads in one cwd share the daemon but never the root: sharing the
    /// root is what made every conversation in a directory split one
    /// `MAX_DIRECT_CHILDREN` budget.
    #[test]
    fn two_threads_in_one_cwd_share_the_daemon_but_not_the_root() {
        let repo = tempfile::TempDir::new().unwrap();
        let runtime = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());
        let mut cfg = test_config();
        cfg.workdir = repo.path().to_path_buf();
        let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));
        let thread_roots: ThreadRoots = Arc::new(StdMutex::new(HashMap::new()));
        let cwd = cfg.workdir.to_string_lossy().to_string();

        let first = attach_delegation(
            &runtimes,
            &thread_roots,
            &runtime.path().to_path_buf(),
            &cfg,
            &cwd,
            "thread-a",
            build_test_agent(None, &cfg.workdir, crate::thread_store::ThreadMode::Normal).unwrap(),
        );
        let second = attach_delegation(
            &runtimes,
            &thread_roots,
            &runtime.path().to_path_buf(),
            &cfg,
            &cwd,
            "thread-b",
            build_test_agent(None, &cfg.workdir, crate::thread_store::ThreadMode::Normal).unwrap(),
        );

        assert_eq!(runtimes.lock().unwrap().len(), 1, "one daemon per project");
        let a = first
            .runtime
            .expect("thread a attached")
            .root_task_id()
            .expect("thread a root is attached");
        let b = second
            .runtime
            .expect("thread b attached")
            .root_task_id()
            .expect("thread b root is attached");
        assert_ne!(a, b, "two conversations must not share one root");
    }
```

（`runtime_dir` 作为参数传入是为了让测试隔离临时目录；Task 5 Step 3 的 `attach_delegation`
签名须包含它。若实现时发现 `attach_delegation` 内部自己算 `runtime_dir` 更自然，则改成
从 `cfg` 推导——但**测试的断言部分不得改动**。）

- [ ] **Step 2: 运行确认失败**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.t1
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/yi-agent-rs
cargo test -p yi-agent-app-server two_threads_in_one_cwd_share_the_daemon_but_not_the_root
```

预期：编译失败（app-server 尚未适配 Task 4 的新签名）。若本机路径过长导致运行期 socket 报错，按 Global Constraints 第 3 条处理并记录，不得跳过。

- [ ] **Step 3: 接线 per-thread root**

1. `attach_delegation`（`:121`）：保留 `attach_cwd_runtime` 取共享 binding（这是 G2），
   新增建立该会话的 `ThreadRoot`。**签名要多一个 `runtime_dir: &Path`**（生产调用方传
   `yi_agent_subagent::attach::project_runtime_directory(Path::new(cwd))`；测试传临时目录以获得
   隔离），Step 1 的测试按此签名调用：

```rust
fn attach_delegation(
    runtimes: &ProjectRuntimes,
    thread_roots: &ThreadRoots,
    runtime_dir: &Path,
    cfg: &RuntimeConfig,
    cwd: &str,
    thread_id: &str,
    built: BuiltAgent,
) -> Activation {
    let mut thread_cfg = cfg.clone();
    thread_cfg.workdir = PathBuf::from(cwd);
    let binding = match attach_cwd_runtime(runtimes, runtime_dir, &thread_cfg) {
        Ok(binding) => binding,
        Err(cause) => { /* 既有降级分支保持不变 */ }
    };
    let root = ThreadRoot::new(Arc::clone(&binding), thread_id, PathBuf::from(cwd));
    if let Err(cause) = root.attach() {
        // A conversation that cannot attach its own root degrades exactly like a
        // project that cannot attach at all: keep the plain agent, log the cause.
        tracing::warn!(stage = "attach", %cause, cwd, thread_id, "subagent delegation unavailable for this thread");
        return Activation { built, runtime: None };
    }
    thread_roots
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(thread_id.to_string(), Arc::clone(&root));
    match build_runtime_tooling(&thread_cfg, &root, thread_id, built.yolo.clone()) {
        Ok(tooling) => Activation { built: wrap_for_delegation(built, tooling), runtime: Some(root) },
        Err(cause) => { /* 既有降级分支保持不变 */ }
    }
}
```

2. 新增类型与 `Activation` 的字段类型：

```rust
/// One conversation's own root, on the project's shared binding.
type ThreadRoots = Arc<StdMutex<HashMap<String, Arc<ThreadRoot>>>>;
```

`Activation.runtime` 的类型由 `Option<Arc<RuntimeBinding>>` 改为 `Option<Arc<ThreadRoot>>`。

3. `build_runtime_tooling`（`:518`）第二参数由 `&Arc<RuntimeBinding>` 改为 `&Arc<ThreadRoot>`，
   其中 `binding.current()?` 改为 `root.handle()?`（`workspace_root` 仍取自它），
   `register_attached_root_tools_in_thread(&mut registry, Arc::clone(root), controller, Some(thread_id.to_string()))`。

4. 在 `run_with` 里建立 `let thread_roots: ThreadRoots = Arc::new(StdMutex::new(HashMap::new()));`
   （`runtimes` `:685` 旁），并把两个调用点（`:1093`、`:1293`）补上新参数。

5. `thread/delete`（`:1487-1530`）：在 `threads.remove(&thread_id)` 之后、
   `detach_unused_runtimes` 之前，detach 该会话自己的 root：

```rust
                        if let Some(root) = thread_roots
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&thread_id)
                        {
                            root.detach();
                        }
                        let live_cwds = threads
                            .values()
                            .map(|session| session.cwd.clone())
                            .collect::<Vec<_>>();
                        detach_unused_runtimes(&runtimes, &live_cwds);
```

（`detach_unused_runtimes` 保持原样：它负责的是**共享 binding/daemon** 在最后一个会话离开后断开，这是 G2 的收尾。）

6. activate 路径（`:2471-2477`）：`binding.activate(&objective)` 的 `binding` 现在是
   `Option<Arc<ThreadRoot>>`，`ThreadRoot::activate` 签名与 `RuntimeBinding::activate` 一致，
   该行本身**不需要改**——只在变量名上从 `binding` 改为 `runtime` 之类以反映语义（可选，保持最小改动则不动）。

- [ ] **Step 4: 运行测试**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.t1
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/yi-agent-rs
cargo test -p yi-agent-app-server
cargo test -p yi-agent-subagent
cargo clippy --workspace --all-targets -- -D warnings
```

预期：全绿（本机路径受限的既有用例除外，需单独说明）。

- [ ] **Step 5: 补跨界契约守卫（store 层，本计划最重要的一条）**

在 `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs` 追加。它把「每会话一 root」
带来的两个不变量用真实 daemon 钉死：**跨会话不可达** 与 **会话结束即回收**。
该文件已有 `TextCompletionFactory`（`:393`）与 root attach 的既有模式（`:555`），照用。

```rust
#[test]
fn two_conversation_roots_do_not_share_children_or_reach_across() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(TextCompletionFactory::default());
    let daemon =
        Daemon::start_with_factory(directory.path().join("runtime"), &database, factory.clone())
            .unwrap();
    let workspace = PathBuf::from("/tmp/yi-agent-test-project");

    // Two conversations, two roots, one daemon.
    let attach = |key: &str| {
        let IpcResponse::ApplicationRootAttached {
            session_id,
            root_task_id,
            message_capability,
            ..
        } = send_request(
            daemon.socket_path(),
            IpcRequest::AttachApplicationRoot {
                idempotency_key: key.into(),
                workspace: workspace.clone(),
            },
        )
        .unwrap()
        else {
            panic!("expected attachment for {key}");
        };
        (session_id, root_task_id, message_capability)
    };
    let (session_a, root_a, cap_a) = attach("thread:thread-a");
    let (session_b, root_b, cap_b) = attach("thread:thread-b");
    assert_ne!(root_a, root_b, "two conversations must get two roots");

    // A's child, reached only through A's own session/capability.
    let IpcResponse::TaskSpawned { task_id: child_a } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            workdir: None,
            session_id: session_a.clone(),
            parent_task_id: root_a.clone(),
            capability: cap_a.clone(),
            objective: "conversation A work".into(),
            mode: Some("read_only".into()),
            model: None,
            thread_id: Some("thread-a".into()),
            sandbox: None,
        },
    )
    .unwrap()
    else {
        panic!("A must admit its own child");
    };

    // B cannot reach A's child: the ids cross but the session does not match.
    let crossed = send_request(
        daemon.socket_path(),
        IpcRequest::InspectChild {
            session_id: session_b.clone(),
            caller_task_id: root_b.clone(),
            capability: cap_b.clone(),
            task_id: child_a.clone(),
        },
    );
    assert!(
        matches!(
            crossed,
            Err(_) | Ok(IpcResponse::Error { .. })
        ),
        "a conversation must not reach another conversation's child, got {crossed:?}"
    );

    // Each root has its own budget: A's terminal child frees A's slot only.
    for index in 0..4 {
        assert!(
            matches!(
                send_request(
                    daemon.socket_path(),
                    IpcRequest::SpawnApplicationChild {
                        workdir: None,
                        session_id: session_b.clone(),
                        parent_task_id: root_b.clone(),
                        capability: cap_b.clone(),
                        objective: format!("conversation B child {index}"),
                        mode: Some("read_only".into()),
                        model: None,
                        thread_id: Some("thread-b".into()),
                        sandbox: None,
                    },
                )
                .unwrap(),
                IpcResponse::TaskSpawned { .. }
            ),
            "B must still admit its own four children while A holds one"
        );
        factory.handles.lock().unwrap()[index].report_completed(format!("b-done-{index}"));
    }
}

#[test]
fn ending_one_conversation_reclaims_only_its_own_root() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(TextCompletionFactory::default());
    let daemon =
        Daemon::start_with_factory(directory.path().join("runtime"), &database, factory.clone())
            .unwrap();
    let workspace = PathBuf::from("/tmp/yi-agent-test-project");

    let attach = |key: &str| {
        let IpcResponse::ApplicationRootAttached {
            session_id,
            root_task_id,
            message_capability,
            ..
        } = send_request(
            daemon.socket_path(),
            IpcRequest::AttachApplicationRoot {
                idempotency_key: key.into(),
                workspace: workspace.clone(),
            },
        )
        .unwrap()
        else {
            panic!("expected attachment for {key}");
        };
        (session_id, root_task_id, message_capability)
    };
    let (session_a, root_a, cap_a) = attach("thread:thread-a");
    let (_, root_b, _) = attach("thread:thread-b");

    send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            workdir: None,
            session_id: session_a.clone(),
            parent_task_id: root_a.clone(),
            capability: cap_a.clone(),
            objective: "conversation A work".into(),
            mode: Some("read_only".into()),
            model: None,
            thread_id: Some("thread-a".into()),
            sandbox: None,
        },
    )
    .unwrap();

    // Detaching A ends A's root; B's root must survive.
    assert!(matches!(
        send_request(
            daemon.socket_path(),
            IpcRequest::DetachApplicationRoot {
                session_id: session_a,
                root_task_id: root_a,
                capability: cap_a,
            },
        )
        .unwrap(),
        IpcResponse::ApplicationRootDetached
    ));

    // B still admits work: detaching A did not take B's root down with it.
    assert!(
        matches!(
            send_request(
                daemon.socket_path(),
                IpcRequest::SpawnApplicationChild {
                    workdir: None,
                    session_id: /* B's session */ String::new(),
                    parent_task_id: root_b,
                    capability: /* B's capability */ String::new(),
                    objective: "conversation B work".into(),
                    mode: Some("read_only".into()),
                    model: None,
                    thread_id: Some("thread-b".into()),
                    sandbox: None,
                },
            )
            .unwrap(),
            IpcResponse::TaskSpawned { .. }
        ),
        "ending conversation A must not end conversation B"
    );
}
```

**实现该用例时必须修掉的两处占位**（不要留 `String::new()`）：上面 B 的 `session_id` /
`capability` 要从 `attach("thread:thread-b")` 的解构里取——把第二行改成
`let (session_b, root_b, cap_b) = attach("thread:thread-b");` 并使用它们。这里的
`/* ... */` 只是标明取值来源，落地代码必须是真实变量。

- [ ] **Step 6: 运行测试**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.t1
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/yi-agent-rs
cargo test -p yi-agent-store --test runtime_ipc two_conversation_roots_do_not_share_children_or_reach_across
cargo test -p yi-agent-store --test runtime_ipc ending_one_conversation_reclaims_only_its_own_root
cargo test -p yi-agent-app-server
cargo test -p yi-agent-subagent
cargo clippy --workspace --all-targets -- -D warnings
```

预期：全绿（本机路径受限的既有用例除外，需单独说明）。

- [ ] **Step 7: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs \
        yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs
git commit -m "feat(app-server): give every conversation its own application root"
```

---

## Task 6: 文档与偏差记录

**Files:**
- Modify: `docs/project-management/subagent-runtime.md`
- Modify: `docs/project-management/desktop.md`（子 Agent 委派条目的「已知限制」）

**Interfaces:**
- Consumes: 前五个任务的结论
- Produces: 文档与实现一致

- [ ] **Step 1: 记录本次变更与偏差**

在 `docs/project-management/subagent-runtime.md` 追加两条（沿用该文件既有的 `- [x]` 条目格式与「代码 + 验证命令」体例）：

1. 每会话独立 root：说明 `ProjectRuntimes` 仍按 cwd 共享 daemon，但
   `ThreadRoot`（`yi-agent-subagent/src/thread_root.rs`）按 `thread:<id>` 拿到独立 root，
   因而每个会话各有 4 个直接子任务名额；验证命令写本计划各任务的 `cargo test`
   命令（逐条列出，不用「见上」）。
2. 偏差记录（spec §7）：

```markdown
- [ ] 已知偏差：设计文档声明的 `max_coding_agents = 6` 与 `max_host_build_jobs = 2`
  （`docs/superpowers/specs/2026-08-09-runtime-scheduling-design.md:64-70`）**当前未被强制**
  ——`ResourceCoordinator` 只为 `coding:global` / `build:host` 定义了容量，却没有任何申请方
  （`scheduler.rs:118-119`；实际被申请的资源键只有 `resident:global` 与 `llm:{profile}`）。
  据文档推断行为会被误导。本期不修，仅记录。
```

- [ ] **Step 2: 更新桌面端「已知限制」**

`docs/project-management/desktop.md` 的「子 Agent 委派」条目里，把共享 root 的表述更新为
「同一 cwd 的多个会话共享一个 daemon，但**各有独立 root**，因此各自 4 个名额」，
并注明新配置项 `YI_AGENT_MAX_RESIDENT_SUBAGENTS`（默认 64）。

- [ ] **Step 3: 提交**

```bash
git add docs/project-management/subagent-runtime.md docs/project-management/desktop.md
git commit -m "docs: record the per-conversation root change and the unenforced coding capacity"
```

---

## 验收总表（计划完成后逐条核对）

| # | 判据 | 命令 |
|---|---|---|
| 1 | 默认常驻容量 64 且可覆盖 | `cargo test -p yi-agent-core --test subagent_scheduler` |
| 2 | 配置值真的流到 coordinator | `cargo test -p yi-agent-store --lib the_factorys_resident_capacity_reaches_the_coordinator` |
| 3 | 会话键稳定且互不相同 | `cargo test -p yi-agent-subagent --lib thread_root` |
| 4 | 工具以本会话 root 行事 | `cargo test -p yi-agent-subagent --test attach_delegation` |
| 5 | 两会话共享 daemon、根不同（走真实接线） | `cargo test -p yi-agent-app-server two_threads_in_one_cwd_share_the_daemon_but_not_the_root` |
| 6 | 跨会话不可达 + 各自独立名额 | `cargo test -p yi-agent-store --test runtime_ipc two_conversation_roots_do_not_share_children_or_reach_across` |
| 7 | 结束一个会话不影响另一个 | `cargo test -p yi-agent-store --test runtime_ipc ending_one_conversation_reclaims_only_its_own_root` |
| 8 | 工作区整体健康 | `cargo clippy --workspace --all-targets -- -D warnings` |
| 9 | 前端零改动 | `cd desktop && npx tsc --noEmit && npm test` 且 `git diff --stat -- desktop/` 为空 |
