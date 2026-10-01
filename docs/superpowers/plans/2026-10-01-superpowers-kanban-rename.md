# Superpowers Kanban 改名与迁移 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把看板相关命名统一为 `superpowers-kanban`（用户可见面、落盘键、crate/模块/组件名），并保证已有数据与配置**可读、不丢失**。

**Architecture:** 先做"行为层"迁移（开关键、状态目录、日历文件——新名优先 + 旧名回退读），再做"命名层"重命名（crate、二进制、模块、组件、清单、RPC）。行为层先行，使重命名过程中始终有可测的兼容行为兜底。

**Tech Stack:** Rust（cargo workspace，两个独立 workspace：主程序 `yi-agent-rs/`、插件 `plugins/superpowers-board/`）、TypeScript/React（`desktop/`）、vitest。

## Global Constraints

- 主程序工作区根：`yi-agent-rs/`；插件工作区根：`plugins/superpowers-board/`。
- Rust 工具链：`export PATH="/Users/gongyichen/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`；所有 cargo 命令加 `--offline`。
- **TMPDIR 必须短**：socket 路径上限 103 字节，测 store/IPC 时用 `TMPDIR=/Users/gongyichen/.yi-agent-tmp`。
- desktop 用 `export PATH="/opt/homebrew/bin:$PATH"`；vitest 用 `TMPDIR="$PWD/.tmpverify"`。
- **非破坏性**：只读旧位置、只写新位置；**绝不修改或删除**旧文件/旧键。
- 开关键新名 `superpowers_kanban`，旧名 `superpowers_board`。
- 状态目录新名 `<workdir>/.yi-agent/superpowers-kanban`，旧名 `<workdir>/.yi-agent/board`。
- 日历文件名新 `superpowers-kanban.toml`，旧 `kanban.toml`。
- `board.json` 与 `inbox/` 的文件名**不变**。
- 主程序 crate `yi-agent-board-ui` **不改名**（本计划不动它，Spec 4 会删除它）。

---

### Task 1: 开关键迁移（`superpowers_board` → `superpowers_kanban`）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-board-ui/src/switch.rs`
- Modify: `plugins/superpowers-board/crates/board-core/src/switch.rs`
- Test: 上述两文件的 `#[cfg(test)] mod tests`

**Interfaces:**
- Produces: `read_layer(path) -> Option<BoardSwitch>` 保持不变式（先读新键、回退旧键）；`write_layer(path, value)` 只写新键。

- [ ] **Step 1: 写失败测试（board-ui 侧）**

在 `yi-agent-rs/crates/yi-agent-board-ui/src/switch.rs` 的测试模块加入：

```rust
#[test]
fn reads_the_legacy_switch_key_when_the_new_one_is_absent() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("preferences.json");
    std::fs::write(&path, r#"{"superpowers_board":true}"#).unwrap();
    assert_eq!(read_layer(&path), Some(BoardSwitch::Enabled));
}

#[test]
fn the_new_key_wins_over_the_legacy_one() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("preferences.json");
    std::fs::write(
        &path,
        r#"{"superpowers_board":true,"superpowers_kanban":false}"#,
    )
    .unwrap();
    assert_eq!(read_layer(&path), Some(BoardSwitch::Disabled));
}

#[test]
fn writes_only_the_new_key_and_preserves_other_keys() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("preferences.json");
    std::fs::write(&path, r#"{"subagent_runtime":"always"}"#).unwrap();
    write_layer(&path, BoardSwitch::Enabled).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(value["superpowers_kanban"], true);
    assert_eq!(value["subagent_runtime"], "always");
    assert!(value.get("superpowers_board").is_none());
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && TMPDIR=/Users/gongyichen/.yi-agent-tmp cargo test -p yi-agent-board-ui --offline switch`
Expected: FAIL（新键未被识别）

- [ ] **Step 3: 改实现**

把 `read_layer` 中的取值改为先新后旧：

