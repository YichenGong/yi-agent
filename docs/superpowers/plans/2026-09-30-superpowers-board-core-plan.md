# Superpowers 看板 — Plan 1：插件内核（卡片模型 / 状态机 / 并发日历 / 开关 / 提升校验）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实现 Superpowers 看板的纯逻辑内核：卡片与队列状态机、时段并发日历、两层开关解析、提升校验，全部无 I/O 副作用、可独立单测。

**Architecture:** 本 Plan 只建**插件自己的独立 Cargo workspace**（`plugins/superpowers-board/`，**不是** `yi-agent-rs` 的 workspace 成员），其中 `board-core` 是纯逻辑库。它不依赖 `yi-agent-store`、不碰 daemon、不做 IPC——因此可以完全独立地编译、测试、安装、卸载。IPC 与进程循环在 Plan 3。

**Tech Stack:** Rust 2024、`chrono`（日期/时区）、`serde`（配置与状态序列化）、`toml`（配置解析）。测试用内置 `#[cfg(test)]` + `tempfile`。

## Global Constraints

- 命名统一为 **Superpowers 看板**（Superpowers Board）；crate 名 `board-core`，目录 `plugins/superpowers-board/`。
- 并发上限的精确值（来自 spec §7，逐字）：
  - `default_max_tasks = 3`
  - 工作日（Mon–Fri）`09:00–24:00` → `max_tasks = 3`
  - 工作日（Mon–Fri）`00:00–09:00` → `max_tasks = 10`
  - 周六、周日全天 → `max_tasks = 10`
- 时段区间为**左闭右开**；`end = "24:00"` 表示到当日 23:59:59。
- 时段切换**不打断**正在运行的卡片：上限只约束**新启动**。
- 开关两层解析：**项目层覆盖全局层**；两层都缺失 → **默认关闭**。
- 任何配置文件缺失、不可读或损坏 → **回退默认并继续**，绝不 panic。
- 提交信息用 conventional commits，**不写** `Co-Authored-By`。
- 每个任务结束时 `cargo fmt` + `cargo test` 必须通过。

> **Spec 状态机补遗（本计划引入，spec 已同步）：** spec §6 的状态表原缺 `Paused` 与
> `Cancelled`，但 §10 明确要求"暂停/恢复、取消"操作。因此本计划的状态集为 8 个：
> `Queued`、`Running`、`NeedsYou`、`AwaitingMerge`、`Failed`、`Done`、`Paused`、`Cancelled`。
> **spec §6 已于本计划落地时补齐这两行（占用槽位均为"否"），spec 与实现现已一致。**

---
## 文件结构

| 文件 | 职责 |
|------|------|
| `plugins/superpowers-board/Cargo.toml` | 独立 workspace 定义（非 yi-agent-rs 成员） |
| `plugins/superpowers-board/crates/board-core/Cargo.toml` | 内核库的清单与依赖 |
| `plugins/superpowers-board/crates/board-core/src/lib.rs` | 模块导出与 crate 文档 |
| `plugins/superpowers-board/crates/board-core/src/card.rs` | 卡片、`CardId`、`CardState` 与状态转换合法性 |
| `plugins/superpowers-board/crates/board-core/src/board.rs` | 队列：入队、槽位、FIFO + 手动插队、状态迁移 |
| `plugins/superpowers-board/crates/board-core/src/calendar.rs` | 时段并发日历与配置解析 |
| `plugins/superpowers-board/crates/board-core/src/switch.rs` | 两层开关解析 |
| `plugins/superpowers-board/crates/board-core/src/promotion.rs` | 提升校验（spec + plan 成对存在） |

---

### Task 1: 独立 workspace 脚手架 + 卡片与状态机

**Files:**
- Create: `plugins/superpowers-board/Cargo.toml`
- Create: `plugins/superpowers-board/crates/board-core/Cargo.toml`
- Create: `plugins/superpowers-board/crates/board-core/src/lib.rs`
- Create: `plugins/superpowers-board/crates/board-core/src/card.rs`

**Interfaces:**
- Consumes: 无（首个任务）
- Produces:
  - `board_core::card::CardId(pub String)`
  - `board_core::card::CardState`（8 变体：`Queued`/`Running`/`NeedsYou`/`AwaitingMerge`/`Failed`/`Done`/`Paused`/`Cancelled`）
  - `CardState::occupies_slot(self) -> bool`
  - `CardState::is_terminal(self) -> bool`
  - `CardState::can_transition_to(self, next: CardState) -> bool`
  - `board_core::card::Card { id, spec_path, plan_path, state, enqueued_at, order, workdir }`
    （`workdir: Option<PathBuf>`，serde `default`；由插件在启动卡片前用 `git worktree add` 预建后写入）

- [ ] **Step 1: 建独立 workspace 与内核 crate 骨架**

`plugins/superpowers-board/Cargo.toml`：

```toml
[workspace]
resolver = "2"
members = ["crates/board-core"]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.85"
license = "MIT"

[workspace.dependencies]
chrono = { version = "0.4", features = ["serde"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
tempfile = "3"
```

`plugins/superpowers-board/crates/board-core/Cargo.toml`：

```toml
[package]
name = "board-core"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
chrono.workspace = true
serde.workspace = true
toml.workspace = true

[dev-dependencies]
serde_json.workspace = true
tempfile.workspace = true
```

`plugins/superpowers-board/crates/board-core/src/lib.rs`：

```rust
//! Superpowers 看板（Superpowers Board）插件内核。
//!
//! 纯逻辑：卡片模型、队列状态机、时段并发日历、两层开关解析、提升校验。
//! 本 crate 不做 I/O、不依赖 daemon、不做 IPC，因此可独立编译与测试。

pub mod card;
```

