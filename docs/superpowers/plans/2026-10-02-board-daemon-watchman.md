# 看板 daemon 持续值守（watchman）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让排队中的看板任务在无人盯着时持续自动推进——app 关闭、daemon 被杀、机器重启后都能自愈——且任何时刻同一项目只有一个插件在推进队列。

**Architecture:** 一份 `ensure_daemons`（探测 `is_running`，不在就 `launch_if_absent`），两个宿主：常驻 watchman（launchd 托管 `yi-agent boards watch`）与 app-server 内循环。watchman 只读**通用**常驻登记 `~/.yi-agent/resident-daemons.json`（不含 board 语义），看板生命周期向它登记/摘除。插件加单实例锁 + daemon 失联即退，修掉 daemon 崩溃后孤儿插件与新插件双跑推进同一队列的问题。

**Tech Stack:** Rust 2024（主工作区 `yi-agent-rs/` + 独立插件 workspace `plugins/superpowers-kanban/`）、macOS launchd（plist）、React/TypeScript 桌面端、vitest。

## Global Constraints

- 主工作区命令一律从 `yi-agent-rs/` 跑：`export PATH="/Users/gongyichen/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`，cargo 一律 `--offline`。
- `export TMPDIR=/Users/gongyichen/.yi-agent-tmp`（socket 路径 103 字节上限）；桌面端 `export PATH="/opt/homebrew/bin:$PATH"`。
- 插件是**独立 workspace**，零 `yi-agent-*` 依赖；测试在其根目录 `cargo test --offline`。
- 命名统一 `superpowers-kanban` / `board_watchman` / `resident-daemons`；LaunchAgent label 用 `ai.yi-agent.board-watchman`。
- 测试**不得**读写真实 `$HOME`；一律临时目录（`tempfile`）。
- 提交纪律：共享工作树，只显式路径 `git add`，提交前 `git status` 核对；工作树为 `.worktrees/board-daemon-watchman`（分支 `board-daemon-watchman`）。
- **不改** daemon 的 `InstanceLock` 语义；**不改** `yi-agent-supervisors` 公开语义。

---

### Task 1: 通用常驻登记 `yi-agent-store::resident`

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-store/src/resident.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/lib.rs`（加 `pub mod resident;`）
- Test: 同文件 `#[cfg(test)] mod tests`

**Interfaces:**
- Produces:
  - `pub fn default_dir() -> Option<PathBuf>` —— `$HOME/.yi-agent`（`HOME` 空/缺 → `None`）。
  - `pub fn require(dir: &Path, project: &Path, requester: &str) -> std::io::Result<()>`
  - `pub fn release(dir: &Path, project: &Path, requester: &str) -> std::io::Result<()>`
  - `pub fn list(dir: &Path) -> Vec<PathBuf>` —— 读侧宽容（损坏按空表）。
  - `pub fn registry_path(dir: &Path) -> PathBuf` —— `dir/resident-daemons.json`。
- 语义：`require` 幂等（同一 requester 不重复）；`release` 摘除 requester，`required_by` 空则删该项目项；`require`/`release` 遇到**损坏**文件报错且不覆盖（不静默丢别人的登记）。

- [ ] **Step 1: 写失败测试**

```rust
// yi-agent-rs/crates/yi-agent-store/src/resident.rs 末尾
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_is_idempotent_and_lists_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let project = PathBuf::from("/proj/a");
        require(dir.path(), &project, "superpowers-kanban").unwrap();
        require(dir.path(), &project, "superpowers-kanban").unwrap();
        assert_eq!(list(dir.path()), vec![project]);
    }

    #[test]
    fn two_requesters_share_one_entry_and_release_only_their_own() {
        let dir = tempfile::tempdir().unwrap();
        let project = PathBuf::from("/proj/a");
        require(dir.path(), &project, "superpowers-kanban").unwrap();
        require(dir.path(), &project, "another-plugin").unwrap();
        release(dir.path(), &project, "superpowers-kanban").unwrap();
        assert_eq!(list(dir.path()), vec![project.clone()], "still needed by the other plugin");
        release(dir.path(), &project, "another-plugin").unwrap();
        assert!(list(dir.path()).is_empty(), "entry drops when nothing needs it");
    }

    #[test]
    fn a_corrupt_registry_reads_empty_and_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(registry_path(dir.path()), "{ not json").unwrap();
        assert!(list(dir.path()).is_empty(), "readers must not crash");
        let error = require(dir.path(), &PathBuf::from("/proj/a"), "superpowers-kanban").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(
            std::fs::read_to_string(registry_path(dir.path())).unwrap(),
            "{ not json",
            "a corrupt file must not be clobbered"
        );
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent-store resident:: 2>&1 | tail -20`
Expected: 编译失败 / `resident` 模块不存在。

- [ ] **Step 3: 实现**

```rust
// yi-agent-rs/crates/yi-agent-store/src/resident.rs
//! 通用「常驻 daemon」登记：哪些项目需要常驻 daemon、由谁请求。
//!
//! 与任何具体插件/看板无关：字段只有 `project` 与 `required_by`。写它的是
//! 需要常驻 daemon 的那个组件，读它的是值守者（watchman 与 app 内循环）。
//! 放 store 层而非某个功能 crate，是为了让登记本身保持通用。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 宿主通用目录：`$HOME/.yi-agent`。`HOME` 缺失返回 `None`。
pub fn default_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".yi-agent"))
}

pub fn registry_path(dir: &Path) -> PathBuf {
    dir.join("resident-daemons.json")
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    #[serde(default)]
    projects: Vec<Entry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    project: PathBuf,
    #[serde(default)]
    required_by: Vec<String>,
}

/// 严格读：缺文件→空表；损坏→报错（写路径不得覆盖损坏文件）。
fn read_strict(dir: &Path) -> std::io::Result<Registry> {
    let path = registry_path(dir);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|error| {
            std::io::Error::other(format!(
                "resident registry is corrupt ({error}); refusing to overwrite {}",
                path.display()
            ))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Registry::default()),
        Err(error) => Err(error),
    }
}

fn write(dir: &Path, registry: &Registry) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let text = serde_json::to_string_pretty(registry).map_err(std::io::Error::other)?;
    let tmp = dir.join("resident-daemons.json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, registry_path(dir))
}

/// 登记「`project` 需要常驻 daemon，请求者 `requester`」。幂等。
pub fn require(dir: &Path, project: &Path, requester: &str) -> std::io::Result<()> {
    let mut registry = read_strict(dir)?;
    match registry
        .projects
        .iter_mut()
        .find(|entry| entry.project == project)
    {
        Some(entry) => {
            if !entry.required_by.iter().any(|name| name == requester) {
                entry.required_by.push(requester.to_string());
                entry.required_by.sort();
            }
        }
        None => registry.projects.push(Entry {
            project: project.to_path_buf(),
            required_by: vec![requester.to_string()],
        }),
    }
    write(dir, &registry)
}

/// 撤销 `requester` 对该项目的需要；没人再需要就删掉该项目项。
pub fn release(dir: &Path, project: &Path, requester: &str) -> std::io::Result<()> {
    let mut registry = read_strict(dir)?;
    registry.projects.retain_mut(|entry| {
        if entry.project != project {
            return true;
        }
        entry.required_by.retain(|name| name != requester);
        !entry.required_by.is_empty()
    });
    write(dir, &registry)
}

/// 读侧宽容：损坏按空表，绝不因一份坏文件让值守者崩掉。
pub fn list(dir: &Path) -> Vec<PathBuf> {
    read_strict(dir)
        .unwrap_or_default()
        .projects
        .into_iter()
        .map(|entry| entry.project)
        .collect()
}
```

