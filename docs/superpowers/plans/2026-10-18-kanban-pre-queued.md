# 看板 pre-queued 列 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在看板里加一个常驻的 PRE-QUEUED 列，承载「已决定要做、spec/plan 还没写」的需求，写完设计后经 `promote` 显式提升为正式排队卡，全生命周期 id 不变。

**Architecture:** 新增 `CardState::PreQueued`（不占并发槽、不参与 claim）。预排队卡在创建时由 `slug(title)` 定死 id、`spec_path`/`plan_path` 留空。所有写入仍走 `inbox/` 投递：新增 `prequeued` 与 `promote` 两种投递形状，runner 的推进循环是 `board.json` 唯一写者，`consume` 改为两趟（先建卡类、后提升），保证同一 tick 内「先建后提」可用。四个消费面同步：CLI、IPC dispatch、TUI、桌面。

**Tech Stack:** Rust 2024（`plugins/superpowers-kanban` 独立 workspace + `yi-agent-rs` workspace）、serde/serde_json、chrono、React + TypeScript + vitest。

## Global Constraints

- 状态名落盘为 snake_case `"pre_queued"`；桌面列 key 为 `"preQueued"`；列标题为 `"PRE-QUEUED"`。三者不可混用。
- 预排队卡 id = `slug(title)`，撞名追加 `-2`/`-3`…；slug 为**空**（非 ASCII 标题，如中文）时回退 `prequeued-<YYYYMMDDHHMMSS>`，绝不塌成同一个 id；**`promote` 绝不重算 id**。
- 预排队卡 `spec_path`/`plan_path` 为空 `PathBuf`，`title: Some(..)`，`kind = Implementation`，不占并发槽。
- `promote` 成功后 `order` 重排到队尾（取 `next_order` 并递增），状态翻 `PreQueued → Queued`。
- `board.json` 的唯一写者是 runner 推进循环；CLI/IPC 一律只投递到 `inbox/`，绝不直接写 `board.json`。
- 新增 `Card` 字段一律带 `#[serde(default)]`，旧 `board.json` 必须仍能反序列化。
- 提交前在改动所在 workspace 跑 `cargo fmt --all`；commit message 用 conventional commits，不写 `Co-Authored-By`。

---

### Task 1: core 状态机与卡片字段

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/board.rs`

**Interfaces:**
- Consumes: 无（本任务是基座）。
- Produces:
  - `CardState::PreQueued`（serde `"pre_queued"`）
  - `Card.title: Option<String>`
  - `Board::enqueue_prequeued(&mut self, id: CardId, title: String, now: DateTime<Local>) -> &Card`
  - `Board::next_free_prequeued_id(&self, title: &str, now: DateTime<Local>) -> CardId`
  - `Board::promote_prequeued(&mut self, id: &CardId, spec: PathBuf, plan: PathBuf, title: Option<String>) -> Result<(), TransitionError>`

- [ ] **Step 1: 写失败测试（card.rs 的 `#[cfg(test)] mod tests` 末尾追加）**

```rust
    #[test]
    fn a_pre_queued_card_neither_occupies_a_slot_nor_is_terminal() {
        assert!(!CardState::PreQueued.occupies_slot());
        assert!(!CardState::PreQueued.is_terminal());
    }

    #[test]
    fn a_pre_queued_card_can_be_promoted_or_cancelled() {
        use CardState::*;
        assert!(PreQueued.can_transition_to(Queued));
        assert!(PreQueued.can_transition_to(Cancelled));
        // 不能越过排队直接跑，也不能直接进终态。
        assert!(!PreQueued.can_transition_to(Running));
        assert!(!PreQueued.can_transition_to(AwaitingMerge));
    }

    #[test]
    fn an_old_card_without_a_title_still_deserializes() {
        let json = r#"{"id":"a","spec_path":"a.spec.md","plan_path":"a.plan.md","state":"queued","enqueued_at":"2026-10-01T00:00:00+08:00","order":0}"#;
        let card: Card = serde_json::from_str(json).unwrap();
        assert_eq!(card.title, None, "旧 board.json 缺 title 必须回 None");
    }
```

- [ ] **Step 2: 写失败测试（board.rs 的 `mod tests` 末尾追加）**

```rust
    #[test]
    fn a_pre_queued_card_is_queued_nowhere_near_a_slot() {
        let mut board = Board::new();
        board.enqueue_prequeued(CardId::new("idea"), "一个想法".into(), at(1, 0));
        let card = board.get(&CardId::new("idea")).unwrap();
        assert_eq!(card.state, CardState::PreQueued);
        assert_eq!(card.title.as_deref(), Some("一个想法"));
        assert!(card.spec_path.as_os_str().is_empty());
        assert!(card.plan_path.as_os_str().is_empty());
        // 不占槽、不会被 claim 起来跑（它压根不是 Queued）。
        assert_eq!(board.running_count(), 0);
        assert_eq!(board.next_startable(), None);
        assert_eq!(board.claim_next_launch(3), None);
    }

    #[test]
    fn a_pre_queued_id_falls_back_and_gets_a_suffix_on_collision() {
        let mut board = Board::new();
        assert_eq!(board.next_free_prequeued_id("idea"), CardId::new("idea"));
        // `slug()` 只保留 ASCII 字母数字，中文标题会 slug 成空串——此时回退到
        // 带时间戳的 id，绝不能全部塌成同一个 `prequeued`（否则每张卡同 id，
        // 追踪就废了，且 worktree 分支名会互撞）。
        let fallback = board.next_free_prequeued_id("我的想法", at(1, 0));
        assert!(
            fallback.0.starts_with("prequeued-"),
            "非 ASCII slug 回退到带时间戳的 id: {fallback:?}"
        );
        let other = board.next_free_prequeued_id("另一个想法", at(1, 1));
        assert_ne!(fallback, other, "两张中文标题的卡必须拿到不同 id");
        board.enqueue_prequeued(CardId::new("idea"), "idea".into(), at(1, 0));
        assert_eq!(board.next_free_prequeued_id("idea"), CardId::new("idea-2"));
    }

    #[test]
    fn promoting_fills_the_paths_keeps_the_id_and_moves_to_the_back() {
        let mut board = board_with(&["older"]);
        board.enqueue_prequeued(CardId::new("idea"), "想法".into(), at(1, 5));
        let idea = CardId::new("idea");
        board
            .promote_prequeued(
                &idea,
                "docs/idea.spec.md".into(),
                "docs/idea.plan.md".into(),
                None,
            )
            .unwrap();
        let card = board.get(&idea).unwrap();
        assert_eq!(card.id, idea, "id 绝不重算");
        assert_eq!(card.state, CardState::Queued);
        assert_eq!(card.spec_path, PathBuf::from("docs/idea.spec.md"));
        assert_eq!(card.plan_path, PathBuf::from("docs/idea.plan.md"));
        assert_eq!(card.title.as_deref(), Some("想法"), "未给新标题则保留");
        // 重排到队尾：`older`(order 0) 之后。
        assert_eq!(board.next_startable(), Some(CardId::new("older")));
        assert!(card.order > board.get(&CardId::new("older")).unwrap().order);
    }

    #[test]
    fn promoting_with_a_new_title_overwrites_it_but_not_the_id() {
        let mut board = Board::new();
        let idea = CardId::new("idea");
        board.enqueue_prequeued(idea.clone(), "旧标题".into(), at(1, 0));
        board
            .promote_prequeued(&idea, "s".into(), "p".into(), Some("新标题".into()))
            .unwrap();
        let card = board.get(&idea).unwrap();
        assert_eq!(card.title.as_deref(), Some("新标题"));
        assert_eq!(card.id, idea);
    }

    #[test]
    fn promoting_a_card_that_is_not_pre_queued_is_rejected() {
        let mut board = board_with(&["a"]); // a 已是 Queued
        let error = board
            .promote_prequeued(&CardId::new("a"), "s".into(), "p".into(), None)
            .unwrap_err();
        assert_eq!(
            error,
            TransitionError::Illegal {
                from: CardState::Queued,
                to: CardState::Queued,
            }
        );
    }
```

