# TUI 未完成 slash 命令补齐 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 TUI 里未接通的 slash 命令补齐（`/config`、`/model`、`/daemon`），把无后端支撑的命令从弹窗目录隐藏但保留识别与理由（`/approve` `/deny` `/budget` `/priority`），并修复 `/compact` 完成状态在 TUI 路径上的回归。

**Architecture:** 所有改动集中在 `crates/yi-agent` 的 TUI 与 driver，外加 `yi-agent-core` 一个新 `AgentEvent` 变体。`/config` 用启动时组装的只读快照；`/model` 复用 driver 已有的「重建 agent、保留 session」模式并广播新事件；`/daemon` 复用既有 IPC 同步命令路径。隐藏命令靠「目录视图分裂」：`all()` 保持不变供解析，新增 `completable()` 供弹窗与 `/help` 全量列表。

**Tech Stack:** Rust、ratatui、tokio mpsc、clap（CLI，本期不改）、`yi_agent_store::ipc`（daemon 控制）。

## Global Constraints

- **严禁在 `main` 分支直接改代码**：本计划在 worktree `.worktrees/tui-slash-complete`（分支 `feat/tui-slash-command-completion`）中执行。
- **提交前必须 `cargo fmt --all`**（在 `yi-agent-rs/` 下）；commit message 用 conventional commits，首行 ≤72 字符，**不要**写 `Co-Authored-By: Claude`。
- **不要并行跑多个 `cargo test`**：跑前 `ps aux | grep cargo` 确认无残留；按 crate 跑，避免 `--workspace` 全量。
- **不新增后端能力**：不实现 `/approve` `/deny` `/budget` `/priority` 的 core/store/IPC 支持；不改 CLI grammar 与 `ControlCommand` 目录项。
- **绝不渲染 `api_key`**（`/config` 及任何输出）。
- **隐藏项文案统一**：`/approve`、`/deny` 用「子 agent 验收由父 agent 真实合并后 daemon 自动观察，无交互式权限审批」；`/budget`、`/priority` 用「daemon 尚未提供预算/优先级写接口」。二者都表述为「`/<name>` 暂不支持：<原因>」。
- **`current_model` 单一真值**：只存在于 `run_loop` 的局部变量，状态栏与 `/config` 都读它。
- **测试命令**（各任务验证用）：
  - `cargo test -p yi-agent-core --lib`
  - `cargo test -p yi-agent --bin yi-agent tui::slash`
  - `cargo test -p yi-agent --bin yi-agent tui::app`
  - `cargo test -p yi-agent --bin yi-agent -- manual_compaction`
  - `cargo test -p yi-agent-app-server`

---

## 文件结构

| 文件 | 责任 | 改动 |
|---|---|---|
| `crates/yi-agent-core/src/agent.rs` | `AgentEvent` 定义 | 新增 `ModelChanged { model }` 变体 |
| `crates/yi-agent-app-server/src/translate.rs` | app-server 事件翻译（唯一穷尽匹配） | 把 `ModelChanged` 加进忽略组 |
| `crates/yi-agent/src/main.rs` | driver、`ControlCommand`、headless drain | `SetModel`、compact outcome、`TuiConfigSnapshot` 组装与传参 |
| `crates/yi-agent/src/tui/slash.rs` | slash 目录、解析、popup | `completable()`、`unavailable_reason()`、`TuiConfigSnapshot` 视图渲染、`parse_daemon_args` |
| `crates/yi-agent/src/tui/app.rs` | TUI 循环、slash 执行 | `/config` `/model` `/daemon` 实现、隐藏项理由、隐藏项不进弹窗、`current_model` |
| `crates/yi-agent/src/tui/history.rs` | 事件→转录 | `ModelChanged` 渲染确认行 |
| `docs/project-management/yi-agent-tui.md` | 进度文档 | 更新 slash 目录与 compact 条目 |

### 参数穿透约定（Task 4 起所有任务共用）

`/config` 需要只读快照，`/config` 与 `/model` 都需要当前 model。穿透路径：

```
run_tui_agent (main.rs)
  └─ run_tui(...)                     // +1 参数: config: TuiConfigSnapshot
       └─ run_loop(...)               // +1 参数: config: &TuiConfigSnapshot；内部持有 mut current_model
            └─ handle_key(...)        // +2 参数: config: &TuiConfigSnapshot, model: &str
                 └─ execute_slash_command(...)  // +2 参数: config: &TuiConfigSnapshot, current_model: &str
```

- `run_loop` 现有 `model: &str` 参数保留其名，仅供初始化；Task 6 把它变成
  `let mut current_model: String = model.to_string();`，此后渲染与 dispatch 都用
  `&current_model`。
- **参数顺序约定**：新增参数一律追加在**最后一个现有参数之后**（`execute_slash_command`
  是 `mcp` 之后加 `config`、`current_model`；`handle_key` 是 `mcp` 之后加 `config`、
  `model`）。所有测试调用点按同一顺序补齐。
- **调用点清单（将随 Task 4 一次性更新，之后 Task 5/6/7 只改分支体）**：
  - `run_loop` 调用点 13 处：`app.rs:101`（prod）、`:183`、`:221`（prod helper），
    以及测试 `:4973 :5055 :5114 :5184 :5296 :5365 :5414 :5470 :5582 :5640 :5804 :7337`。
  - `handle_key` 调用点：prod `app.rs:661`；测试 `:7812 :7837 :7879 :7922 :7965 :8005
    :8025 :8069 :8116 :8166 :8192 :8261 :8428 :8480 :8506 :8546 :8604 :9387 :9441`。
  - `execute_slash_command` 调用点：prod `app.rs:1912 :1994`；测试 `:8662 :8706 :8747
    :8788 :8822 :8865 :8904 :8943 :8984`（Task 4/5/6/7 各再增若干）。
  - 让编译器逐个指路：先改签名，再按 `error[E0061]` 的每处补齐实参。
- 测试里传 `&snapshot_for_tests()`（Task 5 定义的 helper）与字面量 `"test-model"`。
- `run_tui_with_backend` / `run_tui_with_backend_and_events`（`app.rs:170` / `:209`）
  需同步新增 `config: &TuiConfigSnapshot` 参数，并在 `run_loop` 调用处透传。

---

