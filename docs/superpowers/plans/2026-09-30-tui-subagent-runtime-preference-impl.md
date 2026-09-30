# TUI 子 Agent Runtime 启动偏好 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 TUI 只在首次启动时询问是否启用本地子 Agent runtime，把用户选择记到项目级
`.yi-agent/preferences.json`，并新增 `/runtime` 命令以逆转选择。

**Architecture:** 新增单职责模块 `tui/runtime_prefs.rs` 负责偏好文件的读写（原子写）。启动
时 `main.rs` 读偏好并折算成一个 `RuntimeStartupIntent`（`Prompt` / `AutoStart` /
`DisabledNotice`）传给 `run_tui`；`run_loop` 据此决定弹窗、静默或打印一行提示。`always`
通过向既有的 `runtime_choice_tx` 预置 `Start` 实现，**driver 与 daemon 完全不动**。清除
`tuigents.rs` 中三个零引用的死代码类型。

**Tech Stack:** Rust 2021、ratatui（TUI）、tokio（mpsc）、serde / serde_json（偏好文件）、
tempfile（测试）。

**设计依据：** `docs/superpowers/specs/2026-09-30-tui-subagent-runtime-preference-design.md`

## Global Constraints

以下为项目级硬性要求，每个任务都隐含包含：

- 所有命令在 `yi-agent-rs/` 目录下执行。
- **提交前必须跑 `cargo fmt --all`**（`CLAUDE.md:24`），否则 `just fmt-check` 失败。
- lint 门禁：`cargo clippy --all-targets --all-features -- -D warnings` 必须通过
  （`justfile:18-19`，`just ci` = fmt-check + lint + test + build）。**警告即错误**，所以
  本次删除死代码后残留的 `unused` 会直接导致门禁失败——这正是任务 3 要移除
  `#![allow(dead_code)]` 的原因。
- 测试只按 crate 跑，不要 `cargo test --workspace`；本计划统一用
  `cargo test -p yi-agent --bin yi-agent`（`CLAUDE.md:55-58`）。
- 不要在多个 shell 同时跑 `cargo test`（`CLAUDE.md:33`）。
- TUI 上面向用户的文案一律中文（先例：`/mcp` 的 `MCP server '{name}' 已开启`、
  `/clear` 的 `对话已清空`）。
- **不得留下 `TODO` / `TBD` / `unimplemented!()`**；本计划所有步骤给出完整代码。
- 偏好文件路径固定为 `<workdir>/.yi-agent/preferences.json`，序列化格式固定为
  `{"subagent_runtime": "ask"|"always"|"never"}`。这三个字面量是实现契约，不要在任务间改名。
- 死在任务 3 的三个类型名固定为：`TuiRuntimeMode`、`RuntimeBootstrapState`、
  `RuntimeBootstrapModel`。

---

## 关键陷阱（务必先读）

**driver 依赖 `runtime_choice_tx` 存活。** `crates/yi-agent/src/main.rs:1329-1332` 的
`select!` 等待 `runtime_choice_rx.recv()`，而 `main.rs:1502` 对该分支的 `None` 处理是
`break`：

```rust
Some(crate::tui::subagents::RuntimeStartupChoice::ContinueWithoutDelegation) => {
    tracing::info!("TUI subagent runtime disabled by user choice");
}
None => break,
```

即：**只要 `runtime_choice_tx` 被 drop，`recv()` 立刻返回 `None`，driver 循环直接退出，
整个会话结束。** 因此无论意图是 `AutoStart`、`DisabledNotice` 还是 `Prompt`，
`run_tui` 都必须收到一个活着的 `Some(runtime_choice_tx)`。本计划的做法是：**永远传
`Some(tx)`，只用"是否预置 `Start`"来区分 `always`**。

---

## File Structure

| 文件 | 职责 | 动作 |
| --- | --- | --- |
| `crates/yi-agent/src/tui/runtime_prefs.rs` | 偏好文件的唯一读写处（`RuntimePreference` + `load`/`save` 原子写） | 新建 |
| `crates/yi-agent/src/tui/mod.rs` | 模块注册 | 修改 |
| `crates/yi-agent/src/tui/subagents.rs` | runtime 附着模型：删死代码、加 `RuntimeStartupIntent` | 修改 |
| `crates/yi-agent/src/tui/slash.rs` | `/runtime` 命令定义与参数解析 | 修改 |
| `crates/yi-agent/src/tui/app.rs` | `run_tui`/`run_loop` 接收 intent、弹窗文案、按键落盘、`/runtime` 执行 | 修改 |
| `crates/yi-agent/src/main.rs` | 读偏好 → 折算 intent → 传参；`always` 预置 `Start` | 修改 |
| `docs/bug-list.md`、`docs/project-management/subagent-runtime.md` | 记录该项与验证命令 | 修改 |

---

## Spec 覆盖对照

| spec 章节 | 由哪个任务实现 |
| --- | --- |
| §2 决策表（三态、y/n 落盘、Esc 不落盘、`/runtime` 重启生效） | Task 1-5 |
| §2.1 非目标（不做 CLI/env 覆盖、不热切换） | 全计划无相关步骤 = 已排除 |
| §3.1 偏好文件（路径/格式/缺失与损坏降级/原子写） | Task 1 |
| §3.2 生效偏好解析（唯一来源、workdir 基准） | Task 5 |
| §3.3 启动意图 `RuntimeStartupIntent` | Task 3（定义）、Task 4-5（消费） |
| §3.4 `always` 预置 `Start`、失败不二次弹窗 | Task 5 Step 4 |
| §3.5 弹窗文案、`box_h`、`.wrap()` | Task 4 Step 4a/4b |
| §3.6 `/runtime` 命令（注册/解析/执行/中文回显） | Task 2（定义）、Task 4 Step 4e（执行） |
| §3.7 `never` 的中文禁用提示 | Task 5（生成 reason）、Task 4 Step 4b（渲染） |
| §5.1 测试不得污染 `/tmp` | Task 4 Step 1 |
| §5.2 落盘失败不阻断会话 | Task 4 Step 4d |
| §5.3 不走 `ControlCommand` | Task 2/4（只落在 SlashCommand 与 execute_slash_command） |
| §5.4 `always` 仍属显式同意 | Task 5 Step 4（仅由文件驱动，无 CLI/env 捷径） |
| §5.5 删除死代码 | Task 3 |
| §6 验证 | 各任务 Step + Task 7 |

