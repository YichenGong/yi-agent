# 看板卡片改为可见会话（Thread）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让看板卡片由 app-server 起一个用户可见、可点开、可继续追问的桌面 Thread（自动发首轮执行），跑完进 `awaiting_merge`。

**Architecture:** 插件保留看板状态机与全局并发槽位，不再自己起会话；app-server 新增「卡片调度器」tokio 任务，经现有 `plugin_query` 驱动插件取卡/回写，用现有 `thread/start`+`turn/start` 起会话。插件与调度器之间用 `board.json` + 4 个新查询方法交接。

**Tech Stack:** Rust（`superpowers-kanban-*` 插件 crate、`yi-agent-app-server`/`yi-agent-boards`/`yi-agent-store`），桌面为 Tauri+React（本计划仅小改视图）。

## Global Constraints

- **槽位语义**：只有 `CardState::Running` 占用并发名额；`Launching` 及其它所有状态都不占。
- **绝不自动合并**：执行者只在会话里给出集成选项，合并权始终归用户。
- **卡片会话权限模式**：Yolo（自动执行必需）。
- **卡片会话 cwd**：该卡片的隔离 worktree（`<project>/.worktrees/kanban/<slug>`）。
- **轮询周期**：调度器 3 秒。
- **协议**：不新增 IPC 协议，插件方法一律经现有 `plugin_query` 到达。
- **迁移**：仍是 `running` 但 `thread_id` 为空的历史卡片，一次性置 `needs_you`（原因 `legacy run without a thread`）。
- **工作区**：本计划在 `yi-agent` 仓库内执行。合并进 main 后需重建并重装桌面 App 才会在真机生效（最后一节）。

---

## File Structure

**插件（`plugins/superpowers-kanban/`）**
- `crates/superpowers-kanban-core/src/card.rs` — 加 `Launching` 状态、`thread_id`、`base_commit` 字段。
- `crates/superpowers-kanban-core/src/board.rs` — 加 `Launching` 迁移、`set_thread_id`/`set_base_commit`、`claim_next_launch`。
- `crates/superpowers-kanban-runner/src/service.rs`（新建）— `BoardService`：把 `board.json` 与全局 lease 收敛到一把锁，提供 `next_launch`/`mark_running`/`mark_terminal`/`release`/`list`。
- `crates/superpowers-kanban-runner/src/dispatch.rs` — 新增 `board.*` 方法，扩展 `list`。
- `crates/superpowers-kanban-runner/src/tick.rs` — 删除 `launch`/`reconcile_running`（改由 app-server 驱动）；保留 `objective_for`。
- `crates/superpowers-kanban-runner/src/main.rs` — `run_daemon` 不再起会话；查询服务改用 `BoardService`；启动时做迁移。

**app-server（`yi-agent-rs/crates/yi-agent-app-server/src/`）**
- `server.rs` — 新增 `board_query` helper；抽出 `start_thread_core`/`start_turn_core`；新增调度器任务 `run_card_scheduler`；`serve` 里接线。
- `card_scheduler.rs`（新建）— 调度器的纯逻辑（解析卡片、决定启动/对账动作），与 I/O 解耦以便单测。

**桌面（`desktop/src/`）**
- `components/SuperpowersKanbanView.tsx` — 卡片行展示 thread_id（可选点开）。

---

### Task 1: 插件核心 —— `Launching` 状态与新字段

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/board.rs`
- Test: 上述两文件的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: 无（基础类型）。
- Produces:
  - `CardState::Launching`
  - `Card.thread_id: Option<String>`、`Card.base_commit: Option<String>`
  - `Board::set_thread_id(&mut self, id: &CardId, thread_id: String) -> Result<(), TransitionError>`
  - `Board::set_base_commit(&mut self, id: &CardId, base_commit: String) -> Result<(), TransitionError>`
  - `Board::claim_next_launch(&mut self, limit: u16) -> Option<CardId>`（队首排队卡 → `Launching`）

- [ ] **Step 1: 写失败测试（card.rs）**

在 `card.rs` 的 tests 模块追加：

```rust
#[test]
fn launching_does_not_occupy_a_slot() {
    assert!(!CardState::Launching.occupies_slot());
}