`lib.rs` 增加一行：

```rust
pub mod resident;
```

- [ ] **Step 4: 运行确认通过**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent-store resident:: 2>&1 | tail -20`
Expected: PASS（3 个测试）。

- [ ] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-store/src/resident.rs yi-agent-rs/crates/yi-agent-store/src/lib.rs
git commit -m "feat(store): generic resident-daemon registry"
```

---

### Task 2: 看板生命周期写通用登记

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-boards/src/lifecycle.rs`（`create_with_project` / `remove_with` 内）
- Test: `yi-agent-rs/crates/yi-agent-boards/src/lifecycle.rs` 的 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: Task 1 的 `yi_agent_store::resident::{require, release, list, default_dir}`。
- Produces: `create` 成功后通用登记含该项目且 `required_by` 含 `"superpowers-kanban"`；`remove` 后该项目项被摘除。常量 `pub const REQUESTER: &str = "superpowers-kanban";`。

- [ ] **Step 1: 写失败测试**

```rust
// 追加到 lifecycle.rs 的 tests 模块
#[test]
fn creating_a_board_registers_a_resident_daemon_need() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global");
    let resident = dir.path().join("resident");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let mut launcher = |_project: &Path| Ok(true);

    create_with_project(&project, &global, &resident, &mut launcher).unwrap();
    assert_eq!(
        yi_agent_store::resident::list(&resident),
        vec![project.clone()],
        "a created board must declare its need for a resident daemon"
    );
}

#[test]
fn removing_a_board_releases_the_resident_daemon_need() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global");
    let resident = dir.path().join("resident");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let mut launcher = |_project: &Path| Ok(true);
    create_with_project(&project, &global, &resident, &mut launcher).unwrap();

    remove_with(&project, &global, &resident, &mut |_project: &Path| Ok(())).unwrap();
    assert!(
        yi_agent_store::resident::list(&resident).is_empty(),
        "removing the last board drops the resident need"
    );
}
```

> 注：真实签名是 `create_with_project(project, global, launcher)` 与
> `remove_with(project, global, stopper)`（**不是** `remove_with_project`）。本任务给两者
> 各**新增**一个 `resident_dir: &Path` 形参（放在 `global` 之后），并同步所有调用点。

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent-boards lifecycle:: 2>&1 | tail -30`
Expected: 编译失败（`create_with_project` 参数个数不符 / `resident` 未用）。

- [ ] **Step 3: 实现**

在 `lifecycle.rs` 顶部加常量与引入：

```rust
use yi_agent_store::resident;

/// 看板在通用常驻登记里的请求者名。
pub const REQUESTER: &str = "superpowers-kanban";
```

`create` / `create_in` 解析出通用目录并下传（`global` 仍是看板登记表目录，两者不同）：

```rust
pub fn create(project: &Path) -> Result<BoardStatus, String> {
    let global = crate::global_dir().map_err(|error| error.to_string())?;
    create_in(project, &global)
}

pub fn create_in(project: &Path, global: &Path) -> Result<BoardStatus, String> {
    let resident = resident::default_dir()
        .ok_or_else(|| "cannot locate the resident registry: HOME is not set".to_string())?;
    let mut launcher = |project: &Path| launch_if_absent(project);
    create_with_project(project, global, &resident, &mut launcher)
}
```

`create_with_project`：在**成功**注册后追加一次登记（失败不应留下「已声明」的假状态）：

```rust
pub fn create_with_project(
    project: &Path,
    global: &Path,
    resident_dir: &Path,
    launcher: &mut dyn FnMut(&Path) -> Result<bool, String>,
) -> Result<BoardStatus, String> {
    // ... 既有实现（登记表 register + launcher + status）不变 ...
    let status = { /* 既有返回值 */ };
    resident::require(resident_dir, project, REQUESTER)
        .map_err(|error| format!("could not record the resident daemon need: {error}"))?;
    Ok(status)
}
```

`remove` / `remove_in` / `remove_with` 对称地加 `resident_dir` 形参，并在成功移除后：

```rust
    resident::release(resident_dir, project, REQUESTER)
        .map_err(|error| format!("could not release the resident daemon need: {error}"))?;
```

**把所有既有调用点补上新参数**（否则编译不过，这是有意的）。完整清单（`grep -rn "create_with_project\|remove_with" yi-agent-rs/`）：
- `yi-agent-boards/src/lifecycle.rs`：`create_in`、`remove_in`（生产包装，用 `resident::default_dir()`）
- `yi-agent-boards/src/lifecycle.rs` 测试模块：第 311/327/361/381/403/445/592/607 行的 `create_with_project`、第 410 行的 `remove_with`（测试用临时目录）
- `yi-agent-app-server/src/server.rs:1592` `create_with_project`（用注入的 `resident_dir` 字段，见 Task 6）
- `yi-agent-app-server/src/server.rs` 测试 helper 里的调用
- `yi-agent/src/tui/board.rs:41` `create_with_project`、`:50` `remove_with`（`resident::default_dir()`）

`remove` / `remove_in` / `remove_with` 对称地加 `resident_dir` 形参，并在成功移除后：

- [ ] **Step 4: 运行确认通过**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent-boards 2>&1 | tail -30`
Expected: PASS（含 2 个新测试）。
再跑调用点所在 crate：`cargo test --offline -p yi-agent-app-server -p yi-agent 2>&1 | tail -20` → PASS。

- [ ] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-boards/src/lifecycle.rs yi-agent-rs/crates/yi-agent-app-server/src/server.rs yi-agent-rs/crates/yi-agent/src/tui/board.rs
git commit -m "feat(boards): declare a resident daemon need when a board is created"
```

---