- [ ] **Step 2: 写失败的测试（状态与合法迁移）**

`plugins/superpowers-board/crates/board-core/src/card.rs` 末尾追加：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_running_occupies_a_slot() {
        assert!(CardState::Running.occupies_slot());
        for state in [
            CardState::Queued,
            CardState::NeedsYou,
            CardState::AwaitingMerge,
            CardState::Failed,
            CardState::Done,
            CardState::Paused,
            CardState::Cancelled,
        ] {
            assert!(!state.occupies_slot(), "{state:?} must not occupy a slot");
        }
    }

    #[test]
    fn terminal_states_are_done_failed_cancelled() {
        assert!(CardState::Done.is_terminal());
        assert!(CardState::Failed.is_terminal());
        assert!(CardState::Cancelled.is_terminal());
        assert!(!CardState::AwaitingMerge.is_terminal());
        assert!(!CardState::NeedsYou.is_terminal());
    }

    #[test]
    fn happy_path_transitions_are_allowed() {
        use CardState::*;
        assert!(Queued.can_transition_to(Running));
        assert!(Running.can_transition_to(AwaitingMerge));
        assert!(AwaitingMerge.can_transition_to(Done));
        assert!(Running.can_transition_to(NeedsYou));
        assert!(NeedsYou.can_transition_to(Queued));
        assert!(Running.can_transition_to(Failed));
    }

    #[test]
    fn needs_you_never_holds_a_slot_so_it_can_return_to_the_queue() {
        assert!(!CardState::NeedsYou.occupies_slot());
        assert!(CardState::NeedsYou.can_transition_to(CardState::Queued));
    }

    #[test]
    fn pause_and_resume_are_allowed_only_around_running() {
        use CardState::*;
        assert!(Running.can_transition_to(Paused));
        assert!(Paused.can_transition_to(Queued));
        assert!(!Paused.can_transition_to(AwaitingMerge));
    }

    #[test]
    fn cancel_is_allowed_from_every_non_terminal_state() {
        use CardState::*;
        for state in [Queued, Running, NeedsYou, AwaitingMerge, Paused] {
            assert!(state.can_transition_to(Cancelled), "{state:?} must be cancellable");
        }
        assert!(!Done.can_transition_to(Cancelled));
        assert!(!Cancelled.can_transition_to(Queued));
    }

    #[test]
    fn a_running_card_cannot_be_started_twice() {
        assert!(!CardState::Running.can_transition_to(CardState::Running));
    }
}
```

- [ ] **Step 3: 运行测试确认失败**

Run: `cd plugins/superpowers-board && cargo test -p board-core card::`
Expected: 编译失败，`CardState` / `CardId` 未定义（尚未实现）。

- [ ] **Step 4: 实现最小代码**

`plugins/superpowers-board/crates/board-core/src/card.rs`（在测试模块之前）：

```rust
use std::path::PathBuf;

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

/// 一张卡片 = 一个需求 = 一对 spec + plan 文件。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CardId(pub String);

impl CardId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl std::fmt::Display for CardId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 卡片状态。`Running` 是唯一占用并发槽位的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardState {
    Queued,
    Running,
    NeedsYou,
    AwaitingMerge,
    Failed,
    Done,
    Paused,
    Cancelled,
}

impl CardState {
    /// 只有 `Running` 占用并发槽位；其余状态一律让出槽位。
    pub fn occupies_slot(self) -> bool {
        matches!(self, CardState::Running)
    }

    /// 终态：不再有任何自动推进。
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            CardState::Done | CardState::Failed | CardState::Cancelled
        )
    }

    /// 状态迁移合法性。不在此表内的迁移一律拒绝。
    pub fn can_transition_to(self, next: CardState) -> bool {
        use CardState::*;
        if self.is_terminal() {
            return false;
        }
        if self == next {
            return false;
        }
        // 取消：任何非终态都可以。
        if next == Cancelled {
            return true;
        }
        match (self, next) {
            (Queued, Running) => true,
            (Running, AwaitingMerge) => true,
            (Running, NeedsYou) => true,
            (Running, Failed) => true,
            (Running, Paused) => true,
            (Paused, Queued) => true,
            (NeedsYou, Queued) => true,
            (AwaitingMerge, Done) => true,
            _ => false,
        }
    }
}

/// 队列中的一张卡片。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Card {
    pub id: CardId,
    pub spec_path: PathBuf,
    pub plan_path: PathBuf,
    pub state: CardState,
    pub enqueued_at: DateTime<Local>,
    /// 排序键：越小越靠前。手动插队会把它压到当前最小值之下（因此可为负）。
    pub order: i64,
    /// 该卡片会话要跑在哪个 worktree。入队时为 `None`；插件在启动前用
    /// `git worktree add` 预建好目录再填入。旧状态文件没有此字段 → 反序列化为 `None`。
    #[serde(default)]
    pub workdir: Option<PathBuf>,
}
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cd plugins/superpowers-board && cargo test -p board-core card::`
Expected: PASS（7 个测试）。

- [ ] **Step 6: 提交**

```bash
cd plugins/superpowers-board
cargo fmt --all
git add plugins/superpowers-board
git commit -m "feat(board): scaffold the superpowers board plugin core with the card state machine"
```

---

### Task 2: 队列与槽位调度（FIFO + 手动插队）

**Files:**
- Create: `plugins/superpowers-board/crates/board-core/src/board.rs`
- Modify: `plugins/superpowers-board/crates/board-core/src/lib.rs`

**Interfaces:**
- Consumes: `board_core::card::{Card, CardId, CardState}`
- Produces:
  - `board_core::board::Board::new() -> Board`
  - `Board::enqueue(&mut self, id: CardId, spec_path: PathBuf, plan_path: PathBuf, now: DateTime<Local>) -> &Card`
  - `Board::get(&self, id: &CardId) -> Option<&Card>`
  - `Board::running_count(&self) -> usize`
  - `Board::free_slots(&self, limit: u16) -> usize`
  - `Board::next_startable(&self) -> Option<CardId>`
  - `Board::start_due(&mut self, limit: u16) -> Vec<CardId>`
  - `Board::transition(&mut self, id: &CardId, next: CardState) -> Result<(), TransitionError>`
  - `Board::prioritize(&mut self, id: &CardId) -> Result<(), TransitionError>`
  - `board_core::board::TransitionError`

- [ ] **Step 1: 写失败的测试**

`plugins/superpowers-board/crates/board-core/src/board.rs`：

```rust
use chrono::{Local, TimeZone};

