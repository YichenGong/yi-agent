# 进行中 Turn 的 Checkpoint 落盘 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让一轮进行中的 turn 在 SIGKILL / 崩溃 / 断电后仍能恢复出"已经完成的部分"（用户提问 + 已 finalize 的助手消息块与工具调用），而不是整轮丢失。

**Architecture:** 每个 thread 增加一个可变的 checkpoint 文件 `<id>.partial.json`，由 driver 在 turn 内增量重写（turn 开始写提问、每次 item finalize 标脏并按 ~500ms 去抖重写、turn 收尾落主 jsonl 后删除）。`ThreadStore::load` 读主 jsonl 后，用"partial 首个 item id 是否已出现在 jsonl"判断该轮是否已收尾：未收尾则拼接 partial 的 items 并采纳其 messages/usage，同时置 `pending_turn`，由 `thread/resume` 补发一条 `turn/completed{Interrupted}`。主 `.jsonl` / `.meta.json` 格式不变。

**Tech Stack:** Rust（`yi-agent-app-server` crate），tokio async，serde_json，现有 `ThreadStore` / `Translator` / `run_thread_driver` / `serve`。

## Global Constraints

- 不修改主 `.jsonl` 的 `TurnLine::Turn` 结构与 `<id>.meta.json` 格式（向后兼容旧日志）。
- checkpoint 的写失败一律**不阻断** turn（`eprintln!` 记 stderr，与现有 `failed to persist turn` 同策略）。
- checkpoint 解析失败一律**忽略**（记 stderr），退回"只有 jsonl"的行为，绝不因此让 resume 报错。
- 落盘只含**已 finalize** 的 item（用户提问、`AgentMessage`、`ToolCall`）；进行中的流式文本与 running 工具**不落盘**。
- 原子写复用现有 `crate::thread_store::write_atomic`（temp + rename）。所有新文件路径都通过 `ThreadStore` 的私有 `root` 构造，禁止硬编码路径。
- 测试不得触碰真实 `~`/项目目录：用 `tempfile::TempDir` + `Harness::with_config`（已隔离 workdir/索引/看板/配对）。
- 所有 Rust 命令在 `yi-agent-rs/` 下运行。

## File Structure

- `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs` — 新增 `PartialTurn` 类型、`partial_path` / `write_partial` / `read_partial` / `clear_partial`、`load` 合流、`delete`/`truncate` 清理；`LoadedThread` 加 `pending_turn`。
- `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` — driver 内写 checkpoint（turn 开始 + finalize 标脏 + 去抖 timer + 收尾删除）；`thread/resume` 回放后按 `pending_turn` 补发 interrupted 标记；`interrupt_and_wait_for_persist` 无需改动。
- 测试分别就近放在两个文件的 `#[cfg(test)] mod tests` 中。

---

### Task 1: `ThreadStore` 支持 partial checkpoint 的读写与 load 合流

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`（`LoadedThread` ~72-77；`load` ~125-180；新增 partial 方法约在 `delete` 之后 ~333）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs` 的 `mod tests`

**Interfaces:**
- Consumes: 现有 `ThreadStore::new(workdir: &Path)`、`log_path`、`meta_path`、`write_atomic(path, bytes)`、`TurnLine::Turn{items, usage, messages}`、`Item`（`crate::protocol::Item`）、`Message`（`yi_agent_core::Message`）。
- Produces:
  - `pub struct PartialTurn { pub turn_id: String, pub items: Vec<Item>, pub messages: Vec<Message>, pub usage: Option<TurnUsage> }`（`Serialize + Deserialize + Clone + Default`）
  - `pub fn write_partial(&self, id: &str, turn: &PartialTurn) -> io::Result<()>`
  - `pub fn clear_partial(&self, id: &str) -> io::Result<()>`
  - `LoadedThread.pending_turn: bool`（新增字段）

- [ ] **Step 1: 写失败测试（load 无 partial 时行为不变 + partial 合并）**

在 `thread_store.rs` 的 `mod tests` 内新增：

