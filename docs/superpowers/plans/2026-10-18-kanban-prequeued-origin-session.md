# 看板 pre-queued 卡关联讨论会话 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让看板 pre-queued 卡记住「提出它的讨论会话」，并在桌面看板上可点击跳回那个会话；该关联独立于执行会话（`thread_id`），提升/开跑都不覆盖它。

**Architecture:** 插件侧 `Card` 新增 `origin_thread_id` 字段，经既有 `inbox/` 投递通道把会话 id 带到 runner 消费（保持「`board.json` 唯一写者是 runner」不变量）。会话身份的来源分两条：桌面端面板投递时带上 `sess.currentId`；会话内 CLI 由宿主在装配期把 thread id 注入 **bash 子进程环境变量** `YI_AGENT_THREAD_ID`（模型/CLI 无感）。拿不到时静默留空，不阻断投递。

**Tech Stack:** Rust（`superpowers-kanban-core`/`-runner` 独立 workspace、`yi-agent-tools`/`-runtime`/`-app-server`）、TypeScript + React + vitest（`desktop/`）、serde（`#[serde(default)]` 前向兼容）。

## Global Constraints

- 严禁在 `main` 分支上改动；一切变更在本 worktree 的分支 `feat/kanban-prequeued-origin-session` 上进行。
- 提交前必须跑格式化：`cd yi-agent-rs && cargo fmt --all`；插件侧同样跑 `cd plugins/superpowers-kanban && cargo fmt --all`。
- commit message 用 conventional commits 风格，**不要**写 `Co-Authored-By` 行。
- `board.json` 的唯一写者是 runner 推进循环；任何外部写入（CLI/TUI/桌面/dispatch）**只能**投递到 `<state_dir>/inbox/`，不得直接改 `board.json`。
- 新字段必须带 `#[serde(default)]`：旧 `board.json` 缺该键时反序列化为 `None`，**不得**整份解析失败。
- 插件（`plugins/superpowers-kanban/`）**零** `yi-agent-*` 依赖，协议字段手写。
- 环境变量名固定为 `YI_AGENT_THREAD_ID`，不得改名。
- 跑测试前先 `ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent"` 确认无残留进程；被中断的测试会留下持有 target 锁的僵尸进程。
- 插件测试命令：`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core -p superpowers-kanban-runner`。
- 桌面测试命令：`cd desktop && npm test`。

---

### Task 1: 插件 core —— `Card.origin_thread_id` 字段与前向兼容

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs`（`Card` 结构体，紧随 `thread_id` 之后；`mod tests` 末尾）
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/board.rs:43-73`（`enqueue`）、`:76-105`（`enqueue_merge`）、`:107-135`（`enqueue_prequeued`）

**Interfaces:**
- Consumes: 无（本任务是最底层）。
- Produces:
  - `Card.origin_thread_id: Option<String>`（serde 键 `origin_thread_id`，`#[serde(default)]`）；
  - `Board::enqueue_prequeued(&mut self, id: CardId, title: String, origin_thread_id: Option<String>, now: DateTime<Local>) -> &Card`（**新增第 3 个参数**，插在 `title` 与 `now` 之间）。

- [ ] **Step 1: 写失败的测试**

在 `card.rs` 的 `mod tests` 末尾追加（该文件已有 `use super::*;`）：

```rust
    #[test]
    fn an_old_card_without_an_origin_thread_still_deserializes() {
        // 旧 board.json 没有 origin_thread_id：必须回 None，不得整份解析失败。
        let json = r#"{"id":"a","spec_path":"a.spec.md","plan_path":"a.plan.md","state":"queued","enqueued_at":"2026-10-01T00:00:00+08:00","order":0}"#;
        let card: Card = serde_json::from_str(json).unwrap();
        assert_eq!(card.origin_thread_id, None);
    }
```

在 `board.rs` 的 `mod tests` 末尾追加：

```rust
    #[test]
    fn a_prequeued_card_keeps_the_discussion_session_it_was_captured_in() {
        let mut board = Board::new();
        board.enqueue_prequeued(
            CardId::new("idea"),
            "一个想法".into(),
            Some("thread-discuss-1".into()),
            at(1, 0),
        );
        let card = board.get(&CardId::new("idea")).unwrap();
        assert_eq!(card.origin_thread_id.as_deref(), Some("thread-discuss-1"));
    }

    #[test]
    fn a_prequeued_card_without_a_discussion_session_has_none() {
        let mut board = Board::new();
        board.enqueue_prequeued(CardId::new("idea"), "一个想法".into(), None, at(1, 0));
        assert_eq!(
            board.get(&CardId::new("idea")).unwrap().origin_thread_id,
            None
        );
    }

    #[test]
    fn implementation_and_merge_cards_have_no_origin_thread() {
        // 只有预排队卡记录讨论会话；实现卡/合并卡的该字段为空。
        let mut board = Board::new();
        board.enqueue(
            CardId::new("impl"),
            "a.spec.md".into(),
            "a.plan.md".into(),
            at(1, 0),
        );
        board.enqueue_merge(
            CardId::new("merge"),
            "kanban/a".into(),
            "main".into(),
            None,
            at(1, 0),
        );
        assert_eq!(board.get(&CardId::new("impl")).unwrap().origin_thread_id, None);
        assert_eq!(
            board.get(&CardId::new("merge")).unwrap().origin_thread_id,
            None
        );
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core --lib`
Expected: 编译失败，报 `no field origin_thread_id` / `this function takes 4 arguments but 5 were supplied`。

- [ ] **Step 3: 最小实现**

`card.rs` 的 `Card` 结构体，在 `pub thread_id: Option<String>,` 之后插入：

```rust
    /// 提出这张卡的**讨论会话**。与 `thread_id`（跑这张卡的执行会话）语义不同，
    /// 故不复用后者：`thread_id` 会被调度器的 `mark_running` 覆写成执行会话，
    /// 复用它会让溯源线在提升/开跑时断掉。旧 board.json 缺此字段 → `None`。
    #[serde(default)]
    pub origin_thread_id: Option<String>,
```

`board.rs` 三个构造点各补一行 `origin_thread_id:`。