use crate::card::{Card, CardId, CardState};

#[cfg(test)]
mod tests {
    use super::*;

    fn at(day: u32, hour: u32) -> chrono::DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 10, day, hour, 0, 0)
            .single()
            .expect("valid local time")
    }

    fn board_with(ids: &[&str]) -> Board {
        let mut board = Board::new();
        for (index, id) in ids.iter().enumerate() {
            board.enqueue(
                CardId::new(*id),
                format!("{id}.spec.md").into(),
                format!("{id}.plan.md").into(),
                at(1, index as u32),
            );
        }
        board
    }

    #[test]
    fn starts_the_oldest_queued_card_first() {
        let mut board = board_with(&["a", "b", "c"]);
        let started = board.start_due(2);
        assert_eq!(
            started,
            vec![CardId::new("a"), CardId::new("b")],
            "FIFO by enqueue time"
        );
    }

    #[test]
    fn never_exceeds_the_slot_limit() {
        let mut board = board_with(&["a", "b", "c"]);
        board.start_due(1);
        assert_eq!(board.running_count(), 1);
        board.start_due(1);
        assert_eq!(board.running_count(), 1, "no free slot, nothing else starts");
    }

    #[test]
    fn when_a_running_card_finishes_its_slot_is_reused_by_the_next() {
        let mut board = board_with(&["a", "b", "c"]);
        board.start_due(1);
        board
            .transition(&CardId::new("a"), CardState::AwaitingMerge)
            .unwrap();
        assert_eq!(board.free_slots(1), 1);
        assert_eq!(board.start_due(1), vec![CardId::new("b")]);
    }

    #[test]
    fn needs_you_does_not_hold_the_queue_back() {
        let mut board = board_with(&["a", "b"]);
        board.start_due(1);
        board
            .transition(&CardId::new("a"), CardState::NeedsYou)
            .unwrap();
        assert_eq!(
            board.start_due(1),
            vec![CardId::new("b")],
            "a blocked card frees its slot immediately"
        );
    }

    #[test]
    fn prioritize_moves_a_card_to_the_front() {
        let mut board = board_with(&["a", "b", "c"]);
        board.prioritize(&CardId::new("c")).unwrap();
        assert_eq!(board.next_startable(), Some(CardId::new("c")));
    }

    #[test]
    fn an_illegal_transition_is_rejected() {
        let mut board = board_with(&["a"]);
        let error = board
            .transition(&CardId::new("a"), CardState::AwaitingMerge)
            .unwrap_err();
        assert_eq!(error, TransitionError::Illegal {
            from: CardState::Queued,
            to: CardState::AwaitingMerge,
        });
    }

    #[test]
    fn a_terminal_card_is_never_started_again() {
        let mut board = board_with(&["a", "b"]);
        board.start_due(1);
        board.transition(&CardId::new("a"), CardState::Failed).unwrap();
        board.start_due(1);
        assert_eq!(
            board.get(&CardId::new("a")).unwrap().state,
            CardState::Failed
        );
        assert_eq!(board.get(&CardId::new("b")).unwrap().state, CardState::Running);
    }

    #[test]
    fn a_paused_card_returns_to_the_queue() {
        let mut board = board_with(&["a"]);
        board.start_due(1);
        board.transition(&CardId::new("a"), CardState::Paused).unwrap();
        assert_eq!(board.running_count(), 0);
        board.transition(&CardId::new("a"), CardState::Queued).unwrap();
        assert_eq!(board.start_due(1), vec![CardId::new("a")]);
    }

    #[test]
    fn a_board_survives_a_serialization_round_trip() {
        // 控制面靠这个文件渲染看板：队列必须能完整地写出去再读回来。
        let mut board = board_with(&["a", "b"]);
        board.start_due(1);
        board
            .set_workdir(&CardId::new("a"), PathBuf::from("/w/a"))
            .unwrap();
        let json = serde_json::to_string(&board).unwrap();
        let restored: Board = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.len(), 2);
        assert_eq!(
            restored.get(&CardId::new("a")).unwrap().state,
            CardState::Running
        );
        assert_eq!(
            restored.get(&CardId::new("a")).unwrap().workdir,
            Some(PathBuf::from("/w/a"))
        );
        assert_eq!(restored.running_count(), 1);
    }

    #[test]
    fn a_card_without_a_workdir_field_still_deserializes() {
        // 前向兼容：早期落盘的 board.json 没有 workdir 字段，读回时必须是 None，
        // 而不是整份状态解析失败。
        // 注意：CardState 带 `#[serde(rename_all = "snake_case")]`（见 card.rs），
        // 所以线格式里的状态值是 `"queued"`，不是 `"Queued"`。
        let json = r#"{"cards":[{"id":"a","spec_path":"a.spec.md","plan_path":"a.plan.md","state":"queued","enqueued_at":"2026-10-01T00:00:00+08:00","order":0}],"next_order":1}"#;
        let board: Board = serde_json::from_str(json).unwrap();
        assert_eq!(board.get(&CardId::new("a")).unwrap().workdir, None);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd plugins/superpowers-board && cargo test -p board-core board::`
Expected: 编译失败，`Board` / `TransitionError` 未定义。

- [ ] **Step 3: 实现最小代码**

`plugins/superpowers-board/crates/board-core/src/board.rs`（在测试模块之前）：

```rust
use std::path::PathBuf;

use chrono::{DateTime, Local};

use crate::card::{Card, CardId, CardState};

/// 状态迁移被拒绝的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionError {
    UnknownCard(CardId),
    Illegal { from: CardState, to: CardState },
}

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransitionError::UnknownCard(id) => write!(f, "unknown card: {id}"),
            TransitionError::Illegal { from, to } => {
                write!(f, "illegal transition: {from:?} -> {to:?}")
            }
        }
    }
}