### Task 3: `ensure_daemons` 胶水

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-boards/src/watch.rs`
- Modify: `yi-agent-rs/crates/yi-agent-boards/src/lib.rs`（`pub mod watch;`）
- Test: `yi-agent-rs/crates/yi-agent-boards/src/watch.rs` 的 `#[cfg(test)] mod tests`

**Interfaces:**
- Produces:
  - `pub fn ensure_daemons(projects: &[PathBuf])` —— 生产：每个项目 `launch_if_absent`。
  - `pub fn ensure_daemons_with(projects: &[PathBuf], launcher: &mut dyn FnMut(&Path) -> Result<bool, String>) -> Vec<PathBuf>` —— 测试用注入；返回「本次真正拉起」的项目（`Ok(true)` 且之前不在跑）。失败只记日志、不中断其余项目。
  - `pub fn once(resident_dir: &Path, ensure: &mut dyn FnMut(&[PathBuf])) -> usize` —— 读通用登记 → `ensure(&projects)` → 返回登记项目数。**CLI 与 app-server 共用这一个**，避免两处逐字重复的胶水。
- Consumes: `lifecycle::launch_if_absent`；`resident::list`（调用方传 projects）。

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn every_registered_project_is_ensured() {
        let projects = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        let seen = RefCell::new(Vec::new());
        ensure_daemons_with(&projects, &mut |project| {
            seen.borrow_mut().push(project.to_path_buf());
            Ok(true)
        });
        assert_eq!(*seen.borrow(), projects);
    }

    #[test]
    fn once_reads_the_registry_and_hands_it_to_the_ensurer() {
        let dir = tempfile::tempdir().unwrap();
        let a = PathBuf::from("/a");
        yi_agent_store::resident::require(dir.path(), &a, "superpowers-kanban").unwrap();
        let mut seen: Vec<PathBuf> = Vec::new();
        let count = once(dir.path(), &mut |projects| seen.extend_from_slice(projects));
        assert_eq!(count, 1);
        assert_eq!(seen, vec![a]);
    }

    #[test]
    fn one_failing_project_does_not_stop_the_others() {
        let projects = vec![PathBuf::from("/a"), PathBuf::from("/b"), PathBuf::from("/c")];
        let seen = RefCell::new(Vec::new());
        let started = ensure_daemons_with(&projects, &mut |project| {
            seen.borrow_mut().push(project.to_path_buf());
            if project == Path::new("/b") {
                return Err("boom".to_string());
            }
            Ok(true)
        });
        assert_eq!(seen.borrow().len(), 3, "all projects are attempted");
        assert_eq!(started, vec![PathBuf::from("/a"), PathBuf::from("/c")]);
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent-boards watch:: 2>&1 | tail -20`
Expected: 编译失败（`watch` 模块不存在）。

- [ ] **Step 3: 实现**

```rust
// yi-agent-rs/crates/yi-agent-boards/src/watch.rs
//! 保证「登记在册的项目」各自有一个活着的 daemon。
//!
//! 一份逻辑、两个宿主：常驻 watchman（launchd 托管）与 app-server 内循环都调它。
//! 幂等——daemon 已应答就什么都不做；daemon 自身有独占锁，重复探测/拉起不会双起。

use std::path::{Path, PathBuf};

/// 生产入口：用真实 `launch_if_absent`。
pub fn ensure_daemons(projects: &[PathBuf]) {
    ensure_daemons_with(projects, &mut crate::lifecycle::launch_if_absent);
}

/// 注入版：返回本次真正拉起的项目。单个失败只记日志，不影响其余项目。
pub fn ensure_daemons_with(
    projects: &[PathBuf],
    launcher: &mut dyn FnMut(&Path) -> Result<bool, String>,
) -> Vec<PathBuf> {
    let mut started = Vec::new();
    for project in projects {
        match launcher(project) {
            Ok(true) => started.push(project.clone()),
            Ok(false) => {}
            Err(error) => {
                eprintln!(
                    "board watchman: could not ensure a daemon for {}: {error}",
                    project.display()
                );
            }
        }
    }
    started
}
```

```rust
/// 读通用登记并交给 `ensure`；返回登记项目数。CLI 与 app-server 共用这一份，
/// 不各自复制「读登记 → ensure」的胶水。
pub fn once(resident_dir: &Path, ensure: &mut dyn FnMut(&[PathBuf])) -> usize {
    let projects = yi_agent_store::resident::list(resident_dir);
    ensure(&projects);
    projects.len()
}
```

`lib.rs` 加：`pub mod watch;`

- [ ] **Step 4: 运行确认通过**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent-boards watch:: 2>&1 | tail -20`
Expected: PASS（2 个测试）。

- [ ] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-boards/src/watch.rs yi-agent-rs/crates/yi-agent-boards/src/lib.rs
git commit -m "feat(boards): ensure_daemons glue shared by watchman and the app"
```

---

### Task 4: `yi-agent boards watch` 子命令

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs`（新增 `Command::Boards` + `BoardsAction`）
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`（分发 + `watch` 循环）
- Test: `yi-agent-rs/crates/yi-agent/src/main.rs` 的 tests（参数解析）+ `watch` 循环单测

**Interfaces:**
- Consumes: `yi_agent_boards::resident::{default_dir, list}`、`yi_agent_boards::watch::ensure_daemons`。
- Produces: CLI `yi-agent boards watch [--interval-secs N]`（默认 30）；`fn watch_once(resident_dir: &Path, ensure: &mut dyn FnMut(&[PathBuf])) -> usize` 返回本轮登记项目数（便于测试）。

- [ ] **Step 1: 写失败测试**

```rust
// main.rs tests
#[test]
fn boards_watch_parses_with_a_default_interval() {
    let cli = Cli::try_parse_from(["yi-agent", "boards", "watch"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Boards { action: BoardsAction::Watch { interval_secs: 30 } })
    ));
}

#[test]
fn boards_watch_once_ensures_every_registered_project() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    yi_agent_store::resident::require(dir.path(), &a, "superpowers-kanban").unwrap();
    yi_agent_store::resident::require(dir.path(), &b, "superpowers-kanban").unwrap();

    let mut seen: Vec<PathBuf> = Vec::new();
    let count = yi_agent_boards::watch::once(dir.path(), &mut |projects: &[PathBuf]| {
        seen.extend_from_slice(projects)
    });
    assert_eq!(count, 2);
    assert_eq!(seen, vec![a, b]);
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent boards_watch 2>&1 | tail -20`
Expected: 编译失败（`Command::Boards` / `watch_once` 不存在）。

- [ ] **Step 3: 实现**

`config.rs`：

```rust
    /// 常驻值守：保证登记在册的项目各自有一个活着的 daemon。
    Boards {
        #[command(subcommand)]
        action: BoardsAction,
    },
