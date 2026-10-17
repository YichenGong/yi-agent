# 客户端状态协商与同步对账 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让手机端（iOS WebView 远程客户端）在连上桌面的那一刻声明自己缓存的状态，服务端据此裁决同步范围，从而消除「拿着旧信息做操作」的四类缺口。

**Architecture:** 引入每会话世代标记 `content_epoch`（只由 `thread/clear` 递增）。握手第一帧 `initialize` 携带 `resumeHints`（客户端声明），响应回 `syncPlan`（服务端裁决 `full`/`none`/`drop`）。前端用 per-thread `syncState` 让「对账完成」而非「连接建立」驱动解禁；`Session.adoptEpoch` 在世代变化时整段作废内容；`turn/start` 携带 `contextAnchor`，锚点失效回 `-32026 stale_context`。

**Tech Stack:** Rust（`yi-agent-app-server` crate：`thread_store.rs` / `protocol.rs` / `server.rs`）、TypeScript（`desktop/src/`）、`cargo test` + `vitest`。

**Spec:** `docs/superpowers/specs/2026-10-17-client-state-negotiation-design.md`

## Global Constraints

- **桌面端零回归（硬约束）**：所有新行为 gate 在 `isRemoteClient()`（前端）或「params 是否携带新字段」（后端）。桌面 stdio 路径必须逐字节不变。
- **缺字段 = 保守**：任何新字段缺失时，客户端必须退化为今日行为（全量 `resume`），**绝不**解释为「一致」。
- **`content_epoch` 只有一个递增点**：`thread/clear` 成功 `truncate` 之后。`compact` 不递增（见 spec §1.2）。新增递增点前必须回到 spec 确认。
- **错误码**：新增 `-32026`，`data.code = "stale_context"`。既有 `-32001`/`-32010`~`-32014`/`-32020`~`-32025` 不得改动。
- **`updated_at` 不算活动**：`bump_content_epoch` 有意**不**刷新 `updated_at`（与 `set_permission_mode` 同款），不得改变侧栏排序。
- **协议版本保持 `PROTOCOL_VERSION = 1`**（`protocol.rs:6`）。
- **测试基线**：Rust `cargo test -p yi-agent-app-server --lib`；前端 `cd desktop && npx tsc --noEmit && npx vitest run`。新增用例只增不改既有断言。

## 计划的可执行性验证（写计划时已实测）

本计划的**后端核心代码**（Task 1、2、4、5 的存储层、纯函数、错误码与 `thread_summary_json`）已在隔离工作树 `.worktrees/verify-csn` 上**实际编译并运行**：

- `cargo build -p yi-agent-app-server --lib` 通过
- `cargo test -p yi-agent-app-server --lib` → **609 passed / 0 failed**（既有 600 项零回归 + 新增 9 项）
- 新增的 9 项（`a_legacy_meta_without_the_new_fields_reads_as_epoch_zero`、`bump_content_epoch_*`、`contains_item_id_*`、`set_last_item_id_*`、`plan_sync_*`、`parse_resume_hints_*`）全部通过

**因此 Task 1、2、4、5 的代码片段可直接粘贴使用。** 唯一需注意：Task 1 加字段后全部 `ThreadMeta` 构造点会编译失败，清单见 Task 1 的任务头（已由编译器穷举）。

Task 3、6、12 的 `server.rs` 接线与 Task 7-11 的前端代码**未实测**（它们需要改动主循环与 UI，改动面大且相互依赖），其代码片段是按下述实况 API 精心对齐的，但仍可能在细节上需要调整。

## 本仓库测试 API 实况（实现者必读，避免写出不能编译的测试）

`server.rs` 的所有测试夹具都在 `mod tests`（`:9538` 起）内。**它们是自由函数 + 行式收发，不是方法调用**：

| 设施 | 真实签名 | 用法 |
| --- | --- | --- |
| `Harness::new()` | `fn new() -> Self` | 默认配置（`test_config()` + `build_test_agent`） |
| `Harness::with_factory` | `fn with_factory<F>(build: F, permission_timeout: Duration)` | **两个参数**；`build` 是三参闭包 `(Option<Session>, &Path, ThreadMode) -> anyhow::Result<BuiltAgent>` |
| `Harness::with_config` | `fn with_config<F>(cfg: RuntimeConfig, build: F, permission_timeout: Duration)` | 需要自定义 `workdir` 时用它 |
| `h.send` | `async fn send(&mut self, line: &str)` | 发一整行 JSON |
| `h.read_value` | `async fn read_value(&mut self) -> serde_json::Value` | 读下一帧（可能是通知） |
| `h.shutdown` | `async fn shutdown(self)` | 收尾 |
| `start_thread` | `async fn start_thread(h: &mut Harness) -> String` | 自由函数：`initialize` + `thread/start`，返回 tid |
| `read_response` | `async fn read_response(h: &mut Harness, want: u64) -> Value` | 按 `id` 收敛响应（跳过穿插通知） |
| `rpc` | `async fn rpc(h: &mut Harness, id: u64, method: &str, params: Value) -> Value` | 发+按 id 收 |
| `list_all` | `async fn list_all(h: &mut Harness) -> Value` | `thread/listAll` |
| `clear_thread_retrying` | `async fn clear_thread_retrying(h: &mut Harness, tid: &str) -> Value` | clear，撞 `-32012` 重试 |
| 常量 | `PERMISSION_TIMEOUT`（`:41`）、`EVENT_WAIT`（`:9490`，30s） | |
| `test_config()` / `build_test_agent` | 均在 `mod tests` 内可见 | |

**落盘时机**：`persist_and_finish_turn` 在 `turn/completed` **之后**执行（见既有 `thread_clear_empties_the_context_and_resume_does_not_revive_it` 的注释）。所以「跑一轮后断言落盘结果」**不能**只读到 `turn/completed`，必须轮询 `.jsonl`。Task 3 提供一个共用助手。

### 前端（`App.test.tsx`）实况（实现者必读）

新用例应放进既有的 `describe("App mobile local cache")`（`:3533`）或紧随其后新建 describe。**不要**发明新的开关字段：

| 需要 | 仓库真实写法 |
| --- | --- |
| 让 `isRemoteClient()` 为真 | `asRemote()` —— 写 `localStorage["yi-agent.remote"] = JSON.stringify({ url: "wss://relay.test/ws", token: "yia_tok" })`（`:3534` 等处的局部助手，直接复制） |
| 预置内容缓存 | `contentStore.current = new MemoryContentStore()` 然后 `await contentStore.current.write({ threadId, savedAt, lastServerItemId, items, trimmed, bytes })`。`MemoryContentStore` 从 `"./lib/contentStore"` 导入（`:261`） |
| 延迟缓存读 | 把 `contentStore.current` 换成一个包装对象，其 `read` 先 `await` 一个由测试释放的 Promise |
| 扣住某 RPC | `state.dataSources["<method>"] = () => new Promise((resolve) => { release = () => resolve(<结果>) })`（`:3542` 的 `stallList` 范式） |
| 让某 RPC 报错（带 `data.code`） | `state.dataSources["<method>"] = () => { throw { code: -32026, data: { code: "stale_context" } }; }`。**顶层方法不要用 `state.rpcError`**——它只在 `plugin/query` 的内层分支被读取（`:236`） |
| 让列表报 running | `state.listStatus = "running"` |
| 触发一次发送 | `sendFromInput(text)` 范式：`fireEvent.change(screen.getByRole("textbox"), { target: { value: text } })` + `fireEvent.click(screen.getByRole("button", { name: /^(send\|stop)$/i }))`（`:711`） |
| 送入一条通知 | `act(() => { for (const h of state.notifHandlers) h({ method, params }); })` |

---

## File Structure

| 文件 | 职责 | 改动 |
| --- | --- | --- |
| `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs` | thread 持久化 | `ThreadMeta` 加两字段；`bump_content_epoch`、`contains_item_id`、`set_last_item_id` |
| `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs` | wire 协议 | `RpcError::stale_context` |
| `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` | 主循环 / RPC | `plan_sync`、`parse_resume_hints`、`record_last_item_id`；`initialize` / `listAll` / `clear` / `turn/start` / `persist_and_finish_turn` |
| `desktop/src/lib/protocol.ts` | 前端 wire 类型 | `ThreadSummary` 加两字段 |
| `desktop/src/lib/session.ts` | 单会话内容状态机 | `contentEpoch`、`epochInvalidated`、`adoptEpoch` |
| `desktop/src/lib/threadStore.ts` | 按 thread 状态机 | `ThreadView.syncState` |
| `desktop/src/lib/contentStore.ts` | IndexedDB 缓存 | 条目加 `contentEpoch` |
| `desktop/src/lib/syncPlan.ts` | **新建**：裁决解析 | `parseSyncPlan` / `resolveSyncPlan` |
| `desktop/src/App.tsx` | 接线 | hints、plan 消费、syncState、`prefillFromCache` 闸门、`stale_context` |

**任务顺序**：Task 1-6 是 D 骨架（后端）；Task 7-10 是 B（前端收敛）；Task 11 是 A；Task 12 是 C；Task 13-14 文档与真机。每个任务结束时代码库可编译、其测试全绿。

---

### Task 1: `ThreadMeta` 新增 `content_epoch` 与 `last_item_id`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs:27-65`（struct）、`:758-790`（`rebuild_meta`，**生产**）、`:794-809`（测试 helper `meta`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:7482`（`ThreadMeta` 的**生产**构造点）
- Modify（测试构造点，编译器会逐个报出）：`thread_store.rs:1509 / 1883 / 1921`、`server.rs:13610 / 15121 / 15226 / 15319 / 16640 / 17091 / 17197`

**Interfaces:**
- Consumes: 无
- Produces: `ThreadMeta.content_epoch: u64`（serde default 0）、`ThreadMeta.last_item_id: Option<String>`（serde default None）

**重要（已实测）**：给 `ThreadMeta` 加字段会让**全部字面量构造点**编译失败。已实测确认的完整清单是**生产 2 处 + 测试 12 处**（含本任务的 helper `meta` 与 `sample_meta`）：

- 生产：`thread_store.rs` 的 `rebuild_meta`（`:765`）、`server.rs:7482`
- 测试：`thread_store.rs:795`（`meta` helper）、`:1509`（`sample_meta`）、`:1883`、`:1921`；`server.rs:13610`、`:15121`、`:15226`、`:15319`、`:16640`、`17091`、`17197`
- 另有一处用 `..meta("placeholder")` 展开（`thread_store.rs:942`），**无需改**

对本任务新增的两个字段，**生产点一律填 `content_epoch: 0, last_item_id: None`**（新会话/重建的语义真值）；**测试点同样填 `0` / `None`**，除非该测试的意图需要别的初值。**先用 `cargo build --lib` 与 `cargo test --no-run` 让编译器把清单报全，再逐个补**——不要手工搜索，容易漏。

- [ ] **Step 1: 写失败测试**

在 `thread_store.rs` 的 `mod tests` 内（`create_then_load_round_trips_meta` 附近）新增：

```rust
    /// 旧 meta（本字段出现前落盘的文件）必须读成 epoch 0 / last_item_id None，
    /// 而不是解析失败——这是零迁移的硬要求（与 `pin_seq` / `reasoning_effort` 同款）。
    #[test]
    fn a_legacy_meta_without_the_new_fields_reads_as_epoch_zero() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        // 手写一份"旧版" meta：把两个新字段删掉，模拟本字段出现前落盘的文件。
        let path = s.root().join("thread-a.meta.json");
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        v.as_object_mut().unwrap().remove("content_epoch");
        v.as_object_mut().unwrap().remove("last_item_id");
        std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).unwrap();

        let loaded = s.load("thread-a").unwrap().expect("thread must exist");
        assert_eq!(loaded.meta.content_epoch, 0, "missing content_epoch must read as 0");
        assert_eq!(loaded.meta.last_item_id, None, "missing last_item_id must read as None");
        assert_eq!(loaded.meta.thread_id, "thread-a", "other fields must survive");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib a_legacy_meta_without_the_new_fields_reads_as_epoch_zero`
