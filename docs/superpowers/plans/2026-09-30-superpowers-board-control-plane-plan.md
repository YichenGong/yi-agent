# Superpowers 看板 — Plan 3b：控制面（TUI、Desktop、设置界面）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给两端加上 Superpowers 看板的控制面：TUI 的 `/kanban` 与开关项，desktop 的看板视图与设置入口，以及全局/项目两层开关的读写。

**Architecture:** 新增一个宿主侧 crate `yi-agent-board-ui`（`yi-agent-rs` 的成员），把「开关读写」与「看板视图数据装配」做成**可单测的纯逻辑 + 薄的 I/O 边界**。TUI 与 desktop 各自只做渲染与事件转发。之所以复用既有前端骨架：TUI 已有 `/agents` 的 `ListPopup`/`DetailPopup` 范式，desktop 已有 `ThreadSidebar` + 状态角标。

**Tech Stack:** Rust 2024（TUI 侧，`ratatui`）、TypeScript/React（desktop 侧）、`serde_json`（与插件共享 `preferences.json` 约定）。

## Global Constraints

- 开关键名逐字为 **`superpowers_board`**（布尔），写在 `preferences.json` 顶层，与既有键（如 `subagent_runtime`）并存，读写不得破坏其他键。
- 全局层：`~/.yi-agent/preferences.json`；项目层：`<workdir>/.yi-agent/preferences.json`。
- 解析顺序：**项目层覆盖全局层**；两层都缺失 → **默认关闭**。
- 文件缺失/不可读/损坏 → 回退默认并记 warning，**绝不 panic**，也**绝不**因此停掉前端。
- 写入必须 **temp 文件 + rename 原子替换**（沿用 `tui/runtime_prefs.rs` 的既有写法）。
- **关闭时**：UI 入口隐藏、`/kanban` 拒绝执行并提示开关位置；**不杀**任何已在 daemon 中运行的会话。
- TUI 第一版**不做**会话时间线跳转（spec §10 的降级决定）。
- 提交信息用 conventional commits，**不写** `Co-Authored-By`。
- 每个任务结束：Rust 侧 `cd yi-agent-rs && cargo fmt --all && cargo test -p yi-agent-board-ui`；desktop 侧 `cd desktop && npx tsc --noEmit && npm test`。
- **desktop 组件测试约定**（既有约定，必须遵守）：文件首行写 `/** @vitest-environment jsdom */`，
  且每个测试文件必须 `afterEach(() => cleanup())`，否则多次 `render` 会累积 DOM，
  导致 `Found multiple elements`。

> **依赖：** Plan 2（IPC 能力）与 Plan 3a（插件进程与状态来源）已合并。本计划的 UI 只读 Plan 3a 的看板状态。

---
## 文件结构

| 文件 | 职责 |
|------|------|
| `yi-agent-rs/crates/yi-agent-board-ui/Cargo.toml` | 新宿主侧 crate |
| `.../src/lib.rs` | 导出 |
| `.../src/switch.rs` | 两层开关：读取（项目→全局）、解析、原子写入 |
| `.../src/view.rs` | 看板视图数据：卡片行、开关状态、格式化 |
| `yi-agent-rs/crates/yi-agent/src/tui/slash.rs` | 新增 `SlashCommand::Kanban` |
| `yi-agent-rs/crates/yi-agent/src/tui/board.rs` | `/kanban` popup 的渲染与交互 |
| `desktop/src/components/BoardView.tsx` | desktop 看板视图 |
| `desktop/src/components/SettingsPanel.tsx` | desktop 设置入口（含开关） |
| `desktop/src/lib/boardSwitch.ts` | 前端开关读写与格式化 |

---