```rust
    let key = |name: &str| value.get(name).and_then(Value::as_bool);
    match key("superpowers_kanban").or_else(|| key("superpowers_board")) {
        Some(true) => Some(BoardSwitch::Enabled),
        Some(false) => Some(BoardSwitch::Disabled),
        None => None,
    }
```

把 `write_layer` 中的插入键改为 `"superpowers_kanban"`。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && TMPDIR=/Users/gongyichen/.yi-agent-tmp cargo test -p yi-agent-board-ui --offline switch`
Expected: PASS

- [ ] **Step 5: 同步插件侧（board-core）**

在 `plugins/superpowers-board/crates/board-core/src/switch.rs`，把 `Preferences` 结构体加字段并同样"新优先、旧回退"：

```rust
    #[derive(Deserialize)]
    struct Preferences {
        superpowers_kanban: Option<bool>,
        superpowers_board: Option<bool>,
    }
    let parsed: Preferences = serde_json::from_str(text).ok()?;
    match parsed
        .superpowers_kanban
        .or(parsed.superpowers_board)?
    {
        true => Some(SwitchValue::Enabled),
        false => Some(SwitchValue::Disabled),
    }
```

并新增测试：仅旧键 → `Enabled`；仅新键 → `Enabled`；两者冲突 → 新键胜。

- [ ] **Step 6: 运行插件测试**

Run: `cd plugins/superpowers-board && TMPDIR=/Users/gongyichen/.yi-agent-tmp cargo test --offline switch`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-board-ui/src/switch.rs plugins/superpowers-board/crates/board-core/src/switch.rs
git commit -m "feat(superpowers-kanban): migrate the switch key with a legacy fallback"
```

---

### Task 2: 状态目录迁移（`.yi-agent/board` → `.yi-agent/superpowers-kanban`）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-board-ui/src/inbox.rs`
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/supervisor.rs:13-30`
- Test: 上述两文件的测试模块

**Interfaces:**
- Produces:
  - `board_state_dir(workdir) -> PathBuf`：**写入**位置，恒为新目录。
  - `board_state_dir_for_read(workdir) -> PathBuf`：**读取**位置，新目录存在则新，否则旧目录存在则旧，否则新。
  - `Supervisor::layout.state_dir` 指向写位置（新目录）。
- Consumes: Task 1 无依赖。

> **设计决定（spec §3 的落地口径）**：写入恒发新目录；读取先新后旧回退。这样旧数据仍可见，新数据不与旧数据混写。旧目录**不被修改**。

- [ ] **Step 1: 写失败测试**

在 `yi-agent-rs/crates/yi-agent-board-ui/src/inbox.rs` 测试模块加入：

```rust
#[test]
fn the_write_directory_is_always_the_new_name() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".yi-agent/board")).unwrap();
    assert_eq!(
        board_state_dir(dir.path()),
        dir.path().join(".yi-agent/superpowers-kanban")
    );
}

#[test]
fn reads_fall_back_to_the_legacy_directory() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".yi-agent/board")).unwrap();
    assert_eq!(
        board_state_dir_for_read(dir.path()),
        dir.path().join(".yi-agent/board")
    );
}

#[test]
fn reads_prefer_the_new_directory_when_both_exist() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".yi-agent/board")).unwrap();
    std::fs::create_dir_all(dir.path().join(".yi-agent/superpowers-kanban")).unwrap();
    assert_eq!(
        board_state_dir_for_read(dir.path()),
        dir.path().join(".yi-agent/superpowers-kanban")
    );
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && TMPDIR=/Users/gongyichen/.yi-agent-tmp cargo test -p yi-agent-board-ui --offline inbox`
Expected: FAIL（`board_state_dir_for_read` 未定义）

- [ ] **Step 3: 改实现**

```rust
/// 插件状态目录（**写入**位置）：`<workdir>/.yi-agent/superpowers-kanban`。
pub fn board_state_dir(workdir: &Path) -> PathBuf {
    workdir.join(".yi-agent").join("superpowers-kanban")
}

