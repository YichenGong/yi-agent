# 看板卡片归档与清理 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让看板卡片能结算与清理：人工合并后一句 `done <id>` 放行，真终止态过宽限期自动归档，终态卡能显式 `purge` 真删。

**Architecture:** 三层。①**core**：`Card` 加 `archived`/`terminal_at`，`Board` 加 `archive`/`purge`/`archive_due`/`visible`。②**CLI**：`done`（结算 `awaiting_merge`，核对推导出的 `kanban/<slug>` 分支）、`archive`、`purge`（仅限已归档）。③**推进循环**：每 tick 调 `archive_due`，真终止态超宽限期自动归档；`service.list()` 过滤归档卡，宿主与桌面随之不显示。

**Tech Stack:** Rust（`superpowers-kanban-core` / `-runner`：serde、chrono、std::process::Command 跑 git）；skill 文档为 Markdown。

**Spec:** `docs/superpowers/specs/2026-10-03-board-card-archive-design.md`

## Global Constraints

- 行号以基线 `e7952b4` 为准；改前 `grep` 复核（main 会前移）。
- 新字段一律 `#[serde(default)]`，旧 `board.json` 反序列化无损。
- **自动归档只对真终止态**（`done|failed|cancelled`）；`awaiting_merge`/`needs_you`/`running`/`queued` **永不**自动归档。
- **`purge` 只接受已归档卡**（未归档一律拒绝），不可逆。
- `archive` 拒绝仍在活跃工作的卡（`running`/`launching`/`merging`/`queued`）——隐藏在跑的卡会造成对账混乱。这是对 spec §4.1「任何状态可归档」的收窄（见 Self-Review 的偏差说明）。
- 宽限期默认 **24h**，可由 `<state_dir>/superpowers-kanban.toml` 的 `archive_grace_hours` 覆盖（`0` = 立即）。
- 测试用 `tempfile`，不碰真实 `$HOME`；git 相关用临时真仓库。
- 只显式 `git add <path>`；commit 前 `cargo fmt --all`（在插件目录）。
- 环境：`export PATH="/Users/gongyichen/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`、`export TMPDIR=/Users/gongyichen/.yi-agent-tmp`、`cargo --offline`。

---

## File Structure

- `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs` — `Card` 加 `archived`/`terminal_at`。
- `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/board.rs` — `archive`/`purge`/`purge_archived`/`archive_due`/`visible`；`transition` 记 `terminal_at`。
- `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs` — `done`/`archive`/`purge` 子命令 + `list --all` + `run_daemon` 的 sweep。
- `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs` — `archive_due` 包装 + `list()` 过滤归档。
- `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/merge.rs` — `branch_merged_into` 助手。
- `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/calendar.rs` — `archive_grace_hours`。
- `plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md` + `plugins/superpowers-kanban/README.md` — 文档。

---

### Task 1: core —— 归档字段与 Board 动作

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs`（`Card`）
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/board.rs`（动作 + `transition`）

**Interfaces:**
- Produces:
  - `Card.archived: bool`、`Card.terminal_at: Option<DateTime<Local>>`。
  - `Board::archive(&mut self, id: &CardId) -> Result<(), TransitionError>`
  - `Board::purge(&mut self, id: &CardId) -> Result<(), TransitionError>`
  - `Board::purge_archived(&mut self) -> usize`
  - `Board::archive_due(&mut self, now: DateTime<Local>, grace: chrono::Duration) -> Vec<CardId>`
  - `Board::visible(&self) -> impl Iterator<Item = &Card>`

- [ ] **Step 1: 写失败测试**

在 `card.rs` 测试模块追加：

```rust
    #[test]
    fn a_card_without_archive_fields_defaults_to_visible_and_no_terminal_time() {
        let json = r#"{"id":"a","spec_path":"a.spec.md","plan_path":"a.plan.md","state":"queued","enqueued_at":"2026-10-01T00:00:00+08:00","order":0}"#;
        let card: Card = serde_json::from_str(json).unwrap();
        assert!(!card.archived, "旧 board.json 缺字段必须回 false");
        assert_eq!(card.terminal_at, None);
    }
```

在 `board.rs` 测试模块追加：