```rust
fn sample_meta(id: &str) -> ThreadMeta {
    ThreadMeta {
        thread_id: id.to_string(),
        cwd: "/tmp".into(),
        model: "m".into(),
        created_at: 1,
        updated_at: 1,
        title: None,
        permission_mode: ThreadMode::Normal,
        pin_seq: None,
    }
}

fn user_item(id: &str, text: &str) -> crate::protocol::Item {
    crate::protocol::Item::UserMessage { id: id.into(), text: text.into() }
}

#[test]
fn load_without_partial_is_unchanged() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = ThreadStore::new(dir.path());
    store.create(&sample_meta("thread-a")).unwrap();
    store
        .append_turn(
            "thread-a",
            &TurnLine::Turn {
                items: vec![user_item("user-t1", "hi")],
                usage: None,
                messages: vec![],
            },
        )
        .unwrap();

    let loaded = store.load("thread-a").unwrap().unwrap();
    assert_eq!(loaded.items.len(), 1);
    assert!(!loaded.pending_turn, "no partial file ⇒ nothing pending");
}

#[test]
fn load_merges_an_uncommitted_partial_turn() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = ThreadStore::new(dir.path());
    store.create(&sample_meta("thread-a")).unwrap();
    store
        .append_turn(
            "thread-a",
            &TurnLine::Turn {
                items: vec![user_item("user-t1", "first")],
                usage: None,
                messages: vec![],
            },
        )
        .unwrap();
    // 崩溃残留：下一轮只写了 partial，主 jsonl 里没有它的首个 item。
    store
        .write_partial(
            "thread-a",
            &PartialTurn {
                turn_id: "turn-t2".into(),
                items: vec![
                    user_item("user-turn-t2", "second"),
                    crate::protocol::Item::AgentMessage {
                        id: "item-turn-t2-1".into(),
                        text: "partial answer".into(),
                    },
                ],
                messages: vec![],
                usage: None,
            },
        )
        .unwrap();

    let loaded = store.load("thread-a").unwrap().unwrap();
    let ids: Vec<String> = loaded
        .items
        .iter()
        .map(|i| crate::server::item_id(i).unwrap().to_string())
        .collect();
    assert_eq!(ids, vec!["user-t1", "user-turn-t2", "item-turn-t2-1"]);
    assert!(loaded.pending_turn, "uncommitted partial ⇒ pending_turn");
}

#[test]
fn load_ignores_a_partial_whose_turn_is_already_committed() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = ThreadStore::new(dir.path());
    store.create(&sample_meta("thread-a")).unwrap();
    // 收尾成功（提问已进 jsonl），但 delete partial 失败。
    store
        .append_turn(
            "thread-a",
            &TurnLine::Turn {
                items: vec![user_item("user-turn-t2", "second")],
                usage: None,
                messages: vec![],
            },
        )
        .unwrap();
    store
        .write_partial(
            "thread-a",
            &PartialTurn {
                turn_id: "turn-t2".into(),
                items: vec![user_item("user-turn-t2", "second")],
                messages: vec![],
                usage: None,
            },
        )
        .unwrap();

    let loaded = store.load("thread-a").unwrap().unwrap();
    assert_eq!(loaded.items.len(), 1, "must not duplicate the committed turn");
    assert!(!loaded.pending_turn);
}

#[test]
fn load_ignores_a_corrupt_partial() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = ThreadStore::new(dir.path());
    store.create(&sample_meta("thread-a")).unwrap();
    store
        .append_turn(
            "thread-a",
            &TurnLine::Turn { items: vec![user_item("user-t1", "hi")], usage: None, messages: vec![] },
        )
        .unwrap();
    let threads = dir.path().join(".yi-agent/threads");
    std::fs::write(threads.join("thread-a.partial.json"), b"{ not json").unwrap();

    let loaded = store.load("thread-a").unwrap().unwrap();
    assert_eq!(loaded.items.len(), 1, "corrupt partial must be ignored");
    assert!(!loaded.pending_turn);
}

#[test]
fn clear_partial_removes_the_file() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = ThreadStore::new(dir.path());
    store.create(&sample_meta("thread-a")).unwrap();
    store
        .write_partial("thread-a", &PartialTurn::default())
        .unwrap();
    store.clear_partial("thread-a").unwrap();
    assert!(!dir
        .path()
        .join(".yi-agent/threads/thread-a.partial.json")
        .exists());
    // 幂等：再删一次仍成功。
    store.clear_partial("thread-a").unwrap();
}
```

> 注：`crate::server::item_id` 目前是 server.rs 的私有 `fn`。Step 3 顺带把它提升为
> `pub(crate) fn item_id`（一行签名改动），否则本测试编译不过。

- [ ] **Step 2: 运行测试，确认失败**

Run: `cargo test -p yi-agent-app-server --lib thread_store::tests::load_merges_an_uncommitted_partial_turn thread_store::tests::load_without_partial_is_unchanged`
Expected: 编译失败（`PartialTurn` / `write_partial` / `clear_partial` / `pending_turn` 未定义）。

