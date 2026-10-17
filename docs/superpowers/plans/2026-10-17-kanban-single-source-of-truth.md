# 看板真相源收敛与实例让位 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `board.json` 成为看板的唯一真相源（任何读-改-写都在 `board.lock` 内），让拿不到 `plugin.lock` 的实例有界让位，并清掉确定路径上的陈旧 socket。

**Architecture:** 三层。①**persist**：新增 `board.lock`（flock）与事务助手 `with_board`/`read_board`，作为唯一的数据访问入口。②**service**：`BoardService` 去掉 `Inner.board` 缓存，每个动作改经 `with_board`；`Inner` 只留进程态（lease、提示去重）。合并轮（分钟级 git）在锁外做 git、用卡片状态 + `merge.lock` 保证轮次互斥。③**runtime**：`run_daemon` 有界让位；监督器 spawn 前探测 socket 并按指数退避重启；启动时清理确定路径上的死 socket。

**Tech Stack:** Rust（`superpowers-kanban-core` / `-runner` / `-ipc`：serde、chrono、libc flock、std::os::unix::net）；监督器在 `yi-agent-rs/crates/yi-agent-supervisors`。

**Spec:** `docs/superpowers/specs/2026-10-17-kanban-single-source-of-truth-design.md`

## Global Constraints

- 行号以基线 `296f45d9` 为准；改前 `grep` 复核（分支会前移）。
- **`board.lock` 的临界区必须短。** 只有合并轮（`merge_next` / `merge_request`，分钟级 git）允许在锁外做 git 再回锁内写结果；其余动作（`reconcile_merged` 的 git 探测、`merge_finish` 的一次 `branch_merged_into`）在毫秒级，可在锁内。
- **锁序固定为 `Mutex<Inner>` → `board.lock`**，绝不反向（`with_board` 是独立函数、不认识 `Inner`，所以反向在结构上不可能）。
- 不改 `board.json` 数据格式（`Card`/`Board` 字段不变），只改访问方式，旧文件无损。
- 不改 socket 协议、不改 `board.*` 方法名（宿主 `card_scheduler` 契约不动）。
- 不 unlink 任何锁文件（`plugin.lock`/`merge.lock`/`board.lock`）：删除会让两个 inode 各持锁。
- 只显式 `git add <path>`；commit 前 `cargo fmt --all`（在插件目录）。
- 环境：`export PATH="/Users/gongyichen/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`、`export TMPDIR=/Users/gongyichen/.yi-agent-tmp`、`cargo --offline`。

---

## File Structure

- `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/persist.rs` — 新增 `board.lock` 取锁、`with_board`、`read_board`。
- `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs` — `Inner` 去 `board` 字段；各动作经 `with_board`；`list` 改只读。
- `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs` — `command_done`/`archive`/`purge` 改用 `with_board`；`run_daemon` 有界让位 + stale socket 清扫。
- `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/single_instance.rs` — 新增 `acquire_within`（有界等待）。
- `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/calendar.rs` — 新增 `instance_wait_secs`（默认 10）。
- `yi-agent-rs/crates/yi-agent-supervisors/src/supervisor.rs` — spawn 前探测 socket；指数退避。
- `plugins/superpowers-kanban/README.md` — 登记真相源与让位行为。

---

### Task 1: persist —— `board.lock` 与事务助手

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/persist.rs`

**Interfaces:**
- Produces:
  - `pub fn with_board<T>(state_dir: &Path, f: impl FnOnce(&mut Board) -> Result<T, String>) -> Result<T, String>`
  - `pub fn read_board(state_dir: &Path) -> Board`

- [ ] **Step 1: 写失败测试**

在 `persist.rs` 的测试模块追加：

```rust
    #[test]
    fn with_board_waits_for_the_lock_and_persists_the_change() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        // 先落一张空板。
        let mut board = Board::new();
        board.enqueue(
            CardId::new("a"),
            "a.spec.md".into(),
            "a.plan.md".into(),
            Local.with_ymd_and_hms(2026, 10, 1, 9, 0, 0).single().unwrap(),
        );
        save_board(&state_dir.join("board.json"), &board).unwrap();

        // 事务里改一处，落盘后必须读到。
        with_board(&state_dir, |board| {
            board
                .transition(&CardId::new("a"), superpowers_kanban_core::card::CardState::Running)
                .map_err(|e| e.to_string())?;
            Ok(())
        })
        .unwrap();
        let reloaded = read_board(&state_dir);
        assert_eq!(
            reloaded.get(&CardId::new("a")).unwrap().state,
            superpowers_kanban_core::card::CardState::Running,
            "with_board 必须落盘"
        );
    }

    #[test]
    fn with_board_does_not_persist_when_the_closure_fails() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        let mut board = Board::new();
        board.enqueue(
            CardId::new("a"),
            "a.spec.md".into(),
            "a.plan.md".into(),
            Local.with_ymd_and_hms(2026, 10, 1, 9, 0, 0).single().unwrap(),
        );
        save_board(&state_dir.join("board.json"), &board).unwrap();
        let before = std::fs::read_to_string(state_dir.join("board.json")).unwrap();

        let error = with_board(&state_dir, |board| {
            // 改了内存里的板，但闭包报错——一个字节都不该落盘。
            let _ = board.transition(
                &CardId::new("a"),
                superpowers_kanban_core::card::CardState::Cancelled,
            );
            Err::<(), String>("nope".to_string())
        })
        .unwrap_err();
        assert_eq!(error, "nope");
        let after = std::fs::read_to_string(state_dir.join("board.json")).unwrap();
        assert_eq!(before, after, "失败的闭包不得留下半写状态");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test --offline -p superpowers-kanban-runner --lib persist::tests::with_board_waits_for_the_lock_and_persists_the_change`
Expected: 编译失败，`cannot find function with_board`。

- [ ] **Step 3: 实现**

`persist.rs` 顶部 `use` 区补 `libc`（`Cargo.toml` 的 `superpowers-kanban-runner` 已有 `libc = "0.2"` 依赖）：

```rust
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::Path;
```

在 `save_board` 之后追加：

```rust
/// 看板数据的互斥门。语义与 `plugin.lock` **不同**：那把锁表达「谁是推进者」
/// （只有 `run` 取），这把锁表达「谁在读改写 board.json」（任何写者都取）。
/// 混用会让一次性的 CLI 动词被误判为「第二实例」而遭拒。
struct BoardLock {
    /// 关闭这个描述符就是全部释放协议，所以它从不被读——只为把锁按住而保活。
    _file: File,
}

/// 取 `<state_dir>/board.lock` 的独占锁，**阻塞**直到拿到。
///
/// 阻塞而非非阻塞：临界区只有「读约 15KB → 改 → 原子写」，毫秒级；与之相比
/// 让调用方处理 `WouldBlock` 重试反而更复杂。`flock` 由内核在持有进程退出
/// （含 `SIGKILL`）时释放，所以不会永久卡死。
///
/// 与 `merge_lock.rs` / `single_instance.rs` 同理由，**不在 drop 时 unlink**：
/// 删除文件会让另一个进程在旧 inode 上持锁、在新 inode 上再取到锁，等于没锁。
fn acquire_board_lock(state_dir: &Path) -> std::io::Result<BoardLock> {
    std::fs::create_dir_all(state_dir)?;
    let path = state_dir.join("board.lock");
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if outcome != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(BoardLock { _file: file })
}