impl std::error::Error for TransitionError {}

/// 卡片队列。槽位只由 `CardState::Running` 占用。
///
/// `Serialize`/`Deserialize` 让插件进程能把队列原子落盘到 `<state-dir>/board.json`，
/// 控制面（TUI / desktop）再读同一个文件渲染看板——文件即契约，无需新增 IPC。
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Board {
    cards: Vec<Card>,
    next_order: i64,
}

impl Board {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enqueue(
        &mut self,
        id: CardId,
        spec_path: PathBuf,
        plan_path: PathBuf,
        now: DateTime<Local>,
    ) -> &Card {
        let order = self.next_order;
        self.next_order += 1;
        let index = self.cards.len();
        self.cards.push(Card {
            id,
            spec_path,
            plan_path,
            state: CardState::Queued,
            enqueued_at: now,
            order,
            workdir: None,
        });
        &self.cards[index]
    }

    pub fn get(&self, id: &CardId) -> Option<&Card> {
        self.cards.iter().find(|card| &card.id == id)
    }

    pub fn len(&self) -> usize {
        self.cards.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }

    pub fn running_count(&self) -> usize {
        self.cards
            .iter()
            .filter(|card| card.state.occupies_slot())
            .count()
    }

    /// 当前还有多少空槽位。`limit` 已由并发日历按时段算好。
    pub fn free_slots(&self, limit: u16) -> usize {
        (limit as usize).saturating_sub(self.running_count())
    }

    /// 记下某张卡片会话要跑在哪个 worktree。插件在启动前调用。
    pub fn set_workdir(
        &mut self,
        id: &CardId,
        workdir: PathBuf,
    ) -> Result<(), TransitionError> {
        let card = self
            .cards
            .iter_mut()
            .find(|card| &card.id == id)
            .ok_or_else(|| TransitionError::UnknownCard(id.clone()))?;
        card.workdir = Some(workdir);
        Ok(())
    }

    /// 队首（`order` 最小）的排队卡片。
    pub fn next_startable(&self) -> Option<CardId> {
        self.cards
            .iter()
            .filter(|card| card.state == CardState::Queued)
            .min_by_key(|card| card.order)
            .map(|card| card.id.clone())
    }

    /// 在 `limit` 之内启动尽可能多的排队卡片，返回本次启动的卡片（按启动顺序）。
    pub fn start_due(&mut self, limit: u16) -> Vec<CardId> {
        let mut started = Vec::new();
        while self.free_slots(limit) > 0 {
            let Some(next) = self.next_startable() else {
                break;
            };
            if let Some(card) = self.cards.iter_mut().find(|card| card.id == next) {
                card.state = CardState::Running;
                started.push(next);
            } else {
                break;
            }
        }
        started
    }

    /// 迁移一张卡片的状态，非法迁移一律拒绝且不改动队列。
    pub fn transition(
        &mut self,
        id: &CardId,
        next: CardState,
    ) -> Result<(), TransitionError> {
        let card = self
            .cards
            .iter_mut()
            .find(|card| &card.id == id)
            .ok_or_else(|| TransitionError::UnknownCard(id.clone()))?;
        if !card.state.can_transition_to(next) {
            return Err(TransitionError::Illegal {
                from: card.state,
                to: next,
            });
        }
        card.state = next;
        Ok(())
    }

    /// 手动插队：把该卡片的排序键压到当前最小值之下。
    pub fn prioritize(&mut self, id: &CardId) -> Result<(), TransitionError> {
        let min_order = self.cards.iter().map(|card| card.order).min().unwrap_or(0);
        let card = self
            .cards
            .iter_mut()
            .find(|card| &card.id == id)
            .ok_or_else(|| TransitionError::UnknownCard(id.clone()))?;
        if card.state.is_terminal() {
            return Err(TransitionError::Illegal {
                from: card.state,
                to: card.state,
            });
        }
        card.order = min_order.saturating_sub(1);
        Ok(())
    }
}
```

在 `lib.rs` 加入模块导出：

```rust
pub mod board;
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd plugins/superpowers-board && cargo test -p board-core board::`
Expected: PASS（10 个测试）。

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-board
cargo fmt --all
git add plugins/superpowers-board
git commit -m "feat(board): add the queue with slot-aware fifo scheduling and manual prioritization"
```

---

### Task 3: 时段并发日历

**Files:**
- Create: `plugins/superpowers-board/crates/board-core/src/calendar.rs`
- Modify: `plugins/superpowers-board/crates/board-core/src/lib.rs`