/// 读取位置：新目录存在则用新，否则旧目录存在则用旧（迁移期兼容），
/// 两者都无则返回新目录（供首次创建）。**从不修改旧目录。**
pub fn board_state_dir_for_read(workdir: &Path) -> PathBuf {
    let new = board_state_dir(workdir);
    if new.is_dir() {
        return new;
    }
    let legacy = workdir.join(".yi-agent").join("board");
    if legacy.is_dir() {
        return legacy;
    }
    new
}
```

- [ ] **Step 4: 让只读调用方走读位置**

`yi-agent-board-ui/src/state.rs::load_cards` 的调用方（app-server 的 `board_list`、TUI 的 `handle_kanban`）改为传入 `board_state_dir_for_read(workdir)`。**写入**（`deliver_card`）继续用 `board_state_dir`。

- [ ] **Step 5: 运行确认通过**

Run: `cd yi-agent-rs && TMPDIR=/Users/gongyichen/.yi-agent-tmp cargo test -p yi-agent-board-ui --offline`
Expected: PASS

- [ ] **Step 6: 改 supervisor Layout**

`yi-agent-rs/crates/yi-agent-supervisors/src/supervisor.rs`：`state_dir` 由 `.yi-agent/board` 改为 `.yi-agent/superpowers-kanban`。新增测试断言 `Layout::for_workdir` 的 `state_dir` 为新名。

- [ ] **Step 7: 运行**

Run: `cd yi-agent-rs && TMPDIR=/Users/gongyichen/.yi-agent-tmp cargo test -p yi-agent-supervisors --offline`
Expected: PASS

- [ ] **Step 8: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-board-ui/src/inbox.rs yi-agent-rs/crates/yi-agent-board-ui/src/state.rs yi-agent-rs/crates/yi-agent-supervisors/src/supervisor.rs
git commit -m "feat(superpowers-kanban): migrate the state directory with a legacy read fallback"
```

---

### Task 3: 日历配置文件名迁移（`kanban.toml` → `superpowers-kanban.toml`）

**Files:**
- Modify: `plugins/superpowers-board/crates/board-core/src/calendar.rs`
- Modify: `plugins/superpowers-board/crates/board-runner/src/main.rs:81`
- Test: `plugins/superpowers-board/crates/board-core/src/calendar.rs`

**Interfaces:**
- Produces: `ConcurrencyCalendar::load_preferring_new(state_dir: &Path) -> Self`——新文件存在用新，否则旧文件存在用旧，否则默认。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn prefers_the_new_calendar_file_over_the_legacy_name() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("kanban.toml"), "default_max_tasks = 3\n").unwrap();
    std::fs::write(
        dir.path().join("superpowers-kanban.toml"),
        "default_max_tasks = 10\n",
    )
    .unwrap();
    assert_eq!(
        ConcurrencyCalendar::load_preferring_new(dir.path()).default_max_tasks,
        10
    );
}

#[test]
fn falls_back_to_the_legacy_calendar_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("kanban.toml"), "default_max_tasks = 3\n").unwrap();
    assert_eq!(
        ConcurrencyCalendar::load_preferring_new(dir.path()).default_max_tasks,
        3
    );
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd plugins/superpowers-board && TMPDIR=/Users/gongyichen/.yi-agent-tmp cargo test --offline calendar`
Expected: FAIL

- [ ] **Step 3: 改实现**

```rust
    /// 新名优先、旧名回退、都无则默认。
    pub fn load_preferring_new(state_dir: &std::path::Path) -> Self {
        let new = state_dir.join("superpowers-kanban.toml");
        if new.is_file() {
            return Self::load_or_default(&new);
        }
        let legacy = state_dir.join("kanban.toml");
        if legacy.is_file() {
            return Self::load_or_default(&legacy);
        }
        Self::default()
    }