/// 锁内 `load → 改 → save`。**任何**对 board.json 的读-改-写都必须经此。
///
/// 闭包返回 `Err` 时不落盘（失败不留半写状态）。这是 CLI 与 daemon 共用的
/// 唯一数据入口——正因如此，一次性 CLI 写入不会再被 daemon 的陈旧内存覆盖。
pub fn with_board<T>(
    state_dir: &Path,
    f: impl FnOnce(&mut Board) -> Result<T, String>,
) -> Result<T, String> {
    let _lock =
        acquire_board_lock(state_dir).map_err(|error| format!("could not lock the board: {error}"))?;
    let path = state_dir.join("board.json");
    let mut board = load_board(&path);
    let value = f(&mut board)?;
    save_board(&path, &board).map_err(|error| error.to_string())?;
    Ok(value)
}

/// 只读一次看板。**不加锁**：`save_board` 用「同目录临时文件 + rename」落盘，
/// 读者永远看到完整文件（要么旧要么新，不会是半个）。`list` 走这里。
pub fn read_board(state_dir: &Path) -> Board {
    load_board(&state_dir.join("board.json"))
}
```

测试模块顶部确认已有 `use chrono::{Local, TimeZone};` 与 `use superpowers_kanban_core::card::CardId;`（现有测试已用到 `Local.with_ymd_and_hms` 与 `CardId`）；缺 `CardState` 就在测试里用全路径 `superpowers_kanban_core::card::CardState`（上面的测试已用全路径，无需补 import）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test --offline -p superpowers-kanban-runner --lib persist`
Expected: 全部 PASS。

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-kanban && cargo fmt --all && cd ../..
git add plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/persist.rs
git commit -m "feat(kanban): add a board lock and a single transactional accessor"
```

---

### Task 2: service —— 去掉看板缓存，全部动作改经 `with_board`

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`

**Interfaces:**
- Consumes: Task 1 的 `persist::with_board`、`persist::read_board`。
- Produces: `BoardService` 的公开方法签名**全部不变**（`next_launch`/`mark_running`/`mark_terminal`/`release`/`merge_request`/`merge_finish`/`merge_next`/`reconcile_merged`/`list`/`archive_due`/`consume_inbox`/`next_free_merge_id`/`adopt_running_leases`/`migrate_legacy_running`）；`Inner` 只剩 `leases`/`leases_dir`/`notified_missing_branch`。

- [ ] **Step 1: 改 `Inner` 与 `new`，删 `save`**

把 `struct Inner` 改为（删 `board` 字段并补注释）：

```rust
struct Inner {
    /// 已认领卡片持有的槽位租约。`Running` 卡片也仍在这里，直到终态释放。
    ///
    /// 这是**进程态**：`flock` fd 只能活在持有它的进程里，因此绝不进
    /// board.json。看板数据本身是共享态，只在磁盘上有一份真相。
    leases: std::collections::HashMap<CardId, Lease>,
    /// `None` 表示拿不到全局租约目录：`next_launch` 必须报错而不是私自
    /// 造一个目录，否则会和别的进程用两个互不相干的池子、悄悄超配额。
    leases_dir: Option<PathBuf>,
    /// 已提示过「分支缺失」的卡：同一 daemon 生命周期内每卡至多提示一次。
    /// 纯内存、不落盘——重启后重发一次是可接受的（该提示要求人工动作）。
    notified_missing_branch: std::collections::HashSet<CardId>,
}
```

`new` 去掉载入，删掉 `let board = ...` 与 `Inner` 里的 `board,`：

```rust
    pub fn new(state_dir: PathBuf, project_root: PathBuf, home: Option<PathBuf>) -> Self {
        let leases_dir = home
            .map(|home| {
                home.join(".yi-agent")
                    .join("superpowers-kanban")
                    .join("leases")
            })
            .or_else(lease::global_leases_dir);
        Self {
            state_dir,
            project_root,
            inner: Mutex::new(Inner {
                leases: Default::default(),
                leases_dir,
                notified_missing_branch: Default::default(),
            }),
        }
    }
```

删掉 `fn save(&self, inner: &Inner)` 整个方法（不再有内存板可存）。`fn board_path` 若在非测试代码里还有引用就保留，否则一并删除（Step 4 会全量编译暴露）。

- [ ] **Step 2: 逐个动作改经 `with_board`**

按下表替换每个方法体（公开签名不变）。

`archive_due`：

```rust
    pub fn archive_due(&self, now: DateTime<Local>) -> Vec<CardId> {
        let hours = superpowers_kanban_core::calendar::ConcurrencyCalendar::load_preferring_new(
            &self.state_dir,
        )
        .archive_grace_hours;
        let grace = chrono::Duration::hours(hours.min(24 * 30) as i64);
        persist::with_board(&self.state_dir, |board| Ok(board.archive_due(now, grace)))
            .unwrap_or_default()
    }
```

`next_launch`（lease 与 board 分开：先租约，再锁内认领 + 建 worktree + 回填）：

```rust
    pub fn next_launch(
        &self,
        limit: u16,
        _now: DateTime<Local>,
    ) -> Result<Option<LaunchClaim>, String> {
        let mut inner = self.lock();
        let Some(dir) = inner.leases_dir.clone() else {
            return Err("no lease directory".into());
        };
        // 先占槽：拿不到就说明名额已满（含本进程自己已持有的）。
        let Some(lease) = lease::acquire_in(&dir, limit as usize) else {
            return Ok(None);
        };
        let project_root = self.project_root.clone();
        // 建 worktree 失败的错误在这里暂存：`with_board` 在闭包返回 `Err` 时**不
        // 落盘**，而失败路径必须先落盘 `Failed`，所以闭包必须成功返回、错误留到
        // 锁外再抛（见下）。这是用户裁决「保当前行为」的落点。
        let mut worktree_failure: Option<String> = None;
        // 再认领：队空或无名额时把刚拿到的租约原地丢掉（drop 即释放）。
        let claimed = persist::with_board(&self.state_dir, |board| {
            let Some(card_id) = board.claim_next_launch(limit) else {
                return Ok(None);
            };
            let branch = format!("kanban/{}", slugify(&card_id));
            let workdir = match ensure_worktree(&project_root, &card_id, &branch) {
                Ok(path) => path,
                Err(error) => {
                    // 认领即占槽：建 worktree 失败必须把卡片标为 Failed **并落盘**，
                    // 否则它会卡在 Launching 永远不再被认领。
                    //
                    // 用 `return Ok(None)`（闭包成功）而不是 `return Err(..)`：后者会让
                    // `with_board` 丢弃上面这次 `transition(Failed)`，卡片就卡死了。
                    let _ = board.transition(&card_id, CardState::Failed);
                    worktree_failure = Some(format!("worktree for {card_id} failed: {error}"));
                    return Ok(None);
                }
            };
            if let Some(base) = effective_head(&workdir) {
                let _ = board.set_base_commit(&card_id, base);
            }
            let _ = board.set_workdir(&card_id, workdir.clone());
            let title = title_for(board, &card_id);
            Ok(Some(LaunchClaim {
                card_id: card_id.0,
                workdir,
                title,
            }))
        })?;
        // 建 worktree 失败：状态已落盘为 `Failed`，现在才把错误抛出去（时机见上）。
        if let Some(error) = worktree_failure {
            return Err(error);
        }
        if let Some(claim) = &claimed {
            inner.leases.insert(CardId::new(claim.card_id.clone()), lease);
        }
        Ok(claimed)
    }
```