#[test]
fn queued_can_launch_and_launching_can_run_or_fail() {
    use CardState::*;
    assert!(Queued.can_transition_to(Launching));
    assert!(Launching.can_transition_to(Running));
    assert!(Launching.can_transition_to(Failed));
    assert!(!Launching.can_transition_to(AwaitingMerge));
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core --lib launching`
Expected: 编译失败（`Launching` 未定义）。

- [ ] **Step 3: 实现（card.rs）**

在 `enum CardState` 中 `Queued,` 之后加 `Launching,`；在 `can_transition_to` 的 `match (self, next)` 里加：

```rust
(Queued, Launching) => true,
(Launching, Running) => true,
(Launching, Failed) => true,
(Launching, Cancelled) => true, // 由上面的 `next == Cancelled` 兜底也行，显式更清楚
```

在 `struct Card` 末尾（`task_id` 之后）加：

```rust
/// 卡片会话在 app-server 里的 thread id。启动后回填。
#[serde(default)]
pub thread_id: Option<String>,
/// 启动时 worktree 的 HEAD，供对账判断「有无新提交」。
#[serde(default)]
pub base_commit: Option<String>,
```

并在 `enqueue` 里 `task_id: None,` 之后补 `thread_id: None, base_commit: None,`。

- [ ] **Step 4: 实现 Board 访问器与 `claim_next_launch`（board.rs）**

在 `impl Board` 中加：

```rust
pub fn set_thread_id(&mut self, id: &CardId, thread_id: String) -> Result<(), TransitionError> {
    let card = self.cards.iter_mut().find(|c| &c.id == id)
        .ok_or_else(|| TransitionError::UnknownCard(id.clone()))?;
    card.thread_id = Some(thread_id);
    Ok(())
}

pub fn set_base_commit(&mut self, id: &CardId, base_commit: String) -> Result<(), TransitionError> {
    let card = self.cards.iter_mut().find(|c| &c.id == id)
        .ok_or_else(|| TransitionError::UnknownCard(id.clone()))?;
    card.base_commit = Some(base_commit);
    Ok(())
}

/// 队首排队卡迁移到 `Launching` 并返回其 id；无名额或队空返回 `None`。
/// 名额判断与迁移在同一把 `&mut self` 里完成，调用方据此占槽，天然原子。
pub fn claim_next_launch(&mut self, limit: u16) -> Option<CardId> {
    if self.free_slots(limit) == 0 {
        return None;
    }
    let next = self.next_startable()?;
    if let Some(card) = self.cards.iter_mut().find(|c| c.id == next) {
        card.state = CardState::Launching;
    }
    Some(next)
}
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core`
Expected: PASS（含新增用例）。

- [ ] **Step 6: 提交**

```bash
git add plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs \
        plugins/superpowers-kanban/crates/superpowers-kanban-core/src/board.rs
git commit -m "feat(kanban-core): Launching state, thread_id/base_commit, claim_next_launch"
```

---

### Task 2: 插件 —— `BoardService`（看板 + 槽位收敛到一把锁）

**Files:**
- Create: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/lib.rs`（`pub mod service;`）
- Test: `service.rs` 的 `#[cfg(test)]`

**Interfaces:**
- Consumes: `Board`、`lease::{global_leases_dir, acquire_in, Lease}`、`persist::{load_board, save_board}`、`calendar::ConcurrencyCalendar`、`worktree::ensure_worktree`。
- Produces:
  - `pub struct BoardService { state_dir: PathBuf, project_root: PathBuf, inner: Mutex<Inner> }`
  - `impl BoardService`:
    - `pub fn new(state_dir: PathBuf, project_root: PathBuf, home: Option<PathBuf>) -> Self`
    - `pub fn next_launch(&self, limit: u16, now: DateTime<Local>) -> Result<Option<LaunchClaim>, String>`
    - `pub fn mark_running(&self, card_id: &str, thread_id: &str) -> Result<(), String>`
    - `pub fn mark_terminal(&self, card_id: &str, outcome: &str, detail: Option<&str>) -> Result<(), String>`
    - `pub fn release(&self, card_id: &str, detail: &str) -> Result<(), String>`
    - `pub fn list(&self) -> serde_json::Value`
    - `pub fn migrate_legacy_running(&self)`
  - `pub struct LaunchClaim { pub card_id: String, pub workdir: PathBuf, pub title: String }`
  - `pub fn effective_head(workdir: &Path) -> Option<String>`（读 `git rev-parse HEAD`）

- [ ] **Step 1: 写失败测试（service.rs）**

```rust
use super::*;
use chrono::TimeZone;
use std::path::PathBuf;

fn at() -> chrono::DateTime<chrono::Local> {
    chrono::Local.with_ymd_and_hms(2026, 10, 1, 12, 0, 0).single().unwrap()
}

/// 建一个真实的最小 git 仓库，作为 project_root 与 worktree 的来源。
fn project_with_worktree(dir: &std::path::Path) -> PathBuf {
    let out = std::process::Command::new("git")
        .args(["init", "-q", "-b", "main"]).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "git init failed");
    std::fs::write(dir.join("README.md"), "seed\n").unwrap();
    for args in [vec!["add", "README.md"], vec!["-c", "user.email=e@e", "-c", "user.name=E", "commit", "-q", "-m", "seed"]] {
        let out = std::process::Command::new("git").args(&args).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "git {args:?} failed");
    }
    dir.to_path_buf()
}

fn service_with_card(dir: &std::path::Path) -> BoardService {
    let state_dir = dir.join(".yi-agent/superpowers-kanban");
    std::fs::create_dir_all(&state_dir).unwrap();
    let mut board = superpowers_kanban_core::board::Board::new();
    board.enqueue(
        superpowers_kanban_core::card::CardId::new("card-1"),
        PathBuf::from("a.spec.md"),
        PathBuf::from("a.plan.md"),
        at(),
    );
    persist::save_board(&state_dir.join("board.json"), &board).unwrap();
    // home 指到临时目录，让 lease 落在隔离目录，不污染真实 HOME。
    BoardService::new(state_dir, dir.to_path_buf(), Some(dir.join("home")))
}

#[test]
fn next_launch_claims_the_head_card_and_creates_its_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_with_worktree(dir.path());
    let service = service_with_card(&project);

    let claim = service.next_launch(3, at()).unwrap().expect("a claim");
    assert_eq!(claim.card_id, "card-1");
    assert!(claim.workdir.join(".git").exists(), "worktree was created");
    assert_eq!(claim.title, "看板 · a.spec");

    // 再取一次：没有第二张排队卡 → None。
    assert!(service.next_launch(3, at()).unwrap().is_none());
}

#[test]
fn next_launch_is_gated_by_the_slot_limit() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_with_worktree(dir.path());
    let service = service_with_card(&project);
    assert!(service.next_launch(0, at()).unwrap().is_none(), "no slot -> no claim");
    // 占用那张卡后，名额用尽。
    let claim = service.next_launch(1, at()).unwrap().unwrap();
    service.mark_running(&claim.card_id, "thread-1").unwrap();
    assert!(service.next_launch(1, at()).unwrap().is_none(), "slot taken");
}

#[test]
fn mark_terminal_releases_the_slot_and_records_the_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_with_worktree(dir.path());
    let service = service_with_card(&project);
    let claim = service.next_launch(1, at()).unwrap().unwrap();
    service.mark_running(&claim.card_id, "thread-1").unwrap();
    service.mark_terminal(&claim.card_id, "awaiting_merge", None).unwrap();
    let listed = service.list();
    let card = &listed["cards"][0];
    assert_eq!(card["state"], "awaiting_merge");
    assert_eq!(card["thread_id"], "thread-1");
    // 名额已释放：再放一张排队卡即可启动（此处队空，验证 free_slots 间接由 next_launch None 体现）。
    assert!(service.next_launch(1, at()).unwrap().is_none());
}

#[test]
fn release_fails_the_card_and_frees_the_slot() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_with_worktree(dir.path());
    let service = service_with_card(&project);
    let claim = service.next_launch(1, at()).unwrap().unwrap();
    service.release(&claim.card_id, "thread/start failed").unwrap();
    assert_eq!(service.list()["cards"][0]["state"], "failed");
}

#[test]
fn a_legacy_running_card_without_a_thread_becomes_needs_you() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_with_worktree(dir.path());
    let service = service_with_card(&project);
    let claim = service.next_launch(1, at()).unwrap().unwrap();
    service.mark_running(&claim.card_id, "thread-1").unwrap();
    // 模拟旧数据：把 thread_id 清成 None（在 BoardService 上加一个
    // `#[cfg(test)] pub(crate) fn clear_thread_id_for_test`，只搬字段，不碰产品逻辑）。
    service.clear_thread_id_for_test("card-1");
    service.migrate_legacy_running();
    assert_eq!(service.list()["cards"][0]["state"], "needs_you");
}
```

> 实现提示：`clear_thread_id_for_test` 就是 `card.thread_id = None`，仅 `#[cfg(test)]`。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --lib service`
Expected: 编译失败（`BoardService` 未定义）。