```

```rust
#[derive(clap::Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum BoardsAction {
    /// 值守循环：按登记表确保各项目 daemon 存活（由 launchd 常驻托管）。
    Watch {
        #[arg(long, default_value_t = 30)]
        interval_secs: u64,
    },
}
```

`main.rs` 分发（在 `match cli.command` 里加一支）：

```rust
        Some(Command::Boards { ref action }) => match action {
            BoardsAction::Watch { interval_secs } => run_boards_watch(*interval_secs),
        },
```

```rust
/// 常驻值守循环。由 launchd 托管，KeepAlive 负责它自己的存活。
fn run_boards_watch(interval_secs: u64) {
    let Some(resident_dir) = yi_agent_store::resident::default_dir() else {
        eprintln!("yi-agent: cannot watch boards: HOME is not set");
        std::process::exit(1);
    };
    let interval = std::time::Duration::from_secs(interval_secs);
    loop {
        yi_agent_boards::watch::once(&resident_dir, &mut |projects| {
            yi_agent_boards::watch::ensure_daemons(projects);
        });
        std::thread::sleep(interval);
    }
}
```

> 注意：`watch` 是**故意**的死循环（常驻）。不要在测试里调 `run_boards_watch`，只测 `watch_once`。

- [ ] **Step 4: 运行确认通过**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent boards_watch 2>&1 | tail -20`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/config.rs yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "feat(cli): yi-agent boards watch, the resident daemon watchman loop"
```

---

### Task 5: LaunchAgent 安装 / 卸载 / 偏好开关

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-boards/src/watchman.rs`
- Modify: `yi-agent-rs/crates/yi-agent-boards/src/lib.rs`（`pub mod watchman;`）
- Test: 同文件 tests

**Interfaces:**
- Produces:
  - `pub const PREFERENCE_KEY: &str = "board_watchman_enabled";`
  - `pub const LABEL: &str = "ai.yi-agent.board-watchman";`
  - `pub fn plist_path(home: &Path) -> PathBuf` —— `<home>/Library/LaunchAgents/ai.yi-agent.board-watchman.plist`
  - `pub fn plist_contents(exe: &Path, home: &Path) -> String`
  - `pub fn install(exe: &Path, home: &Path) -> Result<(), String>` —— 写 plist + `launchctl bootstrap`（经注入的 runner）
  - `pub fn uninstall(home: &Path) -> Result<(), String>` —— `launchctl bootout` + 删 plist
  - `pub fn is_installed(home: &Path) -> bool` —— plist 存在且 `ProgramArguments[0]` == 当前 exe
- Consumes: 无（`std::process::Command` 调 `launchctl`，测试注入 `_with` 变体）。

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn the_plist_keeps_it_alive_and_runs_boards_watch() {
        let exe = PathBuf::from("/usr/local/bin/yi-agent");
        let home = PathBuf::from("/Users/tester");
        let plist = plist_contents(&exe, &home);
        assert!(plist.contains("<key>Label</key>"));
        assert!(plist.contains(LABEL));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("/usr/local/bin/yi-agent"));
        assert!(plist.contains("boards"));
        assert!(plist.contains("watch"));
    }

    #[test]
    fn install_writes_the_plist_and_bootstraps_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let mut calls: Vec<Vec<String>> = Vec::new();
        install_with(
            &PathBuf::from("/usr/local/bin/yi-agent"),
            home,
            &mut |args| { calls.push(args.to_vec()); Ok(()) },
        )
        .unwrap();
        assert!(plist_path(home).exists(), "the plist must be on disk");
        assert_eq!(calls.len(), 1, "bootstrap is invoked once");
        assert_eq!(calls[0][0], "bootstrap");
        assert_eq!(calls[0][1], format!("gui/{}", unsafe { libc::getuid() }));
    }

    #[test]
    fn uninstall_boots_out_and_removes_the_plist() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        install_with(
            &PathBuf::from("/usr/local/bin/yi-agent"),
            home,
            &mut |_args| Ok(()),
        )
        .unwrap();
        let mut calls: Vec<Vec<String>> = Vec::new();
        uninstall_with(home, &mut |args| { calls.push(args.to_vec()); Ok(()) }).unwrap();
        assert_eq!(calls[0][0], "bootout");
        assert!(!plist_path(home).exists(), "the plist must be gone");
    }

    #[test]
    fn a_plist_pointing_at_a_different_exe_is_not_considered_installed() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        write_plist(home, &plist_contents(&PathBuf::from("/old/yi-agent"), home)).unwrap();
        assert!(!is_installed_for(home, &PathBuf::from("/new/yi-agent")));
        assert!(is_installed_for(home, &PathBuf::from("/old/yi-agent")));
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent-boards watchman:: 2>&1 | tail -20`
Expected: 编译失败（模块不存在）。

- [ ] **Step 3: 实现**

```rust
// yi-agent-rs/crates/yi-agent-boards/src/watchman.rs
//! 把常驻值守装成 macOS LaunchAgent：登录自启 + 崩溃自动拉起。
//!
//! 装的是通用子命令 `yi-agent boards watch`，它只读通用常驻登记、不认识看板。
//! launchctl 调用经 `_with` 变体注入，测试不需要真的动 launchd。

use std::path::{Path, PathBuf};

pub const PREFERENCE_KEY: &str = "board_watchman_enabled";
pub const LABEL: &str = "ai.yi-agent.board-watchman";

pub fn plist_path(home: &Path) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

/// 生成 plist。日志落到 `~/.yi-agent/logs/board-watchman.log`，便于排障。
pub fn plist_contents(exe: &Path, home: &Path) -> String {
    let log = home.join(".yi-agent").join("logs").join("board-watchman.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>boards</string>
    <string>watch</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        exe = exe.display(),
        log = log.display(),
    )
}

fn write_plist(home: &Path, contents: &str) -> Result<(), String> {
    let path = plist_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(&path, contents).map_err(|error| error.to_string())
}

/// 是否已装且指向**当前**可执行文件。路径变了要重装（升级后 exe 可能换位置）。
pub fn is_installed(home: &Path) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    is_installed_for(home, &exe)
}

pub fn is_installed_for(home: &Path, exe: &Path) -> bool {
    std::fs::read_to_string(plist_path(home))
        .map(|text| text.contains(&exe.display().to_string()))
        .unwrap_or(false)
}

