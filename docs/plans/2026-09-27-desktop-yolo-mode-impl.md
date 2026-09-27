# Desktop 按线程 YOLO 模式 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 desktop app 输入框旁提供模式 chip,按线程即时开关 YOLO;开启后跳过审批并把 OS 沙箱放开为 `DangerFullAccess`,黑名单仍硬拒——与 CLI `--yolo` 完全一致。

**Architecture:** 单一共享 `YoloSwitch`(`Arc<AtomicBool>`),permission 与 sandbox 读同一开关。核心是先让 `PermissionChecker.yolo` 与 `SandboxPolicy` 都改为读该开关,再把开关经 `bootstrap_agent` 暴露给 app-server 并存入 `ThreadSession`,由新 RPC `thread/setPermissionMode` 翻转并持久化到 `ThreadMeta`。

**Tech Stack:** Rust(core / tools / runtime / app-server)、Tauri 2、React + TS + Vite + Tailwind、serde、tokio。

**设计文档:** `docs/plans/2026-09-27-desktop-yolo-mode-design.md`(已合入 main)。

---

## 关键修正(实现时以此为准)

设计文档第 1.1 节把 `YoloSwitch` 放在 `yi-agent-tools`,**这是错的**。
实测依赖方向:`yi-agent-tools` 依赖 `yi-agent-core`(见 `crates/yi-agent-tools/Cargo.toml`
的 `yi-agent-core = { workspace = true }`),反之不成立。因此:

- **`YoloSwitch` 放 `yi-agent-core`**(新建 `crates/yi-agent-core/src/autonomy.rs`),
  供 `permission.rs`(core)与 `sandbox.rs`(tools)共同使用。
- `SandboxController` 放 `crates/yi-agent-tools/src/sandbox.rs`,引用
  `yi_agent_core::autonomy::YoloSwitch`。

其余设计不变。

## 全局约定

- 每个 Task 内的小步骤(写测试→跑失败→实现→跑通过→提交)各成一步。
- 测试命令按 crate 跑(CLAUDE.md 要求,避免 `--workspace` OOM):
  `cargo test -p <crate> --lib <name>`。
- **跑测试前先确认无其他 cargo 进程**:`ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent"`。
- 提交前在 `yi-agent-rs/` 下 `cargo fmt --all`。
- 提交信息用 conventional commits,不带 `Co-Authored-By`。
- 所有工作在 worktree `.worktrees/feat-desktop-yolo-mode`(分支 `feat/desktop-yolo-mode`)。

---

## Task 1: `YoloSwitch`(core)

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-core/src/autonomy.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/lib.rs`(加 `pub mod autonomy;`)

- [ ] **Step 1: 写失败测试**

在 `autonomy.rs` 内:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_flip() {
        let s = YoloSwitch::new(false);
        assert!(!s.get());
        s.set(true);
        assert!(s.get());
        s.set(false);
        assert!(!s.get());
    }

    #[test]
    fn clones_share_one_flag() {
        let a = YoloSwitch::new(false);
        let b = a.clone();
        a.set(true);
        assert!(b.get()); // 克隆共享同一开关
    }
}
```

- [ ] **Step 2: 跑测试确认失败(未定义)**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib autonomy`
Expected: 编译失败,`YoloSwitch` 未定义。

- [ ] **Step 3: 实现**

```rust
//! 运行时可翻转的 yolo 开关,由权限层与沙箱层共享。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// 单一事实来源:permission 与 sandbox 读同一个原子标志。
#[derive(Clone, Debug)]
pub struct YoloSwitch(Arc<AtomicBool>);

impl YoloSwitch {
    pub fn new(on: bool) -> Self {
        Self(Arc::new(AtomicBool::new(on)))
    }