**Interfaces:**
- Consumes: 无
- Produces:
  - `board_core::calendar::ConcurrencyWindow { days: Vec<Weekday>, start: NaiveTime, end: NaiveTime, all_day: bool, max_tasks: u16 }`
  - `board_core::calendar::ConcurrencyCalendar { default_max_tasks: u16, windows: Vec<ConcurrencyWindow> }`
  - `ConcurrencyCalendar::from_toml(&str) -> Result<ConcurrencyCalendar, CalendarError>`
  - `ConcurrencyCalendar::load_or_default(path: &Path) -> ConcurrencyCalendar`
  - `ConcurrencyCalendar::limit_at(&self, now: DateTime<Local>) -> u16`

- [ ] **Step 1: 写失败的测试**

`plugins/superpowers-board/crates/board-core/src/calendar.rs`：

```rust
use std::path::Path;

use chrono::{DateTime, Local, NaiveTime, Weekday};

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

    fn at(month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, month, day, hour, minute, 0)
            .single()
            .expect("valid local time")
    }

    /// 2026-10-01 是周四，2026-10-03 是周六。
    const SPEC_CONFIG: &str = r#"
default_max_tasks = 3

[[window]]
days = "Mon-Fri"
start = "09:00"
end = "24:00"
max_tasks = 3

[[window]]
days = "Mon-Fri"
start = "00:00"
end = "09:00"
max_tasks = 10

[[window]]
days = "Sat,Sun"
all_day = true
max_tasks = 10
"#;

    #[test]
    fn workday_daytime_is_three() {
        let calendar = ConcurrencyCalendar::from_toml(SPEC_CONFIG).unwrap();
        assert_eq!(calendar.limit_at(at(10, 1, 9, 0)), 3);
        assert_eq!(calendar.limit_at(at(10, 1, 12, 30)), 3);
        assert_eq!(calendar.limit_at(at(10, 1, 23, 59)), 3);
    }

    #[test]
    fn workday_early_morning_is_ten() {
        let calendar = ConcurrencyCalendar::from_toml(SPEC_CONFIG).unwrap();
        assert_eq!(calendar.limit_at(at(10, 1, 0, 0)), 10);
        assert_eq!(calendar.limit_at(at(10, 1, 8, 59)), 10);
    }

    #[test]
    fn the_0900_boundary_is_exclusive_on_the_early_side() {
        let calendar = ConcurrencyCalendar::from_toml(SPEC_CONFIG).unwrap();
        assert_eq!(calendar.limit_at(at(10, 1, 8, 59)), 10);
        assert_eq!(calendar.limit_at(at(10, 1, 9, 0)), 3);
    }

    #[test]
    fn weekends_are_ten_all_day() {
        let calendar = ConcurrencyCalendar::from_toml(SPEC_CONFIG).unwrap();
        // 2026-10-03 is a Saturday, 2026-10-04 a Sunday.
        assert_eq!(calendar.limit_at(at(10, 3, 3, 0)), 10);
        assert_eq!(calendar.limit_at(at(10, 3, 14, 0)), 10);
        assert_eq!(calendar.limit_at(at(10, 4, 14, 0)), 10);
    }

    #[test]
    fn a_calendar_without_windows_falls_back_to_the_default() {
        let calendar = ConcurrencyCalendar::from_toml("default_max_tasks = 7").unwrap();
        assert_eq!(calendar.limit_at(at(10, 1, 12, 0)), 7);
    }

    #[test]
    fn broken_toml_is_rejected_by_from_toml() {
        assert!(ConcurrencyCalendar::from_toml("this is not toml = = =").is_err());
    }

    #[test]
    fn a_broken_file_loads_the_conservative_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kanban.toml");
        std::fs::write(&path, "this is not toml = = =").unwrap();
        let calendar = ConcurrencyCalendar::load_or_default(&path);
        assert_eq!(calendar.limit_at(at(10, 1, 12, 0)), 3);
        assert_eq!(calendar.default_max_tasks, 3);
    }

    #[test]
    fn a_missing_file_loads_the_conservative_default() {
        let calendar = ConcurrencyCalendar::load_or_default(Path::new("/nonexistent/kanban.toml"));
        assert_eq!(calendar.limit_at(at(10, 1, 12, 0)), 3);
        assert_eq!(calendar.default_max_tasks, 3);
    }

    #[test]
    fn all_day_windows_ignore_start_and_end() {
        let config = r#"
default_max_tasks = 1

[[window]]
days = "Wed"
all_day = true
max_tasks = 5
"#;
        let calendar = ConcurrencyCalendar::from_toml(config).unwrap();
        // 2026-10-07 is a Wednesday.
        assert_eq!(calendar.limit_at(at(10, 7, 0, 0)), 5);
        assert_eq!(calendar.limit_at(at(10, 7, 23, 59)), 5);
    }

    #[test]
    fn the_first_matching_window_wins() {
        let config = r#"
default_max_tasks = 1

[[window]]
days = "Thu"
start = "00:00"
end = "24:00"
max_tasks = 4

[[window]]
days = "Thu"
start = "12:00"
end = "13:00"
max_tasks = 99
"#;
        let calendar = ConcurrencyCalendar::from_toml(config).unwrap();
        assert_eq!(calendar.limit_at(at(10, 1, 12, 30)), 4, "earlier window wins");
    }

    #[test]
    fn limit_at_ignores_seconds() {
        let calendar = ConcurrencyCalendar::from_toml(SPEC_CONFIG).unwrap();
        let nine = at(10, 1, 9, 0).with_second(59).unwrap();
        assert_eq!(calendar.limit_at(nine), 3);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd plugins/superpowers-board && cargo test -p board-core calendar::`