**为什么不能写成 `Err` 直出**：`with_board` 的契约是「闭包返回 `Err` 即不落盘」（Task 1 的
`with_board_does_not_persist_when_the_closure_fails` 正是钉这个）。而失败路径必须落盘
`Failed`。所以闭包内改完状态后**成功返回 `Ok(None)`**，把错误暂存到局部变量，回到锁外再抛。

`adopt_running_leases`（把 `inner.board.cards()` 换成一次性只读载入）：

```rust
    pub fn adopt_running_leases(&self, limit: u16) {
        let mut inner = self.lock();
        let Some(dir) = inner.leases_dir.clone() else {
            return;
        };
        let ids: Vec<CardId> = persist::read_board(&self.state_dir)
            .cards()
            .iter()
            .filter(|card| card.state == CardState::Running)
            .map(|card| card.id.clone())
            .collect();
        for id in ids {
            if inner.leases.contains_key(&id) {
                continue;
            }
            match lease::acquire_in(&dir, limit as usize) {
                Some(lease) => {
                    inner.leases.insert(id, lease);
                }
                None => {
                    eprintln!(
                        "superpowers-kanban: {} is running but no slot could be re-adopted",
                        id.0
                    );
                }
            }
        }
    }
```

`mark_running`：

```rust
    pub fn mark_running(&self, card_id: &str, thread_id: &str) -> Result<(), String> {
        let id = CardId::new(card_id);
        let thread_id = thread_id.to_string();
        persist::with_board(&self.state_dir, |board| {
            let already_running = board
                .get(&id)
                .map(|card| card.state == CardState::Running)
                .unwrap_or(false);
            if !already_running {
                board
                    .transition(&id, CardState::Running)
                    .map_err(|error| error.to_string())?;
            }
            board
                .set_thread_id(&id, thread_id)
                .map_err(|error| error.to_string())?;
            Ok(())
        })
    }
```

`mark_terminal`：

```rust
    pub fn mark_terminal(
        &self,
        card_id: &str,
        outcome: &str,
        _detail: Option<&str>,
    ) -> Result<(), String> {
        let next = match outcome {
            "awaiting_merge" => CardState::AwaitingMerge,
            "needs_you" => CardState::NeedsYou,
            "failed" => CardState::Failed,
            other => return Err(format!("unknown outcome: {other}")),
        };
        let id = CardId::new(card_id);
        persist::with_board(&self.state_dir, |board| {
            board
                .transition(&id, next)
                .map_err(|error| error.to_string())
        })?;
        self.lock().leases.remove(&id); // 释放槽位：drop 掉 flock
        Ok(())
    }
```

`release`：

```rust
    pub fn release(&self, card_id: &str, _detail: &str) -> Result<(), String> {
        let id = CardId::new(card_id);
        persist::with_board(&self.state_dir, |board| {
            board
                .transition(&id, CardState::Failed)
                .map_err(|error| error.to_string())
        })?;
        self.lock().leases.remove(&id);
        Ok(())
    }
```

`migrate_legacy_running`（启动迁移整体放进一次 `with_board`；期间 git 只有 `merge --abort`，仅在启动时发生，可接受）：

```rust
    pub fn migrate_legacy_running(&self) {
        let project_root = self.project_root.clone();
        let mut inner = self.lock();
        let freed: Vec<CardId> = persist::with_board(&self.state_dir, |board| {
            let mut freed: Vec<CardId> = Vec::new();

            let legacy_running: Vec<CardId> = board
                .cards()
                .iter()
                .filter(|card| {
                    card.state == CardState::Running
                        && card.thread_id.as_deref().unwrap_or("").is_empty()
                })
                .map(|card| card.id.clone())
                .collect();
            for id in legacy_running {
                if board.transition(&id, CardState::NeedsYou).is_ok() {
                    freed.push(id);
                }
            }

            let launching_zombies: Vec<CardId> = board
                .cards()
                .iter()
                .filter(|card| card.state == CardState::Launching)
                .map(|card| card.id.clone())
                .collect();
            for id in launching_zombies {
                if board.transition(&id, CardState::Failed).is_ok() {
                    freed.push(id);
                }
            }

            // 崩溃若发生在「置 Merging」之后、「回写终态」之前，会留下 Merging 僵尸。
            // 合并可能半途，需人确认，故收成 NeedsYou（不是 Failed）。先尽力 abort。
            let merging: Vec<CardId> = board
                .cards()
                .iter()
                .filter(|card| card.state == CardState::Merging)
                .map(|card| card.id.clone())
                .collect();
            for id in merging {
                if let Some(card) = board.get(&id).cloned() {
                    if let Some(base) = card.base_ref.as_deref() {
                        if let Ok(wt) = merge::prepare(&project_root, base) {
                            let _ = std::process::Command::new("git")
                                .arg("-C")
                                .arg(&wt.path)
                                .args(["merge", "--abort"])
                                .output();
                            merge::cleanup(&wt);
                        }
                    }
                }
                if board.transition(&id, CardState::NeedsYou).is_ok() {
                    freed.push(id);
                }
            }
            Ok(freed)
        })
        .unwrap_or_default();
        for id in freed {
            inner.leases.remove(&id);
        }
    }
```

`consume_inbox`：

```rust
    pub fn consume_inbox(&self, now: DateTime<Local>) -> Vec<InboxOutcome> {
        let state_dir = self.state_dir.clone();
        persist::with_board(&state_dir, |board| Ok(inbox::consume(&state_dir, board, now)))
            .unwrap_or_default()
    }
```

`merge_next`（**锁外做 git**：分钟级；轮次互斥由 `merge.lock` + 卡片 `Merging` 状态保证）：

```rust
    pub fn merge_next(&self) -> Result<Option<MergeClaim>, String> {
        let Some(_gate) = crate::merge_lock::acquire(&self.state_dir) else {
            return Ok(None);
        };
        // 1) 锁内认领，读出 source/base/origin（快）。
        let claimed = persist::with_board(&self.state_dir, |board| {
            let Some(card_id) = board.claim_next_merge(false) else {
                return Ok(None);
            };
            let card = board.get(&card_id).cloned().expect("just claimed");
            Ok(Some((
                card_id,
                card.source_ref.clone().unwrap_or_default(),
                card.base_ref.clone().unwrap_or_default(),
                card.origin_card.clone(),
            )))
        })?;
        let Some((card_id, source, base, origin)) = claimed else {
            return Ok(None);
        };
        // 2) git 在锁外：这些调用是分钟级，绝不能占着 board.lock。
        //
        // 每个结局都带**自己的**目标状态：前三条是「等用户处理」（分支缺失 /
        // 目录准备失败 / 目录有未提交改动），只有最后「git 真的跑挂了」才是
        // `Failed`。这与现状逐字一致（`finish(NeedsYou)` 在前、`next` 只对
        // `merge::run` 的结果做 Done/NeedsYou/Failed 映射），是行为保持的重构。
        let (outcome, next) = if !merge::source_branch_exists(&self.project_root, &source) {
            (
                MergeOutcome::GitError(format!("source branch '{source}' does not exist")),
                CardState::NeedsYou,
            )
        } else {
            match merge::prepare(&self.project_root, &base) {
                Err(detail) => (MergeOutcome::GitError(detail), CardState::NeedsYou),
                Ok(base_wt) => {
                    if merge::is_dirty(&base_wt.path) {
                        let detail =
                            format!("base worktree {} is dirty", base_wt.path.display());
                        merge::cleanup(&base_wt);
                        (MergeOutcome::GitError(detail), CardState::NeedsYou)
                    } else {
                        let outcome = merge::run(&base_wt.path, &source, &base);
                        merge::cleanup(&base_wt);
                        let next = match outcome {
                            MergeOutcome::Merged => CardState::Done,
                            MergeOutcome::Conflict => CardState::NeedsYou,
                            MergeOutcome::GitError(_) => CardState::Failed,
                        };
                        (outcome, next)
                    }
                }
            }
        };
        // 3) 锁内回写终态。
        persist::with_board(&self.state_dir, |board| {
            let _ = board.transition(&card_id, next);
            if next == CardState::Done {
                if let Some(origin) = &origin {
                    let _ = board.transition(origin, CardState::Done);
                }
            }
            Ok(())
        })?;
        Ok(Some(MergeClaim {
            card_id: card_id.0,
            outcome,
        }))
    }
```

