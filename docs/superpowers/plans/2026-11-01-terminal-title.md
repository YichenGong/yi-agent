# 终端标题上报（OSC 2）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 TUI 把「项目名 + 运行态圆点」写进终端标签页标题（OSC 2）：空闲 `myproject`、运行中 `● myproject`、退出还原为空串。

**Architecture:** 新建独立的 `tui/title.rs`（不并入 `notify/`），分三层：纯函数（`project_label` / `compose`）、编码（`SetTitle` 实现 `crossterm::Command`）、策略（`TitleWriter` + `TitleState` 做 diff，`TitleSink` trait 隔离「写哪」）。最外层加一个 `TitleHandle` 聚合器，让 `run_loop` 只多一个参数。`run_loop` 每帧在绘制间隙读 `is_running` 原子量并同步标题；`run_tui` 的清理段在 `LeaveAlternateScreen` 之后走独立路径还原。

**Tech Stack:** Rust 2024（`rust-version = 1.85`）、crossterm 0.28（自定义 `Command`）、ratatui、`unicode-width`（workspace 已开 `cjk` feature）、Vitest（桌面端一处标签表）。

## Global Constraints

- 规范来源：`docs/superpowers/specs/2026-11-01-terminal-title-design.md`（下称 spec）。与本计划冲突时以 spec 为准；发现冲突先停下来报告，不要自行选边。
- `MAX_COLUMNS = 24`、`FALLBACK_PROJECT = "yi-agent"`、运行态前缀 `"● "`（U+25CF + 空格）、终止符 BEL `\u{7}`（**不用** ST `\u{1b}\\`）。
- 标题写入**不得**影响退出码：`clear` 返回 `()`，不得并入 `cleanup_error`。
- tmux / zellij 内**一个字节都不写**（不做 DCS 透传）。
- 配置键 `tui.terminal_title` 取值必须是 JSON 字符串 `"true"` / `"false"`，默认 `"true"`；解析用 `!matches!(v.as_deref(), Some("false"))`，**不得**写成 `== Some("true")`。
- 每处 commit 前 `cargo fmt --all`；合并前 `just ci` 全绿。
- 所有 `cargo` 命令在 `yi-agent-rs/` 下执行。若 `cargo` 不在 PATH：
  `export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`

---

## File Structure

| 文件 | 动作 | 职责 |
|---|---|---|
| `crates/yi-agent/src/tui/title.rs` | **新建** | 本功能全部逻辑：常量、`project_label`、`compose`、`SetTitle`、`TitleSink`/`StdoutSink`/`RecordingSink`、两个枚举、`TitleState`、`TitleWriter`、`TitleHandle` |
| `crates/yi-agent/src/tui/mod.rs` | 修改 | 登记 `pub mod title;`（字母序，`theme` 与 `trace` 之间） |
| `crates/yi-agent/src/tui/app.rs` | 修改 | 构造 handle、每帧 sync、退出 clear、`run_loop` 加参数、15 处调用点、`snapshot_for_tests` 补 2 字段、集成与护栏测试 |
| `crates/yi-agent/src/tui/slash.rs` | 修改 | `TuiConfigSnapshot` 加 2 字段、`render` 加一行、测试字面量补字段 |
| `crates/yi-agent/src/main.rs` | 修改 | 快照构造处填 2 个新字段 |
| `crates/yi-agent-runtime/src/settings.rs` | 修改 | `TuiSettings` 字段、`FIELDS` 登记、非法值白名单 |
| `crates/yi-agent-runtime/src/config.rs` | 修改 | `RuntimeConfig` 字段、解析、`default()`、`sample_config()` |
| `crates/yi-agent-runtime/src/models.rs` | 修改 | 2 处 `RuntimeConfig` 字面量 |
| `crates/yi-agent-app-server/src/{test_support,rpc_tests,onboarding_rpc,model_rpc}.rs` | 修改 | 5 处 `RuntimeConfig` 字面量 |
| `crates/yi-agent-subagent/src/{attach,lib}.rs` + `tests/attach_delegation.rs` | 修改 | 5 处 `RuntimeConfig` 字面量 |
| `desktop/src/components/SettingsRuntimeTab.tsx` | 修改 | `VALUE_LABELS` 加一行 |
| `desktop/src/components/SettingsRuntimeTab.test.tsx` | 修改 | 补两条 `labelFor` 断言 |

**为什么 `title.rs` 不塞进 `notify/`**：通知是离散事件（攒候选、去抖、择一、丢弃无所谓），标题是持续状态（有当前值、需 diff、退出必须还原）。两者没有共享状态，只复用 `sanitize` 与 `is_multiplexed` 两个纯工具函数——复用工具函数不构成合并理由。

---

## Task 1: 纯函数层（常量 + `project_label` + `compose`）

**Files:**
- Create: `crates/yi-agent/src/tui/title.rs`
- Modify: `crates/yi-agent/src/tui/mod.rs`

**Interfaces:**
- Consumes: `crate::tui::notify::text::sanitize(&str) -> String`（`pub`）、`crate::tui::wrap::truncate_to_width(&str, usize) -> String`（`pub(crate)`，同 crate 可用）
- Produces:
  - `pub const MAX_COLUMNS: usize = 24;`
  - `pub const FALLBACK_PROJECT: &str = "yi-agent";`
  - `pub fn project_label(cwd: &Path) -> String`
  - `pub fn compose(project: &str, running: bool) -> String`

- [ ] **Step 1: 登记模块并建最小骨架**

编辑 `crates/yi-agent/src/tui/mod.rs`，在 `pub mod theme;` 与 `pub mod trace;` 之间插入一行（保持字母序）：

```rust
pub mod theme;
pub mod title;
pub mod trace;
```

创建 `crates/yi-agent/src/tui/title.rs`：

```rust
//! 终端标签页标题（OSC 2）。
//!
//! 与 `notify/` 平级而非其子模块：通知是离散事件（攒候选、去抖、丢弃
//! 无所谓），标题是持续状态（有当前值、需 diff、退出必须还原）。两者没有
//! 共享状态，只复用 `sanitize` 与 `is_multiplexed` 两个纯工具。

use std::path::Path;

use unicode_width::UnicodeWidthStr;

/// 标题的显示列宽上限。
///
/// 终端标签栏普遍 15-25 列，24 是「尽量多显示又不挤掉两侧标签」的折中；
/// 再长终端自己也会截断，而终端的截断不受我们控制（可能切在 CJK 中间）。
pub const MAX_COLUMNS: usize = 24;

/// 取不到项目名时的回退值。
pub const FALLBACK_PROJECT: &str = "yi-agent";

/// 运行态前缀。`●` 是 U+25CF，实测 `UnicodeWidthChar::width` 返回 `Some(1)`，
/// 加一个空格共 2 列。
const RUNNING_PREFIX: &str = "● ";

/// 从工作目录取项目名：最后一级目录名。
///
/// 判据与 `app-server/src/lead.rs` 的 `lead_title_for_cwd` 一致
/// （`file_name() → to_str() → 非空` 三步），但产物不同：那边拼
/// 「`{name} 组长`」、回退「组长」，这里不带后缀、回退 [`FALLBACK_PROJECT`]。
pub fn project_label(cwd: &Path) -> String {
    cwd.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(FALLBACK_PROJECT)
        .to_string()
}

/// 组装标题：运行态前缀 + 项目名，经消毒后按显示列宽截断到 [`MAX_COLUMNS`]。
///
/// 消毒在截断之前：项目目录名可以含 ESC，一个裸 ESC 就能截断 OSC 序列并
/// 注入终端指令。这是安全边界，不是格式美化。
pub fn compose(project: &str, running: bool) -> String {
    let raw = if running {
        format!("{RUNNING_PREFIX}{project}")
    } else {
        project.to_string()
    };
    let cleaned = crate::tui::notify::text::sanitize(&raw);
    crate::tui::wrap::truncate_to_width(&cleaned, MAX_COLUMNS)
}
```

- [ ] **Step 2: 写全部测试（先写好，此时截断相关会红）**