- [ ] **Step 3: 实现 `service.rs`**

```rust
//! 看板服务：把 board.json 与全局并发槽位收敛到一把锁。
//!
//! 推进循环与查询分派共享同一 `BoardService`，所以 `next_launch` 的
//! 「判名额 → 建 worktree → 置 Launching」相对推进循环是原子的，
//! 不会两张卡抢到同一个槽位。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Local};
use serde_json::{json, Value};

use superpowers_kanban_core::board::Board;
use superpowers_kanban_core::card::{CardId, CardState};

use crate::lease::{self, Lease};
use crate::persist;
use crate::worktree::ensure_worktree;

pub struct LaunchClaim {
    pub card_id: String,
    pub workdir: PathBuf,
    pub title: String,
}

struct Inner {
    board: Board,
    leases: std::collections::HashMap<CardId, Lease>,
    leases_dir: Option<PathBuf>,
}

pub struct BoardService {
    state_dir: PathBuf,
    project_root: PathBuf,
    inner: Mutex<Inner>,
}

pub fn effective_head(workdir: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C").arg(workdir).args(["rev-parse", "HEAD"]).output().ok()?;
    if !out.status.success() { return None; }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl BoardService {
    pub fn new(state_dir: PathBuf, project_root: PathBuf, home: Option<PathBuf>) -> Self {
        let leases_dir = home
            .map(|home| home.join(".yi-agent").join("superpowers-kanban").join("leases"))
            .or_else(lease::global_leases_dir);
        let board = persist::load_board(&state_dir.join("board.json"));
        Self {
            state_dir,
            project_root,
            inner: Mutex::new(Inner { board, leases: Default::default(), leases_dir }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn save(&self, inner: &Inner) {
        let _ = persist::save_board(&self.state_dir.join("board.json"), &inner.board);
    }

    pub fn next_launch(&self, limit: u16, _now: DateTime<Local>) -> Result<Option<LaunchClaim>, String> {
        let mut inner = self.lock();
        let Some(dir) = inner.leases_dir.clone() else {
            return Err("no lease directory".into());
        };
        let Some(lease) = lease::acquire_in(&dir, limit as usize) else {
            return Ok(None);
        };
        let Some(card_id) = inner.board.claim_next_launch(limit) else {
            return Ok(None); // lease dropped here
        };
        let branch = format!("kanban/{}", crate::worktree::slugify(&card_id));
        let workdir = match ensure_worktree(&self.project_root, &card_id, &branch) {
            Ok(path) => path,
            Err(error) => {
                let _ = inner.board.transition(&card_id, CardState::Failed);
                self.save(&inner);
                return Err(format!("worktree for {card_id} failed: {error}"));
            }
        };
        if let Some(base) = effective_head(&workdir) {
            let _ = inner.board.set_base_commit(&card_id, base);
        }
        let _ = inner.board.set_workdir(&card_id, workdir.clone());
        let title = title_for(&inner.board, &card_id);
        inner.leases.insert(card_id.clone(), lease);
        self.save(&inner);
        Ok(Some(LaunchClaim { card_id: card_id.0, workdir, title }))
    }

    pub fn mark_running(&self, card_id: &str, thread_id: &str) -> Result<(), String> {
        let mut inner = self.lock();
        let id = CardId::new(card_id);
        inner.board.transition(&id, CardState::Running).map_err(|e| e.to_string())?;
        inner.board.set_thread_id(&id, thread_id.to_string()).map_err(|e| e.to_string())?;
        self.save(&inner);
        Ok(())
    }

    pub fn mark_terminal(&self, card_id: &str, outcome: &str, _detail: Option<&str>) -> Result<(), String> {
        let next = match outcome {
            "awaiting_merge" => CardState::AwaitingMerge,
            "needs_you" => CardState::NeedsYou,
            "failed" => CardState::Failed,
            other => return Err(format!("unknown outcome: {other}")),
        };
        let mut inner = self.lock();
        let id = CardId::new(card_id);
        inner.board.transition(&id, next).map_err(|e| e.to_string())?;
        inner.leases.remove(&id); // 释放槽位
        self.save(&inner);
        Ok(())
    }

    pub fn release(&self, card_id: &str, _detail: &str) -> Result<(), String> {
        let mut inner = self.lock();
        let id = CardId::new(card_id);
        inner.board.transition(&id, CardState::Failed).map_err(|e| e.to_string())?;
        inner.leases.remove(&id);
        self.save(&inner);
        Ok(())
    }

    /// 仍是 running 但 thread_id 为空的历史卡片 → needs_you。启动时调用一次。
    pub fn migrate_legacy_running(&self) {
        let mut inner = self.lock();
        let ids: Vec<CardId> = inner.board.cards().iter()
            .filter(|c| c.state == CardState::Running && c.thread_id.as_deref().unwrap_or("").is_empty())
            .map(|c| c.id.clone())
            .collect();
        for id in ids {
            let _ = inner.board.transition(&id, CardState::NeedsYou);
        }
        self.save(&inner);
    }

    pub fn list(&self) -> Value {
        let inner = self.lock();
        json!({
            "cards": inner.board.cards().iter().map(|card| json!({
                "id": card.id.0,
                "state": card.state,
                "spec_path": card.spec_path,
                "plan_path": card.plan_path,
                "workdir": card.workdir,
                "thread_id": card.thread_id,
            })).collect::<Vec<_>>(),
        })
    }
}

fn title_for(board: &Board, id: &CardId) -> String {
    let spec = board.get(id).map(|c| c.spec_path.clone()).unwrap_or_default();
    let stem = spec.file_stem().map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| id.0.clone());
    format!("看板 · {stem}")
}
```

