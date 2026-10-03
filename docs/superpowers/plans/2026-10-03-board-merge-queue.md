# 看板合并队列（Merge Cards + 每项目单合并闸）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给看板加「合并卡」——不写代码、只把源分支合进 base；与实现卡同队列严格 FIFO、不占并发名额，仅受每项目一把 `merge.lock` 约束；合并成功把配对实现卡送 `Done`，冲突停 `NeedsYou`。

**Architecture:** 延续 `core`（纯逻辑、无 I/O）/ `runner`（I/O）/ `dispatch`+`ipc`（只转发）分层。新增 `CardKind::Merge` 与 `CardState::Merging`；合并执行放在 runner 的 `merge.rs`（本地 `git merge --no-ff`），在 **base 被检出的那个 worktree** 里执行（主检出若正是 base，就在主检出里合——与 CLAUDE.md 的人工约定一致），绝不留下脏工作区。手动入口（CLI `add-merge` + 对话 skill）先做，自动派生与桌面/TUI 靠后。

**Tech Stack:** Rust 2024（plugin workspace：`plugins/superpowers-kanban/`，crates：`superpowers-kanban-core` / `-runner` / `-ipc`）；`libc` flock；`tempfile` 测试；宿主 `yi-agent-app-server`（Rust）；桌面 TS/React（`desktop/src/`）。

## Global Constraints

- Rust edition 2024，`rust-version = 1.85`；**不新增第三方依赖**（插件已依赖 `libc = "0.2"`、`chrono`、`serde`、`serde_json`、`toml`、`tempfile`）。
- `superpowers-kanban-core` **必须保持无 I/O、无进程调用**（纯逻辑，可独立测试）。任何 `git` 调用只能落在 `superpowers-kanban-runner`。
- 锁一律用 `flock`（内核在进程退出含 `SIGKILL` 时释放），**绝不**用 PID 文件；锁文件在释放时**不 unlink**。
- 反序列化向后兼容：所有新增 `Card` 字段用 `#[serde(default)]`，旧 `board.json` 必须能读回。
- 提交前在 `plugins/superpowers-kanban/` 下跑 `cargo fmt --all`；**不要**同时跑多个 `cargo test`。
- commit message 用 conventional commits，中文正文说明「为什么」；**不要**写 `Co-Authored-By`。
- 测试统一放各文件底部 `#[cfg(test)] mod tests`，与现有风格一致。

---

## File Structure

**Create**
- `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/merge.rs` —— 合并执行引擎：默认分支解析、base worktree 定位/创建、`source` 存在性、干净/冲突分类、清理。
- `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/merge_lock.rs` —— 每项目 `merge.lock`（flock）。

**Modify**
- `core/src/card.rs` —— `CardKind`、`Card` 新字段、`CardState::Merging`、迁移表、`enqueue_merge`。
- `core/src/board.rs` —— `claim_next_launch` 跳过合并卡、`next_startable` 只取实现卡、新增 `claim_next_merge`、`contains`、`next_free_merge_id`。
- `core/src/card_id.rs` —— 抽出 `slug`、新增 `merge_card_id_for`。
- `core/src/promotion.rs` —— 新增错误变体与 `validate_merge_refs`（纯校验）。
- `core/src/inbox.rs` —— 新增 `deliver_merge_card`。
- `core/src/switch.rs` —— 新增 `read_bool`（读任意布尔偏好键，供 `board_auto_merge`）。
- `runner/src/inbox.rs` —— `RawDelivery` 支持 `kind`/`source`/`base`/`origin_card`，按 kind 分派校验与入队。
- `runner/src/lib.rs` —— 导出 `merge`、`merge_lock`。
- `runner/src/service.rs` —— `merge_next()`、`mark_terminal` 自动派生、启动迁移收 `Merging` 僵尸、`list` 增字段。
- `runner/src/single_instance.rs` —— `acquire_merge`（或改由 `merge_lock.rs` 承担，见 Task 8）。
- `runner/src/main.rs` —— `add-merge` 子命令、`run` 循环调用 `merge_next`。
- `runner/src/dispatch.rs` —— 新方法 `enqueue_merge`。
- `plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md` —— 对话入口文档。
- `plugins/superpowers-kanban/README.md` —— CLI 与入口说明。
- `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` —— `BoardCard` 增 `kind`/`source`/`base` 解析。
- `desktop/src/lib/superpowersKanbanState.ts`（+ `.test.ts`）—— 放宽过滤，合并卡可见。

---

## Task 1: 卡片模型 —— `CardKind` 与合并字段

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/board.rs`（新增 `enqueue_merge`）
- Test: 两个文件底部 `mod tests`

**Interfaces:**
- Produces:
  - `pub enum CardKind { Implementation, Merge }`（`Default` = `Implementation`，`serde(rename_all="snake_case")`）
  - `Card` 新字段：`kind: CardKind`、`source_ref: Option<String>`、`base_ref: Option<String>`、`origin_card: Option<CardId>`（全 `#[serde(default)]`）
  - `Board::enqueue_merge(&mut self, id: CardId, source: String, base: String, origin: Option<CardId>, now: DateTime<Local>) -> &Card`

- [ ] **Step 1: 写失败测试（card.rs）**

```rust
    #[test]
    fn a_card_defaults_to_an_implementation_kind() {
        assert_eq!(CardKind::default(), CardKind::Implementation);
    }

    #[test]
    fn a_board_json_without_a_kind_reads_back_as_implementation() {
        // 旧 board.json 没有 kind/source_ref/base_ref/origin_card 字段。
        let json = r#"{"cards":[{"id":"a","spec_path":"a.spec.md","plan_path":"a.plan.md",
            "state":"queued","enqueued_at":"2026-10-01T00:00:00+08:00","order":0}],
            "next_order":1}"#;
        let board: crate::board::Board = serde_json::from_str(json).unwrap();
        let card = board.get(&CardId::new("a")).unwrap();
        assert_eq!(card.kind, CardKind::Implementation);
        assert_eq!(card.source_ref, None);
        assert_eq!(card.origin_card, None);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core card::`
Expected: 编译失败（`CardKind` 未定义）。

- [ ] **Step 3: 实现（card.rs）**

在 `CardId` 定义之后加：

```rust
/// 卡片种类。缺省（含旧状态文件）为实现卡。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardKind {
    #[default]
    Implementation,
    /// 只做分支合并，不跑会话、不写代码。
    Merge,
}
```

在 `Card` 结构体末尾追加：

```rust
    /// 卡片种类；旧状态文件没有此字段 → 实现卡。
    #[serde(default)]
    pub kind: CardKind,
    /// 合并卡的源分支。
    #[serde(default)]
    pub source_ref: Option<String>,
    /// 合并卡的目标分支。
    #[serde(default)]
    pub base_ref: Option<String>,
    /// 自动派生时指向配对的实现卡。
    #[serde(default)]
    pub origin_card: Option<CardId>,
```

在 `Board::enqueue` 的 `Card { … }` 字面量里补全字段：

```rust
            kind: CardKind::Implementation,
            source_ref: None,
            base_ref: None,
            origin_card: None,
```

在 `board.rs` 的 `impl Board` 里 `enqueue` 之后新增：

```rust
    /// 排入一张合并卡。载荷是 `source`/`base` 两个分支名；`origin` 仅自动派生时非空。
    pub fn enqueue_merge(
        &mut self,
        id: CardId,
        source: String,
        base: String,
        origin: Option<CardId>,
        now: DateTime<Local>,
    ) -> &Card {
        let order = self.next_order;
        self.next_order += 1;
        let index = self.cards.len();
        self.cards.push(Card {
            id,
            spec_path: PathBuf::new(),
            plan_path: PathBuf::new(),
            state: CardState::Queued,
            enqueued_at: now,
            order,
            workdir: None,
            task_id: None,
            thread_id: None,
            base_commit: None,
            kind: CardKind::Merge,
            source_ref: Some(source),
            base_ref: Some(base),
            origin_card: origin,
        });
        &self.cards[index]
    }
```