## Task 1: `/compact` 完成状态闭环（回归修复，独立可交付）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`（`ControlCommand::Compact` 分支、文件尾部 helper）
- Test: `yi-agent-rs/crates/yi-agent/src/main.rs`（`mod tests`）

**Interfaces:**
- Consumes: `yi_agent_core::AgentEvent::{ManualCompacted, ManualCompactFailed}`（已存在）、`yi_agent_core::compact_session`（已存在，签名 `async fn(&Arc<dyn Provider>, &AgentConfig, &Session) -> Result<Option<Session>, AgentError>`）。
- Produces: `fn manual_compaction_outcome_event(old_msg_count: usize, result: Result<usize, String>) -> yi_agent_core::AgentEvent`（供 driver 与本任务测试使用）。

- [ ] **Step 1: 写失败测试**

在 `crates/yi-agent/src/main.rs` 的 `mod tests`（`use super::*;` 之下）加入：

```rust
#[test]
fn manual_compaction_outcome_events_preserve_counts_and_errors() {
    use yi_agent_core::AgentEvent;
    assert!(matches!(
        manual_compaction_outcome_event(9, Ok(4)),
        AgentEvent::ManualCompacted { old_msg_count: 9, new_msg_count: 4 }
    ));
    assert!(matches!(
        manual_compaction_outcome_event(3, Err("没有可压缩的历史".into())),
        AgentEvent::ManualCompactFailed { message } if message == "没有可压缩的历史"
    ));
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent -- manual_compaction_outcome_events_preserve_counts_and_errors`
Expected: FAIL（`cannot find function manual_compaction_outcome_event`）。

- [ ] **Step 3: 实现 helper 并接入 driver**

在 `main.rs` 中 `pub(crate) enum ControlCommand` 定义之后（`ControlCommand::McpRefresh` 之后的空行处）加入：

```rust
/// 把 `/compact` 的三种结果映射成 driver 回给 TUI 的事件。
///
/// `Ok(None)` 表示没有可安全缩减的历史（历史太短），**不是错误**，但对
/// pending 行「正在压缩对话...」而言它就是一次可报告的终局，因此同样落到
/// `ManualCompactFailed`，让状态行一定闭环。
fn manual_compaction_outcome_event(
    old_msg_count: usize,
    result: Result<usize, String>,
) -> yi_agent_core::AgentEvent {
    match result {
        Ok(new_msg_count) => yi_agent_core::AgentEvent::ManualCompacted {
            old_msg_count,
            new_msg_count,
        },
        Err(message) => yi_agent_core::AgentEvent::ManualCompactFailed { message },
    }
}
```

把 driver 的 `ControlCommand::Compact` 分支改为（保留原有重建逻辑，仅替换事件发送）：

```rust
ControlCommand::Compact => {
    let session = agent.session();
    let old_msg_count = session.messages().len();
    match yi_agent_core::compact_session(
        &rebuild_provider,
        &rebuild_config,
        &session,
    )
    .await
    {
        Ok(Some(new_session)) => {
            let new_msg_count = new_session.messages().len();
            agent = yi_agent_core::Agent::new(
                Arc::clone(&rebuild_provider),
                Arc::clone(&current_tools),
                rebuild_config.clone(),
            )
            .with_session(new_session)
            .with_permission(
                Arc::clone(&current_checker),
                Arc::clone(&rebuild_decision_rx),
            );
            tracing::info!("agent session compacted via /compact");
            let _ = agent_tx
                .send(manual_compaction_outcome_event(
                    old_msg_count,
                    Ok(new_msg_count),
                ))
                .await;
        }
        Ok(None) => {
            tracing::info!("no compactable session history");
            let _ = agent_tx
                .send(manual_compaction_outcome_event(
                    old_msg_count,
                    Err("没有可压缩的历史".into()),
                ))
                .await;
        }
        Err(e) => {
            tracing::warn!(error = %e, "compact failed");
            let _ = agent_tx
                .send(manual_compaction_outcome_event(
                    old_msg_count,
                    Err(e.to_string()),
                ))
                .await;
        }
    }
}
```

> 注意：`Err` 分支原来是 `agent_tx.send(yi_agent_core::AgentEvent::Error(e))`，现改为发 `ManualCompactFailed`，这样 pending 行三类结果都闭环。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent -- manual_compaction_outcome_events_preserve_counts_and_errors`
Expected: PASS。

- [ ] **Step 5: 跑完整 TUI 测试与 fmt，提交**

```bash
cd yi-agent-rs && cargo fmt --all && cargo test -p yi-agent --bin yi-agent tui::history
git add crates/yi-agent/src/main.rs
git commit -m "fix(tui): restore /compact completion events in the driver"
```

---

## Task 2: 隐藏命令的目录视图与理由（纯 `slash.rs`，独立可交付）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs`
- Test: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs`（`mod tests`）

**Interfaces:**
- Consumes: `SlashCommand`（既有枚举）、`ControlCommand`（既有）。
- Produces:
  - `SlashCommand::completable() -> &'static [SlashCommand]`（弹窗与 `/help` 全量列表用）
  - `SlashCommand::unavailable_reason(&self) -> Option<&'static str>`
  - `help_text(Option<&str>)` 的全量分支改用 `completable()`

- [ ] **Step 1: 写失败测试**

在 `slash.rs` 的 `mod tests` 末尾追加：

```rust
#[test]
fn hidden_commands_are_completable_but_still_resolve() {
    let completable: Vec<&str> = SlashCommand::completable().iter().map(|c| c.name()).collect();
    for hidden in ["approve", "deny", "budget", "priority"] {
        assert!(!completable.contains(&hidden), "{hidden} must be hidden from completion");
        assert!(
            SlashCommand::from_name(hidden).is_some(),
            "{hidden} must still resolve (anchor for re-entry)"
        );
    }
    // `all()` keeps them so `from_name` and the catalog assertions are intact.
    assert!(SlashCommand::all().iter().any(|c| c.name() == "approve"));
}

#[test]
fn hidden_commands_report_a_reason_not_unknown() {
    assert!(SlashCommand::Approve.unavailable_reason().unwrap().contains("暂不支持"));
    assert!(SlashCommand::Deny.unavailable_reason().is_some());
    assert!(SlashCommand::Budget.unavailable_reason().is_some());
    assert!(SlashCommand::Priority.unavailable_reason().is_some());
    assert!(SlashCommand::Quit.unavailable_reason().is_none());
    assert!(SlashCommand::Cost.unavailable_reason().is_none());
}

#[test]
fn full_help_lists_only_completable_commands() {
    let help = help_text(None);
    for hidden in ["/approve", "/deny", "/budget", "/priority"] {
        assert!(!help.contains(hidden), "full help must not list {hidden}");
    }
    assert!(help.contains("/config") && help.contains("/model") && help.contains("/daemon"));
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::slash`
Expected: FAIL（`no function completable` / `no method unavailable_reason`）。