---

## Task 1: 偏好文件读写模块

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/src/tui/runtime_prefs.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/mod.rs`

**Interfaces:**
- Consumes: 无（叶子模块）。
- Produces: 供后续任务使用，签名固定如下——
  - `pub enum RuntimePreference { Ask, Always, Never }`，`Default = Ask`，实现
    `Copy + PartialEq + Eq + Debug`
  - `pub fn preferences_path(workdir: &Path) -> PathBuf`
  - `pub fn load(workdir: &Path) -> RuntimePreference`（缺失/损坏/未知值 → `Ask`）
  - `pub fn save(workdir: &Path, pref: RuntimePreference) -> std::io::Result<()>`

- [ ] **Step 1: 注册模块**

编辑 `yi-agent-rs/crates/yi-agent/src/tui/mod.rs`，在 `pub mod queued;` 之后按字母序插入
一行（该文件现有模块为 `app, bash_popup, cell, cost, history, input, markdown,
process_popup, queued, slash, state, statusbar, subagents, wrap`）：

```rust
pub mod runtime_prefs;
```

- [ ] **Step 2: 写失败的测试**

创建 `yi-agent-rs/crates/yi-agent/src/tui/runtime_prefs.rs`，先只写测试与占位实现：

```rust
//! Project-level TUI preferences persisted under `<workdir>/.yi-agent/`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Whether the TUI should offer to start the local subagent runtime on launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimePreference {
    #[default]
    Ask,
    Always,
    Never,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_defaults_to_ask() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(load(dir.path()), RuntimePreference::Ask);
    }

    #[test]
    fn save_then_load_round_trips_every_state() {
        for pref in [
            RuntimePreference::Ask,
            RuntimePreference::Always,
            RuntimePreference::Never,
        ] {
            let dir = tempfile::TempDir::new().unwrap();
            save(dir.path(), pref).unwrap();
            assert_eq!(load(dir.path()), pref);
        }
    }

    #[test]
    fn malformed_and_unknown_values_fall_back_to_ask() {
        for body in [
            "not json at all",
            "{\"subagent_runtime\":\"bogus\"}",
            "[]",
            "{\"subagent_runtime\":5}",
        ] {
            let dir = tempfile::TempDir::new().unwrap();
            std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
            std::fs::write(preferences_path(dir.path()), body).unwrap();
            assert_eq!(load(dir.path()), RuntimePreference::Ask, "body: {body}");
        }
    }

    #[test]
    fn empty_object_defaults_to_ask() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        std::fs::write(preferences_path(dir.path()), "{}").unwrap();
        assert_eq!(load(dir.path()), RuntimePreference::Ask);
    }

    #[test]
    fn save_creates_directory_and_leaves_no_temp_file() {
        let dir = tempfile::TempDir::new().unwrap();
        save(dir.path(), RuntimePreference::Never).unwrap();
        assert!(dir.path().join(".yi-agent/preferences.json").exists());
        assert!(!dir.path().join(".yi-agent/preferences.json.tmp").exists());
    }
}
```

- [ ] **Step 3: 运行测试确认失败**

```bash
cargo test -p yi-agent --bin yi-agent -- runtime_prefs 2>&1 | tail -20
```
预期：编译失败，报 `cannot find function 'load'` / `'save'` / `'preferences_path'`（模块已注册但函数不存在）。

- [ ] **Step 4: 写最小实现**

在 `runtime_prefs.rs` 的 `enum RuntimePreference` 之后（`#[cfg(test)]` 之前）插入：

```rust
/// On-disk shape. A wrapper object (not a bare string) so more preferences can
/// be added later without breaking existing files.
#[derive(Debug, Default, Serialize, Deserialize)]
struct PreferencesFile {
    #[serde(default)]
    subagent_runtime: RuntimePreference,
}

/// Path of the project-level preference file: `<workdir>/.yi-agent/preferences.json`.
pub fn preferences_path(workdir: &Path) -> PathBuf {
    workdir.join(".yi-agent").join("preferences.json")
}

/// Load the preference.
///
/// A missing, unreadable, or malformed file yields `Ask`: a broken preference
/// must never block startup or silently disable delegation.
pub fn load(workdir: &Path) -> RuntimePreference {
    let path = preferences_path(workdir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return RuntimePreference::Ask;
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                path = %path.display(),
                "could not read TUI preferences; using defaults"
            );
            return RuntimePreference::Ask;
        }
    };
    match serde_json::from_str::<PreferencesFile>(&text) {
        Ok(file) => file.subagent_runtime,
        Err(error) => {
            tracing::warn!(
                error = %error,
                path = %path.display(),
                "invalid TUI preferences; using defaults"
            );
            RuntimePreference::Ask
        }
    }
}

/// Persist the preference atomically: write a sibling temp file, then rename.
///
/// Mirrors `yi-agent-core/src/permission.rs` (`permissions.toml`), where rename
/// within one filesystem is atomic — a crash cannot leave a half-written file.
pub fn save(workdir: &Path, pref: RuntimePreference) -> std::io::Result<()> {
    let dir = workdir.join(".yi-agent");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("preferences.json");
    let body = PreferencesFile {
        subagent_runtime: pref,
    };
    let text = serde_json::to_string_pretty(&body)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp_path = dir.join("preferences.json.tmp");
    std::fs::write(&tmp_path, &text)?;
    std::fs::rename(&tmp_path, &path)
}
```

- [ ] **Step 5: 运行测试确认通过**

```bash
cargo test -p yi-agent --bin yi-agent -- runtime_prefs 2>&1 | tail -20
```
预期：`5 passed`。

- [ ] **Step 6: 格式化并提交**

```bash
cargo fmt --all && cargo clippy -p yi-agent --all-targets --all-features -- -D warnings 2>&1 | tail -5
git add crates/yi-agent/src/tui/runtime_prefs.rs crates/yi-agent/src/tui/mod.rs
git commit -m "feat(tui): add project-level runtime preference store"
```

---