（`board.rs` 顶部 `use crate::card::{Card, CardId, CardState};` 改为含 `CardKind`。）

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core card:: board::`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add crates/superpowers-kanban-core/src/card.rs crates/superpowers-kanban-core/src/board.rs
git commit -m "feat(board): add a merge card kind with source/base payload"
```

---

## Task 2: 状态机 —— `Merging`

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs`
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: Task 1 的 `CardKind`（本任务不改它）。
- Produces: `CardState::Merging`；`occupies_slot()` 仍只认 `Running`；新迁移 `Queued→Merging`、`Merging→{Done,NeedsYou,Failed}`。

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn merging_does_not_occupy_a_provider_slot() {
        assert!(!CardState::Merging.occupies_slot());
    }

    #[test]
    fn a_merging_card_can_finish_or_stop() {
        use CardState::*;
        assert!(Queued.can_transition_to(Merging));
        assert!(Merging.can_transition_to(Done));
        assert!(Merging.can_transition_to(NeedsYou));
        assert!(Merging.can_transition_to(Failed));
        assert!(Merging.can_transition_to(Cancelled));
        // 合并卡绝不能回到会话通路。
        assert!(!Merging.can_transition_to(Running));
        assert!(!Merging.can_transition_to(Launching));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core card::`
Expected: 编译失败（无 `Merging`）。

- [ ] **Step 3: 实现**

在 `enum CardState` 的 `Running,` 之后插入 `Merging,`（保持 `#[serde(rename_all="snake_case")]`）。`occupies_slot()` **不动**（仍 `matches!(self, CardState::Running)`）。在 `can_transition_to` 的 `match (self, next)` 里，`(Queued, Running) => true,` 附近加：