在 `title.rs` 末尾追加：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_idle_is_just_the_project() {
        assert_eq!(compose("myproject", false), "myproject");
    }

    #[test]
    fn compose_running_adds_dot_prefix() {
        assert_eq!(compose("myproject", true), "● myproject");
    }

    #[test]
    fn compose_truncates_to_24_columns() {
        let long = "a".repeat(40);
        let out = compose(&long, false);
        let width = UnicodeWidthStr::width(out.as_str());
        assert!(width <= MAX_COLUMNS, "{out:?} is {width} cols");
        assert!(out.ends_with('…'), "should end with an ellipsis: {out:?}");
    }

    #[test]
    fn compose_measures_cjk_by_display_width() {
        // 24 个汉字 = 48 列。按 `chars().count()` 截断会得出「24 个字符」
        // 但实际 48 列，标签栏错位。
        let long = "中".repeat(24);
        let out = compose(&long, false);
        let width = UnicodeWidthStr::width(out.as_str());
        assert!(width <= MAX_COLUMNS, "{out:?} is {width} cols");
    }

    #[test]
    fn compose_counts_the_dot_prefix_against_the_budget() {
        // 前缀占 2 列，所以运行态下留给项目名的预算少 2 列。用『留下的 a
        // 的个数』断言，而不是断言两边总宽度相等——后者恒真，测不出东西。
        let long = "a".repeat(40);
        let idle = compose(&long, false);
        let running = compose(&long, true);
        assert!(running.starts_with("● "), "{running:?}");
        let idle_a = idle.matches('a').count();
        let running_a = running.matches('a').count();
        assert_eq!(
            idle_a,
            MAX_COLUMNS - 1,
            "idle should keep {} a's plus the ellipsis: {idle:?}",
            MAX_COLUMNS - 1
        );
        assert_eq!(
            running_a,
            MAX_COLUMNS - 1 - 2,
            "the 2-column prefix must cost exactly 2 a's: {running:?}"
        );
    }

    #[test]
    fn compose_strips_control_characters_from_project() {
        // 项目目录名可以含控制字符。判定用断言而非肉眼：把一个真实会被
        // 终端解释的 OSC 2 序列塞进项目名。
        let out = compose("evil\u{1b}]2;pwned\u{7}", false);
        assert_eq!(out.matches('\u{1b}').count(), 0, "ESC: {out:?}");
        assert_eq!(out.matches('\u{7}').count(), 0, "BEL: {out:?}");
    }

    #[test]
    fn compose_never_exceeds_budget_for_any_shape() {
        let twenty_three = "a".repeat(23);
        let twenty_four = "a".repeat(24);
        let twenty_five = "a".repeat(25);
        let cjk_twelve = "中".repeat(12);
        let cjk_thirteen = "中".repeat(13);
        let mixed = "a中".repeat(20);
        let shapes = [
            "",
            "a",
            twenty_three.as_str(),
            twenty_four.as_str(),
            twenty_five.as_str(),
            cjk_twelve.as_str(),
            cjk_thirteen.as_str(),
            mixed.as_str(),
        ];
        for shape in shapes {
            for running in [false, true] {
                let out = compose(shape, running);
                let width = UnicodeWidthStr::width(out.as_str());
                assert!(
                    width <= MAX_COLUMNS,
                    "shape {shape:?} running={running} -> {out:?} at {width} cols"
                );
            }
        }
    }

    #[test]
    fn project_label_uses_last_path_segment() {
        assert_eq!(project_label(Path::new("/work/my-app")), "my-app");
    }

    #[test]
    fn project_label_falls_back_for_root() {
        assert_eq!(project_label(Path::new("/")), FALLBACK_PROJECT);
    }

    #[test]
    fn project_label_falls_back_for_empty_name() {
        assert_eq!(project_label(Path::new("")), FALLBACK_PROJECT);
    }
}
```

- [ ] **Step 3: 跑测试，确认全绿（骨架阶段无红）**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::title`
Expected: `10 passed`。

本步无红灯是预期的：Step 1 的 `compose` 已经带了截断（我不打算先写一个不截断的版本再改，那只会多一次无用往返）。真正的红灯在 Task 2/3 引入新类型时出现。若这里报 `sanitize` 或 `truncate_to_width` 不可见，先修可见性再继续。

若 `compose_counts_the_dot_prefix_against_the_budget` 失败，多半是 `truncate_to_width` 在「恰好等于上限」时原样返回（`wrap.rs:95` 的 `<= width` 分支），检查你传的长度是否真的超限。

- [ ] **Step 4: 跑 clippy**