- [ ] **Step 3: 实现 `completable` 与 `unavailable_reason`**

在 `impl SlashCommand` 中，`all()` 之后加入：

```rust
/// Commands shown in the popup and in the full `/help` listing.
///
/// [`Self::all()`] stays the complete catalog so `from_name` keeps resolving
/// commands whose backend does not exist yet; this view is what the user browses.
/// Cached so the popup's per-keystroke calls do not rebuild (or leak) the list.
pub fn completable() -> &'static [SlashCommand] {
    static COMPLETABLE: std::sync::OnceLock<Vec<SlashCommand>> = std::sync::OnceLock::new();
    COMPLETABLE.get_or_init(|| {
        SlashCommand::all()
            .iter()
            .copied()
            .filter(|cmd| cmd.unavailable_reason().is_none())
            .collect()
    })
}
```

> 不要用 `.leak()`：popup 每次按键都会调 `completable()`，leak 会持续泄漏内存。
> `OnceLock` 只构造一次，且返回 `&'static [SlashCommand]`，与 `all()` 签名一致。

在 `impl SlashCommand` 中加入：

```rust
/// Why a command has no backend yet, or `None` when it is usable.
///
/// The reason is shown when the user explicitly types a hidden command: a bare
/// "未知命令" would misreport a known-but-unwired command as a typo.
pub fn unavailable_reason(&self) -> Option<&'static str> {
    match self {
        Self::Approve | Self::Deny => Some(
            "子 agent 验收由父 agent 真实合并后 daemon 自动观察，无交互式权限审批",
        ),
        Self::Budget | Self::Priority => Some("daemon 尚未提供预算/优先级写接口"),
        _ => None,
    }
}
```

- [ ] **Step 4: 让 `/help` 全量分支用 `completable()`**

把 `help_text` 的 `None` 分支：

```rust
None => {
    let mut text = String::from("可用命令:\n");
    for command in SlashCommand::all() {
```

改为：

```rust
None => {
    let mut text = String::from("可用命令:\n");
    for command in SlashCommand::completable() {
```

`Some(name)` 分支保持不变（仍走 `all()`，所以 `/help approve` 能给出信息）。

- [ ] **Step 5: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::slash`
Expected: PASS。本任务不改 popup，既有 `filter_empty_shows_all`（断言
`popup.filtered().len() == SlashCommand::all().len()`）仍成立，**Task 3 才会**把它
改成 `completable()`。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/slash.rs
git commit -m "feat(tui): hide commands without a backend, keep them resolvable"
```

---

## Task 3: 弹窗只显示可完成命令

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs`（`CommandPopup::new` / `filter`）
- Test: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs`

**Interfaces:**
- Consumes: `SlashCommand::completable()`（Task 2）。
- Produces: `CommandPopup` 的候选集等于 `completable()`。

- [ ] **Step 1: 更新并新增测试**

在 `slash.rs` 测试中把 `filter_empty_shows_all` 改为：

```rust
#[test]
fn filter_empty_shows_all() {
    let popup = CommandPopup::new();
    assert_eq!(popup.filtered().len(), SlashCommand::completable().len());
}

#[test]
fn popup_never_lists_hidden_commands() {
    let mut popup = CommandPopup::new();
    popup.filter("a"); // would otherwise match approve
    let names: Vec<&str> = popup.filtered().iter().map(|c| c.name()).collect();
    assert!(!names.contains(&"approve"));
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::slash`
Expected: FAIL（`filter_empty_shows_all` 长度不等）。

- [ ] **Step 3: 切换 popup 候选集**

`CommandPopup::new` 与 `filter` 中三处 `SlashCommand::all()`（构造与两个分支）改为 `SlashCommand::completable()`：

```rust
// new()
filtered: SlashCommand::completable().to_vec(),

// filter(), empty branch
self.filtered = SlashCommand::completable().to_vec();

// filter(), non-empty branch
self.filtered = SlashCommand::completable()
    .iter()
    .copied()
    .filter(|cmd| cmd.name().starts_with(text))
    .collect();
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::slash`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/slash.rs
git commit -m "feat(tui): keep hidden commands out of the slash popup"
```

---

## Task 4: `/config` 展示真实配置

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs`（新增 `TuiConfigSnapshot` + 渲染函数）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`（`run_tui` / `run_loop` / `handle_key` / `execute_slash_command` 传参 + `/config` 实现）
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`（组装快照并传入 `run_tui`）
- Test: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs`、`crates/yi-agent/src/tui/app.rs`

**Interfaces:**
- Consumes: `yi_agent_runtime::config::RuntimeConfig`（`main.rs` 已有 `config: Config`）、`yi_agent_core::AgentConfig`、`yi_agent_mcp::McpManager::master()`、`crate::tui::runtime_prefs::{load, preferences_path}`、`yi_agent_tools::SandboxMode::as_str()`。
- Produces:
  - `pub struct TuiConfigSnapshot { ... }`（字段见下）
  - `impl TuiConfigSnapshot { pub fn render(&self) -> String }`
  - `run_tui(..., config: TuiConfigSnapshot, ...)`（新增参数）

- [ ] **Step 1: 写失败测试（渲染，纯函数）**

在 `slash.rs` 的 `mod tests` 末尾追加：

```rust
#[test]
fn config_snapshot_renders_key_fields_without_secrets() {
    let snap = TuiConfigSnapshot {
        provider: "anthropic".into(),
        workdir: std::path::PathBuf::from("/tmp/proj"),
        sandbox: "workspace-write".into(),
        yolo: false,
        max_turns: 200,
        compact_threshold: 160_000,
        mcp_master: true,
        runtime_preference: "ask".into(),
        runtime_preference_path: std::path::PathBuf::from("/tmp/proj/.yi-agent/preferences.json"),
    };
    let text = snap.render("claude-sonnet-4-5");
    assert!(text.contains("anthropic"));
    assert!(text.contains("claude-sonnet-4-5"));
    assert!(text.contains("/tmp/proj"));
    assert!(text.contains("workspace-write"));
    assert!(text.contains("160000") || text.contains("160_000"));
    assert!(!text.to_lowercase().contains("api_key"));
    assert!(!text.to_lowercase().contains("api-key"));
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::slash::tests::config_snapshot_renders_key_fields_without_secrets`
Expected: FAIL（`cannot find type TuiConfigSnapshot`）。

- [ ] **Step 3: 定义 `TuiConfigSnapshot` 与 `render`**

在 `slash.rs`（`help_text` 之后）加入：

```rust
/// Read-only, secret-free view of the running session's configuration.
///
/// Assembled once in `main.rs` from `RuntimeConfig` / `AgentConfig` / the MCP
/// manager / the persisted runtime preference. The model is **not** stored here:
/// `/model` can change it mid-session, so `render` takes the live value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiConfigSnapshot {
    pub provider: String,
    pub workdir: std::path::PathBuf,
    pub sandbox: String,
    pub yolo: bool,
    pub max_turns: u32,
    pub compact_threshold: u32,
    pub mcp_master: bool,
    pub runtime_preference: String,
    pub runtime_preference_path: std::path::PathBuf,
}