fn default_launchctl(args: &[String]) -> Result<(), String> {
    let status = std::process::Command::new("launchctl")
        .args(args)
        .status()
        .map_err(|error| format!("could not run launchctl: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("launchctl {} failed: {status}", args.join(" ")))
    }
}

fn uid_domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

pub fn install(exe: &Path, home: &Path) -> Result<(), String> {
    install_with(exe, home, &mut default_launchctl)
}

pub fn install_with(
    exe: &Path,
    home: &Path,
    launchctl: &mut dyn FnMut(&[String]) -> Result<(), String>,
) -> Result<(), String> {
    write_plist(home, &plist_contents(exe, home))?;
    // 已加载过则先 bootout，避免 bootstrap 报 already bootstrapped。
    let _ = launchctl(&["bootout".into(), format!("{}/{}", uid_domain(), LABEL)]);
    launchctl(&[
        "bootstrap".into(),
        uid_domain(),
        plist_path(home).display().to_string(),
    ])
}

pub fn uninstall(home: &Path) -> Result<(), String> {
    uninstall_with(home, &mut default_launchctl)
}

pub fn uninstall_with(
    home: &Path,
    launchctl: &mut dyn FnMut(&[String]) -> Result<(), String>,
) -> Result<(), String> {
    // bootout 失败（没加载过）不算错；目标是「最终没有它」。
    let _ = launchctl(&["bootout".into(), format!("{}/{}", uid_domain(), LABEL)]);
    match std::fs::remove_file(plist_path(home)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}
```

`lib.rs` 加：`pub mod watchman;`；`Cargo.toml` 确认已有 `libc = "0.2"`（是）。

- [ ] **Step 4: 运行确认通过**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent-boards watchman:: 2>&1 | tail -20`
Expected: PASS（4 个测试）。

- [ ] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-boards/src/watchman.rs yi-agent-rs/crates/yi-agent-boards/src/lib.rs
git commit -m "feat(boards): launchd watchman install/uninstall with a preference switch"
```

---

### Task 6: app-server 接入——设置开关 + 首次创建时装 watchman

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`ui/settings/read|write`、`board/create` 处理器）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/settings_store.rs`（新增 `board_watchman_enabled` 读写）
- Test: 两个文件的 tests

**Interfaces:**
- Consumes: Task 5 的 `watchman::{PREFERENCE_KEY, install, uninstall, is_installed}`；Task 2 的 `lifecycle::REQUESTER`。
- Produces: `ui/settings/read` 返回体新增 `"board_watchman_enabled": bool`（缺省 `true`）；`ui/settings/write` 接受 `board_watchman_enabled` 并据此 `install`/`uninstall`；`board/create` 成功后若开关为开则确保 watchman 已装。

- [ ] **Step 1: 写失败测试**

```rust
// settings_store.rs tests
#[test]
fn the_watchman_preference_defaults_to_on_and_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    assert!(load_watchman_enabled(dir.path()), "default is on");
    save_watchman_enabled(dir.path(), false).unwrap();
    assert!(!load_watchman_enabled(dir.path()));
    // 与 theme 共处一文件且互不覆盖。
    save(dir.path(), Theme::Light).unwrap();
    assert!(!load_watchman_enabled(dir.path()));
    assert_eq!(load(dir.path()), Theme::Light);
}
```

```rust
// server.rs tests：UI 写入 board_watchman_enabled 会触发 watchman 安装/卸载（经注入）
#[tokio::test]
async fn writing_the_watchman_setting_installs_or_uninstalls_it() {
    // 复用既有 Harness（server.rs:5213 附近）。与既有 `launcher` 注入同款：
    // Harness 新增一个记录式 watchman 注入点。
    let mut h = harness().await;
    let calls = h.watchman_calls.clone(); // Arc<Mutex<Vec<(String, bool)>>：("install"|"uninstall", ok)
    rpc(&mut h, 2, "ui/settings/write", json!({ "board_watchman_enabled": false })).await;
    assert_eq!(calls.lock().unwrap().last().unwrap().0, "uninstall");
    rpc(&mut h, 3, "ui/settings/write", json!({ "board_watchman_enabled": true })).await;
    assert_eq!(calls.lock().unwrap().last().unwrap().0, "install");
}
```

> 注：`board_watchman_enabled` 是**宿主级**偏好，写在 `<workdir>/.yi-agent/preferences.json`（与 `theme`、`superpowers_kanban` 同文件）。`ui/settings/write` 目前只认 `theme`，本任务扩展它接受该键。

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent-app-server watchman 2>&1 | tail -30`
Expected: 编译失败（`load_watchman_enabled` / `watchman_calls` 不存在）。

- [ ] **Step 3: 实现**

`settings_store.rs` 增加（沿用同一读-改-写 + 原子替换约定，保留无关键）：

```rust
/// 后台值守/开机自启开关。缺省 `true`。
pub fn load_watchman_enabled(workdir: &Path) -> bool {
    let path = preferences_path(workdir);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.get("board_watchman_enabled").and_then(|v| v.as_bool()))
        .unwrap_or(true)
}

pub fn save_watchman_enabled(workdir: &Path, enabled: bool) -> std::io::Result<()> {
    let dir = workdir.join(".yi-agent");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("preferences.json");
    let mut object = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    object.insert(
        "board_watchman_enabled".to_string(),
        serde_json::Value::Bool(enabled),
    );
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(object))
        .map_err(std::io::Error::other)?;
    let tmp_path = dir.join("preferences.json.tmp");
    std::fs::write(&tmp_path, &text)?;
    std::fs::rename(tmp_path, path)
}
```

`server.rs` 的 `ui/settings/read` 返回值加一个键：

```rust
                        write_response(
                            &hub, &client,
                            ok_response(id, json!({
                                "theme": theme_handle.current().as_str(),
                                "board_watchman_enabled": crate::settings_store::load_watchman_enabled(&workdir),
                            })),
                        ).await?;
```

`ui/settings/write`：在现有 theme 分支之外，若 params 含 `board_watchman_enabled`，写偏好并调用 watchman：

```rust
                    "ui/settings/write" => {
                        if let Some(enabled) = req.params.get("board_watchman_enabled").and_then(|v| v.as_bool()) {
                            crate::settings_store::save_watchman_enabled(&workdir, enabled)
                                .map_err(|e| RpcError::internal(e.to_string()))?;
                            let home = /* workdir 的 home 解析：见下 */;
                            let outcome = if enabled {
                                let exe = std::env::current_exe().map_err(|e| RpcError::internal(e.to_string()))?;
                                watchman_install(&exe, &home)
                            } else {
                                watchman_uninstall(&home)
                            };
                            // 失败不静默：写日志 + 在回复里带 warning，前端展示内联提示。
                            let warning = outcome.err();
                            write_response(&hub, &client, ok_response(id, json!({ "ok": true, "warning": warning }))).await?;
                            continue;
                        }
                        // ... 既有 theme 分支保持不变 ...
                    }