Run: `cd yi-agent-rs && cargo clippy -p yi-agent --bin yi-agent -- -D warnings`
Expected: 干净。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/mod.rs crates/yi-agent/src/tui/title.rs
git commit -m "feat(tui): 标题的纯函数层（项目名与组装）"
```

---

## Task 2: 编码层（`SetTitle` 实现 `Command`）

**Files:**
- Modify: `crates/yi-agent/src/tui/title.rs`

**Interfaces:**
- Consumes: 无
- Produces: `pub struct SetTitle(pub String);` 实现 `crossterm::Command`，`write_ansi` 输出 `"\u{1b}]2;{title}\u{7}"`

- [ ] **Step 1: 写失败的测试**

在 `title.rs` 的 `mod tests` 里追加：

```rust
    #[test]
    fn set_title_writes_exactly_one_osc2() {
        use crossterm::Command;
        let mut out = String::new();
        SetTitle("myproject".to_string())
            .write_ansi(&mut out)
            .unwrap();
        assert_eq!(out, "\u{1b}]2;myproject\u{7}");
        assert_eq!(out.as_bytes(), b"\x1b]2;myproject\x07");
    }

    #[test]
    fn set_title_with_empty_string_still_terminates() {
        use crossterm::Command;
        let mut out = String::new();
        SetTitle(String::new()).write_ansi(&mut out).unwrap();
        assert_eq!(out, "\u{1b}]2;\u{7}");
    }

    #[test]
    fn set_title_does_not_emit_st() {
        use crossterm::Command;
        let mut out = String::new();
        SetTitle("x".to_string()).write_ansi(&mut out).unwrap();
        assert!(
            !out.contains("\u{1b}\\"),
            "BEL and ST must not both appear: {out:?}"
        );
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::title::tests::set_title`
Expected: 编译失败 `cannot find struct SetTitle`。

- [ ] **Step 3: 实现 `SetTitle`**

在 `title.rs` 的 `compose` 之后、`#[cfg(test)] mod tests` 之前插入：

```rust
/// 写 OSC 2 设置终端标题。
///
/// 自己实现 `Command` 而不用 crossterm 自带的 `terminal::SetTitle`：本仓已
/// 确立「自定义 `Command` + 可断言 `write_ansi`」的范式（`notify/osc9.rs`、
/// `notify/bel.rs`），且单测需要逐字节断言——依赖 crossterm 的类型做不到。
///
/// 终止符用 BEL 而非 ST：BEL 是 OSC 的传统终止符，兼容面最广；ST 更规范但
/// 少部分老终端不认。
pub struct SetTitle(pub String);

impl crossterm::Command for SetTitle {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        write!(f, "\u{1b}]2;{}\u{7}", self.0)
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "tried to execute SetTitle using WinAPI; use ANSI instead",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}
```

用 `Error::other` 而非 `Error::new(ErrorKind::Other, ..)`：前者 1.85 已稳定（`Cargo.toml:24`），仓内已有先例（`crates/yi-agent-store/src/resident.rs:38`）；后者触发 `clippy::io_other_error`，而 `just lint` 是 `-D warnings`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::title`
Expected: `13 passed`。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/title.rs
git commit -m "feat(tui): OSC 2 编码层（SetTitle 命令）"
```

---

## Task 3: 策略层（sink、枚举、状态、writer、聚合器）

**Files:**
- Modify: `crates/yi-agent/src/tui/title.rs`

**Interfaces:**
- Consumes: `compose`（Task 1）、`SetTitle`（Task 2）
- Produces（后续任务只用这些）：
  - `pub enum DisabledReason { Off, Multiplexer, WriteFailed }`（`Debug, Clone, Copy, PartialEq, Eq`）
  - `pub enum TitleStatus { On, Off, Multiplexer, WriteFailed }`（同上 + `Display`，输出 `on` / `off` / `disabled (multiplexer)` / `disabled (write failed)`）
  - `pub trait TitleSink: Send { fn write_title(&mut self, text: &str) -> std::io::Result<()>; }`
  - `pub struct StdoutSink;`（`Debug, Default, Clone`）
  - `pub struct RecordingSink;`（`Debug, Clone, Default`；`calls() -> Vec<String>`、`fail_next()`）
  - `pub struct TitleState`（`Debug, Default`；字段私有）
  - `pub struct TitleWriter`：`new(bool, bool)`、`with_sink(bool, bool, Box<dyn TitleSink>)`、`status() -> TitleStatus`、`sync(&mut self, &mut TitleState, &str, bool) -> std::io::Result<()>`、`clear(&mut self, &mut TitleState)`
  - `pub struct TitleHandle`：`new(enabled: bool, multiplexed: bool, workdir: &Path)`、`with_sink(enabled, multiplexed, workdir, Box<dyn TitleSink>)`、`disabled()`、`status(&self) -> TitleStatus`、`sync(&mut self, running: bool)`、`clear(&mut self)`

- [ ] **Step 1: 写失败的测试**

在 `title.rs` 的 `mod tests` 里追加（`use super::*;` 已在作用域内，私有字段在子模块里可直接读写）：

```rust
    /// 构造一个能观测写入的 writer，并留下探针。
    fn writer_with_probe(
        enabled: bool,
        multiplexed: bool,
    ) -> (TitleWriter, RecordingSink, TitleState) {
        let sink = RecordingSink::default();
        let probe = sink.clone();
        let writer = TitleWriter::with_sink(enabled, multiplexed, Box::new(sink));
        (writer, probe, TitleState::default())
    }

    #[test]
    fn disabled_writer_writes_nothing() {
        let (mut writer, probe, mut state) = writer_with_probe(false, false);
        writer.sync(&mut state, "p", true).unwrap();
        writer.sync(&mut state, "p", false).unwrap();
        assert_eq!(probe.calls(), Vec::<String>::new());
        assert_eq!(writer.status(), TitleStatus::Off);
    }

    #[test]
    fn multiplexed_writer_writes_nothing() {
        let (mut writer, probe, mut state) = writer_with_probe(true, true);
        writer.sync(&mut state, "p", true).unwrap();
        assert_eq!(probe.calls(), Vec::<String>::new());
        assert_eq!(writer.status(), TitleStatus::Multiplexer);
    }

    #[test]
    fn off_wins_over_multiplexer() {
        assert_eq!(TitleWriter::new(false, true).status(), TitleStatus::Off);
    }

    #[test]
    fn status_maps_each_disabled_reason() {
        assert_eq!(TitleWriter::new(true, false).status(), TitleStatus::On);
        assert_eq!(TitleWriter::new(false, false).status(), TitleStatus::Off);
        assert_eq!(
            TitleWriter::new(true, true).status(),
            TitleStatus::Multiplexer
        );
        assert_eq!(TitleStatus::On.to_string(), "on");
        assert_eq!(TitleStatus::Off.to_string(), "off");
        assert_eq!(
            TitleStatus::Multiplexer.to_string(),
            "disabled (multiplexer)"
        );
        assert_eq!(
            TitleStatus::WriteFailed.to_string(),
            "disabled (write failed)"
        );
    }

    #[test]
    fn sync_is_a_no_op_when_running_is_unchanged() {
        let (mut writer, probe, mut state) = writer_with_probe(true, false);
        for _ in 0..5 {
            writer.sync(&mut state, "p", true).unwrap();
        }
        assert_eq!(probe.calls(), vec!["● p".to_string()]);
    }

    #[test]
    fn sync_rewrites_when_running_flips() {
        let (mut writer, probe, mut state) = writer_with_probe(true, false);
        writer.sync(&mut state, "p", true).unwrap();
        writer.sync(&mut state, "p", false).unwrap();
        assert_eq!(probe.calls(), vec!["● p".to_string(), "p".to_string()]);
    }

    #[test]
    fn sync_rewrites_when_project_changes() {
        // 运行态未变（true → true），但项目名变了：第 2 步会挡住这次 sync。
        // 现实中项目名在会话内不变，所以这里直接把 `observed_running` 置回
        // `None` 模拟「下一次观察」，验证第 3 步的 diff 用的是最新目标串。
        let (mut writer, probe, mut state) = writer_with_probe(true, false);
        writer.sync(&mut state, "p", true).unwrap();
        writer.sync(&mut state, "p", true).unwrap();
        assert_eq!(probe.calls().len(), 1, "unchanged running must not rewrite");

        state.observed_running = None;
        writer.sync(&mut state, "q", true).unwrap();
        assert_eq!(probe.calls(), vec!["● p".to_string(), "● q".to_string()]);
    }

    #[test]
    fn sync_skips_when_target_equals_written() {
        // 运行态翻转，但截断后的目标串与上次写过的完全相同 → 不写。
        let long = "a".repeat(40);
        let (mut writer, probe, mut state) = writer_with_probe(true, false);
        let already = compose(&long, true);
        state.written = Some(already);
        state.observed_running = Some(false);
        writer.sync(&mut state, &long, true).unwrap();
        assert_eq!(probe.calls(), Vec::<String>::new());
    }

    #[test]
    fn clear_empties_the_title() {
        let (mut writer, probe, mut state) = writer_with_probe(true, false);
        writer.sync(&mut state, "p", true).unwrap();
        writer.clear(&mut state);
        assert_eq!(probe.calls(), vec!["● p".to_string(), String::new()]);
        assert_eq!(state.written, Some(String::new()));
    }

    #[test]
    fn clear_is_a_no_op_when_already_empty() {
        let (mut writer, probe, mut state) = writer_with_probe(true, false);
        writer.clear(&mut state);
        writer.clear(&mut state);
        assert_eq!(probe.calls(), vec![String::new()]);
    }

    #[test]
    fn clear_is_a_no_op_when_never_wrote() {
        // 配置关闭时从没写过，退出也不该写空串去覆盖用户的标题。
        let (mut writer, probe, mut state) = writer_with_probe(false, false);
        writer.clear(&mut state);
        assert_eq!(probe.calls(), Vec::<String>::new());
    }

    #[test]
    fn a_failed_write_disables_the_writer() {
        let (mut writer, probe, mut state) = writer_with_probe(true, false);
        probe.fail_next();
        writer.sync(&mut state, "p", true).unwrap();
        assert_eq!(writer.status(), TitleStatus::WriteFailed);
        assert_eq!(probe.calls().len(), 1);

        // 自禁后不再碰 sink。
        writer.sync(&mut state, "p", false).unwrap();
        assert_eq!(probe.calls().len(), 1);
    }

    #[test]
    fn a_disabled_writer_stays_quiet_and_ok() {
        let (mut writer, probe, mut state) = writer_with_probe(true, false);
        probe.fail_next();
        writer.sync(&mut state, "p", true).unwrap();
        assert!(
            writer.sync(&mut state, "p", false).is_ok(),
            "a disabled writer must keep returning Ok"
        );
        assert_eq!(writer.status(), TitleStatus::WriteFailed);
    }

    #[test]
    fn clear_swallows_errors_without_disabling() {
        let (mut writer, probe, mut state) = writer_with_probe(true, false);
        probe.fail_next();
        writer.clear(&mut state); // 返回 ()，不 panic
        assert_eq!(
            writer.status(),
            TitleStatus::On,
            "clear must not self-disable"
        );
    }

    #[test]
    fn title_handle_exposes_project_and_status() {
        let handle = TitleHandle::new(false, false, Path::new("/work/my-app"));
        assert_eq!(handle.status(), TitleStatus::Off);
    }

    #[test]
    fn disabled_handle_never_writes() {
        let mut handle = TitleHandle::disabled();
        handle.sync(true);
        handle.sync(false);
        handle.clear();
        assert_eq!(handle.status(), TitleStatus::Off);
    }

    #[test]
    fn title_handle_writes_through_its_sink() {
        let sink = RecordingSink::default();
        let probe = sink.clone();
        let mut handle = TitleHandle::with_sink(
            true,
            false,
            Path::new("/work/my-app"),
            Box::new(sink),
        );
        handle.sync(true);
        handle.sync(true);
        handle.sync(false);
        assert_eq!(
            probe.calls(),
            vec!["● my-app".to_string(), "my-app".to_string()]
        );
        handle.clear();
        assert_eq!(probe.calls().last().map(String::as_str), Some(""));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::title::tests`
Expected: 编译失败，`cannot find type TitleWriter` / `RecordingSink` / `TitleStatus` / `TitleHandle`。

- [ ] **Step 3: 实现枚举、sink、状态与 writer**

在 `title.rs` 的 `SetTitle` 实现之后、`mod tests` 之前插入：

```rust
use std::io::{self, stdout};

/// 跳过原因。`None` = 正常可用。
///
/// 三种原因共用一个字段：`sync` / `clear` 只需一次 `is_some()` 判断，不必
/// 分别检查「配置关」「多路复用器」「写失败」三个来源，也就不存在两处状态
/// 不一致的可能。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisabledReason {
    /// 配置关闭（`tui.terminal_title = "false"`）。
    Off,
    /// tmux / zellij 内：不写任何字节（不做 DCS 透传）。
    Multiplexer,
    /// 写过一次失败，本会话自禁。
    WriteFailed,
}

/// `/config` 的展示值。
///
/// 四个变体里 `WriteFailed` 是运行期状态，而 `/config` 渲染的是启动时的
/// 快照，所以界面上只会看到前三者。保留第四态是为了将来接热更新时不必改
/// 渲染层。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleStatus {
    On,
    Off,
    Multiplexer,
    WriteFailed,
}

impl DisabledReason {
    fn to_status(self) -> TitleStatus {
        match self {
            Self::Off => TitleStatus::Off,
            Self::Multiplexer => TitleStatus::Multiplexer,
            Self::WriteFailed => TitleStatus::WriteFailed,
        }
    }
}

impl std::fmt::Display for TitleStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::On => "on",
            Self::Off => "off",
            Self::Multiplexer => "disabled (multiplexer)",
            Self::WriteFailed => "disabled (write failed)",
        })
    }
}

/// 往哪里写标题。
///
/// 抽成 trait 的唯一理由是「写入次数」必须可观测：`cargo test` 的 stdout
/// 不是 tty，`StdoutSink` 会短路，于是「逐帧重写导致闪烁」这个最需要钉住的
/// 属性在集成测试里完全看不见。生产实现只有 [`StdoutSink`]，每次会话几十次
/// 调用，动态分发的开销可忽略。
pub trait TitleSink: Send {
    fn write_title(&mut self, text: &str) -> io::Result<()>;
}

/// 写 stdout 的生产实现。
///
/// stdout 非 tty 时返回 `Ok(())` 而不写、也**不**置自禁：那不是「终端不支持
/// OSC 2」，只是「这次没有终端」。会话中途被重定向同样走这条路径。
#[derive(Debug, Default, Clone)]
pub struct StdoutSink;

impl TitleSink for StdoutSink {
    fn write_title(&mut self, text: &str) -> io::Result<()> {
        use std::io::IsTerminal;
        if !stdout().is_terminal() {
            return Ok(());
        }
        crossterm::execute!(stdout(), SetTitle(text.to_string()))
    }
}

/// 测试用：把写入记进外部持有的容器，调用方在 writer 被移动后仍可读。
///
/// `Box<dyn TitleSink>` 交出去就拿不回来了，所以记录必须共享：调用方先
/// `clone()` 一份探针，再把原 sink 装箱喂进去。
#[derive(Debug, Clone, Default)]
pub struct RecordingSink {
    calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    fail_next: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl RecordingSink {
    /// 至今收到的所有写入值，按调用顺序。
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// 让下一次写入返回 `Err`（只生效一次）。
    pub fn fail_next(&self) {
        self.fail_next
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl TitleSink for RecordingSink {
    fn write_title(&mut self, text: &str) -> io::Result<()> {
        self.calls.lock().unwrap().push(text.to_string());
        if self
            .fail_next
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(io::Error::other("injected failure"));
        }
        Ok(())
    }
}

/// 上一次的写入状态，用于避免每帧重写。
#[derive(Debug, Default)]
pub struct TitleState {
    /// 上一次**成功**写出的值。
    written: Option<String>,
    /// 上一次观察到的运行态。`None` = 还没观察过。
    observed_running: Option<bool>,
}

/// 标题的写入策略。
pub struct TitleWriter {
    /// 三种跳过原因的唯一来源。`None` = 正常可用。
    disabled_reason: Option<DisabledReason>,
    sink: Box<dyn TitleSink>,
}

impl std::fmt::Debug for TitleWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TitleWriter")
            .field("disabled_reason", &self.disabled_reason)
            .finish_non_exhaustive()
    }
}

impl TitleWriter {
    /// 生产构造：写 stdout。
    pub fn new(enabled: bool, multiplexed: bool) -> Self {
        Self::with_sink(enabled, multiplexed, Box::new(StdoutSink))
    }

    /// 可注入 sink 的构造（测试用）。
    ///
    /// 判定顺序固定为 `enabled` → `multiplexed`：配置关且身处 tmux 时报告
    /// `Off`——用户明确关掉的东西，没必要再报告环境限制。
    pub fn with_sink(enabled: bool, multiplexed: bool, sink: Box<dyn TitleSink>) -> Self {
        let disabled_reason = if !enabled {
            Some(DisabledReason::Off)
        } else if multiplexed {
            Some(DisabledReason::Multiplexer)
        } else {
            None
        };
        Self {
            disabled_reason,
            sink,
        }
    }

    /// 当前状态，供 `/config` 渲染。
    pub fn status(&self) -> TitleStatus {
        self.disabled_reason
            .map_or(TitleStatus::On, DisabledReason::to_status)
    }

    /// 按运行态同步标题。值没变就不写。
    ///
    /// 判定顺序（每一步都有理由）：
    /// 1. 已失效 → 直接返回，且**不**更新 `observed_running`：自禁是终态，
    ///    状态应冻结在「失效那一刻」，而不是看起来还在跟进。
    /// 2. 运行态未变 → 不重组字符串、不碰 stdout：挡的是「每帧调用」。
    /// 3. 目标值与自己写过的相同 → 跳过：挡的是「运行态翻转但截断后目标串
    ///    恰好一样」。
    /// 4. 写。失败则自禁并吞掉错误——调用方不该因标题失败而中断。
    pub fn sync(
        &mut self,
        state: &mut TitleState,
        project: &str,
        running: bool,
    ) -> io::Result<()> {
        if self.disabled_reason.is_some() {
            return Ok(());
        }
        if state.observed_running == Some(running) {
            return Ok(());
        }
        state.observed_running = Some(running);

        let target = compose(project, running);
        if state.written.as_deref() == Some(target.as_str()) {
            return Ok(());
        }

        match self.sink.write_title(&target) {
            Ok(()) => {
                state.written = Some(target);
                Ok(())
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "terminal title write failed; disabling for this session"
                );
                self.disabled_reason = Some(DisabledReason::WriteFailed);
                Ok(())
            }
        }
    }

    /// 退出还原：写空串。
    ///
    /// 返回 `()` 而非 `Result` 是刻意的——签名层面杜绝「有人把它的错误并进
    /// `run_tui` 的 `cleanup_error`」。那会让上百种不支持 OSC 2 的终端在每次
    /// 正常退出时都返回非零，被 shell 的 `set -e`、CI 脚本当成崩溃。
    ///
    /// 第一行为同样是失效短路：配置关闭或身处 tmux 时我们从没写过，也就
    /// 不该在退出时写空串去覆盖用户终端。
    pub fn clear(&mut self, state: &mut TitleState) {
        if self.disabled_reason.is_some() {
            return;
        }
        if state.written.as_deref() == Some("") {
            return;
        }
        if self.sink.write_title("").is_ok() {
            state.written = Some(String::new());
        }
    }
}

/// 把 writer、state 与项目名绑在一起，让 `run_loop` 只多一个参数。
///
/// `run_loop` 已经有 16 个参数（靠 `#[allow(clippy::too_many_arguments)]`
/// 压着），不该为标题再涨两个。这个聚合器是纯转发，不含新逻辑。
#[derive(Debug)]
pub struct TitleHandle {
    writer: TitleWriter,
    state: TitleState,
    project: String,
}

impl TitleHandle {
    /// 生产构造：从工作目录取项目名。
    pub fn new(enabled: bool, multiplexed: bool, workdir: &Path) -> Self {
        Self::with_sink(
            enabled,
            multiplexed,
            workdir,
            Box::new(StdoutSink),
        )
    }

    /// 可注入 sink 的构造（测试用）。
    pub fn with_sink(
        enabled: bool,
        multiplexed: bool,
        workdir: &Path,
        sink: Box<dyn TitleSink>,
    ) -> Self {
        Self {
            writer: TitleWriter::with_sink(enabled, multiplexed, sink),
            state: TitleState::default(),
            project: project_label(workdir),
        }
    }

    /// 永不写入。给那些只关心渲染、不关心标题的既有测试用。
    pub fn disabled() -> Self {
        Self {
            writer: TitleWriter::with_sink(false, false, Box::new(StdoutSink)),
            state: TitleState::default(),
            project: FALLBACK_PROJECT.to_string(),
        }
    }

    pub fn status(&self) -> TitleStatus {
        self.writer.status()
    }

    /// 同步标题。错误在 writer 内部已吞掉，这里也不返回。
    pub fn sync(&mut self, running: bool) {
        let _ = self.writer.sync(&mut self.state, &self.project, running);
    }

    /// 退出还原。
    pub fn clear(&mut self) {
        self.writer.clear(&mut self.state);
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::title`
Expected: `30 passed`（13 + 17）。

若 `title_handle_writes_through_its_sink` 失败，检查 `project_label(Path::new("/work/my-app"))` 是否为 `my-app`，以及 `compose` 是否把 `● ` 加了进去。

- [ ] **Step 5: 补 `StdoutSink` 的非 tty 测试**

在 `mod tests` 里追加（照 `notify/mod.rs:397-415` 的范式——真终端里跳过而不是给假结论）：

```rust
    /// `cargo test` 的 stdout 不是 TTY，这条恰好覆盖 `StdoutSink` 的第一道
    /// 短路：必须返回 `Ok` 而不是报错。
    #[test]
    fn stdout_sink_is_a_no_op_when_not_a_tty() {
        use std::io::IsTerminal;
        if std::io::stdout().is_terminal() {
            eprintln!("skip: stdout is a tty");
            return;
        }
        let mut sink = StdoutSink;
        assert!(sink.write_title("hello").is_ok());
    }
```

- [ ] **Step 6: 跑 clippy 与测试**

Run: `cd yi-agent-rs && cargo clippy -p yi-agent --bin yi-agent --all-targets -- -D warnings && cargo test -p yi-agent --bin yi-agent tui::title`
Expected: clippy 干净，`31 passed`。

- [ ] **Step 7: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/title.rs
git commit -m "feat(tui): 标题写入策略与聚合器（diff、自禁、退出还原）"
```

---

## Task 4: 配置键 `tui.terminal_title`（runtime crate）

**Files:**
- Modify: `crates/yi-agent-runtime/src/settings.rs`（`TuiSettings`、`FIELDS`、白名单）
- Modify: `crates/yi-agent-runtime/src/config.rs`（`RuntimeConfig`、解析、`default()`、`sample_config()`）

**Interfaces:**
- Consumes: 无
- Produces: `TuiSettings::terminal_title: Option<String>`、`FIELDS` 条目 `tui.terminal_title`、`RuntimeConfig::terminal_title: bool`（默认 `true`）

- [ ] **Step 1: 写失败的测试**

在 `config.rs` 的 `mod tests` 里、`tui_notification_defaults_apply_when_absent`（`:777`）之后追加：

```rust
    #[test]
    fn tui_terminal_title_reaches_runtime_config() {
        // 端到端钉子：写进 settings.json 的值必须真的出现在 RuntimeConfig
        // 上。缺了这条，字段名写错（比如 `tui.terminaltitle`）在编译期毫无
        // 反应，用户改了配置却永远拿默认值。
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let instance = IsolatedInstance::new();
        instance.write_settings(r#"{"tui":{"terminal_title":"false"}}"#);

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(instance.dir.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();

        assert!(!cfg.terminal_title, "explicit false must reach the config");
    }

    #[test]
    fn tui_terminal_title_defaults_apply_when_absent() {
        // 钉住 `!matches!(.., Some("false"))` 那行：键缺失时必须是 true。
        // 若有人写成 `== Some("true")`，这条会红。
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let instance = IsolatedInstance::new();
        instance.write_settings(r#"{}"#);

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(instance.dir.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();

        assert!(cfg.terminal_title, "标题默认开启");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime terminal_title`
Expected: 编译失败 `no field terminal_title on type RuntimeConfig`。

- [ ] **Step 3: 加 `TuiSettings` 字段**

在 `settings.rs` 的 `TuiSettings` 里、`pub notification_condition: Option<String>,` 之后加：

```rust
    /// `true` / `false`；`None` = 默认 `true`。
    ///
    /// 与同组三个通知键同构：必须是字符串而不是 `Option<bool>`，因为
    /// `FIELDS` 的 `Select` / `Text` 只接受 JSON 字符串，用 `Option<bool>`
    /// 会导致设置页写进 `"true"` 后反序列化读不回来。
    #[serde(default, deserialize_with = "de_optional_string")]
    pub terminal_title: Option<String>,
```

- [ ] **Step 4: 登记 `FIELDS`**

在 `settings.rs` 的 `FIELDS` 里、`tui.notifications` 那条（`FieldSpec { key: "tui.notifications", ... }`）之后插入：

```rust
    FieldSpec {
        key: "tui.terminal_title",
        group: "tui",
        label: "终端标题",
        field_type: FieldType::Select,
        default_display: "true",
        description: "在终端标签页标题里显示项目名与运行状态；空表示用默认（开）",
        options: &["true", "false"],
        min: None,
        max: None,
    },
```

`FieldSpec` 的 9 个字段（`key` / `group` / `label` / `field_type` / `default_display` / `description` / `options` / `min` / `max`）全部显式写出，没有 `..Default`。

- [ ] **Step 5: 加非法值白名单**

在 `settings.rs` 的 `if let Some(enabled) = self.tui.notifications.as_deref() { ... }` 块之后追加：

```rust
        if let Some(enabled) = self.tui.terminal_title.as_deref() {
            if !matches!(enabled.to_ascii_lowercase().as_str(), "true" | "false") {
                tracing::warn!(
                    value = enabled,
                    "settings.json has an unknown tui.terminal_title; falling back to the default"
                );
                self.tui.terminal_title = None;
            }
        }
```

- [ ] **Step 6: 加 `RuntimeConfig` 字段、解析、默认值**

在 `config.rs` 的 `RuntimeConfig` 结构体里、`notification_condition` 之后加：

```rust
    /// 是否把项目名与运行状态写进终端标签页标题
    /// （`tui.terminal_title`，默认 true）。
    pub terminal_title: bool,
```

在 `let notifications = !matches!(...)` 那一行（`:419`）之后加：

```rust
        // 与 `notifications` 同理：存储侧是字符串，这里解析成 bool。
        // 用 `!matches!(.., Some("false"))` 而不是 `== Some("true")`：
        // 后者会让「键缺失」（最常见情况）得到 false，把默认的 true 变成
        // false。
        let terminal_title = !matches!(settings.tui.terminal_title.as_deref(), Some("false"));
```

在 `Ok(RuntimeConfig {` 的构造里、`notification_condition,` 之后加：

```rust
            terminal_title,
```

在 `RuntimeConfig::default()` 里、`notification_condition: "unfocused".to_string(),` 之后加：

```rust
            terminal_title: true,
```

- [ ] **Step 7: 修 `sample_config()`**

在 `config.rs` 的 `sample_config()`（`#[cfg(test)]`）里、`notification_condition` 之后加：

```rust
        terminal_title: true,
```

- [ ] **Step 8: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime terminal_title`
Expected: `2 passed`。

- [ ] **Step 9: 加 `settings.rs` 两条白名单测试**

在 `settings.rs` 的 `mod tests` 里、`valid_notification_values_survive` 之后追加：

```rust
    #[test]
    fn invalid_terminal_title_flag_is_dropped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"tui":{"terminal_title":"maybe"}}"#).unwrap();
        let settings = load_from(&path);
        assert_eq!(
            settings.tui.terminal_title, None,
            "an unknown value must fall back to the default, not abort"
        );
    }

    #[test]
    fn valid_terminal_title_values_survive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"tui":{"terminal_title":"false"}}"#).unwrap();
        let settings = load_from(&path);
        assert_eq!(settings.tui.terminal_title.as_deref(), Some("false"));
    }