Expected: 编译失败，`ConcurrencyCalendar` 未定义。

- [ ] **Step 3: 实现最小代码**

`plugins/superpowers-board/crates/board-core/src/calendar.rs`（在测试模块之前）：

```rust
use chrono::{DateTime, Local, NaiveTime, Weekday};
use serde::Deserialize;

/// 保守默认：配置缺失或损坏时使用。
pub const DEFAULT_MAX_TASKS: u16 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalendarError {
    Toml(String),
    Time(String),
    Day(String),
}

impl std::fmt::Display for CalendarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CalendarError::Toml(message) => write!(f, "invalid kanban.toml: {message}"),
            CalendarError::Time(message) => write!(f, "invalid time: {message}"),
            CalendarError::Day(message) => write!(f, "invalid day: {message}"),
        }
    }
}

impl std::error::Error for CalendarError {}

/// 一个并发窗口。区间为左闭右开；`all_day` 为真时忽略 `start`/`end`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConcurrencyWindow {
    pub days: Vec<Weekday>,
    pub start: NaiveTime,
    pub end: NaiveTime,
    pub all_day: bool,
    pub max_tasks: u16,
}

impl ConcurrencyWindow {
    fn matches(&self, now: DateTime<Local>) -> bool {
        use chrono::{Datelike, Timelike};
        if !self.days.contains(&now.weekday()) {
            return false;
        }
        if self.all_day {
            return true;
        }
        let time = NaiveTime::from_hms_opt(now.hour(), now.minute(), 0)
            .expect("valid time components");
        // 左闭右开；end 用 "24:00" 时归一为次日 00:00，等价于到 23:59:59。
        if self.end == NaiveTime::MIN {
            return time >= self.start;
        }
        time >= self.start && time < self.end
    }
}

/// 时段并发日历。`limit_at` 返回给定时刻允许的**任务级**并发上限。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConcurrencyCalendar {
    pub default_max_tasks: u16,
    pub windows: Vec<ConcurrencyWindow>,
}

impl Default for ConcurrencyCalendar {
    fn default() -> Self {
        Self {
            default_max_tasks: DEFAULT_MAX_TASKS,
            windows: Vec::new(),
        }
    }
}

impl ConcurrencyCalendar {
    /// 命中第一个匹配窗口；无命中取 `default_max_tasks`。
    pub fn limit_at(&self, now: DateTime<Local>) -> u16 {
        self.windows
            .iter()
            .find(|window| window.matches(now))
            .map(|window| window.max_tasks)
            .unwrap_or(self.default_max_tasks)
    }

    /// 解析 TOML 配置。
    pub fn from_toml(input: &str) -> Result<Self, CalendarError> {
        let file: CalendarFile =
            toml::from_str(input).map_err(|error| CalendarError::Toml(error.to_string()))?;
        let windows = file
            .window
            .into_iter()
            .map(RawWindow::into_window)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            default_max_tasks: file.default_max_tasks.unwrap_or(DEFAULT_MAX_TASKS),
            windows,
        })
    }

    /// 读取配置；文件缺失、不可读或损坏一律回退默认，绝不 panic。
    pub fn load_or_default(path: &std::path::Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml(&text).unwrap_or_else(|error| {
                tracing_fallback(&error, path);
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }
}

fn tracing_fallback(error: &CalendarError, path: &std::path::Path) {
    eprintln!(
        "superpowers board: {error}; falling back to default_max_tasks={DEFAULT_MAX_TASKS} ({})",
        path.display()
    );
}

#[derive(Debug, Deserialize)]
struct CalendarFile {
    default_max_tasks: Option<u16>,
    #[serde(default)]
    window: Vec<RawWindow>,
}

#[derive(Debug, Deserialize)]
struct RawWindow {
    days: Option<String>,
    start: Option<String>,
    end: Option<String>,
    #[serde(default)]
    all_day: bool,
    max_tasks: u16,
}

impl RawWindow {
    fn into_window(self) -> Result<ConcurrencyWindow, CalendarError> {
        let all_day = self.all_day;
        let days = self
            .days
            .as_deref()
            .map(parse_days)
            .transpose()?
            .unwrap_or_else(|| {
                // 未声明 days 视为每天。
                vec![
                    Weekday::Mon,
                    Weekday::Tue,
                    Weekday::Wed,
                    Weekday::Thu,
                    Weekday::Fri,
                    Weekday::Sat,
                    Weekday::Sun,
                ]
            });
        let (start, end) = if all_day {
            (NaiveTime::MIN, NaiveTime::MIN)
        } else {
            (
                parse_time(self.start.as_deref().unwrap_or("00:00"))?,
                parse_time(self.end.as_deref().unwrap_or("24:00"))?,
            )
        };
        Ok(ConcurrencyWindow {
            days,
            start,
            end,
            all_day,
            max_tasks: self.max_tasks,
        })
    }
}

/// "Mon-Fri,Sun" 形式：逗号分隔，每段可为 `X`、`X-Y` 或 `X..Y` 之外的简写。
fn parse_days(input: &str) -> Result<Vec<Weekday>, CalendarError> {
    let order = [
        Weekday::Mon,
        Weekday::Tue,
        Weekday::Wed,
        Weekday::Thu,
        Weekday::Fri,
        Weekday::Sat,
        Weekday::Sun,
    ];
    let mut indices = std::collections::BTreeSet::new();
    for part in input.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if let Some((from, to)) = part.split_once('-') {
            let from = weekday_index(from.trim())?;
            let to = weekday_index(to.trim())?;
            if from > to {
                return Err(CalendarError::Day(format!("reversed range: {part}")));
            }
            indices.extend(from..=to);
        } else {
            indices.insert(weekday_index(part)?);
        }
    }
    if indices.is_empty() {
        return Err(CalendarError::Day("no days given".into()));
    }
    Ok(indices.into_iter().map(|index| order[index]).collect())
}

fn weekday_index(name: &str) -> Result<usize, CalendarError> {
    match name.to_ascii_lowercase().as_str() {
        "mon" | "monday" => Ok(0),
        "tue" | "tuesday" => Ok(1),
        "wed" | "wednesday" => Ok(2),
        "thu" | "thursday" => Ok(3),
        "fri" | "friday" => Ok(4),
        "sat" | "saturday" => Ok(5),
        "sun" | "sunday" => Ok(6),
        other => Err(CalendarError::Day(other.to_string())),
    }
}

/// 解析 `HH:MM`；"24:00" 归一为 `NaiveTime::MIN`（表示当日末尾）。
fn parse_time(input: &str) -> Result<NaiveTime, CalendarError> {
    if input.trim() == "24:00" {
        return Ok(NaiveTime::MIN);
    }
    NaiveTime::parse_from_str(input.trim(), "%H:%M")
        .map_err(|error| CalendarError::Time(format!("{input}: {error}")))
}
```