### Task 1: 宿主侧 crate 与两层开关读写

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-board-ui/Cargo.toml`
- Create: `yi-agent-rs/crates/yi-agent-board-ui/src/lib.rs`
- Create: `yi-agent-rs/crates/yi-agent-board-ui/src/switch.rs`
- Modify: `yi-agent-rs/Cargo.toml`

**Interfaces:**
- Consumes: 无
- Produces:
  - `yi_agent_board_ui::switch::BoardSwitch { Enabled, Disabled }` + `is_enabled()`
  - `yi_agent_board_ui::switch::SwitchSource { Project, Global, Default }`
  - `yi_agent_board_ui::switch::ResolvedSwitch { value: BoardSwitch, source: SwitchSource }`
  - `yi_agent_board_ui::switch::global_path() -> Option<PathBuf>`
  - `yi_agent_board_ui::switch::project_path(workdir: &Path) -> PathBuf`
  - `yi_agent_board_ui::switch::read_layer(path: &Path) -> Option<BoardSwitch>`
  - `yi_agent_board_ui::switch::resolve(global: Option<BoardSwitch>, project: Option<BoardSwitch>) -> ResolvedSwitch`
  - `yi_agent_board_ui::switch::write_layer(path: &Path, value: BoardSwitch) -> std::io::Result<()>`

- [ ] **Step 1: 写失败的测试**

`yi-agent-rs/crates/yi-agent-board-ui/src/switch.rs`：

```rust
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_project_layer_wins_over_the_global_layer() {
        let resolved = resolve(Some(BoardSwitch::Enabled), Some(BoardSwitch::Disabled));
        assert_eq!(resolved.value, BoardSwitch::Disabled);
        assert_eq!(resolved.source, SwitchSource::Project);

        let resolved = resolve(Some(BoardSwitch::Disabled), Some(BoardSwitch::Enabled));
        assert_eq!(resolved.value, BoardSwitch::Enabled);
        assert_eq!(resolved.source, SwitchSource::Project);
    }

    #[test]
    fn a_missing_project_layer_inherits_the_global_layer() {
        let resolved = resolve(Some(BoardSwitch::Enabled), None);
        assert_eq!(resolved.value, BoardSwitch::Enabled);
        assert_eq!(resolved.source, SwitchSource::Global);
    }

    #[test]
    fn both_layers_missing_defaults_to_disabled() {
        let resolved = resolve(None, None);
        assert_eq!(resolved.value, BoardSwitch::Disabled);
        assert_eq!(resolved.source, SwitchSource::Default);
    }

    #[test]
    fn a_written_layer_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        write_layer(&path, BoardSwitch::Enabled).unwrap();
        assert_eq!(read_layer(&path), Some(BoardSwitch::Enabled));
        write_layer(&path, BoardSwitch::Disabled).unwrap();
        assert_eq!(read_layer(&path), Some(BoardSwitch::Disabled));
    }

    #[test]
    fn writing_preserves_unrelated_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, r#"{"subagent_runtime":"always"}"#).unwrap();

        write_layer(&path, BoardSwitch::Enabled).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["subagent_runtime"], "always");
        assert_eq!(value["superpowers_board"], true);
    }

    #[test]
    fn a_broken_file_reads_as_none_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(read_layer(&path), None);
    }

    #[test]
    fn a_missing_file_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_layer(&dir.path().join("absent.json")), None);
    }

    #[test]
    fn writing_is_atomic_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        write_layer(&path, BoardSwitch::Enabled).unwrap();
        assert!(!dir.path().join("preferences.json.tmp").exists());
    }

    #[test]
    fn the_project_path_lives_under_the_workdir() {
        assert_eq!(
            project_path(Path::new("/project")),
            PathBuf::from("/project/.yi-agent/preferences.json")
        );
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-board-ui`
Expected: 编译失败（crate 尚不存在）。

- [ ] **Step 3: 实现最小代码**

`yi-agent-rs/crates/yi-agent-board-ui/Cargo.toml`：

```toml
[package]
name = "yi-agent-board-ui"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
serde_json.workspace = true
tracing.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

在 `yi-agent-rs/Cargo.toml` 的 `members` 加入 `"crates/yi-agent-board-ui"`；
若 `[workspace.dependencies]` 尚无 `tempfile`，加入 `tempfile = "3"`。

`src/lib.rs`：

```rust
//! Superpowers 看板在宿主前端里的共享逻辑。
//!
//! 开关读写与视图数据装配放在这里，使 TUI 与 desktop 共用同一份语义
//! （尤其是两层解析顺序与原子写入），而不是各自重写一遍。

pub mod switch;
```

`src/switch.rs` 在测试模块之前：

```rust
use std::path::{Path, PathBuf};

use serde_json::Value;

/// 生效的开关值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardSwitch {
    Enabled,
    Disabled,
}

impl BoardSwitch {
    pub fn is_enabled(self) -> bool {
        matches!(self, BoardSwitch::Enabled)
    }
}

/// 生效值来自哪一层，供 UI 显示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchSource {
    Project,
    Global,
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedSwitch {
    pub value: BoardSwitch,
    pub source: SwitchSource,
}

/// 全局层路径：`~/.yi-agent/preferences.json`。
pub fn global_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".yi-agent")
            .join("preferences.json")
    })
}

/// 项目层路径：`<workdir>/.yi-agent/preferences.json`。
pub fn project_path(workdir: &Path) -> PathBuf {
    workdir.join(".yi-agent").join("preferences.json")
}

/// 读一层。文件缺失、不可读、损坏或缺键一律返回 `None`（视作该层未设置）。
pub fn read_layer(path: &Path) -> Option<BoardSwitch> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(error = %error, path = %path.display(), "invalid preferences; treating the layer as unset");
            return None;
        }
    };
    match value.get("superpowers_board").and_then(Value::as_bool) {
        Some(true) => Some(BoardSwitch::Enabled),
        Some(false) => Some(BoardSwitch::Disabled),
        None => None,
    }
}

/// 两层解析：项目层覆盖全局层；两层都缺 → 默认关闭。
pub fn resolve(global: Option<BoardSwitch>, project: Option<BoardSwitch>) -> ResolvedSwitch {
    if let Some(value) = project {
        return ResolvedSwitch {
            value,
            source: SwitchSource::Project,
        };
    }
    if let Some(value) = global {
        return ResolvedSwitch {
            value,
            source: SwitchSource::Global,
        };
    }
    ResolvedSwitch {
        value: BoardSwitch::Disabled,
        source: SwitchSource::Default,
    }
}

/// 写一层：读-改-写整个 JSON 对象，保留其他键；临时文件 + rename 原子替换。
pub fn write_layer(path: &Path, value: BoardSwitch) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut object = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    object.insert(
        "superpowers_board".to_string(),
        Value::Bool(value.is_enabled()),
    );
    let body = serde_json::to_string_pretty(&Value::Object(object))
        .map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}
```

> 若 `write_layer` 的临时文件名与既有 `runtime_prefs.rs` 的 `.tmp` 约定不同，以本文件为准（两者作用于同一目录但同一次写入只有一个进程在跑；`preferences.json.tmp` 的清理由 rename 保证）。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-board-ui switch::`
Expected: PASS（9 个测试）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/Cargo.toml yi-agent-rs/crates/yi-agent-board-ui
git commit -m "feat(board-ui): add two-layer board switch storage shared by both frontends"
```

---

### Task 2: 看板视图数据装配

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-board-ui/src/view.rs`
- Modify: `yi-agent-rs/crates/yi-agent-board-ui/src/lib.rs`

**Interfaces:**
- Consumes: 无（纯数据）
- Produces:
  - `yi_agent_board_ui::view::CardRow { id: String, state: String, progress: Option<String>, detail: String }`
  - `yi_agent_board_ui::view::BoardView { switch_on: bool, switch_source: &'static str, cards: Vec<CardRow> }`
  - `yi_agent_board_ui::view::BoardView::render_lines(&self) -> Vec<String>`
  - `yi_agent_board_ui::view::BoardView::header(&self) -> String`

- [ ] **Step 1: 写失败的测试**

`yi-agent-rs/crates/yi-agent-board-ui/src/view.rs`：

```rust
use std::fmt::Write as _;

#[cfg(test)]
mod tests {
    use super::*;

    fn view(on: bool, cards: Vec<CardRow>) -> BoardView {
        BoardView {
            switch_on: on,
            switch_source: "project",
            cards,
        }
    }

    #[test]
    fn the_header_names_the_feature_and_shows_the_switch() {
        let header = view(true, vec![]).header();
        assert!(header.contains("Superpowers 看板"), "{header}");
        assert!(header.contains("on"), "{header}");
        assert!(header.contains("project"), "{header}");
    }

    #[test]
    fn a_disabled_board_says_so_instead_of_pretending_to_be_empty() {
        let lines = view(false, vec![]).render_lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("disabled"), "{lines:?}");
    }

    #[test]
    fn cards_render_with_id_state_progress_and_detail() {
        let lines = view(
            true,
            vec![CardRow {
                id: "card-1".into(),
                state: "running".into(),
                progress: Some("3/7 tasks".into()),
                detail: "kanban/card-1-foo".into(),
            }],
        )
        .render_lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("card-1"));
        assert!(lines[0].contains("running"));
        assert!(lines[0].contains("3/7 tasks"));
        assert!(lines[0].contains("kanban/card-1-foo"));
    }

    #[test]
    fn a_card_without_progress_omits_the_progress_slot() {
        let lines = view(
            true,
            vec![CardRow {
                id: "card-2".into(),
                state: "queued".into(),
                progress: None,
                detail: String::new(),
            }],
        )
        .render_lines();
        assert!(lines[0].contains("card-2"));
        assert!(!lines[0].contains("("), "no empty parens: {lines:?}");
    }

    #[test]
    fn an_enabled_board_with_no_cards_says_it_is_empty() {
        let lines = view(true, vec![]).render_lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("empty"), "{lines:?}");
    }

    #[test]
    fn cards_keep_their_order() {
        let rows = vec![
            CardRow {
                id: "first".into(),
                state: "running".into(),
                progress: None,
                detail: String::new(),
            },
            CardRow {
                id: "second".into(),
                state: "queued".into(),
                progress: None,
                detail: String::new(),
            },
        ];
        let lines = view(true, rows).render_lines();
        assert!(lines[0].contains("first"));
        assert!(lines[1].contains("second"));
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-board-ui view::`
Expected: 编译失败，`BoardView` 未定义。

- [ ] **Step 3: 实现最小代码**

在 `view.rs` 测试模块之前：

```rust
/// One card as the frontends display it. Deliberately flat strings: the UI
/// layers differ (ratatui vs React) and neither should re-derive semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardRow {
    pub id: String,
    pub state: String,
    /// e.g. `Some("3/7 tasks")`; `None` when progress is unknown.
    pub progress: Option<String>,
    /// Evidence for a finished card: branch name, commits, verification result.
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardView {
    pub switch_on: bool,
    /// `project` / `global` / `default`, for the user to see where the value came from.
    pub switch_source: &'static str,
    pub cards: Vec<CardRow>,
}

impl BoardView {
    pub fn header(&self) -> String {
        let state = if self.switch_on { "on" } else { "off" };
        format!("Superpowers 看板: {state} ({})", self.switch_source)
    }

    pub fn render_lines(&self) -> Vec<String> {
        if !self.switch_on {
            return vec![
                "Superpowers 看板 is disabled. Enable it in settings, or set \
                 \"superpowers_board\": true in preferences.json."
                    .to_string(),
            ];
        }
        if self.cards.is_empty() {
            return vec!["Superpowers 看板 is empty.".to_string()];
        }
        self.cards
            .iter()
            .map(|card| {
                let mut line = format!("{}  {}", card.id, card.state);
                if let Some(progress) = &card.progress {
                    let _ = write!(line, "  ({progress})");
                }
                if !card.detail.is_empty() {
                    let _ = write!(line, "  {}", card.detail);
                }
                line
            })
            .collect()
    }
}
```

`lib.rs` 追加：

```rust
pub mod view;
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-board-ui view::`
Expected: PASS（6 个测试）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-board-ui
git commit -m "feat(board-ui): add the flat board view data both frontends render"
```

---

### Task 3: TUI `/kanban` 命令与开关项

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs`
- Create: `yi-agent-rs/crates/yi-agent/src/tui/board.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/mod.rs`
- Modify: `yi-agent-rs/crates/yi-agent/Cargo.toml`

**Interfaces:**
- Consumes: `yi_agent_board_ui::{switch, view}`
- Produces:
  - `SlashCommand::Kanban`（`name() == "kanban"`，`description()` 含 "Superpowers 看板"）
  - `tui::board::handle_kanban(workdir: &Path, args: &str) -> KanbanOutcome`
  - `tui::board::KanbanOutcome { lines: Vec<String>, toggled_to: Option<bool> }`

- [ ] **Step 1: 写失败的测试**

`yi-agent-rs/crates/yi-agent/src/tui/board.rs`：

```rust
use std::path::Path;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_no_arguments_it_shows_the_board() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = handle_kanban(dir.path(), "");
        assert_eq!(outcome.toggled_to, None);
        assert!(!outcome.lines.is_empty());
        assert!(outcome.lines[0].contains("Superpowers 看板"), "{:?}", outcome.lines);
    }

    #[test]
    fn on_enables_the_project_layer() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = handle_kanban(dir.path(), "on");
        assert_eq!(outcome.toggled_to, Some(true));
        assert_eq!(
            yi_agent_board_ui::switch::read_layer(
                &yi_agent_board_ui::switch::project_path(dir.path())
            ),
            Some(yi_agent_board_ui::switch::BoardSwitch::Enabled)
        );
    }

    #[test]
    fn off_disables_the_project_layer() {
        let dir = tempfile::tempdir().unwrap();
        handle_kanban(dir.path(), "on");
        let outcome = handle_kanban(dir.path(), "off");
        assert_eq!(outcome.toggled_to, Some(false));
        assert_eq!(
            yi_agent_board_ui::switch::read_layer(
                &yi_agent_board_ui::switch::project_path(dir.path())
            ),
            Some(yi_agent_board_ui::switch::BoardSwitch::Disabled)
        );
    }

    #[test]
    fn an_unknown_argument_is_reported_instead_of_silently_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = handle_kanban(dir.path(), "sideways");
        assert_eq!(outcome.toggled_to, None);
        assert!(
            outcome.lines[0].contains("usage"),
            "expected a usage line, got {:?}",
            outcome.lines
        );
    }

    #[test]
    fn a_disabled_board_refuses_to_run_and_says_where_the_switch_is() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = handle_kanban(dir.path(), "run");
        assert_eq!(outcome.toggled_to, None);
        assert!(
            outcome.lines[0].contains("disabled"),
            "expected a disabled notice, got {:?}",
            outcome.lines
        );
    }
}
```

在 `slash.rs` 的测试模块追加：

```rust
#[test]
fn kanban_is_a_known_slash_command_with_a_description() {
    let command = SlashCommand::Kanban;
    assert_eq!(command.name(), "kanban");
    assert!(
        command.description().contains("Superpowers 看板"),
        "{}",
        command.description()
    );
    assert!(SlashCommand::all().contains(&SlashCommand::Kanban));
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent kanban`
Expected: 编译失败，`SlashCommand::Kanban` 与 `tui::board` 未定义。

- [ ] **Step 3: 实现最小代码**

`yi-agent-rs/crates/yi-agent/src/tui/board.rs` 在测试模块之前：

```rust
use std::path::Path;

use yi_agent_board_ui::switch::{self, BoardSwitch};

/// What `/kanban` produced: lines to show, and whether it changed the switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KanbanOutcome {
    pub lines: Vec<String>,
    /// `Some(true)` / `Some(false)` when this invocation wrote the project layer.
    pub toggled_to: Option<bool>,
}

/// Handles `/kanban [on|off|run]`.
///
/// The board is *disabled by default*, so an accidental invocation cannot start
/// a queue. `run` is intentionally refused while disabled: the switch is the
/// user's explicit consent to let the board act.
pub fn handle_kanban(workdir: &Path, args: &str) -> KanbanOutcome {
    let argument = args.trim();
    let project = switch::read_layer(&switch::project_path(workdir));
    let global = switch::global_path().and_then(|path| switch::read_layer(&path));
    let resolved = switch::resolve(global, project);
    let source = match resolved.source {
        switch::SwitchSource::Project => "project",
        switch::SwitchSource::Global => "global",
        switch::SwitchSource::Default => "default",
    };

    match argument {
        "" => {
            let view = yi_agent_board_ui::view::BoardView {
                switch_on: resolved.value.is_enabled(),
                switch_source: source,
                // The card list is supplied by the plugin process (Plan 3a);
                // this first version renders the switch state and the empty
                // state without inventing rows.
                cards: Vec::new(),
            };
            let mut lines = vec![view.header()];
            lines.extend(view.render_lines());
            KanbanOutcome {
                lines,
                toggled_to: None,
            }
        }
        "on" | "off" => {
            let value = if argument == "on" {
                BoardSwitch::Enabled
            } else {
                BoardSwitch::Disabled
            };
            let path = switch::project_path(workdir);
            match switch::write_layer(&path, value) {
                Ok(()) => KanbanOutcome {
                    lines: vec![format!(
                        "Superpowers 看板 {} (project: {})",
                        if value.is_enabled() { "enabled" } else { "disabled" },
                        path.display()
                    )],
                    toggled_to: Some(value.is_enabled()),
                },
                Err(error) => KanbanOutcome {
                    lines: vec![format!("could not write {}: {error}", path.display())],
                    toggled_to: None,
                },
            }
        }
        "run" => {
            if resolved.value.is_enabled() {
                KanbanOutcome {
                    lines: vec![
                        "Superpowers 看板 is enabled; the plugin process advances the queue."
                            .to_string(),
                    ],
                    toggled_to: None,
                }
            } else {
                KanbanOutcome {
                    lines: vec![format!(
                        "Superpowers 看板 is disabled (source: {source}). \
                         Enable it with /kanban on, or set \"superpowers_board\": true."
                    )],
                    toggled_to: None,
                }
            }
        }
        _ => KanbanOutcome {
            lines: vec!["usage: /kanban [on|off|run]".to_string()],
            toggled_to: None,
        },
    }
}
```

在 `slash.rs` 的 `SlashCommand` 枚举加入 `Kanban,`，并在三处补齐：

```rust
// name()
SlashCommand::Kanban => "kanban",

// description()
SlashCommand::Kanban => "Superpowers 看板：查看状态与开关",

// all()
SlashCommand::Kanban,
```

在 `tui/mod.rs` 加入 `pub mod board;`。

在 `yi-agent-rs/crates/yi-agent/Cargo.toml` 的 `[dependencies]` 加入：

```toml
yi-agent-board-ui = { workspace = true }
```

并在 `yi-agent-rs/Cargo.toml` 的 `[workspace.dependencies]` 加入：

```toml
yi-agent-board-ui = { path = "crates/yi-agent-board-ui" }
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent kanban`
Expected: PASS（6 个测试：5 个 board + 1 个 slash）。

- [ ] **Step 5: 验证 slash completion 目录未遗漏**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent slash`
Expected: PASS（既有 slash 目录测试会把新命令纳入，若断言枚举完整性会自动覆盖）。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/Cargo.toml yi-agent-rs/crates/yi-agent/Cargo.toml yi-agent-rs/crates/yi-agent/src/tui
git commit -m "feat(tui): add the /kanban command and the board switch"
```

---

### Task 4: Desktop 看板视图与设置入口

**Files:**
- Create: `desktop/src/lib/boardSwitch.ts`
- Create: `desktop/src/lib/boardSwitch.test.ts`
- Create: `desktop/src/components/BoardView.tsx`
- Create: `desktop/src/components/BoardView.test.tsx`
- Create: `desktop/src/components/SettingsPanel.tsx`
- Create: `desktop/src/components/SettingsPanel.test.tsx`

**Interfaces:**
- Consumes: Plan 3a 的看板状态（第一版由父组件传入；本任务只做纯展示）
- Produces:
  - `formatSwitch(switchOn: boolean, source: "project" | "global" | "default"): string`
  - `resolveSwitch(global: boolean | null, project: boolean | null): { value: boolean; source: "project" | "global" | "default" }`
  - `<BoardView switchOn source cards />`
  - `<SettingsPanel switchOn source onToggle />`

- [ ] **Step 1: 写失败的测试**

`desktop/src/lib/boardSwitch.test.ts`：

```typescript
import { describe, expect, it } from "vitest";
import { formatSwitch, resolveSwitch } from "./boardSwitch";

describe("resolveSwitch", () => {
  it("lets the project layer win over the global layer", () => {
    expect(resolveSwitch(true, false)).toEqual({ value: false, source: "project" });
    expect(resolveSwitch(false, true)).toEqual({ value: true, source: "project" });
  });

  it("inherits the global layer when the project layer is unset", () => {
    expect(resolveSwitch(true, null)).toEqual({ value: true, source: "global" });
  });

  it("defaults to disabled when both layers are unset", () => {
    expect(resolveSwitch(null, null)).toEqual({ value: false, source: "default" });
  });
});

describe("formatSwitch", () => {
  it("names the feature and shows the source", () => {
    const text = formatSwitch(true, "project");
    expect(text).toContain("Superpowers 看板");
    expect(text).toContain("on");
    expect(text).toContain("project");
  });

  it("says off when disabled", () => {
    expect(formatSwitch(false, "default")).toContain("off");
  });
});
```

`desktop/src/components/BoardView.test.tsx`：

```tsx
/** @vitest-environment jsdom */
import { describe, expect, it, afterEach } from "vitest";
import { render, cleanup, screen } from "@testing-library/react";
import { BoardView } from "./BoardView";

afterEach(() => {
  cleanup();
});

describe("BoardView", () => {
  it("renders a card with its state, progress and detail", () => {
    render(
      <BoardView
        switchOn
        source="project"
        cards={[
          {
            id: "card-1",
            state: "running",
            progress: "3/7 tasks",
            detail: "kanban/card-1-foo",
          },
        ]}
      />,
    );
    // `card-1` also appears inside the detail string, so match element text
    // exactly rather than as a substring across the row.
    expect(screen.getByText("card-1")).toBeTruthy();
    expect(screen.getByText(/3\/7 tasks/)).toBeTruthy();
    expect(screen.getByText("kanban/card-1-foo")).toBeTruthy();
  });

  it("explains itself instead of looking empty when disabled", () => {
    render(<BoardView switchOn={false} source="default" cards={[]} />);
    expect(screen.getByText(/disabled/i)).toBeTruthy();
  });

  it("says the board is empty when enabled with no cards", () => {
    render(<BoardView switchOn source="project" cards={[]} />);
    expect(screen.getByText(/empty/i)).toBeTruthy();
  });
});
```

`desktop/src/components/SettingsPanel.test.tsx`：

```tsx
/** @vitest-environment jsdom */
import { describe, expect, it, vi, afterEach } from "vitest";
import { fireEvent, render, cleanup, screen } from "@testing-library/react";
import { SettingsPanel } from "./SettingsPanel";

afterEach(() => {
  cleanup();
});

describe("SettingsPanel", () => {
  it("shows the switch state and its source", () => {
    render(<SettingsPanel switchOn source="global" onToggle={() => {}} />);
    expect(screen.getByText(/Superpowers 看板/)).toBeTruthy();
    expect(screen.getByText(/global/)).toBeTruthy();
  });

  it("reports the requested value when toggled", () => {
    const onToggle = vi.fn();
    render(<SettingsPanel switchOn={false} source="default" onToggle={onToggle} />);
    fireEvent.click(screen.getByRole("checkbox"));
    expect(onToggle).toHaveBeenCalledWith(true);
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd desktop && npx tsc --noEmit && npm test`
Expected: 失败，模块不存在。

- [ ] **Step 3: 实现最小代码**

`desktop/src/lib/boardSwitch.ts`：

```typescript
/** Which layer supplied the effective switch value. */
export type SwitchSource = "project" | "global" | "default";

export interface ResolvedSwitch {
  value: boolean;
  source: SwitchSource;
}

/**
 * Two-layer resolution. The project layer wins; both unset means disabled,
 * matching the Rust side so the UI never disagrees with the plugin process.
 */
export function resolveSwitch(
  global: boolean | null,
  project: boolean | null,
): ResolvedSwitch {
  if (project !== null) return { value: project, source: "project" };
  if (global !== null) return { value: global, source: "global" };
  return { value: false, source: "default" };
}

export function formatSwitch(switchOn: boolean, source: SwitchSource): string {
  return `Superpowers 看板: ${switchOn ? "on" : "off"} (${source})`;
}
```

`desktop/src/components/BoardView.tsx`：

```tsx
import type { SwitchSource } from "../lib/boardSwitch";

export interface BoardCard {
  id: string;
  state: string;
  progress: string | null;
  detail: string;
}

export function BoardView({
  switchOn,
  source,
  cards,
}: {
  switchOn: boolean;
  source: SwitchSource;
  cards: BoardCard[];
}) {
  if (!switchOn) {
    return (
      <div className="p-4 text-sm text-neutral-400">
        Superpowers 看板 is disabled ({source}). Enable it in settings.
      </div>
    );
  }
  if (cards.length === 0) {
    return <div className="p-4 text-sm text-neutral-400">Superpowers 看板 is empty.</div>;
  }
  return (
    <ul className="divide-y divide-neutral-800">
      {cards.map((card) => (
        <li key={card.id} className="flex items-center gap-3 p-3 text-sm">
          <span className="font-mono text-neutral-300">{card.id}</span>
          <span className="text-neutral-400">{card.state}</span>
          {card.progress ? (
            <span className="text-neutral-500">({card.progress})</span>
          ) : null}
          {card.detail ? (
            <span className="truncate text-neutral-500">{card.detail}</span>
          ) : null}
        </li>
      ))}
    </ul>
  );
}
```

`desktop/src/components/SettingsPanel.tsx`：

```tsx
import type { SwitchSource } from "../lib/boardSwitch";

export function SettingsPanel({
  switchOn,
  source,
  onToggle,
}: {
  switchOn: boolean;
  source: SwitchSource;
  onToggle: (next: boolean) => void;
}) {
  return (
    <section className="p-4">
      <label className="flex items-center gap-3 text-sm">
        <input
          type="checkbox"
          checked={switchOn}
          onChange={(event) => onToggle(event.target.checked)}
        />
        <span>Superpowers 看板</span>
        <span className="text-neutral-500">({source})</span>
      </label>
      <p className="mt-2 text-xs text-neutral-500">
        Off by default. Turning it off stops the plugin from advancing the queue; it
        does not cancel sessions already running in the daemon.
      </p>
    </section>
  );
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd desktop && npx tsc --noEmit && npm test`
Expected: PASS（全部既有测试 + 本任务新增 8 个）。

- [ ] **Step 5: 提交**

```bash
cd desktop && npm test
git add desktop/src
git commit -m "feat(desktop): add the superpowers board view and its settings switch"
```

---

## 完成判据

- `cd yi-agent-rs && cargo test -p yi-agent-board-ui` 全绿（15 个测试：9 switch + 6 view）。
- `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent kanban` 全绿（6 个测试）。
- `cd desktop && npx tsc --noEmit && npm test` 全绿。
- `/kanban` 在开关关闭时**拒绝执行**并指出开关位置（有专门测试）。
- 写入 `preferences.json` 保留其他键，且不留 `.tmp` 文件（有专门测试）。
- 两端对「项目覆盖全局、默认关闭」的解析一致（Rust 与 TS 各有一组同构测试）。
