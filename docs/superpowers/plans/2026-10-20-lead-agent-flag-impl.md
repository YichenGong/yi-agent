# 组长功能纳入功能开关 — 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 用一个开关 `lead_agent` 管住组长全部能力（7 个工具、1 个 RPC、惰性建组长、后台 tick、侧栏入口），默认关，关掉后行为与「组长功能不存在」一致。

**Architecture:** 开关是**按实例注入的值**（`RuntimeAttachments` 新增 `lead_flags_path: PathBuf`），由 `serve()` 解析一次后下发到四个硬闸与两个呈现层。判定函数收在 `lead_gate.rs`，只做「与」运算，不参与「谁是组长」的判定。

**Tech Stack:** Rust 2024、`yi-agent-flags`（已存在）、React + vitest

---

## 为什么必须按实例注入（不要跳过这节）

`yi_agent_flags::is_enabled` 读的是**进程级全局量**：

- `flags_path()` → `yi_agent_store::paths::home_join("flags.json")` → 进程 `YI_AGENT_HOME`
- `build_kind()` → 进程级 `OnceLock`，**未注入时恒为 `"release"`**
- `status_at` 的第一条：`build_kind != "dev"` → 直接返回 `(false, Source::BuildKind)`

`yi-agent-app-server` 的 lib 测试**从不注入 build info**（只有 `yi-agent` 二进制在
`main` 里注入）。因此在这个 crate 的测试里：

1. `is_enabled("lead_agent")` 恒为 `false`，写 `flags.json` 也没用；
2. 就算调 `set_build_info("dev", …)`，`OnceLock` 是进程级的，**一次设置影响该进程
   全部并行用例**，且 `YI_AGENT_HOME` 同样是进程级 → 测试互相污染，还会写开发者真实
   `~/.yi-agent/flags.json`。

结论：**开关必须作为值随实例传递**，`lead_gate` 里不读全局量。真实取值在
`serve()` 入口解析一次，测试则直接传 `Some(true)` / `Some(false)`。

---

## Global Constraints

- 工作目录：所有 cargo 命令在 `yi-agent-rs/` 下执行。
- **禁止**在 `main` 上直接提交；本工作在 `.worktrees/lead-agent-flag` 内完成，最后 `merge --no-ff`。
- 每次 commit 前必须 `cargo fmt --all`（在 `yi-agent-rs/` 下）。
- **不要**在 commit message 里写 `Co-Authored-By` 行。
- 测试按 crate 跑，**不要**用 `cargo test --workspace`（OOM / exit 137）。
- 跑测试前先 `ps aux | grep -v grep | grep -E "cargo|rustc"` 确认没有残留 cargo 进程。
- `cargo` 不在默认 PATH：先 `export PATH="/opt/homebrew/opt/rustup/bin:$PATH"`。
- desktop 测试需要 `export PATH="/opt/homebrew/bin:$PATH"`。
- 环境变量未设置时，所有行为必须与改动前完全一致。
- flag 名一旦发布不改名（`lead_agent` 就是 `flags.json` 的键）。
- 注册表 `default` 字段必须为 `false`。
- 开关**只能关、不能开**：它不能把普通会话变成组长。

## File Structure

| 文件 | 职责 |
|---|---|
| `crates/yi-agent-flags/src/registry.rs`（改） | 加 `LEAD_AGENT_FLAG` 常量与注册项 |
| `crates/yi-agent-app-server/src/lead_gate.rs`（新建） | 判定函数，纯值运算，无全局读 |
| `crates/yi-agent-app-server/src/server.rs`（改） | `RuntimeAttachments` 加字段、`serve()` 解析、`build_runtime_tooling` / `attach_delegation` / `start_thread_core` 传参、tick 臂门禁 |
| `crates/yi-agent-app-server/src/rpc/thread.rs`（改） | `thread/lead` 门禁（经 `AppState`）、`scan_workspace_metas` 门禁 |
| `crates/yi-agent-app-server/src/rpc/mod.rs`（改） | `AppState` 加 `lead_agent_enabled: bool` |
| `crates/yi-agent-app-server/src/rpc_tests.rs`（改） | `Harness` 加开关注入、既有组长测试显式打开 |
| `desktop/src/App.tsx`（改） | 读 `build/info` + `flags/list`、补建循环门禁、透传 |
| `desktop/src/components/ThreadSidebar.tsx`（改） | 新增 prop、隐藏组长行 |
| `docs/project-management/*.md`（改） | 进度与判据 |

---