```rust
            (Queued, Merging) => true,
            (Merging, Done) => true,
            (Merging, NeedsYou) => true,
            (Merging, Failed) => true,
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core card::`
Expected: PASS（含既有 `only_running_occupies_a_slot` 仍绿）。

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add crates/superpowers-kanban-core/src/card.rs
git commit -m "feat(board): add a Merging state for merge cards"
```

---

## Task 3: 队列选取 —— 会话通路跳过合并卡，合并通路单取一张

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/board.rs`
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: Task 1 `enqueue_merge`、Task 2 `Merging`。
- Produces:
  - `Board::claim_next_launch(&mut self, limit: u16) -> Option<CardId>` —— 现在**只**认领 `kind == Implementation`。
  - `Board::next_startable(&self) -> Option<CardId>` —— 同上。
  - `Board::claim_next_merge(&mut self, merge_busy: bool) -> Option<CardId>` —— `merge_busy` 为真返回 `None`；否则取 order 最小的 `Queued + Merge`，置 `Merging`。
  - `Board::contains(&self, id: &str) -> bool`

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn the_session_path_never_claims_a_merge_card() {
        let mut board = Board::new();
        board.enqueue_merge(
            CardId::new("m1"),
            "kanban/a".into(),
            "main".into(),
            None,
            at(1, 0),
        );
        board.enqueue(CardId::new("a"), "a.spec.md".into(), "a.plan.md".into(), at(1, 1));
        // 即使合并卡 order 更小、名额充足，会话通路也只能拿到实现卡。
        assert_eq!(board.claim_next_launch(3), Some(CardId::new("a")));
        assert_eq!(board.claim_next_launch(3), None);
    }

    #[test]
    fn a_busy_merge_gate_yields_nothing() {
        let mut board = Board::new();
        board.enqueue_merge(CardId::new("m1"), "s".into(), "main".into(), None, at(1, 0));
        assert_eq!(board.claim_next_merge(true), None);
        assert_eq!(board.claim_next_merge(false), Some(CardId::new("m1")));
        assert_eq!(board.get(&CardId::new("m1")).unwrap().state, CardState::Merging);
    }

    #[test]
    fn merge_cards_are_claimed_in_fifo_order_and_only_once() {
        let mut board = Board::new();
        board.enqueue_merge(CardId::new("m2"), "s2".into(), "main".into(), None, at(1, 2));
        board.enqueue_merge(CardId::new("m1"), "s1".into(), "main".into(), None, at(1, 1));
        assert_eq!(board.claim_next_merge(false), Some(CardId::new("m1")));
        assert_eq!(board.claim_next_merge(false), Some(CardId::new("m2")));
        assert_eq!(board.claim_next_merge(false), None);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core board::`
Expected: 编译失败（`claim_next_merge` 未定义）。

- [ ] **Step 3: 实现**

`next_startable` 的过滤条件由 `card.state == CardState::Queued` 改为：

```rust
            .filter(|card| card.state == CardState::Queued && card.kind == CardKind::Implementation)
```

（`start_due`、`claim_next_launch` 经它自动跳过合并卡。）新增：

```rust
    /// 队首可做合并的卡：`merge_busy` 为真（本项目已有合并在跑）时不出手。
    pub fn claim_next_merge(&mut self, merge_busy: bool) -> Option<CardId> {
        if merge_busy {
            return None;
        }
        let next = self
            .cards
            .iter()
            .filter(|card| card.state == CardState::Queued && card.kind == CardKind::Merge)
            .min_by_key(|card| card.order)
            .map(|card| card.id.clone())?;
        if let Some(card) = self.cards.iter_mut().find(|c| c.id == next) {
            card.state = CardState::Merging;
        }
        Some(next)
    }

    /// 看板上是否已有该 id（含终态），用于合并卡 id 去重。
    pub fn contains(&self, id: &str) -> bool {
        self.cards.iter().any(|card| card.id.0 == id)
    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core board::`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add crates/superpowers-kanban-core/src/board.rs
git commit -m "feat(board): claim merge cards on their own path, never via the session path"
```

---

## Task 4: 合并卡 id 与载荷校验（core，纯逻辑）

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card_id.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/promotion.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/board.rs`（`next_free_merge_id`）
- Test: 各文件 `mod tests`

**Interfaces:**
- Produces:
  - `card_id::merge_card_id_for(source: &str, base: &str) -> String`（`merge-<slug(source)>-into-<slug(base)>`）
  - `promotion::validate_merge_refs(source: &str, base: &str) -> Result<(), PromotionError>`
  - `PromotionError::{EmptyRef(String), SameRef(String)}`
  - `Board::next_free_merge_id(&self, source: &str, base: &str) -> CardId`

- [ ] **Step 1: 写失败测试**

`card_id.rs`：

```rust
    #[test]
    fn builds_a_merge_id_from_source_and_base() {
        assert_eq!(
            super::merge_card_id_for("kanban/card-1", "main"),
            "merge-kanban-card-1-into-main"
        );
        assert_eq!(super::merge_card_id_for("Feature/X", "release-2"), "merge-feature-x-into-release-2");
    }
```

`promotion.rs`：

```rust
    #[test]
    fn merge_refs_must_be_non_empty_and_different() {
        assert_eq!(validate_merge_refs("kanban/a", "main"), Ok(()));
        assert_eq!(
            validate_merge_refs("", "main"),
            Err(PromotionError::EmptyRef("source".into()))
        );
        assert_eq!(
            validate_merge_refs("main", "main"),
            Err(PromotionError::SameRef("main".into()))
        );
    }
```

`board.rs`：

```rust
    #[test]
    fn a_used_merge_id_gets_a_numeric_suffix() {
        let mut board = Board::new();
        board.enqueue_merge(CardId::new("merge-a-into-main"), "a".into(), "main".into(), None, at(1, 0));
        assert_eq!(board.next_free_merge_id("a", "main"), CardId::new("merge-a-into-main-2"));
        assert_eq!(board.next_free_merge_id("b", "main"), CardId::new("merge-b-into-main"));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core`
Expected: 编译失败（新函数未定义）。

- [ ] **Step 3: 实现**

`card_id.rs`：把 `card_id_for` 内的 `slug` 闭包提取为模块级函数并复用：

```rust
/// 路径/分支名 → 文件名友好的 slug：只留 ASCII 字母数字，其余折叠为一个 `-`。
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// 合并卡 id：`merge-<slug(source)>-into-<slug(base)>`。
pub fn merge_card_id_for(source: &str, base: &str) -> String {
    let source = slug(source);
    let base = slug(base);
    let source = if source.is_empty() { "source" } else { &source };
    let base = if base.is_empty() { "base" } else { &base };
    format!("merge-{source}-into-{base}")
}
```

`card_id_for` 内部改用 `slug(...)`（行为不变）。

`promotion.rs`：`PromotionError` 增两变体与 Display：

```rust
    EmptyRef(String),
    SameRef(String),
```

```rust
            PromotionError::EmptyRef(which) => write!(f, "{which} must not be empty"),
            PromotionError::SameRef(ref_) => write!(f, "source and base must differ: {ref_}"),
```

```rust
/// 合并卡载荷的**纯**校验：两个 ref 非空且互不相同。
/// 「source 分支是否存在」需要 git I/O，归 runner（`merge::source_branch_exists`）。
pub fn validate_merge_refs(source: &str, base: &str) -> Result<(), PromotionError> {
    if source.trim().is_empty() {
        return Err(PromotionError::EmptyRef("source".into()));
    }
    if base.trim().is_empty() {
        return Err(PromotionError::EmptyRef("base".into()));
    }
    if source == base {
        return Err(PromotionError::SameRef(source.to_string()));
    }
    Ok(())
}
```

`board.rs`：

```rust
    /// 未被占用的合并卡 id：与已有卡撞名时追加 `-2`、`-3`…。
    pub fn next_free_merge_id(&self, source: &str, base: &str) -> CardId {
        let base_id = crate::card_id::merge_card_id_for(source, base);
        if !self.contains(&base_id) {
            return CardId::new(base_id);
        }
        for n in 2u32.. {
            let candidate = format!("{base_id}-{n}");
            if !self.contains(&candidate) {
                return CardId::new(candidate);
            }
        }
        unreachable!("u32 suffix space exhausted")
    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add crates/superpowers-kanban-core/src/card_id.rs crates/superpowers-kanban-core/src/promotion.rs crates/superpowers-kanban-core/src/board.rs
git commit -m "feat(board): derive and validate merge card ids without spec/plan"
```

---

## Task 5: 投递与消费 —— 合并卡进 inbox

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/inbox.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/inbox.rs`
- Test: 各文件 `mod tests`

**Interfaces:**
- Consumes: Task 4 `validate_merge_refs`、Task 1 `enqueue_merge`。
- Produces:
  - `core::inbox::deliver_merge_card(state_dir, id, source, base, origin: Option<&str>) -> io::Result<()>`
  - `runner::inbox::consume` 识别 `kind == "merge"` 的投递并按 kind 入队。

- [ ] **Step 1: 写失败测试**

`core/inbox.rs`：

```rust
    #[test]
    fn delivers_a_merge_card_with_its_kind_and_refs() {
        let dir = tempfile::tempdir().unwrap();
        deliver_merge_card(dir.path(), "merge-a-into-main", "kanban/a", "main", None).unwrap();
        let text = std::fs::read_to_string(dir.path().join("inbox/merge-a-into-main.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["kind"], "merge");
        assert_eq!(value["source"], "kanban/a");
        assert_eq!(value["base"], "main");
    }
```

`runner/inbox.rs`：

```rust
    #[test]
    fn a_merge_delivery_is_queued_as_a_merge_card() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(
            inbox.join("m1.json"),
            r#"{"id":"m1","kind":"merge","source":"kanban/a","base":"main"}"#,
        )
        .unwrap();
        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, at());
        assert!(outcomes[0].result.is_ok(), "{:?}", outcomes[0]);
        let card = board.get(&CardId::new("m1")).unwrap();
        assert_eq!(card.kind, superpowers_kanban_core::card::CardKind::Merge);
        assert_eq!(card.source_ref.as_deref(), Some("kanban/a"));
    }

    #[test]
    fn a_merge_delivery_with_empty_refs_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(inbox.join("bad.json"), r#"{"id":"bad","kind":"merge","source":"","base":"main"}"#).unwrap();
        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, at());
        assert!(outcomes[0].result.is_err());
        assert!(dir.path().join("inbox/rejected/bad.json").exists());
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core inbox:: && cargo test -p superpowers-kanban-runner inbox::`
Expected: 编译失败（`deliver_merge_card` 未定义）。

- [ ] **Step 3: 实现**

`core/inbox.rs` 新增：

```rust
/// 投递一张合并卡：`{"id","kind":"merge","source","base"[,"origin_card"]}`。
pub fn deliver_merge_card(
    state_dir: &Path,
    id: &str,
    source: &str,
    base: &str,
    origin: Option<&str>,
) -> std::io::Result<()> {
    let dir = inbox_dir(state_dir);
    std::fs::create_dir_all(&dir)?;
    let path = enqueue_path(state_dir, id);
    let mut body = serde_json::json!({ "id": id, "kind": "merge", "source": source, "base": base });
    if let Some(origin) = origin {
        body["origin_card"] = serde_json::Value::String(origin.to_string());
    }
    let text = serde_json::to_string_pretty(&body).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}
```

`runner/inbox.rs`：`RawDelivery` 改为

```rust
#[derive(serde::Deserialize)]
struct RawDelivery {
    id: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    spec_path: Option<String>,
    #[serde(default)]
    plan_path: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    origin_card: Option<String>,
}
```

消费循环中，在 `board.get(&id).is_some()` 幂等分支之后、实现卡校验之前，插入合并分派：

```rust
        if delivery.kind.as_deref() == Some("merge") {
            let (source, base) = (
                delivery.source.clone().unwrap_or_default(),
                delivery.base.clone().unwrap_or_default(),
            );
            if let Err(error) =
                superpowers_kanban_core::promotion::validate_merge_refs(&source, &base)
            {
                reject(&inbox, &path, &delivery.id);
                outcomes.push(InboxOutcome { id: delivery.id, result: Err(error.to_string()) });
                continue;
            }
            let origin = delivery
                .origin_card
                .as_deref()
                .map(CardId::new);
            board.enqueue_merge(
                id,
                source,
                base,
                origin,
                now,
            );
            let _ = std::fs::remove_file(&path);
            outcomes.push(InboxOutcome { id: delivery.id, result: Ok(()) });
            continue;
        }
```

实现卡分支改为用 `delivery.spec_path.clone().unwrap_or_default()` / `plan_path`：

```rust
        let spec = delivery.spec_path.clone().unwrap_or_default();
        let plan = delivery.plan_path.clone().unwrap_or_default();
        if let Err(error) = validate_promotion(Path::new(&spec), Path::new(&plan)) { … }
        board.enqueue(id, spec.into(), plan.into(), now);
```

（注意：`id` 在 `enqueue_merge`/`enqueue` 里被 move，需在幂等分支之后用 `CardId::new(delivery.id.clone())`。）

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core inbox:: && cargo test -p superpowers-kanban-runner inbox::`
Expected: PASS（含既有实现卡投递测试）。

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add crates/superpowers-kanban-core/src/inbox.rs crates/superpowers-kanban-runner/src/inbox.rs
git commit -m "feat(board): deliver and consume merge cards through the inbox"
```

---

## Task 6: 每项目合并锁 `merge.lock`

**Files:**
- Create: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/merge_lock.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/lib.rs`
- Test: 新文件 `mod tests`

**Interfaces:**
- Produces: `superpowers_kanban_runner::merge_lock::acquire(state_dir: &Path) -> Option<MergeLock>`；`MergeLock` 在 drop 时经内核释放 flock。

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn one_project_admits_a_single_merger() {
        let dir = tempfile::tempdir().unwrap();
        let held = acquire(dir.path()).expect("first merger takes the gate");
        assert!(held.path().ends_with("merge.lock"));
        assert!(acquire(dir.path()).is_none(), "a second merger must wait");
        drop(held);
        // drop 释放经内核解 flock；有界轮询即可。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut released = false;
        while std::time::Instant::now() < deadline {
            if let Some(lock) = acquire(dir.path()) { drop(lock); released = true; break; }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(released, "the gate reopens after the holder drops it");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner merge_lock::`
Expected: 编译失败（模块不存在）。

- [ ] **Step 3: 实现**

`merge_lock.rs`（照抄 `single_instance.rs` 的 flock 套路，锁文件换 `merge.lock`）：

```rust
//! 每项目一把合并闸：同一项目任意时刻至多一个合并在执行。
//!
//! 与 `plugin.lock` 同理由：flock 由内核在持有进程退出（含 SIGKILL）时释放，
//! PID 文件做不到。锁文件在释放时**不 unlink**，否则会出现两个 inode 各持锁。

use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

pub struct MergeLock {
    _file: File,
    path: PathBuf,
}

impl MergeLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// 取 `<state_dir>/merge.lock` 的独占锁；已被占用返回 `None`。
pub fn acquire(state_dir: &Path) -> Option<MergeLock> {
    std::fs::create_dir_all(state_dir).ok()?;
    let path = state_dir.join("merge.lock");
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .ok()?;
    let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if outcome == 0 {
        Some(MergeLock { _file: file, path })
    } else {
        None
    }
}
```

`lib.rs` 加 `pub mod merge_lock;`。

（`runner/Cargo.toml` 已有 `libc` 依赖，无需改。）

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner merge_lock::`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add crates/superpowers-kanban-runner/src/merge_lock.rs crates/superpowers-kanban-runner/src/lib.rs
git commit -m "feat(board): add a per-project merge gate backed by flock"
```

---

## Task 7: 合并执行引擎 `merge.rs`

**Files:**
- Create: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/merge.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/lib.rs`
- Test: 新文件 `mod tests`（用真实临时 git 仓库）

**Interfaces:**
- Consumes: `worktree::slugify`（已存在）。
- Produces:
  - `pub fn default_branch(project_root: &Path) -> String`
  - `pub fn source_branch_exists(project_root: &Path, source: &str) -> bool`
  - `pub struct BaseWorktree { pub path: PathBuf, pub created: bool }`
  - `pub fn prepare(project_root: &Path, base: &str) -> Result<BaseWorktree, String>`
  - `pub fn is_dirty(wt: &Path) -> bool`
  - `pub fn run(wt: &Path, source: &str, base: &str) -> MergeOutcome`
  - `pub fn cleanup(wt: &BaseWorktree)`
  - `pub enum MergeOutcome { Merged, Conflict, GitError(String) }`

- [ ] **Step 1: 写失败测试**

```rust
    fn repo(dir: &Path) {
        let git = |args: &[&str]| {
            assert!(Command::new("git").arg("-C").arg(dir).args(args).status().unwrap().success());
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "b@t"]);
        git(&["config", "user.name", "B"]);
        std::fs::write(dir.join("f.txt"), "base\n").unwrap();
        git(&["add", "f.txt"]);
        git(&["commit", "-qm", "seed"]);
    }

    #[test]
    fn a_clean_merge_moves_the_base_and_leaves_it_clean() {
        let dir = tempfile::tempdir().unwrap();
        repo(dir.path());
        let git = |args: &[&str]| {
            assert!(Command::new("git").arg("-C").arg(dir.path()).args(args).status().unwrap().success());
        };
        git(&["checkout", "-qb", "feat/x"]);
        std::fs::write(dir.path().join("x.txt"), "x\n").unwrap();
        git(&["add", "x.txt"]);
        git(&["commit", "-qm", "feat"]);
        git(&["checkout", "-q", "main"]);
        assert!(source_branch_exists(dir.path(), "feat/x"));

        let base = prepare(dir.path(), "main").unwrap();
        assert_eq!(run(&base.path, "feat/x", "main"), MergeOutcome::Merged);
        assert!(dir.path().join("x.txt").exists(), "base worktree got the file");
        assert!(!is_dirty(&base.path), "the base worktree is clean after merging");
        cleanup(&base);
    }

    #[test]
    fn a_conflict_is_aborted_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        repo(dir.path());
        let git = |args: &[&str]| {
            assert!(Command::new("git").arg("-C").arg(dir.path()).args(args).status().unwrap().success());
        };
        git(&["checkout", "-qb", "feat/c"]);
        std::fs::write(dir.path().join("f.txt"), "one\n").unwrap();
        git(&["commit", "-qam", "c"]);
        git(&["checkout", "-q", "main"]);
        std::fs::write(dir.path().join("f.txt"), "two\n").unwrap();
        git(&["commit", "-qam", "m"]);

        let base = prepare(dir.path(), "main").unwrap();
        assert_eq!(run(&base.path, "feat/c", "main"), MergeOutcome::Conflict);
        assert!(!is_dirty(&base.path), "conflict must be aborted, not left half-merged");
        cleanup(&base);
    }

    #[test]
    fn a_missing_source_branch_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        repo(dir.path());
        assert!(!source_branch_exists(dir.path(), "kanban/nope"));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner merge::`
Expected: 编译失败（模块不存在）。

- [ ] **Step 3: 实现**

```rust
//! 合并执行引擎：把 source 合进 base。
//!
//! **在 base 被检出的 worktree 里执行**（若 base 正是项目主检出的分支，就在主检出里合），
//! 因此主检出不会因为「base 没检出」而被切换/污染——这正是 CLAUDE.md 里
//! 「回 main 做 git merge --no-ff」的人工约定。base 未被任何 worktree 检出时，
//! 才另建一个专用 worktree，用完移除。

use std::path::{Path, PathBuf};
use std::process::Command;

use superpowers_kanban_core::card_id::slug;

#[derive(Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    Merged,
    Conflict,
    GitError(String),
}

pub struct BaseWorktree {
    pub path: PathBuf,
    /// 这个 worktree 是本引擎新建的（用完可移除）；主检出复用时为 false。
    pub created: bool,
}

pub fn default_branch(project_root: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .output();
    if let Ok(out) = out {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if let Some(name) = text.strip_prefix("origin/") {
                if !name.is_empty() {
                    return name.to_string();
                }
            }
        }
    }
    "main".to_string()
}