impl TuiConfigSnapshot {
    /// Render the config for the transcript. `model` is the live model, so a
    /// `/model` switch shows up here without re-reading anything.
    pub fn render(&self, model: &str) -> String {
        format!(
            "当前配置:\n  provider: {}\n  model: {}\n  workdir: {}\n  sandbox: {}\n  yolo: {}\n  max_turns: {}\n  compact_threshold: {}\n  mcp master: {}\n  subagent runtime: {}（{}）",
            self.provider,
            model,
            self.workdir.display(),
            self.sandbox,
            if self.yolo { "on" } else { "off" },
            self.max_turns,
            self.compact_threshold,
            if self.mcp_master { "on" } else { "off" },
            self.runtime_preference,
            self.runtime_preference_path.display(),
        )
    }
}
```

- [ ] **Step 4: 跑 render 测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::slash::tests::config_snapshot_renders_key_fields_without_secrets`
Expected: PASS。

- [ ] **Step 5: 组装快照并穿透到 `execute_slash_command`**

> **关键顺序约束**：`run_tui_agent` 里 `config`（`RuntimeConfig`）在 driver
> `tokio::spawn(async move { ... })`（`main.rs:1462`）**之前**被 `move` 进闭包
> （`cli`），且 `tui_handle = spawn_blocking(move || run_tui(...))`（`main.rs:1809`）
> 也在其后。因此**快照必须在 driver 生成之前构造**（放在 `main.rs:1452` 附近、
> `let driver = tokio::spawn(...)` 之上），把需要的标量 clone 出来，避免借用到被
> move 的 `config`。

在 `main.rs:1452` 附近（`let is_running = ...` / `DriverInput` 定义之前的合适位置）
构造快照：

```rust
// `workdir` is moved into the driver closure below, so clone once here for both
// the snapshot and the TUI call site.
let snapshot_workdir = workdir.clone();
let runtime_preference = match crate::tui::runtime_prefs::load(&snapshot_workdir) {
    crate::tui::runtime_prefs::RuntimePreference::Ask => "ask",
    crate::tui::runtime_prefs::RuntimePreference::Always => "always",
    crate::tui::runtime_prefs::RuntimePreference::Never => "never",
};
let tui_config = crate::tui::slash::TuiConfigSnapshot {
    provider: config.provider.clone(),
    workdir: snapshot_workdir.clone(),
    sandbox: config.sandbox.as_str().to_string(),
    yolo: config.yolo,
    max_turns: config.max_turns,
    compact_threshold: config.compact_threshold,
    mcp_master: mcp.master(),
    runtime_preference: runtime_preference.to_string(),
    runtime_preference_path: crate::tui::runtime_prefs::preferences_path(&snapshot_workdir),
};
```

> `config` 与 `workdir` 都会被 driver 的 `async move` 闭包捕获：`config` 在
> `:1612` 被 `&config` 借用（借用在 `await` 内），`workdir` 在 `:1472` 被
> `let _ = workdir;` 消费。快照必须建在 `let driver = tokio::spawn(async move {`
> （`:1462`）**之前**，且用 clone 出的 `snapshot_workdir` 而非 `workdir` 本身，
> 否则会把 `workdir` 提前移走、破坏下面的 driver。`mcp` 是 `Arc`，`mcp.master()`
> 在此之前调用安全。

把 `tui_config` 作为参数传给 `tui_handle` 的 `run_tui(...)` 调用（`main.rs:1810`
附近），并逐层传到 `run_loop` → `handle_key` → `execute_slash_command`（参数追加
顺序见上文「参数穿透约定」）。

`execute_slash_command` 的 `/config` 分支改为：

```rust
SlashCommand::Config => {
    if args.is_some() {
        history.push(
            HistoryCell::Separator {
                label: Some("用法: /config".to_string()),
            },
            width,
        );
    } else {
        let text = config.render(current_model);
        history.push(HistoryCell::Markdown { text }, width);
    }
    KeyOutcome::None
}
```

> `current_model` 是 Task 6 引入的 `run_loop` 局部变量；本任务先传 `model`（`run_loop` 现有 `model: &str` 参数），Task 6 再换成可变变量。为降低耦合，本任务给 `execute_slash_command` 传 `current_model: &str` 参数（值来自 `model`），Task 6 改其来源即可。

- [ ] **Step 6: 写 `/config` 执行测试**

在 `app.rs` 的 `mod tests` 中，参照既有 `cost_command_renders_tracker` 的脚手架，新增：