```rust
    fn terminal_board(ids: &[&str]) -> Board {
        let mut board = board_with(ids);
        for id in ids {
            // queued -> running -> awaiting_merge -> done
            let card = CardId::new(*id);
            board.transition(&card, CardState::Running).unwrap();
            board.transition(&card, CardState::AwaitingMerge).unwrap();
            board.transition(&card, CardState::Done).unwrap();
        }
        board
    }

    #[test]
    fn entering_a_terminal_state_records_terminal_at() {
        let mut board = board_with(&["a"]);
        let a = CardId::new("a");
        board.transition(&a, CardState::Running).unwrap();
        assert!(board.get(&a).unwrap().terminal_at.is_none(), "running 不是终态");
        board.transition(&a, CardState::Cancelled).unwrap();
        assert!(board.get(&a).unwrap().terminal_at.is_some(), "终态要记时刻");
    }

    #[test]
    fn archive_hides_without_removing_and_refuses_active_cards() {
        let mut board = board_with(&["a", "b"]);
        let a = CardId::new("a");
        let b = CardId::new("b");
        // a 在队列中(活跃) → 拒绝归档
        assert!(board.archive(&a).is_err(), "queued 卡不该被归档");
        board.transition(&b, CardState::Running).unwrap();
        assert!(board.archive(&b).is_err(), "running 卡不该被归档");
        // b 到终态后可归档，且记录仍在
        board.transition(&b, CardState::Cancelled).unwrap();
        board.archive(&b).unwrap();
        assert!(board.get(&b).unwrap().archived);
        assert_eq!(board.visible().count(), 1, "归档卡不计入可见");
    }

    #[test]
    fn purge_only_accepts_archived_cards() {
        let mut board = terminal_board(&["a"]);
        let a = CardId::new("a");
        assert!(board.purge(&a).is_err(), "未归档不得 purge");
        board.archive(&a).unwrap();
        board.purge(&a).unwrap();
        assert!(board.get(&a).is_none(), "purge 后记录消失");
    }

    #[test]
    fn purge_archived_removes_only_archived() {
        let mut board = terminal_board(&["a", "b"]);
        board.archive(&CardId::new("a")).unwrap();
        assert_eq!(board.purge_archived(), 1);
        assert!(board.get(&CardId::new("a")).is_none());
        assert!(board.get(&CardId::new("b")).is_some(), "未归档卡保留");
    }

    #[test]
    fn archive_due_only_takes_terminal_cards_past_the_grace() {
        let mut board = board_with(&["a", "b"]);
        let a = CardId::new("a");
        let b = CardId::new("b");
        board.transition(&a, CardState::Running).unwrap();
        board.transition(&a, CardState::AwaitingMerge).unwrap(); // 非终态，需人决策
        board.transition(&b, CardState::Running).unwrap();
        board.transition(&b, CardState::Cancelled).unwrap();
        // 把 b 的 terminal_at 拨回 2 天前
        board.get_mut_for_test(&b).terminal_at =
            Some(Local::now() - chrono::Duration::days(2));

        let swept = board.archive_due(Local::now(), chrono::Duration::hours(24));
        assert_eq!(swept, vec![b.clone()], "只有超期终态被归档");
        assert!(board.get(&b).unwrap().archived);
        assert!(!board.get(&a).unwrap().archived, "awaiting_merge 永不自动归档");
    }
```

> 说明：`get_mut_for_test` 若不存在，则改为在 `board.rs` 测试模块内直接构造带 `terminal_at` 的卡（见 Step 3 的实现备注）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test --offline -p superpowers-kanban-core --lib board::tests::archive_due_only_takes_terminal_cards_past_the_grace`
Expected: 编译失败，`no method archive_due`。

- [ ] **Step 3: 实现**

`card.rs` 的 `Card` 在 `origin_card` 之后追加：

```rust
    /// 已归档：隐藏但仍保留在 board.json 里。旧状态文件缺字段 → false。
    #[serde(default)]
    pub archived: bool,
    /// 进入终态（done/failed/cancelled）的时刻；供宽限期自动归档判断。
    #[serde(default)]
    pub terminal_at: Option<DateTime<Local>>,