## Task 2: `/runtime` slash 命令定义与解析

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs`

**Interfaces:**
- Consumes: `crate::tui::runtime_prefs::RuntimePreference`（Task 1）。
- Produces:
  - `SlashCommand::Runtime`，`name() == "runtime"`，
    `argument_usage() == Some("[ask|always|never]")`
  - `pub enum RuntimeAction { Status, Set(RuntimePreference) }`
  - `pub fn parse_runtime_args(args: &str) -> Result<RuntimeAction, String>`

- [ ] **Step 1: 写失败的测试**

在 `slash.rs` 的 `mod tests` 中追加（该文件已有 `mcp_command_is_registered_with_usage`，
放在它之后）：

```rust
    #[test]
    fn runtime_command_is_registered_with_usage() {
        assert_eq!(SlashCommand::from_name("runtime"), Some(SlashCommand::Runtime));
        assert_eq!(
            SlashCommand::Runtime.argument_usage(),
            Some("[ask|always|never]")
        );
        assert!(SlashCommand::all().contains(&SlashCommand::Runtime));
    }

    #[test]
    fn parse_runtime_args_accepts_status_and_every_state() {
        assert_eq!(parse_runtime_args(""), Ok(RuntimeAction::Status));
        assert_eq!(parse_runtime_args("status"), Ok(RuntimeAction::Status));
        assert_eq!(
            parse_runtime_args("ask"),
            Ok(RuntimeAction::Set(RuntimePreference::Ask))
        );
        assert_eq!(
            parse_runtime_args("always"),
            Ok(RuntimeAction::Set(RuntimePreference::Always))
        );
        assert_eq!(
            parse_runtime_args("never"),
            Ok(RuntimeAction::Set(RuntimePreference::Never))
        );
    }

    #[test]
    fn parse_runtime_args_rejects_unknown_input() {
        assert_eq!(
            parse_runtime_args("sometimes"),
            Err("用法: /runtime [ask|always|never]".into())
        );
        assert_eq!(
            parse_runtime_args("always never"),
            Err("用法: /runtime [ask|always|never]".into())
        );
    }
```

在 `slash.rs` 顶部的 `use crate::control_commands::{CommandSpec, ControlCommand};` 之后
加上（供测试引用）：

```rust
use crate::tui::runtime_prefs::RuntimePreference;
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cargo test -p yi-agent --bin yi-agent -- runtime 2>&1 | tail -20
```
预期：编译失败，报 `cannot find variant 'Runtime'` / `cannot find function 'parse_runtime_args'`。

- [ ] **Step 3: 加枚举变体与元数据**

对 `slash.rs` 做 7 处机械修改（**数量以这里为准**）：

1. 枚举 `pub enum SlashCommand`：把结尾的 `Mcp,` 改为
   ```rust
       Mcp,
       Runtime,
   ```
2. `control_spec()` 中 `return None` 那一臂，把 `Self::Config` 换成 `Self::Config | Self::Runtime`：
   ```rust
           Self::Quit | Self::Clear | Self::Model | Self::Cost | Self::Compact | Self::Config | Self::Runtime => {
               return None;
           }
   ```
3. `name()` 的 match：在 `SlashCommand::Mcp => "mcp",` 之后加
   ```rust
               SlashCommand::Runtime => "runtime",
   ```
4. `description()` 的 match：在 `SlashCommand::Mcp => "管理 MCP server 开关",` 之后加
   ```rust
               SlashCommand::Runtime => "查看或设置子 Agent runtime 偏好",
   ```
5. `all()` 列表末尾：在 `SlashCommand::Mcp,` 之后加
   ```rust
               SlashCommand::Runtime,
   ```
6. `argument_usage()` 的 match：在 `SlashCommand::Approve => Some("<request-id> [once|task]"),`
   之前加
   ```rust
               SlashCommand::Runtime => Some("[ask|always|never]"),
   ```

- [ ] **Step 4: 加解析器**

在 `slash.rs` 中 `pub enum McpAction` 定义之前，插入：

```rust
/// A parsed `/runtime` action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeAction {
    /// Show the current preference and its source.
    Status,
    /// Persist a new preference.
    Set(RuntimePreference),
}

/// Parse `/runtime` arguments. Returns a user-facing usage error on bad input.
pub fn parse_runtime_args(args: &str) -> Result<RuntimeAction, String> {
    match args.split_whitespace().collect::<Vec<_>>().as_slice() {
        [] | ["status"] => Ok(RuntimeAction::Status),
        ["ask"] => Ok(RuntimeAction::Set(RuntimePreference::Ask)),
        ["always"] => Ok(RuntimeAction::Set(RuntimePreference::Always)),
        ["never"] => Ok(RuntimeAction::Set(RuntimePreference::Never)),
        _ => Err("用法: /runtime [ask|always|never]".into()),
    }
}
```

- [ ] **Step 5: 运行测试确认通过**

```bash
cargo test -p yi-agent --bin yi-agent -- runtime 2>&1 | tail -20
```
预期：3 个新测试通过；注意 `execute_slash_command` 此时会因**穷尽 match** 编译失败——
这是预期的，Task 4 会补上执行分支。若只想验证本任务，可临时用
`cargo test -p yi-agent --bin yi-agent -- runtime 2>&1 | grep -c "^error"` 观察，但
**不要提交**，直接进入 Task 4 一并修复。

> 若希望每个任务都可独立编译，把本任务的提交与 Task 4 合并（见 Task 4 Step 6 的说明）。

---

## Task 3: 删除死代码 + 新增 `RuntimeStartupIntent`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/subagents.rs`

**Interfaces:**
- Consumes: `RuntimeStartupChoice`（保留两态）。
- Produces:
  - `pub enum RuntimeStartupIntent { Prompt, DisabledNotice { reason: String }, AutoStart }`
  - 删除 `RuntimeStartPrompt`、`TuiRuntimeMode`、`RuntimeBootstrapState`、`RuntimeBootstrapModel`

- [ ] **Step 1: 确认零引用（改之前先复核）**

```bash
cd yi-agent-rs && for t in TuiRuntimeMode RuntimeBootstrapModel RuntimeBootstrapState; do
  echo -n "$t: "; grep -rn "$t" crates/ --include=*.rs | grep -v "^crates/yi-agent/src/tui/subagents.rs" | wc -l
done
```
预期：三个都是 `0`。**若非 0，停下**，说明设计假设失效，需要回去更新 spec。

- [ ] **Step 2: 删除死代码**