- [ ] **Step 3: 实现 PartialTurn 与 partial 读写、load 合流**

在 `thread_store.rs` 顶部 `use` 后、`LoadedThread` 附近新增：

```rust
/// 进行中 turn 的 checkpoint：崩溃后据此恢复"已完成的部分"。
///
/// 写时机见 `server.rs` 的 driver：turn 开始写一次，之后每次 item finalize
/// 重写，turn 收尾（append 主 jsonl）后删除。`items` 只含已 finalize 的 item
/// （含本轮的 `UserMessage` 提问），进行中的流式文本不入内。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PartialTurn {
    pub turn_id: String,
    pub items: Vec<Item>,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TurnUsage>,
}
```

`LoadedThread` 增加字段：

```rust
pub struct LoadedThread {
    pub meta: ThreadMeta,
    pub items: Vec<Item>,
    pub messages: Vec<Message>,
    pub usage: Option<TurnUsage>,
    /// 末尾是否是一段崩溃残留的、未收尾的 turn（来自 `.partial.json`）。
    /// `true` 时调用方（`thread/resume`）应在回放后补发 interrupted 标记。
    pub pending_turn: bool,
}
```

`impl ThreadStore` 内新增：

```rust
fn partial_path(&self, id: &str) -> PathBuf {
    self.root.join(format!("{id}.partial.json"))
}

/// 整体原子重写该 thread 的 checkpoint。`items` 已含本轮提问。
pub fn write_partial(&self, id: &str, turn: &PartialTurn) -> io::Result<()> {
    if !valid_id(id) {
        return Err(invalid_id(id));
    }
    std::fs::create_dir_all(&self.root)?;
    let bytes = serde_json::to_vec(turn).map_err(io_err)?;
    write_atomic(&self.partial_path(id), &bytes)
}

/// 删除 checkpoint；不存在则幂等成功。
pub fn clear_partial(&self, id: &str) -> io::Result<()> {
    match std::fs::remove_file(self.partial_path(id)) {
        Ok(()) => Ok(()),
        Err(ref e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// 读 checkpoint；缺失或损坏返回 `None`（损坏记 stderr，绝不向上报错）。
fn read_partial(&self, id: &str) -> Option<PartialTurn> {
    let text = std::fs::read_to_string(self.partial_path(id)).ok()?;
    match serde_json::from_str::<PartialTurn>(&text) {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("[app-server] ignoring corrupt partial turn ({id}): {e}");
            None
        }
    }
}
```

`load` 在构造 `LoadedThread` 之前插入合流逻辑（即把现有

```rust
        Ok(Some(LoadedThread {
            meta,
            items,
            messages,
            usage,
        }))
```

改为）：

```rust
        // 合流 checkpoint：仅当 partial 的首个 item id 尚未出现在主 jsonl
        // （= 该轮未成功收尾）时采纳。首个 item 是本轮提问 `user-<turn_id>`，
        // 全局唯一，足以判断"这轮是否已 append"。
        let mut pending_turn = false;
        if let Some(partial) = self.read_partial(id) {
            let committed = partial
                .items
                .first()
                .and_then(|it| item_id_of(it))
                .is_some_and(|first| items.iter().any(|it| item_id_of(it) == Some(first)));
            if !committed {
                for item in partial.items {
                    // 防御性去重：正常不与已落盘 items 重叠。
                    if !items.iter().any(|it| item_id_of(it) == item_id_of(&item)) {
                        items.push(item);
                    }
                }
                if !partial.messages.is_empty() {
                    messages = partial.messages;
                }
                if partial.usage.is_some() {
                    usage = partial.usage;
                }
                pending_turn = true;
            }
        }

        Ok(Some(LoadedThread {
            meta,
            items,
            messages,
            usage,
            pending_turn,
        }))
```

在 `thread_store.rs` 内新增一个本地取值小函数（避免依赖 server 的私有 fn）：

```rust
/// 一条 `Item` 的稳定 id（镜像 `server::item_id`；本模块自用）。
fn item_id_of(item: &Item) -> Option<&str> {
    match item {
        Item::UserMessage { id, .. }
        | Item::AgentMessage { id, .. }
        | Item::ToolCall { id, .. }
        | Item::UserInterjection { id, .. } => Some(id),
    }
}
```

同时把 `server.rs:4890` 的 `fn item_id` 改签名为 `pub(crate) fn item_id`（Task 1 测试需要）。