```

- [ ] **Step 4: 运行确认通过**

Run: `cd plugins/superpowers-board && TMPDIR=/Users/gongyichen/.yi-agent-tmp cargo test --offline calendar`
Expected: PASS

- [ ] **Step 5: 接线**

`board-runner/src/main.rs:81` 的 `ConcurrencyCalendar::load_or_default(&args.state_dir.join("kanban.toml"))` 改为 `ConcurrencyCalendar::load_preferring_new(&args.state_dir)`。

- [ ] **Step 6: Commit**

```bash
git add plugins/superpowers-board/crates/board-core/src/calendar.rs plugins/superpowers-board/crates/board-runner/src/main.rs
git commit -m "feat(superpowers-kanban): prefer the new calendar filename with a legacy fallback"
```

---

### Task 4: 插件 crate 与二进制改名

**Files:**
- Modify: `plugins/superpowers-board/Cargo.toml`、三个 crate 的 `Cargo.toml`
- Modify: 所有 `use board_core::` / `board_ipc::` / `board_runner::` 与 `-p board-*` 调用
- Rename dirs: `crates/board-core` → `crates/superpowers-kanban-core`，`board-ipc` → `-ipc`，`board-runner` → `-runner`

**Interfaces:**
- Produces: crate 名 `superpowers-kanban-core` / `-ipc` / `-runner`；二进制 `superpowers-kanban`（Task 5 完成 `run` 子命令）。

- [ ] **Step 1: 改 Cargo 清单**

`plugins/superpowers-board/Cargo.toml` members 与 `[workspace.dependencies]` 的 path 键改为新名；三个 crate 的 `name =` 改为 `superpowers-kanban-core|-ipc|-runner`；`board-runner` 的 `[[bin]] name` 改为 `superpowers-kanban`。

- [ ] **Step 2: 改目录名**

```bash
cd plugins/superpowers-board && git mv crates/board-core crates/superpowers-kanban-core && git mv crates/board-ipc crates/superpowers-kanban-ipc && git mv crates/board-runner crates/superpowers-kanban-runner
```

- [ ] **Step 3: 改用例**

`grep -rl "board_core\|board_ipc\|board_runner" crates/` 逐个替换为 `superpowers_kanban_core` 等（Rust 标识符用下划线）。

- [ ] **Step 4: 构建并跑全部插件测试**

Run: `cd plugins/superpowers-board && TMPDIR=/Users/gongyichen/.yi-agent-tmp cargo test --offline`
Expected: PASS（原 76 项全过）

- [ ] **Step 5: Commit**

```bash
git add -A plugins/superpowers-board
git commit -m "refactor(superpowers-kanban): rename the plugin crates and binary"
```

---

### Task 5: 插件 `run` 子命令

**Files:**
- Modify: `plugins/superpowers-board/crates/superpowers-kanban-runner/src/main.rs`
- Test: 同文件测试模块

**Interfaces:**
- Produces: `superpowers-kanban run --runtime-dir <d> --state-dir <d> --project-root <d> [--interval-secs N]`——即原守护循环。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn run_is_the_only_accepted_subcommand_for_the_daemon_loop() {
    assert!(parse_args_from(["run", "--runtime-dir", "/r", "--state-dir", "/s"]).is_ok());
    assert!(parse_args_from(["--runtime-dir", "/r", "--state-dir", "/s"]).is_err());
}
```

（把现有 `parse_args()` 抽出参数化的 `parse_args_from<I: IntoIterator<Item = &str>>` 以便测试。）

- [ ] **Step 2: 运行确认失败**

Run: `cd plugins/superpowers-board && TMPDIR=/Users/gongyichen/.yi-agent-tmp cargo test --offline run_is_the_only`
Expected: FAIL

- [ ] **Step 3: 实现**

`main()` 先取 `args.next()`，必须是 `"run"`；否则打印用法并以码 2 退出。其余参数解析沿用。

- [ ] **Step 4: 运行确认通过**

Run: `cd plugins/superpowers-board && TMPDIR=/Users/gongyichen/.yi-agent-tmp cargo test --offline`
Expected: PASS

- [ ] **Step 5: 同时更新 supervisor 清单的 args**

`plugins/superpowers-board/supervisors/superpowers-kanban.json`（原名 `superpowers-board.json`，Task 6 改名）：`command` = `superpowers-kanban`，`args` 首位加 `"run"`，`name` = `superpowers-kanban`，`switch_key` = `superpowers_kanban`。