```

如果 `load_from` 在你的 crate 里叫别的名字（`settings.rs:1560` 附近有既有用例可对照），照那两条既有测试的取用方式写。

- [ ] **Step 10: 跑全部 runtime 测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime`
Expected: 全绿。

- [ ] **Step 11: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-runtime/src/settings.rs crates/yi-agent-runtime/src/config.rs
git commit -m "feat(runtime): 新增 tui.terminal_title 配置键"
```

---

## Task 5: 补齐 `RuntimeConfig` 的 12 个构造点

**Files:**
- Modify: `crates/yi-agent-runtime/src/models.rs`
- Modify: `crates/yi-agent-app-server/src/test_support.rs`、`rpc_tests.rs`、`onboarding_rpc.rs`、`model_rpc.rs`
- Modify: `crates/yi-agent-subagent/src/attach.rs`、`lib.rs`、`tests/attach_delegation.rs`

**Interfaces:**
- Consumes: `RuntimeConfig::terminal_title: bool`（Task 4）
- Produces: 无（纯补齐，让 workspace 重新编译）

**背景**：`RuntimeConfig` 的所有构造点都是穷举字面量、**没有一处** `..Default::default()` 兜底，Task 4 加字段后这些文件全部编译失败。这是编译期错误，不是隐藏风险，但跨三个 crate，容易漏。

- [ ] **Step 1: 让编译器列出全部失败点**

Run: `cd yi-agent-rs && cargo build --workspace --all-targets 2>&1 | grep -oE 'crates/[a-z-]+/(src|tests)/[a-z_]+\.rs:[0-9]+' | sort -u`
Expected: 约 12 处，分布在这些文件：`yi-agent-runtime/src/models.rs`（2）、`yi-agent-app-server/src/{test_support,rpc_tests×2,onboarding_rpc,model_rpc}.rs`（5）、`yi-agent-subagent/src/{attach×3,lib}.rs`（4）、`yi-agent-subagent/tests/attach_delegation.rs`（1）。

以编译器输出为准，不要照抄数量硬找——Task 4 的改动会让行号平移。

- [ ] **Step 2: 逐处补字段**

在每一处 `RuntimeConfig { ... }` 字面量里、`notification_condition` 之后插入一行（测试辅助取 `true`，与生产默认语义一致）：

```rust
        terminal_title: true,