```

`board/create`：成功后确保 watchman 已装（开关为开且未指向当前 exe 时）：

```rust
                    // 在既有 create 成功之后
                    if crate::settings_store::load_watchman_enabled(&workdir) {
                        if let (Ok(exe), Some(home)) = (std::env::current_exe(), home_dir()) {
                            if !yi_agent_boards::watchman::is_installed_for(&home, &exe) {
                                let _ = yi_agent_boards::watchman::install(&exe, &home);
                            }
                        }
                    }
```

为可测，把 watchman 调用集中到两个薄包装 `watchman_install` / `watchman_uninstall`，并在 harness 里注入一个记录器（与既有 launcher 注入同款）。`home_dir()` 用 `std::env::var_os("HOME")`。

- [ ] **Step 4: 运行确认通过**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent-app-server 2>&1 | tail -30`
Expected: PASS（含新测试）。

- [ ] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs yi-agent-rs/crates/yi-agent-app-server/src/settings_store.rs
git commit -m "feat(app-server): watchman setting RPC and install-on-first-board"
```

---

### Task 7: app-server 内循环（B，会话期自愈）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（启动一个后台 tokio 任务）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` tests（测注入的单轮函数）

**Interfaces:**
- Consumes: Task 3 的 `watch::ensure_daemons_with`；Task 1 的 `resident::list`。
- Produces: server 启动时 `tokio::spawn` 一个每 ~30s 的任务；单轮逻辑直接调 `yi_agent_boards::watch::once`（Task 3），**不再另写一份**。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn the_app_side_loop_ensures_every_registered_project() {
    // 单轮逻辑是共享的 `watch::once`（Task 3 已在其自身 crate 内测过）；
    // 这里只断言 app-server 用的是同名同款（覆盖 hooks 见本任务 Step 3）。
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    yi_agent_store::resident::require(dir.path(), &a, "superpowers-kanban").unwrap();
    let mut seen: Vec<PathBuf> = Vec::new();
    let count = yi_agent_boards::watch::once(dir.path(), &mut |projects: &[PathBuf]| {
        seen.extend_from_slice(projects)
    });
    assert_eq!(count, 1);
    assert_eq!(seen, vec![a]);
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent-app-server app_side_loop 2>&1 | tail -20`
Expected: 编译失败。

- [ ] **Step 3: 实现**

```rust
/// 后台自愈循环：app 打开期间每 ~30s 一次，与 watchman 重叠也无害（幂等）。
/// 单轮逻辑复用 `yi_agent_boards::watch::once`（Task 3），不复制一份胶水。
fn spawn_board_watchman_loop() {
    let Some(resident_dir) = yi_agent_store::resident::default_dir() else {
        return;
    };
    tokio::spawn(async move {
        loop {
            yi_agent_boards::watch::once(&resident_dir, &mut |projects| {
                yi_agent_boards::watch::ensure_daemons(projects);
            });
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
}
```

在 server 启动处调一次 `spawn_board_watchman_loop();`（与既有的 `tokio::spawn` 启动点并列）。

- [ ] **Step 4: 运行确认通过**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent-app-server b_loop_once 2>&1 | tail -20`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): session-time self-heal loop for board daemons"
```

---

### Task 8: 插件单实例锁 + daemon 失联即退

**Files:**
- Create: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/single_instance.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/lib.rs`（`pub mod single_instance;`）
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`（`run_daemon` 开头取锁；加 daemon 失联探测）
- Test: 两个文件的 tests

**Interfaces:**
- Produces:
  - `pub struct InstanceLock`；`pub fn acquire(state_dir: &Path) -> Option<InstanceLock>` —— 对 `<state_dir>/plugin.lock` 取 `flock(LOCK_EX|LOCK_NB)`，占用返回 `None`（复用 `lease.rs` 的 flock 手法）。
  - `pub struct Liveness { misses: u32 }`；`Liveness::observe(ok: bool, threshold: u32) -> bool`（返回是否应退出）。

- [ ] **Step 1: 写失败测试**

```rust
// single_instance.rs tests
#[test]
fn a_second_instance_cannot_take_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let first = acquire(dir.path()).expect("first instance takes the lock");
    assert!(acquire(dir.path()).is_none(), "a second instance must be refused");
    drop(first);
    assert!(acquire(dir.path()).is_some(), "released on drop");
}

#[test]
fn a_killed_holder_does_not_deadlock_the_lock() {
    // 与 lease.rs 的 `a_slot_held_by_a_dead_process_is_reclaimed` 同款手法：
    // 把测试二进制自身以 LEASE_HOLD_DIR 重起为持有者，SIGKILL 后轮询到可再取。
    // 这是「孤儿不会把锁占死」的关键属性，必须真杀进程，不能只测 drop。
    let dir = tempfile::tempdir().unwrap();
    let mut holder = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "single_instance::tests::lock_holder_child", "--nocapture"])
        .env("LEASE_HOLD_DIR", dir.path())
        .spawn()
        .expect("re-invoke the test binary as a lock holder");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut held = false;
    while std::time::Instant::now() < deadline {
        if acquire(dir.path()).is_none() {
            held = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(held, "子进程必须在时限内占住锁");

    let status = std::process::Command::new("kill")
        .args(["-9", &holder.id().to_string()])
        .status()
        .expect("kill -9 the holder");
    assert!(status.success(), "kill -9 必须成功");
    let _ = holder.wait(); // 收尸，避免僵尸

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut reclaimed = false;
    while std::time::Instant::now() < deadline {
        if acquire(dir.path()).is_some() {
            reclaimed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(reclaimed, "SIGKILL 之后锁必须可再取，否则孤儿会永久占死推进权");
}

/// 仅在以 LEASE_HOLD_DIR 重起时生效的持有者；正常测试运行立即返回。
#[test]
fn lock_holder_child() {
    let Ok(dir) = std::env::var("LEASE_HOLD_DIR") else {
        return;
    };
    let _lock = acquire(std::path::Path::new(&dir)).expect("holder must take the lock");
    println!("LOCK-HELD");
    std::thread::sleep(std::time::Duration::from_secs(30));
}

#[test]
fn liveness_exits_after_consecutive_misses_only() {
    let mut liveness = Liveness::default();
    assert!(!liveness.observe(true, 3));
    assert!(!liveness.observe(false, 3));
    assert!(!liveness.observe(true, 3), "a success resets the streak");
    assert!(!liveness.observe(false, 3));
    assert!(!liveness.observe(false, 3));
    assert!(liveness.observe(false, 3), "three consecutive misses -> should exit");
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd plugins/superpowers-kanban && cargo test --offline single_instance 2>&1 | tail -20`
Expected: 编译失败（模块不存在）。

- [ ] **Step 3: 实现**

```rust
// single_instance.rs
//! 同一项目同一时刻只允许一个插件推进队列。
//!
//! daemon 崩溃后旧插件会变孤儿（无 daemon 存活探测、也无 setsid）；新 daemon
//! 起来会拉新插件。两个插件同时推进同一队列 = 同一张卡被重复启动。这把锁把
//! 「同时只有一个推进者」变成不变量；孤儿则靠 daemon 失联即退自行让位。

use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

pub struct InstanceLock {
    _file: File,
    path: PathBuf,
}

impl InstanceLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// 取独占锁；已被占用返回 `None`。锁随 fd 关闭由内核释放（含 SIGKILL）。
pub fn acquire(state_dir: &Path) -> Option<InstanceLock> {
    std::fs::create_dir_all(state_dir).ok()?;
    let path = state_dir.join("plugin.lock");
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .ok()?;
    let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if outcome == 0 {
        Some(InstanceLock { _file: file, path })
    } else {
        None
    }
}

/// 连续失联计数：达到阈值就该退出，一次成功即清零。
#[derive(Debug, Default)]
pub struct Liveness {
    misses: u32,
}

impl Liveness {
    /// 记录一次探测结果，返回是否应退出。
    pub fn observe(&mut self, ok: bool, threshold: u32) -> bool {
        if ok {
            self.misses = 0;
            false
        } else {
            self.misses += 1;
            self.misses >= threshold
        }
    }
}
```

`lib.rs` 加：`pub mod single_instance;`

`main.rs` 的 `run_daemon` 开头取锁（拿不到就等——新插件等旧孤儿退出；用轮询，避免忙等）：

```rust
fn run_daemon(args: Args) {
    // 单实例：同一项目同一时刻只有一个插件推进队列。拿不到就等旧实例让位。
    let _lock = loop {
        if let Some(lock) = superpowers_kanban_runner::single_instance::acquire(&args.state_dir) {
            break lock;
        }
        eprintln!("superpowers-kanban: another instance holds the lock; waiting");
        std::thread::sleep(Duration::from_millis(500));
    };
    // ... 既有实现 ...
```

在推进循环里加 daemon 存活探测。**必须放在开关判断之前**：否则开关关闭的孤儿插件
（`continue` 分支）永远发现不了 daemon 已死，就一直留着占锁。

```rust
    let mut liveness = superpowers_kanban_runner::single_instance::Liveness::default();
    loop {
        // 先探 daemon：无论开关开关，孤儿都要有界退出，否则会占着单实例锁不放。
        if liveness.observe(daemon.is_alive(), 3) {
            eprintln!("superpowers-kanban: daemon is gone; exiting so a fresh instance can take over");
            std::process::exit(0);
        }
        if !board_switch(&args.state_dir).is_enabled() {
            std::thread::sleep(args.interval);
            continue;
        }
        // ... 既有每轮逻辑 ...
        std::thread::sleep(args.interval);
    }
```

`client.rs` 增加一个轻量存活探测：

```rust
impl BoardDaemon {
    /// daemon 是否还在应答。最便宜的一次读。
    pub fn is_alive(&self) -> bool {
        superpowers_kanban_ipc::client::send(&self.socket, Command::Status).is_ok()
    }
}
```

> 探测频率复用推进循环的 `--interval-secs`（默认 60s 偏慢；将 `run` 的**默认间隔改为 10s**，并把失联阈值设为 3 → 最多 ~30s 让位）。更新 `USAGE` 文案里的默认值。

- [ ] **Step 4: 运行确认通过**

Run: `cd plugins/superpowers-kanban && cargo test --offline 2>&1 | tail -20`
Expected: PASS（既有 133 + 新测试）。

- [ ] **Step 5: Commit**

```bash
git add plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/single_instance.rs \
        plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/lib.rs \
        plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs \
        plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/client.rs
git commit -m "fix(kanban-plugin): single-instance lock and exit when the daemon is gone"
```

---

### Task 9: 桌面端开关与告知

**Files:**
- Modify: `desktop/src/components/SuperpowersKanbanSettings.tsx`（加「后台值守/开机自启」勾选 + 一次安装告知）
- Modify: `desktop/src/lib/superpowersKanbanSettings.ts`（读/写该设置的封装）
- Modify: `desktop/src/App.tsx`（把设置值接进组件、读写走 rpc）
- Test: 组件的 `.test.tsx` + `App.test.tsx`

**Interfaces:**
- Consumes: `ui/settings/read` 返回的 `board_watchman_enabled`、`ui/settings/write` 接受该键（Task 6）。
- Produces: 组件新 props `watchmanEnabled: boolean`、`onToggleWatchman: (next: boolean) => void`、可选 `watchmanWarning?: string`。

- [ ] **Step 1: 写失败测试**

```tsx
// SuperpowersKanbanSettings.test.tsx
it("toggles the background watchman and explains it", () => {
  const onToggleWatchman = vi.fn();
  render(
    <SuperpowersKanbanSettings
      switchOn source="project" onToggle={() => {}}
      watchmanEnabled onToggleWatchman={onToggleWatchman}
    />,
  );
  const box = screen.getByLabelText("后台值守（开机自启）");
  expect(box).toBeChecked();
  fireEvent.click(box);
  expect(onToggleWatchman).toHaveBeenCalledWith(false);
  expect(screen.getByText(/后台持续运行/)).toBeInTheDocument();
});
```

- [ ] **Step 2: 运行确认失败**

Run: `cd desktop && export PATH="/opt/homebrew/bin:$PATH" && npx vitest run src/components/SuperpowersKanbanSettings.test.tsx 2>&1 | tail -20`
Expected: FAIL（无该复选框）。

- [ ] **Step 3: 实现**

`SuperpowersKanbanSettings.tsx`：在现有「Superpowers 看板」行下新增一段：

```tsx
      <label className="mt-3 flex items-center gap-3 text-sm">
        <input
          type="checkbox"
          checked={watchmanEnabled}
          onChange={(event) => onToggleWatchman(event.target.checked)}
        />
        <span>后台值守（开机自启）</span>
      </label>
      <p className="mt-1 text-xs text-fg-subtle">
        开启后看板将在后台持续运行，电脑重启后自动恢复，无需打开本应用。
      </p>
      {watchmanWarning && (
        <p className="mt-1 text-xs text-amber-500">{watchmanWarning}</p>
      )}
```

props 加进签名（`watchmanEnabled: boolean; onToggleWatchman: (next: boolean) => void; watchmanWarning?: string;`）。

`superpowersKanbanSettings.ts`：加 `readBoardWatchman(rpc): Promise<boolean>`（`ui/settings/read` 取 `board_watchman_enabled`，缺省 true）与 `writeBoardWatchman(rpc, enabled)`（`ui/settings/write`，读取返回里的 `warning`）。

`App.tsx`：启动后读一次该设置，`SuperpowersKanbanSettings` 处传入值；`onToggleWatchman` → `writeBoardWatchman` 后更新本地状态与 warning。

- [ ] **Step 4: 运行确认通过**

Run: `cd desktop && export PATH="/opt/homebrew/bin:$PATH" && npx tsc --noEmit && npx vitest run 2>&1 | tail -20`
Expected: tsc 干净；vitest 全绿（含新用例）。

- [ ] **Step 5: Commit**

```bash
git add desktop/src/components/SuperpowersKanbanSettings.tsx desktop/src/components/SuperpowersKanbanSettings.test.tsx \
        desktop/src/lib/superpowersKanbanSettings.ts desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): expose the background watchman switch with a clear notice"
```

---

### Task 10: 端到端验证 + 文档

**Files:**
- Test: `yi-agent-rs/crates/yi-agent/tests/board_watchman_e2e.rs`（`YI_AGENT_BOARD_E2E=1` 门控，沿用 `board_e2e.rs` 的临时 HOME + 真二进制手法）
- Modify: `docs/superpowers/specs/2026-10-02-board-daemon-watchman-design.md`（状态标为已实施）
- Modify: `.superpowers/sdd/2026-10-02-per-project-kanban-boards/progress.md`（台账追加）

- [ ] **Step 1: 写端到端测试（覆盖 spec §5）**

```rust
// 门控集成测试：临时 HOME + worktree 构建的真 yi-agent。
#[test]
fn killing_a_board_daemon_is_healed_by_the_watchman() {
    if std::env::var("YI_AGENT_BOARD_E2E").as_deref() != Ok("1") { return; }
    // 1. 起真 app-server，board/create 一个临时项目（真 daemon 拉起）
    // 2. 记下 daemon pid；SIGKILL 它
    // 3. 跑一次 `yi-agent boards watch` 的单轮（或用注入 ensure），断言 ~30s 内 daemon 回来
    //    （is_running true）
    // 4. 断言期间同一项目只有一个插件在跑（扫 plugin.lock 只能被一个进程持有）
}

#[test]
fn an_orphan_plugin_exits_after_the_daemon_dies() {
    if std::env::var("YI_AGENT_BOARD_E2E").as_deref() != Ok("1") { return; }
    // 1. 起 daemon + 插件（真）
    // 2. SIGKILL daemon（不带子进程）
    // 3. 断言孤儿插件在有界时间内自行退出（进程消失 / plugin.lock 可再取）
    // 4. 之后再起 daemon → 新插件拿到锁 → 只有一个插件在跑
}
```

- [ ] **Step 2: 运行确认通过**

Run: `cd yi-agent-rs && YI_AGENT_BOARD_E2E=1 TMPDIR=/Users/gongyichen/.yi-agent-tmp cargo test --offline -p yi-agent --test board_watchman_e2e 2>&1 | tail -30`
Expected: PASS（2 个测试）。
未设 `YI_AGENT_BOARD_E2E` 时：`cargo test --offline -p yi-agent --test board_watchman_e2e` → 立即跳过、绿。

- [ ] **Step 3: 全量回归**

Run:
```bash
cd yi-agent-rs && cargo test --offline --workspace 2>&1 | grep -E "^test result|FAILED" | tail -40
cd ../plugins/superpowers-kanban && cargo test --offline 2>&1 | tail -5
cd ../../desktop && export PATH="/opt/homebrew/bin:$PATH" && npx tsc --noEmit && npx vitest run 2>&1 | tail -5
```
Expected: 全绿（已知的 flaky `cancel_returns_unconsumed_interjections_before_cancelled` 若在并行下偶发失败，单独复跑确认与本次无关）。

- [ ] **Step 4: 更新文档与台账**

- spec 顶部状态改为「已实施并通过验收」，§5 各条标注证据（命令 + 结果）。
- 台账追加：Task 1–10 的提交、测试数字、偏差，以及**本次仍不做的**（TUI 侧 B、Linux/Windows 自启、开机自启管理 UI、孤儿即时感知的 kqueue 优化）。

- [ ] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/tests/board_watchman_e2e.rs \
        docs/superpowers/specs/2026-10-02-board-daemon-watchman-design.md
git commit -m "test(boards): end-to-end watchman and orphan-plugin verification"
```

---

## Self-Review

**Spec 覆盖：**
- §4.1 通用常驻登记 → Task 1（`resident`）+ Task 2（看板写入）。
- §4.2 watchman `boards watch` → Task 4。
- §4.3 `ensure_daemons` → Task 3。
- §4.4 LaunchAgent 安装/卸载/开关 → Task 5（机制）+ Task 6（接入与开关）+ Task 9（UI 告知）。
- §4.5 app 侧自愈 B → Task 7。
- §4.6 插件单实例锁 + 失联即退 → Task 8。
- §4.7 重启后对账 → 复用既有 Task 8（全局额度）的 `reconcile_running`；Task 10 用集成测试验证「重启后停在 Running 的卡被对账移走并释放名额」。
- §5 验收 1–8 → Task 10 的端到端覆盖 1/2/3/4/7，Task 6/9 覆盖 5/6，Task 10 Step 3 覆盖 8。

**类型一致性：** `resident::{require, release, list, default_dir}`（Task 1）在 Task 2/4/7 一致引用；`watch::ensure_daemons{,_with}`（Task 3）在 Task 4/7 一致；`watchman::{PREFERENCE_KEY, LABEL, plist_path, plist_contents, install{,_with}, uninstall{,_with}, is_installed{,_for}}`（Task 5）在 Task 6/9 一致；`single_instance::{acquire, InstanceLock, Liveness}`（Task 8）在 main.rs 一致。

**签名变更需同步的调用点（有意让其编译失败以强制更新）：** Task 2 给 `create_with_project`/`remove_with` 加 `resident_dir` 形参——涉及 app-server、tui/board、各测试；Task 8 把插件 `run` 默认间隔由 60s 改为 10s——涉及 `USAGE` 与既有解析测试。

**已知取舍：** 孤儿插件是「有界退出」而非「即时感知」；让位窗口 ≈ 3×探测间隔（约 30s）。期间旧插件已无法连上死掉的 daemon，推进循环只会失败重试，**不会重复启动卡片**；新插件在窗口内取锁失败会等待。这是可接受的简单实现，kqueue 即时感知列为非目标。