- [ ] **Step 4: 运行测试，确认通过**

Run: `cargo test -p yi-agent-app-server --lib thread_store::tests`
Expected: PASS（含既有测试）。

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): merge a crashed turn's partial checkpoint on load"
```

---

### Task 2: `delete` / `truncate` 清理 checkpoint

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`（`delete` ~310-333；`truncate` ~336-356）
- Test: `thread_store.rs` 的 `mod tests`

**Interfaces:**
- Consumes: Task 1 的 `clear_partial`。
- Produces: 无新接口；`delete`/`truncate` 后 `partial` 文件不存在。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn delete_removes_the_partial_checkpoint() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = ThreadStore::new(dir.path());
    store.create(&sample_meta("thread-a")).unwrap();
    store.write_partial("thread-a", &PartialTurn::default()).unwrap();
    assert!(store.delete("thread-a").unwrap());
    assert!(!dir.path().join(".yi-agent/threads/thread-a.partial.json").exists());
}

#[test]
fn truncate_removes_the_partial_checkpoint() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = ThreadStore::new(dir.path());
    store.create(&sample_meta("thread-a")).unwrap();
    store
        .append_turn(
            "thread-a",
            &TurnLine::Turn { items: vec![user_item("user-t1", "hi")], usage: None, messages: vec![] },
        )
        .unwrap();
    store.write_partial("thread-a", &PartialTurn::default()).unwrap();
    store.truncate("thread-a").unwrap();
    assert!(!dir.path().join(".yi-agent/threads/thread-a.partial.json").exists());
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p yi-agent-app-server --lib thread_store::tests::delete_removes_the_partial_checkpoint thread_store::tests::truncate_removes_the_partial_checkpoint`
Expected: FAIL（partial 文件仍存在）。

- [ ] **Step 3: 实现清理**

`delete` 内把

```rust
        let meta = self.meta_path(id);
        let log = self.log_path(id);
        let existed = meta.exists() || log.exists();
        for p in [meta, log] {
```

改为：

```rust
        let meta = self.meta_path(id);
        let log = self.log_path(id);
        let partial = self.partial_path(id);
        let existed = meta.exists() || log.exists() || partial.exists();
        for p in [meta, log, partial] {
```

`truncate` 内，在 `Ok(existed)` 之前加一行（清掉可能残留的 checkpoint，`/clear` 之后不得被 load 复活）：

```rust
        // `/clear` 语义：连进行中的 checkpoint 一起丢弃，否则重启后它会
        // 把已清空的上下文又拼回来。
        if let Err(e) = self.clear_partial(id) {
            eprintln!("[app-server] failed to clear partial turn ({id}): {e}");
        }
        Ok(existed)
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p yi-agent-app-server --lib thread_store::tests`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs
git commit -m "feat(app-server): drop the checkpoint on delete and truncate"
```

---

### Task 3: driver 在 turn 内写 checkpoint

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（driver 内：turn 循环体起始 ~4536-4541；finalize 检测 ~4673-4676；收尾 `persist_and_finish_turn` 之后 ~4837-4850；新增 helper 函数约在 `persist_and_finish_turn` ~4304 之前）
- Test: `server.rs` 的 `mod tests`

**Interfaces:**
- Consumes: Task 1 的 `ThreadStore::write_partial` / `clear_partial` / `PartialTurn`；driver 已有的 `store: Arc<ThreadStore>`、`turn_id: String`、`prompt: String`、`agent: Agent`、`completed_items: Vec<Item>`、`translator`。`opening_user_item(turn_id, text) -> Item` 已在同一文件（`server.rs:5405` 附近）。
- Produces: driver 在盘上留下 `<id>.partial.json`（turn 进行中）或删除它（收尾后）。

- [ ] **Step 1: 写失败测试**

在 server.rs 的 `mod tests` 内（与 `build_slow_agent` 同层）新增一个 provider：先发一段
`TextDelta`，再发一个工具调用（`ToolUseStart`/`ToolUseEnd`）——translator 在收到 `ToolCall`
前会 `finalize_agent_msg`，把累积的文本转成一个 `AgentMessage` 的 `ItemCompleted`；随后挂一段
**永不产出**的尾部，让 turn 一直不收尾，从而停在"已 finalize 一块文本、但还没 persist"的窗口：

```rust
    /// 先产出一段助手文本（被 translator finalize 成 item），随后阻塞不收尾——
    /// 模拟"turn 进行中、checkpoint 应已写、但还没到 persist"的窗口。
    struct CheckpointProvider;

    #[async_trait]
    impl yi_agent_core::Provider for CheckpointProvider {
        async fn call_stream(
            &self,
            _req: yi_agent_core::provider::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
            yi_agent_core::provider::ProviderError,
        > {
            use yi_agent_core::provider::ProviderEvent as E;
            let head = futures::stream::iter(vec![
                E::TextDelta("a".into()),
                E::ToolUseStart { id: "t1".into(), name: "noop".into() },
                E::ToolUseEnd { id: "t1".into() },
            ]);
            // 永不产出的尾部：保证 turn 不收尾（否则会被正常 persist）。
            let tail = futures::stream::unfold((), |_| async {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                Option::<E>::None
            });
            Ok(head.chain(tail).boxed())
        }
    }
```

> `chain` 来自 `futures::StreamExt`（server.rs 顶部已 `use futures::StreamExt;`，测试模块内可直接用）。
> 工具名 `noop` 在空 `ToolRegistry` 里不存在，core 会把它当作 `tool not found` 处理并回一个错误结果——
> 这不影响本测试：我们只等待**第一个** `item/completed`（即助手文本块）出现。

测试本体：

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn an_in_flight_turn_leaves_a_checkpoint() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_checkpoint_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"do work"}}]}}}}"#
        ))
        .await;

        // 等到 agent 的 item/completed（助手文本块）出现 —— 说明已 finalize 过。
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("item/completed") {
                break;
            }
        }

        let partial = dir
            .path()
            .join(".yi-agent/threads")
            .join(format!("{tid}.partial.json"));
        // checkpoint 是去抖写的，轮询等待。
        let mut text = String::new();
        for _ in 0..100 {
            match std::fs::read_to_string(&partial) {
                Ok(t) if t.contains("do work") => { text = t; break; }
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        assert!(text.contains("do work"), "checkpoint must carry the prompt: {text:?}");
        assert!(text.contains("user-"), "checkpoint must carry the opening user item: {text:?}");

        h.shutdown().await;
    }
```

并新增工厂（放在 `build_slow_agent` 旁）：

```rust
    fn build_checkpoint_agent(
        session: Option<yi_agent_core::Session>,
        _cwd: &std::path::Path,
        _mode: crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent> {
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(CheckpointProvider);
        let config = yi_agent_core::AgentConfig::default();
        let mut agent = yi_agent_core::Agent::new(
            provider.clone(),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            config.clone(),
        );
        apply_session(&mut agent, session);
        Ok(BuiltAgent {
            agent,
            provider,
            config,
            decision_tx: None,
            decision_rx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        })
    }
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p yi-agent-app-server --lib an_in_flight_turn_leaves_a_checkpoint`
Expected: FAIL（partial 文件不存在）。

- [ ] **Step 3: 实现 driver 侧 checkpoint 写入**

在 `server.rs` 新增两个小函数（放在 `persist_and_finish_turn` 之前）：

```rust
/// 组装当前 turn 的 checkpoint：已 finalize 的 item（含开头的用户提问）+ 当前
/// 上下文快照。进行中的流式文本不在 `completed_items` 里，故天然不入内。
fn build_partial(
    turn_id: &str,
    user_prompt: &str,
    completed_items: &[crate::protocol::Item],
    messages: Vec<Message>,
    usage: Option<crate::thread_store::TurnUsage>,
) -> crate::thread_store::PartialTurn {
    let mut items = Vec::with_capacity(completed_items.len() + 1);
    items.push(opening_user_item(turn_id, user_prompt));
    items.extend(completed_items.iter().cloned());
    crate::thread_store::PartialTurn {
        turn_id: turn_id.to_string(),
        items,
        messages,
        usage,
    }
}

/// 尽力写一次 checkpoint：失败只记 stderr，绝不打断 turn。
fn checkpoint(
    store: &crate::thread_store::ThreadStore,
    thread_id: &str,
    partial: crate::thread_store::PartialTurn,
) {
    if let Err(e) = store.write_partial(thread_id, &partial) {
        eprintln!("[app-server] failed to write turn checkpoint ({thread_id}): {e}");
    }
}
```

在 driver 的 turn 循环体内，**取得 `turn_id`/`prompt` 之后、`agent.run(prompt)` 之前**
（即现有 `let mut completed_items ... let user_prompt = prompt.clone();` 一带，~4536）
加"turn 开始即写一次、只含提问"：

```rust
        // turn 开始就把提问落进 checkpoint：即使这一轮随后立刻崩溃，
        // 至少提问不会丢。
        checkpoint(
            &store,
            &thread_id,
            build_partial(&turn_id, &user_prompt, &[], agent.session().messages().to_vec(), None),
        );
```

> 若 `user_prompt` 在那里还没定义，则把该调用移到 `let user_prompt = prompt.clone();` 之后一行。

在 finalize 检测处（现有 `if let Notification::ItemCompleted { item, .. } = &n { completed_items.push(item.clone()); }`，~4674）之后紧接标脏：

```rust
                                if let crate::protocol::Notification::ItemCompleted { item, .. } = &n {
                                    completed_items.push(item.clone());
                                    checkpoint_dirty = true;
                                }
```

并在内层 `loop` 的 `tokio::select!` 中新增一个去抖 tick 分支（与既有 `delta_tick` 并列）：

```rust
                _ = checkpoint_tick.tick(), if checkpoint_dirty => {
                    checkpoint(
                        &store,
                        &thread_id,
                        build_partial(
                            &turn_id,
                            &user_prompt,
                            &completed_items,
                            agent.session().messages().to_vec(),
                            last_usage.clone(),
                        ),
                    );
                    checkpoint_dirty = false;
                }
```

> `checkpoint_tick` 与 `checkpoint_dirty` 在进入这个 `loop` 之前初始化：
> `let mut checkpoint_tick = tokio::time::interval(Duration::from_millis(500));`
> `checkpoint_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);`
> `let mut checkpoint_dirty = false;`（放在与 `delta_tick`/`coalescer` 同一处，~4562）。
> `turn_id`/`user_prompt`/`store` 在循环内均可见；`last_usage` 若被 `take()` 走，
> 用 `clone()` 传值。

在收尾处（常规 `persist_and_finish_turn(...)` 调用之后，~4850）清理 checkpoint：

```rust
        // 收尾已把整轮 append 进主 jsonl；checkpoint 的使命结束。
        if let Err(e) = store.clear_partial(&thread_id) {
            eprintln!("[app-server] failed to clear turn checkpoint ({thread_id}): {e}");
        }
```

> 同样在 clear/compact 的命令路径（`pending_session_command` 分支结尾）追加一次
> `let _ = store.clear_partial(&thread_id);`，因为那条路径不走常规收尾。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p yi-agent-app-server --lib an_in_flight_turn_leaves_a_checkpoint`
Expected: PASS。

- [ ] **Step 5: 回归——正常收尾后 checkpoint 消失**

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn a_completed_turn_removes_its_checkpoint() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
                break;
            }
        }
        let partial = dir
            .path()
            .join(".yi-agent/threads")
            .join(format!("{tid}.partial.json"));
        for _ in 0..100 {
            if !partial.exists() { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!partial.exists(), "a finished turn must not leave a checkpoint");
        h.shutdown().await;
    }