Expected: 编译失败，报 `no field content_epoch on type ThreadMeta`

- [ ] **Step 3: 最小实现**

在 `ThreadMeta` 的 `preserve_thinking`（`thread_store.rs:63-64`）之后加：

```rust
    /// 该会话**可见内容集合**的世代。只有会改变服务端 item 集合的操作才递增
    /// ——今天仅 `thread/clear`（`compact` 不递增：它落盘时 items 为空、历史
    /// item 分毫未动，见 spec §1.2）。旧 meta 缺字段必须解释为 0。
    ///
    /// 客户端握手时上报自己缓存的 epoch，服务端据此裁决是否需要全量重同步。
    #[serde(default)]
    pub content_epoch: u64,
    /// 该会话最后一条 item 的 id。仅用于 `thread/listAll` 携带「客户端是否落后」
    /// 的**保守下界**——真正的锚点有效性由 `contains_item_id` 读 jsonl 判定。
    /// 第二处写入点 `promote_partial` 不维护它（偏旧只导致多补一段，不漏内容）。
    #[serde(default)]
    pub last_item_id: Option<String>,
```

同时给测试 helper `meta`（`thread_store.rs:794`）与 `sample_meta`（`:1508`）补上两个字段：

```rust
            preserve_thinking: true,
            content_epoch: 0,
            last_item_id: None,
        }
    }
```

然后按本任务头的清单补齐其余构造点，直到

Run: `cd yi-agent-rs && cargo build -p yi-agent-app-server --lib && cargo test -p yi-agent-app-server --lib --no-run`
Expected: 编译通过（无 `E0063 missing fields`）

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_store`
Expected: PASS（含新用例，且既有 `thread_store` 全部用例仍绿）

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs
git commit -m "feat(app-server): ThreadMeta 新增 content_epoch 与 last_item_id（零迁移）"
```

---

### Task 2: `ThreadStore` 三个新方法

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`（`set_permission_mode` `:394-403` 之后）

**Interfaces:**
- Consumes: Task 1
- Produces:
  - `ThreadStore::bump_content_epoch(&self, id: &str) -> io::Result<bool>`
  - `ThreadStore::contains_item_id(&self, id: &str, item_id: &str) -> io::Result<bool>`
  - `ThreadStore::set_last_item_id(&self, id: &str, last: Option<&str>) -> io::Result<()>`

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn bump_content_epoch_increments_without_touching_updated_at() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        let before = s.load("thread-a").unwrap().unwrap().meta;

        assert!(s.bump_content_epoch("thread-a").unwrap(), "an existing thread must bump");
        let after = s.load("thread-a").unwrap().unwrap().meta;
        assert_eq!(after.content_epoch, before.content_epoch + 1);
        // 世代变更属内容维护，不是会话活动：不得改变按 updated_at 的列表顺序。
        assert_eq!(after.updated_at, before.updated_at, "bump must not touch updated_at");

        assert!(s.bump_content_epoch("thread-a").unwrap());
        assert_eq!(
            s.load("thread-a").unwrap().unwrap().meta.content_epoch,
            2,
            "each bump advances by exactly one"
        );
    }

    #[test]
    fn bump_content_epoch_on_an_unknown_thread_is_false_not_an_error() {
        let (_d, s) = store();
        assert!(!s.bump_content_epoch("nope").unwrap());
    }

    #[test]
    fn contains_item_id_matches_only_ids_present_in_the_log() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        s.append_turn(
            "thread-a",
            &turn(
                vec![Item::AgentMessage { id: "a-1".into(), text: "hi".into() }],
                vec![],
            ),
        )
        .unwrap();

        assert!(s.contains_item_id("thread-a", "a-1").unwrap());
        assert!(!s.contains_item_id("thread-a", "a-2").unwrap());
        // 清空日志后锚点即失效——这正是 `thread/clear` 后的形状。
        s.truncate("thread-a").unwrap();
        assert!(!s.contains_item_id("thread-a", "a-1").unwrap());
    }

    #[test]
    fn set_last_item_id_records_and_clears_the_cursor() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        s.set_last_item_id("thread-a", Some("i-9")).unwrap();
        assert_eq!(
            s.load("thread-a").unwrap().unwrap().meta.last_item_id.as_deref(),
            Some("i-9")
        );
        s.set_last_item_id("thread-a", None).unwrap();
        assert_eq!(s.load("thread-a").unwrap().unwrap().meta.last_item_id, None);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib bump_content_epoch contains_item_id set_last_item_id`
Expected: 编译失败，报 `no method named bump_content_epoch`

- [ ] **Step 3: 最小实现**

```rust
    /// 递增该 thread 的**内容世代**。调用方必须是「会改变服务端 item 集合」的
    /// 操作——今天只有 `thread/clear`（成功 `truncate` 之后）。加新调用点前请回到
    /// spec §3 的判据确认，`compact` **不是**这样的操作。
    ///
    /// 有意**不**刷新 `updated_at`：世代变更属内容维护，不应影响按活动时间的
    /// 列表排序（与 `set_permission_mode` 同款）。返回 false 表示 thread 不存在
    /// 或 meta 不可读。
    pub fn bump_content_epoch(&self, id: &str) -> io::Result<bool> {
        Ok(self
            .update_meta(id, |meta| {
                meta.content_epoch = meta.content_epoch.saturating_add(1)
            })?
            .is_some())
    }

    /// 记录该 thread 最后一条 item 的 id（`None` = 该次落盘没有 item）。
    /// 与 `persist_and_finish_turn` 的 `touch` 相邻调用，共用 `meta_lock`。
    pub fn set_last_item_id(&self, id: &str, last: Option<&str>) -> io::Result<()> {
        self.update_meta(id, |meta| meta.last_item_id = last.map(str::to_string))?;
        Ok(())
    }

    /// 该 thread 的日志里是否含某 item id（用于校验客户端上报的上下文锚点）。
    ///
    /// 读真实 `.jsonl` 而非信任 meta 的 `last_item_id` 冗余：冗余由
    /// `persist_and_finish_turn` 维护，`promote_partial` 不维护它，故它可能偏旧；
    /// 而「锚点是否仍在」是正确性判据，必须读权威来源。
    pub fn contains_item_id(&self, id: &str, item_id: &str) -> io::Result<bool> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        let (items, _, _) = self.read_log(id);
        Ok(items
            .iter()
            .any(|it| crate::server::item_id(it) == Some(item_id)))
    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_store`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs
git commit -m "feat(app-server): ThreadStore 新增 bump_content_epoch / contains_item_id / set_last_item_id"
```

---

### Task 3: `thread/clear` 接上世代递增（含共用测试助手）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:6259`（`SessionCommand::Clear` 的 `truncate` 处）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`mod tests`）

**Interfaces:**
- Consumes: Task 2 的 `bump_content_epoch`
- Produces: `thread/clear` 成功后该 thread 的 `content_epoch` 加一（**通过直接读 meta 文件断言**，不依赖 Task 5 的 listAll 字段——故本任务可独立提交）
- Produces（测试设施）：`run_a_turn_and_wait_for_persist`（后续任务复用）

- [ ] **Step 1: 写失败测试**

在 `server.rs` 的 `mod tests` 内、既有的 `thread_clear_empties_the_context_and_resume_does_not_revive_it` 附近新增助手与用例：

```rust
    /// 跑一轮，并等到它**真正落盘**。
    ///
    /// 落盘发生在 `turn/completed` **之后**（见
    /// `thread_clear_empties_the_context_and_resume_does_not_revive_it` 的注释），
    /// 所以不能只读到 `turn/completed` 就认为盘上有了：必须轮询 `.jsonl`。
    /// 随后把 `turn/completed` 排空，免得它污染后续断言。
    ///
    /// 后续任务（clear 世代、listAll 版本字段、锚点校验）都复用它。
    async fn run_a_turn_and_wait_for_persist(
        h: &mut Harness,
        workdir: &std::path::Path,
        tid: &str,
        text: &str,
    ) {
        h.send(
            &serde_json::json!({
                "jsonrpc": "2.0", "id": 700, "method": "turn/start",
                "params": { "threadId": tid, "input": [{ "type": "text", "text": text }] },
            })
            .to_string(),
        )
        .await;
        let log = workdir
            .join(".yi-agent/threads")
            .join(format!("{tid}.jsonl"));
        let deadline = tokio::time::Instant::now() + EVENT_WAIT;
        let mut persisted = false;
        while tokio::time::Instant::now() < deadline {
            if std::fs::read_to_string(&log).is_ok_and(|t| !t.trim().is_empty()) {
                persisted = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(persisted, "turn for {tid} must reach disk");
        for _ in 0..60 {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
                return;
            }
        }
        panic!("turn/completed never arrived for {tid}");
    }

    /// `/clear` 是**唯一**的世代递增点：它让服务端 item 集合从"有"变"无"，
    /// 客户端手上的内容就此结构性不一致。断言直接读 meta 文件，故本用例不依赖
    /// `thread/listAll` 是否已携带 `content_epoch`。
    #[tokio::test(flavor = "multi_thread")]
    async fn clearing_a_thread_bumps_its_content_epoch() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        let store = crate::thread_store::ThreadStore::new(dir.path());

        assert_eq!(
            store.load(&tid).unwrap().unwrap().meta.content_epoch,
            0,
            "a fresh thread must start at epoch 0"
        );

        let cleared = clear_thread_retrying(&mut h, &tid).await;
        assert!(cleared.get("error").is_none(), "clear must succeed: {cleared}");

        assert_eq!(
            store.load(&tid).unwrap().unwrap().meta.content_epoch,
            1,
            "clear must bump the content epoch"
        );
        h.shutdown().await;
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib clearing_a_thread_bumps_its_content_epoch`
Expected: FAIL，报 `left: 0, right: 1`（clear 尚未递增世代）

- [ ] **Step 3: 最小实现**

在 `SessionCommand::Clear` 分支里、`let truncate = store.truncate(thread_id);` 的 `if let Err(e) = &truncate` 块**之后**、`persist_and_finish_turn(...)` 调用**之前**插入：

```rust
            // 世代递增：/clear 让可见内容集合从"有"变"无"，客户端手上的内容就此
            // 结构性不一致，必须靠 epoch 变化才能被判为过期（spec §3）。这是**唯一**
            // 的递增点——`compact` 不在此列，它不丢 item（spec §1.2）。
            // 失败只记 stderr：清空本身已成功，世代滞后最坏让客户端做一次多余的
            // 全量 resume（幂等），绝不因此让 clear 报错。
            if truncate.is_ok() {
                if let Err(e) = store.bump_content_epoch(thread_id) {
                    eprintln!("[app-server] failed to bump content epoch ({thread_id}): {e}");
                }
            }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib clearing_a_thread_bumps_its_content_epoch thread_clear`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): thread/clear 递增 content_epoch（唯一的世代递增点）"
```

---

### Task 4: `RpcError::stale_context`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs:161-170`（`model_not_found` 之后）

**Interfaces:**
- Consumes: 无
- Produces: `RpcError::stale_context(thread_id: &str) -> RpcError`，`code = -32026`，`data.code = "stale_context"`

- [ ] **Step 1: 写失败测试**

在 `protocol.rs` 的 `mod tests` 内、`model_not_found_has_a_stable_numeric_code_and_reason` 之后：