`enqueue` 与 `enqueue_merge` 的 `Card { .. }` 字面量里加（放在 `thread_id: None,` 之后）：

```rust
            origin_thread_id: None,
```

`enqueue_prequeued` 签名与字面量改为：

```rust
    /// 排入一张预排队卡：无 spec/plan，只有一个标题；id 由调用方定死。
    /// `origin_thread_id` 是提出它的讨论会话（拿不到时为 `None`）。
    pub fn enqueue_prequeued(
        &mut self,
        id: CardId,
        title: String,
        origin_thread_id: Option<String>,
        now: DateTime<Local>,
    ) -> &Card {
```

其字面量里把 `thread_id: None,` 之后加：

```rust
            origin_thread_id,
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core --lib`
Expected: 仍会编译失败——**core 内部既有调用点**（`board.rs` 的 `a_pre_queued_card_is_queued_nowhere_near_a_slot`、`a_pre_queued_id_falls_back_and_gets_a_suffix_on_collision`、`promoting_a_card_that_is_not_pre_queued_is_rejected`、`a_promote_can_overwrite_the_title` 等，以及 `inbox.rs` 测试）都调用 `enqueue_prequeued(...)`，需要补参数。

- [ ] **Step 5: 修 core 内部既有调用点**

`board.rs` 与 `inbox.rs` 的 `mod tests` 里逐个 `enqueue_prequeued(...)` 调用在 `title` 与 `now` 之间补 `None`；在要断言溯源保留的用例（`promote` 系列）传 `Some("thread-x".into())`。**用编译器报错逐个定位，不猜**（已知位置：`board.rs:807/836/851/896`、`inbox.rs:279`）。

- [ ] **Step 6: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core --lib`
Expected: PASS，0 failures。（`-p` 只编译 core 一方，故 `superpowers-kanban-runner` 的调用点此处不会报错——那归 Task 3。）

- [ ] **Step 7: 提交**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs plugins/superpowers-kanban/crates/superpowers-kanban-core/src/board.rs
git commit -m "feat(kanban-core): add origin_thread_id to cards"
```

---