```

缩进跟该字面量里相邻字段保持一致。若某处已有 `..Default::default()`（理论上没有，但以编译器报错为准），则不需要改。

- [ ] **Step 3: 确认编译通过**

Run: `cd yi-agent-rs && cargo build --workspace --all-targets`
Expected: 成功，无 `missing field`。

- [ ] **Step 4: 跑受影响的 crate**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib`
Expected: 全绿。

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib`
Expected: 全绿。

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: 全绿（900+ 条，较慢）。若卡住或 OOM，先 `ps aux | grep -v grep | grep cargo` 查残留进程再重跑。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-runtime/src/models.rs \
  crates/yi-agent-app-server/src/test_support.rs \
  crates/yi-agent-app-server/src/rpc_tests.rs \
  crates/yi-agent-app-server/src/onboarding_rpc.rs \
  crates/yi-agent-app-server/src/model_rpc.rs \
  crates/yi-agent-subagent/src/attach.rs \
  crates/yi-agent-subagent/src/lib.rs \
  crates/yi-agent-subagent/tests/attach_delegation.rs
git commit -m "chore: 补齐 RuntimeConfig 构造点的 terminal_title 字段"
```

---

## Task 6: `/config` 显示（`TuiConfigSnapshot` 加两个字段）

**Files:**
- Modify: `crates/yi-agent/src/tui/slash.rs`（字段、`render`、测试字面量）
- Modify: `crates/yi-agent/src/main.rs`（快照构造）
- Modify: `crates/yi-agent/src/tui/app.rs`（`snapshot_for_tests`）