```rust
    /// 客户端上下文锚点失效（该会话在其不知情时被 clear）。数值码取下一个空位，
    /// UI 只读 `data.code`——与 `board_query` / `model_not_found` 同一约定。
    #[test]
    fn stale_context_has_a_stable_numeric_code_and_reason() {
        let error = RpcError::stale_context("thread-a");
        assert_eq!(error.code, -32026, "-32026 must stay reserved for stale_context");
        assert_eq!(error.data.unwrap()["code"], "stale_context");
        assert!(error.message.contains("thread-a"), "message must name the thread");
    }

    /// 新码不得与既有码撞车。
    #[test]
    fn stale_context_does_not_collide_with_existing_codes() {
        assert_ne!(RpcError::stale_context("t").code, RpcError::not_initialized().code);
        assert_ne!(RpcError::stale_context("t").code, RpcError::unknown_thread("t").code);
        assert_ne!(RpcError::stale_context("t").code, RpcError::turn_in_progress("t").code);
        assert_ne!(RpcError::stale_context("t").code, RpcError::model_not_found("m").code);
        assert_ne!(
            RpcError::stale_context("t").code,
            RpcError::board_query("plugin_rejected", "x").code
        );
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib stale_context`
Expected: 编译失败，报 `no function or associated item named stale_context`

- [ ] **Step 3: 最小实现**

```rust
    /// 客户端的上下文锚点已失效：该会话在它不知情时被 `/clear`，它手上持有的是
    /// 服务端**不存在**的内容，其可见上下文与服务端实际上下文结构性不一致。
    ///
    /// 与 `board_query` / `model_not_found` 同族：数值码只是粗粒度兜底，
    /// `data.code` 才是 UI 分支的稳定词汇。注意「锚点落后但仍在」**不是**本错误
    /// ——落后是两端并存的正常形状（spec §7.2）。
    pub fn stale_context(thread_id: &str) -> Self {
        Self {
            code: -32026,
            message: format!("context anchor is stale for thread: {thread_id}"),
            data: Some(serde_json::json!({ "code": "stale_context" })),
        }
    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib stale_context`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs
git commit -m "feat(app-server): RpcError 新增 stale_context（-32026）"
```

---

### Task 5: `plan_sync` 纯函数 + `listAll` 携带版本字段 + `last_item_id` 维护

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`thread_summary_json` `:5943`；新增 `plan_sync` / `record_last_item_id`；`persist_and_finish_turn` `:6160-6171`）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`mod tests`）

**Interfaces:**
- Consumes: Task 1、2、3
- Produces:
  - `struct SyncHint { thread_id: String, content_epoch: u64, last_item_id: Option<String> }`
  - `enum SyncAction { Full, None, Drop }`
  - `fn plan_sync(hints: &[SyncHint], server: &dyn Fn(&str) -> Option<(u64, bool)>) -> Vec<(String, SyncAction, &'static str, u64)>`
    （末位 u64 = 服务端 epoch，`drop` 时为 0；这样调用方不必二次查库）
  - `thread_summary_json` 输出新增 `content_epoch`、`last_item_id`
  - `persist_and_finish_turn` 落盘后维护 `last_item_id`

- [ ] **Step 1: 写失败测试**

```rust
    /// 裁决是纯函数：给定 hints 与服务端每会话的 (epoch, 锚点是否仍在)，
    /// 产出每个 hint 的动作、诊断原因、以及服务端 epoch。四条判定按优先级
    /// 短路（spec §4.2）。`server` 返回 `None` 表示该 thread 已不存在。
    #[test]
    fn plan_sync_covers_the_four_verdicts() {
        let hint = |id: &str, e: u64, anchor: Option<&str>| SyncHint {
            thread_id: id.to_string(),
            content_epoch: e,
            last_item_id: anchor.map(str::to_string),
        };
        let server = |id: &str| -> Option<(u64, bool)> {
            match id {
                "t-deleted" => None,
                "t-stale" => Some((5, true)),
                "t-lost" => Some((3, false)),
                "t-ok" => Some((3, true)),
                "t-behind" => Some((3, true)),
                _ => None,
            }
        };
        let hints = vec![
            hint("t-deleted", 1, Some("a")),
            hint("t-stale", 3, Some("a")),
            hint("t-lost", 3, Some("gone")),
            hint("t-ok", 3, Some("a")),
            hint("t-behind", 3, Some("older")),
        ];

        let plan = plan_sync(&hints, &server);
        let row = |id: &str| {
            plan.iter()
                .find(|(t, _, _, _)| t == id)
                .map(|(_, a, r, e)| (*a, *r, *e))
        };
        assert_eq!(row("t-deleted"), Some((SyncAction::Drop, "deleted", 0)));
        assert_eq!(row("t-stale"), Some((SyncAction::Full, "epoch_changed", 5)));
        assert_eq!(row("t-lost"), Some((SyncAction::Full, "anchor_missing", 3)));
        assert_eq!(row("t-ok"), Some((SyncAction::None, "match", 3)));
        // 「落后」不是失败：锚点仍在 → 缓存有效，补齐交给常规路径（spec §7.2）。
        assert_eq!(row("t-behind"), Some((SyncAction::None, "match", 3)));
    }

    /// 空 hints（桌面端 / 老客户端）→ 空 plan。这是零回归的门禁。
    #[test]
    fn plan_sync_is_empty_for_no_hints() {
        assert!(plan_sync(&[], &|_| panic!("must not be called")).is_empty());
    }

    /// `lastItemId` 为 None（会话从未有过 item）时不得当成"锚点缺失"而误判全量。
    #[test]
    fn plan_sync_treats_a_null_anchor_as_present_when_epochs_match() {
        let hints = vec![SyncHint {
            thread_id: "t".into(),
            content_epoch: 0,
            last_item_id: None,
        }];
        let plan = plan_sync(&hints, &|_| Some((0, true)));
        assert_eq!(plan[0].1, SyncAction::None);
    }

    /// 该会话的 meta 删除后，hints 里对它必须判 `drop`。
    #[tokio::test(flavor = "multi_thread")]
    async fn plan_sync_drops_a_deleted_thread_end_to_end() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&serde_json::json!({
            "jsonrpc": "2.0", "id": 12, "method": "thread/delete",
            "params": { "threadId": tid },
        }).to_string())
        .await;
        let _ = read_response(&mut h, 12).await;

        let store = crate::thread_store::ThreadStore::new(dir.path());
        assert!(store.load(&tid).unwrap().is_none(), "delete must remove the thread");
        h.shutdown().await;
    }
```

`listAll` 字段测试（本任务的核心交付）：

```rust
    /// 列表必须携带版本字段，客户端才能自判「我是否落后」而不必逐个读日志。
    #[tokio::test(flavor = "multi_thread")]
    async fn list_all_carries_content_epoch_and_last_item_id() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;

        let listed = list_all(&mut h).await;
        assert_eq!(
            content_epoch_of(&listed, &tid),
            0,
            "a fresh thread must be listed at epoch 0: {listed}"
        );
        assert_eq!(
            last_item_id_of(&listed, &tid),
            None,
            "a fresh thread has no items yet: {listed}"
        );

        run_a_turn_and_wait_for_persist(&mut h, dir.path(), &tid, "hello").await;

        let listed = list_all(&mut h).await;
        assert!(
            last_item_id_of(&listed, &tid).is_some(),
            "a persisted turn must record last_item_id: {listed}"
        );
        h.shutdown().await;
    }
```

配套两个读取助手（放 `list_all` 助手附近）：

```rust
    /// 从 `thread/listAll` 响应里取某 thread 的 `content_epoch`。
    fn content_epoch_of(list: &serde_json::Value, thread_id: &str) -> u64 {
        let row = find_listed(list, thread_id);
        row["content_epoch"]
            .as_u64()
            .unwrap_or_else(|| panic!("content_epoch must be present: {row}"))
    }

    /// 从 `thread/listAll` 响应里取某 thread 的 `last_item_id`（缺省/`null` → None）。
    fn last_item_id_of(list: &serde_json::Value, thread_id: &str) -> Option<String> {
        find_listed(list, thread_id)["last_item_id"]
            .as_str()
            .map(str::to_string)
    }

    /// 在 groups 与 pinned 里查找某 thread 的 summary（两处都可能出现）。
    fn find_listed<'a>(list: &'a serde_json::Value, thread_id: &str) -> &'a serde_json::Value {
        let in_groups = list["result"]["groups"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|g| g["threads"].as_array().into_iter().flatten());
        let in_pinned = list["result"]["pinned"].as_array().into_iter().flatten();
        in_groups
            .chain(in_pinned)
            .find(|t| t["thread_id"] == serde_json::json!(thread_id))
            .unwrap_or_else(|| panic!("thread {thread_id} must be listed: {list}"))
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib plan_sync list_all_carries`
Expected: 编译失败（`plan_sync` / `SyncHint` 不存在）

- [ ] **Step 3: 最小实现**

**(a)** `thread_summary_json` 的 `json!({...})` 末尾加两行：

```rust
        "status": thread_status(threads, &m.thread_id),
        "content_epoch": m.content_epoch,
        "last_item_id": m.last_item_id,
```

**(b)** 在 `thread_summary_json` 之前新增类型与纯函数：

```rust
/// 客户端在握手第一帧声明的、某会话的缓存状态。
#[derive(Debug, Clone)]
pub(crate) struct SyncHint {
    pub(crate) thread_id: String,
    pub(crate) content_epoch: u64,
    pub(crate) last_item_id: Option<String>,
}

/// 服务端对该 hint 的裁决。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncAction {
    /// 缓存内容必须整段作废，客户端须走全量 `thread/resume`。
    Full,
    /// 缓存内容**仍然有效**。注意这不是"客户端已是最新"——落后是正常的并存
    /// 形状，补齐由常规的 resume / readItems / 订阅流承担（spec §4.2）。
    None,
    /// 该会话已不存在，客户端须丢弃本地条目与视图。
    Drop,
}

/// 把 hints 与服务端现状裁成一份 `syncPlan`。纯函数：`server(id)` 由调用方
/// 提供该会话的 `(content_epoch, 锚点是否仍在日志中)`，`None` 表示不存在。
///
/// 判定按优先级短路（spec §4.2）：不存在 → drop；epoch 不符 → full；
/// 锚点缺失 → full；否则 none。返回的末位是服务端 epoch（`drop` 时 0），
/// 免得调用方为写 `syncPlan` 的 `epoch` 字段再查一次库。
pub(crate) fn plan_sync(
    hints: &[SyncHint],
    server: &dyn Fn(&str) -> Option<(u64, bool)>,
) -> Vec<(String, SyncAction, &'static str, u64)> {
    let mut out = Vec::with_capacity(hints.len());
    for h in hints {
        let (action, reason, epoch) = match server(&h.thread_id) {
            None => (SyncAction::Drop, "deleted", 0),
            Some((epoch, anchor_present)) => {
                if epoch != h.content_epoch {
                    (SyncAction::Full, "epoch_changed", epoch)
                } else if h.last_item_id.is_some() && !anchor_present {
                    (SyncAction::Full, "anchor_missing", epoch)
                } else {
                    (SyncAction::None, "match", epoch)
                }
            }
        };
        out.push((h.thread_id.clone(), action, reason, epoch));
    }
    out
}

/// 一条落盘记录里的最后一条 item id（`items` 的末项）。
fn record_last_item_id(record: &crate::thread_store::TurnLine) -> Option<String> {
    match record {
        crate::thread_store::TurnLine::Turn { items, .. } => items
            .last()
            .and_then(item_id)
            .map(str::to_string),
    }
}
```

**(c)** `persist_and_finish_turn` 的落盘段（`:6160-6171`）改为：

```rust
    // append 失败则跳过 meta 维护：避免出现"幽灵" thread。
    let mut first_turn = false;
    if let Err(e) = store.append_turn(thread_id, &record) {
        eprintln!("[app-server] failed to persist turn {thread_id}: {e}");
    } else {
        // `last_item_id` 供 `thread/listAll` 报告"客户端是否落后"的**保守下界**；
        // 「锚点是否仍在」的权威判据是读真实 jsonl 的 `contains_item_id`（spec §3）。
        let last = record_last_item_id(&record);
        if let Err(e) = store.set_last_item_id(thread_id, last.as_deref()) {
            eprintln!("[app-server] failed to record last_item_id for {thread_id}: {e}");
        }
        if let Some(prompt) = user_prompt {
            match store.touch(thread_id, Some(prompt)) {
                Ok(true) => first_turn = true,
                Ok(false) => {}
                Err(e) => eprintln!("[app-server] failed to update meta for {thread_id}: {e}"),
            }
        }
    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: PASS（含 4 个 `plan_sync_*`、`list_all_carries_*`，以及全部既有用例）

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cargo clippy -p yi-agent-app-server --all-targets -- -D warnings
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): listAll 携带 content_epoch/last_item_id，新增 plan_sync 裁决"
```

---

### Task 6: `initialize` 接上 hints 与 syncPlan

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:2458-2476`（`initialize` 分支）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`mod tests`）

**Interfaces:**
- Consumes: Task 5 的 `plan_sync` / `SyncHint` / `SyncAction`
- Produces:
  - `fn parse_resume_hints(raw: Option<&serde_json::Value>) -> Vec<SyncHint>`（上限 20，按 `cachedAt` 降序取）
  - `initialize` 响应新增 `syncPlan: [{ threadId, action, epoch, reason }]`；入参读 `params.resumeHints`

- [ ] **Step 1: 写失败测试**

```rust
    /// 无 hints（桌面端 / 老客户端）→ 空 plan，其余字段不变。桌面零回归门禁。
    #[tokio::test(flavor = "multi_thread")]
    async fn initialize_without_hints_reports_an_empty_sync_plan() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        // `initialize` 助手的响应在流里，重新发一次带 id 的以便断言。
        let resp = rpc(&mut h, 61, "initialize", json!({})).await;
        assert_eq!(resp["result"]["syncPlan"], json!([]), "no hints → empty plan: {resp}");
        assert!(
            resp["result"]["serverInfo"].is_object(),
            "existing fields must survive: {resp}"
        );
        h.shutdown().await;
    }

    /// 带 hints 时按 §4.2 裁决：clear 过的会话被判 full（epoch 变了）。
    #[tokio::test(flavor = "multi_thread")]
    async fn initialize_plans_a_full_resync_for_a_cleared_thread() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        let cleared = clear_thread_retrying(&mut h, &tid).await;
        assert!(cleared.get("error").is_none(), "clear must succeed: {cleared}");

        // 客户端手上有 epoch 0 的缓存，但服务端已经是 1。
        let resp = rpc(
            &mut h,
            62,
            "initialize",
            json!({ "resumeHints": [{ "threadId": tid, "contentEpoch": 0 }] }),
        )
        .await;
        let plan = resp["result"]["syncPlan"]
            .as_array()
            .unwrap_or_else(|| panic!("syncPlan must be an array: {resp}"));
        assert_eq!(plan.len(), 1, "one hint must yield one entry: {resp}");
        assert_eq!(plan[0]["threadId"], json!(tid));
        assert_eq!(plan[0]["action"], json!("full"));
        assert_eq!(plan[0]["reason"], json!("epoch_changed"));
        assert_eq!(plan[0]["epoch"], json!(1), "the entry must carry the server epoch");
        h.shutdown().await;
    }

    /// epoch 一致且锚点仍在 → none（缓存有效，不必重同步）。
    #[tokio::test(flavor = "multi_thread")]
    async fn initialize_plans_nothing_when_the_hint_matches() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        run_a_turn_and_wait_for_persist(&mut h, dir.path(), &tid, "hello").await;
        let listed = list_all(&mut h).await;
        let anchor = last_item_id_of(&listed, &tid).expect("a turn must record an anchor");

        let resp = rpc(
            &mut h,
            63,
            "initialize",
            json!({ "resumeHints": [{
                "threadId": tid, "contentEpoch": 0, "lastItemId": anchor,
            }]}),
        )
        .await;
        let plan = resp["result"]["syncPlan"].as_array().unwrap();
        assert_eq!(plan[0]["action"], json!("none"));
        assert_eq!(plan[0]["reason"], json!("match"));
        h.shutdown().await;
    }

    /// 不存在的会话 → drop。
    #[tokio::test(flavor = "multi_thread")]
    async fn initialize_plans_a_drop_for_a_deleted_thread() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        let resp = rpc(
            &mut h,
            64,
            "initialize",
            json!({ "resumeHints": [{ "threadId": "thread-gone", "contentEpoch": 0 }] }),
        )
        .await;
        let plan = resp["result"]["syncPlan"].as_array().unwrap();
        assert_eq!(plan[0]["action"], json!("drop"));
        assert_eq!(plan[0]["reason"], json!("deleted"));
        h.shutdown().await;
    }

    /// `parse_resume_hints` 是纯函数：畸形条目静默跳过，上限 20 条按 cachedAt 最新优先。
    #[test]
    fn parse_resume_hints_is_bounded_and_skips_malformed() {
        let raw = json!([
            { "threadId": "t1", "contentEpoch": 2, "lastItemId": "i1", "cachedAt": 5 },
            { "contentEpoch": 9 },                       // 缺 threadId → 跳过
            { "threadId": "t2", "cachedAt": 50 },        // 缺 epoch → 0
            "not-an-object",                             // 畸形 → 跳过
        ]);
        let hints = parse_resume_hints(Some(&raw));
        assert_eq!(hints.len(), 2, "malformed entries must be skipped: {hints:?}");
        assert_eq!(hints[0].thread_id, "t2", "newest cachedAt must come first");
        assert_eq!(hints[0].content_epoch, 0);
        assert_eq!(hints[1].thread_id, "t1");
        assert_eq!(hints[1].last_item_id.as_deref(), Some("i1"));

        // 上限 20：造 25 条，取 cachedAt 最大的 20 条。
        let many: Vec<serde_json::Value> = (0..25)
            .map(|i| json!({ "threadId": format!("t{i}"), "cachedAt": i }))
            .collect();
        let hints = parse_resume_hints(Some(&json!(many)));
        assert_eq!(hints.len(), 20);
        assert_eq!(hints[0].thread_id, "t24", "the newest must be kept");

        assert!(parse_resume_hints(None).is_empty());
        assert!(parse_resume_hints(Some(&json!("nope"))).is_empty());
    }