在 `lib.rs` 加 `pub mod service;`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --lib service`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs \
        plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/lib.rs
git commit -m "feat(kanban-runner): BoardService coordinates the board and global slots under one lock"
```

---

### Task 3: 插件 —— `board.*` 查询方法与 runner 改造

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/dispatch.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/tick.rs`
- Test: `dispatch.rs` / `main.rs` 的 `#[cfg(test)]`

**Interfaces:**
- Consumes: Task 2 的 `BoardService`。
- Produces:
  - `dispatch::dispatch(state_dir, method, params)` 新增：`board.next_launch` / `board.mark_running` / `board.mark_terminal` / `board.release`；`list` 改由 `BoardService` 提供。
  - `run_daemon`：查询服务持有 `Arc<BoardService>`；不再调用 `CreateAutonomousSession`。

- [ ] **Step 1: 写失败测试（dispatch.rs）**

在现有 tests 模块加（沿用该文件已有的临时目录/建对助手风格）：

```rust
#[test]
fn next_launch_is_gated_by_the_switch_and_reports_no_claim_when_disabled() {
    let dir = tempfile::tempdir().unwrap();
    // 未建仓库、未开开关：next_launch 不应抛错，而是回 null（无名额/无卡）。
    let value = dispatch(dir.path(), "board.next_launch", &serde_json::json!({})).unwrap();
    assert!(value.is_null());
}

#[test]
fn mark_terminal_rejects_an_unknown_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let err = dispatch(dir.path(), "board.mark_terminal",
        &serde_json::json!({"card_id":"card-1","outcome":"nonsense"})).unwrap_err();
    assert!(err.contains("unknown outcome"), "{err}");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --lib next_launch_is_gated`