**Interfaces:**
- Consumes: `TitleWriter::new` / `TitleStatus`（Task 3）、`crate::tui::notify::is_multiplexed` / `TerminalEnv::from_process`、`RuntimeConfig::terminal_title`（Task 4）
- Produces: `TuiConfigSnapshot::terminal_title: bool`（Task 7 要用它构造 `TitleHandle`）、`TuiConfigSnapshot::title_status: String`（仅用于渲染）

**为什么加两个字段**：`terminal_title` 是 Task 7 构造 `TitleHandle` 的输入；`title_status` 是渲染用的展示串。后者可由前者推出，但保留成字符串与同结构里的 `effective_backend` 完全同构（那个字段也是「解析后的展示名」）。

- [ ] **Step 1: 加字段与渲染行，并补测试**

在 `slash.rs` 的 `TuiConfigSnapshot` 里、`effective_backend` 之后加：

```rust
    /// 是否写终端标签标题（`tui.terminal_title`，默认 true）。
    pub terminal_title: bool,
    /// 标题状态：`on` / `off` / `disabled (multiplexer)`。
    ///
    /// `disabled (write failed)` 属运行期状态，而这是启动快照，看不到它。
    pub title_status: String,
```

在 `render`（`slash.rs:312-336`）的返回表达式末尾追加一段：

```rust
        ) + &format!("\n  terminal title: {}", self.title_status)
```

即把原来的

```rust
        ) + &format!(
            "\n\n  terminal notify: {}（method={} → {}, condition={}）",
            ...
        )
    }
```

改成

```rust
        ) + &format!(
            "\n\n  terminal notify: {}（method={} → {}, condition={}）",
            ...
        ) + &format!("\n  terminal title: {}", self.title_status)
    }
```

（`terminal title` 与 `terminal notify` 是同组，缩进两格、不空行。）

在 `slash.rs` 的 `config_snapshot_renders_key_fields_without_secrets`（`:1227`）里，给字面量补两个字段（`effective_backend: "osc9".into(),` 之后）：

```rust
            terminal_title: true,
            title_status: "on".into(),
```

并在 `let text = snap.render("claude-sonnet-4-5");` 之后追加断言：

```rust
        assert!(
            text.contains("terminal title: on"),
            "title status must be visible in /config: {text}"
        );
```

- [ ] **Step 2: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent config_snapshot_renders_key_fields`
Expected: 先因 `snapshot_for_tests` 缺字段而编译失败（下两步修完才绿）。

- [ ] **Step 3: 补 `snapshot_for_tests`**

在 `app.rs` 的 `snapshot_for_tests()`（`:349-367`）里、`effective_backend: "bel".into(),` 之后加：

```rust
        terminal_title: true,
        title_status: "on".into(),
```

**漏掉这一步会让整个 `app.rs` 测试二进制编不出来**（该工厂被 80 个 `#[test]` 函数使用）。

- [ ] **Step 4: 生产构造点填值**

在 `main.rs` 的 `let tui_config = crate::tui::slash::TuiConfigSnapshot { ... }`（`:2341`）里、`effective_backend: { ... },` 之后、`focus_degraded: false,` 之前加：

```rust
            terminal_title: config.terminal_title,
            title_status: {
                let multiplexed = crate::tui::notify::is_multiplexed(
                    &crate::tui::notify::TerminalEnv::from_process(),
                );
                crate::tui::title::TitleWriter::new(config.terminal_title, multiplexed)
                    .status()
                    .to_string()
            },
```

- [ ] **Step 5: 确认编译并跑测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent --no-run`
Expected: 编译成功。

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent config_snapshot_renders_key_fields`
Expected: PASS。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/slash.rs crates/yi-agent/src/main.rs crates/yi-agent/src/tui/app.rs
git commit -m "feat(tui): /config 显示终端标题状态"
```

---

## Task 7: 接线（启动构造、每帧 sync、退出 clear）

**Files:**
- Modify: `crates/yi-agent/src/tui/app.rs`

**Interfaces:**
- Consumes: `TitleHandle`（Task 3）、`TuiConfigSnapshot::terminal_title`（Task 6）
- Produces: `fn run_loop` 新增最后一个参数 `title: &mut crate::tui::title::TitleHandle`

**行号基准（本任务开始时的现状）**：`run_loop` 定义在 `:543-544`，调用点在 `:178`（生产）、`:402`、`:486`（`#[cfg(test)]` 辅助函数内）、`mod tests`（起点 `:4018`）内 12 处，共 15 处。

**为什么只多一个参数而不是两个**：spec §5 的说法是 `run_loop` 需要「当前运行态 + 项目名」来算标题，最直白的接法是注入 `&mut TitleWriter` 与 `&mut TitleState`（再加项目名共三样）。`run_loop` 已经是 16 参数、靠 `#[allow(clippy::too_many_arguments)]` 压着，再涨两个会让签名更难读且每处调用点都要多备两个局部变量。Task 3 的 `TitleHandle` 把 writer/state/project 三者聚合起来，`run_loop` 只多 1 个参数，15 处调用点各多 1 行。这是**等价实现**，不是缩减需求：`sync`/`clear` 的行为完全不变。

- [ ] **Step 1: 给 `run_loop` 加参数**

在 `app.rs` 的 `run_loop` 签名里，`theme: TuiTheme,` 之后加：

```rust
    title: &mut crate::tui::title::TitleHandle,
```

- [ ] **Step 2: 在 `run_loop` 内部每帧同步**

在 `let mut notifications = NotificationState::new();`（`:607`）附近、紧跟其后加一行注释（不需要新变量，`title` 已是入参）。

在 `notifications.take()` 那个块（`:870-876`）之后加：

```rust
        // 标题跟随运行态。值没变就不写（`TitleHandle` 内部 diff），所以每帧
        // 调用是安全的：一次会话只在状态翻转点真正写字节。
        title.sync(is_running.load(std::sync::atomic::Ordering::SeqCst));
```

用 `SeqCst` 而非 `Relaxed`：与同文件 `:2208`、`:2219` 已有的取消键判定保持一致，不引入新的内存序约定。

- [ ] **Step 3: 在 `run_tui` 里构造 handle**

在 `run_tui` 的 `let channels = TuiChannels { ... };`（`:170-176`）之后、`let result = run_loop(` 之前加：

```rust
    // 标题：必须在 `workdir` 被 move 进 `run_loop` 之前取项目名。
    let mut title = crate::tui::title::TitleHandle::new(
        config.terminal_title,
        crate::tui::notify::is_multiplexed(
            &crate::tui::notify::TerminalEnv::from_process(),
        ),
        &workdir,
    );
```

并在 `run_loop(...)` 的实参末尾（`theme,` 之后）加：

```rust
        &mut title,
```

- [ ] **Step 4: 批改其余 14 处调用点**

- `app.rs:402`、`:486`（两个 `#[cfg(test)]` 辅助函数体内）：在各自 `run_loop(` 之前加

  ```rust
      let mut title = crate::tui::title::TitleHandle::disabled();
  ```

  并在实参末尾加 `&mut title,`。

- `mod tests` 内 12 处：同样在每处 `run_loop(` 之前加 `let mut title = crate::tui::title::TitleHandle::disabled();`，实参末尾加 `&mut title,`。

用 `TitleHandle::disabled()` 而不是真 handle：这些测试只关心渲染，不该向 stdout 写 OSC 序列（`cargo test` 的 stdout 虽非 tty，但显式 never-write 更清晰）。

先跑一次编译拿到全部失败位置，再逐个补：

Run: `cd yi-agent-rs && cargo build -p yi-agent --all-targets 2>&1 | grep -E 'error\[E0061\]' | head -20`
Expected: 一批 `this function takes 17 arguments but 16 were supplied`。

- [ ] **Step 5: 退出还原（在 `run_tui` 的清理段）**

在 `run_tui` 的 `LeaveAlternateScreen` 那块（`:208-210`）之后、`if let Ok(dropped) = result {`（`:214`）之前插入：

```rust
    // 标题还原：独立路径，失败只记日志、不影响退出码。必须在
    // `LeaveAlternateScreen` 之后——alternate screen 关闭时多数终端会重置
    // 标题，先写空串等于白写。
    title.clear();
```