> 注：`ConcurrencyWindow::matches` 用 `NaiveTime::MIN` 表示"当日末尾"（`24:00`），
> 与 `parse_time` 的归一化一致。

在 `lib.rs` 加入模块导出：

```rust
pub mod calendar;
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd plugins/superpowers-board && cargo test -p board-core calendar::`
Expected: PASS（12 个测试）。

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-board
cargo fmt --all
cargo clippy --all-targets -- -D warnings
git add plugins/superpowers-board
git commit -m "feat(board): add the time-of-day concurrency calendar"
```

---

### Task 4: 两层开关解析

**Files:**
- Create: `plugins/superpowers-board/crates/board-core/src/switch.rs`
- Modify: `plugins/superpowers-board/crates/board-core/src/lib.rs`

**Interfaces:**
- Consumes: 无
- Produces:
  - `board_core::switch::SwitchValue { Enabled, Disabled }`
  - `board_core::switch::BoardSwitch { Enabled, Disabled }`
  - `board_core::switch::resolve(global: Option<SwitchValue>, project: Option<SwitchValue>) -> BoardSwitch`
  - `board_core::switch::parse_switch_json(text: &str) -> Option<SwitchValue>`

- [ ] **Step 1: 写失败的测试**

`plugins/superpowers-board/crates/board-core/src/switch.rs`：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_project_layer_overrides_the_global_layer() {
        assert_eq!(
            resolve(Some(SwitchValue::Enabled), Some(SwitchValue::Disabled)),
            BoardSwitch::Disabled
        );
        assert_eq!(
            resolve(Some(SwitchValue::Disabled), Some(SwitchValue::Enabled)),
            BoardSwitch::Enabled
        );
    }

    #[test]
    fn a_missing_project_layer_inherits_the_global_layer() {
        assert_eq!(resolve(Some(SwitchValue::Enabled), None), BoardSwitch::Enabled);
        assert_eq!(
            resolve(Some(SwitchValue::Disabled), None),
            BoardSwitch::Disabled
        );
    }

    #[test]
    fn both_layers_missing_defaults_to_disabled() {
        assert_eq!(resolve(None, None), BoardSwitch::Disabled);
    }

    #[test]
    fn json_parsing_reads_the_board_key() {
        assert_eq!(
            parse_switch_json(r#"{"superpowers_board": true}"#),
            Some(SwitchValue::Enabled)
        );
        assert_eq!(
            parse_switch_json(r#"{"superpowers_board": false}"#),
            Some(SwitchValue::Disabled)
        );
    }

    #[test]
    fn a_broken_or_irrelevant_file_yields_none_instead_of_panicking() {
        assert_eq!(parse_switch_json("not json"), None);
        assert_eq!(parse_switch_json("{}"), None);
        assert_eq!(parse_switch_json(r#"{"other": 1}"#), None);
    }

    #[test]
    fn a_preferences_file_keeps_its_other_keys_untouched_on_read() {
        // 现有 preferences.json 里已有 subagent_runtime 等键，读取必须忽略它们。
        assert_eq!(
            parse_switch_json(r#"{"subagent_runtime":"always","superpowers_board":true}"#),
            Some(SwitchValue::Enabled)
        );
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd plugins/superpowers-board && cargo test -p board-core switch::`
Expected: 编译失败，`resolve` / `SwitchValue` 未定义。

- [ ] **Step 3: 实现最小代码**

`plugins/superpowers-board/crates/board-core/src/switch.rs`（在测试模块之前）：

```rust
use serde::Deserialize;

/// 单层偏好里记录的开关值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchValue {
    Enabled,
    Disabled,
}

/// 解析后生效的开关。
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

/// 两层解析：项目层覆盖全局层；两层都缺失 → 默认关闭。
pub fn resolve(global: Option<SwitchValue>, project: Option<SwitchValue>) -> BoardSwitch {
    match project.or(global) {
        Some(SwitchValue::Enabled) => BoardSwitch::Enabled,
        Some(SwitchValue::Disabled) | None => BoardSwitch::Disabled,
    }
}

/// 从 `preferences.json` 文本里读出 `superpowers_board` 键。
///
/// 文件损坏、缺键或类型不对一律返回 `None`（调用方视作"该层未设置"），绝不 panic。
/// 其他键（如 `subagent_runtime`）被忽略，因此可以安全读取现有偏好文件。
pub fn parse_switch_json(text: &str) -> Option<SwitchValue> {
    #[derive(Deserialize)]
    struct Preferences {
        superpowers_board: Option<bool>,
    }
    let parsed: Preferences = serde_json::from_str(text).ok()?;
    match parsed.superpowers_board? {
        true => Some(SwitchValue::Enabled),
        false => Some(SwitchValue::Disabled),
    }
}
```