从 `subagents.rs` 删除以下内容：
- 第 3 行 `#![allow(dead_code)]` 及其上一空行（整块删掉，保留 `//!` 文档行与 `use` 行）
- `pub enum TuiRuntimeMode { .. }`（含其 `#[derive]` 行）
- `pub enum RuntimeBootstrapState { .. }`（含其 `#[derive]` 行）
- `pub struct RuntimeBootstrapModel { state: RuntimeBootstrapState }` 与整个
  `impl RuntimeBootstrapModel { .. }` 块
- 测试模块中两个用例 `disconnected_state_only_prompts_before_user_confirms_runtime_start`
  与 `user_can_continue_without_starting_runtime`
- 测试模块顶部的 `use std::sync::{Arc, Mutex};`（删掉两个用例后它变成未使用，
  而 `-D warnings` 会因此失败）

**同时删除 `RuntimeStartPrompt`**：改造后它的 `title` / `body` 两个字段
（`subagents.rs:64-67`）将不再被任何代码读取——弹窗文案改由 `app.rs` 的
`runtime_prompt_lines()` 渲染，`title` 硬编码在渲染处。`yi-agent` 是**二进制 crate**，
`pub` 字段不会被自动豁免 `dead_code`，留着会直接让
`clippy -D warnings`（计划 Global Constraints）失败。

删除后 `subagents.rs` 应保留：`RuntimeStartupChoice`、`AttachedRoot`、
`CURRENT_ATTACHED_ROOT`、`set_current_attached_root`、`current_attached_root`、
`register_attached_root_tools`，以及测试
`attached_tui_root_exposes_subagent_tools_without_a_delegate_command`。

- [ ] **Step 3: 新增 intent 枚举**

在 `RuntimeStartupPrompt` 原位置插入（`Prompt` 无载荷：文案与标题由 `app.rs` 渲染，
`main.rs` 无需再传递它们）：

```rust
/// What the TUI should do about the local subagent runtime on launch.
///
/// Chosen by `main.rs` from the persisted preference (§`runtime_prefs`), so the
/// UI layer never reads the preference file itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeStartupIntent {
    /// Show the yes/no dialog and wait for the user.
    Prompt,
    /// Do not start; show one separator line explaining why.
    DisabledNotice { reason: String },
    /// Start without asking; `main.rs` pre-seeds `RuntimeStartupChoice::Start`.
    AutoStart,
}
```

- [ ] **Step 4: 验证编译与现有测试**

```bash
cargo test -p yi-agent --bin yi-agent -- subagents 2>&1 | tail -20
```
预期：编译通过，`attached_tui_root_exposes_subagent_tools_without_a_delegate_command` 通过。

- [ ] **Step 5: 验证 dead-code 告警已消失**

```bash
cargo clippy -p yi-agent --all-targets --all-features -- -D warnings 2>&1 | tail -20
```
预期：无 `unused` 相关报错。（`RuntimeStartupChoice` 未变。**注意**：`app.rs` 与
`main.rs` 此时仍引用已删除的 `RuntimeStartPrompt`，所以
`cargo build -p yi-agent` 与 `clippy` 将在 Task 4 完成后才恢复通过——见下方"任务耦合"。）

> **任务耦合（必须知悉）**：本任务删除了 `RuntimeStartPrompt`，而 `app.rs:706` 与
> `main.rs:1049` 仍在引用它，因此本任务结束时 **`cargo test -- subagents` 能通过，但
> `clippy` 会因未定义类型而失败**。这是刻意的拆分选择：Task 4 负责把这两处换成
> `RuntimeStartupIntent::Prompt`。
> 因此 **Task 3 与 Task 4 必须一起评审**（Task 4 的 review 包 BASE 取 Task 3 之前的
> commit），不要在 Task 3 单独跑 clippy 门禁——那是 Task 4 的完成条件。若你更希望
> 每个任务都能独立过 clippy，就把 Task 3 与 Task 4 合并成一个任务。

- [ ] **Step 6: 格式化并提交**

```bash
cargo fmt --all
git add crates/yi-agent/src/tui/subagents.rs
git commit -m "refactor(tui): drop unused runtime bootstrap model, add startup intent"
```

---

## Task 4: `app.rs` 接入 intent、落盘、`/runtime` 执行

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`
  - `run_tui` 签名与转发（现状 `app.rs:72-113`）
  - `run_loop` 签名（现状 `app.rs:236-255`）与局部变量（`app.rs:261`）
  - 弹窗渲染（现状 `app.rs:415-435`）
  - 按键处理（现状 `app.rs:494-513`）
  - `execute_slash_command`（`app.rs:1548` 起，穷尽 match）
  - 两个现有测试（`app.rs:4449`、`app.rs:4501`）

**Interfaces:**
- Consumes: `RuntimeStartupIntent`（Task 3）、`RuntimePreference` / `save`（Task 1）、
  `RuntimeAction` / `parse_runtime_args`（Task 2）。
- Produces:
  - `run_tui(..., runtime_intent: Option<RuntimeStartupIntent>, runtime_choice_tx: Option<Sender<RuntimeStartupChoice>>, ...)`
  - `run_loop(..., runtime_intent: Option<RuntimeStartupIntent>, runtime_choice_tx: ..., workdir: PathBuf, ...)`
  - `fn persist_runtime_choice(workdir: &Path, pref: RuntimePreference, history: &mut HistoryState, width: u16)`

- [ ] **Step 1: 写失败的测试（先改现有两个测试 + 新增 Esc 用例）**

`app.rs:4449` 与 `app.rs:4501` 两个测试当前把 `std::env::temp_dir()` 当 `workdir`。先按
新签名改造它们，并断言落盘结果。以 `runtime_start_prompt_sends_start_choice_before_accepting_chat_input`
为例，把 `run_loop(...)` 的实参改为：

```rust
        let project = tempfile::TempDir::new().unwrap();
        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut HistoryState::new(),
            &mut InputLine::new(),
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &events,
            "test-model",
            Some(crate::tui::subagents::RuntimeStartupIntent::Prompt),
            Some(runtime_choice_tx),
            yi_agent_tools::ProcessManager::new(project.path().to_path_buf()),
            project.path().to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
        )
        .unwrap();

        assert_eq!(
            runtime_choice_rx.try_recv().unwrap(),
            crate::tui::subagents::RuntimeStartupChoice::Start
        );
        assert!(
            input_rx.try_recv().is_err(),
            "startup choice must not be sent as chat input"
        );
        assert_eq!(
            crate::tui::runtime_prefs::load(project.path()),
            crate::tui::runtime_prefs::RuntimePreference::Always
        );