```rust
#[test]
fn config_command_renders_snapshot() {
    let mut history = HistoryState::new();
    let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
    let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
    let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(1);
    let mut queued = crate::tui::queued::DeliveredInterjections::new();
    let snapshot = crate::tui::slash::TuiConfigSnapshot {
        provider: "anthropic".into(),
        workdir: std::path::PathBuf::from("/tmp/proj"),
        sandbox: "workspace-write".into(),
        yolo: false,
        max_turns: 200,
        compact_threshold: 160_000,
        mcp_master: true,
        runtime_preference: "ask".into(),
        runtime_preference_path: std::path::PathBuf::from("/tmp/proj/.yi-agent/preferences.json"),
    };
    let outcome = execute_slash_command(
        SlashCommand::Config,
        None,
        None,
        &mut history,
        80,
        &CostTracker::default(),
        &input_tx,
        &interrupt_tx,
        &kill_tx,
        &control_tx,
        &std::env::temp_dir(),
        &mut queued,
        &yi_agent_mcp::McpManager::empty(),
        &snapshot,
        "test-model",
    );
    assert_eq!(outcome, KeyOutcome::None);
    let rendered: String = history
        .cells
        .iter()
        .filter_map(|c| match c {
            HistoryCell::Markdown { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(rendered.contains("anthropic"));
    assert!(rendered.contains("test-model"));
    assert!(!rendered.contains("api_key"));
}
```

> 参数顺序要与 Step 5 的实现一致；实现时以「在 `mcp` 参数之后追加 `config: &TuiConfigSnapshot`、`current_model: &str`」为准，并在本测试里对齐。

- [ ] **Step 7: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::`
Expected: PASS（含新测试与既有回归）。

- [ ] **Step 8: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/main.rs crates/yi-agent/src/tui/slash.rs crates/yi-agent/src/tui/app.rs
git commit -m "feat(tui): /config shows the real session configuration"
```

---

## Task 5: `/daemon [status|stop]`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs`（`parse_daemon_args`）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`（`/daemon` 实现 + `daemon_status_at` / `daemon_stop_at`）
- Test: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs`、`crates/yi-agent/src/tui/app.rs`

**Interfaces:**
- Consumes: `crate::runtime_socket_for(workdir)`、`yi_agent_store::ipc::{send_request, IpcRequest, IpcResponse}`（`IpcRequest::Status` / `Stop`，`IpcResponse::Status { high_water_event_id }` / `Stopping`）。
- Produces:
  - `pub enum DaemonAction { Status, Stop }`
  - `pub fn parse_daemon_args(args: &str) -> Result<DaemonAction, String>`

- [ ] **Step 1: 写失败测试（解析，纯函数）**

在 `slash.rs` 测试末尾追加：

```rust
#[test]
fn parse_daemon_args_defaults_to_status() {
    assert_eq!(parse_daemon_args(""), Ok(DaemonAction::Status));
    assert_eq!(parse_daemon_args("status"), Ok(DaemonAction::Status));
    assert_eq!(parse_daemon_args("stop"), Ok(DaemonAction::Stop));
}

#[test]
fn parse_daemon_args_rejects_start_and_unknown() {
    assert!(parse_daemon_args("start").is_err());
    assert!(parse_daemon_args("bogus").is_err());
}

#[test]
fn daemon_usage_advertises_only_status_and_stop() {
    let usage = SlashCommand::Daemon.argument_usage().unwrap();
    assert_eq!(usage, "[status|stop]");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::slash::tests::parse_daemon_args_defaults_to_status tui::slash::tests::daemon_usage_advertises_only_status_and_stop`
Expected: FAIL（`cannot find function parse_daemon_args`；usage 断言为 `"status"`）。

- [ ] **Step 3: 实现解析与 usage**

在 `slash.rs` 的 `parse_mcp_args` 之后加入：

```rust
/// A parsed `/daemon` action. `start` is deliberately absent: the TUI embeds
/// its own daemon, so starting a detached one from here would be ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonAction {
    Status,
    Stop,
}

/// Parse `/daemon` arguments. Returns a user-facing usage error on bad input.
pub fn parse_daemon_args(args: &str) -> Result<DaemonAction, String> {
    match args.split_whitespace().collect::<Vec<_>>().as_slice() {
        [] | ["status"] => Ok(DaemonAction::Status),
        ["stop"] => Ok(DaemonAction::Stop),
        _ => Err("用法: /daemon [status|stop]".into()),
    }
}
```

把 `SlashCommand::Daemon` 的 `argument_usage()` 从 `Some("status")` 改为 `Some("[status|stop]")`。

> 同时把 `control_commands.rs` 里 `Self::Daemon => spec(self, "daemon", "status", ...)` 的 usage 改为 `"[status|stop]"`——否则 `slash.rs` 经 `control_spec()` 取到的是 `"status"`。检查 `SlashCommand::argument_usage()`：`Daemon` 走 `control_spec()` 分支（`slash.rs:64` 把 `Self::Daemon => ControlCommand::Daemon`），所以以 `control_commands.rs` 为准，改那里。

- [ ] **Step 4: 跑解析测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::slash`
Expected: PASS。

- [ ] **Step 5: 实现 `/daemon` 执行与 IPC helper**

在 `app.rs` 的 `daemon_agents_summary` 附近加入两个 helper（与既有 `daemon_*_at` 同构）：

```rust
fn daemon_status_at(socket: &std::path::Path) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(socket, yi_agent_store::ipc::IpcRequest::Status)
        .map_err(|error| error.to_string())?;
    match response {
        yi_agent_store::ipc::IpcResponse::Status { high_water_event_id } => {
            Ok(format!("daemon 运行中（event high-water: {high_water_event_id}）"))
        }
        yi_agent_store::ipc::IpcResponse::Error { code, message } => {
            Err(format!("daemon 拒绝请求: {code:?} {message:?}"))
        }
        other => Err(format!("daemon 返回了非预期响应: {other:?}")),
    }
}

fn daemon_stop_at(socket: &std::path::Path) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(socket, yi_agent_store::ipc::IpcRequest::Stop)
        .map_err(|error| error.to_string())?;
    match response {
        yi_agent_store::ipc::IpcResponse::Stopping => Ok("daemon 正在停止".to_string()),
        yi_agent_store::ipc::IpcResponse::Error { code, message } => {
            Err(format!("daemon 拒绝请求: {code:?} {message:?}"))
        }
        other => Err(format!("daemon 返回了非预期响应: {other:?}")),
    }
}
```

把 `execute_slash_command` 的 `/daemon` 分支从占位改为：