- [ ] **Step 3: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core`
Expected: 编译错误 `no variant named PreQueued` / `no field title` / `no method enqueue_prequeued`。

- [ ] **Step 4: 实现 card.rs**

在 `CardState` 枚举里、`Queued` 之前加一个变体：

```rust
pub enum CardState {
    /// 已决定要做，但 spec/plan 还没写出来；不占槽、不进会话。
    PreQueued,
    Queued,
    // ... 其余不变
```

在 `Card` 结构体末尾（`terminal_at` 之后）加字段：

```rust
    /// 展示标题。预排队卡在创建时定下；正式卡为 `None`（标题由 spec_path 派生）。
    #[serde(default)]
    pub title: Option<String>,
```

在 `can_transition_to` 的 `match (self, next)` 里加一条边（`Cancelled` 已由函数开头的 `next == Cancelled` 兜底，不必再加）：

```rust
            (PreQueued, Queued) => true,
```

`occupies_slot` / `is_terminal` **不改**（`PreQueued` 天然不在其中）。

- [ ] **Step 5: 实现 board.rs**

`enqueue` 与 `enqueue_merge` 两处 `Card { .. }` 字面量各补一行 `title: None,`。然后新增三个方法（放在 `enqueue_merge` 之后）：

```rust
    /// 排入一张预排队卡：无 spec/plan，只有一个标题；id 由调用方定死。
    pub fn enqueue_prequeued(
        &mut self,
        id: CardId,
        title: String,
        now: DateTime<Local>,
    ) -> &Card {
        let order = self.next_order;
        self.next_order += 1;
        let index = self.cards.len();
        self.cards.push(Card {
            id,
            spec_path: PathBuf::new(),
            plan_path: PathBuf::new(),
            state: CardState::PreQueued,
            enqueued_at: now,
            order,
            workdir: None,
            task_id: None,
            thread_id: None,
            base_commit: None,
            kind: CardKind::Implementation,
            source_ref: None,
            base_ref: None,
            origin_card: None,
            archived: false,
            terminal_at: None,
            title: Some(title),
        });
        &self.cards[index]
    }

    /// 未占用的预排队卡 id。
    ///
    /// `slug(title)` 只保留 ASCII 字母数字，中文标题会 slug 成空串——此时用
    /// `prequeued-<YYYYMMDDHHMMSS>` 回退。**绝不**把所有非 ASCII 标题塌成同一个
    /// `prequeued`：那会让每张中文卡的 id 撞成一个（`slugify` 再把它压成 `card`），
    /// worktree 分支名与追踪线一起废掉。
    pub fn next_free_prequeued_id(&self, title: &str, now: DateTime<Local>) -> CardId {
        let slug = crate::card_id::slug(title);
        let base = if slug.is_empty() {
            format!("prequeued-{}", now.format("%Y%m%d%H%M%S"))
        } else {
            slug
        };
        if !self.contains(&base) {
            return CardId::new(base);
        }
        for n in 2u32.. {
            let candidate = format!("{base}-{n}");
            if !self.contains(&candidate) {
                return CardId::new(candidate);
            }
        }
        unreachable!("u32 suffix space exhausted")
    }