pub fn source_branch_exists(project_root: &Path, source: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{source}")])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// base 已被检出的 worktree 路径（通常是主检出）。
fn worktree_for_branch(project_root: &Path, base: &str) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut current: Option<PathBuf> = None;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current = Some(PathBuf::from(path));
        } else if line == format!("branch refs/heads/{base}") {
            return current;
        }
    }
    None
}

/// 定位（或创建）执行合并的 worktree。创建失败视为配置问题（base 名写错等），返错误。
pub fn prepare(project_root: &Path, base: &str) -> Result<BaseWorktree, String> {
    if let Some(path) = worktree_for_branch(project_root, base) {
        return Ok(BaseWorktree { path, created: false });
    }
    let path = project_root
        .join(".worktrees")
        .join("kanban-merge")
        .join(slug(base));
    if path.join(".git").exists() {
        return Ok(BaseWorktree { path, created: true });
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["worktree", "add"])
        .arg(&path)
        .arg(base)
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "could not check out base '{base}': {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(BaseWorktree { path, created: true })
}

pub fn is_dirty(wt: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["status", "--porcelain"])
        .output()
        .map(|out| !String::from_utf8_lossy(&out.stdout).trim().is_empty())
        .unwrap_or(true)
}

pub fn run(wt: &Path, source: &str, base: &str) -> MergeOutcome {
    let message = format!("merge {source} into {base} (kanban)");
    let out = Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["merge", "--no-ff", source, "-m", &message])
        .output();
    let out = match out {
        Ok(out) => out,
        Err(error) => return MergeOutcome::GitError(format!("could not run git: {error}")),
    };
    if out.status.success() {
        return MergeOutcome::Merged;
    }
    // 失败：有未合并路径 = 冲突；一律先 abort 复原，保持 base worktree 干净可重试。
    let unmerged = Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["diff", "--name-only", "--diff-filter=U"])
        .output()
        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false);
    let _ = Command::new("git").arg("-C").arg(wt).args(["merge", "--abort"]).output();
    if unmerged {
        MergeOutcome::Conflict
    } else {
        MergeOutcome::GitError(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// 只移除本引擎创建的 worktree；复用主检出时绝不动它。
pub fn cleanup(wt: &BaseWorktree) {
    if !wt.created {
        return;
    }
    let _ = Command::new("git")
        .arg("-C")
        .arg(&wt.path)
        .args(["worktree", "remove", "--force"])
        .arg(&wt.path)
        .output();
}
```

`lib.rs` 加 `pub mod merge;`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner merge::`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add crates/superpowers-kanban-runner/src/merge.rs crates/superpowers-kanban-runner/src/lib.rs
git commit -m "feat(board): merge a source branch into its base in the base worktree"
```

---

## Task 8: 服务层 `merge_next` 与启动收僵尸

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: Tasks 3/6/7。
- Produces:
  - `BoardService::merge_next(&self) -> Result<Option<MergeClaim>, String>`，`MergeClaim { card_id: String, outcome: MergeOutcome }`
  - `migrate_legacy_running` 现同时把残留 `Merging` 卡收成 `NeedsYou`

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn a_merge_card_merges_its_source_and_sends_the_origin_card_to_done() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        // 造一个 source 分支：feat/x 在 main 之外加一个文件。
        let git = |args: &[&str]| {
            assert!(std::process::Command::new("git").arg("-C").arg(&project).args(args).status().unwrap().success());
        };
        git(&["checkout", "-qb", "feat/x"]);
        std::fs::write(project.join("x.txt"), "x\n").unwrap();
        git(&["add", "x.txt"]);
        git(&["commit", "-qm", "feat"]);
        git(&["checkout", "-q", "main"]);

        let service = service_with_card(&project);
        // 加一张实现卡（配对的 origin）与一张合并卡。
        service.enqueue_merge_card_for_test("m1", "feat/x", "main", Some("card-1"));
        service.mark_running("card-1", "thread-1").unwrap();
        service.mark_terminal("card-1", "awaiting_merge", None).unwrap();

        let claim = service.merge_next().unwrap().expect("a merge claim");
        assert_eq!(claim.card_id, "m1");
        let listed = service.list();
        let cards = listed["cards"].as_array().unwrap();
        let states: std::collections::HashMap<_, _> = cards
            .iter()
            .map(|c| (c["id"].as_str().unwrap().to_string(), c["state"].as_str().unwrap().to_string()))
            .collect();
        assert_eq!(states["m1"], "done");
        assert_eq!(states["card-1"], "done", "merge success drives the origin card to done");
    }
```

（`enqueue_merge_card_for_test` 在 Step 3 里作为 `#[cfg(test)]` 辅助加入 `impl BoardService`：直接改内存 board 调 `enqueue_merge` 并 save。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner service::`
Expected: 编译失败（`merge_next` 未定义）。

- [ ] **Step 3: 实现**

`use` 区加 `use superpowers_kanban_core::card::CardKind;` 与 `use crate::merge::{self, MergeOutcome};`。

`impl BoardService` 加：

```rust
    /// 试做下一张合并卡。拿不到每项目合并闸即返回 `None`（本项目已有合并在跑）。
    ///
    /// 全程持 `merge.lock`：认领 → 在 base worktree 里 `git merge --no-ff` → 回写终态。
    /// 合并成功且卡片带 `origin_card` 时，把那张实现卡一并送到 `Done`。
    pub fn merge_next(&self) -> Result<Option<MergeClaim>, String> {
        let Some(_gate) = crate::merge_lock::acquire(&self.state_dir) else {
            return Ok(None);
        };
        let mut inner = self.lock();
        let Some(card_id) = inner.board.claim_next_merge(false) else {
            return Ok(None);
        };
        let card = inner.board.get(&card_id).cloned().expect("just claimed");
        let source = card.source_ref.clone().unwrap_or_default();
        let base = card.base_ref.clone().unwrap_or_default();
        let origin = card.origin_card.clone();

        let finish = |inner: &mut Inner, next: CardState| {
            let _ = inner.board.transition(&card_id, next);
            if next == CardState::Done {
                if let Some(origin) = &origin {
                    let _ = inner.board.transition(origin, CardState::Done);
                }
            }
        };

        if !merge::source_branch_exists(&self.project_root, &source) {
            let detail = format!("source branch '{source}' does not exist");
            finish(&mut inner, CardState::NeedsYou);
            self.save(&inner);
            return Ok(Some(MergeClaim { card_id: card_id.0, outcome: MergeOutcome::GitError(detail) }));
        }
        let base_wt = match merge::prepare(&self.project_root, &base) {
            Ok(wt) => wt,
            Err(detail) => {
                finish(&mut inner, CardState::NeedsYou);
                self.save(&inner);
                return Ok(Some(MergeClaim { card_id: card_id.0, outcome: MergeOutcome::GitError(detail) }));
            }
        };
        if merge::is_dirty(&base_wt.path) {
            let detail = format!("base worktree {} is dirty", base_wt.path.display());
            merge::cleanup(&base_wt);
            finish(&mut inner, CardState::NeedsYou);
            self.save(&inner);
            return Ok(Some(MergeClaim { card_id: card_id.0, outcome: MergeOutcome::GitError(detail) }));
        }
        let outcome = merge::run(&base_wt.path, &source, &base);
        merge::cleanup(&base_wt);
        let next = match outcome {
            MergeOutcome::Merged => CardState::Done,
            MergeOutcome::Conflict => CardState::NeedsYou,
            MergeOutcome::GitError(_) => CardState::Failed,
        };
        finish(&mut inner, next);
        self.save(&inner);
        Ok(Some(MergeClaim { card_id: card_id.0, outcome }))
    }
```

顶部加：

```rust
pub struct MergeClaim {
    pub card_id: String,
    pub outcome: crate::merge::MergeOutcome,
}
```

在 `migrate_legacy_running` 里，`launching_zombies` 收完之后、`self.save(&inner)` 之前，追加：

```rust
        // 崩溃若发生在「置 Merging」之后、「回写终态」之前，会留下 Merging 僵尸。
        // 合并可能半途，需人确认，故收成 NeedsYou（不是 Failed）。先尽力 abort。
        let merging: Vec<CardId> = inner
            .board
            .cards()
            .iter()
            .filter(|card| card.state == CardState::Merging)
            .map(|card| card.id.clone())
            .collect();
        for id in merging {
            if let Some(card) = inner.board.get(&id).cloned() {
                if let Some(base) = card.base_ref.as_deref() {
                    if let Ok(wt) = merge::prepare(&self.project_root, base) {
                        let _ = std::process::Command::new("git")
                            .arg("-C")
                            .arg(&wt.path)
                            .args(["merge", "--abort"])
                            .output();
                        merge::cleanup(&wt);
                    }
                }
            }
            if inner.board.transition(&id, CardState::NeedsYou).is_ok() {
                inner.leases.remove(&id);
            }
        }
```

测试辅助（`impl BoardService` 内，带 `#[cfg(test)]`）：

```rust
    #[cfg(test)]
    pub(crate) fn enqueue_merge_card_for_test(
        &self,
        id: &str,
        source: &str,
        base: &str,
        origin: Option<&str>,
    ) {
        let mut inner = self.lock();
        inner.board.enqueue_merge(
            CardId::new(id),
            source.to_string(),
            base.to_string(),
            origin.map(CardId::new),
            chrono::Local::now(),
        );
        self.save(&inner);
    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner service::`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add crates/superpowers-kanban-runner/src/service.rs
git commit -m "feat(board): run one merge per project and drive the origin card to done"
```

---

## Task 9: CLI `add-merge` 与 `run` 循环接线

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`
- Modify: `plugins/superpowers-kanban/README.md`
- Test: `main.rs` `mod tests`

**Interfaces:**
- Consumes: Task 4 `merge_card_id_for`/`next_free_merge_id`、Task 5 `deliver_merge_card`、Task 7 `default_branch`/`source_branch_exists`、Task 8 `merge_next`。
- Produces: 子命令 `superpowers-kanban add-merge <source> [--base <ref>] [--state-dir <d>]`；`run` 循环每 tick 调一次 `merge_next`。

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn add_merge_requires_a_source() {
        assert!(parse(&["add-merge"]).is_err());
    }

    #[test]
    fn add_merge_parses_the_base_flag() {
        match parse(&["add-merge", "kanban/a", "--base", "develop", "--state-dir", "/s"]) {
            Ok(Subcommand::AddMerge { state_dir, source, base }) => {
                assert_eq!(source, "kanban/a");
                assert_eq!(base.as_deref(), Some("develop"));
                assert_eq!(state_dir.to_string_lossy(), "/s");
            }
            other => panic!("expected add-merge, got {other:?}"),
        }
    }

    #[test]
    fn add_merge_refuses_an_unknown_source_branch() {
        let dir = tempfile::tempdir().unwrap();
        // 初始化一个真实仓库，且没有 feat/x 分支。
        assert!(std::process::Command::new("git").arg("-C").arg(dir.path())
            .args(["init", "-q", "-b", "main"]).status().unwrap().success());
        let error = command_add_merge(dir.path(), "feat/x", Some("main"), dir.path())
            .expect_err("a missing source branch must be refused");
        assert!(error.contains("feat/x"), "{error}");
        assert!(!dir.path().join("inbox").exists(), "nothing delivered on failure");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --bin superpowers-kanban add_merge`
Expected: 编译失败（无 `AddMerge`）。

- [ ] **Step 3: 实现**

`Subcommand` 加变体：

```rust
    AddMerge {
        state_dir: PathBuf,
        source: String,
        base: Option<String>,
    },
```

`USAGE` 增行 `add-merge <source> [--base <ref>] [--state-dir <d>]`。`parse_subcommand` 增分支：

```rust
        Some("add-merge") => {
            let (state_dir, rest) = parse_state_dir(args)?;
            let mut rest = rest.into_iter();
            let source = rest.next().ok_or_else(|| format!("add-merge needs a source branch\n{USAGE}"))?;
            let mut base = None;
            while let Some(token) = rest.next() {
                if token == "--base" {
                    base = Some(rest.next().ok_or_else(|| "--base needs a value".to_string())?);
                } else {
                    return Err(format!("unexpected argument: {token}"));
                }
            }
            Ok(Subcommand::AddMerge { state_dir, source, base })
        }
```

（注意 `parse_state_dir` 只抽 `--state-dir`，`--base` 会留在 rest 里——上面的循环正确处理。）

新增：

```rust
/// `add-merge`：先校验 refs 与 source 分支存在，再投递一张合并卡。
fn command_add_merge(
    state_dir: &std::path::Path,
    source: &str,
    base: Option<&str>,
    project_root: &std::path::Path,
) -> Result<String, String> {
    let base = match base {
        Some(base) => base.to_string(),
        None => superpowers_kanban_runner::merge::default_branch(project_root),
    };
    superpowers_kanban_core::promotion::validate_merge_refs(source, &base)
        .map_err(|error| error.to_string())?;
    if !superpowers_kanban_runner::merge::source_branch_exists(project_root, source) {
        return Err(format!("source branch does not exist: {source}"));
    }
    // 与已有卡撞名时用 next_free_merge_id 派生唯一 id。
    let board = superpowers_kanban_runner::persist::load_board(&state_dir.join("board.json"));
    let id = board.next_free_merge_id(source, &base).0;
    superpowers_kanban_core::inbox::deliver_merge_card(state_dir, &id, source, &base, None)
        .map_err(|error| format!("could not deliver the card: {error}"))?;
    Ok(format!(
        "delivered {id} to {}\n{source} -> {base}",
        superpowers_kanban_core::inbox::inbox_dir(state_dir).display()
    ))
}
```

`main()` 的 `match subcommand` 加：

```rust
        Subcommand::AddMerge { state_dir, source, base } => {
            let project_root = superpowers_kanban_core::layout::project_root(&state_dir);
            command_add_merge(&state_dir, &source, base.as_deref(), &project_root)
        }
```

`run_daemon` 循环里，`consume_inbox` 的 `for` 之后追加：

```rust
        // 合并与实现共用一个 tick：消费投递后，若本项目没有合并在跑，就做一张。
        match service.merge_next() {
            Ok(Some(claim)) => eprintln!(
                "superpowers-kanban: merged {} ({:?})",
                claim.card_id, claim.outcome
            ),
            Ok(None) => {}
            Err(error) => eprintln!("superpowers-kanban: merge failed: {error}"),
        }
```

`README.md` 「加入看板」「命令行」两节补 `add-merge <source> [--base <ref>]` 的用法与示例。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --bin superpowers-kanban`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add crates/superpowers-kanban-runner/src/main.rs README.md
git commit -m "feat(board): add-merge CLI and run merges on the advance loop"
```

---

## Task 10: dispatch `enqueue_merge` 与 `list` 新字段

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/dispatch.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`（`list`）
- Test: 两个文件 `mod tests`

**Interfaces:**
- Consumes: Task 4/5/7/8。
- Produces:
  - `dispatch` 新方法 `enqueue_merge`，params `{source, base?}` → `{"id": "..."}`
  - `list` 每张卡新增 `kind`、`source`、`base`、`origin_card`

- [ ] **Step 1: 写失败测试**

`dispatch.rs`：

```rust
    #[test]
    fn enqueue_merge_refuses_a_missing_source_branch_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git").arg("-C").arg(dir.path())
            .args(["init", "-q", "-b", "main"]).status().unwrap().success());
        let state = dir.path().join(".yi-agent/superpowers-kanban");
        let error = dispatch_with_global(&state, None, "enqueue_merge",
            &json!({"source": "feat/x", "base": "main"})).unwrap_err();
        assert!(error.contains("feat/x"), "{error}");
        assert!(!superpowers_kanban_core::inbox::inbox_dir(&state).exists());
    }

    #[test]
    fn list_reports_the_merge_payload_and_nulls_for_implementation_cards() {
        let dir = tempfile::tempdir().unwrap();
        let (spec, plan) = write_pair(dir.path());
        let mut board = superpowers_kanban_core::board::Board::new();
        board.enqueue(superpowers_kanban_core::card::CardId::new("impl"), spec.into(), plan.into(), chrono::Local::now());
        board.enqueue_merge(superpowers_kanban_core::card::CardId::new("m1"), "kanban/a".into(), "main".into(), Some(superpowers_kanban_core::card::CardId::new("impl")), chrono::Local::now());
        crate::persist::save_board(&dir.path().join("board.json"), &board).unwrap();
        let value = dispatch_with_global(dir.path(), None, "list", &json!({})).unwrap();
        let cards = value["cards"].as_array().unwrap();
        let merge = cards.iter().find(|c| c["id"] == "m1").unwrap();
        assert_eq!(merge["kind"], "merge");
        assert_eq!(merge["source"], "kanban/a");
        assert_eq!(merge["origin_card"], "impl");
        let impl_card = cards.iter().find(|c| c["id"] == "impl").unwrap();
        assert_eq!(impl_card["kind"], "implementation");
        assert!(impl_card["source"].is_null());
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner dispatch::`
Expected: FAIL（未知方法 / 字段缺失）。

- [ ] **Step 3: 实现**

`service.rs` 的 `list()` JSON 里每张卡补：

```rust
                "kind": card.kind,
                "source": card.source_ref,
                "base": card.base_ref,
                "origin_card": card.origin_card.as_ref().map(|id| id.0.clone()),
```

`dispatch_with_service` 的 `match method` 加：

```rust
        "enqueue_merge" => {
            let source = params.get("source").and_then(Value::as_str)
                .ok_or_else(|| "enqueue_merge needs a `source`".to_string())?;
            let project_root = superpowers_kanban_core::layout::project_root(state_dir);
            let base = match params.get("base").and_then(Value::as_str) {
                Some(base) => base.to_string(),
                None => crate::merge::default_branch(&project_root),
            };
            superpowers_kanban_core::promotion::validate_merge_refs(source, &base)
                .map_err(|error| error.to_string())?;
            if !crate::merge::source_branch_exists(&project_root, source) {
                return Err(format!("source branch does not exist: {source}"));
            }
            let id = service.next_free_merge_id(source, &base);
            deliver_merge_card(state_dir, &id, source, &base, None)
                .map_err(|error| format!("could not deliver the card: {error}"))?;
            Ok(json!({ "id": id }))
        }
```

`service.rs` 加：

```rust
    /// 看板上未被占用的合并卡 id（供 dispatch / CLI 共用）。
    pub fn next_free_merge_id(&self, source: &str, base: &str) -> String {
        let inner = self.lock();
        inner.board.next_free_merge_id(source, base).0
    }
```

（`deliver_merge_card` 加进 dispatch 的 `use superpowers_kanban_core::inbox::{deliver_card, deliver_merge_card};`。）

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add crates/superpowers-kanban-runner/src/dispatch.rs crates/superpowers-kanban-runner/src/service.rs
git commit -m "feat(board): expose enqueue_merge and the merge payload over the plugin socket"
```

---

## Task 11: 对话入口 —— 更新 superpowers-kanban skill

**Files:**
- Modify: `plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md`
- Modify: `plugins/superpowers-kanban/README.md`

**Interfaces:**
- Consumes: Task 9 的 `add-merge`。

- [ ] **Step 1: 加「合并入队」小节**

在 skill 的「怎么做」里，`### 2. 入队` 之后插入：

```markdown
### 2b. 合并入队

用户说「把 <分支> 合进 <base>」「合并这个分支」「把这个分支 merge 了」时，投递一张**合并卡**：

```bash
superpowers-kanban add-merge <source> [--base <ref>] [--state-dir <dir>]
```

- `--base` 缺省取仓库默认分支（`origin/HEAD`，退化到 `main`）。
- 命令**立刻**校验：refs 非空且不同、`<source>` 分支在仓库里存在；失败原样转述错误，
  **不要**猜分支名、不要重试。
- 成功打印 `delivered <card-id> ...`，据此跟用户复述「第几张卡」。

合并卡与实现卡共用同一张看板队列、**不占**并发名额，且**同一项目同时只有一个合并**在执行
（其余排队）。它只做本地 `git merge --no-ff`，不消耗模型调用。冲突时卡停在 `NeedsYou`
等人处理，绝不自动解冲突。
```

- [ ] **Step 2: 更新职责边界措辞**

在「你的职责边界」的「只入队」一段末尾补一句：

```markdown
合并卡同样**只入队**：你只负责投递，执行与串行由插件进程负责。
```

- [ ] **Step 3: 更新 README**

README「加入看板」补一条：`superpowers-kanban add-merge <source> [--base <ref>]`；
「命令行」小节把它并入 `<run|add|add-merge|list|on|off|workdir>` 列表。

- [ ] **Step 4: 校验 spec/plan 未被改动**

Run: `cd plugins/superpowers-kanban && git diff --stat`
Expected: 只涉及 `skills/superpowers-kanban/SKILL.md` 与 `README.md`。

- [ ] **Step 5: 提交**

```bash
git add plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md plugins/superpowers-kanban/README.md
git commit -m "docs(board): teach the kanban skill to enqueue merge cards"
```

（部署：把更新后的 skill 复制到 `~/.yi-agent/skills/superpowers-kanban/SKILL.md`——这是安装动作，不属于本任务提交。）

---

## Task 12: 自动派生合并卡（开关 `board_auto_merge`，默认关）

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/switch.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`
- Test: 两个文件 `mod tests`

**Interfaces:**
- Produces:
  - `core::switch::read_bool(path: &Path, key: &str) -> Option<bool>`
  - `BoardService::mark_terminal` 在 `awaiting_merge` 且偏好开启时，就地派生一张合并卡（`source = kanban/<slug(card)>`、`base = 默认分支`、`origin_card = 该卡`）。

- [ ] **Step 1: 写失败测试**

`switch.rs`：

```rust
    #[test]
    fn reads_an_arbitrary_boolean_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, r#"{"board_auto_merge":true}"#).unwrap();
        assert_eq!(read_bool(&path, "board_auto_merge"), Some(true));
        assert_eq!(read_bool(&path, "missing"), None);
    }
```

`service.rs`：

```rust
    #[test]
    fn awaiting_merge_derives_a_merge_card_only_when_the_preference_is_on() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        // 默认关：不派生。
        service.mark_terminal(&claim.card_id, "awaiting_merge", None).unwrap();
        assert_eq!(service.list()["cards"].as_array().unwrap().len(), 1);

        // 打开开关：再收一张卡时会派生合并卡。
        let pref = superpowers_kanban_core::layout::project_preferences_path(&service.state_dir);
        superpowers_kanban_core::switch::write_bool(&pref, "board_auto_merge", true).unwrap();
        service.enqueue_impl_card_for_test("card-2");
        service.mark_terminal("card-2", "awaiting_merge", None).unwrap();
        let cards = service.list()["cards"].as_array().unwrap().clone();
        assert!(
            cards.iter().any(|c| c["kind"] == "merge" && c["origin_card"] == "card-2"),
            "a merge card must be derived for card-2: {cards:?}"
        );
    }
```

（`enqueue_impl_card_for_test` 同 Task 8 的测试辅助风格，`#[cfg(test)]`。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core switch:: && cargo test -p superpowers-kanban-runner service::`
Expected: 编译失败（`read_bool`/`write_bool` 未定义）。

- [ ] **Step 3: 实现**

`switch.rs` 加：

```rust
/// 读任意布尔偏好键（如 `board_auto_merge`）。缺失/损坏/类型不符一律 `None`。
pub fn read_bool(path: &Path, key: &str) -> Option<bool> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value.get(key)?.as_bool()
}

/// 写任意布尔偏好键（读-改-写，保留其他键；temp + rename 原子替换）。
pub fn write_bool(path: &Path, key: &str, value: bool) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut object = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    object.insert(key.to_string(), serde_json::Value::Bool(value));
    let body = serde_json::to_string_pretty(&serde_json::Value::Object(object))
        .map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}
```

`service.rs`：`mark_terminal` 在 `self.save(&inner)` 之前插入自动派生（仅对实现卡、且 outcome 为 `awaiting_merge`）：

```rust
        // 自动派生：实现卡到达 AwaitingMerge 且偏好开启时，就地排一张配对的合并卡。
        if next == CardState::AwaitingMerge {
            let pref =
                superpowers_kanban_core::layout::project_preferences_path(&self.state_dir);
            if superpowers_kanban_core::switch::read_bool(&pref, "board_auto_merge")
                == Some(true)
            {
                if let Some(card) = inner.board.get(&id).cloned() {
                    if card.kind == CardKind::Implementation {
                        let source = format!(
                            "kanban/{}",
                            crate::worktree::slugify(&card.id)
                        );
                        let base = crate::merge::default_branch(&self.project_root);
                        let merge_id = inner.board.next_free_merge_id(&source, &base);
                        inner.board.enqueue_merge(
                            merge_id,
                            source,
                            base,
                            Some(id.clone()),
                            chrono::Local::now(),
                        );
                    }
                }
            }
        }
```

（`CardKind` 已在 Task 8 引入；`crate::worktree::slugify` 对 `CardId` 取参，签名 `slugify(id: &CardId) -> String`。）

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban`
Expected: 全绿（`-p superpowers-kanban` 覆盖 core 与 runner 的测试？——若不含 core，分别跑 core 与 runner 两个包）。

实际命令：
`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core && cargo test -p superpowers-kanban-runner`

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add crates/superpowers-kanban-core/src/switch.rs crates/superpowers-kanban-runner/src/service.rs
git commit -m "feat(board): optionally derive a merge card when an implementation card awaits merge"
```

---

## Task 13: 宿主解析合并卡（`BoardCard`）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:1074-1135`（`BoardCard` 与其 `board_cards` 解析）
- Test: 同文件测试模块

**Interfaces:**
- Consumes: Task 10 的 `list` 输出。
- Produces: `BoardCard { kind: String, source: Option<String>, base: Option<String>, … }`（`kind` 缺省 `"implementation"`）。

- [ ] **Step 1: 写失败测试**

在该文件既有 `board_cards` 测试旁加：

```rust
    #[test]
    fn board_cards_parses_a_merge_card_and_defaults_the_kind() {
        // 直接喂一个假 `list` 结果给解析逻辑（与现有 board_cards 测试同款做法）。
        let cards = serde_json::json!({ "cards": [
            { "id": "m1", "state": "queued", "kind": "merge", "source": "kanban/a", "base": "main" },
            { "id": "impl", "state": "queued", "spec_path": "i.spec.md", "plan_path": "i.plan.md" }
        ]});
        let parsed = parse_board_cards(&cards);
        assert_eq!(parsed[0].kind, "merge");
        assert_eq!(parsed[0].source.as_deref(), Some("kanban/a"));
        assert_eq!(parsed[1].kind, "implementation");
        assert_eq!(parsed[1].source, None);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server board_cards`
Expected: 编译失败（`kind` 未定义 / `parse_board_cards` 未定义）。

- [ ] **Step 3: 实现**

`BoardCard` 结构体加：

```rust
    /// 卡片种类；旧/缺省卡片视为实现卡。
    pub kind: String,
    /// 合并卡的源分支。
    pub source: Option<String>,
    /// 合并卡的目标分支。
    pub base: Option<String>,
```

把 `board_cards` 里那段 `cards.into_iter().filter_map(...)` 抽成可测纯函数：

```rust
pub(crate) fn parse_board_cards(value: &serde_json::Value) -> Vec<BoardCard> {
    let cards = value
        .get("cards")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    cards
        .into_iter()
        .filter_map(|card| {
            Some(BoardCard {
                id: card.get("id")?.as_str()?.to_string(),
                state: card.get("state")?.as_str()?.to_string(),
                kind: card
                    .get("kind")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("implementation")
                    .to_string(),
                source: card.get("source").and_then(serde_json::Value::as_str).map(str::to_string),
                base: card.get("base").and_then(serde_json::Value::as_str).map(str::to_string),
                thread_id: card.get("thread_id").and_then(serde_json::Value::as_str).map(str::to_string),
                workdir: card.get("workdir").and_then(serde_json::Value::as_str).map(PathBuf::from),
                spec_path: card.get("spec_path").and_then(serde_json::Value::as_str).unwrap_or("").to_string(),
                plan_path: card.get("plan_path").and_then(serde_json::Value::as_str).unwrap_or("").to_string(),
            })
        })
        .collect()
}
```

`board_cards` 改为：

```rust
    let value = board_query(project, board_dir, "list", json!({}))?;
    Ok(parse_board_cards(&value))
```

（既有构造 `BoardCard { … }` 的地方——含测试——补 `kind`/`source`/`base` 字段。）

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: PASS（含既有 board 相关测试）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/server.rs
git commit -m "feat(board): carry the merge payload onto host board cards"
```

---

## Task 14: 桌面端让合并卡可见

**Files:**
- Modify: `desktop/src/lib/superpowersKanbanState.ts`
- Modify: `desktop/src/lib/superpowersKanbanState.test.ts`
- Modify: `desktop/src/components/SuperpowersKanbanView.tsx`（`BoardCard` 类型如需 `detail` 语义说明——`detail` 已是字符串，通常无需改）

**Interfaces:**
- Consumes: Task 10 的 `list` 字段（`kind`/`source`/`base`）。

- [ ] **Step 1: 写失败测试**

```ts
  it("keeps a merge card that has no plan_path and shows its refs", () => {
    const cards = parseBoard(
      JSON.stringify({
        cards: [
          { id: "m1", state: "queued", kind: "merge", source: "kanban/a", base: "main", order: 0 },
        ],
      }),
    );
    expect(cards).toHaveLength(1);
    expect(cards[0].id).toBe("m1");
    expect(cards[0].detail).toBe("kanban/a → main");
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npm test -- --run superpowersKanbanState`
Expected: FAIL（合并卡被丢弃，`cards` 为空）。

- [ ] **Step 3: 实现**

`parseBoard` 的映射里读取新字段并放宽过滤：

```ts
    const kind = typeof record.kind === "string" ? record.kind : "implementation";
    const source = typeof record.source === "string" ? record.source : "";
    const base = typeof record.base === "string" ? record.base : "";
    // 合并卡没有 plan_path，不能再按 plan_path 丢弃；只按 id / state 校验。
    if (id === "" || state === "") continue;
    const detail =
      workdir !== ""
        ? workdir
        : kind === "merge"
          ? `${source} → ${base}`
          : planPath;
```

（同时更新函数上方文档注释：跳过条件由「id/plan_path/state」改为「id/state」。）

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npm test -- --run superpowersKanbanState SuperpowersKanbanView`
Expected: PASS（含既有实现卡用例）。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/superpowersKanbanState.ts desktop/src/lib/superpowersKanbanState.test.ts
git commit -m "feat(board): render merge cards in the desktop board panel"
```

---

## Self-Review

**Spec coverage（逐节对照）：**

- §6.1 卡种与字段 → Task 1。
- §6.2 合并卡 id（+ 去重）→ Task 4（`merge_card_id_for`、`next_free_merge_id`）。
- §6.3 `Merging` 状态与迁移 → Task 2。
- §6.4 两种选取（会话跳过合并卡 / `claim_next_merge`）→ Task 3。
- §6.5 校验 → Task 4（纯）+ Task 7/9/10（分支存在性，runner）。
- §7.1 每项目合并锁 → Task 6。
- §7.2 基座 worktree → Task 7（`prepare`/`cleanup`；细化：base 已检出时复用该 worktree 并在其中合并，避免污染主检出——见「偏离说明」）。
- §7.3 合并与分类 → Task 7（`run`）。
- §7.4 服务层 `merge_next` + 驱动 origin → Task 8。
- §7.5 崩溃恢复 → Task 8（扩展 `migrate_legacy_running`）。
- §8.1 投递格式 → Task 5。
- §8.2 命令行 `add-merge` → Task 9。
- §8.3 对话入口 → Task 11。
- §8.4 桌面 `enqueue_merge` RPC → Task 10（接口）；**接 UI 按钮为后续**，与 spec「不阻塞第一刀」一致。
- §9 对接口与宿主接线 → Task 10（list/dispatch）、Task 12（自动派生位置）、Task 13（宿主 `BoardCard`）、Task 14（桌面 `parseBoard`）。
- §10 测试与验收 → 每任务内；端到端三条验收由 Task 8/9/10 的集成测试覆盖。

**偏离说明（须让 spec 与 plan 对齐）：** spec §7.2 原写「在专用 base worktree（`<project>/.worktrees/kanban-merge/<base>`）里执行」，但 git 实测表明**若 base 已被主检出占用，把它在别处合并后前移分支会让主检出的 index/worktree 变脏**（`git update-ref` 虽成功但出现虚假的已暂存改动）。因此实现改为：**base 已被某个 worktree（通常是主检出）检出时，就在那个 worktree 里合并**；只有 base 未被检出时才新建专用 worktree 并在用后移除。这与 CLAUDE.md「回 main 做 merge」的人工约定一致。建议把 spec §7.2 同步成这个措辞。

**Placeholder scan：** 无 TBD/TODO；每个代码步骤给了完整代码。Task 12 Step 4 的测试命令已修正为分包跑。

**Type consistency：** `CardKind`（Task 1）/`Merging`（Task 2）/`claim_next_merge`（Task 3）/`validate_merge_refs`、`merge_card_id_for`、`next_free_merge_id`（Task 4）/`deliver_merge_card`（Task 5）/`merge_lock::acquire`（Task 6）/`merge::{default_branch,source_branch_exists,prepare,is_dirty,run,cleanup,MergeOutcome}`（Task 7）/`BoardService::merge_next`、`MergeClaim`（Task 8）/`enqueue_merge`、`next_free_merge_id`（Task 10）/`read_bool`、`write_bool`（Task 12）/`parse_board_cards`（Task 13）——各任务引用处命名一致。

---

## Execution Handoff

计划已保存到 `docs/superpowers/plans/2026-10-03-board-merge-queue.md`。

两条执行路径：
1. **Subagent-Driven（推荐）** —— 每个任务派一个全新 subagent，任务间做两阶段评审，迭代快。
2. **Inline Execution** —— 在本会话用 executing-plans 批量执行，带检查点。

选哪条？