```

给 `Card { ... }` 的两处构造点（`board.rs::enqueue`、`enqueue_merge`）补 `archived: false, terminal_at: None,`。

`board.rs`：
- `transition` 在写入 `card.state = next;` 之后，若 `next.is_terminal()` 且 `card.terminal_at.is_none()`，则 `card.terminal_at = Some(Local::now());`。
- 新增方法（放在 `prioritize` 之后）：

```rust
    /// 归档：隐藏但不删除。拒绝仍在活跃工作的卡（隐藏在跑的卡会打乱对账）。
    pub fn archive(&mut self, id: &CardId) -> Result<(), TransitionError> {
        let card = self
            .cards
            .iter_mut()
            .find(|card| &card.id == id)
            .ok_or_else(|| TransitionError::UnknownCard(id.clone()))?;
        if matches!(
            card.state,
            CardState::Queued | CardState::Launching | CardState::Running | CardState::Merging
        ) {
            return Err(TransitionError::Illegal {
                from: card.state,
                to: card.state,
            });
        }
        card.archived = true;
        Ok(())
    }

    /// 清除：仅接受已归档的卡；未归档一律拒绝（不可逆，多一道闸）。
    pub fn purge(&mut self, id: &CardId) -> Result<(), TransitionError> {
        let index = self
            .cards
            .iter()
            .position(|card| &card.id == id)
            .ok_or_else(|| TransitionError::UnknownCard(id.clone()))?;
        if !self.cards[index].archived {
            return Err(TransitionError::Illegal {
                from: self.cards[index].state,
                to: self.cards[index].state,
            });
        }
        self.cards.remove(index);
        Ok(())
    }

    /// 清除所有已归档卡，返回数量。
    pub fn purge_archived(&mut self) -> usize {
        let before = self.cards.len();
        self.cards.retain(|card| !card.archived);
        before - self.cards.len()
    }

    /// 归档「已到终态且超过宽限期」的卡；返回被归档的 id。非终态永不触碰。
    pub fn archive_due(
        &mut self,
        now: DateTime<Local>,
        grace: chrono::Duration,
    ) -> Vec<CardId> {
        let mut swept = Vec::new();
        for card in self.cards.iter_mut() {
            if card.archived || !card.state.is_terminal() {
                continue;
            }
            let due = card
                .terminal_at
                .map(|at| now.signed_duration_since(at) > grace)
                .unwrap_or(false);
            if due {
                card.archived = true;
                swept.push(card.id.clone());
            }
        }
        swept
    }

    /// 未归档的卡（`list` 与宿主默认只看这些）。
    pub fn visible(&self) -> impl Iterator<Item = &Card> {
        self.cards.iter().filter(|card| !card.archived)
    }