**不要**把它写进 `cleanup_error`。`TitleHandle::clear()` 返回 `()`，签名上就进不去。

**注意 Task 8 会替换这段**：Task 8 Step 3 把 `:197-210` 整块换成 `run_cleanup_with(..)`，而那个函数内部已经调用了 `title.clear()`。所以本步的内联 `title.clear()` 到 Task 8 就删掉了。两次 `clear` 即便同时存在也不会坏（第二次被 `state.written == Some("")` 短路），但不要留着——Task 8 的变异验证依赖「`clear` 只在 `run_cleanup_with` 里出现一次」。

- [ ] **Step 6: 编译并跑既有的全部 TUI 测试**

Run: `cd yi-agent-rs && cargo build -p yi-agent --all-targets`
Expected: 成功。

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::`
Expected: 既有测试全绿（`TitleHandle::disabled()` 不改变任何渲染行为）。

若某条既有测试红了，先确认是不是漏改了它的 `run_loop` 调用点。

- [ ] **Step 7: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/app.rs
git commit -m "feat(tui): 标题接线（启动构造、每帧 diff、退出还原）"
```

---

## Task 8: 集成测试与退出码护栏

**Files:**
- Modify: `crates/yi-agent/src/tui/app.rs`

**Interfaces:**
- Consumes: Task 7 的接线、`RecordingSink`（Task 3）、`ResizableTestBackend`（`app.rs:5302` 已存在）
- Produces: `fn run_cleanup_with`（`app.rs` 内私有）

**为什么抽 `run_cleanup_with` 而不是直接测 `run_tui`**：spec §8.5 要求「`run_tui` 正常退出时返回 `Ok(())`，标题还原失败不得改变退出码」。直接测 `run_tui` 做不到——它第一行就是 `enable_raw_mode()`（`app.rs:134`），测试进程没有 tty，必然 `Err`，根本走不到清理段。抽出的 `run_cleanup_with` 接受一个 `FnMut(&str) -> io::Result<()>` 执行器：生产传真实 crossterm 调用，测试传记录器。这样**清理顺序**与**退出码裁决**都可测，而 `run_tui` 的 `match (result, cleanup_error)`（`:218-222`）一行不动。这是 spec 意图的等价落地，不是放弃测试。

- [ ] **Step 1: 加集成测试辅助与三条集成测试**

在 `app.rs` 的 `mod tests` 里追加：

```rust
    /// 按脚本逐帧设置 `is_running`，脚本走完后按 Ctrl+Q 退出。
    struct TitleScriptEvents {
        script: Vec<bool>,
        is_running: Arc<AtomicBool>,
        poll_count: Cell<usize>,
    }

    impl EventSource for TitleScriptEvents {
        // `run_loop` 每帧的顺序是：draw → title.sync(is_running) → events.poll()。
        // 也就是说第 N 帧的 sync 看到的是「第 N-1 次 poll 写入的值」。所以
        // `is_running` 必须用脚本首帧的值初始化（首帧 sync 才能看到正确状态），
        // 而 poll 里要写入的是**下一帧**该看到的值，即 `script[index + 1]`。
        // 若在 poll 里就地写 `script[index]`，首帧 sync 会先看到初始的 false，
        // 白写一次 "p"，断言序列变成 ["p", "● p", "p"]。
        fn poll(&self, _timeout: Duration) -> std::io::Result<Option<Event>> {
            let index = self.poll_count.get();
            if index >= self.script.len() {
                return Ok(Some(Event::Key(KeyEvent::new(
                    KeyCode::Char('q'),
                    KeyModifiers::CONTROL,
                ))));
            }
            if let Some(next) = self.script.get(index + 1) {
                self.is_running
                    .store(*next, std::sync::atomic::Ordering::SeqCst);
            }
            self.poll_count.set(index + 1);
            Ok(None)
        }
    }

    /// 跑 `run_loop` 直到事件源说停，返回记录到的标题写入序列。
    ///
    /// 用 `RecordingSink` 而非真 stdout：`cargo test` 的 stdout 不是 tty，
    /// `StdoutSink` 会短路，写入次数根本看不见。
    fn run_loop_for_title(workdir: &std::path::Path, script: Vec<bool>) -> Vec<String> {
        let backend = Rc::new(RefCell::new(TestBackend::new(80, 24)));
        let mut terminal = Terminal::new(ResizableTestBackend {
            inner: Rc::clone(&backend),
        })
        .unwrap();
        let (_agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        // 首帧 sync 发生在第一次 poll 之前，故初值必须取脚本首帧。
        let first = script.first().copied().unwrap_or(false);
        let is_running = Arc::new(AtomicBool::new(first));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let source = TitleScriptEvents {
            script,
            is_running: Arc::clone(&is_running),
            poll_count: Cell::new(0),
        };

        let sink = RecordingSink::default();
        let probe = sink.clone();
        let mut title = crate::tui::title::TitleHandle::with_sink(
            true,
            false,
            workdir,
            Box::new(sink),
        );
        let config = snapshot_for_tests();

        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut history,
            &mut input,
            TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            ),
            &is_running,
            &source,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            workdir.to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
            &config,
            &catalog_for_tests(),
            TuiTheme::DARK,
            &mut title,
        )
        .unwrap();

        probe.calls()
    }

    #[test]
    fn title_follows_the_running_flag() {
        let calls = run_loop_for_title(std::path::Path::new("/work/p"), vec![true, false]);
        assert_eq!(calls, vec!["● p".to_string(), "p".to_string()]);
    }

    #[test]
    fn title_is_written_once_while_running_stays_true() {
        let calls = run_loop_for_title(std::path::Path::new("/work/p"), vec![true; 60]);
        assert_eq!(
            calls,
            vec!["● p".to_string()],
            "60 identical frames must not rewrite the title (would flicker)"
        );
    }

    #[test]
    fn cancelled_turn_returns_title_to_idle() {
        // 与通知功能相反：`Done{Interrupted}` 不发通知，但标题必须回空闲。
        // 这里用「置 false」等价于 `main.rs` 收尾处的 `store(false)`。
        let calls = run_loop_for_title(std::path::Path::new("/work/p"), vec![true, false]);
        assert_eq!(
            calls.last().map(String::as_str),
            Some("p"),
            "an interrupted turn must not leave the running dot on the tab"
        );
    }
```

上面用到的 `KeyCode` / `KeyEvent` / `KeyModifiers`（`app.rs:4136`）、`Cell` / `RefCell`（`:4139`）、`Arc`（`:4141`）、`AtomicBool`（`:4142`）、`Duration`（`:4143`）、`TestBackend`（`:4138`）都已在 `mod tests` 的 `use` 清单里，无需新增。

`Path` 与 `Ordering` **刻意写成全限定** `std::path::Path::new(..)` / `std::sync::atomic::Ordering::SeqCst`：`app.rs` 顶层从未 `use std::path::Path`（全文 0 处，见 `:391` 起一律写 `std::path::Path`），沿用同一风格可以少动一行 import。

- [ ] **Step 2: 跑测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent title_follows_the_running_flag`
Expected: PASS。

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent title_`
Expected: 三条集成测试 + 标题单测全绿。

若 `title_follows_the_running_flag` 收到的是空 vec，检查 `run_loop` 里 `title.sync(...)` 是否真的在那帧执行（`notifications.take()` 之后、`events.poll()` 之前）。

- [ ] **Step 3: 抽出清理段，让「clear 在最后一步之后」可测**

在 `app.rs` 的 `run_tui` **之前**加：

```rust
/// 终端清理段。
///
/// 抽出来是为了让「标题还原发生在最后一步之后」可被测试断言。`step` 是
/// 可注入的执行器：生产传入真实 crossterm 调用，测试传入记录器。
///
/// 标题还原刻意**不**参与 `cleanup_error`：`TitleHandle::clear` 返回 `()`，
/// 签名层面就进不来。否则上百种不支持 OSC 2 的终端会在每次正常退出时
/// 返回非零，被 `set -e` / CI 脚本当成崩溃。
fn run_cleanup_with<F>(
    mut step: F,
    title: &mut crate::tui::title::TitleHandle,
) -> Option<io::Error>
where
    F: FnMut(&str) -> io::Result<()>,
{
    let mut cleanup_error: Option<io::Error> = None;
    for name in [
        "DisableBracketedPaste",
        "DisableFocusChange",
        "disable_raw_mode",
        "LeaveAlternateScreen",
    ] {
        if let Err(error) = step(name) {
            cleanup_error.get_or_insert(error);
        }
    }
    // 必须在 `LeaveAlternateScreen` 之后：alternate screen 关闭时多数终端
    // 会重置标题，先写空串等于白写。
    title.clear();
    cleanup_error
}
```

把 `run_tui` 的清理段（`:197-210`）整块换成：