- [ ] **Step 6: Commit**

```bash
git add -A plugins/superpowers-board
git commit -m "feat(superpowers-kanban): add the run subcommand and point the manifest at it"
```

---

### Task 6: 插件目录、清单文件名与文档

**Files:**
- Rename: `plugins/superpowers-board/` → `plugins/superpowers-kanban/`
- Rename: `supervisors/superpowers-board.json` → `superpowers-kanban.json`
- Modify: 插件 `README.md`、`kanban.toml`（示例改名）

- [ ] **Step 1: 目录与文件改名**

```bash
git mv plugins/superpowers-board plugins/superpowers-kanban && cd plugins/superpowers-kanban && git mv supervisors/superpowers-board.json supervisors/superpowers-kanban.json && git mv kanban.toml superpowers-kanban.toml
```

- [ ] **Step 2: 更新 Cargo 工作区引用**

主程序 `yi-agent-rs/` 中若有指向 `plugins/superpowers-board/...` 的 path 依赖或脚本，一并更新（用 `grep -rn "superpowers-board" yi-agent-rs/ --include=*.toml --include=*.rs` 确认）。

- [ ] **Step 3: 更新 README**

把安装步骤改为新名：二进制 `superpowers-kanban`、skill `superpowers-kanban`、清单 `superpowers-kanban.json`、开关键 `superpowers_kanban`、日历 `superpowers-kanban.toml`。

- [ ] **Step 4: 构建确认**

Run: `cd plugins/superpowers-kanban && TMPDIR=/Users/gongyichen/.yi-agent-tmp cargo test --offline`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "refactor(superpowers-kanban): rename the plugin directory, manifest and sample calendar"
```

---

### Task 7: TUI 命令改名（`/kanban` → `/superpowers-kanban` + 别名）

**Files:**
- Rename: `yi-agent-rs/crates/yi-agent/src/tui/board.rs` → `superpowers_kanban.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/mod.rs:5`、`slash.rs`、`app.rs:2316`

**Interfaces:**
- Produces: `/superpowers-kanban [on|off|run|add <spec> <plan>]`；`/kanban` 作为别名，命中时先打印一行"已更名为 /superpowers-kanban"再执行。

- [ ] **Step 1: 写失败测试**

在 `slash.rs` 测试模块：

```rust
#[test]
fn the_command_is_named_superpowers_kanban() {
    assert_eq!(SlashCommand::Kanban.name(), "superpowers-kanban");
}