```

- 测试里用的 `board.get_mut_for_test` 不存在——改为在 `board.rs` 测试模块内加一个私有助手：

```rust
    // 测试用：直接改某卡的 terminal_at。
    impl Board {
        #[cfg(test)]
        pub(crate) fn get_mut_for_test(&mut self, id: &CardId) -> &mut Card {
            self.cards.iter_mut().find(|c| &c.id == id).unwrap()
        }
    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test --offline -p superpowers-kanban-core --lib`
Expected: 全部 PASS。

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-kanban && cargo fmt --all && cd ../..
git add plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs \
        plugins/superpowers-kanban/crates/superpowers-kanban-core/src/board.rs
git commit -m "feat(board): archive and purge cards instead of only accumulating"
```

---

### Task 2: CLI —— `done` / `archive` / `purge` 与 `list --all`

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/merge.rs`（`branch_merged_into`）
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`（子命令 + `list --all`）
- Modify: `plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md`、`plugins/superpowers-kanban/README.md`

**Interfaces:**
- Consumes: Task 1 的 `Board::{archive,purge,purge_archived,visible}`。
- Produces: 子命令 `done <id>`、`archive <id>|--all-terminal`、`purge <id>|--all-archived`；`list --all`。
- `merge::branch_merged_into(project_root: &Path, source: &str, base: &str) -> bool`。

- [ ] **Step 1: 写失败测试（解析）**

在 `main.rs` 测试模块追加：

```rust
    #[test]
    fn done_archive_purge_parse_and_reject_extras() {
        assert!(parse(&["done", "card-1", "--state-dir", "/s"]).is_ok());
        assert!(parse(&["archive", "card-1"]).is_ok());
        assert!(parse(&["archive", "--all-terminal"]).is_ok());
        assert!(parse(&["purge", "card-1"]).is_ok());
        assert!(parse(&["purge", "--all-archived"]).is_ok());
        assert!(parse(&["done"]).is_err(), "done 缺 id");
        assert!(parse(&["archive", "a", "b"]).is_err(), "archive 多参数");
        assert!(parse(&["purge", "--all-archived", "card-1"]).is_err(), "互斥");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test --offline -p superpowers-kanban-runner --bin superpowers-kanban done_archive_purge_parse_and_reject_extras`
Expected: FAIL（`unknown subcommand: done`）。

- [ ] **Step 3: 实现**

`merge.rs` 追加：

```rust
/// `source` 是否已并入 `base`（即 base 是 source 的祖先或等于 source）。
pub fn branch_merged_into(project_root: &Path, source: &str, base: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["merge-base", "--is-ancestor", source, base])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
```

`main.rs`：
- `USAGE` 改为 `... <run|add|add-merge|done|archive|purge|list|on|off|workdir>`，并加三行说明。
- `Subcommand` 枚举加：

```rust
    Done {
        state_dir: PathBuf,
        card_id: String,
    },
    Archive {
        state_dir: PathBuf,
        card_id: Option<String>,
        all_terminal: bool,
    },
    Purge {
        state_dir: PathBuf,
        card_id: Option<String>,
        all_archived: bool,
    },
```

- `parse_subcommand` 加三个分支（`--all-terminal` / `--all-archived` 与 id 互斥）。
- dispatch 加三个分支，调 `command_done` / `command_archive` / `command_purge`。
- `command_list` 增 `all: bool` 参数：默认 `board.visible()`；`all` 时用 `board.cards()` 并在归档行尾加 ` [archived]`。`Subcommand::List` 也加 `all` 字段、解析 `--all`。

`command_done`（新）：

```rust
fn command_done(
    state_dir: &std::path::Path,
    card_id: &str,
    project_root: &std::path::Path,
) -> Result<String, String> {
    use superpowers_kanban_core::card::{CardId, CardState};
    let path = state_dir.join("board.json");
    let mut board = superpowers_kanban_runner::persist::load_board(&path);
    let id = CardId::new(card_id);
    match board.get(&id) {
        None => return Err(format!("unknown card: {card_id}")),
        Some(card) if card.state != CardState::AwaitingMerge => {
            return Err(format!(
                "card {card_id} is {:?}, not awaiting_merge",
                card.state
            ))
        }
        Some(_) => {}
    }
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
    board.transition(&id, CardState::Done).map_err(|e| e.to_string())?;
    superpowers_kanban_runner::persist::save_board(&path, &board).map_err(|e| e.to_string())?;
    Ok(format!(
        "card {card_id} -> done ({verdict}); auto-archives in 24h — \
         `superpowers-kanban archive {card_id}` to hide it now"
    ))
}
```

`command_archive` / `command_purge`（新）：load → 按 `card_id`/`--all-terminal`/`--all-archived` 调
`board.archive` / `board.purge` / `board.purge_archived`；`--all-terminal` 遍历
`board.cards()` 中 `state.is_terminal() && !archived` 的卡逐个 `archive` → save → 打印计数。
失败信息带原因。

`SKILL.md` 新增小节（放在「开关」之后）：

```markdown
## 确认合并（更新看板进度）

用户说「我合并了这张卡 / 确认合并 / 更新看板进度」时：

1. 确认这张卡确实在 `awaiting_merge`（`superpowers-kanban list`）。
2. 调 `superpowers-kanban done <card-id>`，把打印的核对结果**原样转述**：
   - `verified`（分支已并入）/ `branch-missing`（分支已删，按人工确认）/
     `not-merged`（分支在但未并入 —— 提示用户再确认）。
3. 说明「已放行，24h 后自动归档；要立刻隐藏可 `superpowers-kanban archive <card-id>`」。

**不要**代替用户执行 `git merge`——合并权归人，这里只登记「已合并」。
```

`README.md` 命令行小节补三动词与自动归档一句。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test --offline`
Expected: 全部 PASS（含既有解析/CLI 测试零回归）。

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-kanban && cargo fmt --all && cd ../..
git add plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs \
        plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/merge.rs \
        plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md \
        plugins/superpowers-kanban/README.md
git commit -m "feat(board): done/archive/purge verbs and a confirm-merge skill step"
```

---

### Task 3: 自动归档 + 列表过滤

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/calendar.rs`（`archive_grace_hours`）
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`（`archive_due` + `list()` 过滤）
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`（`run_daemon` 调 sweep）

**Interfaces:**
- Consumes: Task 1 的 `Board::archive_due`。
- Produces: `ConcurrencyCalendar::archive_grace_hours: u32`（默认 24）；`BoardService::archive_due(&self, now) -> Vec<CardId>`；`service.list()` 只回可见卡。

- [ ] **Step 1: 写失败测试**

`calendar.rs` 测试模块：

```rust
    #[test]
    fn archive_grace_defaults_to_24_and_can_be_overridden() {
        assert_eq!(ConcurrencyCalendar::default().archive_grace_hours, 24);
        let c = ConcurrencyCalendar::from_toml("default_max_tasks = 3\narchive_grace_hours = 0\n").unwrap();
        assert_eq!(c.archive_grace_hours, 0);
    }
```

`service.rs` 测试模块：

```rust
    #[test]
    fn list_hides_archived_cards_but_archive_due_sweeps_terminal_ones() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        // 一张终态卡，terminal_at 拨回 2 天前。
        let mut board = superpowers_kanban_core::board::Board::new();
        let id = superpowers_kanban_core::card::CardId::new("c1");
        board.enqueue(id.clone(), "c.spec.md".into(), "c.plan.md".into(), chrono::Local::now());
        board.transition(&id, superpowers_kanban_core::card::CardState::Running).unwrap();
        board.transition(&id, superpowers_kanban_core::card::CardState::Cancelled).unwrap();
        // 直接落盘后再载入，模拟宽限期已过。
        superpowers_kanban_runner::persist::save_board(&state_dir.join("board.json"), &board).unwrap();
        let service = BoardService::new(state_dir.clone(), dir.path().to_path_buf(), None);
        let swept = service.archive_due(chrono::Local::now() + chrono::Duration::days(3));
        assert_eq!(swept.len(), 1, "超期终态卡被自动归档");
        let listed = service.list();
        assert_eq!(listed["cards"].as_array().unwrap().len(), 0, "归档卡不出现在 list");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test --offline -p superpowers-kanban-core --lib calendar::tests::archive_grace_defaults_to_24_and_can_be_overridden`
Expected: FAIL（`no field archive_grace_hours`）。

- [ ] **Step 3: 实现**

`calendar.rs`：`ConcurrencyCalendar` 加 `pub archive_grace_hours: u32`，`Default` 置 24；
`CalendarFile` 加 `archive_grace_hours: Option<u32>`（`#[serde(default)]`），`from_toml` 里
`archive_grace_hours: file.archive_grace_hours.unwrap_or(24)`。

`service.rs`：
- 新增：

```rust
    /// 自动归档超期的真终止态卡；返回被归档的 id。
    pub fn archive_due(&self, now: DateTime<Local>) -> Vec<CardId> {
        // 宽限期取自项目自己的日历（`archive_grace_hours`，默认 24）。每次 tick 读一次，
        // 不缓存进 `Inner`——`Inner` 目前只放看板与租约，多一个字段要多一处同步。
        let hours = superpowers_kanban_core::calendar::ConcurrencyCalendar::load_preferring_new(
            &self.state_dir,
        )
        .archive_grace_hours;
        let grace = chrono::Duration::hours(hours.min(24 * 30) as i64);
        let mut inner = self.lock();
        let swept = inner.board.archive_due(now, grace);
        if !swept.is_empty() {
            let _ = persist::save_board(&self.board_path(), &inner.board);
        }
        swept
    }
```

  > 注意 `Inner` 现有字段只有 `board`/`leases`/`leases_dir`（`service.rs:41`），**没有**
  > `calendar_grace_hours`；不要新增该字段，按上面每次读日历即可。
- `list()`（`:418`）把 `inner.board.cards()` 改为 `inner.board.visible()`。

`main.rs` 的 `run_daemon` 循环里，紧跟 `merge_next()` 之后加：

```rust
        // 归档：真终止态过宽限期自动隐藏；awaiting_merge/needs_you 永不自动。
        let swept = service.archive_due(chrono::Local::now());
        if !swept.is_empty() {
            eprintln!("superpowers-kanban: archived {} finished card(s)", swept.len());
        }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test --offline`
Expected: 全部 PASS。

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-kanban && cargo fmt --all && cd ../..
git add plugins/superpowers-kanban/crates/superpowers-kanban-core/src/calendar.rs \
        plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs \
        plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs
git commit -m "feat(board): auto-archive finished cards and hide archived from list"
```

---

### Task 4: 回归 + 端到端确认

**Files:** 无代码改动（除非 Step 1/2 暴露格式问题）。

- [ ] **Step 1: 插件全量**

Run: `cd plugins/superpowers-kanban && cargo test --offline`
Expected: 全部 `test result: ok`，无 FAILED。

- [ ] **Step 2: 宿主 + 桌面零回归**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent-app-server`
Run: `cd desktop && npx tsc --noEmit && npx vitest run`
Expected: 全绿（`board.list` 出口过滤归档卡，宿主/桌面自动不显示，无需改前端）。

- [ ] **Step 3: 手动确认（可选，需重建插件二进制）**

```bash
cd plugins/superpowers-kanban && cargo build --release
# 用当前三张僵尸卡验证：
superpowers-kanban list                      # 三张 awaiting_merge
superpowers-kanban done 2026-10-03-board-smoke-spec-2026-10-03-board-smoke-plan
superpowers-kanban archive <id>              # 立刻隐藏
superpowers-kanban purge <id>                # 真删（已归档）
superpowers-kanban list                      # 干净
```

- [ ] **Step 4: 提交（如有修正）**

```bash
cd plugins/superpowers-kanban && cargo fmt --all && cd ../..
git add -u && git commit -m "chore: fmt after board archive work"
```

---

## Self-Review

**Spec 覆盖：**
- §4.1 数据（`archived`/`terminal_at`）+ Board 动作（`archive`/`purge`/`purge_archived`/`archive_due`/`visible`）+ `transition` 记 `terminal_at` → Task 1。
- §4.2 CLI 三动词（`done`/`archive`/`purge`）与 `done` 的三种核对结果 → Task 2。
- §4.3 自动归档（advance loop sweep + 宽限期可配）→ Task 3。
- §4.4 呈现（`list` 默认隐藏、`list --all`；`service.list()` 过滤使宿主/桌面自动隐藏）→ Task 2 + Task 3。
- §4.5 skill「确认合并」→ Task 2。
- §5 边界 / §6 验收 1–8 → Task 1–3 的单测逐条覆盖（`done` 仅 `awaiting_merge`；`purge` 仅已归档；`archive` 拒绝活跃；`archive_due` 不碰 `awaiting_merge`；旧 `board.json` 无损；`list` 默认不含归档）。

**与 spec 的偏差（显式）：**
- spec §4.1 写「任何状态都可归档（含 `awaiting_merge`）」；本计划**收窄为拒绝活跃态**
  （`queued`/`launching`/`running`/`merging`）——归档一张在跑的卡会把它从 `visible()` 里摘走，
  打乱调度器对账。`awaiting_merge`/`needs_you`/终态仍可归档，覆盖了「我已处理」的真实用法。
- spec 未规定 `archive` 的 CLI 形态细节；本计划定为 `<id> | --all-terminal`，与 `purge` 的
  `<id> | --all-archived` 对称。

**类型一致性：** `archived`/`terminal_at` 在 `Card`、`Board` 方法、`service.list()` 过滤处命名一致；
`archive_due` 的入参 `(now: DateTime<Local>, grace: chrono::Duration)` 在 core 与 service 包装间一致；
`branch_merged_into` 与既有 `source_branch_exists`/`default_branch` 同处 `merge.rs`。

**已检查的坑：**
- `Card { ... }` 字面量在 `board.rs` 有 2 处（`enqueue`/`enqueue_merge`），漏补新字段会编译失败。
- `Subcommand` 的 `List` 加 `all` 字段后，既有 `parse`/dispatch 处都要更新。
- 测试里 `get_mut_for_test` 需在 `board.rs` 测试模块内补（Step 3 已给）。
- `service.list()` 过滤后，桌面/宿主无需改动（它们只渲染收到的卡）。