```

Run: `cargo test -p yi-agent-app-server --lib thread_store::tests an_in_flight_turn_leaves_a_checkpoint a_completed_turn_removes_its_checkpoint`
Expected: PASS。

- [ ] **Step 6: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): checkpoint an in-flight turn so a crash keeps finished work"
```

---

### Task 4: `thread/resume` 回放后标注崩溃残留轮

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（resume 回放段 ~2694-2720，即 `for item in loaded.items` 之后、`write_response` 之前）
- Test: `server.rs` 的 `mod tests`

**Interfaces:**
- Consumes: Task 1 的 `LoadedThread.pending_turn`；`Notification::TurnCompleted{thread_id, turn_id, status, error}`；`TurnStatus::Interrupted`。
- Produces: 当 `pending_turn` 为真，resume 在回放 items 后多推一条 `turn/completed{status:Interrupted}`。

- [ ] **Step 1: 写失败测试**

```rust
    /// 崩溃残留：主 jsonl 有一轮，另有未收尾的 partial。resume 必须回放两轮的
    /// items，并以一条 interrupted 的 turn/completed 收尾。
    #[tokio::test(flavor = "multi_thread")]
    async fn resume_flags_a_crashed_partial_turn_as_interrupted() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::new();
        // 直接用 store 造盘面：一轮已落盘 + 一段 partial 残留。
        let store = crate::thread_store::ThreadStore::new(dir.path());
        let tid = "thread-crash";
        store
            .create(&crate::thread_store::ThreadMeta {
                thread_id: tid.into(),
                cwd: dir.path().to_string_lossy().to_string(),
                model: "m".into(),
                created_at: 1,
                updated_at: 1,
                title: None,
                permission_mode: crate::thread_store::ThreadMode::Normal,
                pin_seq: None,
            })
            .unwrap();
        store
            .append_turn(
                tid,
                &crate::thread_store::TurnLine::Turn {
                    items: vec![crate::protocol::Item::UserMessage {
                        id: "user-t1".into(),
                        text: "first".into(),
                    }],
                    usage: None,
                    messages: vec![],
                },
            )
            .unwrap();
        store
            .write_partial(
                tid,
                &crate::thread_store::PartialTurn {
                    turn_id: "turn-t2".into(),
                    items: vec![
                        crate::protocol::Item::UserMessage {
                            id: "user-turn-t2".into(),
                            text: "second".into(),
                        },
                        crate::protocol::Item::AgentMessage {
                            id: "item-turn-t2-1".into(),
                            text: "half".into(),
                        },
                    ],
                    messages: vec![],
                    usage: None,
                },
            )
            .unwrap();

        initialize(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;

        let mut saw_half = false;
        let mut interrupted = false;
        for _ in 0..40 {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("item/completed")
                && v["params"]["item"]["text"] == "half"
            {
                saw_half = true;
            }
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
                if v["params"]["status"] == "interrupted" {
                    interrupted = true;
                    break;
                }
            }
            if v.get("id") == Some(&serde_json::json!(5)) && interrupted {
                break;
            }
        }
        assert!(saw_half, "the crashed turn's finished items must be replayed");
        assert!(interrupted, "a crashed partial turn must be flagged interrupted");
        h.shutdown().await;
    }
```