```

`runtime_start_prompt_can_continue_without_delegation` 同理，只是按键为 `n`、断言
`ContinueWithoutDelegation` 与 `RuntimePreference::Never`。

再新增一个 Esc 用例，紧接其后：

```rust
    #[test]
    fn escape_skips_runtime_for_this_session_without_persisting() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let (runtime_choice_tx, mut runtime_choice_rx) = tokio::sync::mpsc::channel(1);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let events = ScriptedEvents {
            events: Rc::new(RefCell::new(vec![
                Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
                Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            ])),
        };
        let project = tempfile::TempDir::new().unwrap();

        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut HistoryState::new(),
            &mut InputLine::new(),
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &events,
            "test-model",
            Some(crate::tui::subagents::RuntimeStartupIntent::Prompt),
            Some(runtime_choice_tx),
            yi_agent_tools::ProcessManager::new(project.path().to_path_buf()),
            project.path().to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
        )
        .unwrap();

        // Session behaviour matches "n": delegation is not enabled this run.
        assert_eq!(
            runtime_choice_rx.try_recv().unwrap(),
            crate::tui::subagents::RuntimeStartupChoice::ContinueWithoutDelegation
        );
        // ...but Esc must not write a preference: the next launch asks again.
        assert!(!crate::tui::runtime_prefs::preferences_path(project.path()).exists());
        assert!(input_rx.try_recv().is_err());
    }
```

再新增一个宽度健壮性测试（防回归，见 spec §3.5）。它必须捕获两类缺陷：**右侧裁剪**与
**行数溢出弹窗高度**：

```rust
    #[test]
    fn runtime_start_prompt_is_fully_visible_at_supported_widths() {
        // (terminal width, box width, box height) — 40 exercises folding of the
        // legend line; 80 covers the common case where the text fits the box.
        for (term_w, box_w, box_h) in [(40u16, 58u16, 10u16), (80, 58, 10)] {
            let backend = TestBackend::new(term_w, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            let area = ratatui::layout::Rect::new(0, 0, box_w, box_h);
            terminal
                .draw(|f| {
                    f.render_widget(
                        ratatui::widgets::Paragraph::new(runtime_prompt_lines())
                            .wrap(ratatui::widgets::Wrap { trim: true }),
                        area,
                    );
                })
                .unwrap();

            let rendered = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();

            // Tail of every selectable option must survive; clipping would drop it.
            assert!(rendered.contains("启动并记住"), "width {term_w}");
            assert!(rendered.contains("跳过并记住"), "width {term_w}");
            assert!(rendered.contains("本次跳过"), "width {term_w}");
            assert!(rendered.contains("不记住"), "width {term_w}");
        }
    }
```

> 该测试是 `Paragraph` 层的行为契约：若去掉 `.wrap(...)`，40 列那一轮会因右侧被裁而
> 失败——这正是它存在的意义。它不覆盖 `run_loop` 的居中放置与 `box_h` 计算（那部分靠
> Task 7 手工确认）。

- [ ] **Step 2: 运行测试确认失败**

```bash
cargo test -p yi-agent --bin yi-agent -- runtime_start_prompt escape_skips 2>&1 | tail -30
```
预期：编译失败（`RuntimeStartupIntent` 未在 `run_loop` 签名中、`runtime_prompt_lines` 未定义）。

- [ ] **Step 3: 改签名与转发**

`run_tui`：把
```rust
    runtime_intent: Option<crate::tui::subagents::RuntimeStartupIntent>,
```
改为
```rust
    runtime_intent: Option<crate::tui::subagents::RuntimeStartupIntent>,
```
并在其 `run_loop(...)` 调用处把实参名同步改为 `runtime_intent`。

`run_loop`：同样把参数名与类型改为
```rust
    runtime_intent: Option<crate::tui::subagents::RuntimeStartupIntent>,
```
第 261 行的局部变量 `let mut runtime_start_prompt = runtime_start_prompt;` 改为：
```rust
    let mut runtime_intent = runtime_intent;
    // `DisabledNotice` prints exactly one line, on the first frame, once the
    // history width is known.
    let mut runtime_notice_pending = matches!(
        runtime_intent,
        Some(crate::tui::subagents::RuntimeStartupIntent::DisabledNotice { .. })
    );
```

- [ ] **Step 4: 改渲染、按键、执行分支**

4a. 把 `app.rs:415-435` 的弹窗块替换为对 intent 的分支，并把文案抽成函数（供测试复用）：

```rust
            if let Some(crate::tui::subagents::RuntimeStartupIntent::Prompt) = &runtime_intent {
                let box_w = 58u16.min(chunks[0].width.saturating_sub(4));
                let box_h = 10u16.min(chunks[0].height.max(1));
                let box_x = chunks[0].x + (chunks[0].width.saturating_sub(box_w)) / 2;
                let box_y = chunks[0].y + (chunks[0].height.saturating_sub(box_h)) / 3;
                let box_area = ratatui::layout::Rect {
                    x: box_x,
                    y: box_y,
                    width: box_w,
                    height: box_h,
                };
                f.render_widget(Clear, box_area);
                f.render_widget(
                    ratatui::widgets::Paragraph::new(runtime_prompt_lines()).block(
                        ratatui::widgets::Block::default()
                            .borders(ratatui::widgets::Borders::ALL)
                            .title("启动本地 Agent Runtime?"),
                    ),
                    box_area,
                );
            }
```

`box_h` 由 `6` 提升为 `10`（文案由 3 行增至 5 行，并需要余量容许折行）。

**必须同时加 `.wrap()`**：ratatui 0.29 的 `Paragraph` 文档明确写 "**not wrapped**"
（`ratatui-0.29.0/src/widgets/paragraph.rs:29`），跳过这只会在窄终端静默裁掉右半行；
而仓库现有渲染从不需要折行，因此**仓库内没有 `.wrap()` 先例可抄**——必须显式设置：

```rust
                    ratatui::widgets::Paragraph::new(runtime_prompt_lines())
                        .wrap(ratatui::widgets::Wrap { trim: true })
                        .block(
                            ratatui::widgets::Block::default()
                                .borders(ratatui::widgets::Borders::ALL)
                                .title("启动本地 Agent Runtime?"),
                        ),
```

（`Wrap` 由 `ratatui::widgets` 重导出，见 `ratatui-0.29.0/src/widgets.rs:51`。）

在其上方（`fn run_loop` 之外，作为模块级函数）加入：

```rust
/// Body lines of the runtime startup dialog.
///
/// Kept as a function so the narrow-terminal test renders exactly what the live
/// UI renders — a duplicated literal could drift and hide a clipping bug.
///
/// Keep every line's **display width** (CJK counts as 2 columns) within
/// `BOX_WIDTH - 2` = 56, or it needs `Wrap` to fold. The key legend is 58 columns
/// on purpose: it must fold into two lines, and the test below locks that in.
fn runtime_prompt_lines() -> Vec<ratatui::text::Line<'static>> {
    vec![
        ratatui::text::Line::raw("启动后可以直接用自然语言创建和管理子 Agent。"),
        ratatui::text::Line::raw("此选择会被记住，可用 /runtime 修改。"),
        ratatui::text::Line::raw(""),
        ratatui::text::Line::raw("[y] 启动并记住"),
        ratatui::text::Line::raw("[n] 跳过并记住    [Esc] 本次跳过（不记住）"),
    ]
}
```

4b. 在 `let layout = compute_layout(area, input, pending_quit, &popup, queued_height);`
（`app.rs:374`）之后、`terminal.draw(...)` 之前插入：

```rust
        if runtime_notice_pending {
            runtime_notice_pending = false;
            if let Some(crate::tui::subagents::RuntimeStartupIntent::DisabledNotice { reason }) =
                &runtime_intent
            {
                history.push(
                    HistoryCell::Separator {
                        label: Some(reason.clone()),
                    },
                    layout.chunks[0].width,
                );
            }
        }