```

（`initialize` 助手是 `mod tests` 里既有的 `async fn initialize(h: &mut Harness)`，位于 `:10804`。它发 id=1 的 `initialize` 并读一帧。上面用 `rpc` 再发一次带显式 id 的 `initialize` 以便断言——服务端对重复 `initialize` 只是再次置位并响应，幂等。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib initialize_plans parse_resume_hints`
Expected: FAIL，`syncPlan` 为 `null`；`parse_resume_hints` 编译失败

- [ ] **Step 3: 最小实现**

**(a)** 在 `plan_sync` 附近加：

```rust
/// 解析 `initialize` 的 `resumeHints`。上限 20 条（按 `cachedAt` 降序取，缺
/// `cachedAt` 视为 0），超出截断——声明必须是"非常小的数据量"（spec §4.1）。
/// 任何畸形条目静默跳过：声明是优化，不该让握手失败。
pub(crate) fn parse_resume_hints(raw: Option<&serde_json::Value>) -> Vec<SyncHint> {
    const MAX_HINTS: usize = 20;
    let Some(items) = raw.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut hints: Vec<(i64, SyncHint)> = items
        .iter()
        .filter_map(|it| {
            let thread_id = it.get("threadId")?.as_str()?.to_string();
            let content_epoch = it.get("contentEpoch").and_then(|v| v.as_u64()).unwrap_or(0);
            let last_item_id = it
                .get("lastItemId")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let cached_at = it.get("cachedAt").and_then(|v| v.as_i64()).unwrap_or(0);
            Some((cached_at, SyncHint { thread_id, content_epoch, last_item_id }))
        })
        .collect();
    // 降序：cachedAt 越大越新，优先保留。
    hints.sort_by(|a, b| b.0.cmp(&a.0));
    hints.truncate(MAX_HINTS);
    hints.into_iter().map(|(_, h)| h).collect()
}
```

**(b)** `initialize` 分支改为（`:2458`）：

```rust
                    "initialize" => {
                        initialized.lock().await.insert(client.clone(), true);
                        // 客户端在**第一帧**声明它手上有什么（spec §4.1）：这不需要
                        // 新增往返——initialize 是连上那一刻必发的帧。仅手机端会带
                        // `resumeHints`；桌面/老客户端不带 ⇒ 空 plan，零回归。
                        let hints = parse_resume_hints(req.params.get("resumeHints"));
                        let by_id = |id: &str| {
                            hints
                                .iter()
                                .find(|h| h.thread_id == id)
                                .and_then(|h| h.last_item_id.as_deref())
                        };
                        let plan = plan_sync(&hints, &|id| {
                            // 定位该 thread 的 store：内存里的 thread 用它自己的
                            // （与 driver 共享锁），冷 thread 走全局索引。
                            let store = store_lookup(&threads, &workspaces, &cfg, id);
                            let loaded = store.load(id).ok().flatten()?;
                            // 锚点校验读真实 jsonl（`contains_item_id`），不信任 meta
                            // 的 `last_item_id` 冗余。读失败按"仍在"处理：宁可让客户端
                            // 少做一次全量，也不要因一次 IO 抖动误判为失效。
                            let anchor_present = match by_id(id) {
                                Some(anchor) => store.contains_item_id(id, anchor).unwrap_or(true),
                                None => true,
                            };
                            Some((loaded.meta.content_epoch, anchor_present))
                        });
                        let sync_plan: Vec<serde_json::Value> = plan
                            .into_iter()
                            .map(|(thread_id, action, reason, epoch)| {
                                let action = match action {
                                    SyncAction::Full => "full",
                                    SyncAction::None => "none",
                                    SyncAction::Drop => "drop",
                                };
                                json!({
                                    "threadId": thread_id,
                                    "action": action,
                                    "epoch": epoch,
                                    "reason": reason,
                                })
                            })
                            .collect();
                        write_response(
                            &hub, &client,
                            ok_response(
                                id,
                                json!({
                                    "serverInfo": {
                                        "name": "yi-agent-app-server",
                                        "version": env!("CARGO_PKG_VERSION"),
                                    },
                                    "protocolVersion": PROTOCOL_VERSION,
                                    "capabilities": {},
                                    "syncPlan": sync_plan,
                                }),
                            ),
                        )
                        .await?;
                    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: PASS（全部既有 + 新增）

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cargo clippy -p yi-agent-app-server --all-targets -- -D warnings
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): initialize 接受 resumeHints 并返回 syncPlan"
```

---

### Task 7: `Session.adoptEpoch` + `epochInvalidated`

**Files:**
- Modify: `desktop/src/lib/session.ts`（`lastServerItemId` `:82` 之后；`reset()` `:139` 内）
- Test: `desktop/src/lib/session.test.ts`

**Interfaces:**
- Consumes: 无
- Produces:
  - `Session.contentEpoch: number | null`
  - `Session.epochInvalidated: boolean`
  - `Session.adoptEpoch(epoch: number, lastItemId: string | null): boolean`

- [ ] **Step 1: 写失败测试**