    /// 提升一张预排队卡：补 spec/plan、可换标题、翻 `Queued`、**重排到队尾**。
    /// id 不变——这是「追踪不断线」的核心保障。
    pub fn promote_prequeued(
        &mut self,
        id: &CardId,
        spec: PathBuf,
        plan: PathBuf,
        title: Option<String>,
    ) -> Result<(), TransitionError> {
        let order = self.next_order;
        let card = self
            .cards
            .iter_mut()
            .find(|card| &card.id == id)
            .ok_or_else(|| TransitionError::UnknownCard(id.clone()))?;
        if !card.state.can_transition_to(CardState::Queued) {
            return Err(TransitionError::Illegal {
                from: card.state,
                to: CardState::Queued,
            });
        }
        card.spec_path = spec;
        card.plan_path = plan;
        if let Some(title) = title {
            card.title = Some(title);
        }
        card.order = order;
        card.state = CardState::Queued;
        self.next_order += 1;
        Ok(())
    }
```

- [ ] **Step 6: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core && cargo fmt --all`
Expected: 全绿；此前所有 core 测试仍通过。

- [ ] **Step 7: Commit**

```bash
git add plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs plugins/superpowers-kanban/crates/superpowers-kanban-core/src/board.rs
git commit -m "feat(kanban): add PreQueued state, card title, and promote on the board model"
```

---

### Task 2: core 投递形状

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/inbox.rs`

**Interfaces:**
- Consumes: 无。
- Produces:
  - `inbox::promote_path(state_dir: &Path, id: &str) -> PathBuf`（`<state_dir>/inbox/<id>.promote.json`）
  - `inbox::deliver_prequeued_card(state_dir: &Path, id: &str, title: &str) -> std::io::Result<()>`
  - `inbox::deliver_promote_card(state_dir: &Path, id: &str, spec: &str, plan: &str, title: Option<&str>) -> std::io::Result<()>`

- [ ] **Step 1: 写失败测试（inbox.rs 的 `mod tests` 末尾追加）**

```rust
    #[test]
    fn delivers_a_prequeued_card_with_its_kind_and_title() {
        let dir = tempfile::tempdir().unwrap();
        deliver_prequeued_card(dir.path(), "idea", "一个想法").unwrap();
        let text = std::fs::read_to_string(dir.path().join("inbox/idea.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["kind"], "prequeued");
        assert_eq!(value["id"], "idea");
        assert_eq!(value["title"], "一个想法");
    }

    #[test]
    fn delivers_a_promote_with_its_own_file_name() {
        let dir = tempfile::tempdir().unwrap();
        deliver_promote_card(dir.path(), "idea", "a.spec.md", "a.plan.md", Some("新标题")).unwrap();
        let text = std::fs::read_to_string(dir.path().join("inbox/idea.promote.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["kind"], "promote");
        assert_eq!(value["id"], "idea");
        assert_eq!(value["spec_path"], "a.spec.md");
        assert_eq!(value["plan_path"], "a.plan.md");
        assert_eq!(value["title"], "新标题");
    }

    #[test]
    fn a_promote_without_a_title_omits_the_key() {
        let dir = tempfile::tempdir().unwrap();
        deliver_promote_card(dir.path(), "idea", "s", "p", None).unwrap();
        let text = std::fs::read_to_string(dir.path().join("inbox/idea.promote.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(value.get("title").is_none());
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core inbox`
Expected: FAIL，`cannot find function deliver_prequeued_card`。

- [ ] **Step 3: 实现**

在 `deliver_merge_card` 之后追加（沿用同一套 `temp + rename`）：

```rust
/// 提升投递的文件路径：`<state_dir>/inbox/<id>.promote.json`。
///
/// 与建卡投递分文件：同一张卡可能在同一 tick 内先建后提，两件事必须能并存。
pub fn promote_path(state_dir: &Path, id: &str) -> PathBuf {
    inbox_dir(state_dir).join(format!("{id}.promote.json"))
}

/// 投递一张预排队卡：`{"kind":"prequeued","id","title"}`。
pub fn deliver_prequeued_card(state_dir: &Path, id: &str, title: &str) -> std::io::Result<()> {
    let dir = inbox_dir(state_dir);
    std::fs::create_dir_all(&dir)?;
    let path = enqueue_path(state_dir, id);
    let body = serde_json::json!({ "kind": "prequeued", "id": id, "title": title });
    let text = serde_json::to_string_pretty(&body).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

/// 投递一次提升：`{"kind":"promote","id","spec_path","plan_path"[,"title"]}`。
pub fn deliver_promote_card(
    state_dir: &Path,
    id: &str,
    spec: &str,
    plan: &str,
    title: Option<&str>,
) -> std::io::Result<()> {
    let dir = inbox_dir(state_dir);
    std::fs::create_dir_all(&dir)?;
    let path = promote_path(state_dir, id);
    let mut body = serde_json::json!({
        "kind": "promote",
        "id": id,
        "spec_path": spec,
        "plan_path": plan,
    });
    if let Some(title) = title {
        body["title"] = serde_json::Value::String(title.to_string());
    }
    let text = serde_json::to_string_pretty(&body).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core && cargo fmt --all`
Expected: 全绿。

- [ ] **Step 5: Commit**

```bash
git add plugins/superpowers-kanban/crates/superpowers-kanban-core/src/inbox.rs
git commit -m "feat(kanban): add prequeued and promote inbox delivery shapes"
```

---

### Task 3: runner 两趟消费

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/inbox.rs`

**Interfaces:**
- Consumes: Task 1 的 `Board::enqueue_prequeued` / `Board::promote_prequeued`；Task 2 的投递形状。
- Produces: `consume(state_dir, board, now) -> Vec<InboxOutcome>` 语义不变，但新增处理 `kind == "prequeued"` 与 `kind == "promote"`，且保证同一 tick 内先建卡后提升。

- [ ] **Step 1: 写失败测试（`mod tests` 末尾追加）**

```rust
    #[test]
    fn a_prequeued_delivery_is_queued_without_a_spec_or_plan() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(
            inbox.join("idea.json"),
            r#"{"kind":"prequeued","id":"idea","title":"my idea"}"#,
        )
        .unwrap();
        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, at());
        assert!(outcomes[0].result.is_ok(), "{:?}", outcomes[0]);
        let card = board.get(&CardId::new("idea")).unwrap();
        assert_eq!(card.state, superpowers_kanban_core::card::CardState::PreQueued);
        assert_eq!(card.title.as_deref(), Some("my idea"));
    }

    #[test]
    fn a_promote_in_the_same_tick_as_its_prequeue_takes_effect() {
        // 两趟消费的意义：文件按名字排序处理，`idea.json` 在 `idea.promote.json`
        // 之前——但即便顺序反过来，建卡也必须先于提升生效。
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        let spec = dir.path().join("idea.spec.md");
        let plan = dir.path().join("idea.plan.md");
        std::fs::write(&spec, "# spec").unwrap();
        std::fs::write(&plan, "# plan").unwrap();
        std::fs::write(
            inbox.join("idea.json"),
            r#"{"kind":"prequeued","id":"idea","title":"my idea"}"#,
        )
        .unwrap();
        std::fs::write(
            inbox.join("idea.promote.json"),
            serde_json::json!({
                "kind": "promote",
                "id": "idea",
                "spec_path": spec.to_str().unwrap(),
                "plan_path": plan.to_str().unwrap(),
            })
            .to_string(),
        )
        .unwrap();

        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, at());
        assert!(outcomes.iter().all(|o| o.result.is_ok()), "{outcomes:?}");
        let card = board.get(&CardId::new("idea")).unwrap();
        assert_eq!(card.state, superpowers_kanban_core::card::CardState::Queued);
        assert_eq!(card.spec_path, spec);
        assert!(!inbox.join("idea.promote.json").exists(), "投递被消费");
    }

    #[test]
    fn a_promote_for_an_unknown_card_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(
            inbox.join("ghost.promote.json"),
            r#"{"kind":"promote","id":"ghost","spec_path":"s","plan_path":"p"}"#,
        )
        .unwrap();
        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, at());
        assert!(outcomes[0].result.is_err());
        assert!(inbox.join("rejected/ghost.promote.json").exists());
    }

    #[test]
    fn a_promote_for_a_card_that_is_not_pre_queued_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        let spec = dir.path().join("a.spec.md");
        let plan = dir.path().join("a.plan.md");
        std::fs::write(&spec, "# spec").unwrap();
        std::fs::write(&plan, "# plan").unwrap();
        // 先排一张正式卡（state=Queued）。
        std::fs::write(
            inbox.join("a.json"),
            serde_json::json!({
                "id": "a",
                "spec_path": spec.to_str().unwrap(),
                "plan_path": plan.to_str().unwrap(),
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            inbox.join("a.promote.json"),
            r#"{"kind":"promote","id":"a","spec_path":"s","plan_path":"p"}"#,
        )
        .unwrap();
        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, at());
        assert_eq!(board.get(&CardId::new("a")).unwrap().state, superpowers_kanban_core::card::CardState::Queued);
        assert!(
            outcomes.iter().any(|o| o.result.is_err()),
            "非 pre_queued 的提升必须被拒: {outcomes:?}"
        );
        assert!(inbox.join("rejected/a.promote.json").exists());
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner inbox`
Expected: FAIL——`prequeued` 投递被当作 implementation 拒绝（缺 spec/plan）；`promote` 同理。

- [ ] **Step 3: 实现——`RawDelivery` 加 `title`**

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
    /// 预排队卡的标题 / 提升时的新标题。
    #[serde(default)]
    title: Option<String>,
}
```

- [ ] **Step 4: 实现——把单趟循环拆成「解析 + 两趟应用」**

把现有 `consume` 整体替换为下面结构（`apply_delivery` 容纳原来的逐文件逻辑；`reject` 不动）：

```rust
/// 消费 `<state_dir>/inbox` 下全部投递。
///
/// **两趟**：先处理建卡类（implementation / merge / prequeued），再处理 promote。
/// 同一张卡可能在同一 tick 内先 `add-prequeued` 后 `promote`，文件到达顺序不可
/// 依赖，所以必须显式保证「先建后提」。
pub fn consume(state_dir: &Path, board: &mut Board, now: DateTime<Local>) -> Vec<InboxOutcome> {
    let inbox = state_dir.join("inbox");
    let Ok(entries) = std::fs::read_dir(&inbox) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect();
    paths.sort();

    // 先把每个文件读成「路径 + id + 解析结果」，避免两趟各读一次盘。
    let mut parsed: Vec<(PathBuf, String, Result<RawDelivery, String>)> = Vec::new();
    for path in paths {
        let id_from_file = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_default();
        let parsed_delivery = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<RawDelivery>(&text)
                .map_err(|error| format!("invalid delivery json: {error}")),
            Err(error) => Err(format!("could not read delivery: {error}")),
        };
        parsed.push((path, id_from_file, parsed_delivery));
    }

    let is_promote = |delivery: &Result<RawDelivery, String>| {
        matches!(delivery, Ok(delivery) if delivery.kind.as_deref() == Some("promote"))
    };

    let mut outcomes = Vec::new();
    // 第一趟：非 promote（含解析失败——那些只能按退回收档处理）。
    for (path, id_from_file, delivery) in &parsed {
        if is_promote(delivery) {
            continue;
        }
        outcomes.push(apply_delivery(&inbox, path, id_from_file, delivery, board, now));
    }
    // 第二趟：promote——此时同 tick 的建卡已经生效。
    for (path, id_from_file, delivery) in &parsed {
        if !is_promote(delivery) {
            continue;
        }
        outcomes.push(apply_delivery(&inbox, path, id_from_file, delivery, board, now));
    }
    outcomes
}

/// 处理一条已读入的投递，返回结果并落盘（消费或拒绝）。
fn apply_delivery(
    inbox: &Path,
    path: &Path,
    id_from_file: &str,
    delivery: &Result<RawDelivery, String>,
    board: &mut Board,
    now: DateTime<Local>,
) -> InboxOutcome {
    let delivery = match delivery {
        Ok(delivery) => delivery,
        Err(reason) => {
            let id = id_from_file
                .trim_end_matches(".promote")
                .to_string();
            reject(inbox, path, &id);
            return InboxOutcome {
                id,
                result: Err(reason.clone()),
            };
        }
    };
    let id = CardId::new(delivery.id.clone());
    if board.get(&id).is_some() && delivery.kind.as_deref() != Some("promote") {
        // 幂等：同 id 已在队列里，直接消费投递即可（promote 例外，见下）。
        let _ = std::fs::remove_file(path);
        return InboxOutcome { id: delivery.id, result: Ok(()) };
    }
    if delivery.kind.as_deref() == Some("promote") {
        return apply_promote(inbox, path, delivery, board);
    }
    if delivery.kind.as_deref() == Some("prequeued") {
        let title = delivery.title.clone().unwrap_or_default();
        if title.trim().is_empty() {
            reject(inbox, path, &delivery.id);
            return InboxOutcome {
                id: delivery.id,
                result: Err("prequeued delivery needs a non-empty title".to_string()),
            };
        }
        board.enqueue_prequeued(id, title, now);
        let _ = std::fs::remove_file(path);
        return InboxOutcome { id: delivery.id, result: Ok(()) };
    }
    if delivery.kind.as_deref() == Some("merge") {
        let (source, base) = (
            delivery.source.clone().unwrap_or_default(),
            delivery.base.clone().unwrap_or_default(),
        );
        if let Err(error) = superpowers_kanban_core::promotion::validate_merge_refs(&source, &base)
        {
            reject(inbox, path, &delivery.id);
            return InboxOutcome { id: delivery.id, result: Err(error.to_string()) };
        }
        let origin = delivery.origin_card.as_deref().map(CardId::new);
        board.enqueue_merge(id, source, base, origin, now);
        let _ = std::fs::remove_file(path);
        return InboxOutcome { id: delivery.id, result: Ok(()) };
    }
    let spec = delivery.spec_path.clone().unwrap_or_default();
    let plan = delivery.plan_path.clone().unwrap_or_default();
    if let Err(error) = validate_promotion(Path::new(&spec), Path::new(&plan)) {
        reject(inbox, path, &delivery.id);
        return InboxOutcome { id: delivery.id, result: Err(error.to_string()) };
    }
    board.enqueue(id, spec.into(), plan.into(), now);
    let _ = std::fs::remove_file(path);
    InboxOutcome { id: delivery.id, result: Ok(()) }
}

/// 提升：卡必须存在且正处 `PreQueued`，spec/plan 必须成对存在。
fn apply_promote(
    inbox: &Path,
    path: &Path,
    delivery: &RawDelivery,
    board: &mut Board,
) -> InboxOutcome {
    let id = CardId::new(delivery.id.clone());
    let reject_with = |inbox: &Path, path: &Path, delivery: &RawDelivery, reason: String| {
        reject(inbox, path, &delivery.id);
        InboxOutcome { id: delivery.id.clone(), result: Err(reason) }
    };
    let Some(card) = board.get(&id) else {
        return reject_with(inbox, path, delivery, format!("unknown card: {}", delivery.id));
    };
    if card.state != superpowers_kanban_core::card::CardState::PreQueued {
        let state = card.state;
        return reject_with(
            inbox,
            path,
            delivery,
            format!("card {} is not pre-queued (state: {state:?})", delivery.id),
        );
    }
    let spec = delivery.spec_path.clone().unwrap_or_default();
    let plan = delivery.plan_path.clone().unwrap_or_default();
    if let Err(error) = validate_promotion(Path::new(&spec), Path::new(&plan)) {
        return reject_with(inbox, path, delivery, error.to_string());
    }
    if let Err(error) = board.promote_prequeued(
        &id,
        spec.into(),
        plan.into(),
        delivery.title.clone(),
    ) {
        return reject_with(inbox, path, delivery, error.to_string());
    }
    let _ = std::fs::remove_file(path);
    InboxOutcome { id: delivery.id, result: Ok(()) }
}
```

顶部 `use` 需补 `std::path::PathBuf`（`std::path::Path` 已在）。

- [ ] **Step 5: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner && cargo fmt --all`
Expected: 全绿——**包括既有 inbox 测试**（幂等、损坏投递、merge 投递等语义必须逐字保留）。

- [ ] **Step 6: Commit**

```bash
git add plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/inbox.rs
git commit -m "feat(kanban): consume prequeued and promote deliveries in two passes"
```

---

### Task 4: runner dispatch 与 list

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/dispatch.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`

**Interfaces:**
- Consumes: Task 1 的 `Board::next_free_prequeued_id`；Task 2 的 `deliver_prequeued_card` / `deliver_promote_card`。
- Produces:
  - IPC `enqueue_prequeued`（params `{"title": String}` → `{"id": String}`）
  - IPC `promote`（params `{"id","spec_path","plan_path"[,"title"]}` → `{"id": String}`）
  - `BoardService::next_free_prequeued_id(&self, title: &str, now: DateTime<Local>) -> String`
  - `BoardService::list()` 的每张卡多一个 `"title"` 字段

- [ ] **Step 1: 写失败测试（dispatch.rs 的 `mod tests` 末尾追加）**

```rust
    #[test]
    fn enqueue_prequeued_delivers_a_titled_card() {
        let dir = tempfile::tempdir().unwrap();
        let value = dispatch_with_global(
            dir.path(),
            None,
            "enqueue_prequeued",
            &json!({ "title": "my idea" }),
        )
        .unwrap();
        assert_eq!(value["id"], "my-idea");
        let text = std::fs::read_to_string(dir.path().join("inbox/my-idea.json")).unwrap();
        let delivery: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(delivery["kind"], "prequeued");
        assert_eq!(delivery["title"], "my idea");
    }

    #[test]
    fn enqueue_prequeued_gives_non_ascii_titles_distinct_ids() {
        // 中文标题 slug 成空串：必须回退到时间戳 id，且两张卡不同 id。
        let dir = tempfile::tempdir().unwrap();
        let first = dispatch_with_global(
            dir.path(),
            None,
            "enqueue_prequeued",
            &json!({ "title": "一个想法" }),
        )
        .unwrap();
        assert!(
            first["id"].as_str().unwrap().starts_with("prequeued-"),
            "{first}"
        );
        let delivery: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                dir.path()
                    .join("inbox")
                    .join(format!("{}.json", first["id"].as_str().unwrap())),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(delivery["title"], "一个想法", "id 是 slug 回退，标题仍是原文");
    }

    #[test]
    fn enqueue_prequeued_rejects_an_empty_title() {
        let dir = tempfile::tempdir().unwrap();
        let error = dispatch_with_global(
            dir.path(),
            None,
            "enqueue_prequeued",
            &json!({ "title": "   " }),
        )
        .unwrap_err();
        assert!(error.contains("title"), "{error}");
        assert!(!dir.path().join("inbox").exists(), "拒绝的投递不落盘");
    }

    #[test]
    fn promote_delivers_without_touching_the_board() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("a.spec.md");
        let plan = dir.path().join("a.plan.md");
        std::fs::write(&spec, "# spec").unwrap();
        std::fs::write(&plan, "# plan").unwrap();
        let value = dispatch_with_global(
            dir.path(),
            None,
            "promote",
            &json!({
                "id": "idea",
                "spec_path": spec.to_str().unwrap(),
                "plan_path": plan.to_str().unwrap(),
                "title": "新标题",
            }),
        )
        .unwrap();
        assert_eq!(value["id"], "idea");
        let text = std::fs::read_to_string(dir.path().join("inbox/idea.promote.json")).unwrap();
        let delivery: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(delivery["kind"], "promote");
        assert_eq!(delivery["title"], "新标题");
    }

    #[test]
    fn promote_rejects_a_missing_pair_before_any_disk_write() {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("a.plan.md");
        std::fs::write(&plan, "# plan").unwrap();
        let error = dispatch_with_global(
            dir.path(),
            None,
            "promote",
            &json!({
                "id": "idea",
                "spec_path": dir.path().join("missing.spec.md").to_str().unwrap(),
                "plan_path": plan.to_str().unwrap(),
            }),
        )
        .unwrap_err();
        assert!(error.contains("spec file does not exist"), "{error}");
        assert!(!dir.path().join("inbox").exists(), "拒绝的投递不落盘");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner dispatch`
Expected: FAIL，`unknown method: enqueue_prequeued`。

- [ ] **Step 3: 实现 dispatch.rs**

顶部 `use` 补：

```rust
use superpowers_kanban_core::inbox::{deliver_card, deliver_merge_card, deliver_prequeued_card, deliver_promote_card};
```

在 `match method` 里、`"enqueue"` 分支之后插入两个分支：

```rust
        "enqueue_prequeued" => {
            let title = params
                .get("title")
                .and_then(Value::as_str)
                .ok_or_else(|| "enqueue_prequeued needs a `title`".to_string())?;
            if title.trim().is_empty() {
                return Err("enqueue_prequeued needs a non-empty `title`".to_string());
            }
            let id = service.next_free_prequeued_id(title, chrono::Local::now());
            deliver_prequeued_card(state_dir, &id, title)
                .map_err(|error| format!("could not deliver the card: {error}"))?;
            Ok(json!({ "id": id }))
        }
        "promote" => {
            let card_id = params
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "promote needs an `id`".to_string())?;
            let spec = params
                .get("spec_path")
                .and_then(Value::as_str)
                .ok_or_else(|| "promote needs a `spec_path`".to_string())?;
            let plan = params
                .get("plan_path")
                .and_then(Value::as_str)
                .ok_or_else(|| "promote needs a `plan_path`".to_string())?;
            // 校验先做：拒绝的投递必须一个字节都不落盘。
            validate_promotion(Path::new(spec), Path::new(plan))
                .map_err(|error| error.to_string())?;
            let title = params.get("title").and_then(Value::as_str);
            deliver_promote_card(state_dir, card_id, spec, plan, title)
                .map_err(|error| format!("could not deliver the promote: {error}"))?;
            Ok(json!({ "id": card_id }))
        }
```

- [ ] **Step 4: 实现 service.rs**

在 `next_free_merge_id` 旁边加：

```rust
    /// 看板上未被占用的预排队卡 id（供 dispatch / CLI 共用）。
    pub fn next_free_prequeued_id(&self, title: &str, now: DateTime<Local>) -> String {
        persist::read_board(&self.state_dir)
            .next_free_prequeued_id(title, now)
            .0
    }
```

在 `list()` 的每卡 `json!` 里加一行标题字段（与既有 `"kind"` 等同级）：

```rust
                "title": card.title,
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner && cargo fmt --all`
Expected: 全绿。

- [ ] **Step 6: Commit**

```bash
git add plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/dispatch.rs plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs
git commit -m "feat(kanban): expose enqueue_prequeued/promote over IPC and list card titles"
```

---

### Task 5: CLI 动词

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`

**Interfaces:**
- Consumes: Task 2 的投递函数；Task 1 的 `Board::next_free_prequeued_id` / `CardState::PreQueued`。
- Produces: CLI 子命令 `add-prequeued <title>`、`promote <id> <spec> <plan> [--title <t>]`。

- [ ] **Step 1: 写失败测试（main.rs 的 `mod tests` 末尾追加解析测试）**

```rust
    #[test]
    fn parses_add_prequeued_and_promote() {
        assert!(matches!(
            parse(&["add-prequeued", "一个想法"]),
            Ok(Subcommand::AddPrequeued { title, .. }) if title == "一个想法"
        ));
        assert!(matches!(
            parse(&["promote", "idea", "a.spec.md", "a.plan.md"]),
            Ok(Subcommand::Promote { card_id, spec, plan, title: None, .. })
                if card_id == "idea" && spec == "a.spec.md" && plan == "a.plan.md"
        ));
        assert!(matches!(
            parse(&["promote", "idea", "a.spec.md", "a.plan.md", "--title", "新标题"]),
            Ok(Subcommand::Promote { title: Some(t), .. }) if t == "新标题"
        ));
    }

    #[test]
    fn add_prequeued_and_promote_reject_missing_arguments() {
        assert!(parse(&["add-prequeued"]).is_err());
        assert!(parse(&["promote", "idea"]).is_err());
        assert!(parse(&["promote", "idea", "a.spec.md"]).is_err());
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner main`
Expected: 编译错误 `no variant AddPrequeued`。

- [ ] **Step 3: 实现——子命令类型与解析**

在 `enum Subcommand` 里、`Add` 之后加：

```rust
    AddPrequeued {
        state_dir: PathBuf,
        title: String,
    },
    Promote {
        state_dir: PathBuf,
        card_id: String,
        spec: String,
        plan: String,
        title: Option<String>,
    },
```

在 `parse_subcommand` 的 `Some("add")` 臂之后加两个臂：

```rust
        Some("add-prequeued") => {
            let (state_dir, rest) = parse_state_dir(args)?;
            let mut rest = rest.into_iter();
            let title = rest
                .next()
                .ok_or_else(|| format!("add-prequeued needs a title\n{USAGE}"))?;
            if let Some(extra) = rest.next() {
                return Err(format!(
                    "add-prequeued takes one title, got an extra: {extra}"
                ));
            }
            Ok(Subcommand::AddPrequeued { state_dir, title })
        }
        Some("promote") => {
            let (state_dir, rest) = parse_state_dir(args)?;
            let mut rest = rest.into_iter();
            let card_id = rest
                .next()
                .ok_or_else(|| format!("promote needs a card id\n{USAGE}"))?;
            let spec = rest
                .next()
                .ok_or_else(|| format!("promote needs a spec path\n{USAGE}"))?;
            let plan = rest
                .next()
                .ok_or_else(|| format!("promote needs a plan path\n{USAGE}"))?;
            let mut title = None;
            while let Some(token) = rest.next() {
                if token == "--title" {
                    title = Some(
                        rest.next()
                            .ok_or_else(|| "--title needs a value".to_string())?,
                    );
                } else {
                    return Err(format!("unexpected argument: {token}"));
                }
            }
            Ok(Subcommand::Promote {
                state_dir,
                card_id,
                spec,
                plan,
                title,
            })
        }
```

- [ ] **Step 4: 实现——USAGE 与 main 分派**

`USAGE` 常量改为：

```
const USAGE: &str =
    "usage: superpowers-kanban <run|add|add-prequeued|promote|add-merge|merge-request|merge-finish|done|archive|purge|list|on|off|workdir> [...]
  run            --runtime-dir <d> --state-dir <d> [--project-root <d>] [--interval-secs N]
  add            <spec> <plan> [--state-dir <d>]
  add-prequeued  <title> [--state-dir <d>]
  promote        <card-id> <spec> <plan> [--title <t>] [--state-dir <d>]
  add-merge      <source> [--base <ref>] [--state-dir <d>]
  merge-request  <card-id> [--state-dir <d>]
  merge-finish   <card-id> [--state-dir <d>]
  done           <card-id> [--state-dir <d>]
  archive        <card-id>|--all-terminal [--state-dir <d>]
  purge          <card-id>|--all-archived [--state-dir <d>]
  list           [--all] [--state-dir <d>]
  on|off         [--state-dir <d>]
  workdir        [--state-dir <d>]";
```

在 `main` 的分派里、`Subcommand::Add` 臂之后加（该 `match` 的每个臂都直接返回 `Result<String, String>`，由后面的 `match outcome` 统一打印/定退出码）：

```rust
        Subcommand::AddPrequeued { state_dir, title } => command_prequeued(&state_dir, &title),
        Subcommand::Promote {
            state_dir,
            card_id,
            spec,
            plan,
            title,
        } => command_promote(&state_dir, &card_id, &spec, &plan, title.as_deref()),
```

- [ ] **Step 5: 实现——两个 command 函数（放在 `command_add` 之后）**

```rust
/// `add-prequeued`：只给标题，投递一张预排队卡，id 在这里定死。
fn command_prequeued(state_dir: &std::path::Path, title: &str) -> Result<String, String> {
    if title.trim().is_empty() {
        return Err("add-prequeued needs a non-empty title".to_string());
    }
    let board = superpowers_kanban_runner::persist::load_board(&state_dir.join("board.json"));
    let id = board.next_free_prequeued_id(title, chrono::Local::now()).0;
    superpowers_kanban_core::inbox::deliver_prequeued_card(state_dir, &id, title)
        .map_err(|error| format!("could not deliver the card: {error}"))?;
    Ok(format!(
        "delivered {id} to {}\n{title}",
        superpowers_kanban_core::inbox::inbox_dir(state_dir).display()
    ))
}

/// `promote`：校验成对 + 卡确为 pre-queued（早报错），再投递提升。
fn command_promote(
    state_dir: &std::path::Path,
    card_id: &str,
    spec: &str,
    plan: &str,
    title: Option<&str>,
) -> Result<String, String> {
    validate_promotion(std::path::Path::new(spec), std::path::Path::new(plan))
        .map_err(|error| error.to_string())?;
    let board = superpowers_kanban_runner::persist::load_board(&state_dir.join("board.json"));
    let id = superpowers_kanban_core::card::CardId::new(card_id);
    match board.get(&id) {
        None => return Err(format!("unknown card: {card_id}")),
        Some(card) if card.state != superpowers_kanban_core::card::CardState::PreQueued => {
            return Err(format!(
                "card {card_id} is not pre-queued (state: {:?})",
                card.state
            ));
        }
        Some(_) => {}
    }
    superpowers_kanban_core::inbox::deliver_promote_card(state_dir, card_id, spec, plan, title)
        .map_err(|error| format!("could not deliver the promote: {error}"))?;
    Ok(format!(
        "delivered promote for {card_id} to {}\n{spec}\n{plan}",
        superpowers_kanban_core::inbox::inbox_dir(state_dir).display()
    ))
}
```

- [ ] **Step 6: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner && cargo fmt --all`
Expected: 全绿。

- [ ] **Step 7: Commit**

```bash
git add plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs
git commit -m "feat(kanban): add add-prequeued and promote CLI verbs"
```

---

### Task 6: TUI 命令与渲染

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/superpowers_kanban.rs`

**Interfaces:**
- Consumes: Task 4 的 IPC 方法 `enqueue_prequeued` / `promote`；Task 4 `list()` 新增的 `title` 字段。
- Produces: `/superpowers-kanban add-prequeued <title>`、`/superpowers-kanban promote <id> <spec> <plan> [<title>]`；`render_board` 对无 `plan_path` 的卡片回退显示 `title`。

- [ ] **Step 1: 写失败测试**

先扩 `installed` 桩（`mod tests` 里）以支持新方法：

```rust
            "enqueue_prequeued" => Ok(json!({ "id": "my-idea" })),
            "promote" => Ok(json!({ "id": params["id"] })),
```

再追加测试：

```rust
    #[test]
    fn add_prequeued_delivers_through_the_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global");
        let log = fake_daemon(dir.path(), installed);
        let outcome = handle_kanban_with(dir.path(), "add-prequeued my idea", &global);
        assert!(outcome.lines[0].contains("my-idea"), "{:?}", outcome.lines);
        let calls = log.lock().unwrap().clone();
        assert!(
            calls.iter().any(|(method, params)| method == "enqueue_prequeued"
                && params["title"] == "my idea"),
            "{calls:?}"
        );
    }

    #[test]
    fn promote_delivers_through_the_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global");
        let log = fake_daemon(dir.path(), installed);
        let outcome = handle_kanban_with(
            dir.path(),
            "promote idea a.spec.md a.plan.md",
            &global,
        );
        assert!(outcome.lines[0].contains("idea"), "{:?}", outcome.lines);
        let calls = log.lock().unwrap().clone();
        assert!(
            calls.iter().any(|(method, params)| method == "promote"
                && params["id"] == "idea"
                && params["spec_path"] == "a.spec.md"
                && params["plan_path"] == "a.plan.md"),
            "{calls:?}"
        );
    }

    #[test]
    fn a_board_line_falls_back_to_the_title_when_there_is_no_plan() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global");
        fake_daemon(dir.path(), |method, params| match method {
            "switch.read" => Ok(json!({ "on": true, "source": "project" })),
            "list" => Ok(json!({
                "cards": [{ "id": "idea", "state": "pre_queued", "title": "一个想法" }]
            })),
            other => installed(other, params),
        });
        let outcome = handle_kanban_with(dir.path(), "", &global);
        assert!(
            outcome
                .lines
                .iter()
                .any(|line| line.contains("idea") && line.contains("一个想法")),
            "{:?}",
            outcome.lines
        );
    }
```

（该文件既有测试统一用 `let global = dir.path().join("global");` 再传给 `handle_kanban_with`——上面照此写法，不引入新辅助。`installed` 桩里已加的两个新方法见上一步。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent superpowers_kanban`
Expected: FAIL——未知子命令落到 usage；卡片行缺 title。

- [ ] **Step 3: 实现——两个 verb**

在 `handle_kanban_with` 的 `"add"` 臂之后加两个臂（照抄 `add` 臂的形状）：

```rust
        _ if argument == "add-prequeued" || argument.starts_with("add-prequeued ") => {
            let title = argument
                .strip_prefix("add-prequeued")
                .map(str::trim)
                .unwrap_or("");
            if title.is_empty() {
                plain(vec!["usage: /superpowers-kanban add-prequeued <title>".to_string()])
            } else {
                match query_plugin(workdir, "enqueue_prequeued", json!({ "title": title })) {
                    Ok(value) => {
                        let id = value.get("id").and_then(Value::as_str).unwrap_or("card");
                        plain(vec![format!("Superpowers 看板: delivered {id} to inbox")])
                    }
                    Err(QueryFailure::PluginMissing) => unavailable(),
                    Err(QueryFailure::Other(message)) => {
                        plain(vec![format!("Superpowers 看板: could not deliver: {message}")])
                    }
                }
            }
        }
        _ if argument == "promote" || argument.starts_with("promote ") => {
            let mut parts = argument.split_whitespace();
            let _verb = parts.next();
            match (parts.next(), parts.next(), parts.next(), parts.next()) {
                (Some(id), Some(spec), Some(plan), title) => {
                    let mut params = json!({ "id": id, "spec_path": spec, "plan_path": plan });
                    if let Some(title) = title {
                        params["title"] = json!(title);
                    }
                    match query_plugin(workdir, "promote", params) {
                        Ok(value) => {
                            let id = value.get("id").and_then(Value::as_str).unwrap_or(id);
                            plain(vec![format!("Superpowers 看板: delivered promote for {id}")])
                        }
                        Err(QueryFailure::PluginMissing) => unavailable(),
                        Err(QueryFailure::Other(message)) => plain(vec![format!(
                            "Superpowers 看板: could not deliver: {message}"
                        )]),
                    }
                }
                _ => plain(vec![
                    "usage: /superpowers-kanban promote <card-id> <spec> <plan> [<title>]"
                        .to_string(),
                ]),
            }
        }
```

usage 兜底那行同步扩展：

```rust
            "usage: /superpowers-kanban [on|off|create|remove|status|run|add <spec> <plan>|add-prequeued <title>|promote <id> <spec> <plan> [<title>]]"
                .to_string(),
```

- [ ] **Step 4: 实现——render_board 回退 title**

把 `where_` 的取值链加上 `title`：

```rust
        let where_ = card
            .get("workdir")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .or_else(|| card.get("plan_path").and_then(Value::as_str))
            .filter(|path| !path.is_empty())
            .or_else(|| card.get("title").and_then(Value::as_str))
            .unwrap_or("");
```

（给 `plan_path` 也加 `.filter(|path| !path.is_empty())`：预排队卡的 `plan_path` 是空串，不能当成落点。）

- [ ] **Step 5: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent superpowers_kanban && cargo fmt --all`
Expected: 全绿。

- [ ] **Step 6: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/superpowers_kanban.rs
git commit -m "feat(tui): add add-prequeued/promote verbs and title fallback on the board"
```

---

### Task 7: status 计数含预排队

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-boards/src/lifecycle.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/board.rs`

**Interfaces:**
- Consumes: Task 4 `list()` 的 `state: "pre_queued"`。
- Produces: `BoardStatus.pre_queued: usize`；`/superpowers-kanban status` 行含「预排队 N」。

- [ ] **Step 1: 写失败测试（lifecycle.rs 的 `mod tests` 追加）**

```rust
    #[test]
    fn status_counts_pre_queued_cards_too() {
        let project = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let _daemon = daemon_answering_cards(
            project.path(),
            vec![
                json!({ "id": "i", "state": "pre_queued", "title": "一个想法" }),
                json!({ "id": "q", "state": "queued" }),
            ],
        );
        let reported = status_with(project.path(), global.path());
        assert_eq!(reported.pre_queued, 1, "{reported:?}");
        assert_eq!(reported.queued, 1, "{reported:?}");
    }
```

（`daemon_answering_cards` 指同文件既有测试里「在项目 socket 上起一个回 `list` 的假 daemon」的辅助；沿用它的现名与签名，只把返回的 cards 换成上面这组。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-boards lifecycle`
Expected: 编译错误 `no field pre_queued`。

- [ ] **Step 3: 实现——lifecycle.rs**

`BoardStatus` 加字段：

```rust
    /// Cards decided on but not yet designed (no spec/plan).
    pub pre_queued: usize,
```

`status_with` 改为：

```rust
    let (queued, running, pre_queued) = if daemon_running {
        card_counts(project).unwrap_or((0, 0, 0))
    } else {
        (0, 0, 0)
    };
    BoardStatus {
        registered,
        daemon_running,
        queued,
        running,
        pre_queued,
    }
```

`create_with` 里那处 `BoardStatus { .. }` 同样补 `pre_queued: 0,`。

`card_counts` 返回三元组：

```rust
fn card_counts(project: &Path) -> Option<(usize, usize, usize)> {
    // ... 不变，直到计数循环
    let mut queued = 0;
    let mut running = 0;
    let mut pre_queued = 0;
    for card in cards {
        match card.get("state").and_then(serde_json::Value::as_str) {
            Some("queued") => queued += 1,
            Some("running") => running += 1,
            Some("pre_queued") => pre_queued += 1,
            _ => {}
        }
    }
    Some((queued, running, pre_queued))
}
```

- [ ] **Step 4: 实现——TUI status 行**

`yi-agent-rs/crates/yi-agent/src/tui/board.rs` 的 `render_status`，daemon 在跑那行改为：

```rust
        vec![format!(
            "本目录的看板已登记，daemon 在跑（排队 {}，运行中 {}，预排队 {}）",
            status.queued, status.running, status.pre_queued
        )]
```

同文件测试若断言了旧措辞，同步更新；并加一例断言三计数都在行内。

- [ ] **Step 5: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-boards && cargo test -p yi-agent tui && cargo fmt --all`
Expected: 全绿。

- [ ] **Step 6: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-boards/src/lifecycle.rs yi-agent-rs/crates/yi-agent/src/tui/board.rs
git commit -m "feat(board): count pre-queued cards in board status"
```

---

### Task 8: 桌面列映射与标题

**Files:**
- Modify: `desktop/src/lib/superpowersKanbanBoard.ts`
- Modify: `desktop/src/lib/superpowersKanbanState.ts`
- Modify: `desktop/src/components/SuperpowersKanbanBoard.tsx`
- Modify: `desktop/src/components/SuperpowersKanbanColumn.tsx`
- Test: `desktop/src/lib/superpowersKanbanBoard.test.ts`、`desktop/src/lib/superpowersKanbanState.test.ts`

**Interfaces:**
- Consumes: 插件 `list()` 的 `state: "pre_queued"` 与 `title`。
- Produces: `BoardColumnKey` 含 `"preQueued"`；`boardColumns` 返回五列；`normalizeCard().title` 优先显式 `title`；桌面渲染最左常驻 PRE-QUEUED 列。

- [ ] **Step 1: 写失败测试**

`desktop/src/lib/superpowersKanbanBoard.test.ts`：

```ts
  it("maps pre_queued to its own column", () => {
    expect(columnForState("pre_queued")).toBe("preQueued");
  });

  it("orders pre-queued by FIFO order ascending", () => {
    const cols = boardColumns([
      card("b", "pre_queued", { order: 5 }),
      card("a", "pre_queued", { order: 1 }),
    ]);
    expect(cols.preQueued.map((c) => c.id)).toEqual(["a", "b"]);
  });

  it("collects pre-queued cards into the preQueued column", () => {
    const cols = boardColumns([card("p", "pre_queued"), card("q", "queued")]);
    expect(cols.preQueued.map((c) => c.id)).toEqual(["p"]);
    expect(cols.queued.map((c) => c.id)).toEqual(["q"]);
  });
```

`desktop/src/lib/superpowersKanbanState.test.ts`：

```ts
  it("prefers an explicit title over the spec-derived one", () => {
    const parsed = parseBoard(
      JSON.stringify({
        cards: [
          { id: "idea", state: "pre_queued", title: "一个想法", spec_path: "", plan_path: "" },
        ],
      }),
    );
    expect(parsed[0].title).toBe("一个想法");
  });

  it("still falls back to the spec-derived title when none is given", () => {
    const parsed = parseBoard(
      JSON.stringify({
        cards: [
          { id: "x", state: "queued", spec_path: "/d/2026-10-03-foo.spec.md", plan_path: "/d/p.md" },
        ],
      }),
    );
    expect(parsed[0].title).toBe("2026-10-03-foo");
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/superpowersKanbanBoard.test.ts src/lib/superpowersKanbanState.test.ts`
Expected: FAIL——`columnForState("pre_queued")` 返回 `"doing"`；标题回退到 id。

- [ ] **Step 3: 实现——superpowersKanbanBoard.ts**

```ts
export type BoardColumnKey = "preQueued" | "queued" | "doing" | "needDecision" | "done";

export interface BoardColumns {
  preQueued: BoardCard[];
  queued: BoardCard[];
  doing: BoardCard[];
  needDecision: BoardCard[];
  done: BoardCard[];
}

export function columnForState(state: string): BoardColumnKey {
  switch (state) {
    case "pre_queued":
      return "preQueued";
    case "queued":
    // ... 其余不变
```

`boardColumns` 初始化并入新列，并沿用 queued 的排序规则：

```ts
  const columns: BoardColumns = {
    preQueued: [],
    queued: [],
    doing: [],
    needDecision: [],
    done: [],
  };
  for (const card of cards) columns[columnForState(card.state)].push(card);

  columns.preQueued.sort((l, r) => (l.order ?? 0) - (r.order ?? 0));
  columns.queued.sort((l, r) => (l.order ?? 0) - (r.order ?? 0));
```

- [ ] **Step 4: 实现——superpowersKanbanState.ts**

`normalizeCard` 的返回里改标题一行：

```ts
    title:
      typeof record.title === "string" && record.title !== ""
        ? record.title
        : deriveTitle(specPath, id),
```

- [ ] **Step 5: 实现——组件**

`SuperpowersKanbanBoard.tsx`：

```ts
  const order: BoardColumnKey[] = ["preQueued", "queued", "doing", "needDecision", "done"];
```

`SuperpowersKanbanColumn.tsx` 的 `COLUMN_LABEL`：

```ts
const COLUMN_LABEL: Record<BoardColumnKey, string> = {
  preQueued: "PRE-QUEUED",
  queued: "QUEUED",
  doing: "DOING",
  needDecision: "NEED DECISION",
  done: "DONE",
};
```

- [ ] **Step 6: 跑测试与类型检查**

Run: `cd desktop && npx vitest run src/lib/superpowersKanbanBoard.test.ts src/lib/superpowersKanbanState.test.ts src/components/SuperpowersKanbanBoard.test.tsx && npx tsc --noEmit`
Expected: 全绿；`tsc` 无错（`BoardColumns` 新增字段会强制所有使用点被覆盖）。

- [ ] **Step 7: Commit**

```bash
cd desktop && npx tsc --noEmit
git add desktop/src/lib/superpowersKanbanBoard.ts desktop/src/lib/superpowersKanbanState.ts desktop/src/components/SuperpowersKanbanBoard.tsx desktop/src/components/SuperpowersKanbanColumn.tsx desktop/src/lib/superpowersKanbanBoard.test.ts desktop/src/lib/superpowersKanbanState.test.ts
git commit -m "feat(desktop): render a PRE-QUEUED column and prefer explicit card titles"
```

---

### Task 9: 桌面入口——预排队建卡与提升

**Files:**
- Modify: `desktop/src/lib/superpowersKanbanSwitch.ts`
- Modify: `desktop/src/components/SuperpowersKanbanEnqueue.tsx`
- Modify: `desktop/src/components/SuperpowersKanbanCard.tsx`
- Modify: `desktop/src/components/SuperpowersKanbanColumn.tsx`
- Modify: `desktop/src/components/SuperpowersKanbanView.tsx`
- Modify: `desktop/src/App.tsx`
- Test: `desktop/src/components/SuperpowersKanbanEnqueue.test.tsx`

**Interfaces:**
- Consumes: Task 4 的 IPC `enqueue_prequeued` / `promote`。
- Produces:
  - `enqueuePrequeuedCard(rpc, project, title): Promise<void>`
  - `promoteBoardCard(rpc, project, cardId, specPath, planPath): Promise<void>`
  - `SuperpowersKanbanEnqueue` 新 prop `prequeue: (title: string) => Promise<void>`
  - `SuperpowersKanbanCard` 新可选 prop `onPromote?: (cardId: string) => void`（`state === "pre_queued"` 时显示按钮），经 Column/View 透传

- [ ] **Step 1: 写失败测试（SuperpowersKanbanEnqueue.test.tsx 追加）**

```tsx
  it("delivers a pre-queued card from the title input", async () => {
    const calls: string[] = [];
    render(
      <SuperpowersKanbanEnqueue
        pickFile={async () => null}
        enqueue={async () => {}}
        prequeue={async (title) => {
          calls.push(title);
        }}
      />,
    );
    await userEvent.type(screen.getByLabelText("预排队标题"), "my idea");
    await userEvent.click(screen.getByRole("button", { name: "预排队" }));
    expect(calls).toEqual(["my idea"]);
    expect(await screen.findByText("已投递，等待插件校验")).toBeTruthy();
  });

  it("ignores an empty pre-queued title", async () => {
    const calls: string[] = [];
    render(
      <SuperpowersKanbanEnqueue
        pickFile={async () => null}
        enqueue={async () => {}}
        prequeue={async (title) => {
          calls.push(title);
        }}
      />,
    );
    await userEvent.click(screen.getByRole("button", { name: "预排队" }));
    expect(calls).toEqual([]);
  });
```

（import/`userEvent`/`render` 沿用该测试文件已有的写法。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/SuperpowersKanbanEnqueue.test.tsx`
Expected: FAIL——找不到 `预排队标题` 输入框。

- [ ] **Step 3: 实现——RPC 辅助**

`superpowersKanbanSwitch.ts`，在 `enqueueBoardCard` 之后加：

```ts
/** 经插件给某项目投递一张预排队卡（只有标题，没有 spec/plan）。 */
export async function enqueuePrequeuedCard(
  rpc: BoardRpc,
  project: string,
  title: string,
): Promise<void> {
  await queryPlugin(rpc, project, "enqueue_prequeued", { title });
}

/** 经插件提升一张预排队卡：补 spec/plan，插件下个 tick 翻成排队卡。 */
export async function promoteBoardCard(
  rpc: BoardRpc,
  project: string,
  cardId: string,
  specPath: string,
  planPath: string,
): Promise<void> {
  await queryPlugin(rpc, project, "promote", {
    id: cardId,
    spec_path: specPath,
    plan_path: planPath,
  });
}
```

- [ ] **Step 4: 实现——Enqueue 组件**

```tsx
export function SuperpowersKanbanEnqueue({
  pickFile,
  enqueue,
  prequeue,
}: {
  pickFile: () => Promise<string | null>;
  enqueue: (specPath: string, planPath: string) => Promise<void>;
  prequeue: (title: string) => Promise<void>;
}) {
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [title, setTitle] = useState("");

  // ... onEnqueue 不变

  const onPrequeue = async () => {
    if (busy) return;
    const trimmed = title.trim();
    if (trimmed === "") return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await prequeue(trimmed);
      setTitle("");
      setNotice("已投递，等待插件校验");
    } catch (e) {
      setError(formatError(e));
    } finally {
      setBusy(false);
    }
  };
```

渲染部分在既有「加入看板」按钮之后加一行（保持既有视觉风格）：

```tsx
      <div className="mt-2 flex items-center gap-1">
        <input
          aria-label="预排队标题"
          value={title}
          disabled={busy}
          onChange={(e) => setTitle(e.currentTarget.value)}
          placeholder="还没写 spec/plan？先记个标题"
          className="min-w-0 flex-1 rounded border border-line bg-panel px-2 py-1 text-xs text-fg"
        />
        <button
          type="button"
          aria-label="预排队"
          disabled={busy}
          onClick={() => void onPrequeue()}
          className="shrink-0 cursor-pointer rounded border border-line-strong px-2 py-1 text-xs text-fg-muted hover:text-fg disabled:opacity-50"
        >
          预排队
        </button>
      </div>
```

- [ ] **Step 5: 实现——提升按钮透传**

`SuperpowersKanbanCard.tsx`：加 `onPromote?: (cardId: string) => void`，在卡片底部（`threadId` 按钮之后）加：

```tsx
      {card.state === "pre_queued" && onPromote ? (
        <button
          type="button"
          onClick={() => onPromote(card.id)}
          className="mt-1 text-xs text-fg-muted hover:text-fg"
        >
          补 spec/plan 提升
        </button>
      ) : null}
```

`SuperpowersKanbanColumn.tsx` 与 `SuperpowersKanbanView.tsx`：加同名可选 prop `onPromote`，逐层透传给 `SuperpowersKanbanCard`（Column 的 `props` 解构与 View 的既有 `onOpenThread` 透传同一个写法）。

- [ ] **Step 6: 实现——App 接线**

`App.tsx` 的 `SuperpowersKanbanEnqueue` 处加 `prequeue` prop：

```tsx
                prequeue={(title) =>
                  enqueuePrequeuedCard(board.boardRpc, board.selectedBoard!, title)
                }
```

`SuperpowersKanbanView` 处加 `onPromote`：

```tsx
                onPromote={(cardId) => {
                  void (async () => {
                    const spec = await pickFile();
                    if (spec === null) return;
                    const plan = await pickFile();
                    if (plan === null) return;
                    await promoteBoardCard(
                      board.boardRpc,
                      board.selectedBoard!,
                      cardId,
                      spec,
                      plan,
                    );
                    board.refreshBoards();
                  })();
                }}
```

（`pickFile` 已是该文件既有变量；`refreshBoards` 取自 `useBoardState` 的返回。失败文案沿用该面板既有的 `setBoardError` 通路——若拿不到就退化为 `console.error`，但优先接既有错误通路。）

- [ ] **Step 7: 跑测试与类型检查**

Run: `cd desktop && npx vitest run src/components/SuperpowersKanbanEnqueue.test.tsx && npx tsc --noEmit && npm test`
Expected: 全绿，`tsc` 无错。

- [ ] **Step 8: Commit**

```bash
git add desktop/src/lib/superpowersKanbanSwitch.ts desktop/src/components/SuperpowersKanbanEnqueue.tsx desktop/src/components/SuperpowersKanbanCard.tsx desktop/src/components/SuperpowersKanbanColumn.tsx desktop/src/components/SuperpowersKanbanView.tsx desktop/src/App.tsx desktop/src/components/SuperpowersKanbanEnqueue.test.tsx
git commit -m "feat(desktop): enqueue pre-queued cards and promote them from the board"
```

---

### Task 10: 文档与 skill 同步

**Files:**
- Modify: `plugins/superpowers-kanban/README.md`
- Modify: `plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md`
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/README.md`（仅当计数变化）

**Interfaces:**
- Consumes: 全部实现（描述须与代码一致）。
- Produces: 无代码接口。

- [ ] **Step 1: 插件 README**

在介绍卡片/状态的小节里加一段：列出五列/状态，说明 `pre_queued` 的语义（不占槽、不进会话），并给两条新命令的用法：

```markdown
### 预排队（pre-queued）

需求已决定要做、但 spec/plan 还没写时，先记一张预排队卡持续追踪：

    superpowers-kanban add-prequeued "需求标题"

它出现在看板最左的 `PRE-QUEUED` 列，不占并发槽、不会被起会话。设计写完、两份
文件落盘后提升为正式排队卡：

    superpowers-kanban promote <card-id> <spec> <plan> [--title <新标题>]

卡片的 id 在创建时就定死，提升不重算，因此从预排队到 done 始终是同一张卡。
```

- [ ] **Step 2: 插件 SKILL.md**

把「前提：spec 和 plan 必须已经存在」一节改成同时给出两条路径：正式入队（spec+plan 已存在，`add`）与前植入队（还没写，`add-prequeued <title>` → 写完 `promote <id> <spec> <plan>`）。在「怎么做」里加 `add-prequeued` / `promote` 两条命令示例，并强调「提升时 id 不变、会重排到队尾」。

- [ ] **Step 3: 项目进度**

在 `docs/project-management/desktop.md` 的看板相关条目前加一条 `[x]`：

```markdown
- [x] 看板 PRE-QUEUED 列（需求已定、spec/plan 待写）— 新状态 `pre_queued`（`plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs`，不占并发槽、`next_startable` 天然不选它）；id 由 `slug(title)` 创建时定死（非 ASCII 标题回退 `prequeued-<时间戳>`，`board.rs:next_free_prequeued_id`），`promote` 只补 `spec_path`/`plan_path` + 翻 `queued` + 重排 `order` 到队尾，**不重算 id**；写入全走 `inbox/`，`consume` 两趟（先建卡后提升）；入口 CLI `add-prequeued`/`promote`、IPC `enqueue_prequeued`/`promote`、TUI 同名 verb + `status` 含「预排队 N」（`yi-agent-boards/src/lifecycle.rs`）、桌面最左常驻 `PRE-QUEUED` 列（`desktop/src/lib/superpowersKanbanBoard.ts` 的 `columnForState`）。判据：`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core -p superpowers-kanban-runner`、`cd yi-agent-rs && cargo test -p yi-agent-boards -p yi-agent`、`cd desktop && npm test` 全绿；手工 `add-prequeued` → 最左出现卡片 → `promote` → 移到 QUEUED 且 id 不变
```

按需更新 `docs/project-management/README.md` 的 desktop 计数（新增 1 条即 `53/65` → `54/65`；若总条目数也变，则同步总数）。

- [ ] **Step 4: 校验**

Run: `cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/kanban-pre-queued && grep -c "pre_queued" docs/project-management/desktop.md plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md`
Expected: 两个文件都 > 0，确认改动落到位。

- [ ] **Step 5: Commit**

```bash
git add plugins/superpowers-kanban/README.md plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md docs/project-management/desktop.md docs/project-management/README.md
git commit -m "docs(kanban): document the pre-queued column and promote path"
```

---

## 收尾（全部任务完成后）

- [ ] 在改动涉及的每个 workspace 跑一遍：`cd plugins/superpowers-kanban && cargo test && cargo fmt --all`、`cd yi-agent-rs && cargo test -p yi-agent-boards -p yi-agent && cargo fmt --all`、`cd desktop && npm test && npx tsc --noEmit`。
- [ ] 手工走一遍端到端：`add-prequeued "示例需求"` → 桌面最左列出现卡片 → `promote <id> <spec> <plan>` → 卡片移到 QUEUED、id 未变、排在队尾。
- [ ] 走 `superpowers:finishing-a-development-branch`：合并回 `main`（`--no-ff`），删分支与 worktree。