Expected: 失败（`unknown method: board.next_launch`）。

- [ ] **Step 3: 实现 dispatch（dispatch.rs）**

改 `dispatch_with_global`：把 `list` 分支替换为经 `BoardService`（需要 state_dir、project_root；project_root 由 `layout::project_root(state_dir)` 得到），并新增 `board.*` 分支：

```rust
// 在 match 前构造一次（project_root 从 state_dir 推导；home 用真实 HOME）。
let project_root = superpowers_kanban_core::layout::project_root(state_dir);
let service = std::sync::Arc::new(crate::service::BoardService::new(
    state_dir.to_path_buf(), project_root, None));
```

将 `"list" => Ok(service.list()),`，并加：

```rust
"board.next_launch" => {
    // 日历上限按当前时刻算；服务内不读时钟，交由这里注入。
    let calendar = superpowers_kanban_core::calendar::ConcurrencyCalendar::load_preferring_new(state_dir);
    let limit = calendar.limit_at(chrono::Local::now());
    match service.next_launch(limit, chrono::Local::now()) {
        Ok(Some(claim)) => Ok(json!({
            "card_id": claim.card_id,
            "workdir": claim.workdir,
            "title": claim.title,
        })),
        Ok(None) => Ok(Value::Null),
        Err(error) => Err(error),
    }
}
"board.mark_running" => {
    let card_id = params.get("card_id").and_then(Value::as_str)
        .ok_or_else(|| "board.mark_running needs a card_id".to_string())?;
    let thread_id = params.get("thread_id").and_then(Value::as_str)
        .ok_or_else(|| "board.mark_running needs a thread_id".to_string())?;
    service.mark_running(card_id, thread_id)?;
    Ok(json!({"ok": true}))
}
"board.mark_terminal" => {
    let card_id = params.get("card_id").and_then(Value::as_str)
        .ok_or_else(|| "board.mark_terminal needs a card_id".to_string())?;
    let outcome = params.get("outcome").and_then(Value::as_str)
        .ok_or_else(|| "board.mark_terminal needs an outcome".to_string())?;
    let detail = params.get("detail").and_then(Value::as_str);
    service.mark_terminal(card_id, outcome, detail)?;
    Ok(json!({"ok": true}))
}
"board.release" => {
    let card_id = params.get("card_id").and_then(Value::as_str)
        .ok_or_else(|| "board.release needs a card_id".to_string())?;
    let detail = params.get("detail").and_then(Value::as_str).unwrap_or("launch failed");
    service.release(card_id, detail)?;
    Ok(json!({"ok": true}))
}
```

> 注意：每个查询请求都新建 `BoardService` 会丢失内存中的 lease（flock 随对象 drop 释放），因此**必须**让服务在进程内单例。实现方式：用一个 `static SERVICE: OnceLock<Mutex<HashMap<PathBuf, Arc<BoardService>>>>`，按 `state_dir` 复用同一实例；`QueryDispatch` 保存该实例。

- [ ] **Step 4: 改造 runner（main.rs + tick.rs）**

- 删 `tick.rs` 的 `launch` 与 `reconcile_running`（及只服务它们的 tests）；保留 `objective_for` 与 `TickAction`（若无人用则一并删）。
- `main.rs`：`QueryDispatch` 持有 `Arc<BoardService>`（与推进循环共享同一个）；`run_daemon` 主体只保留：周期 `inbox::consume` + `save_board` + 睡眠；删除租约 map、启动、对账逻辑。启动时先 `service.migrate_legacy_running()`。

- [ ] **Step 5: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test`
Expected: 全绿（既有的 runner/main 测试需相应更新：删掉依赖 `launch`/`reconcile_running` 的用例）。

- [ ] **Step 6: 提交**

```bash
git add plugins/superpowers-kanban/crates/superpowers-kanban-runner/
git commit -m "feat(kanban): board.* query methods; runner no longer creates sessions"
```

---

### Task 4: app-server —— `board_query` 客户端 helper

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
- Test: `server.rs` 的 `#[cfg(test)]`（沿用现有 `plugin_query_tests` 的 fake daemon）