> 重要：`Harness::new()` 的 `cfg.workdir` 是 `test_config()` 的 `/tmp/yi-agent-app-server-test`，
> 与上面 `store` 用的 `dir` 不一致会让 resume 找不到该 thread。**必须**改为：
> ```rust
> let mut cfg = test_config();
> cfg.workdir = dir.path().to_path_buf();
> let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
> ```
> 并让 `store` 也用 `dir.path()`。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p yi-agent-app-server --lib resume_flags_a_crashed_partial_turn_as_interrupted`
Expected: FAIL（没有 interrupted 通知）。

- [ ] **Step 3: 实现回放后的标记**

在 resume 回放段，`if let Some(u) = loaded.usage { ... }` **之后、`write_response` 之前**插入：

```rust
                        // 末尾若是崩溃残留的未收尾 turn，补一条 interrupted 标记，
                        // 让客户端把它显示为"这一轮被中断"，而不是当成正常轮次。
                        if loaded.pending_turn {
                            write_notification(&hub, &Notification::TurnCompleted {
                                    thread_id: thread_id.clone(),
                                    turn_id: "crashed".to_string(),
                                    status: TurnStatus::Interrupted,
                                    error: None,
                                },
                            )
                            .await?;
                        }
```

> 确认 `TurnStatus` 在当前 `use` 作用域内（protocol 已导出的类型，server 内多处使用）。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p yi-agent-app-server --lib resume_flags_a_crashed_partial_turn_as_interrupted thread_resume_replays_history_and_restores_context`
Expected: PASS（含既有 resume 回归）。

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): flag a crashed partial turn as interrupted on resume"
```

---

### Task 5: 全量验证与文档

**Files:**
- Modify: `docs/project-management/yi-agent-app-server.md`（持久化段落追加一句 checkpoint 说明）
- Test: 全 crate

- [ ] **Step 1: 跑 app-server 全量测试**

Run: `cargo test -p yi-agent-app-server`
Expected: PASS（既有 149+ 用例 + 新增用例全绿）。

- [ ] **Step 2: 跑 workspace 构建与 lint**

Run: `cargo build && cargo clippy -p yi-agent-app-server --all-targets`
Expected: 无错误；clippy 无新增 warning。

- [ ] **Step 3: 文档**

在 `docs/project-management/yi-agent-app-server.md` 的持久化相关条目后追加：

```
- [x] 进行中 turn 的 checkpoint（抗震杀/崩溃）：driver 在 turn 内把已 finalize 的 item
  （含提问）+ 上下文快照写 `<cwd>/.yi-agent/threads/<id>.partial.json`（每次 item finalize
  标脏、500ms 去抖、turn 收尾落主 jsonl 后删除）；`ThreadStore::load` 以 partial 首个 item id
  是否已在 jsonl 判定该轮是否收尾，未收尾则合流并置 `pending_turn`，`thread/resume` 据此补发
  `turn/completed{Interrupted}`。见 `docs/superpowers/specs/2026-10-03-turn-checkpoint-persistence-design.md`