```

4c. 把 `app.rs:494-513` 的按键块替换为（注意 `Esc` 不再落盘）：

```rust
                if let Some(crate::tui::subagents::RuntimeStartupIntent::Prompt) = &runtime_intent {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            persist_runtime_choice(
                                &workdir,
                                crate::tui::runtime_prefs::RuntimePreference::Always,
                                history,
                                layout.chunks[0].width,
                            );
                            if let Some(tx) = &runtime_choice_tx {
                                let _ = tx.blocking_send(
                                    crate::tui::subagents::RuntimeStartupChoice::Start,
                                );
                            }
                            runtime_intent = None;
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') => {
                            persist_runtime_choice(
                                &workdir,
                                crate::tui::runtime_prefs::RuntimePreference::Never,
                                history,
                                layout.chunks[0].width,
                            );
                            if let Some(tx) = &runtime_choice_tx {
                                let _ = tx.blocking_send(
                                    crate::tui::subagents::RuntimeStartupChoice::ContinueWithoutDelegation,
                                );
                            }
                            runtime_intent = None;
                        }
                        // Esc means "skip this time"; it must NOT overwrite the
                        // stored preference, so the next launch asks again.
                        KeyCode::Esc => {
                            if let Some(tx) = &runtime_choice_tx {
                                let _ = tx.blocking_send(
                                    crate::tui::subagents::RuntimeStartupChoice::ContinueWithoutDelegation,
                                );
                            }
                            runtime_intent = None;
                        }
                        _ => {}
                    }
                    continue;
                }