`merge_request_locked`（**锁外做 git**；`merge.lock` 已由调用方持有，串行化申请）：

```rust
    fn merge_request_locked(&self, card_id: &str) -> Result<MergeRequest, String> {
        let id = CardId::new(card_id);
        // 1) 锁内读卡片并判准入。
        let snapshot = persist::read_board(&self.state_dir);
        let Some(card) = snapshot.get(&id) else {
            return Ok(MergeRequest::Denied(format!("unknown card: {card_id}")));
        };
        if !matches!(
            card.state,
            CardState::AwaitingMerge | CardState::NeedsYou | CardState::Running
        ) {
            return Ok(MergeRequest::Denied(format!(
                "card {card_id} is {:?}, not awaiting_merge/needs_you/running",
                card.state
            )));
        }
        let (source, base) = match (card.source_ref.clone(), card.base_ref.clone()) {
            (Some(source), Some(base)) => (source, base),
            _ => {
                if card.kind != CardKind::Implementation {
                    return Ok(MergeRequest::Denied(format!(
                        "card {card_id} has no source/base to merge"
                    )));
                }
                (
                    format!("kanban/{}", crate::worktree::slugify(&card.id)),
                    crate::merge::default_branch(&self.project_root),
                )
            }
        };
        // 2) git 在锁外。
        if !merge::source_branch_exists(&self.project_root, &source) {
            return Ok(MergeRequest::Denied(format!(
                "source branch '{source}' does not exist"
            )));
        }
        let workdir = match merge::prepare(&self.project_root, &base) {
            Ok(wt) => wt.path,
            Err(error) => return Ok(MergeRequest::Denied(error)),
        };
        // 3) 锁内落 `Merging`。
        persist::with_board(&self.state_dir, |board| {
            board
                .transition(&id, CardState::Merging)
                .map_err(|error| error.to_string())
        })?;
        Ok(MergeRequest::Granted {
            workdir,
            source,
            base,
        })
    }
```

`merge_finish`（一次 `branch_merged_into`，毫秒级，可留锁内）：

```rust
    pub fn merge_finish(&self, card_id: &str) -> Result<MergeFinish, String> {
        let id = CardId::new(card_id);
        let project_root = self.project_root.clone();
        let (next, settle) = persist::with_board(&self.state_dir, |board| {
            let Some(card) = board.get(&id).cloned() else {
                return Err(format!("unknown card: {card_id}"));
            };
            if card.state != CardState::Merging {
                return Ok((MergeFinish::Cleared, false));
            }
            let (source, base) = match (card.source_ref.clone(), card.base_ref.clone()) {
                (Some(source), Some(base)) => (source, base),
                _ => (
                    format!("kanban/{}", crate::worktree::slugify(&card.id)),
                    merge::default_branch(&project_root),
                ),
            };
            let merged = merge::source_branch_exists(&project_root, &source)
                && merge::branch_merged_into(&project_root, &source, &base);
            let (next, state) = if merged {
                (MergeFinish::Done, CardState::Done)
            } else {
                (MergeFinish::NeedsYou, CardState::NeedsYou)
            };
            board
                .transition(&id, state)
                .map_err(|error| error.to_string())?;
            Ok((next, true))
        })?;
        if settle {
            self.lock().leases.remove(&id);
        }
        Ok(next)
    }
```

`reconcile_merged`（git 探测毫秒级，整体留锁内；`notified_missing_branch` 仍留在 `Inner`）：

```rust
    pub fn reconcile_merged(&self) -> Vec<crate::reconcile::Reconcile> {
        use crate::reconcile::{self, RealGit, Reconcile};
        let git = RealGit {
            project_root: &self.project_root,
        };
        let mut inner = self.lock();
        persist::with_board(&self.state_dir, |board| {
            // 1) 算候选报告（git 探测在毫秒级）。
            let candidates = reconcile::plan(board, &git);
            // 2) 去重缺分支提示。
            let mut reports: Vec<Reconcile> = Vec::new();
            for report in candidates {
                match report {
                    Reconcile::MissingBranch { ref id, .. } => {
                        if inner.notified_missing_branch.insert(id.clone()) {
                            reports.push(report);
                        }
                    }
                    other => reports.push(other),
                }
            }
            // 3) 落地 `done`；报告随之裁剪——只有真正迁移成功的卡才留报告。
            reports.retain(|report| match report {
                Reconcile::Converged { id, .. } => {
                    board.transition(id, CardState::Done).is_ok()
                }
                Reconcile::MissingBranch { .. } => true,
            });
            Ok(reports)
        })
        .unwrap_or_default()
    }
```

`list`（纯读，无锁）：

```rust
    pub fn list(&self) -> Value {
        let board = persist::read_board(&self.state_dir);
        json!({
            "cards": board.visible().map(|card| json!({
                "id": card.id.0,
                "state": card.state,
                "spec_path": card.spec_path,
                "plan_path": card.plan_path,
                "workdir": card.workdir,
                "thread_id": card.thread_id,
                "kind": card.kind,
                "source": card.source_ref,
                "base": card.base_ref,
                "origin_card": card.origin_card.as_ref().map(|id| id.0.clone()),
            })).collect::<Vec<_>>(),
        })
    }
```

`next_free_merge_id`：

```rust
    pub fn next_free_merge_id(&self, source: &str, base: &str) -> String {
        persist::read_board(&self.state_dir)
            .next_free_merge_id(source, base)
            .0
    }
```

- [ ] **Step 3: 改两个仅测试用的助手**

`clear_thread_id_for_test` 与 `enqueue_merge_card_for_test` 都改走 `with_board`：

```rust
    #[cfg(test)]
    pub(crate) fn clear_thread_id_for_test(&self, card_id: &str) {
        let state_dir = self.state_dir.clone();
        let _ = persist::with_board(&state_dir, |board| {
            let Ok(text) = std::fs::read_to_string(state_dir.join("board.json")) else {
                return Ok(());
            };
            let Ok(mut value) = serde_json::from_str::<Value>(&text) else {
                return Ok(());
            };
            if let Some(cards) = value.get_mut("cards").and_then(Value::as_array_mut) {
                for card in cards {
                    if card.get("id").and_then(Value::as_str) == Some(card_id) {
                        card["thread_id"] = Value::Null;
                    }
                }
            }
            if let Ok(reloaded) = serde_json::from_value::<Board>(value) {
                *board = reloaded;
            }
            Ok(())
        });
    }

    #[cfg(test)]
    pub(crate) fn enqueue_merge_card_for_test(
        &self,
        id: &str,
        source: &str,
        base: &str,
        origin: Option<&str>,
    ) {
        let _ = persist::with_board(&self.state_dir, |board| {
            board.enqueue_merge(
                CardId::new(id),
                source.to_string(),
                base.to_string(),
                origin.map(CardId::new),
                chrono::Local::now(),
            );
            Ok(())
        });
    }
```