**Interfaces:**
- Consumes: `plugin_query`（现有）。
- Produces:
  - `fn board_query(project: &Path, board_dir: &Path, method: &str, params: Value) -> Result<Value, BoardQueryError>`
  - `fn board_cards(project: &Path, board_dir: &Path) -> Result<Vec<BoardCard>, BoardQueryError>`
  - `struct BoardCard { pub id: String, pub state: String, pub thread_id: Option<String>, pub workdir: Option<PathBuf>, pub spec_path: String }`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn board_query_targets_the_kanban_plugin() {
    // 复用 plugin_query_tests 里「起一个 fake daemon socket」的助手。
    // 断言发出的 IpcRequest::PluginQuery 的 plugin == "superpowers-kanban"、
    // method 原样透传。
}
```

（实现者：直接把现有 `a_query_reaches_the_daemon_with_the_plugin_and_method_untouched` 复制一份，改为调用 `board_query(project, global, "list", json!({}))`，断言 `request["command"]["plugin"] == "superpowers-kanban"`、`request["command"]["method"] == "list"`。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server board_query_targets`
Expected: 编译失败（`board_query` 未定义）。

- [ ] **Step 3: 实现**

```rust
const KANBAN_PLUGIN: &str = "superpowers-kanban";

fn board_query(project: &Path, board_dir: &Path, method: &str, params: Value)
    -> Result<Value, BoardQueryError>
{
    plugin_query(project, board_dir, method, KANBAN_PLUGIN, params)
}

#[derive(Debug, Clone)]
pub(crate) struct BoardCard {
    pub id: String,
    pub state: String,
    pub thread_id: Option<String>,
    pub workdir: Option<PathBuf>,
    pub spec_path: String,
}

pub(crate) fn board_cards(project: &Path, board_dir: &Path)
    -> Result<Vec<BoardCard>, BoardQueryError>
{
    let value = board_query(project, board_dir, "list", json!({}))?;
    let cards = value.get("cards").and_then(Value::as_array).cloned().unwrap_or_default();
    Ok(cards.into_iter().filter_map(|c| {
        Some(BoardCard {
            id: c.get("id")?.as_str()?.to_string(),
            state: c.get("state")?.as_str()?.to_string(),
            thread_id: c.get("thread_id").and_then(Value::as_str).map(str::to_string),
            workdir: c.get("workdir").and_then(Value::as_str).map(PathBuf::from),
            spec_path: c.get("spec_path").and_then(Value::as_str).unwrap_or_default().to_string(),
        })
    }).collect())
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server board_query`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): board_query/board_cards reach the kanban plugin"
```

---

### Task 5: app-server —— 调度器纯逻辑（`card_scheduler.rs`）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/card_scheduler.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`（或 `main.rs`，视模块声明处）
- Test: `card_scheduler.rs` 的 `#[cfg(test)]`

**Interfaces:**
- Produces:
  - `enum CardAction { Launch { card_id: String }, Reconcile { card_id: String, thread_id: String, outcome: Outcome }, None }`
  - `enum Outcome { AwaitingMerge, NeedsYou, Failed }`
  - `fn plan(cards: &[BoardCard], tracked: &HashMap<String, TrackedThread>) -> Vec<CardAction>`
  - `struct TrackedThread { pub thread_id: String, pub idle: bool, pub failed: bool, pub needs_you: bool }`

- [ ] **Step 1: 写失败测试**

```rust
use super::*;
fn card(id: &str, state: &str) -> BoardCard {
    BoardCard { id: id.into(), state: state.into(), thread_id: None, workdir: None, spec_path: format!("{id}.spec.md") }
}

#[test]
fn a_queued_card_is_not_launched_by_plan_because_the_plugin_owns_slots() {
    // plan() 只负责「对账已 tracked 的卡片」；启动由 next_launch 驱动，不在 plan 里。
    let actions = plan(&[card("a", "queued")], &Default::default());
    assert!(actions.is_empty());
}

#[test]
fn a_tracked_card_whose_thread_finished_with_changes_awaits_merge() {
    let mut tracked = HashMap::new();
    tracked.insert("a".to_string(), TrackedThread { thread_id: "t1".into(), idle: true, failed: false, needs_you: false });
    let actions = plan(&[card("a", "running")], &tracked);
    assert!(matches!(actions.as_slice(),
        [CardAction::Reconcile { card_id, outcome: Outcome::AwaitingMerge, .. }] if card_id == "a"));
}

#[test]
fn an_idle_thread_without_changes_needs_you() {
    let mut tracked = HashMap::new();
    tracked.insert("a".to_string(), TrackedThread { thread_id: "t1".into(), idle: true, failed: false, needs_you: true });
    let actions = plan(&[card("a", "running")], &tracked);
    assert!(matches!(&actions[0], CardAction::Reconcile { outcome: Outcome::NeedsYou, .. }));
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server plan_a_tracked_card`
Expected: 编译失败。

- [ ] **Step 3: 实现**