> 需在 `board-core/Cargo.toml` 的 `[dependencies]` 加入 `serde_json.workspace = true`。

在 `lib.rs` 加入模块导出：

```rust
pub mod switch;
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd plugins/superpowers-board && cargo test -p board-core switch::`
Expected: PASS（8 个测试）。

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-board
cargo fmt --all
git add plugins/superpowers-board
git commit -m "feat(board): add two-layer board switch resolution"
```

---

### Task 5: 提升校验（spec + plan 成对存在）

**Files:**
- Create: `plugins/superpowers-board/crates/board-core/src/promotion.rs`
- Modify: `plugins/superpowers-board/crates/board-core/src/lib.rs`

**Interfaces:**
- Consumes: 无
- Produces:
  - `board_core::promotion::validate_promotion(spec: &Path, plan: &Path) -> Result<(), PromotionError>`
  - `board_core::promotion::PromotionError`

- [ ] **Step 1: 写失败的测试**

`plugins/superpowers-board/crates/board-core/src/promotion.rs`：

```rust
use std::path::Path;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_present_pair_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("feature.spec.md");
        let plan = dir.path().join("feature.plan.md");
        std::fs::write(&spec, "# spec").unwrap();
        std::fs::write(&plan, "# plan").unwrap();
        assert_eq!(validate_promotion(&spec, &plan), Ok(()));
    }

    #[test]
    fn a_missing_spec_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("missing.spec.md");
        let plan = dir.path().join("feature.plan.md");
        std::fs::write(&plan, "# plan").unwrap();
        assert_eq!(
            validate_promotion(&spec, &plan),
            Err(PromotionError::MissingSpec(spec.clone()))
        );
    }

    #[test]
    fn a_missing_plan_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("feature.spec.md");
        let plan = dir.path().join("missing.plan.md");
        std::fs::write(&spec, "# spec").unwrap();
        assert_eq!(
            validate_promotion(&spec, &plan),
            Err(PromotionError::MissingPlan(plan.clone()))
        );
    }

    #[test]
    fn the_same_path_for_both_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let both = dir.path().join("same.md");
        std::fs::write(&both, "x").unwrap();
        assert_eq!(
            validate_promotion(&both, &both),
            Err(PromotionError::SameFile(both.clone()))
        );
    }

    #[test]
    fn a_directory_in_place_of_a_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let spec_dir = dir.path().join("spec_dir");
        std::fs::create_dir(&spec_dir).unwrap();
        let plan = dir.path().join("feature.plan.md");
        std::fs::write(&plan, "# plan").unwrap();
        assert_eq!(
            validate_promotion(&spec_dir, &plan),
            Err(PromotionError::MissingSpec(spec_dir.clone()))
        );
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd plugins/superpowers-board && cargo test -p board-core promotion::`
Expected: 编译失败，`validate_promotion` / `PromotionError` 未定义。

- [ ] **Step 3: 实现最小代码**

`plugins/superpowers-board/crates/board-core/src/promotion.rs`（在测试模块之前）：

```rust
use std::path::{Path, PathBuf};

/// 拒绝提升的原因。判据是**文件存在**，而非模型声称已完成。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromotionError {
    MissingSpec(PathBuf),
    MissingPlan(PathBuf),
    SameFile(PathBuf),
}

impl std::fmt::Display for PromotionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromotionError::MissingSpec(path) => {
                write!(f, "spec file does not exist: {}", path.display())
            }
            PromotionError::MissingPlan(path) => {
                write!(f, "plan file does not exist: {}", path.display())
            }
            PromotionError::SameFile(path) => {
                write!(f, "spec and plan must be different files: {}", path.display())
            }
        }
    }
}

impl std::error::Error for PromotionError {}

/// 校验一对 spec + plan：两者都必须是存在的**文件**，且互不相同。
pub fn validate_promotion(spec: &Path, plan: &Path) -> Result<(), PromotionError> {
    if !spec.is_file() {
        return Err(PromotionError::MissingSpec(spec.to_path_buf()));
    }
    if !plan.is_file() {
        return Err(PromotionError::MissingPlan(plan.to_path_buf()));
    }
    if spec == plan {
        return Err(PromotionError::SameFile(spec.to_path_buf()));
    }
    Ok(())
}
```

在 `lib.rs` 加入模块导出：

```rust
pub mod promotion;
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd plugins/superpowers-board && cargo test -p board-core promotion::`
Expected: PASS（5 个测试）。

- [ ] **Step 5: 全量验证并提交**

```bash
cd plugins/superpowers-board
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
git add plugins/superpowers-board
git commit -m "feat(board): validate spec/plan pairs before promotion"
```

---

## 完成判据

- `cd plugins/superpowers-board && cargo test` 全绿（42 个测试：card 7 + board 10 + calendar 12 + switch 8 + promotion 5）。
- `cargo clippy --all-targets -- -D warnings` 无警告。
- `plugins/superpowers-board` **不在** `yi-agent-rs/Cargo.toml` 的 members 中（可独立编译）。
- 内核四个模块各自职责单一，均无 I/O 副作用（`load_or_default` 是唯一的文件读取，且失败回退）。