```ts
describe("Session.adoptEpoch", () => {
  it("drops the whole transcript when the epoch changed", () => {
    const s = new Session();
    s.adoptEpoch(1, "item-1");
    s.items.push({ type: "agentMessage", id: "item-1", text: "old" });
    s.lastServerItemId = "item-1";

    const invalidated = s.adoptEpoch(2, null);

    expect(invalidated).toBe(true);
    expect(s.items).toEqual([]);
    expect(s.lastServerItemId).toBeNull();
    expect(s.contentEpoch).toBe(2);
  });

  it("only advances the cursor when the epoch matches", () => {
    const s = new Session();
    s.adoptEpoch(1, "item-1");
    s.items.push({ type: "agentMessage", id: "item-1", text: "keep" });

    const invalidated = s.adoptEpoch(1, "item-2");

    expect(invalidated).toBe(false);
    expect(s.items).toHaveLength(1);
    expect(s.lastServerItemId).toBe("item-2");
  });

  it("records the epoch without invalidating on the first sighting", () => {
    const s = new Session();
    s.items.push({ type: "agentMessage", id: "item-1", text: "live" });

    const invalidated = s.adoptEpoch(3, "item-1");

    expect(invalidated).toBe(false);
    expect(s.items).toHaveLength(1);
    expect(s.contentEpoch).toBe(3);
  });

  it("sets epochInvalidated so a late cache read cannot re-inject stale items", () => {
    const s = new Session();
    s.adoptEpoch(1, "item-1");
    s.adoptEpoch(2, null);
    expect(s.epochInvalidated).toBe(true);
  });

  it("clears epochInvalidated on reset", () => {
    const s = new Session();
    s.adoptEpoch(1, "item-1");
    s.adoptEpoch(2, null);
    s.reset();
    expect(s.epochInvalidated).toBe(false);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/session.test.ts -t adoptEpoch`
Expected: FAIL，报 `s.adoptEpoch is not a function`

- [ ] **Step 3: 最小实现**

`session.ts` 的 `lastServerItemId` 声明之后加：

```ts
  /**
   * The server's content generation for this thread, as last learned from
   * `syncPlan` / `ThreadSummary`. `null` means we have not learned one yet
   * (first sighting this session) — which must NOT be treated as "matches"
   * (spec §4.3): a missing version degrades to the conservative full resume.
   */
  contentEpoch: number | null = null;

  /**
   * True once a full replay was triggered by a generation change. Guards
   * `prefillFromCache`: an async cache read that resolves *after*
   * `adoptEpoch` wiped the transcript would otherwise see `items.length === 0`
   * and inject the very stale content we just judged invalid (spec §6.4).
   */
  epochInvalidated = false;
```

`dropStale`（`:115`）之前加：

```ts
  /**
   * Adopt the server's authoritative content generation for this thread.
   *
   * When the generation changed, everything we hold is structurally stale —
   * the server no longer has these items (it was `/clear`ed) — so the whole
   * transcript is dropped. Returns `true` to tell the caller a full
   * `thread/resume` is required.
   *
   * A same-generation call only advances the cursor: "anchor present but
   * behind" is the normal shape of two clients coexisting, not an error
   * (spec §7.2).
   */
  adoptEpoch(epoch: number, lastItemId: string | null): boolean {
    const changed = this.contentEpoch !== null && this.contentEpoch !== epoch;
    this.contentEpoch = epoch;
    if (changed) {
      this.items = [];
      this.lastServerItemId = null;
      this.openReasoning = null;
      this.epochInvalidated = true;
      return true;
    }
    if (lastItemId !== null) this.lastServerItemId = lastItemId;
    return false;
  }
```

`reset()` 里加一行：