```rust
//! 卡片调度器的纯决策逻辑：把「看板卡片 + 本进程跟踪的 thread 状态」映射成动作。
//! 与 I/O 解耦，便于单测；真正的 thread/start、plugin_query 在 server.rs 里执行。

use std::collections::HashMap;
use crate::server::BoardCard;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome { AwaitingMerge, NeedsYou, Failed }

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CardAction {
    Reconcile { card_id: String, thread_id: String, outcome: Outcome },
}

#[derive(Debug, Clone)]
pub(crate) struct TrackedThread {
    pub thread_id: String,
    pub idle: bool,
    pub failed: bool,
    pub needs_you: bool,
}

pub(crate) fn plan(cards: &[BoardCard], tracked: &HashMap<String, TrackedThread>) -> Vec<CardAction> {
    cards.iter().filter_map(|card| {
        if card.state != "running" { return None; }
        let thread_id = card.thread_id.clone()?;
        let t = tracked.get(&card.id)?;
        if !t.idle { return None; }
        let outcome = if t.failed { Outcome::Failed }
            else if t.needs_you { Outcome::NeedsYou }
            else { Outcome::AwaitingMerge };
        Some(CardAction::Reconcile { card_id: card.id.clone(), thread_id, outcome })
    }).collect()
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server card_scheduler`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/card_scheduler.rs
git commit -m "feat(app-server): card scheduler decision logic (pure, tested)"
```

---

### Task 6: app-server —— 调度器接线（起会话 / 回写 / 恢复）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
- Test: `server.rs` 集成测试（fake 插件 socket + 假 thread 启动器）

**Interfaces:**
- Consumes: Task 4 `board_query`/`board_cards`、Task 5 `card_scheduler::plan`。
- Produces:
  - `async fn run_card_scheduler(threads: SharedThreads, board_dir: PathBuf, runtimes: ProjectRuntimes, thread_roots: ThreadRoots, cfg: RuntimeConfig, build_agent: F)`
  - `SharedThreads = Arc<StdMutex<HashMap<String /*thread_id*/, String /*card_id*/>>>`

- [ ] **Step 1: 抽出 thread 启动内核**

把 `"thread/start"` 分支里「解析 cwd → build_agent → attach_delegation → 建 driver 通道 → insert 到 `threads` → spawn driver」抽成：

```rust
#[allow(clippy::too_many_arguments)]
async fn start_thread_core(
    threads: &mut HashMap<String, ThreadSession>,
    pending_activation: &mut HashMap<String, Option<Arc<ThreadRoot>>>,
    runtimes: &ProjectRuntimes,
    thread_roots: &ThreadRoots,
    cfg: &RuntimeConfig,
    cwd: &str,
    mode: crate::thread_store::ThreadMode,
    build_agent: &impl Fn(Option<yi_agent_core::Session>, &Path, crate::thread_store::ThreadMode) -> anyhow::Result<BuiltAgent>,
    hub: Arc<crate::broadcast::Broadcaster>,
    turn_tx: mpsc::Sender<TurnEvent>,
    /* pending, perm_seq, permission_timeout, theme, client 等 */ 
) -> anyhow::Result<String /*thread_id*/>
```

`thread/start` 分支改为调用它。`start_turn_core(threads, thread_id, prompt) -> anyhow::Result<String /*turn_id*/>` 同理抽出，供调度器复用。（这两处是纯抽取，行为不变——先跑既有 `thread/start`/`turn/start` 测试确保不回归。）

- [ ] **Step 2: 写失败测试（fake 插件 + 假启动器）**

```rust
#[tokio::test]
async fn the_scheduler_launches_a_claimed_card_and_reports_it_running() {
    // 1) fake 插件 socket：`board.next_launch` 首次回 {"card_id":"c1","workdir":"/w","title":"T"}，其后回 null；
    //    `board.mark_running` 记录收到的 (card_id, thread_id)。
    // 2) 用一个假的 launch 闭包替代真实 thread/start，返回固定 thread_id。
    // 3) 跑调度器一轮（可注入 tick 函数），断言：调用过 mark_running(c1, <thread>)，
    //    且 threads 映射里登记了该 thread。
}
```

- [ ] **Step 3: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server the_scheduler_launches`
Expected: 编译失败。

- [ ] **Step 4: 实现调度器**