```rust
SlashCommand::Daemon => {
    let label = match parse_daemon_args(args.as_deref().unwrap_or("")) {
        Ok(action) => {
            let result = crate::runtime_socket_for(workdir)
                .map_err(|error| error.to_string())
                .and_then(|socket| match action {
                    crate::tui::slash::DaemonAction::Status => daemon_status_at(&socket),
                    crate::tui::slash::DaemonAction::Stop => daemon_stop_at(&socket),
                });
            match result {
                Ok(message) => message,
                Err(error) => format!("无法联系本地 daemon runtime: {error}"),
            }
        }
        Err(error) => error,
    };
    history.push(HistoryCell::Separator { label: Some(label) }, width);
    KeyOutcome::None
}
```

> `daemon` 从隐藏组（`Approving | Deny | Budget | Priority | Daemon`）里移出：`execute_slash_command` 的 match 现在要**保留** `Approve | Deny | Budget | Priority` 四个作为带理由的分支（Task 7）。

- [ ] **Step 6: 写执行测试**

在 `app.rs` 测试中新增（用 fake socket 路径触发错误路径，避免真 daemon）：

```rust
#[test]
fn daemon_command_reports_unavailable_runtime() {
    let mut history = HistoryState::new();
    let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
    let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
    let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(1);
    let mut queued = crate::tui::queued::DeliveredInterjections::new();
    // A temp dir that no daemon serves: the socket cannot be reached.
    let dir = tempfile::tempdir().unwrap();
    let outcome = execute_slash_command(
        SlashCommand::Daemon,
        None,
        Some("status".into()),
        &mut history,
        80,
        &CostTracker::default(),
        &input_tx,
        &interrupt_tx,
        &kill_tx,
        &control_tx,
        dir.path(),
        &mut queued,
        &yi_agent_mcp::McpManager::empty(),
        &snapshot_for_tests(),
        "test-model",
    );
    assert_eq!(outcome, KeyOutcome::None);
    let labels = separator_labels(&history);
    assert!(
        labels.iter().any(|l| l.contains("无法联系本地 daemon runtime")),
        "expected an unreachable-daemon message, got {labels:?}"
    );
}

#[test]
fn daemon_command_rejects_unknown_subcommand() {
    let mut history = HistoryState::new();
    let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
    let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
    let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(1);
    let mut queued = crate::tui::queued::DeliveredInterjections::new();
    let _ = execute_slash_command(
        SlashCommand::Daemon,
        None,
        Some("start".into()),
        &mut history,
        80,
        &CostTracker::default(),
        &input_tx,
        &interrupt_tx,
        &kill_tx,
        &control_tx,
        &std::env::temp_dir(),
        &mut queued,
        &yi_agent_mcp::McpManager::empty(),
        &snapshot_for_tests(),
        "test-model",
    );
    let labels = separator_labels(&history);
    assert!(labels.iter().any(|l| l.contains("用法: /daemon [status|stop]")));
}
```

加一个测试辅助（`app.rs` 测试模块内，供 Task 4/5/7 复用）：

```rust
fn snapshot_for_tests() -> crate::tui::slash::TuiConfigSnapshot {
    crate::tui::slash::TuiConfigSnapshot {
        provider: "anthropic".into(),
        workdir: std::path::PathBuf::from("/tmp/proj"),
        sandbox: "workspace-write".into(),
        yolo: false,
        max_turns: 200,
        compact_threshold: 160_000,
        mcp_master: true,
        runtime_preference: "ask".into(),
        runtime_preference_path: std::path::PathBuf::from("/tmp/proj/.yi-agent/preferences.json"),
    }
}
```

（Task 4 的 `config_command_renders_snapshot` 里手写的 snapshot 也可改用此 helper。）

- [ ] **Step 7: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::`
Expected: PASS。

- [ ] **Step 8: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/main.rs crates/yi-agent/src/tui/slash.rs crates/yi-agent/src/tui/app.rs crates/yi-agent/src/control_commands.rs
git commit -m "feat(tui): /daemon status and stop via the existing IPC"
```

---