### Task 2: 插件 core —— `promote` 保留 `origin_thread_id`、投递形状带该字段

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/inbox.rs:70-79`（`deliver_prequeued_card` 签名与 body；`mod tests`）

**Interfaces:**
- Consumes: Task 1 的 `Card.origin_thread_id`。
- Produces: `deliver_prequeued_card(state_dir: &Path, id: &str, title: &str, origin_thread_id: Option<&str>) -> std::io::Result<()>`（**新增第 4 个参数**）。

- [ ] **Step 1: 写失败的测试**

在 `inbox.rs` 的 `mod tests` 里，把既有 `delivers_a_prequeued_card_with_its_kind_and_title` 更新为带会话的版本，并追加两个测试：

```rust
    #[test]
    fn delivers_a_prequeued_card_with_its_discussion_session() {
        let dir = tempfile::tempdir().unwrap();
        deliver_prequeued_card(dir.path(), "idea", "一个想法", Some("thread-abc")).unwrap();
        let text = std::fs::read_to_string(dir.path().join("inbox/idea.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["kind"], "prequeued");
        assert_eq!(value["id"], "idea");
        assert_eq!(value["title"], "一个想法");
        assert_eq!(value["origin_thread_id"], "thread-abc");
    }

    #[test]
    fn a_prequeued_delivery_without_a_session_omits_the_key() {
        // 没有会话时**不落**该键（与 deliver_promote_card 对 title 的处理同款）。
        let dir = tempfile::tempdir().unwrap();
        deliver_prequeued_card(dir.path(), "idea", "一个想法", None).unwrap();
        let text = std::fs::read_to_string(dir.path().join("inbox/idea.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(value.get("origin_thread_id").is_none());
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core --lib`
Expected: 编译失败，`deliver_prequeued_card` 参数个数不符。

- [ ] **Step 3: 最小实现**

```rust
/// 投递一张预排队卡：`{"kind":"prequeued","id","title"[,"origin_thread_id"]}`。
/// `origin_thread_id` 为 `None` 时不落该键。
pub fn deliver_prequeued_card(
    state_dir: &Path,
    id: &str,
    title: &str,
    origin_thread_id: Option<&str>,
) -> std::io::Result<()> {
    let dir = inbox_dir(state_dir);
    std::fs::create_dir_all(&dir)?;
    let path = enqueue_path(state_dir, id);
    let mut body = serde_json::json!({ "kind": "prequeued", "id": id, "title": title });
    if let Some(origin) = origin_thread_id {
        body["origin_thread_id"] = serde_json::Value::String(origin.to_string());
    }
    let text = serde_json::to_string_pretty(&body).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}
```

- [ ] **Step 4: 写 `promote` 保留溯源的测试**

在 `board.rs` 的 `mod tests` 追加：

```rust
    #[test]
    fn promoting_a_prequeued_card_keeps_its_origin_thread() {
        // 提升不重算 id、也不动溯源：讨论会话随卡走到 Queued。
        let mut board = Board::new();
        board.enqueue_prequeued(
            CardId::new("idea"),
            "一个想法".into(),
            Some("thread-discuss-1".into()),
            at(1, 0),
        );
        board
            .promote_prequeued(
                &CardId::new("idea"),
                "a.spec.md".into(),
                "a.plan.md".into(),
                None,
            )
            .unwrap();
        let card = board.get(&CardId::new("idea")).unwrap();
        assert_eq!(card.state, CardState::Queued);
        assert_eq!(card.origin_thread_id.as_deref(), Some("thread-discuss-1"));
    }
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core --lib`
Expected: PASS。`promote_prequeued` 本身无需改动（它不触碰该字段）；若测试失败说明实现里误改了，回查。

- [ ] **Step 6: 提交**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add plugins/superpowers-kanban/crates/superpowers-kanban-core/src/inbox.rs plugins/superpowers-kanban/crates/superpowers-kanban-core/src/board.rs
git commit -m "feat(kanban-core): carry origin_thread_id through prequeued delivery"
```

---

### Task 3: 插件 runner —— 消费投递、CLI 读 env、dispatch 可选参数

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/inbox.rs:17-35`（`RawDelivery`）、`:145-160`（prequeued 分支；`mod tests`）
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs:439-457`（`command_prequeued`；`mod tests`）
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/dispatch.rs:94-106`（`enqueue_prequeued` 分支；测试模块）
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs:617-631`（`list` 输出）

**Interfaces:**
- Consumes: Task 1 的 `Board::enqueue_prequeued(id, title, origin_thread_id, now)`；Task 2 的 `deliver_prequeued_card(state_dir, id, title, origin_thread_id)`。
- Produces:
  - CLI `superpowers-kanban add-prequeued <title>` 读环境变量 `YI_AGENT_THREAD_ID`（非空才用）；
  - IPC `enqueue_prequeued` 接受可选 params 字段 `origin_thread_id`；
  - `list` 每张卡 JSON 输出键 `origin_thread_id`。

- [ ] **Step 1: 写失败的测试（消费侧）**

在 `inbox.rs` 的 `mod tests` 追加：

```rust
    #[test]
    fn a_prequeued_delivery_carries_its_origin_thread_onto_the_board() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("inbox")).unwrap();
        std::fs::write(
            dir.path().join("inbox/idea.json"),
            r#"{"kind":"prequeued","id":"idea","title":"my idea","origin_thread_id":"thread-abc"}"#,
        )
        .unwrap();
        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, Local::now());
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].result, Ok(()));
        let card = board.get(&CardId::new("idea")).unwrap();
        assert_eq!(card.origin_thread_id.as_deref(), Some("thread-abc"));
    }

    #[test]
    fn a_prequeued_delivery_without_an_origin_thread_still_lands() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("inbox")).unwrap();
        std::fs::write(
            dir.path().join("inbox/idea.json"),
            r#"{"kind":"prequeued","id":"idea","title":"my idea"}"#,
        )
        .unwrap();
        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, Local::now());
        assert_eq!(outcomes[0].result, Ok(()));
        assert_eq!(
            board.get(&CardId::new("idea")).unwrap().origin_thread_id,
            None
        );
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --lib`
Expected: 编译失败（`RawDelivery` 无 `origin_thread_id`；`enqueue_prequeued` 参数不符）。

- [ ] **Step 3: 最小实现（消费侧）**

`RawDelivery` 在 `title` 字段后加：

```rust
    /// 预排队卡的来源讨论会话（可选）。
    #[serde(default)]
    origin_thread_id: Option<String>,
```

`inbox.rs` 的 prequeued 分支（`board.enqueue_prequeued(id, title, now);` 一行）改为：

```rust
        let origin_thread_id = delivery.origin_thread_id.clone();
        board.enqueue_prequeued(id, title, origin_thread_id, now);
```

（以该分支现有变量名为准调整；`title` 已在上一行解出。）

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --lib`
Expected: 新增两个测试 PASS。

- [ ] **Step 5: 写失败的测试（CLI 读 env）**

**关键：环境变量是进程级的，必须用互斥锁串行，并在结尾还原。** 本仓库既有范式见
`yi-agent-runtime/src/config.rs` 的 `ENV_TEST_MUTEX` + `EnvVarGuard`。在 `main.rs`
的 `mod tests` 顶部（模块内）加入这两个 helper，再写测试：

```rust
    /// 涉及进程级环境变量的测试必须串行，否则并行跑时互相干扰。
    static ENV_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 记录原值并在 drop 时还原，确保断言失败也不会把环境变量留给别的测试。
    struct EnvVarGuard {
        name: &'static str,
        original: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let original = std::env::var_os(name);
            // SAFETY: 递归持有 ENV_TEST_MUTEX，进程内无人并发读写该变量。
            unsafe { std::env::set_var(name, value) };
            Self { name, original }
        }

        fn unset(name: &'static str) -> Self {
            let original = std::env::var_os(name);
            // SAFETY: 同上。
            unsafe { std::env::remove_var(name) };
            Self { name, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: 同上。
            match &self.original {
                Some(value) => unsafe { std::env::set_var(self.name, value) },
                None => unsafe { std::env::remove_var(self.name) },
            }
        }
    }

    #[test]
    fn add_prequeued_reads_the_thread_id_from_the_environment() {
        let _lock = ENV_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvVarGuard::set("YI_AGENT_THREAD_ID", "thread-abc");
        let dir = tempfile::tempdir().unwrap();
        command_prequeued(dir.path(), "my idea").unwrap();
        let text = std::fs::read_to_string(dir.path().join("inbox/my-idea.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["origin_thread_id"], "thread-abc");
    }

    #[test]
    fn add_prequeued_without_the_env_var_still_delivers() {
        let _lock = ENV_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvVarGuard::unset("YI_AGENT_THREAD_ID");
        let dir = tempfile::tempdir().unwrap();
        command_prequeued(dir.path(), "my idea").unwrap();
        let text = std::fs::read_to_string(dir.path().join("inbox/my-idea.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(value.get("origin_thread_id").is_none());
    }

    #[test]
    fn add_prequeued_ignores_a_blank_thread_id() {
        // 空/空白 env 视为没有（宿主不该注入空串，但别让它污染数据）。
        let _lock = ENV_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvVarGuard::set("YI_AGENT_THREAD_ID", "   ");
        let dir = tempfile::tempdir().unwrap();
        command_prequeued(dir.path(), "my idea").unwrap();
        let text = std::fs::read_to_string(dir.path().join("inbox/my-idea.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(value.get("origin_thread_id").is_none());
    }
```

（若这些测试与同模块既有测试共用 `ENV_TEST_MUTEX` 名字冲突，改名或提到模块顶部共用一份。）

- [ ] **Step 6: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --lib add_prequeued`
Expected: FAIL（`origin_thread_id` 缺失）。

- [ ] **Step 7: 最小实现（CLI）**

```rust
/// `add-prequeued`：只给标题，投递一张预排队卡，id 在这里定死。
///
/// 会话内的 agent 经 bash 调用本命令时，宿主已在装配期把当前 thread id 注入
/// 环境变量 `YI_AGENT_THREAD_ID`（模型看不到自己的 thread id，只能由装配期注入）。
/// 读不到（会话外手动调用、TUI 本地会话）就留空——关联是尽力而为，不阻断投递。
fn command_prequeued(state_dir: &std::path::Path, title: &str) -> Result<String, String> {
    if title.trim().is_empty() {
        return Err("add-prequeued needs a non-empty title".to_string());
    }
    let origin_thread_id = std::env::var("YI_AGENT_THREAD_ID")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let board = superpowers_kanban_runner::persist::load_board(&state_dir.join("board.json"));
    let id = superpowers_kanban_core::inbox::next_free_prequeued_id(
        &board,
        state_dir,
        title,
        chrono::Local::now(),
    )
    .0;
    superpowers_kanban_core::inbox::deliver_prequeued_card(
        state_dir,
        &id,
        title,
        origin_thread_id.as_deref(),
    )
    .map_err(|error| format!("could not deliver the card: {error}"))?;
    Ok(format!(
        "delivered {id} to {}\n{title}",
        superpowers_kanban_core::inbox::inbox_dir(state_dir).display()
    ))
}
```

- [ ] **Step 8: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --lib add_prequeued`
Expected: PASS。

- [ ] **Step 9: 写失败的测试（dispatch 与 list）**

在 `dispatch.rs` 测试模块追加：

```rust
    #[test]
    fn enqueue_prequeued_passes_an_optional_origin_thread() {
        let dir = tempfile::tempdir().unwrap();
        let value = dispatch_with_global(
            dir.path(),
            None,
            "enqueue_prequeued",
            &json!({ "title": "my idea", "origin_thread_id": "thread-abc" }),
        )
        .unwrap();
        assert_eq!(value["id"], "my-idea");
        let text = std::fs::read_to_string(dir.path().join("inbox/my-idea.json")).unwrap();
        let delivery: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(delivery["origin_thread_id"], "thread-abc");
    }

    #[test]
    fn enqueue_prequeued_without_an_origin_thread_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let value = dispatch_with_global(
            dir.path(),
            None,
            "enqueue_prequeued",
            &json!({ "title": "my idea" }),
        )
        .unwrap();
        assert_eq!(value["id"], "my-idea");
    }

    #[test]
    fn list_reports_the_origin_thread_of_a_prequeued_card() {
        let dir = tempfile::tempdir().unwrap();
        let mut board = superpowers_kanban_core::board::Board::new();
        board.enqueue_prequeued(
            superpowers_kanban_core::card::CardId::new("idea"),
            "我的想法".into(),
            Some("thread-abc".into()),
            chrono::Local::now(),
        );
        crate::persist::save_board(&dir.path().join("board.json"), &board).unwrap();
        let value = dispatch_with_global(dir.path(), None, "list", &json!({})).unwrap();
        assert_eq!(value["cards"][0]["origin_thread_id"], "thread-abc");
    }
```

- [ ] **Step 10: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --lib`
Expected: FAIL（`enqueue_prequeued` 未读 `origin_thread_id`；`list` 无该键）。

- [ ] **Step 11: 最小实现（dispatch 与 list）**

`dispatch.rs` 的 `"enqueue_prequeued"` 分支改为：

```rust
        "enqueue_prequeued" => {
            let title = params
                .get("title")
                .and_then(Value::as_str)
                .ok_or_else(|| "enqueue_prequeued needs a `title`".to_string())?;
            if title.trim().is_empty() {
                return Err("enqueue_prequeued needs a non-empty `title`".to_string());
            }
            // 来源会话可选：桌面面板带上当前会话，其它调用方可以不带。
            let origin_thread_id = params
                .get("origin_thread_id")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty());
            let id = service.next_free_prequeued_id(title, chrono::Local::now());
            deliver_prequeued_card(state_dir, &id, title, origin_thread_id)
                .map_err(|error| format!("could not deliver the card: {error}"))?;
            Ok(json!({ "id": id }))
        }
```

`service.rs` 的 `list` JSON 里，在 `"thread_id": card.thread_id,` 之后加：

```rust
                "origin_thread_id": card.origin_thread_id,
```

**同时修 runner 内部全部既有调用点**（编译器会逐个报错，不猜）：
- `dispatch.rs` 的 `board.enqueue_prequeued(...)`：`:667`、`:778`、`:838`（测试内，补 `None`）；
- `main.rs:1646`（测试内，补 `None`）；
- `inbox.rs:157` 已在 Step 3 改过。

- [ ] **Step 12: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core -p superpowers-kanban-runner`
Expected: PASS，0 failures。

- [ ] **Step 13: 提交**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add plugins/superpowers-kanban/crates/superpowers-kanban-runner/src plugins/superpowers-kanban/crates/superpowers-kanban-core/src/inbox.rs
git commit -m "feat(kanban-runner): plumb origin_thread_id through cli, ipc and list"
```

（本任务同时修掉了 `inbox.rs` 测试里 `enqueue_prequeued` 的既有调用点：`inbox.rs:279` 补 `None`。若 Task 1 已顺手改过则跳过。）

---

### Task 4: `yi-agent-tools` —— `ToolsContext` 携带会话环境变量，`BashTool` 注入子进程

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/context.rs`（`ToolsContext` 结构体与方法；`mod tests`）
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/shell/bash.rs:168-183`（spawn 处；`mod tests`）
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/lib.rs:71-118`（两个注册入口）

**Interfaces:**
- Consumes: 无。
- Produces:
  - `ToolsContext::with_roots_and_env(roots: Vec<PathBuf>, session_env: Vec<(String, String)>) -> Self`；
  - `ToolsContext::session_env(&self) -> &[(String, String)]`；
  - `register_builtin_tools_with_controller_and_env(registry: &mut ToolRegistry, roots: Vec<PathBuf>, controller: SandboxController, sandbox_writable_roots: Vec<PathBuf>, session_env: Vec<(String, String)>)`；
  - 既有 `register_builtin_tools_with_controller` 与 `register_builtin_tools_with_sandbox` 签名**不变**，内部委托并传空 env。

- [ ] **Step 1: 写失败的测试（context）**

在 `context.rs` 的 `mod tests` 追加：

```rust
    #[test]
    fn a_context_without_env_exposes_an_empty_session_env() {
        let ctx = ToolsContext::new(PathBuf::from("/tmp/foo"));
        assert!(ctx.session_env().is_empty());
    }

    #[test]
    fn a_context_carries_its_session_env() {
        let ctx = ToolsContext::with_roots_and_env(
            vec![PathBuf::from("/tmp/foo")],
            vec![("YI_AGENT_THREAD_ID".to_string(), "thread-abc".to_string())],
        );
        assert_eq!(
            ctx.session_env(),
            &[("YI_AGENT_THREAD_ID".to_string(), "thread-abc".to_string())]
        );
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib context`
Expected: 编译失败（无 `session_env` / `with_roots_and_env`）。

- [ ] **Step 3: 最小实现（context）**

```rust
pub struct ToolsContext {
    roots: Vec<PathBuf>,
    cwd: Mutex<PathBuf>,
    /// 注入给 shell 子进程的环境变量（如当前会话的 thread id）。默认空集，
    /// 此时子进程行为与今天完全一致。
    session_env: Vec<(String, String)>,
}
```

`with_roots` 委托新构造器：

```rust
    pub fn with_roots(roots: Vec<PathBuf>) -> Self {
        Self::with_roots_and_env(roots, Vec::new())
    }

    /// 同 [`Self::with_roots`]，但额外带上注入 shell 子进程的环境变量。
    pub fn with_roots_and_env(roots: Vec<PathBuf>, session_env: Vec<(String, String)>) -> Self {
        let cwd = roots.first().cloned().unwrap_or_default();
        Self {
            roots,
            cwd: Mutex::new(cwd),
            session_env,
        }
    }

    /// 注入给 shell 子进程的环境变量（默认空）。
    pub fn session_env(&self) -> &[(String, String)] {
        &self.session_env
    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib context`
Expected: PASS。

- [ ] **Step 5: 写失败的测试（bash 注入）**

在 `shell/bash.rs` 的 `mod tests` 追加：

```rust
    #[tokio::test]
    async fn bash_injects_the_session_env_into_the_child() {
        let tmp = TempDir::new().unwrap();
        let ctx = Arc::new(ToolsContext::with_roots_and_env(
            vec![tmp.path().to_path_buf()],
            vec![("YI_AGENT_THREAD_ID".to_string(), "thread-abc".to_string())],
        ));
        let tool = BashTool::new(ctx);
        let result = tool
            .call(serde_json::json!({"command": "echo $YI_AGENT_THREAD_ID"}))
            .await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("thread-abc"), "child must see the injected env: {s}");
        } else {
            panic!("expected text block");
        }
    }

    #[tokio::test]
    async fn bash_without_a_session_env_leaves_the_variable_unset() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool
            .call(serde_json::json!({"command": "echo \"[$YI_AGENT_THREAD_ID]\""}))
            .await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("[]"), "no session env means an empty expansion: {s}");
        } else {
            panic!("expected text block");
        }
    }
```

- [ ] **Step 6: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib bash_`
Expected: 第一个测试 FAIL（子进程看不到变量）。

- [ ] **Step 7: 最小实现（bash 注入）**

`bash.rs` 的 `Command::new(program)` 链上加一行（放在 `.current_dir(&cwd)` 之后）：

```rust
            .envs(self.ctx.session_env().iter().cloned())
```

- [ ] **Step 8: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib`
Expected: PASS（含新增 2 个测试与既有 bash 测试）。

- [ ] **Step 9: 写失败的测试（注册入口带 env）**

在 `lib.rs` 的 `mod tests` 追加：

```rust
    #[tokio::test]
    async fn the_env_carrying_registration_reaches_the_bash_tool() {
        let tmp = tempfile::TempDir::new().unwrap();
        let switch = yi_agent_core::autonomy::YoloSwitch::new(false);
        let controller = SandboxController::new(switch, SandboxMode::WorkspaceWrite, false);
        let mut registry = yi_agent_core::ToolRegistry::new();
        register_builtin_tools_with_controller_and_env(
            &mut registry,
            vec![tmp.path().to_path_buf()],
            controller,
            Vec::new(),
            vec![("YI_AGENT_THREAD_ID".to_string(), "thread-abc".to_string())],
        );
        // 真实调用一次，证明 env 确实随注册入口一路到达 shell 子进程——
        // 只断言「bash 被注册」证明不了本函数承诺的那件事。
        let bash = registry.get("bash").expect("bash is registered");
        let result = bash
            .call(serde_json::json!({ "command": "echo $YI_AGENT_THREAD_ID" }))
            .await;
        assert!(!result.is_error, "bash failed: {:?}", result.content);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("thread-abc"), "env must reach the child: {s}");
        } else {
            panic!("expected text block");
        }
    }
```

（`ToolRegistry::get` 已存在，见 `yi-agent-core/src/tool.rs:159`；`tmp` 用 `TempDir`
而非 `tempfile::tempdir()`，与同模块既有测试一致。用 `#[tokio::test]`——同 crate 的
`bash.rs` 已在用，tokio 的 test macro 可用。）

- [ ] **Step 10: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib the_env_carrying_registration`
Expected: 编译失败（无该函数）。

- [ ] **Step 11: 最小实现（注册入口）**

`lib.rs` 里保持既有 `register_builtin_tools_with_controller` 签名字面不变，内部改为委托：

```rust
pub fn register_builtin_tools_with_controller(
    registry: &mut ToolRegistry,
    roots: Vec<PathBuf>,
    controller: SandboxController,
    sandbox_writable_roots: Vec<PathBuf>,
) {
    register_builtin_tools_with_controller_and_env(
        registry,
        roots,
        controller,
        sandbox_writable_roots,
        Vec::new(),
    );
}

/// 同 [`register_builtin_tools_with_controller`]，但把 `session_env` 交给
/// `ToolsContext`，由 `BashTool` 注入 shell 子进程（如当前会话的 thread id）。
pub fn register_builtin_tools_with_controller_and_env(
    registry: &mut ToolRegistry,
    roots: Vec<PathBuf>,
    controller: SandboxController,
    sandbox_writable_roots: Vec<PathBuf>,
    session_env: Vec<(String, String)>,
) {
    let ctx = Arc::new(ToolsContext::with_roots_and_env(roots, session_env));
    // ...以下与既有实现逐字相同（sandbox 构造、各 tool 注册）...
}
```

把既有函数体的内容整体搬进新函数，只把 `ToolsContext::with_roots(roots)` 换成上面这行。

- [ ] **Step 12: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools`
Expected: PASS（lib + tests/ 全绿，既有 7 个 `register_builtin_tools_with_sandbox` 调用点一行未改）。

- [ ] **Step 13: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-tools/src
git commit -m "feat(tools): inject session env into shell children"
```

---

### Task 5: `yi-agent-runtime` + `yi-agent-app-server` —— 装配期注入 thread id

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs:197-245`（`build_tool_setup_with_controller` 签名与调用 `register_builtin_tools_with_controller` 处；`:187` 委托处；`:753` 附近测试）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:954-958`（调用点）

**Interfaces:**
- Consumes: Task 4 的 `register_builtin_tools_with_controller_and_env`。
- Produces: `build_tool_setup_with_controller(cfg: &RuntimeConfig, naked: bool, roots: &[PathBuf], controller: SandboxController, session_env: Vec<(String, String)>) -> Result<ToolSetup>`（**新增第 5 个参数**）。

- [ ] **Step 1: 写失败的测试**

在 `bootstrap.rs` 的 `mod tests` 里，把既有 `build_tool_setup_with_controller_tracks_the_live_switch` 的调用补一个 `Vec::new()` 参数，并追加：

```rust
    #[test]
    fn build_tool_setup_threads_the_session_env_into_bash() {
        let cfg = sample_config();
        let switch = yi_agent_core::autonomy::YoloSwitch::new(false);
        let controller = yi_agent_tools::SandboxController::new(
            switch,
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        );
        let setup = build_tool_setup_with_controller(
            &cfg,
            false,
            std::slice::from_ref(&cfg.workdir),
            controller,
            vec![("YI_AGENT_THREAD_ID".to_string(), "thread-abc".to_string())],
        )
        .expect("setup");
        // bash 拿到该 env：用一次真实调用验证（沿用本文件既有 `rt.block_on(tool.call(...))` 范式）。
        let bash = setup.tools.get("bash").expect("bash is registered");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(bash.call(serde_json::json!({
            "command": "echo $YI_AGENT_THREAD_ID",
        })));
        assert!(
            !result.is_error,
            "bash through the registry failed: {:?}",
            result.content
        );
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("thread-abc"), "env must reach the child: {s}");
        } else {
            panic!("expected text block");
        }
    }
```

（`sample_config` 来自 `crate::config::sample_config`，同文件既有测试已在用；
`cfg.workdir` 是 `/tmp/test-workdir`，`sample_config` 的沙箱是 `WorkspaceWrite`，
`echo` 读取变量不受写入限制。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib build_tool_setup`
Expected: 编译失败（参数个数不符）。

- [ ] **Step 3: 最小实现**

`build_tool_setup_with_controller` 签名加第 5 个参数 `session_env: Vec<(String, String)>`，内部调用改为：

```rust
    yi_agent_tools::register_builtin_tools_with_controller_and_env(
        &mut registry,
        roots.to_vec(),
        controller.clone(),
        sandbox_writable.clone(),
        session_env,
    );
```

`:187` 的 `build_tool_setup_with_switch` 委托处补 `Vec::new()`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib`
Expected: PASS。

- [ ] **Step 5: 接线 app-server**

`server.rs:954` 的调用改为传入本会话的 thread id：

```rust
    let setup = yi_agent_runtime::bootstrap::build_tool_setup_with_controller(
        cfg,
        false,
        &roots,
        controller.clone(),
        vec![("YI_AGENT_THREAD_ID".to_string(), thread_id.to_string())],
    )
    .map_err(|error| error.to_string())?;
```

（`thread_id` 是本函数已有的形参，无需新增来源；与下方 `ShowGitDiffTool`/`AskLeadTool`
的注入同源。）

- [ ] **Step 6: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: PASS。

- [ ] **Step 7: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): inject this thread's id into bash children"
```

---

### Task 6: 桌面端 —— 数据层读取与面板投递带会话

**Files:**
- Modify: `desktop/src/lib/superpowersKanbanState.ts`（`BoardCard` 接口、`normalizeCard`）
- Modify: `desktop/src/lib/superpowersKanbanSwitch.ts`（`BoardCardDto`、`enqueuePrequeuedCard`）
- Modify: `desktop/src/App.tsx:2108-2110`（`prequeue` 投递点）
- Modify: `desktop/src/components/SuperpowersKanbanCard.test.tsx:8-10`、`desktop/src/components/SuperpowersKanbanBoard.test.tsx:9-11`、`desktop/src/components/SuperpowersKanbanView.test.tsx:9-11`、`desktop/src/lib/superpowersKanbanBoard.test.ts:10-12`（既有 `card()` helper 补必填字段）
- Test: `desktop/src/lib/superpowersKanbanState.test.ts`、`desktop/src/lib/superpowersKanbanSwitch.test.ts`

**Interfaces:**
- Consumes: Task 3 的 `list` 输出键 `origin_thread_id`；Task 3 的 IPC 可选参数 `origin_thread_id`。
- Produces:
  - `BoardCard.originThreadId: string | null`；
  - `enqueuePrequeuedCard(rpc: BoardRpc, project: string, title: string, originThreadId: string | null): Promise<void>`（**新增第 4 个参数**）。

- [ ] **Step 1: 写失败的测试（normalizeCard）**

在 `superpowersKanbanState.test.ts` 的 `describe("normalizeCard")` 内追加：

```typescript
  it("reads origin_thread_id as originThreadId", () => {
    const card = normalizeCard({
      id: "idea",
      state: "pre_queued",
      origin_thread_id: "thread-abc",
    });
    expect(card?.originThreadId).toBe("thread-abc");
  });

  it("defaults a missing origin_thread_id to null", () => {
    const card = normalizeCard({ id: "idea", state: "pre_queued" });
    expect(card?.originThreadId).toBeNull();
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/superpowersKanbanState.test.ts`
Expected: FAIL（`originThreadId` 为 `undefined`）。

- [ ] **Step 3: 最小实现（数据层）**

`superpowersKanbanState.ts` 的 `BoardCard` 接口，在 `threadId: string | null;` 之后加：

```typescript
  /** 提出这张卡的讨论会话（pre-queued 卡）。插件未提供则为 null。 */
  originThreadId: string | null;
```

`normalizeCard` 里，`threadId` 解出后加同款归一化：

```typescript
  const originThreadId =
    typeof record.origin_thread_id === "string" && record.origin_thread_id !== ""
      ? record.origin_thread_id
      : null;
```

并在返回对象里 `threadId,` 之后加 `originThreadId,`。

- [ ] **Step 3b: 修既有测试 helper（必做，否则 tsc 挂）**

`originThreadId` 是必填字段，四个既有文件的 `card()` helper 必须补上，否则
`cd desktop && npm run build`（`tsc`）与 vitest 都编译失败：

- `desktop/src/components/SuperpowersKanbanCard.test.tsx:9-11`：
  `return { id: "c1", state: "needs_you", progress: null, detail: "", threadId: null, originThreadId: null, ...extra };`
- `desktop/src/components/SuperpowersKanbanBoard.test.tsx:9-11`：
  `return { id, state, progress: null, detail: "", threadId: null, originThreadId: null, title: id, ...extra };`
- `desktop/src/components/SuperpowersKanbanView.test.tsx:9-11`：同上。
- `desktop/src/lib/superpowersKanbanBoard.test.ts:10-12`：同上。

用 `cd desktop && npx tsc --noEmit` 逐个定位漏改处。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx tsc --noEmit && npx vitest run src/lib/superpowersKanbanState.test.ts`
Expected: PASS。

- [ ] **Step 5: 写失败的测试（投递带会话）**

在 `superpowersKanbanSwitch.test.ts` 追加：

```typescript
  it("enqueuePrequeuedCard sends the discussion session when given", async () => {
    const inner = vi.fn(async () => ({ id: "prequeued-1" }));
    await enqueuePrequeuedCard(asRpc(inner), "/proj", "先记个标题", "thread-abc");
    expect(inner).toHaveBeenCalledWith("plugin/query", {
      plugin: "superpowers-kanban",
      project: "/proj",
      method: "enqueue_prequeued",
      params: { title: "先记个标题", origin_thread_id: "thread-abc" },
    });
  });

  it("enqueuePrequeuedCard omits the session when there is none", async () => {
    const inner = vi.fn(async () => ({ id: "prequeued-1" }));
    await enqueuePrequeuedCard(asRpc(inner), "/proj", "先记个标题", null);
    expect(inner).toHaveBeenCalledWith("plugin/query", {
      plugin: "superpowers-kanban",
      project: "/proj",
      method: "enqueue_prequeued",
      params: { title: "先记个标题" },
    });
  });
```

- [ ] **Step 6: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/superpowersKanbanSwitch.test.ts`
Expected: FAIL（参数未生效）。

- [ ] **Step 7: 最小实现（switch）与接线**

`superpowersKanbanSwitch.ts` 的 `BoardCardDto` 加：

```typescript
  /** 该卡关联的讨论会话（pre-queued 卡）。 */
  origin_thread_id?: string | null;
```

`enqueuePrequeuedCard` 改为：

```typescript
export async function enqueuePrequeuedCard(
  rpc: BoardRpc,
  project: string,
  title: string,
  originThreadId: string | null,
): Promise<void> {
  // 与 UI 的处理同口径：空白标题就地拒绝，不发这趟 RPC——插件的
  // `enqueue_prequeued` 本来也会拒，调用方不该指望 UI 先 trim 好。
  const trimmed = title.trim();
  if (trimmed === "") throw new Error("a prequeued card needs a non-empty title");
  const params: Record<string, unknown> = { title: trimmed };
  // 来源会话是尽力而为：没有（尚未开会话 / 会话外）就不带这个键。
  if (originThreadId !== null && originThreadId.trim() !== "") {
    params.origin_thread_id = originThreadId;
  }
  await queryPlugin(rpc, project, "enqueue_prequeued", params);
}
```

`App.tsx:2108-2110` 的投递点改为把当前会话传进去：

```tsx
                prequeue={(title) =>
                  enqueuePrequeuedCard(
                    board.boardRpc,
                    board.selectedBoard!,
                    title,
                    sess.currentId,
                  )
                }
```

并同步修既有调用点（若有其它 `enqueuePrequeuedCard(...)` 调用，补第 4 个参数）。

- [ ] **Step 8: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/superpowersKanbanSwitch.test.ts src/lib/superpowersKanbanState.test.ts`
Expected: PASS。

- [ ] **Step 9: 提交**

```bash
cd desktop && npx prettier --write src/lib/superpowersKanbanState.ts src/lib/superpowersKanbanSwitch.ts src/App.tsx
git add desktop/src/lib/superpowersKanbanState.ts desktop/src/lib/superpowersKanbanSwitch.ts desktop/src/App.tsx desktop/src/lib/superpowersKanbanState.test.ts desktop/src/lib/superpowersKanbanSwitch.test.ts
git commit -m "feat(desktop): carry the discussion session when pre-queuing"
```

---

### Task 7: 桌面端 —— 卡片按钮回退到讨论会话

**Files:**
- Modify: `desktop/src/components/SuperpowersKanbanCard.tsx:36-45`
- Test: `desktop/src/components/SuperpowersKanbanCard.test.tsx`

**Interfaces:**
- Consumes: Task 6 的 `BoardCard.originThreadId`。
- Produces: 无（叶子节点）。

- [ ] **Step 1: 写失败的测试**

在 `SuperpowersKanbanCard.test.tsx` 追加（该文件已有 `card(extra: Partial<BoardCard>)`
helper 与 `render`/`cleanup`/`screen`；本步新增的 `fireEvent` 需并入文件顶部
`import { render, cleanup, screen, fireEvent } from "@testing-library/react";`）：

```typescript
  it("shows the discussion session while the card is only pre-queued", () => {
    const onOpenThread = vi.fn();
    render(
      <SuperpowersKanbanCard
        card={card({ id: "idea", state: "pre_queued", originThreadId: "thread-abc", threadId: null })}
        onOpenThread={onOpenThread}
      />,
    );
    const button = screen.getByRole("button", { name: "thread-abc" });
    fireEvent.click(button);
    expect(onOpenThread).toHaveBeenCalledWith("thread-abc");
  });

  it("prefers the execution session once the card is running", () => {
    render(
      <SuperpowersKanbanCard
        card={card({ id: "c1", state: "running", threadId: "thread-exec", originThreadId: "thread-abc" })}
      />,
    );
    expect(screen.getByRole("button", { name: "thread-exec" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "thread-abc" })).toBeNull();
  });

  it("renders no session button when neither session is known", () => {
    render(
      <SuperpowersKanbanCard
        card={card({ id: "idea", state: "pre_queued", threadId: null, originThreadId: null })}
      />,
    );
    expect(screen.queryByRole("button", { name: /thread-/ })).toBeNull();
  });
```

本文件顶部需 `import { vi } from "vitest";`（若既有 import 行未含 `vi`，补上）。
`card()` helper 的 `originThreadId: null` 由 Task 6 Step 3b 已补。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/SuperpowersKanbanCard.test.tsx`
Expected: 第一个测试 FAIL（pre-queued 卡没有按钮）。

- [ ] **Step 3: 最小实现**

在 `SuperpowersKanbanCard.tsx` 组件函数体开头（`const needsYou = ...` 附近）加：

```tsx
  // 一张卡同一时刻只显示一个会话链接：pre-queued/queued 阶段 `threadId` 为空，
  // 显示提出它的**讨论会话**；卡跑起来后 `threadId` 有值，自动切到**执行会话**。
  const sessionId = card.threadId ?? card.originThreadId;
```

把既有

```tsx
      {card.threadId ? (
        <button
          type="button"
          onClick={() => onOpenThread?.(card.threadId as string)}
          className="mt-1 block max-w-full cursor-pointer truncate text-left font-mono text-xs text-fg-muted hover:text-fg hover:underline"
        >
          {card.threadId}
        </button>
      ) : null}
```

整块替换为：

```tsx
      {sessionId ? (
        <button
          type="button"
          onClick={() => onOpenThread?.(sessionId)}
          className="mt-1 block max-w-full cursor-pointer truncate text-left font-mono text-xs text-fg-muted hover:text-fg hover:underline"
        >
          {sessionId}
        </button>
      ) : null}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/SuperpowersKanbanCard.test.tsx`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd desktop && npx prettier --write src/components/SuperpowersKanbanCard.tsx
git add desktop/src/components/SuperpowersKanbanCard.tsx desktop/src/components/SuperpowersKanbanCard.test.tsx
git commit -m "feat(desktop): fall back to the discussion session on pre-queued cards"
```

---

### Task 8: 文档与项目状态同步

**Files:**
- Modify: `plugins/superpowers-kanban/README.md`（预排队一节）
- Modify: `plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md`（预排队一节）
- Modify: `docs/project-management/desktop.md`、`docs/project-management/yi-agent-app-server.md`、`docs/project-management/yi-agent-tools.md`
- Modify: `docs/project-management/README.md`（若计数有变）

**Interfaces:**
- Consumes: Task 1-7 的全部行为。
- Produces: 无。

- [ ] **Step 1: 改插件 README**

在预排队（pre-queued）一节补一段（与既有行文风格一致，中文）：

```markdown
预排队卡会记下**提出它的讨论会话**：在会话里经 bash 调 `superpowers-kanban
add-prequeued "<标题>"` 时，宿主已把当前会话 id 注入环境变量 `YI_AGENT_THREAD_ID`，
插件读它并随卡保存；桌面端看板面板则直接带上当前会话。桌面卡片上的会话链接在卡片
被提升/开跑前指向那次讨论，跑起来后自动切到执行会话。拿不到会话（会话外手动调用、
TUI 本地会话）时该字段为空，**不阻断**投递。
```

- [ ] **Step 2: 改 skill**

在 `SKILL.md` 的预排队（`### 2a`）一节补一句：

```markdown
在会话里投递时，插件会自动把**当前会话**记为这张卡的来源（宿主经
`YI_AGENT_THREAD_ID` 注入），无需额外参数；桌面看板上可点击跳回那次讨论。
```

- [ ] **Step 3: 改项目状态文件**

按 `docs/project-management/README.md` 末尾的「维护规则」更新三处：`desktop.md`
（卡片字段 `originThreadId` 与按钮回退）、`yi-agent-app-server.md`（装配期向 bash 子
进程注入 `YI_AGENT_THREAD_ID`）、`yi-agent-tools.md`（`ToolsContext` 携带 session env、
`BashTool` 注入）。每条带可验证判据（`file.rs:line` 或可执行命令），状态只用
`[x]`/`[ ]`/`[-]`。若计数变化，同步 `README.md` 的模块索引表。

- [ ] **Step 4: 提交**

```bash
git add plugins/superpowers-kanban/README.md plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md docs/project-management
git commit -m "docs(kanban): document the origin session of pre-queued cards"
```

---

## 验收（全部任务完成后）

- [ ] `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core -p superpowers-kanban-runner` 全绿。
- [ ] `cd yi-agent-rs && cargo test -p yi-agent-tools -p yi-agent-runtime -p yi-agent-app-server` 全绿。
- [ ] `cd desktop && npm test` 全绿。
- [ ] `cd yi-agent-rs && cargo fmt --all --check` 与插件侧 `cargo fmt --all --check` 通过。
- [ ] 手工：会话内 `superpowers-kanban add-prequeued "示例需求"` → 桌面 PRE-QUEUED 卡出现讨论会话链接 → 点击跳回 → `promote` 后链接仍在 → 卡开跑后链接切为执行会话。
- [ ] 回归：旧 `board.json`（无 `origin_thread_id`）仍可读；会话外 `add-prequeued` 仍成功且字段为空。