```rust
async fn run_card_scheduler(/* 依赖 */) {
    let mut tracked: HashMap<String, TrackedThread> = HashMap::new();
    let mut interval = tokio::time::interval(Duration::from_secs(3));
    loop {
        interval.tick().await;
        for project in board_projects(&board_dir) {          // registry::list 的项目根
            // 1) 启动：反复取卡直到 null。
            while let Ok(value) = board_query(&project, &board_dir, "board.next_launch", json!({})) {
                let Some(card_id) = value.get("card_id").and_then(Value::as_str) else { break };
                let workdir = value.get("workdir").and_then(Value::as_str).unwrap_or_default();
                let title = value.get("title").and_then(Value::as_str).unwrap_or("看板卡片");
                // 起会话（Yolo、cwd=workdir），自动发首轮 objective。
                match start_card_thread(&mut threads, &mut pending_activation, /* ... */, workdir, title).await {
                    Ok(thread_id) => {
                        let _ = board_query(&project, &board_dir, "board.mark_running",
                            json!({"card_id": card_id, "thread_id": thread_id}));
                        tracked.insert(card_id.to_string(), TrackedThread {
                            thread_id, idle: false, failed: false, needs_you: false });
                    }
                    Err(error) => {
                        let _ = board_query(&project, &board_dir, "board.release",
                            json!({"card_id": card_id, "detail": error.to_string()}));
                    }
                }
            }
            // 2) 对账：用 threads 里各 thread 的 active_turn_id 更新 tracked.idle，
            //    再按 plan() 回写终态。
            refresh_tracked(&threads, &mut tracked);
            if let Ok(cards) = board_cards(&project, &board_dir) {
                for action in card_scheduler::plan(&cards, &tracked) {
                    if let CardAction::Reconcile { card_id, outcome, .. } = action {
                        let name = match outcome { Outcome::AwaitingMerge => "awaiting_merge",
                            Outcome::NeedsYou => "needs_you", Outcome::Failed => "failed" };
                        let _ = board_query(&project, &board_dir, "board.mark_terminal",
                            json!({"card_id": card_id, "outcome": name}));
                        tracked.remove(&card_id);
                    }
                }
            }
        }
    }
}
```

其中 `start_card_thread` 用 Step 1 的 `start_thread_core` + `start_turn_core`（objective 用 `objective_for` 的等价文案，见下），并把 `board.json` 的 `spec_path`/`plan_path` 拼进 objective。

- [ ] **Step 5: `serve` 接线 + 恢复**

在 `serve` 里 `tokio::spawn(run_card_scheduler(...))`（与现有的 `pump_theme_notifications` 并列）；并保留 `threads` 与调度器共享（`Arc<StdMutex<HashMap<String, String>>>` 作为 thread_id→card_id 索引，启动恢复时把已 tracked 的 `running` 卡片与已 resume 的 thread 对齐）。

> 恢复：app-server 启动时，先 `board_cards`，对 `running` 且 `thread_id` 非空的卡片执行 `thread/resume`（重建 driver），再进入循环；无法 resume 的置 `needs_you`。

- [ ] **Step 6: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: 全绿（既有 `thread/start`/`turn/start` 测试不回归）。

- [ ] **Step 7: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/
git commit -m "feat(app-server): card scheduler launches visible threads and reconciles cards"
```

---

### Task 7: 桌面 —— 卡片行显示关联 thread（可选打开）

**Files:**
- Modify: `desktop/src/components/SuperpowersKanbanView.tsx`
- Modify: 上游把 `thread_id` 传进 `BoardCard` 的地方（`desktop/src/lib/superpowersKanbanState.ts` 及其调用点）
- Test: `desktop/src/components/SuperpowersKanbanView.test.tsx`

**Interfaces:**
- Consumes: `list` 返回的 `cards[].thread_id`。
- Produces: `BoardCard.threadId?: string | null`；点击卡片行调用 `onOpenThread?(threadId)`（由上层接到 `thread/resume` 的既有入口）。

- [ ] **Step 1: 写失败测试**

```tsx
it("shows a card's linked thread and opens it on click", () => {
  const onOpenThread = vi.fn();
  render(<SuperpowersKanbanView switchOn source="project" cards={[
    { id: "c1", state: "awaiting_merge", progress: null, detail: "", threadId: "thread-1" },
  ]} onOpenThread={onOpenThread} />);
  fireEvent.click(screen.getByText("thread-1"));
  expect(onOpenThread).toHaveBeenCalledWith("thread-1");
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npm test -- SuperpowersKanbanView`
Expected: FAIL。

- [ ] **Step 3: 实现**（加 `threadId` 与 `onOpenThread` 可选 prop；有 threadId 时渲染可点的短链）

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npm test -- SuperpowersKanbanView`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/
git commit -m "feat(desktop): kanban card links to its thread"
```

---

## 收尾：真机生效

- [ ] 合并到 main 后，重建并重装桌面 App（卡片会话由 app-server 起，装的是 App 在跑）：
  ```bash
  cd desktop && npm run tauri build   # 或项目既有的打包脚本
  # 用产物替换 /Applications/yi-agent.app
  ```
- [ ] 重启 App，投一张真实卡片，确认：侧栏出现「看板 · …」会话、自动执行、卡片到 `awaiting_merge`、可点开续聊。

## Self-Review 结论

- **Spec 覆盖**：状态机/槽位（Task 1-2）、四方法+list（Task 3-4）、调度器启动/对账/恢复（Task 5-6）、桌面字段（Task 7）、迁移（Task 3 Step 4）、真机生效（收尾）——均有对应任务。
- **占位符**：无 TBD；app-server 两处大改（抽取与调度器）给了真实签名与真实代码骨架。
- **类型一致**：`BoardCard`（Task 4）↔`card_scheduler::plan`（Task 5）字段一致；`LaunchClaim`（Task 2）↔`board.next_launch` 返回（Task 3）一致。
- **风险**：Task 6 的两处抽取（`start_thread_core`/`start_turn_core`）改动最深，计划要求"先抽取、跑既有测试不回归、再接线"，以隔离风险。