```

- [ ] **Step 4: 提交**

```bash
git add docs/project-management/yi-agent-app-server.md
git commit -m "docs(app-server): record in-flight turn checkpoint persistence"
```

---

### Task 6: 修复终审两项（升格崩溃残轮 + 断连清 checkpoint）

> 来源：最终全分支评审（`1a4ef80..fe07253`）的 C1（Critical）/ I1（Important）。C1 的根因
> 是**设计遗漏**——spec 的"读时机（恢复）"只写了合流，没写升格——故本任务同时回改了 spec。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`（新增 `promote_partial`
  + 抽出 `read_log` / `partial_is_committed`，`load` 复用同一判据）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`thread/resume` 调
  `promote_partial`；driver 三个 `return` 站点前 `clear_partial`）
- Test: `thread_store.rs` / `server.rs` 的 `mod tests`
- Docs: `docs/superpowers/specs/2026-10-03-turn-checkpoint-persistence-design.md`（读时机
  补"升格"段 + 测试清单）

- [x] **Step 1: C1 失败回归测试**

`resume_promotes_the_crashed_turn_so_a_later_turn_cannot_drop_it`：崩溃（一轮 jsonl +
一段 partial）→ resume（断言回放 recovered item + `turn/completed{interrupted}`）→ 同一
thread 起**第二个** turn 并正常收尾 → store 冷 `load`：崩溃轮 items 仍在、只一份、不重复
计数、`pending_turn == false`。另有 `promote_partial_*` 三个单元测试。
Expected（修复前）：FAIL——第二个 turn 的 turn-start checkpoint 覆盖 partial，冷 load 里
`item-turn-t2-1` 消失。