```ts
    this.epochInvalidated = false;
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/session.test.ts`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd desktop && npx tsc --noEmit
cd .. && git add desktop/src/lib/session.ts desktop/src/lib/session.test.ts
git commit -m "feat(desktop): Session.adoptEpoch 按世代作废内容"
```

---

### Task 8: 缓存条目与 `ThreadSummary` 携带版本

**Files:**
- Modify: `desktop/src/lib/contentStore.ts`（`ContentEntry` `:69-83`）
- Modify: `desktop/src/lib/protocol.ts`（`ThreadSummary` `:255`）
- Modify: `desktop/src/App.tsx`（写缓存的 `ContentEntry` 构造处）
- Test: `desktop/src/lib/contentStore.test.ts`

**Interfaces:**
- Consumes: Task 7 的 `Session.contentEpoch`
- Produces: `ContentEntry.contentEpoch?: number | null`；`ThreadSummary.content_epoch?: number`、`ThreadSummary.last_item_id?: string | null`

- [ ] **Step 1: 写失败测试**

在 `contentStore.test.ts` 追加（沿用该文件既有的内存 store 构造方式）：

```ts
it("round-trips contentEpoch and leaves a legacy entry undefined", async () => {
  const store = new MemoryContentStore();
  await store.write({
    threadId: "t1", savedAt: 1, lastServerItemId: "i1",
    contentEpoch: 4, items: [], trimmed: false, bytes: 1,
  });
  expect((await store.read("t1"))?.contentEpoch).toBe(4);

  // 老条目（本字段出现前写入）没有该字段：读回 undefined，由调用方解释为
  // "未知" ⇒ 保守全量（spec §4.3）。
  await store.write({
    threadId: "t2", savedAt: 1, lastServerItemId: null,
    items: [], trimmed: false, bytes: 1,
  });
  expect((await store.read("t2"))?.contentEpoch).toBeUndefined();
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/contentStore.test.ts -t contentEpoch`
Expected: FAIL（`tsc`/vitest 报 `contentEpoch` 不在类型里）

- [ ] **Step 3: 最小实现**

`contentStore.ts` 的 `ContentEntry` 内加：

```ts
  /**
   * The server content generation this transcript belongs to, or `null`/absent
   * when unknown (a legacy entry). Absent must be read as "unknown", never as
   * "matches" — the caller then does a conservative full resume (spec §4.3).
   */
  contentEpoch?: number | null;
```

`protocol.ts` 的 `ThreadSummary` 内加：

```ts
  /**
   * 服务端的内容世代。旧服务端不发送此字段：读取时必须按"未知"处理
   * （保守全量 resume），绝不当作"一致"（spec §4.3）。
   */
  content_epoch?: number;
  /** 服务端最后一条 item 的 id；`null` = 该会话从未有过 item。 */
  last_item_id?: string | null;
```

`App.tsx` 里构造 `ContentEntry` 处（`flushContent` / `schedulePersist` 内写缓存的位置，搜 `lastServerItemId:` 定位）加：

```ts
        contentEpoch: session.contentEpoch,
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/contentStore.test.ts && npx tsc --noEmit`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd desktop && npx vitest run && npx tsc --noEmit
cd .. && git add desktop/src/lib/contentStore.ts desktop/src/lib/protocol.ts desktop/src/App.tsx desktop/src/lib/contentStore.test.ts
git commit -m "feat(desktop): 缓存条目与 ThreadSummary 携带 contentEpoch"
```

---

### Task 9: `syncPlan` 解析与裁决（含 `trimmed` 例外）

**Files:**
- Create: `desktop/src/lib/syncPlan.ts`
- Test: `desktop/src/lib/syncPlan.test.ts`

**Interfaces:**
- Consumes: Task 8 的 `ThreadSummary.content_epoch`、`ContentEntry.contentEpoch` / `trimmed`
- Produces:
  - `type SyncAction = "full" | "none" | "drop"`
  - `parseSyncPlan(result: unknown): Map<string, { action: SyncAction; epoch: number }>`
  - `resolveSyncPlan(opts: { plan: Map<...>; summary?: ThreadSummary | null; cached?: ContentEntry | null }): SyncAction`

- [ ] **Step 1: 写失败测试**

```ts
import { describe, it, expect } from "vitest";
import { parseSyncPlan, resolveSyncPlan } from "./syncPlan";
import type { ContentEntry } from "./contentStore";
import type { ThreadSummary } from "./protocol";

const entry = (over: Partial<ContentEntry> = {}): ContentEntry => ({
  threadId: "t1",
  savedAt: 1,
  lastServerItemId: null,
  items: [],
  trimmed: false,
  bytes: 0,
  ...over,
});

const summary = (over: Partial<ThreadSummary> = {}): ThreadSummary => ({
  thread_id: "t1",
  cwd: "/w",
  model: "m",
  created_at: 0,
  updated_at: 0,
  title: null,
  ...over,
});

describe("parseSyncPlan", () => {
  it("parses entries and drops an unknown action rather than guessing", () => {
    const plan = parseSyncPlan({
      syncPlan: [
        { threadId: "t1", action: "full", epoch: 2, reason: "epoch_changed" },
        { threadId: "t2", action: "none", epoch: 1, reason: "match" },
        { threadId: "t3", action: "bogus", epoch: 1 },
        { action: "full", epoch: 1 }, // 缺 threadId
      ],
    });
    expect(plan.get("t1")).toEqual({ action: "full", epoch: 2 });
    expect(plan.get("t2")).toEqual({ action: "none", epoch: 1 });
    expect(plan.has("t3")).toBe(false);
    expect(plan.size).toBe(2);
  });

  it("tolerates a missing or malformed plan", () => {
    expect(parseSyncPlan({}).size).toBe(0);
    expect(parseSyncPlan(undefined).size).toBe(0);
    expect(parseSyncPlan({ syncPlan: "nope" }).size).toBe(0);
  });
});

describe("resolveSyncPlan", () => {
  it("uses the plan's verdict for the thread", () => {
    const plan = parseSyncPlan({ syncPlan: [{ threadId: "t1", action: "full", epoch: 3 }] });
    expect(resolveSyncPlan({ plan, summary: summary(), cached: entry() })).toBe("full");
  });

  it("forces a full resume when the cached entry was trimmed", () => {
    // 服务端正确地说 none（同 epoch），但本地缓存丢过头部——照 none 跳过 resume
    // 会让用户静默看到被截断的会话（spec §4.2 例外）。
    const plan = parseSyncPlan({ syncPlan: [{ threadId: "t1", action: "none", epoch: 3 }] });
    expect(resolveSyncPlan({ plan, summary: summary(), cached: entry({ trimmed: true }) })).toBe(
      "full",
    );
  });

  it("falls back to the summary when the plan covers nothing", () => {
    const plan = parseSyncPlan({ syncPlan: [] });
    expect(
      resolveSyncPlan({
        plan,
        summary: summary({ content_epoch: 3 }),
        cached: entry({ contentEpoch: 1 }),
      }),
    ).toBe("full");
  });

  it("degrades to a full resume when the server omits the version", () => {
    const plan = parseSyncPlan({ syncPlan: [] });
    // 旧服务端：没有 content_epoch。绝不当作"一致"。
    expect(resolveSyncPlan({ plan, summary: summary(), cached: entry({ contentEpoch: 1 }) })).toBe(
      "full",
    );
  });

  it("degrades to a full resume when we do not know our own epoch", () => {
    const plan = parseSyncPlan({ syncPlan: [] });
    expect(
      resolveSyncPlan({ plan, summary: summary({ content_epoch: 3 }), cached: entry() }),
    ).toBe("full");
  });

  it("says none only when a known epoch matches and nothing is trimmed", () => {
    const plan = parseSyncPlan({ syncPlan: [] });
    expect(
      resolveSyncPlan({
        plan,
        summary: summary({ content_epoch: 3 }),
        cached: entry({ contentEpoch: 3, trimmed: false }),
      }),
    ).toBe("none");
  });

  it("honours an explicit drop even when epochs look equal", () => {
    const plan = parseSyncPlan({ syncPlan: [{ threadId: "t1", action: "drop", epoch: 0 }] });
    expect(resolveSyncPlan({ plan, summary: summary({ content_epoch: 0 }), cached: entry() })).toBe(
      "drop",
    );
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/syncPlan.test.ts`
Expected: FAIL，报模块不存在

- [ ] **Step 3: 最小实现**

```ts
/**
 * Handshake-time sync verdicts: turning the server's `syncPlan` (plus the
 * listing's version fields as a fallback) into one action per thread.
 *
 * The whole point is that the client must not *guess* whether its cache is
 * still valid: the server knows which threads it deleted and whether the
 * client's anchor still exists in its log, and says so explicitly.
 */
import type { ContentEntry } from "./contentStore";
import type { ThreadSummary } from "./protocol";

export type SyncAction = "full" | "none" | "drop";

/**
 * Parse `initialize`'s result into a per-thread verdict.
 *
 * Unknown `action` values are dropped rather than guessed at: an unrecognized
 * verdict must leave the thread on the conservative fallback path.
 */
export function parseSyncPlan(
  result: unknown,
): Map<string, { action: SyncAction; epoch: number }> {
  const out = new Map<string, { action: SyncAction; epoch: number }>();
  const raw = (result as { syncPlan?: unknown } | null | undefined)?.syncPlan;
  if (!Array.isArray(raw)) return out;
  for (const e of raw) {
    const threadId = (e as { threadId?: unknown })?.threadId;
    const action = (e as { action?: unknown })?.action;
    if (typeof threadId !== "string") continue;
    if (action !== "full" && action !== "none" && action !== "drop") continue;
    const epoch = (e as { epoch?: unknown })?.epoch;
    out.set(threadId, { action, epoch: typeof epoch === "number" ? epoch : 0 });
  }
  return out;
}

/**
 * Resolve the action for one thread. Precedence: the server's plan wins;
 * without one, the listing's version fields decide; without those, be
 * conservative and resume fully.
 *
 * The one client-side override: a *trimmed* cache entry is incomplete even
 * when the server (correctly) says `none`, so it must still be replayed in
 * full — otherwise the user silently sees a truncated conversation.
 */
export function resolveSyncPlan(opts: {
  plan: Map<string, { action: SyncAction; epoch: number }>;
  summary?: ThreadSummary | null;
  cached?: ContentEntry | null;
}): SyncAction {
  const fromPlan = opts.summary ? opts.plan.get(opts.summary.thread_id) : undefined;
  const serverEpoch =
    fromPlan?.epoch ??
    (typeof opts.summary?.content_epoch === "number" ? opts.summary.content_epoch : undefined);
  const cachedEpoch = opts.cached?.contentEpoch;

  const verdict: SyncAction =
    serverEpoch === undefined || cachedEpoch == null
      ? "full" // 无从裁量 ⇒ 保守全量（缺字段绝不当作"一致"）
      : cachedEpoch === serverEpoch
        ? "none"
        : "full";

  // 显式 drop 优先于一切：会话已不存在。
  if (fromPlan?.action === "drop") return "drop";
  if (verdict === "none" && opts.cached?.trimmed) return "full";
  return verdict;
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/syncPlan.test.ts && npx tsc --noEmit`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd .. && git add desktop/src/lib/syncPlan.ts desktop/src/lib/syncPlan.test.ts
git commit -m "feat(desktop): syncPlan 解析与裁决（含 trimmed 强制全量）"
```

---

### Task 10: 接线 plan 到握手与 `selectThread`（B 端到端）

**Files:**
- Modify: `desktop/src/App.tsx`（`handshake` `:1453`；`selectThread` `:840-960`；`prefillFromCache` `:227-244`）
- Test: `desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: Task 7、8、9
- Produces: 握手消费 plan；`prefillFromCache` 被 `epochInvalidated` 挡住；`drop` 丢弃本地状态

- [ ] **Step 1: 写失败测试**

新增用例（放进既有的 `describe("App mobile local cache")`，`:3533`；沿用其局部 `asRemote()`）：

```tsx
  it("resumes a thread the server flagged full, and drops the ghost content", async () => {
    asRemote();
    // 服务端说这个会话 epoch 变了（期间被 clear 过）。
    state.dataSources["initialize"] = () =>
      Promise.resolve({
        serverInfo: {}, protocolVersion: 1, capabilities: {},
        syncPlan: [{ threadId: "t1", action: "full", epoch: 4, reason: "epoch_changed" }],
      });
    contentStore.current = new MemoryContentStore();
    await contentStore.current.write({
      threadId: "t1", savedAt: Date.now(), lastServerItemId: "ghost", contentEpoch: 2,
      items: [{ type: "agentMessage", id: "ghost", text: "stale content" }],
      trimmed: false, bytes: 1,
    });

    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    // 幽灵内容必须被清掉，而不是留在屏上。
    await waitFor(() => expect(screen.queryByText("stale content")).toBeNull());
  });

  it("does not let a late cache read re-inject content judged stale", async () => {
    asRemote();
    state.dataSources["initialize"] = () =>
      Promise.resolve({
        serverInfo: {}, protocolVersion: 1, capabilities: {},
        syncPlan: [{ threadId: "t1", action: "full", epoch: 4, reason: "epoch_changed" }],
      });
    // 缓存读故意延迟到 adoptEpoch 之后才 resolve。
    let release!: () => void;
    const gate = new Promise<void>((r) => { release = r; });
    const real = new MemoryContentStore();
    await real.write({
      threadId: "t1", savedAt: Date.now(), lastServerItemId: "ghost", contentEpoch: 2,
      items: [{ type: "agentMessage", id: "ghost", text: "must not come back" }],
      trimmed: false, bytes: 1,
    });
    contentStore.current = {
      read: async (id: string) => { await gate; return real.read(id); },
      write: (e) => real.write(e),
      delete: (id) => real.delete(id),
      keys: () => real.keys(),
    };

    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    release();
    await act(async () => {});
    expect(screen.queryByText("must not come back")).toBeNull();
  });

  it("declares resumeHints on initialize for a remote client", async () => {
    asRemote();
    contentStore.current = new MemoryContentStore();
    await contentStore.current.write({
      threadId: "t1", savedAt: 5, lastServerItemId: "i1", contentEpoch: 3,
      items: [], trimmed: false, bytes: 1,
    });
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "initialize")).toBe(true),
    );
    // hints 从缓存元数据组装；缓存是异步读的，故轮询等待那一次 initialize 带上它。
    await waitFor(() => {
      const init = clients[0].requests.find((r) => r.method === "initialize");
      const hints = (init?.params as { resumeHints?: unknown[] } | undefined)?.resumeHints;
      expect(Array.isArray(hints)).toBe(true);
    });
  });

  it("does not send resumeHints from the desktop path", async () => {
    // 不调 asRemote()：桌面路径。
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "initialize")).toBe(true),
    );
    const init = clients[0].requests.find((r) => r.method === "initialize");
    expect((init?.params as { resumeHints?: unknown } | undefined)?.resumeHints).toBeUndefined();
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "drops the ghost content"`
Expected: FAIL，屏上仍出现 `stale content`

- [ ] **Step 3: 最小实现**

**(a)** 新增两个 ref（放在 `cachedAt` ref `:218` 附近）：

```ts
  /** 每个会话的缓存元数据（epoch / 游标 / 是否被裁剪），用于组装 hints 与裁决。 */
  const cacheMetaRef = useRef(new Map<string, ContentEntry>());
  /** 本连接的 syncPlan（`initialize` 的响应），按 threadId 索引。 */
  const syncPlanRef = useRef(new Map<string, { action: SyncAction; epoch: number }>());
```

**(b)** `handshake` 的 `initialize` 调用改为：

```ts
        const initialized = await client.request<Record<string, unknown>>("initialize", {
          ...(isRemoteClient() ? { resumeHints: buildResumeHints() } : {}),
        });
        syncPlanRef.current = parseSyncPlan(initialized);
```

`buildResumeHints`：

```ts
  /**
   * Declare what this client holds, on the very first frame (spec §4.1).
   * Only remote clients send it; the desktop path stays byte-identical.
   */
  const buildResumeHints = () => {
    const metas = [...cacheMetaRef.current.values()];
    return metas
      .sort((a, b) => b.savedAt - a.savedAt)
      .slice(0, 20)
      .map((e) => ({
        threadId: e.threadId,
        contentEpoch: e.contentEpoch ?? 0,
        lastItemId: e.lastServerItemId,
        cachedAt: e.savedAt,
      }));
  };
```

`cacheMetaRef` 的播种：内容缓存的 `read` 成功处已把 `entry` 拿到（Task 10 的 (d) 里 `cacheMetaRef.current.set(entry.threadId, entry)`）。为让 `buildResumeHints` 在**首次握手**就有料（那时还没有任何 `prefillFromCache` 跑过），在挂载时预热一次：

```ts
  // 冷启首帧：把缓存里已有的条目读进内存元数据，供首次 initialize 的 hints 使用。
  // `contentStore.keys()` 已存在；读失败静默跳过（hints 是优化，不能挡住握手）。
  useEffect(() => {
    if (!isRemoteClient()) return;
    const cs = getContentStore();
    void (async () => {
      try {
        for (const id of await cs.keys()) {
          const entry = await cs.read(id);
          if (entry) cacheMetaRef.current.set(entry.threadId, entry);
        }
      } catch {
        /* hints 是优化：读失败就按"没有缓存"声明 */
      }
    })();
  }, []);
```

**(c)** `selectThread` 在 `prefillFromCache` **之后、早退分支之前**插入裁决（顺序见 spec §6.4）：

```ts
    if (isRemoteClient()) {
      const cached = cacheMetaRef.current.get(id) ?? null;
      const view = store.peek(id);
      if (view) {
        // `summary` 取自既有的列表数据（`groups`/`pinned` 的 state）。若仓库
        // 已有按 threadId 索引的 ref，直接用它；否则用 `groups.flatMap(g => g.threads)`
        // 现算一次——本函数调用频率低（点一次会话一次），无需额外缓存。
        const summary =
          pinned.find((t) => t.thread_id === id) ??
          groups.flatMap((g) => g.threads).find((t) => t.thread_id === id) ??
          null;
        const action = resolveSyncPlan({ plan: syncPlanRef.current, summary, cached });
        if (action === "drop") {
          // 会话已不存在：丢掉本地状态与缓存，回落第一条。
          store.drop(id);
          cacheMetaRef.current.delete(id);
          void getContentStore().delete(id);
          cachedAt.current.delete(id);
          if (currentIdRef.current === id) {
            const first = pinned[0] ?? groups.flatMap((g) => g.threads)[0];
            if (first) void selectThread(first.thread_id, { leaveBoard: false });
          }
          return;
        }
        const epoch = syncPlanRef.current.get(id)?.epoch ?? summary?.content_epoch ?? 0;
        if (action === "none" && cached) {
          // 缓存有效：不必重放，但要照常跑完 selectThread 的其余副作用
          // （订阅、mode chip）——置 warm 让下方的 resume 守卫接管（spec §4.2）。
          view.session.adoptEpoch(epoch, cached.lastServerItemId);
          warm.current.add(id);
        } else {
          // full：按世代作废。epoch 取服务端的权威值（0 仅表示"未知"，用于触发作废）。
          view.session.adoptEpoch(epoch, null);
        }
      }
    }
```

**注意**：`action === "drop"` 分支里若回落第一条，要防自递归（第一条不可能是刚被 drop 的 id，因为 `store.drop` 后它已不在列表里）——但若列表为空则直接返回，不再递归。

**(d)** `prefillFromCache` 的闸门（`:232-240`）：

```ts
      .then((entry) => {
        if (!entry || entry.items.length === 0) return;
        // 记下元数据供 hints 与裁决使用（即使本次不注入内容）。
        cacheMetaRef.current.set(entry.threadId, entry);
        const v = store.peek(id);
        if (!v) return;
        // 已被判过过期（世代变化）⇒ 绝不把旧内容注回（spec §6.4）。
        if (v.session.epochInvalidated) return;
        if (v.session.items.length > 0) return; // 服务端已经填过
        v.session.upsertItems(entry.items);
        v.session.lastServerItemId = v.session.lastServerItemId ?? entry.lastServerItemId;
        cachedAt.current.set(id, entry.savedAt);
        force((n) => n + 1);
      })
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/App.test.tsx && npx tsc --noEmit`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd desktop && npx vitest run && npx tsc --noEmit
cd .. && git add desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): 握手消费 syncPlan，selectThread 按世代裁决收敛"
```

---

### Task 11: `ThreadView.syncState` 驱动解禁（A）

**Files:**
- Modify: `desktop/src/lib/threadStore.ts`（`ThreadView` `:6-30`；`create()` `:50-61`）
- Modify: `desktop/src/components/CacheNotice.tsx`（加可选 `syncing`）
- Modify: `desktop/src/App.tsx`（`selectThread` `:873` 起；`readItems` 收尾 `:882-896`；`resume` 成功/失败；`send` `:1663`；`CacheNotice` 渲染 `:2202`；删除 `:552-556` 的全局清空）
- Test: `desktop/src/lib/threadStore.test.ts`、`desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: Task 7、10
- Produces: `ThreadView.syncState: "syncing" | "synced"`；`CacheNotice({ savedAt, syncing }: { savedAt?: number | null; syncing?: boolean })`

- [ ] **Step 1: 写失败测试**

`threadStore.test.ts`：

```ts
it("defaults syncState to synced so the desktop path is unchanged", () => {
  const s = new ThreadStore();
  expect(s.view("t1").syncState).toBe("synced");
});
```

`App.test.tsx`（放进 `describe("App mobile local cache")`）：

```tsx
  it("blocks sending on a running thread until its catch-up lands", async () => {
    asRemote();
    state.listStatus = "running";
    contentStore.current = new MemoryContentStore();
    // readItems 挂住：模拟补尾部在途。
    let release!: (v: unknown) => void;
    state.dataSources["thread/readItems"] = () =>
      new Promise((resolve) => { release = resolve; });

    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/readItems")).toBe(true),
    );

    // 同步中：提示条在（文案「正在同步」），且发送被拦（不发出 turn/start）。
    await waitFor(() => expect(screen.getByText(/正在同步/)).toBeTruthy());
    const before = clients[0].requests.filter((r) => r.method === "turn/start").length;
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "hello" } });
    fireEvent.click(screen.getByRole("button", { name: /^(send|stop)$/i }));
    expect(clients[0].requests.filter((r) => r.method === "turn/start").length).toBe(before);

    // 补完 ⇒ 解禁，提示退场。
    release({ items: [{ type: "agentMessage", id: "a1", text: "done" }] });
    await waitFor(() => expect(screen.queryByText(/正在同步/)).toBeNull());
  });

  it("converges out of syncing even when the catch-up fails", async () => {
    asRemote();
    state.listStatus = "running";
    contentStore.current = new MemoryContentStore();
    state.dataSources["thread/readItems"] = () => { throw new Error("boom"); };
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/readItems")).toBe(true),
    );
    // 失败也必须收敛，否则永久无法发送（spec §11 风险 3）。
    await waitFor(() => expect(screen.queryByText(/正在同步/)).toBeNull());
  });

  it("shows no syncing notice on the desktop path", async () => {
    // 桌面：不调 asRemote()。
    state.listStatus = "running";
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    expect(screen.queryByText(/正在同步/)).toBeNull();
    expect(screen.queryByText(/离线内容/)).toBeNull();
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "until its catch-up lands"`
Expected: FAIL，无「正在同步」文案

- [ ] **Step 3: 最小实现**

**(a)** `threadStore.ts` 的 `ThreadView` 加：

```ts
  /**
   * Whether this thread's transcript has been reconciled with the server on
   * this connection. `syncing` means the catch-up (resume / readItems) is in
   * flight: the content on screen may be the cached one, so the UI must say so
   * and must not let the user send against it (spec §5).
   */
  syncState: "syncing" | "synced";
```

`create()` 里加 `syncState: "synced",`（桌面端与首次进入无需同步 ⇒ 初值即终值）。

**(b)** `selectThread` 的 remote 分支内置 `syncing`（在发起 `resume` / `readItems` 之前）：

```ts
      // 对账开始：内容随时可能是旧的，UI 与发送门槛据此收紧（spec §5.1）。
      if (view) view.syncState = "syncing";
```

注意：`action === "none"` 那条路径（Task 10 置了 `warm`）**不**置 `syncing`——它没有在途对账，缓存已被判有效。

**(c)** `readItems` 的 fire-and-forget 收尾处置 `synced`（`:882-896`，保持 fire-and-forget 形状，只加 `finally`）：

```ts
          void (async () => {
            try {
              const r = await c0.request<{ items: Item[] }>(
                "thread/readItems",
                after ? { threadId: id, afterItemId: after } : { threadId: id },
              );
              const v = store.peek(id);
              if (v) {
                v.session.upsertItems(r.items);
                force((n) => n + 1);
              }
            } catch {
              // 补齐是附加能力:读失败就保留已有内容,不改动会话状态。
            } finally {
              // 无论成败都必须收敛，否则用户会永久卡在"正在同步"（spec §11 风险 3）。
              const v = store.peek(id);
              if (v) {
                v.syncState = "synced";
                // 该会话已与服务端对过账：去掉"这是缓存"的标注（spec §5.3）。
                cachedAt.current.delete(id);
                force((n) => n + 1);
              }
            }
          })();
```

**(d)** `resume` 成功路径（`:929` `warm.current.add(id)` 附近）置 `synced` + 删 `cachedAt`；`resume` 的 `catch` 里也置 `synced`。

**(e)** 删除 `:552-556` 的全局清空 effect（清除已改为 per-thread）：

```ts
  // （删除）连上即清"内容来自缓存"的标记——已改为在该会话对账完成后按会话清除，
  // 否则另一个仍在显示缓存的会话会失去标注（spec §5.3）。
```

**(f)** `send` 门槛（`:1663`）：

```ts
    if (offline || store.peek(id)?.syncState === "syncing") {
      store.view(id).session.lastError = offline ? "正在连接，请稍候" : "正在同步，请稍候";
      force((v) => v + 1);
      return false;
    }
```

**(g)** `CacheNotice` 的签名与渲染条件。先把 `desktop/src/components/CacheNotice.tsx` 改为：

```tsx
import { formatRelativeTime } from "../lib/relativeTime";

/**
 * The strip shown above the transcript while it is not (yet) the server's
 * authoritative content. Two mutually exclusive states:
 *
 * - `syncing`: the catch-up is in flight on a live connection — the cached
 *   transcript is on screen and may be stale, so sending is blocked meanwhile.
 * - otherwise (offline): the connection is down; `savedAt` says how old the
 *   cached content is.
 *
 * Its only job is to stop a stale transcript (and its stale `running` badge)
 * from being read as the current truth.
 */
export function CacheNotice({
  savedAt,
  syncing,
}: {
  savedAt?: number | null;
  syncing?: boolean;
}) {
  return (
    <div
      role="status"
      className="border-b border-line bg-raised px-4 py-1 text-xs text-fg-muted"
    >
      {syncing ? "正在同步…" : `离线内容 · 更新于 ${formatRelativeTime(savedAt ?? 0)}`}
    </div>
  );
}
```

再改 `App.tsx` 的渲染条件（`:2202`）：

```tsx
              {currentId !== null &&
                ((offline && cachedAt.current.has(currentId)) ||
                  store.peek(currentId)?.syncState === "syncing") && (
                  <CacheNotice
                    savedAt={cachedAt.current.get(currentId) ?? null}
                    syncing={!offline && store.peek(currentId)?.syncState === "syncing"}
                  />
                )}
```

（既有的 `<CacheNotice savedAt={...!} />` 用的是非空断言；改为 `?? null` 后由 `syncing` 分支兜住 `null`。）

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/App.test.tsx src/lib/threadStore.test.ts && npx tsc --noEmit`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd desktop && npx vitest run && npx tsc --noEmit
cd .. && git add desktop/src/lib/threadStore.ts desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): syncState 让对账完成驱动解禁（A 完成）"
```

---

### Task 12: `turn/start` 携带 `contextAnchor` 与服务端校验（C）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`turn/start` 分支 `:4798`）
- Modify: `desktop/src/App.tsx`（发送带 anchor；`stale_context` 处理）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`、`desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: Task 2 的 `contains_item_id`、Task 4 的 `stale_context`
- Produces: `turn/start` 参数可选 `contextAnchor: { contentEpoch, lastItemId }`；不符时回 `-32026`

- [ ] **Step 1: 写失败测试**

Rust（`mod tests`）：

```rust
    /// 不带锚点（桌面 / 老客户端）→ 不做任何校验。这是零回归门禁。
    #[tokio::test(flavor = "multi_thread")]
    async fn turn_start_without_a_context_anchor_is_not_validated() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        // epoch 被 clear 推到 1，但客户端不带锚点 ⇒ 照常准入（服务端不做推断）。
        let cleared = clear_thread_retrying(&mut h, &tid).await;
        assert!(cleared.get("error").is_none(), "clear must succeed: {cleared}");

        let resp = rpc(
            &mut h,
            71,
            "turn/start",
            json!({ "threadId": tid, "input": [{ "type": "text", "text": "hi" }] }),
        )
        .await;
        assert!(resp.get("error").is_none(), "no anchor must mean no validation: {resp}");
        h.shutdown().await;
    }

    /// 锚点世代已失效（会话在客户端不知情时被 clear）→ 拒绝。
    #[tokio::test(flavor = "multi_thread")]
    async fn turn_start_with_a_stale_epoch_is_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        let cleared = clear_thread_retrying(&mut h, &tid).await;
        assert!(cleared.get("error").is_none(), "clear must succeed: {cleared}");

        let resp = rpc(
            &mut h,
            72,
            "turn/start",
            json!({
                "threadId": tid,
                "input": [{ "type": "text", "text": "hi" }],
                "contextAnchor": { "contentEpoch": 0 },
            }),
        )
        .await;
        assert_eq!(resp["error"]["code"], json!(-32026), "must be stale_context: {resp}");
        assert_eq!(resp["error"]["data"]["code"], json!("stale_context"));
        h.shutdown().await;
    }

    /// **关键正向用例**：锚点仍在但落后于末条 → 放行。
    /// 落后是两端并存的正常形状；拒绝它会破坏父 spec 的核心场景。
    #[tokio::test(flavor = "multi_thread")]
    async fn turn_start_with_an_anchor_behind_the_tail_is_accepted() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        run_a_turn_and_wait_for_persist(&mut h, dir.path(), &tid, "first").await;

        // 取一个早期的 item 作为"落后"的锚点。
        let read = rpc(&mut h, 73, "thread/readItems", json!({ "threadId": tid })).await;
        let early = read["result"]["items"][0]["id"]
            .as_str()
            .expect("the first turn must have items")
            .to_string();

        let resp = rpc(
            &mut h,
            74,
            "turn/start",
            json!({
                "threadId": tid,
                "input": [{ "type": "text", "text": "second" }],
                "contextAnchor": { "contentEpoch": 0, "lastItemId": early },
            }),
        )
        .await;
        assert!(
            resp.get("error").is_none(),
            "a behind-but-present anchor must pass: {resp}"
        );
        h.shutdown().await;
    }
```

前端（放进 `describe("App mobile local cache")`）：

```tsx
  it("sends a contextAnchor and does not resend after stale_context", async () => {
    asRemote();
    contentStore.current = new MemoryContentStore();
    // `turn/start` 报 stale_context（顶层方法用 dataSources 而非 rpcError，
    // 后者只在 plugin/query 的内层分支被读）。
    state.dataSources["turn/start"] = () => {
      throw { code: -32026, data: { code: "stale_context" } };
    };
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    fireEvent.change(screen.getByRole("textbox"), { target: { value: "hello" } });
    fireEvent.click(screen.getByRole("button", { name: /^(send|stop)$/i }));

    await waitFor(() => {
      const turns = clients[0].requests.filter((r) => r.method === "turn/start");
      expect(turns).toHaveLength(1);
      expect((turns[0].params as { contextAnchor?: unknown }).contextAnchor).toBeTruthy();
    });

    // 不自动重发：只提示 + 触发重同步（多一次 thread/resume）。
    await waitFor(() =>
      expect(
        clients[0].requests.filter((r) => r.method === "thread/resume").length,
      ).toBeGreaterThan(1),
    );
    expect(clients[0].requests.filter((r) => r.method === "turn/start")).toHaveLength(1);
  });

  it("does not send a contextAnchor from the desktop path", async () => {
    // 桌面：不调 asRemote()。
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "hello" } });
    fireEvent.click(screen.getByRole("button", { name: /^(send|stop)$/i }));
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(true),
    );
    const turn = clients[0].requests.find((r) => r.method === "turn/start");
    expect((turn?.params as { contextAnchor?: unknown } | undefined)?.contextAnchor).toBeUndefined();
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib turn_start_with_a_stale_epoch_is_rejected`
Expected: FAIL（当前无校验，返回成功）

- [ ] **Step 3: 最小实现**

**Rust**：在 `turn/start` 分支（`:4798`）里、`start_turn_core` 调用之前插入：

```rust
                    "turn/start" => {
                        // 客户端可携带它以为的上下文锚点（spec §7.1）。仅当携带时校验：
                        // 桌面/老客户端不带 ⇒ 逐字节零回归。
                        if let Some(anchor) = req.params.get("contextAnchor") {
                            if let Some(thread_id) =
                                req.params.get("threadId").and_then(|v| v.as_str())
                            {
                                let store = store_lookup(&threads, &workspaces, &cfg, thread_id);
                                let current_epoch = store
                                    .load(thread_id)
                                    .ok()
                                    .flatten()
                                    .map(|l| l.meta.content_epoch);
                                let claimed_epoch =
                                    anchor.get("contentEpoch").and_then(|v| v.as_u64());
                                // 世代不符 ⇒ 客户端持有的是服务端不存在的内容。
                                let epoch_stale = matches!(
                                    (claimed_epoch, current_epoch),
                                    (Some(c), Some(cur)) if c != cur
                                );
                                // 锚点已不在日志中 ⇒ 同样结构性不一致。
                                // **落后于末条不算失效**：那是两端并存的正常形状。
                                let anchor_gone = match anchor
                                    .get("lastItemId")
                                    .and_then(|v| v.as_str())
                                {
                                    Some(a) => {
                                        !store.contains_item_id(thread_id, a).unwrap_or(false)
                                    }
                                    None => false,
                                };
                                if epoch_stale || anchor_gone {
                                    write_response(
                                        &hub,
                                        &client,
                                        err_response(id.clone(), RpcError::stale_context(thread_id)),
                                    )
                                    .await?;
                                    continue;
                                }
                            }
                            // 缺 threadId：交给 `prepare_turn_core` 统一报错。
                        }
                        if start_turn_core(
```

**前端**：构造 `turn/start` 参数处（`send` 内）：

```ts
      const session = store.view(id).session;
      const contextAnchor =
        isRemoteClient() && session.contentEpoch !== null
          ? { contentEpoch: session.contentEpoch, lastItemId: session.lastServerItemId }
          : undefined;
      // 随 `turn/start` 的 params 一起发出：
      //   { threadId: id, input, ...(contextAnchor ? { contextAnchor } : {}) }
```

`stale_context` 处理（发送的 `catch` 里，优先于既有错误处理）：

```ts
      } catch (e) {
        if (
          isRemoteClient() &&
          (e as { data?: { code?: string } })?.data?.code === "stale_context"
        ) {
          // 会话内容已在别处被清空：不自动重发（用户的意图不该落到没看过的
          // 上下文上）。置 syncing、清掉两道会挡住重同步的守卫、走全量 resume。
          const v = store.peek(id);
          if (v) {
            v.syncState = "syncing";
            v.session.adoptEpoch(0, null);
          }
          warm.current.delete(id);
          inFlightResume.current.delete(id);
          store.view(id).session.lastError = "会话内容已在别处被清空，正在重新加载";
          force((n) => n + 1);
          void selectThread(id, { leaveBoard: false });
          return false;
        }
        // …既有错误处理…
      }
```

（`adoptEpoch(0, null)` 里的 `0` 只是「强制作废」的哨兵：清空后清掉 `warm` 会让 `selectThread` 走 `resume` 全量回放，回放中 `upsertItems` 会以真实世代覆盖它。）

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib turn_start_with` 与 `cd desktop && npx vitest run src/App.test.tsx -t "contextAnchor"`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cargo clippy -p yi-agent-app-server --all-targets -- -D warnings
cd ../desktop && npx vitest run && npx tsc --noEmit
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat: turn/start 携带 contextAnchor，锚点失效回 -32026（C 完成）"
```

---

### Task 13: 文档同步与全量回归

**Files:**
- Modify: `docs/superpowers/specs/2026-10-11-mobile-local-cache-design.md`（§5.C）
- Modify: `docs/project-management/yi-agent-app-server.md`（协议面）

**Interfaces:**
- Consumes: 全部前置任务
- Produces: 无代码产出

- [ ] **Step 1: 修正前置 spec 的幽灵来源**

把 `2026-10-11-mobile-local-cache-design.md` §5.C 的「（桌面端 `thread/clear` / `compact` / 或被裁剪）」改为「（桌面端 `thread/clear` / 或被裁剪）」，并在该节末尾加：

```markdown
> **修正（2026-10-17）**：`compact` **不是**幽灵来源——它落盘时 `items` 为空、
> 历史 item 分毫未动（`read_log` 跨行拼接）。真正让 item 消失的只有
> `thread/clear`。且 `clear` 后回放集为空会让本节的 `replayed.size > 0` 守卫
> 跳过清理，故收敛判据已改为按世代（`content_epoch`）。见
> `2026-10-17-client-state-negotiation-design.md` §1.2 与 §6。
```

- [ ] **Step 2: 更新协议面文档**

在 `docs/project-management/yi-agent-app-server.md` 记录：
- `initialize` 新增可选 `resumeHints` 入参、`syncPlan` 出参；
- `ThreadSummary` 新增 `content_epoch` / `last_item_id`；
- `turn/start` 新增可选 `contextAnchor` 与新错误码 `-32026 stale_context`；
- `ThreadMeta` 新增 `content_epoch`（唯一递增点：`thread/clear`）与 `last_item_id`。

- [ ] **Step 3: 全量回归（Rust）**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib && cargo test -p yi-agent-app-server`
Expected: 全绿；既有断言一条不改

- [ ] **Step 4: 全量回归（前端）**

Run: `cd desktop && npx tsc --noEmit && npx vitest run`
Expected: 全绿

- [ ] **Step 5: 提交**

```bash
git add docs/
git commit -m "docs: 同步 content_epoch 协议面，修正前置 spec 的幽灵来源

compact 不是幽灵来源（它不丢 item），只有 thread/clear 是。"
```

---

### Task 14: 真机判据（经中继的 iOS WebView）

**Files:**
- Test: 手动实测；把结果记进 `docs/bug-list.md`

**Interfaces:**
- Consumes: 全部前置任务
- Produces: 三条判据的实测结论

- [ ] **Step 1: 判据一 —— clear 后手机重连必须整段刷新**

桌面 `thread/clear` 一个会话 → 手机重连 → 断言手机屏上**不再**残留清空前的任何内容（spec §1.2 那条永不清理的缺口）。

- [ ] **Step 2: 判据二 —— running 会话重连时先见「正在同步…」**

桌面正在跑一个 turn → 手机重连该会话 → 断言：先出现「正在同步…」、此期间发送被拦；补完自动转正常、提示消失、发送放行。

- [ ] **Step 3: 判据三 —— 两端并存不产生 stale_context**

桌面与手机同时正常使用（一端跑 turn、一端发消息、交替切会话）→ 断言全程**不出现** `stale_context`（§7.2「落后放行」的现场验证）。

- [ ] **Step 4: 记录实测结果**

```bash
git add docs/bug-list.md
git commit -m "test: 客户端状态协商真机三判据实测"
```

---

## Self-Review

**Spec coverage：**

| Spec 章节 | 任务 |
| --- | --- |
| §3 `content_epoch` 字段 + 唯一递增点 + 不刷 `updated_at` | Task 1、2、3 |
| §3 `last_item_id` 冗余 + `promote_partial` 不维护的缓解 | Task 1、2、5 |
| §4.1 `resumeHints`（上限 20、按 cachedAt） | Task 6、10 |
| §4.2 裁决表（含「none ≠ 什么都不做」） | Task 5、6、10 |
| §4.2 `trimmed` 强制全量 | Task 9 |
| §4.2 `drop` 作用于当前选中会话 | Task 10 |
| §4.3 `listAll` 字段 + 缺失语义 | Task 5、8、9 |
| §4.4 YAGNI（不做全局 revision / 不推送 / 不新增 RPC） | 明确不做（无任务） |
| §5.1 `syncState` + 三处置 `synced`（含失败路径） | Task 11 |
| §5.2 提示条与发送门槛按**当前会话** | Task 11 |
| §5.3 `cachedAt` 按会话清除 | Task 11 |
| §6.1 `adoptEpoch` | Task 7 |
| §6.2 `dropStale` 守卫保留 | Task 10（守卫未删，仍在 `:924`） |
| §6.3 running 路径也 `adoptEpoch`，且顺序在 `readItems` 之前 | Task 10（裁决在 `selectThread` 同步段） |
| §6.4 `prefillFromCache` 复活竞态（闸门 + 顺序） | Task 7（`epochInvalidated`）、Task 10 |
| §6.5 修正前置 spec | Task 13 |
| §7.1 `contextAnchor` | Task 12 |
| §7.2 四条判定（落后放行） | Task 12 |
| §7.3 客户端处理（清两道守卫、不重发） | Task 12 |
| §7.4 不需校验的操作 | 明确不做（无任务） |
| §8 协议变更汇总 | Task 1-6、8、12、13 |
| §11 风险 1（`PROTOCOL_VERSION` 保持 1） | Task 13（文档记录） |
| §12 分期 | 任务顺序即分期 |

**§10 测试 1-18 对号**：1→Task 3；2→Task 5（`plan_sync` 的 `t-behind` + Task 3 注释锁定的意图；若需可执行锁定，在 Task 3 Step 1 追加 `compacting_a_thread_does_not_bump_its_content_epoch`，断言 `thread/compact` 后 meta 的 `content_epoch` 不变——`run_a_turn_and_wait_for_persist` + `ThreadStore::new(dir.path())` 即可写）；3-7→Task 5（纯函数）/ Task 6（端到端）；8→Task 5；9-11→Task 12；12→Task 7；13→Task 11；14→Task 11（桌面回归）；15→Task 9；16→Task 10；17→Task 5（`t-behind`）；18→Task 9。

**Placeholder scan**：无 TBD/TODO；每个代码步骤含可粘贴代码。三处标注需与仓库既有结构对齐（`state.remote` 开关名、`sendMessageViaUi` 发送助手名、`summaryRef`/`groupsRef` 是否已存在）——这些是**既有设施的引用**，实现者须先 `grep` 确认真实名称；这是本计划唯一类型的不确定性，且已逐处标注。

**Type consistency**：Rust `SyncAction`（enum）与前端 `SyncAction`（字符串联合）是两套独立类型；`plan_sync` 返回四元组 `(String, SyncAction, &'static str, u64)` 在 Task 5 定义、Task 6 消费一致；`parse_resume_hints` 在 Task 6 定义并自带测试；`adoptEpoch(epoch, lastItemId)` 在 Task 7 定义、Task 10/12 调用一致；`resolveSyncPlan({ plan, summary, cached })` 在 Task 9 定义、Task 10 调用一致；`bump_content_epoch` / `contains_item_id` / `set_last_item_id` 在 Task 2 定义、Task 3/5/12 调用一致；`run_a_turn_and_wait_for_persist` 在 Task 3 定义、Task 5/6/12 复用一致。