### Task 1: 注册 `lead_agent` flag

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-flags/src/registry.rs`

**Interfaces:**
- Produces: `pub const LEAD_AGENT_FLAG: &str = "lead_agent";`

- [ ] **Step 1: 写失败测试**

在 `registry.rs` 的 `mod tests` 里加：

```rust
    #[test]
    fn the_lead_agent_flag_is_registered() {
        let def = find("lead_agent").expect("lead_agent must be in the registry");
        assert!(!def.default, "the lead agent must default to off");
        assert!(!def.summary.is_empty(), "a summary is what the settings page shows");
        assert!(!def.owner.is_empty(), "an expired flag needs someone to ask");
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-flags the_lead_agent_flag_is_registered
```

Expected: FAIL，`lead_agent must be in the registry`

- [ ] **Step 3: 实现**

在 `REGISTRY` 上方加常量：

```rust
/// 组长（lead agent）的总开关。名字一旦发布就是 `flags.json` 的键，不能改。
pub const LEAD_AGENT_FLAG: &str = "lead_agent";
```

把 `REGISTRY` 从空表改为：

```rust
pub const REGISTRY: &[FlagDef] = &[FlagDef {
    name: LEAD_AGENT_FLAG,
    summary: "组长：项目级常驻会话，负责分派与回收子任务",
    owner: "yichen",
    expires_on: "2027-01-31",
    default: false,
}];
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-flags
```

Expected: 18 passed（原 17 + 新增 1）

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-flags/src/registry.rs
git commit -m "feat(flags): 注册 lead_agent 开关"
```

---

### Task 2: 判定入口（纯值，不读全局）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/lead_gate.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`

**Interfaces:**
- Consumes: `yi_agent_flags::status_at`、`yi_agent_flags::build_kind`（仅在 `resolve_from_store` 里）
- Produces:
  - `pub const LEAD_AGENT_FLAG: &str = "lead_agent";`
  - `pub fn resolve_from_store(flags_path: &Path, build_kind: &str) -> bool`
  - `pub fn is_lead_agent_enabled(flag_value: Option<bool>) -> bool`

- [ ] **Step 1: 写失败测试**

创建 `lead_gate.rs`，先只放测试与文档：

```rust
//! 组长功能的开关判定。
//!
//! 组长是一整套跨层能力（7 个工具、1 个 RPC、惰性建会话、后台 tick）。它们必须
//! 由**同一个**判定控制——散落各处各写一遍 `is_enabled("lead_agent")` 时，抄错
//! 一个字符串就是静默失效，而且没有任何测试会失败。
//!
//! 判定**不读进程级全局量**：`yi_agent_flags::is_enabled` 依赖 `YI_AGENT_HOME`
//! 与进程级 `build_kind`，在并行测试里会互相污染。真实取值在 `serve()` 入口由
//! [`resolve_from_store`] 解析一次，之后随实例向下传。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_injected_value_wins() {
        assert!(is_lead_agent_enabled(Some(true)));
        assert!(!is_lead_agent_enabled(Some(false)));
    }

    #[test]
    fn a_missing_value_means_off() {
        // 生产路径永远显式给 `Some`；`None` 是「装配方漏传」的兜底，
        // 必须失败关闭（fail closed），不能默认打开一个未验证的功能。
        assert!(!is_lead_agent_enabled(None));
    }

    #[test]
    fn the_flag_name_matches_the_registry() {
        assert_eq!(LEAD_AGENT_FLAG, "lead_agent");
        assert!(
            yi_agent_flags::find(LEAD_AGENT_FLAG).is_some(),
            "the gate and the registry must agree on the name"
        );
    }

    #[test]
    fn a_stable_build_always_reports_off() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        std::fs::write(&path, r#"{"version":1,"flags":{"lead_agent":true}}"#).unwrap();
        assert!(
            !resolve_from_store(&path, "release"),
            "a stable build must ignore the file entirely"
        );
        assert!(
            resolve_from_store(&path, "dev"),
            "a dev build honors an explicit on"
        );
    }

    #[test]
    fn a_dev_build_without_the_file_reports_off() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        assert!(!resolve_from_store(&path, "dev"));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_gate
```

Expected: 编译失败，`cannot find function is_lead_agent_enabled`

- [ ] **Step 3: 实现**

```rust
use std::path::Path;

/// 组长总开关的名字。与注册表同源：`the_flag_name_matches_the_registry` 钉住一致。
pub const LEAD_AGENT_FLAG: &str = "lead_agent";

/// 从 `flags.json` + 构建类型解析生效值。**每个实例调用一次**（`serve()` 入口）。
///
/// 不叫 `is_enabled` 是为了避免与 `yi_agent_flags::is_enabled` 混淆：后者读进程
/// 级 `YI_AGENT_HOME`，本函数只读调用方给的路径。
pub fn resolve_from_store(flags_path: &Path, build_kind: &str) -> bool {
    // 复用库的取值链（默认 → 文件 → 构建类型硬闸），但不走它的环境变量层：
    // 环境变量同样是进程级的，会让并行测试互相污染。生产侧若要临时覆盖，
    // 在 `serve()` 入口自己读环境变量（见 Task 8）。
    matches!(
        yi_agent_flags::store::status_at(flags_path, LEAD_AGENT_FLAG, build_kind, None),
        (true, _)
    )
}

/// 判定入口。`None` = 装配方漏传，**失败关闭**。
pub fn is_lead_agent_enabled(flag_value: Option<bool>) -> bool {
    flag_value == Some(true)
}
```

在 `crates/yi-agent-app-server/src/lib.rs` 的 `pub mod flags_rpc;` 附近加：

```rust
pub mod lead_gate;
```

**注意**：`yi_agent_flags::store` 是 `pub mod`（已在 `lib.rs` 里声明），
`status_at` 也从 crate 根 re-export。两者都能用。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_gate
```

Expected: 5 passed

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-app-server/src/lead_gate.rs yi-agent-rs/crates/yi-agent-app-server/src/lib.rs
git commit -m "feat(app-server): 组长开关收成一个纯值判定入口"
```

---

### Task 3: 让开关随实例下传（`RuntimeAttachments` → `AppState`）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`RuntimeAttachments`、`serve()`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs`（`AppState`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`（`Harness`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` 的 `Host`（`mod tests` 内的另一个宿主）

**Interfaces:**
- Produces:
  - `RuntimeAttachments::lead_flags_path: PathBuf`（`flags.json` 的位置，`serve()` 里解析一次）
  - `AppState::lead_agent_enabled: bool`

**为什么走 `RuntimeAttachments` 而不是 `AppState` 直传**：`AppState` 是每个请求临时
构造的，字段来自主循环局部量；`lead_agent_enabled` 在主循环启动时算一次即可。
`RuntimeAttachments` 正是「进程级接线」的既有载体（`models_path`、`resident_dir`
都在那里），且测试 `Harness` 与内联 `Host` 都已经在构造它。

- [ ] **Step 1: 写失败测试**

在 `rpc_tests.rs` 的 `Harness` 区加一个最小验证：

```rust
#[tokio::test]
async fn a_harness_can_be_built_with_the_lead_flag_off() {
    let mut h = Harness::with_lead_flag(false).await;
    let response = rpc(&mut h, 2, "thread/lead", json!({})).await;
    assert_eq!(response["error"]["code"], json!(-32601), "{response}");
    h.shutdown().await;
}

#[tokio::test]
async fn a_harness_can_be_built_with_the_lead_flag_on() {
    let mut h = Harness::with_lead_flag(true).await;
    let response = rpc(&mut h, 2, "thread/lead", json!({ "cwd": "/tmp/yi-agent-lead-on" })).await;
    assert!(
        response.get("error").is_none(),
        "with the flag on, thread/lead must still work: {response}"
    );
    h.shutdown().await;
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib a_harness_can_be_built_with_the_lead_flag
```

Expected: 编译失败，`no function with_lead_flag`

- [ ] **Step 3: 加字段**

`server.rs` 的 `RuntimeAttachments`（约 `:243`，紧跟 `models_path`）：

```rust
    /// `flags.json` 的位置。`serve()` 在建附件时把它解析成「组长开关是否开」的
    /// **单个 bool**（见 `lead_agent_enabled`），而不是让下游各处分头读盘：
    /// 下游读的是进程级 `YI_AGENT_HOME`，在并行测试里会互相污染。
    pub(crate) lead_flags_path: PathBuf,
```

在同一个结构体末尾（`lead_control_replies` 之后）加解析后的值：

```rust
    /// 组长开关的**生效值**，进程启动时算一次。
    ///
    /// 与 `lead_flags_path` 同时存在是刻意的：路径是输入，值是结论。让每个
    /// 消费点自己从路径解析，就会有 N 份「怎么算」的副本，漂移时无声。
    pub(crate) lead_agent_enabled: bool,
```

`serve()` 的 destructure（约 `:2860`）加两行：

```rust
        lead_flags_path,
        lead_agent_enabled,
```

`ServeLauncher`（约 `:7900` 一带）加字段并透传。

`rpc/mod.rs` 的 `AppState` 加：

```rust
    /// 组长开关的生效值。`thread/lead` 与 `thread/listAll` 用它决定「这个能力
    /// 存在不存在」。按值拷贝（`bool`），与 `client_scope` 同类——都是每请求
    /// 读一次的判定输入。
    pub(crate) lead_agent_enabled: bool,
```

`server.rs:3150` 的 `app_state!` 宏里加一行：

```rust
                            lead_agent_enabled,
```

- [ ] **Step 4: 让测试宿主可注入**

**实际构造链（已核实，`rpc_tests.rs:1850` 起）**：

```
new() → with_factory → with_config
with_cfg / with_scope / with_watchman → with_config_and_scope
                          → with_config_and_scope_and_watchman   ← 这里才真正造 RuntimeAttachments
```

`with_config_and_scope_and_watchman`（`:1929` 起）是唯一构造 `RuntimeAttachments`
的地方，它内部造了 `index_dir`、`models_dir` 等隔离临时目录。**最小改动**：
给它加一个 `lead_agent_enabled: bool` 参数，其余三个包装层各加一个参数透传。

```rust
    fn with_config_and_scope_and_watchman<F>(
        cfg: RuntimeConfig,
        build: F,
        permission_timeout: Duration,
        scope: Scope,
        install: WatchmanInstall,
        uninstall: WatchmanUninstall,
        calls: WatchmanCalls,
        // 组长开关。走这里注入而非改 `flags.json`：后者要求 build_kind == "dev"
        // 且共享进程级 `YI_AGENT_HOME`，并行测试必然互相污染。
        lead_agent_enabled: bool,
    ) -> Self
```

在它内部造 `index_dir` 之后加：

```rust
        // `flags.json` 也落在隔离目录里：即便将来有代码去读它，也不会碰开发者
        // 真实的 `~/.yi-agent/flags.json`。
        let lead_flags_path = index_dir.path().join("flags.json");
```

`RuntimeAttachments { … }` 里加两行（`models_path` 之后）：

```rust
                lead_flags_path,
                lead_agent_enabled,
```

三个包装层（`with_config`、`with_config_and_scope`、`with_config_and_scope_and_watchman`
的调用者）各自加参数并传 `false`——**默认关**，这样 `Harness::new()` 的行为
就是「开关关」，与 stable 一致。新增公开构造器：

```rust
    /// 指定组长开关的 harness。
    ///
    /// 默认（`Harness::new()` / `with_cfg`）一律是**关**——那是 stable 的行为，
    /// 也是 fail-closed 的默认。需要组长的用例显式打开。
    pub(crate) async fn with_lead_flag(enabled: bool) -> Self {
        Self::with_config_and_lead_flag(test_config(), build_test_agent, PERMISSION_TIMEOUT, enabled)
    }

    /// 自定义 config + 组长开关：需要指定 cwd 的用例用它。
    pub(crate) async fn with_config_and_lead_flag(
        cfg: RuntimeConfig,
        build: F,
        permission_timeout: Duration,
        enabled: bool,
    ) -> Self
```

`server.rs` 的 `mod tests` 里那个内联 `Host`（约 `:7939`）同样补两个字段：

```rust
        lead_flags_path: theme_dir.path().join("flags.json"),
        lead_agent_enabled: false,
```

并在 `Host` 上加 `lead_agent_enabled: bool` 字段（`new()` 里置 `false`），需要
组长的用例用 `Host::with_lead(true)`（照 `new()` 写，只改这一个字段）。

`RuntimeAttachments` 的**所有**其他构造点（`rpc_tests.rs:3638` 等）补
`lead_flags_path: PathBuf::new()`、`lead_agent_enabled: false`。

⚠️ `RuntimeAttachments` 没有 `..Default::default()` 可用，且字段全是非 `Option`。
补齐时**以编译器为准**逐点补，不要靠搜索记忆——漏一个就是编译错误（这是好事）。

- [ ] **Step 5: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo build -p yi-agent-app-server 2>&1 | grep -E "^error" | head -20
```

按报错逐个补齐构造点。Expected: 编译通过，且**既有测试全部通过**：

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib
```

Expected: 与改动前同样的通过数（开关默认关，而组长工具此刻还没加门禁，
所以行为未变）。

⚠️ Step 1 那两条 `a_harness_can_be_built_with_the_lead_flag_*` 断言的是
`thread/lead` 的返回码，而门禁要到 Task 5 才加。**此刻它们会失败**——这是
预期的。先只保留它们、跑其余测试确认无回归：

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib -- --skip a_harness_can_be_built_with_the_lead_flag
```

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-app-server/src
git commit -m "feat(app-server): 组长开关随实例下传，不走进程级全局量

yi_agent_flags::is_enabled 读 YI_AGENT_HOME 与进程级 build_kind，
在并行测试里会互相污染；且本 crate 从不注入 build info，默认恒为 release，
写 flags.json 也不生效。故把开关解析成 bool 随 RuntimeAttachments 传。"
```

---

### Task 4: 关掉开关时不注册组长工具

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:1116`（`build_runtime_tooling`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:510`（`attach_delegation`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:6386`（`start_thread_core`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs:496`（`thread/resume` 调用点）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`

**Interfaces:**
- Consumes: `crate::lead_gate::is_lead_agent_enabled`
- Produces: `build_runtime_tooling(..., anchor_sha, lead_agent_enabled: Option<bool>)`

- [ ] **Step 1: 写失败测试**

**本 crate 没有列举工具的 RPC**（已核实：`agent/*` 只有 `children/list`、
`trace/*`、`message`、`cancel*`）。工具集的唯一观测点是直接调
`build_runtime_tooling(...).registry.names()`——`rpc_tests.rs:17315` 起
（`a_card_session_has_no_lead_to_ask`）就是这么写的，照抄它的装配骨架。

在 `rpc_tests.rs` 的 `mod tests` 里加：

```rust
/// 造一份已 attach 的 root，供工具注册断言复用。
///
/// 骨架照抄 `a_card_session_has_no_lead_to_ask`（`rpc_tests.rs:17315`）：
/// 真 git 仓库 + `attach_project_runtime` + `RuntimeBinding::managed`。
fn tooling_names_for(
    repo: &std::path::Path,
    runtime: &std::path::Path,
    thread_id: &str,
    project_key: String,
    lead_agent_enabled: Option<bool>,
) -> Vec<String> {
    let mut cfg = test_config();
    cfg.workdir = repo.to_path_buf();
    let attached = Arc::new(
        yi_agent_subagent::attach::attach_project_runtime(
            &cfg,
            runtime.to_path_buf(),
            &isolated_models_path(runtime),
        )
        .expect("a clean git repo must attach"),
    );
    let binding = RuntimeBinding::managed(
        &cfg,
        runtime.to_path_buf(),
        isolated_models_path(runtime),
        Arc::clone(&attached),
    );
    let root = ThreadRoot::from_handle(binding, attached.attached_root.clone());
    build_runtime_tooling(
        &cfg,
        &root,
        thread_id,
        yi_agent_core::autonomy::YoloSwitch::new(false),
        test_theme(),
        crate::git_diff_tool::GitDiffHandle::new(),
        Vec::new(),
        project_key,
        crate::lead::LeadInbox::new(),
        crate::lead::LeadReplies::new(),
        crate::lead_control::LeadControlInbox::new(),
        crate::lead_control::LeadControlReplies::new(),
        None,              // anchor_sha
        lead_agent_enabled,
    )
    .expect("tooling")
    .registry
    .names()
}

/// 开关关掉时，**任何**会话都没有组长工具。
///
/// 这是「开关只能关不能开」的硬证据：普通会话拿不到 `ask_lead`，组长会话也
/// 拿不到那 6 个工具。
#[test]
fn no_lead_tools_exist_when_the_flag_is_off() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let project_key = repo.path().join(".yi-agent").display().to_string();

    let plain = tooling_names_for(
        repo.path(),
        runtime.path(),
        "thread-plain",
        project_key.clone(),
        Some(false),
    );
    assert!(
        !plain.contains(&"ask_lead".to_string()),
        "a plain session must have no ask_lead when the flag is off: {plain:?}"
    );

    // 组长会话:落一份 `role:"lead"` 的 meta,让 `is_lead_thread` 为真。
    let store = crate::thread_store::ThreadStore::new(repo.path());
    let (lead_meta, _) = crate::lead::ensure_lead_meta(&store, repo.path()).expect("lead meta");
    let lead = tooling_names_for(
        repo.path(),
        runtime.path(),
        &lead_meta.thread_id,
        project_key,
        Some(false),
    );
    assert!(
        !lead.iter().any(|n| n.contains("lead")),
        "a lead must have no lead tools when the flag is off: {lead:?}"
    );
}

/// 开关打开时，两类会话各拿各的工具，且互不越界。
#[test]
fn lead_tools_follow_the_flag() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let project_key = repo.path().join(".yi-agent").display().to_string();

    let plain = tooling_names_for(
        repo.path(),
        runtime.path(),
        "thread-plain",
        project_key.clone(),
        Some(true),
    );
    assert!(
        plain.contains(&"ask_lead".to_string()),
        "a plain session gets ask_lead when the flag is on: {plain:?}"
    );
    assert!(
        !plain.contains(&"lead_remember".to_string()),
        "the flag must not turn a plain session into a lead: {plain:?}"
    );

    let store = crate::thread_store::ThreadStore::new(repo.path());
    let (lead_meta, _) = crate::lead::ensure_lead_meta(&store, repo.path()).expect("lead meta");
    let lead = tooling_names_for(
        repo.path(),
        runtime.path(),
        &lead_meta.thread_id,
        project_key,
        Some(true),
    );
    for name in [
        "lead_remember",
        "lead_recall",
        "lead_list_threads",
        "lead_read_thread",
        "lead_interrupt",
        "lead_reject",
    ] {
        assert!(
            lead.contains(&name.to_string()),
            "{name} must be registered for a lead when the flag is on: {lead:?}"
        );
    }
    assert!(
        !lead.contains(&"ask_lead".to_string()),
        "a lead must not ask itself: {lead:?}"
    );
}
```

**注意**：`ensure_lead_meta` 返回 `(ThreadMeta, bool)`；第二条断言用它落一份真
meta，而不是手搓「像组长」的形状——`is_lead_thread` 读的就是这份落盘值。

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib a_lead_thread_has_no_lead_tools
```

Expected: FAIL，`no lead tool may exist when the flag is off`（关掉时仍注册了 `ask_lead`）

- [ ] **Step 3: 给三处加参数并加门禁**

`build_runtime_tooling` 签名末尾（`anchor_sha` 之后）：

```rust
    // 组长开关。逐调用传入而非读全局：见 `lead_gate` 的模块文档。
    // `None` = 装配方漏传，按关处理（fail closed）。
    lead_agent_enabled: Option<bool>,
) -> Result<RuntimeTooling, String> {
```

函数体里、`let is_lead = ...` **之后**：

```rust
    let lead_enabled = crate::lead_gate::is_lead_agent_enabled(lead_agent_enabled);
```

两个注册块的条件改为：

```rust
    if lead_enabled && !is_lead && !crate::plugin_worktree::is_plugin_worktree(session_cwd) {
        // …原有 AskLeadTool 注册代码不动
    }
```

```rust
    if lead_enabled && is_lead {
        // …原有 6 个组长工具注册代码不动
    }
```

`attach_delegation` 签名加 `lead_agent_enabled: Option<bool>`，原样传给
`build_runtime_tooling`。

`start_thread_core` 签名加 `lead_agent_enabled: Option<bool>`，原样传给
`attach_delegation`。

- [ ] **Step 4: 更新调用点**

| 位置 | 传什么 |
|---|---|
| `server.rs:6444`（`start_thread_core` 内 → `attach_delegation`） | 参数透传 |
| `rpc/thread.rs:496`（`thread/resume` → `attach_delegation`） | `Some(state.lead_agent_enabled)` |
| `rpc/thread.rs` 的 `thread/start` → `start_thread_core` | `Some(state.lead_agent_enabled)` |

```bash
cd yi-agent-rs && cargo build -p yi-agent-app-server 2>&1 | grep -E "^error" | head -20
```

Expected: 编译通过。

- [ ] **Step 5: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib
```

Expected: 全部通过。**已有的组长工具测试会失败**——它们现在需要开关开着。
把这些测试改用 `Harness::with_lead_flag(true)`。逐个改，不要图省事把
`Harness::new()` 的默认值改成开。

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-app-server/src
git commit -m "feat(app-server): 开关关掉时不注册任何组长工具

门禁加在装配期（build_runtime_tooling）而不是每个工具的调用点：
调用点分散，漏一个就是一个仍在工作的组长能力。"
```

---

### Task 5: `thread/lead` 在开关关掉时返回 `-32601`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs:312`
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`

**Interfaces:**
- Consumes: `AppState::lead_agent_enabled`

- [ ] **Step 1: 写失败测试**

Task 3 已写好 `a_harness_can_be_built_with_the_lead_flag_off`。把它改成显式断言：

```rust
#[tokio::test]
async fn thread_lead_is_method_not_found_when_the_flag_is_off() {
    let mut h = Harness::with_lead_flag(false).await;
    let response = rpc(&mut h, 2, "thread/lead", json!({})).await;
    assert_eq!(
        response["error"]["code"],
        json!(-32601),
        "a disabled lead must look non-existent, not forbidden: {response}"
    );
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("thread/lead"),
        "the error must name the method so the client can recognize it: {response}"
    );
    h.shutdown().await;
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_lead_is_method_not_found
```

Expected: FAIL，实际返回 `thread_id`

- [ ] **Step 3: 实现**

在 `rpc/thread.rs` 的 `"thread/lead" => {` 之后、`resolve_thread_cwd` **之前**：

```rust
        "thread/lead" => {
            // 关掉开关时这个方法**不存在**（-32601），而不是「存在但拒绝」：
            // 前端只需看 `build/info` + `flags/list`，不必再判断权限。
            //
            // 放在参数校验之前：否则关掉时传错参数会得到 `invalid_params`，
            // 让人以为是调用方的问题，而不是「这个能力没有」。
            if !state.lead_agent_enabled {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::method_not_found("thread/lead")),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            // …原有 resolve_thread_cwd 起
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_lead
```

Expected: 新增 1 个通过。**已有的 `thread/lead` 测试会失败**（约 15 处，见
`rpc_tests.rs` 的 `9302`、`9341`、`9367`、`9644`、`9872`、`10079`、`10328`、
`10357`、`12010`、`17066`、`17170` 等），全部改用
`Harness::with_lead_flag(true).await`。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-app-server/src
git commit -m "feat(rpc): thread/lead 在开关关掉时返回 -32601"
```

---

### Task 6: `thread/listAll` 不再为空项目种组长

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs:1999`（`scan_workspace_metas`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs:2038`（`build_list_all_response`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs:141`（调用点）

**Interfaces:**
- Produces: `scan_workspace_metas(dirs: &[String], seed_lead: bool)`、
  `build_list_all_response(dirs, workspaces, statuses, seed_lead: bool)`

**为什么必须做**：这是最隐蔽的入口。它跑在 `spawn_blocking` 里、与 RPC 参数无关，
只看「这个目录有没有会话」。不加门禁的话，用户关掉开关、重启 app，每次
`thread/listAll` 依然会为每个空项目建组长——用户看不见，但它真实存在、占一个
thread，开关重新打开时全部复活。

- [ ] **Step 1: 写失败测试**

**`Harness` 没有 `workdir()` 访问器**（已核实）。要指定 cwd，用
`Harness::with_cfg(cfg)`——`isolated_harness()`（`rpc_tests.rs:9238`）就是这么写的：

```rust
#[tokio::test]
async fn list_all_does_not_seed_a_lead_when_the_flag_is_off() {
    let dir = tempfile::TempDir::new().unwrap();
    // 一个存在、但没有任何 `.yi-agent/threads` 的目录，正是会被懒建组长的形状。
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config_and_lead_flag(
        cfg,
        build_test_agent,
        PERMISSION_TIMEOUT,
        false,
    )
    .await;
    let _ = start_thread(&mut h).await; // 让 workdir 进 workspace 索引

    let listed = list_all(&mut h).await;
    let seeded = listed["result"]["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .filter_map(|g| g["threads"].as_array())
        .flatten()
        .any(|t| t["role"] == json!("lead"));
    assert!(!seeded, "a disabled lead must not be seeded: {listed}");

    // 副作用必须真的没发生——响应干净不等于磁盘干净。
    let on_disk = std::fs::read_dir(dir.path().join(".yi-agent/threads"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|e| {
                    std::fs::read_to_string(e.path())
                        .map(|text| text.contains("\"role\":\"lead\"") || text.contains("\"lead\""))
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0);
    assert_eq!(on_disk, 0, "no lead may be written to disk when the flag is off");

    h.shutdown().await;
}
```

`list_all(&mut h)`（`rpc_tests.rs:8734`）是现成助手，直接复用。

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib list_all_does_not_seed_a_lead
```

Expected: FAIL，`a disabled lead must not be seeded`

- [ ] **Step 3: 实现**

`scan_workspace_metas` 签名：

```rust
fn scan_workspace_metas(
    dirs: &[String],
    // 是否为空项目懒建组长。与 `ensure_lead_meta` 的副作用一一对应：
    // 关掉时这个函数**只读**，不写盘。
    seed_lead: bool,
) -> Vec<(String, Option<Vec<crate::thread_store::ThreadMeta>>)> {
```

种组长那段改为：

```rust
        if seed_lead && metas.is_empty() && !crate::plugin_worktree::is_plugin_worktree(path) {
            // …原有 ensure_lead_meta 代码不动
        }
```

`build_list_all_response` 签名加 `seed_lead: bool`，透传给 `scan_workspace_metas`。

调用点（`thread.rs:141`）：

```rust
            let dirs = state.workspaces.list();
            // 开关在**这里**取值并移进闭包：`spawn_blocking` 的闭包里读不到
            // `state`（它已被下面的 `spawn` 移走，且那是阻塞线程池）。
            let seed_lead = state.lead_agent_enabled;
            …
            tokio::task::spawn_blocking(move || {
                build_list_all_response(&dirs, &workspaces, &statuses, seed_lead)
            })
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib
```

Expected: 全部通过。**已有的「空项目会被种组长」测试会失败**，改用
`Harness::with_lead_flag(true)`。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-app-server/src
git commit -m "feat(rpc): 开关关掉时 thread/listAll 不再为空项目种组长

这是最隐蔽的一个入口：它跑在 spawn_blocking 里、与 RPC 参数无关，只看
目录有没有会话。漏掉它等于关掉开关后后台仍在造会话。"
```

---

### Task 7: 开关关掉时不跑组长后台 tick

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:3496`
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` 的 `mod tests`

- [ ] **Step 1: 写失败测试**

`lead_ticker.tick()` 臂内联在 `serve()` 的主循环里，无法直接单测。用源码断言
钉住那一行（看起来笨，但这是不起整个 app-server 就能验证「门禁还在」的唯一办法，
删除门禁时会立刻失败）：

```rust
    /// 组长后台 tick 的开关门禁必须真的在臂里。
    ///
    /// 不测 `serve()` 的 `select!` 臂本身——它内联在主循环里，要起完整
    /// app-server 才碰得到。臂里只有一行 `if !enabled { continue }`，断言它存在
    /// 就够；行为由 `lead_gate` 的单测覆盖。
    #[test]
    fn the_lead_tick_arm_checks_the_flag() {
        let source = include_str!("server.rs");
        let arm = source
            .split("_ = lead_ticker.tick() => {")
            .nth(1)
            .expect("the lead ticker arm must exist");
        let head: String = arm.lines().take(8).collect::<Vec<_>>().join("\n");
        assert!(
            head.contains("lead_agent_enabled"),
            "the lead tick arm must check the flag before doing any work: {head}"
        );
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib the_lead_tick_arm_checks
```

Expected: FAIL，`the lead tick arm must check the flag`

- [ ] **Step 3: 实现**

在 `server.rs:3496` 的臂内、原有逻辑之前：

```rust
            _ = lead_ticker.tick() => {
                // 关掉开关时不做任何事：否则会继续驱动用户看不见的组长会话，
                // 白耗 token。用进程启动时算好的值（与工具注册同一个来源），
                // 不在这里重读盘——重读会让同一进程里「工具没有、tick 有」这种
                // 半开状态成为可能。
                if !lead_agent_enabled {
                    continue;
                }
                // …原有逻辑
```

**注意**：容器内侧栏的入口靠 `flags/list`（Task 9），因此开关变更后需要
`thread/resume` 或重启才生效。这与 spec §6「不热加载」一致。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib the_lead_tick_arm_checks
```

Expected: 1 passed

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): 开关关掉时不跑组长后台 tick"
```

---

### Task 8: `serve()` 入口解析开关

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`serve()` 与 `serve_scoped_core`）

**Interfaces:**
- Consumes: `crate::lead_gate::resolve_from_store`

- [ ] **Step 1: 写失败测试**

这是装配代码，用源码断言 + 已有的 harness 测试覆盖：

```rust
    /// `serve()` 必须把 `lead_flags_path` 解析成 `lead_agent_enabled`。
    ///
    /// 漏掉这一步的后果是**静默的**：`RuntimeAttachments` 里那个 bool 保持
    /// `false`，所有开关永远关——而所有测试都通过（它们本来就在关的状态下跑）。
    #[test]
    fn serve_resolves_the_lead_flag_from_its_path() {
        let source = include_str!("server.rs");
        assert!(
            source.contains("crate::lead_gate::resolve_from_store(&lead_flags_path"),
            "serve() must resolve the flag from the injected path"
        );
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib serve_resolves_the_lead_flag
```

Expected: FAIL

- [ ] **Step 3: 实现**

`serve()` 的 destructure 之后（`let RuntimeAttachments { … } = attachments;` 之后）
立刻：

```rust
    // 开关只在这里解析一次：`lead_flags_path` 是注入的（测试给隔离目录），
    // `build_kind` 是进程级常量。两者都是输入，结论只算一次，避免下游各处分头
    // 读盘造成的「同一进程里有的功能开、有的关」。
    //
    // 环境变量层刻意不用（`status_at(..., None)`）：它同样是进程级的，会让并行
    // 测试互相污染。要临时覆盖请改 `flags.json`。
    let lead_agent_enabled =
        crate::lead_gate::resolve_from_store(&lead_flags_path, yi_agent_flags::build_kind());
```

生产侧构造 `RuntimeAttachments` 的两处（`server.rs:1712`、`:1798`）补：

```rust
            lead_flags_path: yi_agent_flags::store::flags_path()
                .unwrap_or_else(|| PathBuf::from("flags.json")),
            lead_agent_enabled: false, // serve() 会立刻覆盖
```

`lead_agent_enabled: false` 只是让结构体可以构造；`serve()` 的覆盖是唯一真值来源。
**不要**在这里提前解析——那会出现两份解析逻辑。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib serve_resolves_the_lead_flag
```

Expected: 1 passed

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib
```

Expected: 全部通过

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): serve() 入口解析组长开关"
```

---

### Task 9: 前端按开关隐藏组长入口

**Files:**
- Modify: `desktop/src/App.tsx`
- Modify: `desktop/src/components/ThreadSidebar.tsx`
- Test: `desktop/src/components/ThreadSidebar.test.tsx`

**Interfaces:**
- Produces: `ThreadSidebar` 新增 prop `leadAgentEnabled?: boolean`（缺省 `true`，
  保持既有测试与调用点不变）

**实际 helper（已核实，不要写错）**：

- `thread(id: string, title: string, cwd: string, tag?: string): ThreadSummary`
- `pinnedThread(id, title)`
- `sidebarProps(overrides)` / `renderSidebar(overrides)`
- 测试用 **`groups`** prop（不是 `workspaces`），`workspaces` 是「已关联目录」列表
- 组长构造方式：`{ ...thread("lead-1", "组长", "/p"), role: "lead" }`

- [ ] **Step 1: 写失败测试**

在 `ThreadSidebar.test.tsx` 的 `describe("ThreadSidebar lead entry")` 里加：

```tsx
  it("renders the lead as an ordinary row when the flag is off", () => {
    const lead: ThreadSummary = { ...thread("lead-1", "组长", "/p"), role: "lead" };
    const { container } = renderSidebar({
      groups: [{ workspace: "/p", exists: true, threads: [lead] }],
      leadAgentEnabled: false,
    });

    // 没有专属组长行，也就没有头像。
    expect(container.querySelector("[data-lead-row]")).toBeNull();
    expect(container.querySelector("[data-lead-avatar]")).toBeNull();
    // 但会话本身还在：关掉开关是「不标识」，不是「藏起来」。
    expect(screen.getByText("组长")).toBeTruthy();
  });

  it("renders the lead entry when the flag is on", () => {
    const lead: ThreadSummary = { ...thread("lead-1", "组长", "/p"), role: "lead" };
    const { container } = renderSidebar({
      groups: [{ workspace: "/p", exists: true, threads: [lead] }],
      leadAgentEnabled: true,
    });
    expect(container.querySelector("[data-lead-row]")).not.toBeNull();
  });

  it("keeps the lead entry visible by default", () => {
    // 缺省 = 不传 prop，既有调用点与测试的行为必须不变。
    const lead: ThreadSummary = { ...thread("lead-1", "组长", "/p"), role: "lead" };
    const { container } = renderSidebar({
      groups: [{ workspace: "/p", exists: true, threads: [lead] }],
    });
    expect(container.querySelector("[data-lead-row]")).not.toBeNull();
  });
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx
```

Expected: FAIL，`renders the lead as an ordinary row when the flag is off`

- [ ] **Step 3: 实现 `ThreadSidebar`**

props 里加：

```tsx
  /**
   * 组长开关。`undefined` 按 `true` 处理——既有调用点与测试不必逐个改，
   * 且「读不到开关」时维持当前行为（组长可见）而不是突然全部消失。
   *
   * 关掉时**只影响标识**：组长会话本身照常出现在列表里（不排除、不删除）。
   */
  leadAgentEnabled?: boolean;
```

在 `const lead = findLeadThread(...)` / `const leadId = ...` 处改成（实际位置
`:759-760`）：

```tsx
  const lead = findLeadThread(groups.flatMap((g) => g.threads));
  // 开关关掉时组长会话要作为**普通行**显示，故不再从普通列表里排除它。
  // 若仍排除，它会既不在组长行、也不在普通列表——彻底消失。
  const showLead = (leadAgentEnabled ?? true) && lead !== null;
  const leadId = showLead ? (lead?.thread_id ?? null) : null;
```

`leadVisible`（`:827`）的定义里也有 `lead !== null`，保持不变即可（它再与
`showLead` 相乘，关掉时恒 false）。渲染处（`:1106`）改成：

```tsx
          {showLead && leadVisible && lead && <div>{renderLeadRow(lead)}</div>}
```

`LeadAvatar` 只在 `renderLeadRow` 里出现（`:785`），组长行不渲染时它自然不出现，
无需单独改。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx
```

Expected: 新增 3 个通过，既有全部通过

- [ ] **Step 5: 实现 `App.tsx`**

state（放在 `buildKind` 旁边，约 `:208`）：

```tsx
  /**
   * 组长开关。`null` = 还没读到；按**关**处理——宁可晚一帧出现组长入口，
   * 也不要在关掉开关的实例上闪一个用不了的入口。
   */
  const [leadAgentEnabled, setLeadAgentEnabled] = useState<boolean | null>(null);
```

握手时（`build/info` 那段之后，约 `:1398`）读：

```tsx
        // 与 `build/info` 同一时机：都不是会频繁变的东西。
        // stable 上 `flags/list` 是 -32601（见 `flags_rpc.rs` 的 `build_kind != "dev"`
        // 分支），此时组长本就不存在，按关处理。
        try {
          const flags = await client.request<{ flags?: Array<{ name?: string; enabled?: unknown }> }>(
            "flags/list",
            {},
          );
          setLeadAgentEnabled(flags.flags?.find((f) => f.name === "lead_agent")?.enabled === true);
        } catch {
          setLeadAgentEnabled(false);
        }
```

补建循环加门禁（`:1463`）：

```tsx
        // 开关关掉时不补建组长：否则每次启动都会为每个项目建一个用户看不见的
        // 组长会话。`leadEnsured` 在关闭时也要置位——没有理由在会话内反复重试。
        //
        // 必须用 `=== true`，不能用 `!== false`：初始值是 `null`，`!== false`
        // 会让补建循环在 `flags/list` 返回前就执行，等于门禁失效。
        if (!leadEnsured.current && leadAgentEnabled === true) {
```

透传给侧栏（`:2134`）：

```tsx
        leadAgentEnabled={leadAgentEnabled === true}
```

- [ ] **Step 6: 跑桌面全量**

```bash
cd desktop && npx vitest run && npx tsc --noEmit
```

**`App.test.tsx` 必须补一处 mock**（已核实）：那里的假 client 对未知方法返回 `{}`
（`App.test.tsx` 末尾 `return {};`），于是 `flags/list` 返回 `{}` →
`flags.flags` 为 `undefined` → 组长开关被置为关 → 既有的两条组长用例
（`:5042`「asks thread/lead for a project group that has no lead, and shows the
lead」与 `:5062`「still paints the first screen when thread/lead fails」）失败。

在 `if (method === "thread/lead")` **之前**加：

```tsx
      if (method === "flags/list") {
        // 与 `build/info` 同一个时机被读。这里必须给 `lead_agent: true`：
        // 既有的组长用例断言的是「开关打开时」的完整行为。
        return { flags: [{ name: "lead_agent", enabled: state.leadFlagEnabled }] };
      }
```

并在 `state` 里加 `leadFlagEnabled: true`（默认开，保持既有用例语义），再新增一条
覆盖「关掉时补建不发生」的用例：

```tsx
  it("does not ask thread/lead when the lead flag is off", async () => {
    state.leadFlagEnabled = false;
    // …按 :5042 那条用例准备 workspaces / 挂载…
    expect(
      state.leadAttempts.filter((cwd) => cwd === "/w"),
      "the client must not seed a lead when the flag is off",
    ).toHaveLength(0);
  });
```

其余失败按报错逐个处理；**不要**为了过测试把 `flags/list` 的 mock 删掉。

添加这条用例前先确认 `state` 在该文件里的复位机制（搜 `beforeEach`），
把 `leadFlagEnabled` 一并复位成 `true`，否则它会泄漏到下一条用例。

- [ ] **Step 7: Commit**

```bash
git add desktop/src/App.tsx desktop/src/components/ThreadSidebar.tsx desktop/src/components/ThreadSidebar.test.tsx
git commit -m "feat(desktop): 组长入口跟着 lead_agent 开关显示

关掉时只去掉标识与补建，不去掉会话：组长 thread 照常作为普通会话列出。"
```

---

### Task 10: 文档与全量验收

**Files:**
- Modify: `docs/project-management/yi-agent-flags.md`
- Modify: `docs/project-management/README.md`（计数）
- Modify: `docs/project-management/desktop.md`、`docs/project-management/yi-agent-app-server.md`

- [ ] **Step 1: 更新文档**

在 `docs/project-management/yi-agent-flags.md` 的 Features 末尾加：

```markdown
- [x] `lead_agent` 开关 — 组长（lead agent）是一个注册项，默认关。关掉后：7 个组长工具都不注册（`server.rs` 的 `build_runtime_tooling`）、`thread/lead` 返回 `-32601`（`rpc/thread.rs`）、`thread/listAll` 不为空项目种组长（`scan_workspace_metas`）、组长后台 tick 不跑（`lead_ticker.tick()` 臂）、前端不显示组长行与 `LeadAvatar`（`ThreadSidebar.tsx`）。判定收在 `crates/yi-agent-app-server/src/lead_gate.rs`，**按实例注入**而非读进程级 `yi_agent_flags::is_enabled`——后者依赖 `YI_AGENT_HOME` 与进程级 `build_kind`，并行测试下会互相污染。**开关只能关不能开**：它不能把普通会话变成组长。已有组长会话的 `role` 与记忆账本都不动，关掉时按普通会话显示。判据：`cargo test -p yi-agent-app-server --lib lead`（含 `thread_lead_is_method_not_found_when_the_flag_is_off`、`list_all_does_not_seed_a_lead_when_the_flag_is_off`、`the_lead_tick_arm_checks_the_flag`、`serve_resolves_the_lead_flag_from_its_path`）；前端：`npx vitest run src/components/ThreadSidebar.test.tsx`
```

在 `yi-agent-app-server.md` 与 `desktop.md` 里给对应的组长功能行加一句
「受 `lead_agent` 开关控制」。

同步 `README.md` 里 `yi-agent-flags` 的计数。

- [ ] **Step 2: 全量验收**

```bash
ps aux | grep -v grep | grep -E "cargo|rustc" || echo clean
cd yi-agent-rs
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p yi-agent-flags
cargo test -p yi-agent-app-server --lib
cargo test -p yi-agent --bin yi-agent
bash scripts/test_dev_env.sh
cd ../desktop && npx vitest run && npx tsc --noEmit
```

Expected：全部通过；`yi-agent-flags` 18 passed。

- [ ] **Step 3: 端到端复核**

```bash
cd yi-agent-rs
export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
cargo build -p yi-agent
export YI_AGENT_HOME=$(mktemp -d)
printf '{"version":1,"models":[],"default_model":null}' > $YI_AGENT_HOME/models.json

./target/debug/yi-agent flags list                  # 期望有 lead_agent 一行，off
./target/debug/yi-agent flags status lead_agent     # 期望 source: default
./target/debug/yi-agent flags set lead_agent on
./target/debug/yi-agent flags status lead_agent     # 期望 source: file，enabled=true
```

**注意**：CLI 只写 `flags.json`；**生效**还要求构建类型是 `dev`
（`resolve_from_store` 的硬闸）。用 `just dev-build` 产出的二进制验证。

- [ ] **Step 4: Commit**

```bash
git add docs
git commit -m "docs: lead_agent 开关的进度与判据"
```

---

## Self-Review

**Spec coverage:**

| spec 章节 | 对应 Task |
|---|---|
| §3 L1 单开关 | Task 1 |
| §3 L2 判定收敛 | Task 2（`lead_gate`） |
| §3 L3 只能关不能开 | Task 4（`lead_enabled && is_lead`） |
| §3 L4 `thread/lead` → `-32601` | Task 5 |
| §3 L5 保留已有数据 | Task 9（不删 meta、不排除普通列表） |
| §3 L6 记忆不注入不写入 | Task 4（工具没注册即无写入口） |
| §3 L7 后台 tick 不跑 | Task 7 |
| §3 L8 前端靠 `flags/list` | Task 9 |
| §3 L9 惰性建组长加门禁 | Task 5 + Task 6 |
| §4.1 工具注册 | Task 4 |
| §4.2 RPC | Task 5 |
| §4.3 惰性建组长 | Task 6 |
| §4.4 后台 tick | Task 7 |
| §4.5 前端 | Task 9 |
| §5 开关定义 | Task 1 |
| §6 不热加载 | 设计决策；Task 10 Step 3 覆盖「改开关后重启」 |
| §8 测试策略 | Task 4/5/6/7/9 的首步 |
| §10 判定点总表 | Task 4-9 |

**Placeholder scan:** 无 TBD/TODO，无「按现场确认」。

执行前已核实的事实（写进计划，不再是待确认项）：

| 事实 | 结论 |
|---|---|
| `agent/tools/list` | **不存在**（`agent/*` 只有 `children/list`、`trace/*`、`message`、`cancel*`）。工具集只能经 `build_runtime_tooling(...).registry.names()` 观测 |
| `Harness::workdir()` | **不存在**。指定 cwd 用 `Harness::with_cfg(cfg)` |
| `Harness` 构造链 | `with_config_and_scope_and_watchman` 是唯一造 `RuntimeAttachments` 的地方 |
| `rpc()` helper | `rpc(h: &mut Harness, id: u64, method: &str, params: Value) -> Value` |
| `list_all()` helper | 存在（`rpc_tests.rs:8734`） |
| `start_thread()` helper | 存在（`rpc_tests.rs:2224`） |
| `isolated_harness()` | 存在（`rpc_tests.rs:9238`） |
| `thread()` / `renderSidebar()` | `thread(id, title, cwd, tag?)`、`renderSidebar(overrides)`，测试用 `groups` prop |
| `leadVisible` | `ThreadSidebar.tsx:827`，渲染处 `:1106` |
| `LeadAvatar` | 只在 `renderLeadRow` 内（`:785`），随组长行一起消失 |
| `App.test.tsx` 假 client | 未知方法返回 `{}`，故必须显式补 `flags/list` |
| `flags` 取值链 | `status_at` 第一条即 `build_kind != "dev"` → 恒 false；`build_kind()` 是进程级 `OnceLock` |

**Type consistency:**

- `LEAD_AGENT_FLAG` 在 Task 1（`yi-agent-flags`）与 Task 2（`lead_gate`）各定义一次，
  Task 2 的 `the_flag_name_matches_the_registry` 钉住一致。
- `resolve_from_store(&Path, &str) -> bool` 在 Task 2 定义，Task 8 唯一调用。
- `is_lead_agent_enabled(Option<bool>) -> bool` 在 Task 2 定义，Task 4 唯一调用。
- `lead_agent_enabled: bool` 在 Task 3 定义（`RuntimeAttachments` 是进程值、
  `AppState` 是每请求副本），Task 4/5/6/7 只读不重定义。
- `scan_workspace_metas(dirs, seed_lead: bool)` 与
  `build_list_all_response(dirs, workspaces, statuses, seed_lead: bool)` 在 Task 6
  同步改签名，调用点一并改。
- `ThreadSidebar` 的 `leadAgentEnabled?: boolean` 在 Task 9 定义，
  `App.tsx` 传 `leadAgentEnabled === true`。