## Task 6: `/model <name>` 运行期切换

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`（新增 `AgentEvent::ModelChanged { model }`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/translate.rs`（忽略组）
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`（`ControlCommand::SetModel` + driver 分支 + 去掉 `Copy`）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`（`/model` 实现 + `current_model` + 状态栏取用 + `route_event`/事件分支）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/history.rs`（`ModelChanged` 渲染确认行）
- Test: `crates/yi-agent/src/main.rs`、`crates/yi-agent/src/tui/app.rs`

**Interfaces:**
- Consumes: `yi_agent_core::Agent::{new, with_session, with_permission, session}`、`AgentConfig { model, .. }`。
- Produces:
  - `AgentEvent::ModelChanged { model: String }`
  - `ControlCommand::SetModel(String)`（`ControlCommand` 不再 `Copy`）

- [ ] **Step 1: 写失败测试（core 变体可构造；translate 可编译）**

在 `crates/yi-agent-core/src/agent.rs` 测试模块加：

```rust
#[test]
fn model_changed_event_is_constructible() {
    let event = AgentEvent::ModelChanged { model: "claude-opus-4-1".into() };
    assert!(matches!(event, AgentEvent::ModelChanged { ref model } if model == "claude-opus-4-1"));
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib model_changed_event_is_constructible`
Expected: FAIL（`no variant ModelChanged`）。

- [ ] **Step 3: 加事件变体并修 translate**

在 `agent.rs` 的 `AgentEvent` 中 `ManualCompactFailed` 之后加入：

```rust
/// A runtime `/model` switch succeeded and the agent was rebuilt with a new
/// model. Emitted only after the rebuild, so a consumer may treat it as the
/// authoritative "now using this model" signal.
ModelChanged {
    model: String,
},
```

在 `translate.rs:386-394` 的忽略组加入 `AgentEvent::ModelChanged { .. }`：

```rust
AgentEvent::Start
| AgentEvent::ToolRetry { .. }
| AgentEvent::EstimatedPrefill(_)
| AgentEvent::AutoCompacting { .. }
| AgentEvent::ManualCompacted { .. }
| AgentEvent::ManualCompactFailed { .. }
| AgentEvent::ModelChanged { .. }
| AgentEvent::DecodeDelta(_)
| AgentEvent::PermissionRequest { .. }
| AgentEvent::PermissionResolved { .. } => {}
```

- [ ] **Step 4: 跑 core 与 app-server 测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib model_changed_event_is_constructible && cargo test -p yi-agent-app-server`
Expected: PASS（app-server 编译通过、测试绿）。

- [ ] **Step 5: 写 driver 测试（SetModel 保留 session）**

在 `main.rs` `mod tests` 加：

```rust
#[test]
fn set_model_control_command_carries_the_new_model() {
    let cmd = ControlCommand::SetModel("claude-opus-4-1".into());
    assert_eq!(cmd, ControlCommand::SetModel("claude-opus-4-1".into()));
}
```

（`ControlCommand` 去掉 `Copy` 后要保留 `PartialEq`，见 Step 6。）

- [ ] **Step 6: 实现 `SetModel` 与 driver 分支，去掉 `Copy`**

把 `ControlCommand` 派生改为：

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ControlCommand {
    /// Clear the agent session (rebuild with empty session).
    Clear,
    /// Compact the agent session (summarize old messages, keep recent turns).
    Compact,
    /// Rebuild the agent with a new model, preserving the session.
    SetModel(String),
    /// Rebuild the agent so its tool registry matches the MCP switches the TUI
    /// already applied directly to the shared `McpManager`.
    McpRefresh,
}
```

> 去掉 `Copy` 后，`main.rs` 里 `if let DriverInput::Control(Some(cmd)) = input { match cmd { ... } }` 是按值匹配，无需改；`tui/app.rs` 里 `blocking_send(ControlCommand::Clear)` 等按值构造，无需改。若编译器报 `cmd` 被移动，改 `match cmd` 为 `match cmd.clone()`。

在 driver `ControlCommand::McpRefresh` 分支之后加入：

```rust
ControlCommand::SetModel(new_model) => {
    let mut next_config = rebuild_config.clone();
    next_config.model = new_model.clone();
    agent = yi_agent_core::Agent::new(
        Arc::clone(&rebuild_provider),
        Arc::clone(&current_tools),
        next_config,
    )
    .with_session(agent.session())
    .with_permission(
        Arc::clone(&current_checker),
        Arc::clone(&rebuild_decision_rx),
    );
    tracing::info!(model = %new_model, "agent model switched via /model");
    let _ = agent_tx
        .send(yi_agent_core::AgentEvent::ModelChanged { model: new_model })
        .await;
}
```

> `rebuild_config` 是 driver 闭包内的 `AgentConfig`（`main.rs:1457`，
> `let rebuild_config = agent_config.clone();`），在循环里按需 `.clone()`，各分支共享
> 同一份，未被 move。`SetModel` 分支里 `rebuild_config.clone()` 后改 `model` 字段即可，
> 不影响其他分支。

- [ ] **Step 7: 跑 driver 测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent -- set_model_control_command_carries_the_new_model`
Expected: PASS，且 `cargo build -p yi-agent` 无 `Copy` 相关错误（`cargo test` 会顺带编译）。

- [ ] **Step 8: 在 TUI 实现 `/model` 与 `current_model`**

在 `run_loop` 里把 `model: &str` 参数改为可变持有。最小做法：保留参数名 `model`，在函数体开头加：

```rust
let mut current_model: String = model.to_string();
```

把所有渲染点（状态栏 `render_statusbar(..., model, ...)` 即 `app.rs:503`）与 `execute_slash_command` 调用改成传 `&current_model`。

在事件处理循环里，`history.push_event(event, ...)` 之前加：

```rust
if let AgentEvent::ModelChanged { model: new_model } = &event {
    current_model = new_model.clone();
}
```

把 `execute_slash_command` 的 `/model` 分支改为：

```rust
SlashCommand::Model => {
    match args.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        Some(new_model) => {
            let _ = control_tx.blocking_send(crate::ControlCommand::SetModel(new_model.to_string()));
            // 确认行由 `ModelChanged` 事件驱动；这里不预写成功行，避免与
            // driver 真实结果冲突。
        }
        None => {
            history.push(
                HistoryCell::Separator {
                    label: Some("用法: /model <model-name>".to_string()),
                },
                width,
            );
        }
    }
    KeyOutcome::None
}
```

- [ ] **Step 9: 在 `history.rs` 渲染确认行**

在 `history.rs::push_event` 的 `ManualCompactFailed` 分支之后加入：

```rust
AgentEvent::ModelChanged { model } => {
    self.cells.push(HistoryCell::Separator {
        label: Some(format!("已切换模型: {model}")),
    });
}
```

- [ ] **Step 10: 写并跑 TUI 测试**

在 `app.rs` 测试中加：

```rust
#[test]
fn model_command_sends_set_model_control() {
    let mut history = HistoryState::new();
    let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
    let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
    let (control_tx, mut control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
    let mut queued = crate::tui::queued::DeliveredInterjections::new();
    let _ = execute_slash_command(
        SlashCommand::Model,
        None,
        Some("claude-opus-4-1".into()),
        &mut history,
        80,
        &CostTracker::default(),
        &input_tx,
        &interrupt_tx,
        &kill_tx,
        &control_tx,
        &std::env::temp_dir(),
        &mut queued,
        &yi_agent_mcp::McpManager::empty(),
        &snapshot_for_tests(),
        "test-model",
    );
    assert_eq!(
        control_rx.try_recv(),
        Ok(crate::ControlCommand::SetModel("claude-opus-4-1".into()))
    );
}

#[test]
fn model_command_without_args_shows_usage() {
    let mut history = HistoryState::new();
    let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
    let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
    let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(1);
    let mut queued = crate::tui::queued::DeliveredInterjections::new();
    let _ = execute_slash_command(
        SlashCommand::Model,
        None,
        None,
        &mut history,
        80,
        &CostTracker::default(),
        &input_tx,
        &interrupt_tx,
        &kill_tx,
        &control_tx,
        &std::env::temp_dir(),
        &mut queued,
        &yi_agent_mcp::McpManager::empty(),
        &snapshot_for_tests(),
        "test-model",
    );
    let labels = separator_labels(&history);
    assert!(labels.iter().any(|l| l.contains("用法: /model <model-name>")));
}
```

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui:: && cargo test -p yi-agent --bin yi-agent -- model_changed`
Expected: PASS。

- [ ] **Step 11: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src/agent.rs crates/yi-agent-app-server/src/translate.rs crates/yi-agent/src/main.rs crates/yi-agent/src/tui/app.rs crates/yi-agent/src/tui/history.rs
git commit -m "feat(tui): /model switches the model at runtime"
```

---

## Task 7: 隐藏命令显式调用给出理由

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`（`execute_slash_command` 分支）
- Test: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`

**Interfaces:**
- Consumes: `SlashCommand::unavailable_reason()`（Task 2）。
- Produces: 隐藏命令的转录理由行。

- [ ] **Step 1: 写失败测试**

在 `app.rs` 测试中加：

```rust
#[test]
fn hidden_commands_explain_themselves_instead_of_unknown() {
    for (command, needle) in [
        (SlashCommand::Approve, "暂不支持"),
        (SlashCommand::Deny, "暂不支持"),
        (SlashCommand::Budget, "暂不支持"),
        (SlashCommand::Priority, "暂不支持"),
    ] {
        let mut history = HistoryState::new();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(1);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let _ = execute_slash_command(
            command,
            None,
            None,
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &std::env::temp_dir(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
        );
        let labels = separator_labels(&history);
        assert!(
            labels.iter().any(|l| l.contains(needle) && l.contains(command.name())),
            "{} must explain itself, got {labels:?}",
            command.name()
        );
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::app::tests::hidden_commands_explain_themselves_instead_of_unknown`
Expected: FAIL（当前输出「控制客户端接入中」，不含「暂不支持」）。

- [ ] **Step 3: 实现理由分支**

把 `execute_slash_command` 中的：

```rust
SlashCommand::Approve
| SlashCommand::Deny
| SlashCommand::Budget
| SlashCommand::Priority
| SlashCommand::Daemon => {
    history.push(
        HistoryCell::Separator {
            label: Some(format!(
                "/{} 将由本地 daemon runtime 执行 (控制客户端接入中)",
                cmd.name()
            )),
        },
        width,
    );
    KeyOutcome::None
}
```

改为（`Daemon` 已由 Task 5 移出）：

```rust
SlashCommand::Approve
| SlashCommand::Deny
| SlashCommand::Budget
| SlashCommand::Priority => {
    let reason = cmd
        .unavailable_reason()
        .unwrap_or("暂不支持");
    history.push(
        HistoryCell::Separator {
            label: Some(format!("/{} 暂不支持：{}", cmd.name(), reason)),
        },
        width,
    );
    KeyOutcome::None
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::app::tests::hidden_commands_explain_themselves_instead_of_unknown`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/app.rs
git commit -m "feat(tui): hidden slash commands state why they are unavailable"
```

---

## Task 8: 文档同步与全量验证

**Files:**
- Modify: `docs/project-management/yi-agent-tui.md`
- Test: 全套

- [ ] **Step 1: 更新 TUI 文档**

在 `docs/project-management/yi-agent-tui.md` 中：

1. 把 slash 目录条目的命令清单更新：4 个命令（`/approve` `/deny` `/budget` `/priority`）标注为「已从弹窗/全量 help 隐藏，仍可显式调用并给出理由；无后端支撑，另行立项」，并说明 `/config` `/model` `/daemon` 已落地。
2. 「压缩状态闭环」条目补上 driver 侧事件回报的说明与验证命令：
   验证：`cargo test -p yi-agent --bin yi-agent -- manual_compaction`。

- [ ] **Step 2: 跑全量验证**

```bash
cd yi-agent-rs
ps aux | grep -v grep | grep cargo || true   # 确认无残留 cargo 进程
cargo fmt --all && just fmt-check
cargo test -p yi-agent-core --lib
cargo test -p yi-agent-app-server
cargo test -p yi-agent --bin yi-agent
```

Expected: 全绿。

- [ ] **Step 3: 提交**

```bash
git add docs/project-management/yi-agent-tui.md
git commit -m "docs(tui): record the slash command completion status"
```

---

## Self-Review

**Spec coverage：**
- §2.1 `/config` → Task 4 ✅
- §2.2 `/model` → Task 6 ✅
- §2.3 `/daemon` → Task 5 ✅
- §2.4 隐藏 4 命令 → Task 2（目录/理由）、Task 3（弹窗）、Task 7（显式调用） ✅
- §2.5 `/compact` 回归 → Task 1 ✅
- §3.1 配置快照 → Task 4 ✅
- §3.2 driver SetModel / daemon 不进 driver → Task 6 / Task 5 ✅
- §3.3 新事件 `ModelChanged` + 穷尽匹配 → Task 6 ✅
- §3.4 弹窗/help 目录来源 → Task 2 / Task 3 ✅
- §5 测试策略 → 各任务内 + Task 8 全量 ✅
- §6 文档 → Task 8 ✅

**Placeholder scan：** 无 TBD/TODO；每个改动步骤都给了代码。唯一需实现者按现场对齐的是「参数顺序」（Task 4 Step 5/6 已声明以 `mcp` 之后追加为准）。

**Type consistency：**
- `TuiConfigSnapshot` 字段在 Task 4 定义，Task 4/5/7 测试与 `snapshot_for_tests()` 一致（9 个字段）。
- `DaemonAction` 在 Task 5 定义并使用。
- `SlashCommand::completable()` / `unavailable_reason()` 在 Task 2 定义，Task 3/7 使用。
- `ControlCommand::SetModel(String)` 在 Task 6 定义并使用；`ControlCommand` 去 `Copy` 已在 Task 6 Step 6 说明。
- `manual_compaction_outcome_event` 在 Task 1 定义并使用。
- `current_model` 在 Task 4 以参数形式引入（值来自 `model`），Task 6 改为 `run_loop` 可变变量——Task 6 Step 8 明确要求把 `execute_slash_command` 调用点改传 `&current_model`；Task 4 的测试用 `"test-model"` 字面量，两者兼容。

**依赖顺序：** Task 1 独立；Task 2→3；Task 4 引入 `TuiConfigSnapshot` 与 `snapshot_for_tests()`，Task 5/7 复用；Task 5 把 `Daemon` 移出隐藏组，Task 7 收窄隐藏组为 4 项——**Task 5 必须在 Task 7 之前**。Task 6 独立于 4/5/7（但 Task 4 的 `execute_slash_command` 签名含 `current_model`，Task 6 只改其来源）。