    pub fn get(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn set(&self, on: bool) {
        self.0.store(on, Ordering::SeqCst);
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib autonomy`
Expected: PASS(2 tests)。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-core/src/autonomy.rs yi-agent-rs/crates/yi-agent-core/src/lib.rs
git commit -m "feat(core): add runtime-flippable YoloSwitch"
```

---

## Task 2: `PermissionChecker` 改用 `YoloSwitch`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/permission.rs:84-134`
- Test: 同文件 `permission.rs` 的 `#[cfg(test)] mod tests`

- [ ] **Step 1: 写失败测试**

在 `permission.rs` 测试模块加(参考既有 `check_blacklist_overrides_yolo` 的构造方式):

```rust
#[test]
fn yolo_switch_flips_decision_at_runtime() {
    let switch = crate::autonomy::YoloSwitch::new(false);
    let checker = PermissionChecker::new(
        PermissionsConfig::default(),
        switch.clone(),
        std::path::PathBuf::from("/tmp"),
        Arc::new(|_| None),
    );
    // 默认:白名单为空 -> 需确认
    assert!(matches!(
        checker.check("bash", &serde_json::json!({"command": "echo hi"})),
        CheckResult::NeedConfirm(_)
    ));
    // 翻转后:放行
    switch.set(true);
    assert!(matches!(
        checker.check("bash", &serde_json::json!({"command": "echo hi"})),
        CheckResult::Allow
    ));
    // 关回:又需确认
    switch.set(false);
    assert!(matches!(
        checker.check("bash", &serde_json::json!({"command": "echo hi"})),
        CheckResult::NeedConfirm(_)
    ));
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib permissions::tests::yolo_switch_flips_decision_at_runtime`
Expected: 编译失败(`new` 第二参类型不匹配 / 无 `YoloSwitch`)。

- [ ] **Step 3: 改实现**

`permission.rs`:
- 字段 `yolo: bool` → `yolo: crate::autonomy::YoloSwitch`(`:86`)。
- `new(config, yolo: bool, ...)` → `new(config, yolo: crate::autonomy::YoloSwitch, ...)`,
  内部 `self.yolo = yolo`(`:93-106`)。
- `check()` 内 `if self.yolo` → `if self.yolo.get()`(`:118`)。
- 新增 `pub fn is_yolo(&self) -> bool { self.yolo.get() }`。

- [ ] **Step 4: 修所有调用点**

`load_permission_checker`(runtime `bootstrap.rs:282-305`)签名 `yolo: bool` →
`yolo: crate::autonomy::YoloSwitch`(见 Task 6)。其余测试里
`PermissionChecker::new(..., true/false, ...)` 改为 `YoloSwitch::new(true/false)`。

- [ ] **Step 5: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib permissions`
Expected: PASS(含原有 `check_blacklist_overrides_yolo` 仍通过)。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-core/src/permission.rs
git commit -m "refactor(core): PermissionChecker reads shared YoloSwitch"
```

---

## Task 3: `SandboxController` + 改造 `SandboxPolicy`(tools)

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/sandbox.rs:20-70`
- Test: 同文件 `mod tests`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn effective_promotes_only_when_promotable() {
    let sw = yi_agent_core::autonomy::YoloSwitch::new(false);
    let ctrl = SandboxController::new(sw.clone(), SandboxMode::WorkspaceWrite, true);
    assert_eq!(ctrl.effective(), SandboxMode::WorkspaceWrite);
    sw.set(true);
    assert_eq!(ctrl.effective(), SandboxMode::DangerFullAccess);

    // 非 promotable:即使 yolo 也不提权
    let sw2 = yi_agent_core::autonomy::YoloSwitch::new(true);
    let ctrl2 = SandboxController::new(sw2, SandboxMode::ReadOnly, false);
    assert_eq!(ctrl2.effective(), SandboxMode::ReadOnly);
}

#[test]
fn policy_command_switches_with_switch() {
    let sw = yi_agent_core::autonomy::YoloSwitch::new(false);
    let ctrl = SandboxController::new(sw.clone(), SandboxMode::WorkspaceWrite, true);
    let policy = SandboxPolicy::with_controller(Path::new("/tmp"), vec![], ctrl);
    // 非 yolo:走平台包装器(非裸 sh)
    #[cfg(target_os = "macos")]
    assert_eq!(policy.command("echo ok", Path::new("/tmp")).unwrap().0, "/usr/bin/sandbox-exec");
    // yolo:裸 sh
    sw.set(true);
    assert_eq!(policy.command("echo ok", Path::new("/tmp")).unwrap().0, "sh");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib sandbox`
Expected: 编译失败(`SandboxController` 未定义)。

- [ ] **Step 3: 实现**

```rust
use yi_agent_core::autonomy::YoloSwitch;

/// 把「当前 yolo 开关 + 基础模式 + 是否允许提权」组合成一个运行时可变的有效模式。
#[derive(Clone, Debug)]
pub struct SandboxController {
    switch: YoloSwitch,
    base: SandboxMode,
    promotable: bool,
}

impl SandboxController {
    pub fn new(switch: YoloSwitch, base: SandboxMode, promotable: bool) -> Self {
        Self { switch, base, promotable }
    }
    pub fn effective(&self) -> SandboxMode {
        if self.switch.get() && self.promotable {
            SandboxMode::DangerFullAccess
        } else {
            self.base
        }
    }
}
```

`SandboxPolicy` 改造:

```rust
#[derive(Clone, Debug)]
pub struct SandboxPolicy {
    controller: SandboxController,
    writable_roots: Vec<PathBuf>,
}

impl SandboxPolicy {
    /// 保留旧签名:内部建一个「不可共享」的 controller,行为与原实现逐字节一致。
    pub fn new(mode: SandboxMode, workspace_root: &Path, extra_writable_roots: Vec<PathBuf>) -> Self {
        let ctrl = SandboxController::new(
            YoloSwitch::new(false), // 老路径不翻转,effective() == base == mode
            mode,
            false,
        );
        Self::with_controller(workspace_root, extra_writable_roots, ctrl)
    }

    pub fn with_controller(
        workspace_root: &Path,
        extra_writable_roots: Vec<PathBuf>,
        controller: SandboxController,
    ) -> Self {
        let mut writable_roots = Vec::with_capacity(1 + extra_writable_roots.len());
        writable_roots.push(canonicalize_root(workspace_root));
        writable_roots.extend(extra_writable_roots.into_iter().map(|r| canonicalize_root(&r)));
        writable_roots.sort();
        writable_roots.dedup();
        Self { controller, writable_roots }
    }

    pub fn mode(&self) -> SandboxMode { self.controller.effective() }

    pub fn allows_writes(&self) -> bool { self.controller.base != SandboxMode::ReadOnly }

    pub fn command(&self, shell_command: &str, cwd: &Path) -> Result<(String, Vec<String>), ToolsError> {
        match self.controller.effective() {
            SandboxMode::DangerFullAccess => Ok(("sh".into(), vec!["-c".into(), shell_command.into()])),
            mode @ (SandboxMode::ReadOnly | SandboxMode::WorkspaceWrite) => {
                platform_command(mode, &self.writable_roots, shell_command, cwd)
            }
        }
    }
}
```

> `allows_writes()` 用 `base` 计算(工具注册是构造期决策);`base == ReadOnly` 时
> write/edit 不注册,yolo 不会把只读会话变成可写。默认 `WorkspaceWrite` 下工具面不变。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib sandbox`
Expected: PASS(含既有 2 个用例)。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-tools/src/sandbox.rs
git commit -m "feat(tools): runtime-mutable SandboxController behind SandboxPolicy"
```

---

## Task 4: 让 bash 工具与 process manager 共享同一 controller

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/lib.rs:56-83`(新增带 controller 的注册函数)
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/process/manager.rs:236-252`(新增
  `with_controller`)
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs:163-178`

- [ ] **Step 1: 写失败测试**

在 `lib.rs` 测试模块:

```rust
#[test]
fn shared_controller_reaches_bash_and_process_manager() {
    use yi_agent_core::autonomy::YoloSwitch;
    let sw = YoloSwitch::new(false);
    let ctrl = SandboxController::new(sw.clone(), SandboxMode::WorkspaceWrite, true);
    // 分别给 bash policy 与 process manager 各一份 controller 克隆
    let b = SandboxPolicy::with_controller(Path::new("/tmp"), vec![], ctrl.clone());
    let p = SandboxPolicy::with_controller(Path::new("/tmp"), vec![], ctrl.clone());
    sw.set(true);
    assert_eq!(b.mode(), SandboxMode::DangerFullAccess);
    assert_eq!(p.mode(), SandboxMode::DangerFullAccess); // 同一开关,同时生效
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib shared_controller_reaches`
Expected: 失败(需 `controller.clone()` 可用——Task 3 已给 `Clone`;若缺则补)。此步主要用于
确认测试可编译并失败于断言前的缺件。

- [ ] **Step 3: 实现**

`lib.rs` 新增:

```rust
pub fn register_builtin_tools_with_controller(
    registry: &mut ToolRegistry,
    root: PathBuf,
    controller: SandboxController,
    extra_writable_roots: Vec<PathBuf>,
) {
    let ctx = Arc::new(ToolsContext::new(root));
    let sandbox = SandboxPolicy::with_controller(ctx.root(), extra_writable_roots, controller);
    // ...与 register_builtin_tools_with_sandbox 相同,注册 read/view/glob/grep/web
    // 及 (allows_writes ? write+edit) 与 BashTool::with_sandbox(ctx, sandbox)
}
```

保留 `register_builtin_tools_with_sandbox` 不变(内部可委托新函数:先建一个
`SandboxController::new(YoloSwitch::new(false), mode, false)`)。
`ProcessManager` 加 `with_controller(root, controller, extra_roots)`。

`bootstrap.rs:163-178` 改为:先建 `let switch = YoloSwitch::new(cfg.yolo)`(Task 6 会把它
提前到 `bootstrap_agent`),再 `let ctrl = SandboxController::new(switch.clone(), cfg.sandbox,
cfg.sandbox_promotable)`;`register_builtin_tools_with_controller(...)` 与
`ProcessManager::with_controller(...)` 各传一个 `ctrl.clone()`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib shared_controller_reaches`
Expected: PASS。

- [ ] **Step 5: 全 crate 回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib`
Expected: PASS(既有 bash/sandbox 用例不回归)。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-tools/src/lib.rs yi-agent-rs/crates/yi-agent-tools/src/process/manager.rs yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs
git commit -m "feat(tools): share one SandboxController across bash and process tools"
```

---

## Task 5: `RuntimeConfig.sandbox_promotable`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs:24`(struct 字段)、`:271-288`
  (解析)、`:326-338`(`redacted_view`,可选)

- [ ] **Step 1: 写失败测试**

在 `config.rs` 测试模块:

```rust
#[test]
fn sandbox_promotable_false_when_sandbox_explicit() {
    let overrides = ConfigOverrides { sandbox: Some(yi_agent_tools::SandboxMode::ReadOnly), yolo: true, ..Default::default() };
    let cfg = RuntimeConfig::load(&overrides).unwrap();
    assert!(!cfg.sandbox_promotable);
    assert_eq!(cfg.sandbox, yi_agent_tools::SandboxMode::ReadOnly);
}

#[test]
fn sandbox_promotable_true_by_default() {
    let cfg = RuntimeConfig::load(&ConfigOverrides::default()).unwrap();
    assert!(cfg.sandbox_promotable);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib config::tests::sandbox_promotable`
Expected: 编译失败(无字段)。

- [ ] **Step 3: 实现**

`RuntimeConfig` 加 `pub sandbox_promotable: bool`。解析段(`:277-288`)改为:

```rust
let env_sandbox = std::env::var("YI_AGENT_SANDBOX").ok();
let (sandbox, sandbox_promotable) = match overrides.sandbox {
    Some(mode) => (mode, false),
    None => match env_sandbox {
        Some(value) => (parse_sandbox_mode(&value)?, false),
        None if overrides.yolo => (yi_agent_tools::SandboxMode::DangerFullAccess, true),
        None => (yi_agent_tools::SandboxMode::default(), true),
    },
};
```

> 语义与原实现一致(显式 > env > yolo?Danger:default),只是额外记录「是否允许 yolo
> 提权」。`redacted_view()` 可加 `"sandbox_promotable"` 便于调试(可选)。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib config`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-runtime/src/config.rs
git commit -m "feat(runtime): record whether yolo may promote the sandbox"
```

---

## Task 6: `bootstrap_agent` 建共享开关并在 `AgentBootstrap` 暴露

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs:202-212`(结构体)、
  `:231-275`(`bootstrap_agent`)、`:282-305`(`load_permission_checker`)

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn bootstrap_returns_shared_switch_that_toggles_sandbox() {
    let overrides = ConfigOverrides::default(); // yolo=false, sandbox=WorkspaceWrite, promotable=true
    let cfg = RuntimeConfig::load(&overrides).unwrap();
    let built = bootstrap_agent(&cfg, PermissionMode::Interactive).unwrap();
    assert!(!built.yolo.get());
    built.yolo.set(true);
    assert!(built.yolo.get());
    assert!(built.permission.is_yolo());
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib bootstrap::tests::bootstrap_returns_shared`
Expected: 编译失败(无 `built.yolo`)。

- [ ] **Step 3: 实现**

`AgentBootstrap` 加 `pub yolo: yi_agent_core::autonomy::YoloSwitch`。
`bootstrap_agent` 内:

```rust
let yolo_on = match mode {
    PermissionMode::Interactive => cfg.yolo,
    PermissionMode::AutoAllow => true,
};
let switch = yi_agent_core::autonomy::YoloSwitch::new(yolo_on);
let setup = build_tool_setup_with_switch(cfg, false, &cfg.workdir, switch.clone())?;
let checker = load_permission_checker(&cfg.workdir, switch.clone())?;
// ... AgentBootstrap 各分支补 yolo: switch.clone()
```

新增 `build_tool_setup_with_switch(cfg, naked, workspace, switch)`(Task 4 的接线):
内部 `SandboxController::new(switch, cfg.sandbox, cfg.sandbox_promotable)`,并把
`register_builtin_tools_with_controller` / `ProcessManager::with_controller` 接上。
旧 `build_tool_setup` / `build_tool_setup_in` 保留,内部委托(建一个
`YoloSwitch::new(cfg.yolo)`)。

`load_permission_checker(workdir, yolo: YoloSwitch)` 传给 `PermissionChecker::new`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib bootstrap`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs
git commit -m "feat(runtime): expose shared YoloSwitch from bootstrap_agent"
```

---

## Task 7: `ThreadMode` + `ThreadMeta.permission_mode`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs:15-25`(结构体 + 枚举)、
  `:371-386`(`rebuild_meta`)、`:399-408`(测试 helper)

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn permission_mode_defaults_to_normal_and_roundtrips() {
    let v: ThreadMeta = serde_json::from_str(r#"{"thread_id":"t","cwd":"/x","model":"m","created_at":0,"updated_at":0,"title":null}"#).unwrap();
    assert_eq!(v.permission_mode, ThreadMode::Normal); // 旧 meta 兼容

    let mut m = v;
    m.permission_mode = ThreadMode::Yolo;
    let s = serde_json::to_string(&m).unwrap();
    assert!(s.contains("\"permission_mode\":\"yolo\""));
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_store::tests::permission_mode_defaults`
Expected: 编译失败。

- [ ] **Step 3: 实现**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThreadMode {
    #[default]
    Normal,
    Yolo,
}
```

`ThreadMeta` 加:

```rust
#[serde(default)]
pub permission_mode: ThreadMode,
```

更新 `rebuild_meta` 与测试 helper `meta()` 的字面量,补 `permission_mode: ThreadMode::Normal`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_store`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs
git commit -m "feat(app-server): persist per-thread permission_mode"
```

---

## Task 8: `ThreadStore::set_permission_mode`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`(仿 `rename`,约 `:234-241`)

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn set_permission_mode_persists() {
    let dir = TempDir::new().unwrap();
    let store = ThreadStore::new(dir.path().to_path_buf());
    let mut meta = meta(); // 既有 helper
    meta.permission_mode = ThreadMode::Normal;
    store.create(&meta).unwrap();
    store.set_permission_mode("thread-1", ThreadMode::Yolo).unwrap();
    let loaded = store.load("thread-1").unwrap().unwrap();
    assert_eq!(loaded.meta.permission_mode, ThreadMode::Yolo);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_store::tests::set_permission_mode`
Expected: 编译失败(无方法)。

- [ ] **Step 3: 实现**

仿 `rename`,内部走同一把 `update_meta` 锁:

```rust
pub fn set_permission_mode(&self, thread_id: &str, mode: ThreadMode) -> Result<(), ThreadStoreError> {
    self.update_meta(thread_id, |meta| meta.permission_mode = mode)
}
```

(具体签名/错误类型对齐 `rename` 的既有写法。)

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_store`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs
git commit -m "feat(app-server): ThreadStore::set_permission_mode"
```

---

## Task 9: app-server 接线(`BuiltAgent.yolo`、`ThreadSession.yolo`、factory 签名)

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:39-45`(`BuiltAgent`)、
  `:55-78`(factory)、`:411-422`(start 插入)、`:561-572`(resume 插入)
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/session.rs:15-28`

- [ ] **Step 1: 写失败测试**

在 `server.rs` 测试(参考既有 `thread/start` / `thread/resume` 测试的 harness):

```rust
#[tokio::test]
async fn resume_yolo_thread_builds_yolo_checker() {
    // 1) 建线程、set_permission_mode(Yolo)、拿到 thread_id
    // 2) 重新 resume,断言 server 内部该线程的 yolo switch 为 true
}
```

(具体 harness 复用既有 in-process `run_with` 测试写法。)

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib server::tests::resume_yolo_thread`
Expected: 编译失败。

- [ ] **Step 3: 实现**

- `BuiltAgent` 加 `yolo: yi_agent_core::autonomy::YoloSwitch`。
- factory 签名 `Fn(Option<Session>, &Path, ThreadMode) -> Result<BuiltAgent>`;
  内部 `thread_cfg.yolo = (mode == ThreadMode::Yolo);`,`BuiltAgent.yolo = built.yolo`。
- `ThreadSession` 加 `pub yolo: yi_agent_core::autonomy::YoloSwitch`。
- `thread/start`:`build_agent(None, cwd, ThreadMode::Normal)`,插入 session 时带上 `yolo`;
  `ThreadMeta` 用 `permission_mode: ThreadMode::Normal`。
- `thread/resume`:先 `load`,取 `loaded.meta.permission_mode`,`build_agent(Some(sess), cwd,
  mode)`,session 带上 `yolo`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib server`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs yi-agent-rs/crates/yi-agent-app-server/src/session.rs
git commit -m "feat(app-server): carry per-thread YoloSwitch into ThreadSession"
```

---

## Task 10: RPC `thread/setPermissionMode`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`(dispatch,插在 `:639`
  `thread/rename` 附近;新增 handler)

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn set_permission_mode_toggles_and_persists() {
    // 建线程 -> setPermissionMode(yolo) -> 断言:
    //   (a) 该线程 switch.get() == true
    //   (b) reload meta.permission_mode == Yolo
    // 再 setPermissionMode(normal) -> switch == false, meta == Normal
}

#[tokio::test]
async fn set_permission_mode_rejects_bad_mode_and_unknown_thread() {
    // mode="bogus" -> RpcError::invalid_params
    // threadId="nope" -> RpcError::unknown_thread
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib server::tests::set_permission_mode`
Expected: FAIL(method not found / 无 handler)。

- [ ] **Step 3: 实现**

dispatch 加分支:

```rust
"thread/setPermissionMode" => {
    let thread_id = require_thread_id(&req.params)?;
    let mode = match req.params.get("mode").and_then(|v| v.as_str()) {
        Some("normal") => ThreadMode::Normal,
        Some("yolo") => ThreadMode::Yolo,
        _ => { respond(err_response(req.id, RpcError::invalid_params("mode must be \"normal\" or \"yolo\""))); continue; }
    };
    let Some(session) = threads.get(&thread_id) else {
        respond(err_response(req.id, RpcError::unknown_thread(&thread_id))); continue;
    };
    session.yolo.set(mode == ThreadMode::Yolo);
    let _ = session.store.set_permission_mode(&thread_id, mode); // best-effort
    respond(ok_response(req.id, serde_json::json!({})));
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib server`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): add thread/setPermissionMode RPC"
```

---

## Task 11: `thread/list` / `thread/listAll` 暴露 `permission_mode`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:294-306`(list)、`:325-356`(listAll)

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn list_exposes_permission_mode() {
    // 建 yolo 线程后 thread/list / thread/listAll 的条目含 "permission_mode":"yolo"
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib server::tests::list_exposes_permission_mode`
Expected: FAIL(字段缺失)。

- [ ] **Step 3: 实现**

两处映射各加 `"permission_mode": meta.permission_mode`(serde 小写)。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib server`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): expose permission_mode in thread listings"
```

---

## Task 12: 前端协议类型 + helper

**Files:**
- Modify: `desktop/src/lib/protocol.ts:99-107`(`ThreadSummary`)
- Create: `desktop/src/lib/threadPermissionMode.ts`(helper)
- Test: `desktop/src/lib/threadPermissionMode.test.ts`、`protocol.test.ts`

- [ ] **Step 1: 写失败测试**

```ts
// threadPermissionMode.test.ts
import { setPermissionModeParams } from "./threadPermissionMode";
test("builds params", () => {
  expect(setPermissionModeParams("t1", "yolo")).toEqual({ threadId: "t1", mode: "yolo" });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/threadPermissionMode.test.ts`
Expected: FAIL(模块不存在)。

- [ ] **Step 3: 实现**

`protocol.ts` 加 `permission_mode: "normal" | "yolo";` 到 `ThreadSummary`(字段缺失时
视为 `"normal"`,前端读取加 `?? "normal"`)。

```ts
// threadPermissionMode.ts
export type ThreadMode = "normal" | "yolo";
export function setPermissionModeParams(threadId: string, mode: ThreadMode) {
  return { threadId, mode };
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/protocol.ts desktop/src/lib/threadPermissionMode.ts desktop/src/lib/threadPermissionMode.test.ts
git commit -m "feat(desktop): thread permission-mode protocol types and helper"
```

---

## Task 13: `ModeChip` 组件

**Files:**
- Create: `desktop/src/components/ModeChip.tsx`
- Test: `desktop/src/components/ModeChip.test.tsx`

- [ ] **Step 1: 写失败测试**

覆盖:Normal 态渲染;点击展开下拉;选 YOLO 弹确认框;取消不回调;确认后回调 `onChange("yolo")`;
YOLO 态红色标识。

```tsx
test("enabling yolo requires confirmation", async () => {
  const onChange = vi.fn();
  render(<ModeChip mode="normal" onChange={onChange} />);
  await userEvent.click(screen.getByRole("button", { name: /mode/i }));
  await userEvent.click(screen.getByText("YOLO"));
  expect(onChange).not.toHaveBeenCalled();          // 先弹确认
  await userEvent.click(screen.getByRole("button", { name: /confirm|确认/i }));
  expect(onChange).toHaveBeenCalledWith("yolo");
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/ModeChip.test.tsx`
Expected: FAIL。

- [ ] **Step 3: 实现**

`ModeChip({ mode, onChange, disabled })`:
- Normal:低调灰 chip;YOLO:`bg-red-600` + "YOLO"。
- 下拉两项,当前项打勾;选 YOLO 先弹确认 overlay(仿 `ApprovalDialog.tsx` 结构与文案,
  说明「跳过审批、沙箱放开为完全访问、黑名单命令仍拒绝」)。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/ModeChip.test.tsx`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/ModeChip.tsx desktop/src/components/ModeChip.test.tsx
git commit -m "feat(desktop): add ModeChip with confirm-on-enable"
```

---

## Task 14: 接入 `MessageInput` 与 `App`

**Files:**
- Modify: `desktop/src/components/MessageInput.tsx:29-55`(chip 入栏)
- Modify: `desktop/src/App.tsx`(当前线程 mode 状态 + `onSetMode` + 透传)

- [ ] **Step 1: 写失败测试**

`MessageInput.test.tsx`:断言传入 `mode`/`onChange` 时渲染 `ModeChip`;App 层测试:调
`onSetMode` 会 `request("thread/setPermissionMode", ...)` 且**成功后才**更新本地 mode。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/MessageInput.test.tsx`
Expected: FAIL。

- [ ] **Step 3: 实现**

- `MessageInput` 新增 props `mode: ThreadMode`、`onModeChange(mode)`,在 Send 按钮左侧渲染
  `<ModeChip mode={mode} onChange={onModeChange} />`。
- `App.tsx`:从 `thread/started` / `thread/listAll` / resume 响应初始化当前线程 mode;`onSetMode`
  调 `client.request("thread/setPermissionMode", setPermissionModeParams(id, mode))`,成功后
  `setMode(mode)`;切换线程时按该线程的 mode 刷新。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/MessageInput.tsx desktop/src/components/MessageInput.test.tsx desktop/src/App.tsx
git commit -m "feat(desktop): wire per-thread YOLO ModeChip into input bar"
```

---

## Task 15: 文档更新(CLAUDE.md 要求)

**Files:**
- Modify: `docs/project-management/` 对应 desktop / app-server 模块文件(登记 feature +
  可验证判据:代码位置或可执行命令)
- Modify: `README.md`(模块索引表「完成 / 总计」计数)

- [ ] **Step 1:** 逐条 feature 用 `[x]` 记录,判据写代码位置或命令(如
  `cargo test -p yi-agent-app-server --lib server::tests::set_permission_mode`)。
- [ ] **Step 2:** 同步 `README.md` 计数。
- [ ] **Step 3: 提交**

```bash
git add docs/project-management README.md
git commit -m "docs: register per-thread desktop YOLO mode"
```

---

## 验证清单

1. `cd yi-agent-rs && cargo fmt --all` → 无 diff。
2. 按 crate 跑:`cargo test -p yi-agent-core --lib`、`-p yi-agent-tools --lib`、
   `-p yi-agent-runtime --lib`、`-p yi-agent-app-server --lib` → 全 PASS。
3. `cd desktop && npx vitest run` → PASS。
4. 跑测试前 `ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent"` 确认无并发 cargo。
5. `git merge --no-ff feat/desktop-yolo-mode` 回 main(见 finishing-a-development-branch)。

## 手工验收(可选)

1. `just desktop-dev`(或既有启动方式)起 app。
2. 输入框左侧见低调 mode chip → 选 YOLO → 确认框 → chip 变红。
3. 发一条会触发 bash 的任务:不再弹审批;`touch /tmp/x` 之类工作区外写入成功(沙箱已放开)。
4. 关回 Normal:再次触发 bash 恢复弹审批。
5. 重启 app:该线程 chip 仍为 YOLO(持久化生效)。
6. 黑名单命令(如 `rm -rf /`)在 YOLO 下仍被拒。