```rust
    // Try every cleanup step so a failed write cannot leave the terminal in another mode.
    let cleanup_error = run_cleanup_with(
        |name| match name {
            "DisableBracketedPaste" => execute!(terminal.backend_mut(), DisableBracketedPaste),
            "DisableFocusChange" => execute!(terminal.backend_mut(), DisableFocusChange),
            "disable_raw_mode" => disable_raw_mode(),
            _ => execute!(terminal.backend_mut(), LeaveAlternateScreen),
        },
        &mut title,
    );
```

- [ ] **Step 4: 写两条护栏测试**

在 `app.rs` 的 `mod tests` 里追加：

```rust
    /// 把清理步骤和标题写入放进**同一条**时间线。
    ///
    /// 为什么不用两个独立序列比较末尾：那样只能证明「标题还原是标题序列的
    /// 最后一项」+「LeaveAlternateScreen 是步骤序列的最后一项」，证明不了
    /// 两者的**相对顺序**。共享一条 log 才能断言 clear 严格落在最后一步之后。
    #[derive(Debug, Clone, Default)]
    struct TimelineSink {
        log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        fail_next: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl TimelineSink {
        fn entries(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        /// 只让**下一次**标题写入返回 `Err`（只生效一次）。
        fn fail_next(&self) {
            self.fail_next
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl crate::tui::title::TitleSink for TimelineSink {
        fn write_title(&mut self, text: &str) -> std::io::Result<()> {
            self.log.lock().unwrap().push(format!("title:{text:?}"));
            if self
                .fail_next
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Err(std::io::Error::other("injected"));
            }
            Ok(())
        }
    }

    #[test]
    fn title_restore_failure_does_not_change_exit_code() {
        let sink = TimelineSink::default();
        let log = std::sync::Arc::clone(&sink.log);
        let probe = sink.clone();
        let mut title = crate::tui::title::TitleHandle::with_sink(
            true,
            false,
            std::path::Path::new("/work/p"),
            Box::new(sink),
        );

        // 先让一次写入**成功**：否则 writer 立刻进入 disabled，`clear` 短路，
        // 失败根本落不到 clear 上，断言会恒真。
        title.sync(true);
        assert_eq!(probe.entries(), vec![r#"title:"● p""#.to_string()]);

        // 只注入一次失败 —— 正好是接下来的 `clear`。
        probe.fail_next();

        let cleanup_error = run_cleanup_with(
            |name| {
                log.lock().unwrap().push(format!("step:{name}"));
                Ok(())
            },
            &mut title,
        );

        assert!(
            cleanup_error.is_none(),
            "title restore must never contribute a cleanup error"
        );

        let entries = probe.entries();
        assert_eq!(entries.len(), 6, "1 sync + 4 cleanup steps + 1 clear");
        assert_eq!(entries[0], r#"title:"● p""#);
        assert_eq!(entries[1], "step:DisableBracketedPaste");
        assert_eq!(
            entries[5], r#"title:"""#,
            "clear must still be attempted even though it fails"
        );
    }

    #[test]
    fn clear_happens_after_leave_alternate_screen() {
        let sink = TimelineSink::default();
        let log = std::sync::Arc::clone(&sink.log);
        let probe = sink.clone();
        let mut title = crate::tui::title::TitleHandle::with_sink(
            true,
            false,
            std::path::Path::new("/work/p"),
            Box::new(sink),
        );
        title.sync(true);

        let _ = run_cleanup_with(
            |name| {
                log.lock().unwrap().push(format!("step:{name}"));
                Ok(())
            },
            &mut title,
        );

        assert_eq!(
            probe.entries(),
            vec![
                r#"title:"● p""#.to_string(),
                "step:DisableBracketedPaste".to_string(),
                "step:DisableFocusChange".to_string(),
                "step:disable_raw_mode".to_string(),
                "step:LeaveAlternateScreen".to_string(),
                r#"title:"""#.to_string(),
            ],
            "the empty title must be written strictly after the last cleanup step"
        );
    }
```

- [ ] **Step 5: 跑测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent clear_happens_after_leave_alternate_screen`
Expected: PASS。

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent title_restore_failure`
Expected: PASS。

- [ ] **Step 6: 变异验证（不可跳过）**

临时把 `run_cleanup_with` 里的 `title.clear();` 挪到 `for` 循环**之前**，跑：

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent clear_happens_after_leave_alternate_screen`
Expected: **FAIL**（整条时间线变成 `[title:"● p", title:"", step:DisableBracketedPaste, ...]`，`clear` 跑到了步骤之前）。

改回原样，再跑一次确认 PASS。不验证变异就等于没写这条测试。

- [ ] **Step 7: 跑整个 bin 的测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent`
Expected: 全绿。

- [ ] **Step 8: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/app.rs
git commit -m "test(tui): 标题集成测试与退出码护栏"
```

---

## Task 9: 桌面标签、项目文档、合并前 CI

**Files:**
- Modify: `desktop/src/components/SettingsRuntimeTab.tsx`
- Modify: `desktop/src/components/SettingsRuntimeTab.test.tsx`
- Modify: `docs/project-management/tui.md`（不存在则创建）
- Modify: `docs/project-management/README.md`

**Interfaces:**
- Consumes: Task 4 的 `FIELDS` 条目（后端下发）
- Produces: 无

- [ ] **Step 1: 桌面端标签**

在 `desktop/src/components/SettingsRuntimeTab.tsx` 的 `VALUE_LABELS` 里、`"tui.notifications"` 那一行之后加：

```ts
  "tui.terminal_title": { true: "开", false: "关" },
```

不加的话下拉会显示原始值 `true` / `false`。分组表**不用动**：`GROUP_ORDER` 已含 `"tui"`，新键靠 `group: "tui"` 自动落进「终端」分组。

- [ ] **Step 2: 桌面端测试**

在 `desktop/src/components/SettingsRuntimeTab.test.tsx` 的 `translates enum values but keeps unknown ones verbatim` 里、`expect(labelFor("tui.notification_method", "bel")).toBe("响铃");` 之后加：

```ts
    expect(labelFor("tui.terminal_title", "false")).toBe("关");
    expect(labelFor("tui.terminal_title", "true")).toBe("开");
```

- [ ] **Step 3: 跑桌面测试**

```bash
cd desktop && npx vitest run src/components/SettingsRuntimeTab.test.tsx
```
Expected: 全绿。

- [ ] **Step 4: 更新项目进度文档**

按项目规范，功能完成必须同步 `docs/project-management/`。在 `docs/project-management/tui.md`（若无则创建，并在 `README.md` 的模块索引表登记）里加一条：

```markdown
- [x] 终端标签标题（OSC 2）：空闲 `myproject`、运行中 `● myproject`，按显示列宽
  截断到 24 列，tmux / zellij 内不写任何字节，退出还原为空串，写失败本会话自禁
  且不影响退出码。配置键 `tui.terminal_title`（默认 `true`），`/config` 显示
  `on` / `off` / `disabled (multiplexer)`。
  验证：`cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::title`、
  `cargo test -p yi-agent-runtime terminal_title`。
  见 [设计](../superpowers/specs/2026-11-01-terminal-title-design.md)、
  [计划](../superpowers/plans/2026-11-01-terminal-title.md)。
```

同步更新 `docs/project-management/README.md` 模块索引表的「完成 / 总计」计数。

- [ ] **Step 5: 合并前全量 CI**

Run: `cd yi-agent-rs && just ci`
Expected: 全绿。

这一步**必须**跑，不能只跑前面几条 `cargo test`：`lint --all-targets` 会检查测试代码（`TimelineSink`、`TitleScriptEvents` 等同样受 clippy 约束），`bindings-check` 会检查序列化契约（`TuiSettings` 加了字段），`secret-check` 等其他门禁也只在 `just ci` 里。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add ../desktop/src/components/SettingsRuntimeTab.tsx \
  ../desktop/src/components/SettingsRuntimeTab.test.tsx \
  ../docs/project-management/tui.md \
  ../docs/project-management/README.md
git commit -m "feat: 桌面设置页终端标题标签与项目进度"
```

---

## 附：任务依赖

```
Task 1 (纯函数) ─┐
Task 2 (编码)   ─┴─→ Task 3 (策略+聚合器) ──┐
                                            ├─→ Task 7 (接线) ─→ Task 8 (集成/护栏) ─→ Task 9 (桌面/文档/CI)
Task 4 (配置) ──→ Task 5 (跨 crate 补齐) ───┤
                    └─→ Task 6 (/config) ───┘
```

- Task 1 与 Task 2 互不依赖，可并行或对调。
- Task 4 → Task 5 必须串行（5 是 4 的编译后果）。
- Task 6 依赖 Task 3（要 `TitleWriter::new`）与 Task 4（要 `config.terminal_title`）。
- Task 7 依赖 Task 3 与 Task 6（`TuiConfigSnapshot` 要有 `terminal_title`）。
- Task 8 依赖 Task 7。Task 9 依赖 Task 4（`FIELDS`）。

**关键路径**：Task 1 → Task 3 → Task 7 → Task 8 → Task 9。