#[test]
fn the_legacy_kanban_name_still_resolves() {
    assert_eq!(SlashCommand::from_name("kanban"), Some(SlashCommand::Kanban));
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && TMPDIR=/Users/gongyichen/.yi-agent-tmp cargo test -p yi-agent --offline slash::tests::the_command_is_named`
Expected: FAIL

- [ ] **Step 3: 实现**

`name()` 返回 `"superpowers-kanban"`；`from_name` 中把 `"kanban"` 与 `"superpowers-kanban"` 都映射到 `SlashCommand::Kanban`；`description()` 文案保持。

- [ ] **Step 4: 别名提示**

在 `app.rs` 的 `SlashCommand::Kanban` 分支，若原始输入名是 `kanban`，先 push 一行 `HistoryCell::Separator { label: Some("已更名为 /superpowers-kanban".into()) }`。

- [ ] **Step 5: 模块与文件改名**

```bash
cd yi-agent-rs && git mv crates/yi-agent/src/tui/board.rs crates/yi-agent/src/tui/superpowers_kanban.rs
```

`mod.rs` 的 `pub mod board;` → `pub mod superpowers_kanban;`；`app.rs` 的调用改为 `crate::tui::superpowers_kanban::handle_kanban`。

- [ ] **Step 6: 运行**

Run: `cd yi-agent-rs && TMPDIR=/Users/gongyichen/.yi-agent-tmp cargo test -p yi-agent --offline`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add -A yi-agent-rs/crates/yi-agent/src/tui
git commit -m "feat(superpowers-kanban): rename the TUI command with a legacy alias"
```

---

### Task 8: app-server RPC 与桌面端命名

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（4 条 RPC 名）
- Rename+Modify: `desktop/src/components/BoardView.tsx` → `SuperpowersKanbanView.tsx`；`SettingsPanel.tsx` → `SuperpowersKanbanSettings.tsx`；`desktop/src/lib/boardState.ts` → `superpowersKanbanState.ts`；`desktop/src/lib/boardSwitch.ts` → `superpowersKanbanSwitch.ts`（含各自 `.test.*`）

**Interfaces:**
- Produces: RPC `superpowers-kanban/list|enqueue|switch/read|switch/write`（Spec 4 会再替换为 `plugin/query`）。

- [ ] **Step 1: 改 RPC 名（Rust）**

`server.rs` 中 `"board/list"` → `"superpowers-kanban/list"`，其余三条同理；同步改其测试断言。

- [ ] **Step 2: 运行**

Run: `cd yi-agent-rs && TMPDIR=/Users/gongyichen/.yi-agent-tmp cargo test -p yi-agent-app-server --offline board`
Expected: PASS

- [ ] **Step 3: 桌面端改名（文件 + 引用）**

```bash
cd desktop && git mv src/components/BoardView.tsx src/components/SuperpowersKanbanView.tsx && git mv src/components/BoardView.test.tsx src/components/SuperpowersKanbanView.test.tsx && git mv src/components/SettingsPanel.tsx src/components/SuperpowersKanbanSettings.tsx && git mv src/components/SettingsPanel.test.tsx src/components/SuperpowersKanbanSettings.test.tsx && git mv src/lib/boardState.ts src/lib/superpowersKanbanState.ts && git mv src/lib/boardState.test.ts src/lib/superpowersKanbanState.test.ts && git mv src/lib/boardSwitch.ts src/lib/superpowersKanbanSwitch.ts && git mv src/lib/boardSwitch.test.ts src/lib/superpowersKanbanSwitch.test.ts
```

更新导出名与所有 import（`App.tsx`、各 test）。

- [ ] **Step 4: 改桌面端 RPC 字符串**

`superpowersKanbanSwitch.ts` 中的 `"board/list"` 等改为 `"superpowers-kanban/list"` 等。

- [ ] **Step 5: 类型检查与测试**

Run: `cd desktop && export PATH="/opt/homebrew/bin:$PATH" && npx tsc --noEmit && TMPDIR="$PWD/.tmpverify" npx vitest run`
Expected: PASS（29 文件 / 256 测试）

- [ ] **Step 6: Commit**

```bash
git add -A yi-agent-rs/crates/yi-agent-app-server desktop
git commit -m "refactor(superpowers-kanban): rename the RPCs and desktop components"
```

---

### Task 9: 端到端回归

**Files:** 无（验证任务）

- [ ] **Step 1: 全量 Rust 测试**

Run: `cd yi-agent-rs && TMPDIR=/Users/gongyichen/.yi-agent-tmp cargo test --offline 2>&1 | grep -E "test result: (ok|FAILED)"`
Expected: 全 ok（app-server 188、core 236+、store 等）

- [ ] **Step 2: 插件测试**

Run: `cd plugins/superpowers-kanban && TMPDIR=/Users/gongyichen/.yi-agent-tmp cargo test --offline`
Expected: PASS

- [ ] **Step 3: 桌面端**

Run: `cd desktop && export PATH="/opt/homebrew/bin:$PATH" && npx tsc --noEmit && TMPDIR="$PWD/.tmpverify" npx vitest run`
Expected: PASS

- [ ] **Step 4: 迁移的手工冒烟**

在临时 workdir 造一份**旧**布局（`.yi-agent/board/board.json` + 投递文件 + 旧键 `superpowers_board` 的 `preferences.json`），确认：TUI `/superpowers-kanban` 能看到旧卡片；`add` 写入的是新目录；旧文件未被改动。

- [ ] **Step 5: Commit（若冒烟暴露问题则先修）**

```bash
git commit --allow-empty -m "test(superpowers-kanban): record the migration smoke check"
```