- [x] **Step 2: 实现升格**

`ThreadStore::promote_partial(id) -> io::Result<bool>`：读 partial（缺失 → `Ok(false)`）；
首 item id 已在主 jsonl → 只 `clear_partial`、`Ok(false)`；否则 `append_turn(TurnLine::Turn
{items, usage, messages})` 后 `clear_partial`、`Ok(true)`。判据 `partial_is_committed` 与
`load` 共用；`.jsonl` / `.meta.json` 格式不变。`thread/resume` 在 `pending_turn` 时、发
interrupted 之前调用；失败只记 stderr，不阻断 resume。

- [x] **Step 3: C1 通过**

Run: `cargo test -p yi-agent-app-server --lib -- resume_promotes_the_crashed_turn promote_partial`
Expected: PASS（4 个用例）。

- [x] **Step 4: I1 — 三个 early-return 站点前清 checkpoint**

`server.rs` 的 (a) 反向请求序列化失败、(b) 等待审批期间发起客户端断连、(c) 写通知失败
三处 `return` 前，均加：
```rust
if let Err(e) = store.clear_partial(&thread_id) {
    eprintln!("[app-server] failed to clear turn checkpoint ({thread_id}): {e}");
}
```
配套测试 `driver_clears_the_checkpoint_when_the_initiator_disconnects_during_approval`：
以“发起方已从 hub 摘除”接线 `is_connected` 判据，断言 Finished 后 partial 文件被清。

- [x] **Step 5: 全量回归**

Run: `cargo test -p yi-agent-app-server --lib`
Expected: PASS。

---

## Self-Review

**Spec coverage:**
- 存储格式 `.partial.json`（含 turn_id/items/messages/usage）→ Task 1 Step 3。
- 写时机（turn 开始 / finalize 去抖 / 收尾删除）→ Task 3 Step 3。
- 读时机与"首 item id 判收尾"→ Task 1 Step 3；中断标记 → Task 4 Step 3。
- 生命周期（delete/truncate）→ Task 2；`exists`/`list` 无需改动（spec 已说明）→ 未列任务，符合设计。
- 边界（写失败不阻断、损坏忽略）→ Task 1 Step 3（read_partial 记 stderr 返回 None）+ Task 3 `checkpoint()` 只记 stderr。
- 测试（单元 + 集成）→ Task 1/2 单元，Task 3/4 集成，Task 5 全量回归。

**Placeholder scan:** 无 TBD/TODO；所有代码步骤含完整代码；命令与期望结果明确。

**Type consistency:** `PartialTurn{turn_id,items,messages,usage}`、`write_partial`、`clear_partial`、`LoadedThread.pending_turn`、`build_partial`、`checkpoint`、`item_id`(pub(crate)) 在各任务中命名一致。