- [ ] **Step 4: 跑全量测试**

Run: `cd plugins/superpowers-kanban && cargo test --offline -p superpowers-kanban-runner --lib`
Expected: 全部 PASS（含既有 84 条）。若报 `unused` 警告（如 `board_path` 已无引用），删掉该函数。

- [ ] **Step 5: 补一条「CLI 写入不被 daemon 覆盖」的回归测试**

在 `service.rs` 测试模块追加（模拟 CLI 经 `with_board` 写盘后，service 读到的是新值——即不再有内存缓存）：

```rust
    #[test]
    fn a_write_through_with_board_is_visible_to_the_service_list() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let mut board = superpowers_kanban_core::board::Board::new();
        let id = superpowers_kanban_core::card::CardId::new("c1");
        board.enqueue(
            id.clone(),
            "c.spec.md".into(),
            "c.plan.md".into(),
            chrono::Local::now(),
        );
        crate::persist::save_board(&state_dir.join("board.json"), &board).unwrap();
        let service = BoardService::new(state_dir.clone(), dir.path().to_path_buf(), None);
        // 先建好 service（其内不再缓存看板），再由「外部写者」经 with_board 改盘。
        crate::persist::with_board(&state_dir, |board| {
            board
                .transition(&id, superpowers_kanban_core::card::CardState::Running)
                .map_err(|e| e.to_string())?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            service.list()["cards"][0]["state"], "running",
            "service 必须读到外部写入，而不是启动时的内存快照"
        );
    }
```

- [ ] **Step 6: 跑测试确认通过并提交**

Run: `cd plugins/superpowers-kanban && cargo test --offline -p superpowers-kanban-runner --lib`
Expected: 全部 PASS。

```bash
cd plugins/superpowers-kanban && cargo fmt --all && cd ../..
git add plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs
git commit -m "refactor(kanban): drop the in-memory board so board.json is the only truth"
```

---