```

4d. 新增落盘助手（模块级）：

```rust
/// Persists a runtime preference, but never blocks the session on failure.
fn persist_runtime_choice(
    workdir: &std::path::Path,
    pref: crate::tui::runtime_prefs::RuntimePreference,
    history: &mut HistoryState,
    width: u16,
) {
    if let Err(error) = crate::tui::runtime_prefs::save(workdir, pref) {
        history.push(
            HistoryCell::Separator {
                label: Some(format!("无法保存偏好（本次仍然生效）: {error}")),
            },
            width,
        );
    }
}
```

4e. 在 `execute_slash_command` 中新增 `Runtime` 分支（放在 `SlashCommand::Mcp => {` 之前）：

```rust
        SlashCommand::Runtime => {
            use crate::tui::runtime_prefs::{self, RuntimePreference};
            let label = match crate::tui::slash::parse_runtime_args(
                args.as_deref().unwrap_or(""),
            ) {
                Ok(crate::tui::slash::RuntimeAction::Status) => {
                    let current = runtime_prefs::load(workdir);
                    format!(
                        "子 Agent runtime 偏好: {}（来源: {}）; 重启后生效",
                        match current {
                            RuntimePreference::Ask => "ask",
                            RuntimePreference::Always => "always",
                            RuntimePreference::Never => "never",
                        },
                        runtime_prefs::preferences_path(workdir).display()
                    )
                }
                Ok(crate::tui::slash::RuntimeAction::Set(pref)) => {
                    match runtime_prefs::save(workdir, pref) {
                        Ok(()) => format!(
                            "已设为 {}（重启后生效）",
                            match pref {
                                RuntimePreference::Ask => "ask",
                                RuntimePreference::Always => "always",
                                RuntimePreference::Never => "never",
                            }
                        ),
                        Err(error) => format!("无法保存偏好: {error}"),
                    }
                }
                Err(usage) => usage,
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
```

- [ ] **Step 5: 运行测试确认通过**

```bash
cargo test -p yi-agent --bin yi-agent -- runtime 2>&1 | tail -20
cargo test -p yi-agent --bin yi-agent -- escape_skips 2>&1 | tail -10
```
预期：全部通过。

- [ ] **Step 6: 全量回归、格式化、提交**

```bash
cargo fmt --all
cargo test -p yi-agent --bin yi-agent 2>&1 | tail -10
cargo clippy -p yi-agent --all-targets --all-features -- -D warnings 2>&1 | tail -5
git add crates/yi-agent/src/tui/app.rs crates/yi-agent/src/tui/slash.rs
git commit -m "feat(tui): honour persisted runtime preference and add /runtime"
```

> 若 Task 2 后因穷尽 match 无法编译而合并了提交，本步的提交信息请用
> `feat(tui): add /runtime command and honour persisted startup preference`，
> 并在正文注明同时包含 slash 定义与执行分支。

---

## Task 5: `main.rs` 读偏好并折算 intent

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`（调用点在 `main.rs:1591-1609` 附近）

**Interfaces:**
- Consumes: `runtime_prefs::load`、`RuntimeStartupIntent`、`RuntimeStartupChoice`。
- Produces: 无新公开 API；仅是装配点。

- [ ] **Step 1: 写失败的测试**

在 `main.rs` 的测试模块中追加（该模块已有 `runtime_*` 相关测试可参照）：

```rust
    #[test]
    fn preference_maps_to_startup_intent() {
        use crate::tui::runtime_prefs::{save, RuntimePreference};
        use crate::tui::subagents::RuntimeStartupIntent;

        let dir = tempfile::TempDir::new().unwrap();

        save(dir.path(), RuntimePreference::Ask).unwrap();
        assert!(matches!(
            startup_intent_for(dir.path()),
            Some(RuntimeStartupIntent::Prompt)
        ));

        save(dir.path(), RuntimePreference::Always).unwrap();
        assert!(matches!(
            startup_intent_for(dir.path()),
            Some(RuntimeStartupIntent::AutoStart)
        ));

        save(dir.path(), RuntimePreference::Never).unwrap();
        assert!(matches!(
            startup_intent_for(dir.path()),
            Some(RuntimeStartupIntent::DisabledNotice { .. })
        ));
    }

    #[test]
    fn never_notice_is_chinese_and_points_at_the_command() {
        use crate::tui::runtime_prefs::{save, RuntimePreference};
        use crate::tui::subagents::RuntimeStartupIntent;

        let dir = tempfile::TempDir::new().unwrap();
        save(dir.path(), RuntimePreference::Never).unwrap();
        let Some(RuntimeStartupIntent::DisabledNotice { reason }) = startup_intent_for(dir.path())
        else {
            panic!("never must produce a disabled notice");
        };
        assert!(reason.contains("/runtime"), "reason: {reason}");
        assert!(reason.contains("已禁用"), "reason: {reason}");
    }

    #[test]
    fn reading_the_intent_does_not_create_the_preference_directory() {
        use crate::tui::runtime_prefs::preferences_path;
        use crate::tui::subagents::RuntimeStartupIntent;

        let dir = tempfile::TempDir::new().unwrap();
        // A project that never opted in has no `.yi-agent/`. Deriving the
        // startup intent must stay read-only: creating the directory would
        // dirty every project the user merely launched the TUI in.
        assert!(matches!(
            startup_intent_for(dir.path()),
            Some(RuntimeStartupIntent::Prompt)
        ));
        assert!(
            !preferences_path(dir.path()).exists(),
            "deriving the startup intent must not create .yi-agent/"
        );
    }
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cargo test -p yi-agent --bin yi-agent -- preference_maps_to_startup_intent 2>&1 | tail -20
```
预期：编译失败，`cannot find function 'startup_intent_for'`。

- [ ] **Step 3: 实现折算函数**

在 `main.rs` 中 `fn attach_tui_runtime(` 之前插入：

```rust
/// Maps the persisted runtime preference to what the TUI should do on launch.
///
/// The preference file is the single source of truth: a missing or malformed
/// file reads as `Ask`, so existing users keep today's behaviour.
fn startup_intent_for(
    workdir: &std::path::Path,
) -> Option<crate::tui::subagents::RuntimeStartupIntent> {
    use crate::tui::runtime_prefs::{self, RuntimePreference};
    use crate::tui::subagents::RuntimeStartupIntent;
    use crate::tui::runtime_prefs::{self, RuntimePreference};

    Some(match runtime_prefs::load(workdir) {
        RuntimePreference::Ask => RuntimeStartupIntent::Prompt,
        RuntimePreference::Always => RuntimeStartupIntent::AutoStart,
        RuntimePreference::Never => RuntimeStartupIntent::DisabledNotice {
            reason: format!(
                "已禁用子 Agent 委派（{}: never）；用 /runtime 开启",
                runtime_prefs::preferences_path(workdir).display()
            ),
        },
    })
}
```

- [ ] **Step 4: 接线到 `run_tui`（含 `always` 预置 `Start`）**

把 `main.rs:1599-1604` 的

```rust
                Some(crate::tui::subagents::RuntimeStartPrompt {
                    title: "启动本地 Agent Runtime?".into(),
                    body: "启动后可以直接用自然语言创建和管理子 Agent。按 y 启动，按 n 跳过。".into(),
                }),
                Some(runtime_choice_tx),
```

替换为

```rust
                runtime_intent,
                // The driver's `RuntimeChoice(None) => break` means this sender
                // must outlive the session: dropping it would end the run.
                Some(runtime_choice_tx),
```

并在 `run_tui_agent` 中、`tokio::spawn(async move { ... })` 之前（即创建
`runtime_choice_tx` 之后、driver spawn 之前，`main.rs:1286` 附近）插入：

```rust
        // `always` reuses the existing attach path by pre-seeding the choice the
        // driver would otherwise wait for. Capacity is 1 and nothing has been
        // sent yet, so `try_send` cannot fail here.
        let runtime_intent = startup_intent_for(&workdir);
        if matches!(
            runtime_intent,
            Some(crate::tui::subagents::RuntimeStartupIntent::AutoStart)
        ) {
            let _ = runtime_choice_tx
                .try_send(crate::tui::subagents::RuntimeStartupChoice::Start);
        }
```

> 注意：`workdir` 在 `run_tui_agent` 里是入参 `std::path::PathBuf`。这里可以安全借用，因为
> driver 闭包内 `main.rs:1323` 写的是 `let _ = workdir;` —— 这是**通配符模式**（wildcard
> pattern），它**不绑定也不移动** `workdir`（若换成 `let _guard = workdir;` 就会 move）。
> 因此 `workdir.clone()`（`main.rs:1606`）与这里的 `&workdir` 都能编译。改动这一行时不要
> 误把它写成 `let _x = workdir;`。

- [ ] **Step 5: 运行测试确认通过**

```bash
cargo test -p yi-agent --bin yi-agent -- preference_maps_to_startup_intent 2>&1 | tail -10
cargo test -p yi-agent --bin yi-agent -- never_notice 2>&1 | tail -10
cargo test -p yi-agent --bin yi-agent -- reading_the_intent_does_not_create_the_preference_directory 2>&1 | tail -10
```
预期：3 passed。

**为什么需要第三条测试**：`startup_intent_for` 在 `Ask`/`Always`/`Never` 三条路径上都会经过
`load`，而 `load` 只读不写。但如果将来有人为了"顺手建目录"或"写默认值"而在读路径里调用
`save` / `create_dir_all`，**每次启动 TUI 都会在每个项目里凭空创建 `.yi-agent/`**，弄脏用户
仓库，而现有断言"返回 `Prompt`"照样通过。第三条测试断言"读 intent 不产生目录"，用一条断言
钉住这个副作用。

- [ ] **Step 6: 全量回归、格式化、提交**

```bash
cargo fmt --all
cargo test -p yi-agent --bin yi-agent 2>&1 | tail -10
cargo clippy -p yi-agent --all-targets --all-features -- -D warnings 2>&1 | tail -5
git add crates/yi-agent/src/main.rs
git commit -m "feat(tui): derive runtime startup intent from the stored preference"
```

---

## Task 6: 文档条目

**Files:**
- Modify: `docs/bug-list.md`
- Modify: `docs/project-management/subagent-runtime.md`

**Interfaces:**
- Consumes: 全部前序任务。
- Produces: 无代码接口。

> **背景说明**：TUI 上"面向用户"的行一律中文，英文只用于 `tracing` 日志与内部诊断。
> 本改动让 `main.rs` 生成的中文提示字符串第一次出现在 TUI 的 history 中——此前
> `main.rs` 只产出英文诊断（被路由到 logger）；这是 `main.rs` 侧首次产生用户可见文案，
> 因此 Task 5 的两个单测（`never_notice_is_chinese_and_points_at_the_command` 等）直接
> 以"必须含 `/runtime` 与 `已禁用`"作为契约，防止将来被改回英文。

- [ ] **Step 1: 记录到 bug-list**

按 `docs/bug-list.md` 现有条目格式（`- [x] 问题描述（修复：... 见 `path`。验证：`cmd`）`）
追加一条，问题行写：

```
- [x] TUI 每次启动都弹「启动本地 Agent Runtime?」，且按 n 后本会话与后续会话都无法再启用子 Agent（修复：偏好持久化到项目级 `.yi-agent/preferences.json`（`ask`/`always`/`never`，缺失或损坏一律 `ask`，原子写，见 `yi-agent.rs/crates/yi-agent/src/tui/runtime_prefs.rs`）；启动时由 `main.rs` `startup_intent_for` 折算为 `RuntimeStartupIntent`（`Prompt`/`AutoStart`/`DisabledNotice`），`always` 复用既有 attach 路径——预置 `RuntimeStartupChoice::Start` 到 `runtime_choice_tx`，driver 与 daemon 零改动；`y`/`n` 落盘、`Esc` 仅本次不落盘；新增 `/runtime [ask|always|never]` 逆转选择；弹窗文案改为区分三者且 `box_h` 6→8；删除零引用死代码 `RuntimeBootstrapModel`/`RuntimeBootstrapState`/`TuiRuntimeMode`。见 [设计](../superpowers/specs/2026-09-30-tui-subagent-runtime-preference-design.md)、[计划](../superpowers/plans/2026-09-30-tui-subagent-runtime-preference-impl.md)。验证：`cargo test -p yi-agent --bin yi-agent`）
```

> 实现时请把这条的单行内容写成与仓库现有条目一致的一行；上面用多行只是为了可读。

- [ ] **Step 2: 更新 subagent-runtime 进度清单**

在 `docs/project-management/subagent-runtime.md` 的清单中追加一条已完成项，写明偏好文件的
三态、`/runtime` 命令，以及验证命令 `cargo test -p yi-agent --bin yi-agent`。

- [ ] **Step 3: 提交**

```bash
git add docs/bug-list.md docs/project-management/subagent-runtime.md
git commit -m "docs: record TUI runtime startup preference"
```

---

## Task 7: 端到端手工确认

**Files:** 无（验证任务）。

**Interfaces:**
- Consumes: 全部前序任务。
- Produces: 验证结论。

- [ ] **Step 1: 构建二进制**

```bash
cd yi-agent-rs && cargo build -p yi-agent 2>&1 | tail -3
```

- [ ] **Step 2: 在一个临时 git 项目里逐条走查**

```bash
TMP=$(mktemp -d) && cd "$TMP" && git init -q && echo ok > README.md \
  && git add . && git -c user.email=a@b -c user.name=t commit -qm init && pwd
```
然后在该目录运行目标二进制，逐条确认：

1. 无 `.yi-agent/preferences.json` → 弹窗（与今天一致）。
2. 按 `y` → 子 Agent 启用；`cat .yi-agent/preferences.json` 应为
   `{"subagent_runtime": "always"}`；重启不再弹窗。
3. `/runtime` → 显示 `子 Agent runtime 偏好: always`；`/runtime never` → 提示"重启后生效"。
4. 重启（此时为 `never`）→ 不弹窗、无委派、且首屏有一行中文禁用提示。
5. `/runtime ask` → 重启后重新弹窗。
6. 按 `Esc` → 本次无委派，且**文件不存在**（或内容未变）；重启仍弹窗。
7. 手工把文件写成 `{"subagent_runtime":"bogus"}` → 启动不失败，行为等同 `ask`，
   日志有 warn。

- [ ] **Step 3: 清理**

```bash
rm -rf "$TMP"
```

- [ ] **Step 4: 记录结论**

把 7 条的实际观察结果写入 `docs/bug-list.md` 该条目的"验证"段（Task 6 已建立的条目），
若有任一条不符，回到对应任务修复后重跑本任务。

---

## 完成标准

- [ ] `cargo test -p yi-agent --bin yi-agent` 全绿
- [ ] `cargo clippy -p yi-agent --all-targets --all-features -- -D warnings` 通过
- [ ] `cargo fmt --all` 无改动
- [ ] `subagents.rs` 中 `#![allow(dead_code)]` 已移除且无 `unused` 告警
- [ ] 两个旧测试已不再写 `/tmp/.yi-agent/`（改 `TempDir`）
- [ ] Task 7 的 7 条手工观察全部符合预期
- [ ] 文档条目已提交