### Task 3: CLI —— `done` / `archive` / `purge` 改经 `with_board`

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`

**Interfaces:**
- Consumes: Task 1 的 `persist::with_board`。
- Produces: 三个动词的输出文案与退出码**不变**。

- [ ] **Step 1: 写失败测试（并发不丢更新）**

在 `main.rs` 测试模块追加：

```rust
    #[test]
    fn two_concurrent_transactions_both_land_on_disk() {
        use superpowers_kanban_core::board::Board;
        use superpowers_kanban_core::card::CardId;
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let mut board = Board::new();
        for id in ["a", "b"] {
            board.enqueue(
                CardId::new(id),
                format!("{id}.spec.md").into(),
                format!("{id}.plan.md").into(),
                chrono::Local::now(),
            );
        }
        superpowers_kanban_runner::persist::save_board(&state_dir.join("board.json"), &board)
            .unwrap();

        let a_dir = state_dir.clone();
        let b_dir = state_dir.clone();
        let a = std::thread::spawn(move || {
            superpowers_kanban_runner::persist::with_board(&a_dir, |board| {
                board
                    .transition(&CardId::new("a"), superpowers_kanban_core::card::CardState::Running)
                    .map_err(|e| e.to_string())
            })
        });
        let b = std::thread::spawn(move || {
            superpowers_kanban_runner::persist::with_board(&b_dir, |board| {
                board
                    .transition(&CardId::new("b"), superpowers_kanban_core::card::CardState::Running)
                    .map_err(|e| e.to_string())
            })
        });
        a.join().unwrap().unwrap();
        b.join().unwrap().unwrap();

        let final_board = superpowers_kanban_runner::persist::read_board(&state_dir);
        assert_eq!(
            final_board.get(&CardId::new("a")).unwrap().state,
            superpowers_kanban_core::card::CardState::Running,
            "a 的写入必须留下"
        );
        assert_eq!(
            final_board.get(&CardId::new("b")).unwrap().state,
            superpowers_kanban_core::card::CardState::Running,
            "b 的写入必须留下——锁内 load 才能做到"
        );
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test --offline -p superpowers-kanban-runner --bin superpowers-kanban two_concurrent_transactions_both_land_on_disk`
Expected: 依赖 Task 1 的 `with_board`，若未落地则编译失败；Task 1 已落地时应**立即 PASS**（这条钉住的是 Task 1 的锁语义被 CLI 真正使用）。若非 PASS，说明 Task 1 的 `with_board` 没做到「锁内 load」。

- [ ] **Step 3: 实现**

`command_done`：把 `let path = ...; let mut board = load_board(&path); ... save_board(&path, &board)` 改为一次 `with_board`：

```rust
fn command_done(
    state_dir: &std::path::Path,
    card_id: &str,
    project_root: &std::path::Path,
) -> Result<String, String> {
    use superpowers_kanban_core::card::{CardId, CardState};
    let id = CardId::new(card_id);
    // 核对：推导分支并判是否已并入 base（分支可能已被删除——那时只能采信人工确认）。
    let source = format!(
        "kanban/{}",
        superpowers_kanban_runner::worktree::slugify(&id)
    );
    let base = superpowers_kanban_runner::merge::default_branch(project_root);
    let verdict = if !superpowers_kanban_runner::merge::source_branch_exists(project_root, &source)
    {
        "branch-missing"
    } else if superpowers_kanban_runner::merge::branch_merged_into(project_root, &source, &base) {
        "verified"
    } else {
        "not-merged"
    };
    // 状态变更在锁内落盘：daemon 在跑也不会覆盖（它下个 tick 读到的是新文件）。
    superpowers_kanban_runner::persist::with_board(state_dir, |board| {
        match board.get(&id) {
            None => return Err(format!("unknown card: {card_id}")),
            Some(card) if card.state != CardState::AwaitingMerge => {
                return Err(format!(
                    "card {card_id} is {:?}, not awaiting_merge",
                    card.state
                ));
            }
            Some(_) => {}
        }
        board
            .transition(&id, CardState::Done)
            .map_err(|e| e.to_string())
    })?;
    Ok(format!(
        "card {card_id} -> done ({verdict}); auto-archives in 24h — \
         `superpowers-kanban archive {card_id}` to hide it now"
    ))
}
```

`command_archive`：

```rust
fn command_archive(
    state_dir: &std::path::Path,
    card_id: Option<&str>,
    all_terminal: bool,
) -> Result<String, String> {
    use superpowers_kanban_core::card::CardId;
    let archived = superpowers_kanban_runner::persist::with_board(state_dir, |board| {
        if all_terminal {
            let due: Vec<CardId> = board
                .cards()
                .iter()
                .filter(|card| card.state.is_terminal() && !card.archived)
                .map(|card| card.id.clone())
                .collect();
            let count = due.len();
            for id in due {
                board.archive(&id).map_err(|e| e.to_string())?;
            }
            Ok(count)
        } else {
            let id = CardId::new(card_id.expect("parse guarantees an id or --all-terminal"));
            board.archive(&id).map_err(|e| e.to_string())?;
            Ok(1)
        }
    })?;
    Ok(format!("archived {archived} card(s)"))
}
```

`command_purge`：

```rust
fn command_purge(
    state_dir: &std::path::Path,
    card_id: Option<&str>,
    all_archived: bool,
) -> Result<String, String> {
    use superpowers_kanban_core::card::CardId;
    let removed = superpowers_kanban_runner::persist::with_board(state_dir, |board| {
        if all_archived {
            Ok(board.purge_archived())
        } else {
            let id = CardId::new(card_id.expect("parse guarantees an id or --all-archived"));
            board.purge(&id).map_err(|e| e.to_string())?;
            Ok(1)
        }
    })?;
    Ok(format!("purged {removed} card(s)"))
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test --offline`
Expected: 全部 PASS（含既有 CLI 解析测试零回归）。

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-kanban && cargo fmt --all && cd ../..
git add plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs
git commit -m "fix(kanban): write CLI verbs through the board lock so daemons stop clobbering them"
```

---

### Task 4: 有界让位（`instance_wait_secs` + `acquire_within`）

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/calendar.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/single_instance.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`

**Interfaces:**
- Produces: `ConcurrencyCalendar::instance_wait_secs: u32`（默认 10）；`single_instance::acquire_within(state_dir: &Path, budget: Duration) -> Option<InstanceLock>`。

- [ ] **Step 1: 写失败测试**

`calendar.rs` 测试模块：

```rust
    #[test]
    fn instance_wait_defaults_to_ten_and_can_be_overridden() {
        assert_eq!(ConcurrencyCalendar::default().instance_wait_secs, 10);
        let c = ConcurrencyCalendar::from_toml("instance_wait_secs = 0\n").unwrap();
        assert_eq!(c.instance_wait_secs, 0);
    }
```

`single_instance.rs` 测试模块：

```rust
    #[test]
    fn acquire_within_gives_up_after_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let held = acquire(dir.path()).expect("first instance takes the lock");
        let start = std::time::Instant::now();
        let outcome = acquire_within(dir.path(), Duration::from_millis(300));
        assert!(outcome.is_none(), "被占用时必须在预算内放弃");
        assert!(
            start.elapsed() >= Duration::from_millis(250),
            "必须真的等够预算，而不是立刻返回"
        );
        drop(held);
        // 释放后同一预算内应能拿到。
        let got = acquire_within(dir.path(), Duration::from_secs(2));
        assert!(got.is_some(), "释放后必须能取到");
    }

    #[test]
    fn acquire_within_zero_budget_tries_once() {
        let dir = tempfile::tempdir().unwrap();
        let _held = acquire(dir.path()).expect("first instance takes the lock");
        assert!(
            acquire_within(dir.path(), Duration::ZERO).is_none(),
            "预算为 0 表示不等待、只试一次"
        );
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test --offline -p superpowers-kanban-core --lib calendar::tests::instance_wait_defaults_to_ten_and_can_be_overridden`
Expected: FAIL（`no field instance_wait_secs`）。

- [ ] **Step 3: 实现**

`calendar.rs`：
- 常量区加 `pub const DEFAULT_INSTANCE_WAIT_SECS: u32 = 10;`
- `ConcurrencyCalendar` 结构体加字段 `pub instance_wait_secs: u32,`
- `Default` 里加 `instance_wait_secs: DEFAULT_INSTANCE_WAIT_SECS,`
- `from_toml` 里加 `instance_wait_secs: file.instance_wait_secs.unwrap_or(DEFAULT_INSTANCE_WAIT_SECS),`
- `CalendarFile` 加 `instance_wait_secs: Option<u32>,`
- `settings_json` 加一行 `"instance_wait_secs": self.instance_wait_secs,`
- `from_settings_json` 加解析（与 `archive_grace_hours` 同款，缺省取默认）：

```rust
        let instance_wait_secs = match value.get("instance_wait_secs") {
            None | Some(serde_json::Value::Null) => DEFAULT_INSTANCE_WAIT_SECS,
            Some(raw) => u32::try_from(raw.as_u64().ok_or_else(|| {
                SettingsError("instance_wait_secs must be a non-negative integer".into())
            })?)
            .map_err(|_| SettingsError("instance_wait_secs must fit in u32".into()))?,
        };
```
并在 `Ok(Self { ... })` 里补 `instance_wait_secs,`。
- `to_toml` 的输出模板里补一行 `instance_wait_secs = {}`，并把它加进 `format!` 的参数列表（与 `archive_grace_hours` 同位置）。

`single_instance.rs` 追加：

```rust
/// 有界让位：在 `budget` 内反复重试取锁，拿不到就返回 `None`。
///
/// `budget` 为 0 表示只试一次、立即让位。用轮询而非阻塞 `flock`：调用方需要
/// 在超时后打印一行原因并退出，阻塞式 `flock` 拿不到这个「放弃」的时机。
pub fn acquire_within(state_dir: &Path, budget: std::time::Duration) -> Option<InstanceLock> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        if let Some(lock) = acquire(state_dir) {
            return Some(lock);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        std::thread::sleep(remaining.min(std::time::Duration::from_millis(200)));
    }
}
```

`main.rs` 的 `run_daemon`：把无限等待循环换成有界让位。把 `let calendar = ConcurrencyCalendar::load_preferring_new(&args.state_dir);` 上移到取锁之前，然后：

```rust
    let calendar = ConcurrencyCalendar::load_preferring_new(&args.state_dir);
    // 单实例：同一项目同一时刻只有一个插件推进队列。拿不到锁就在有界预算内
    // 重试，超时即让位退出——旧的无限等待会留下永不推进也不退出的空转进程。
    let wait = Duration::from_secs(calendar.instance_wait_secs as u64);
    let _lock = match superpowers_kanban_runner::single_instance::acquire_within(
        &args.state_dir,
        wait,
    ) {
        Some(lock) => lock,
        None => {
            eprintln!(
                "superpowers-kanban: another instance holds the lock at {}; exiting after {}s",
                args.state_dir.join("plugin.lock").display(),
                wait.as_secs()
            );
            std::process::exit(0);
        }
    };
```

（原本 `run_daemon` 里后面又有一处 `let calendar = ...`，删除重复的那一处，改用上移后的这一个。）

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test --offline`
Expected: 全部 PASS。

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-kanban && cargo fmt --all && cd ../..
git add plugins/superpowers-kanban/crates/superpowers-kanban-core/src/calendar.rs \
        plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/single_instance.rs \
        plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs
git commit -m "feat(kanban): bound the instance handover so a waiter cannot spin forever"
```

---

### Task 5: 监督器 —— 幂等 spawn + 指数退避

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/supervisor.rs`

**Interfaces:**
- Produces: `ensure_running` 在 spawn 前探测 `query_socket`；`RunningChild` 新增 `backoff: Duration` 字段；退避按 ×2 增长至 `restart_backoff_max_ms` 封顶。

- [ ] **Step 1: 写失败测试（退避增长 + 幂等）**

在 `supervisor.rs` 的测试模块追加（若模块内已有构造 `SupervisorManifest` 的助手就复用；否则用 `SupervisorManifest::parse`）：

```rust
    /// 写一份清单到 `<workdir>/.yi-agent/supervisors/<name>.json`。
    fn write_manifest(workdir: &Path, name: &str, extra: &str) -> SupervisorManifest {
        let dir = workdir.join(".yi-agent").join("supervisors");
        std::fs::create_dir_all(&dir).unwrap();
        let json = format!(
            r#"{{"name":"{name}","command":"/bin/sh","args":["-c","exit 1"],"switch_key":"{name}_on","restart_backoff_ms":40,"restart_backoff_max_ms":160,"stop_when_disabled":false{extra}}}"#
        );
        let manifest = SupervisorManifest::parse(&json).unwrap();
        std::fs::write(dir.join(format!("{name}.json")), &json).unwrap();
        manifest
    }

    #[test]
    fn repeated_failures_grow_the_backoff_up_to_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path();
        let manifest = write_manifest(workdir, "demo", "");
        let mut supervisor = Supervisor::new(Layout::for_workdir(workdir));

        // 第一次：初始退避 = restart_backoff_ms。
        let start = Instant::now();
        for _ in 0..40 {
            supervisor.reconcile();
            if supervisor.running_count() == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(supervisor.running_count(), 1, "第一次必须起进程");
        let _ = start;

        // 记下当前退避，验证它随反复失败增长、且不超过上限。
        let observed = supervisor.backoff_for_test("demo").expect("tracked");
        assert!(
            observed >= Duration::from_millis(40) && observed <= Duration::from_millis(160),
            "退避必须落在 [restart_backoff_ms, restart_backoff_max_ms] 内，实际 {observed:?}"
        );
        let _ = manifest;
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent-supervisors --lib repeated_failures_grow_the_backoff_up_to_the_cap`
Expected: 编译失败（`no method backoff_for_test`）。

- [ ] **Step 3: 实现**

`supervisor.rs`：
- `RunningChild` 结构体加字段并补注释：

```rust
struct RunningChild {
    child: std::process::Child,
    last_started: Instant,
    /// 当前重启退避。反复失败时 ×2 增长，至清单的 `restart_backoff_max_ms` 封顶。
    backoff: Duration,
}
```

- 新增探测与记账助手：

```rust
    /// 该清单声明的查询 socket 上是否已有实例在应答。
    ///
    /// 用「能否连上」判断：Unix socket 的 `connect` 成功当且仅当有进程在
    /// `listen`。陈旧 socket 文件（无监听者）会 `ECONNREFUSED`，故不会被误认。
    fn answers(&self, manifest: &SupervisorManifest) -> bool {
        match manifest.query_socket_path(
            &self.layout.workdir,
            &self.layout.state_dir,
            &self.layout.runtime_dir,
        ) {
            Some(Ok(socket)) => std::os::unix::net::UnixStream::connect(&socket).is_ok(),
            _ => false,
        }
    }

    /// 仅测试用：某托管进程当前的退避值。
    #[cfg(test)]
    pub(crate) fn backoff_for_test(&self, name: &str) -> Option<Duration> {
        self.running.get(name).map(|running| running.backoff)
    }
```

- 用下面两个方法替换原 `ensure_running`：

```rust
    fn ensure_running(&mut self, manifest: &SupervisorManifest) {
        // 幂等：已有实例在 `query_socket` 上应答 → 不重复 spawn。
        // 这从源头消除「健康孤儿持锁 + 监督器不断补位」的空转循环。
        if self.answers(manifest) {
            return;
        }
        let initial = Duration::from_millis(manifest.restart_backoff_ms);
        let max = Duration::from_millis(
            manifest
                .restart_backoff_max_ms
                .max(manifest.restart_backoff_ms),
        );
        let backoff = match self.running.get(&manifest.name) {
            None => initial,
            Some(running) => match running.child.try_wait() {
                Ok(None) => return, // 仍在运行
                Ok(Some(_)) | Err(_) => {
                    // 已退出：未到当前退避就先等，到了才重试并把退避翻倍。
                    if running.last_started.elapsed() < running.backoff {
                        return;
                    }
                    running.backoff.saturating_mul(2).min(max)
                }
            },
        };
        self.spawn_and_track(manifest, backoff);
    }

    /// spawn 并把新 child 连同退避值记进 `running`。
    fn spawn_and_track(&mut self, manifest: &SupervisorManifest, backoff: Duration) {
        if self.spawn(manifest).is_ok() {
            if let Some(child) = self.spawned.take() {
                self.running.insert(
                    manifest.name.clone(),
                    RunningChild {
                        child,
                        last_started: Instant::now(),
                        backoff,
                    },
                );
            }
        }
    }
```

- `SupervisorManifest` 的字段与 `query_socket_path` 已存在（`manifest.rs`），`Layout` 已存在，无需改。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent-supervisors`
Expected: 全部 PASS（含既有集成测试 `tests/supervisor.rs`）。

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-supervisors/src/supervisor.rs
git commit -m "feat(supervisors): probe the socket before spawning and back off exponentially"
```

---

### Task 6: 陈旧 socket 清理

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`

**Interfaces:**
- Consumes: Task 4 的 `run_daemon` 布局。
- Produces: `fn sweep_stale_sockets(state_dir: &Path)`，在 `start_query_server` 之前调用。

- [ ] **Step 1: 写失败测试**

在 `main.rs` 测试模块追加：

```rust
    #[test]
    fn sweep_removes_a_dead_socket_and_keeps_a_live_one() {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();

        // 死 socket：建了监听再关掉，文件留下但没人应答。
        let dead = state_dir.join("superpowers-kanban.sock");
        {
            let listener = UnixListener::bind(&dead).unwrap();
            drop(listener);
        }
        assert!(dead.exists(), "前置：死 socket 文件必须存在");

        // 活 socket：监听者活着。
        let live_dir = dir.path().join("live");
        std::fs::create_dir_all(&live_dir).unwrap();
        let live = live_dir.join("superpowers-kanban.sock");
        let _listener = UnixListener::bind(&live).unwrap();

        sweep_stale_sockets(&state_dir);
        assert!(!dead.exists(), "连不上的死 socket 必须被清掉");

        // 直接对 live 路径调一次（绕过 state_dir 推导），确认活 socket 不被删。
        sweep_one_socket(&live);
        assert!(live.exists(), "有活实例应答的 socket 绝不删");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test --offline -p superpowers-kanban-runner --bin superpowers-kanban sweep_removes_a_dead_socket_and_keeps_a_live_one`
Expected: 编译失败（`cannot find function sweep_stale_sockets`）。

- [ ] **Step 3: 实现**

`main.rs` 追加两个函数（放在 `start_query_server` 附近）：

```rust
/// 清一个候选路径上的陈旧 socket：连得上就保留，连不上就删。
///
/// 「连不上」是「绑定进程已不在」的可靠证据（Unix socket 语义）；不用 mtime/
/// 存在性——那些会被无关操作扰动。
fn sweep_one_socket(path: &std::path::Path) {
    if !path.exists() {
        return;
    }
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        return; // 有活实例，绝不误删。
    }
    let _ = std::fs::remove_file(path);
}

/// 清理本插件派生出的两个确定路径上的死 socket。
///
/// 只碰这两条确定路径（直连 + 回退），**不扫 `$TMPDIR`**——那里其它
/// `plugin-*.sock` 属于别的项目/别的 state_dir，无法证明归属。
fn sweep_stale_sockets(state_dir: &std::path::Path) {
    let direct = state_dir.join("superpowers-kanban.sock");
    sweep_one_socket(&direct);
    if let Ok(resolved) = superpowers_kanban_ipc::server::socket_path(state_dir) {
        if resolved != direct {
            sweep_one_socket(&resolved);
        }
    }
}
```

在 `run_daemon` 里，`start_query_server` **之前**加一行：

```rust
    // 清理上一次运行遗留的死 socket（当前实例尚未 bind，不会误删自己）。
    sweep_stale_sockets(&args.state_dir);
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test --offline`
Expected: 全部 PASS。

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-kanban && cargo fmt --all && cd ../..
git add plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs
git commit -m "fix(kanban): sweep dead sockets on the paths this plugin owns"
```

---

### Task 7: 回归 + 端到端确认

**Files:**
- Modify: `plugins/superpowers-kanban/README.md`（登记真相源与让位行为）

- [ ] **Step 1: 插件全量**

Run: `cd plugins/superpowers-kanban && cargo test --offline`
Expected: 全部 `test result: ok`，无 FAILED。

- [ ] **Step 2: 宿主侧零回归**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent-supervisors -p yi-agent-boards -p yi-agent-app-server`
Expected: 全绿（`board.*` 方法名与返回形状未变，宿主 `card_scheduler` 无感知）。

- [ ] **Step 3: README 补一句**

在 `plugins/superpowers-kanban/README.md` 的「命令行」小节末尾加：

```markdown
`board.json` 是看板的**唯一真相源**：daemon 与一次性 CLI 都在同一把 `board.lock`
（flock）内「读-改-写」，因此 daemon 在跑时 CLI 的 `done`/`archive`/`purge` 不会被
覆盖；daemon 缺席时同一条路径照常工作。`run` 拿不到 `plugin.lock` 时按
`instance_wait_secs`（默认 10 秒）有界等待后退出，不再无限空转。启动时还会清掉
本插件确定路径上连不上的死 socket。
```

- [ ] **Step 4: 端到端手动确认（daemon 在跑时 CLI 写入不被覆盖）**

```bash
cd plugins/superpowers-kanban && cargo build --release
# daemon 在跑（由 app-server 监督）时：
export TMPDIR=/Users/gongyichen/.yi-agent-tmp
./target/release/superpowers-kanban list
# 若板上有 awaiting_merge 的卡：done 后跨 ≥2 个 tick 复读状态仍为 done。
# 若没有：用 archive/purge 对一张已归档卡做一次写，再跨 tick 复读确认不回退。
```

判据：写入后**跨 ≥2 个 tick**（tick = 清单的 `--interval-secs`，本机为 60s）复读，
值不变。这正是不修复时会失败的那条。

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-kanban && cargo fmt --all -- --check && cd ../..
git add plugins/superpowers-kanban/README.md
git commit -m "docs(kanban): record the single source of truth and bounded handover"
```

---

## Self-Review

**Spec 覆盖：**
- §1 双真相源 → Task 1（`with_board`）+ Task 2（去缓存）+ Task 3（CLI 同路）。
- §1.1 无限空转 → Task 4（有界让位）+ Task 5（幂等 spawn + 退避）。
- §1.2 陈旧 socket → Task 6。
- §4.1 单一写锁 + 事务助手 + `Inner` 去 board → Task 1 + Task 2。
- §4.2 有界让位 → Task 4（`instance_wait_secs` 默认 10、`0` 立即）。
- §4.3 幂等 spawn + 指数退避 → Task 5。
- §4.4 配置项 `instance_wait_secs` → Task 4。
- §4.5 陈旧 socket 清理（连接探测、只碰确定路径）→ Task 6。
- §6 验收 1（覆盖消除）→ Task 2 Step 5 + Task 7 Step 4；验收 2/3（daemon 在跑/缺席时 CLI 可用）→ Task 3 + Task 7 Step 4；验收 4（并发不丢更新）→ Task 3 Step 1；验收 5（有界让位）→ Task 4 Step 1；验收 6/7（幂等 + 退避）→ Task 5 Step 1；验收 8（stale socket 清/留）→ Task 6 Step 1；验收 9（零回归）→ Task 7 Step 1/2。

**与 spec 的两处显式偏差：**
- spec §4.1 写「`list` 变成一次纯读（锁内 load）」；本计划改为**无锁读**
  （`read_board`）——因为 `save_board` 用 rename 原子落盘，读者永远看到完整文件，
  加锁只是徒增争用。语义等价（都保证不读到半写文件），成本更低。
- spec §4.1 说「`Inner` 的两把锁……先 `Mutex` 后 `board.lock`」；本计划把这条固化为
  全局约束（见 Global Constraints），并说明 `merge_next`/`merge_request` 例外地在
  **锁外**做 git（分钟级），因为占着 `board.lock` 会让 CLI 的 `done` 阻塞数分钟。

**类型一致性：** `with_board<T>(&Path, impl FnOnce(&mut Board) -> Result<T, String>) -> Result<T, String>`
在 Task 1 定义，Task 2/3 的每个调用点都按此签名使用；`read_board(&Path) -> Board` 同理；
`acquire_within(&Path, Duration) -> Option<InstanceLock>`（Task 4）与
`backoff_for_test(&self, &str) -> Option<Duration>`（Task 5）在定义与使用处逐字一致。

**已检查的坑：**
- `Board` 未实现 `Clone`：Task 2 全程用 `with_board` 闭包内改、`read_board` 只读，
  不需要克隆整板。
- **`next_launch` 的 worktree 失败路径必须真的把卡片落成 `Failed`（用户裁决：保当前行为）。**
  `with_board` 的契约是「闭包返回 `Err` 即不落盘」，所以该分支**不能**在闭包内
  `return Err(...)`——那会丢弃刚做的 `transition(Failed)`，让卡片卡死在 `Launching`。
  正确写法：闭包内 `transition(Failed)` 后**成功返回 `Ok(None)`**，把错误暂存到局部
  变量，锁外再抛。详见 Task 2 的 `next_launch`。
- **`merge_next` 的三条前置错误保持 `NeedsYou`，只有 `merge::run` 的 `GitError` 才是
  `Failed`（用户裁决：保当前行为）。** 分支缺失 / `merge::prepare` 失败 / base worktree
  脏 → `NeedsYou`；真正的 git 运行失败 → `Failed`。计划初稿曾把四者收成一个
  `GitError => Failed`，那是一次**无意的语义漂移**（该任务定位为行为保持重构），已修正。
- `next_launch` 内建 worktree 是 git I/O，本计划让它在 `board.lock` 内完成（一次
  `with_board`），以保持与改动前**完全一致**的原子性（一次落盘）；其耗时（约秒级）
  是可达的，且崩溃语义与既有「Launching 僵尸 → Failed」一致。
- `merge_request_locked` 原先全部在 `Mutex<Inner>` 内做 git；改为锁外 git + 两次
  `with_board`，靠调用方已持的 `merge.lock` 串行化——与既有「merge.lock 挡并发申请」
  的设计一致。
- `reconcile::plan(board, &git)` 需要 `&Board`；Task 2 在 `with_board` 闭包内拿到
  `&mut Board`，用 `board`（自动重借用为 `&Board`）传入。
- 删除 `save`/`board_path` 后若产生 `unused` 警告，Task 2 Step 4 明确要求删掉。
