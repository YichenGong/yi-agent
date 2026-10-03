# 会话回放不丢帧 + 超长会话冷开优化 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 消除 `thread/resume` 回放时的静默丢帧（长会话重启后丢尾部），并把历史下发改为分块批量帧以加速超长会话冷开。

**Architecture:** 回放期把历史 items 按「字节预算 + 条数上限」切块，每块打成一条新的 `items/completed` 通知，经新增的 `Broadcaster::broadcast_await` 投递——对本地 reliable 客户端 `await` 入队（背压，不丢帧），对 ws 客户端维持既有 `try_send` 语义。实时流与 `thread/readItems` 不变；客户端复用已有的 `upsertItems` 消费批量帧。

**Tech Stack:** Rust（tokio / serde）后端 `yi-agent-app-server`；TypeScript + React + vitest 前端 `desktop`。

## Global Constraints

- 单帧必须 **远小于** `MAX_FRAME_BYTES = 1 MiB`（`protocol.rs:8`，读侧 `ws.rs:461` / `transport.rs:41` 强制）。分块用 256 KiB 软预算 + 200 条上限。
- **不改** `.jsonl` / `.meta.json` 格式；**不改** `Item` 结构。
- **实时流**（`item/started` / `item/completed` / `item/delta`）逐字节不变，仍走 `broadcast_for`（`try_send`）。
- **`thread/readItems`** 不动。
- 回放的背压**只**对 reliable 客户端（stdio `local`）生效；非 reliable（ws）保持「满则丢帧/摘除」。
- 回放中 `broadcast_await` 返回 `Err`（本地客户端真死）时：记 stderr、**中止本轮回放**；`resume` 仍成功返回，不让整个 resume 报错。
- 工具链：`export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`；Rust 命令在 `yi-agent-rs` 下跑；`cargo test` 只接受一个 positional filter，多个用例写 `-- <f1> <f2>`。
- 前端命令在 `desktop` 下跑：`npm test`（vitest）、`npx tsc --noEmit`。

---

### Task 1: 协议新增回放批量通知 `items/completed`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs` (`Notification` enum，约 157-268 行；`thread_key()` 约 268-296 行)
- Test: 同文件 `#[cfg(test)] mod tests`

**Interfaces:**
- Produces: `Notification::ItemsCompleted { thread_id: String, items: Vec<Item> }`，序列化为 `{"method":"items/completed","params":{"thread_id":..,"items":[..]}}`；`thread_key()` 对其返回 `Some(thread_id)`；`delivery()` 归 `Content`（默认分支已覆盖）。

- [ ] **Step 1: 写失败测试**

在 `protocol.rs` 的 `mod tests` 内新增：

```rust
#[test]
fn items_completed_serializes_with_method_and_params() {
    let n = Notification::ItemsCompleted {
        thread_id: "t1".to_string(),
        items: vec![Item::UserMessage {
            id: "user-1".to_string(),
            text: "hi".to_string(),
        }],
    };
    let v = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
    assert_eq!(v["method"], serde_json::json!("items/completed"));
    assert_eq!(v["params"]["thread_id"], serde_json::json!("t1"));
    assert_eq!(v["params"]["items"][0]["type"], serde_json::json!("userMessage"));
    assert_eq!(n.thread_key(), Some("t1"));
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cargo test -p yi-agent-app-server --lib -- items_completed_serializes`
Expected: 编译失败 / 无 `ItemsCompleted` 变体。

- [ ] **Step 3: 实现**

在 `Notification` 枚举中（`ItemCompleted` 之后）加入：

```rust
    /// 回放期批量下发历史 item：一次一帧，替代逐条 `item/completed`。
    /// 仅在 `thread/resume` 的历史回放期使用；实时流不带此方法。
    #[serde(rename = "items/completed")]
    ItemsCompleted { thread_id: String, items: Vec<Item> },
```

在 `thread_key()` 的 `Some(thread_id)` 匹配臂里追加：

```rust
            | Notification::ItemsCompleted { thread_id, .. }
```

（`delivery()` 的 `_ => Delivery::Content` 已覆盖新变体，无需改动。）

- [ ] **Step 4: 运行测试，确认通过**

Run: `cargo test -p yi-agent-app-server --lib -- items_completed_serializes`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs
git commit -m "feat(app-server): add replay-only items/completed notification"
```

---

### Task 2: `Broadcaster::broadcast_await`（对 reliable 客户端背压）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/broadcast.rs`（`impl Broadcaster`，`broadcast_for` 之后；tests 模块）
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: `ClientId`、`Closed`、`CLIENT_QUEUE`。
- Produces: `pub async fn broadcast_await(&self, key: Option<&str>, frame: Value) -> Result<(), Closed>`。对 `reliable` 客户端 `tx.send(frame).await`（队列满则等待，不丢帧）；对非 reliable 客户端 `tx.try_send`，失败则从注册表移除（保留既有 ws 语义）；任一 reliable 客户端队列关闭 → 返回 `Err(Closed)`。

- [ ] **Step 1: 写失败测试**

在 `broadcast.rs` 的 `mod tests` 内新增：

```rust
#[tokio::test]
async fn broadcast_await_does_not_drop_frames_for_a_reliable_client() {
    let hub = Broadcaster::new();
    let id = ClientId::local();
    // reliable:队列满时必须背压等待,而不是丢帧。
    let mut rx = hub.register_reliable(id);
    let total = CLIENT_QUEUE + 32;
    let hub2 = Arc::new(hub);
    let h = Arc::clone(&hub2);
    // 边发边收另一路:把发送放到任务里,主测线程持续收,模拟 pump 抽干队列。
    let sender = tokio::spawn(async move {
        for i in 0..total {
            h.broadcast_await(None, serde_json::json!({ "n": i })).await.unwrap();
        }
    });
    let mut got = Vec::new();
    for _ in 0..total {
        got.push(rx.recv().await.unwrap()["n"].as_u64().unwrap());
    }
    sender.await.unwrap();
    assert_eq!(got.len(), total, "reliable client must receive every frame");
    assert_eq!(got[0], 0);
    assert_eq!(*got.last().unwrap(), (total - 1) as u64);
}
```

（若模块内未 `use std::sync::Arc;`，在测试模块顶部加 `use std::sync::Arc;`。）

- [ ] **Step 2: 运行测试，确认失败**

Run: `cargo test -p yi-agent-app-server --lib -- broadcast_await_does_not_drop`
Expected: 编译失败 / 无 `broadcast_await`。

- [ ] **Step 3: 实现**

在 `impl Broadcaster` 中 `broadcast_for` 之后加入：

```rust
    /// 按 thread 键**带背压**扇出：reliable 客户端 `await` 入队（队列满即等，
    /// 不丢帧）；非 reliable 客户端维持 `try_send`，满则从注册表移除（ws 既有语义）。
    ///
    /// 只用于历史回放等「必须完整送达本地客户端」的批量下发；实时流仍用
    /// [`Self::broadcast_for`]。
    ///
    /// 返回 `Err(Closed)` 表示有 reliable 客户端的出站队列已关闭（对端真死）；
    /// 已投递的帧不受影响。持锁阶段只克隆 `(id, reliable, tx)`，不跨 `.await`。
    pub async fn broadcast_await(&self, key: Option<&str>, frame: Value) -> Result<(), Closed> {
        let targets: Vec<(ClientId, bool, mpsc::Sender<Value>)> = {
            let guard = self.clients.lock().unwrap_or_else(|p| p.into_inner());
            guard
                .iter()
                .filter(|(_, c)| c.feed.accepts(key))
                .map(|(id, c)| (id.clone(), c.reliable, c.tx.clone()))
                .collect()
        };
        let mut closed = false;
        let mut to_remove: Vec<ClientId> = Vec::new();
        for (id, reliable, tx) in targets {
            if reliable {
                if tx.send(frame.clone()).await.is_err() {
                    closed = true;
                }
            } else if tx.try_send(frame.clone()).is_err() {
                to_remove.push(id);
            }
        }
        if !to_remove.is_empty() {
            let mut guard = self.clients.lock().unwrap_or_else(|p| p.into_inner());
            for id in to_remove {
                guard.remove(&id);
            }
        }
        if closed {
            Err(Closed)
        } else {
            Ok(())
        }
    }
```

- [ ] **Step 4: 运行测试，确认通过 + 既有 broadcast 测试仍绿**

Run: `cargo test -p yi-agent-app-server --lib -- broadcast`
Expected: 全部 PASS（含新增与既有的 `a_slow_consumer_is_dropped_without_blocking_others`、`a_reliable_client_keeps_its_registration_when_its_queue_is_full`）。

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/broadcast.rs
git commit -m "feat(app-server): add backpressured broadcast_await for replay"
```

---

### Task 3: 回放分块器（纯函数）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（在 `write_notification` 附近新增常量与函数，约 4171 行）
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: `crate::protocol::Item`。
- Produces:
  - `const REPLAY_CHUNK_BYTES: usize = 256 * 1024;`
  - `const REPLAY_CHUNK_MAX_ITEMS: usize = 200;`
  - `fn chunk_items_for_replay(items: Vec<crate::protocol::Item>) -> Vec<Vec<crate::protocol::Item>>`：顺序保持；每块满足「累计序列化字节 > REPLAY_CHUNK_BYTES」或「条数 >= REPLAY_CHUNK_MAX_ITEMS」即切；单条自身超预算时单独成块。

- [ ] **Step 1: 写失败测试**

在 `server.rs` 的测试模块内新增：

```rust
#[test]
fn chunk_items_for_replay_preserves_order_and_splits_by_count() {
    use crate::protocol::Item;
    let items: Vec<Item> = (0..(REPLAY_CHUNK_MAX_ITEMS * 2 + 5))
        .map(|i| Item::AgentMessage {
            id: format!("i-{i}"),
            text: "x".to_string(),
        })
        .collect();
    let chunks = chunk_items_for_replay(items);
    assert_eq!(chunks.len(), 3, "200*2+5 must split into 3 chunks");
    assert_eq!(chunks[0].len(), REPLAY_CHUNK_MAX_ITEMS);
    assert_eq!(chunks[1].len(), REPLAY_CHUNK_MAX_ITEMS);
    assert_eq!(chunks[2].len(), 5);
    // 顺序保持：展平后 id 与原始一致。
    let flat: Vec<String> = chunks
        .iter()
        .flatten()
        .map(|it| match it {
            Item::AgentMessage { id, .. } => id.clone(),
            _ => unreachable!(),
        })
        .collect();
    for (i, id) in flat.iter().enumerate() {
        assert_eq!(id, &format!("i-{i}"));
    }
}

#[test]
fn chunk_items_for_replay_splits_by_bytes() {
    use crate::protocol::Item;
    // 每条 ~64KiB 文本;预算 256KiB → 每块约 4 条。
    let big = "y".repeat(64 * 1024);
    let items: Vec<Item> = (0..10)
        .map(|i| Item::AgentMessage { id: format!("b-{i}"), text: big.clone() })
        .collect();
    let chunks = chunk_items_for_replay(items);
    assert!(chunks.len() >= 3, "byte budget must force multiple chunks");
    for c in &chunks {
        let bytes: usize = c
            .iter()
            .map(|it| serde_json::to_vec(it).unwrap().len())
            .sum();
        // 允许「最后一条超预算」的余量,但每块仍须远小于 1MiB 硬上限。
        assert!(bytes < 900 * 1024, "chunk must stay under the frame limit");
    }
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cargo test -p yi-agent-app-server --lib -- chunk_items_for_replay`
Expected: 编译失败 / 无该函数。

- [ ] **Step 3: 实现**

在 `server.rs` 中 `write_notification` 函数附近加入：

```rust
/// 回放分块的字节软预算（256 KiB）。单帧序列化后须**远小于**
/// `MAX_FRAME_BYTES = 1 MiB`,故留足余量。
const REPLAY_CHUNK_BYTES: usize = 256 * 1024;
/// 回放分块的条数上限，兜住「海量极小 item」把帧数压不下来的极端。
const REPLAY_CHUNK_MAX_ITEMS: usize = 200;

/// 把历史 items 切成回放帧的块：顺序保持；累计序列化字节超过
/// [`REPLAY_CHUNK_BYTES`] 或条数达到 [`REPLAY_CHUNK_MAX_ITEMS`] 即切块。
/// 单条自身超预算时它单独成块（容积为 1），不在此函数内再切分。
fn chunk_items_for_replay(
    items: Vec<crate::protocol::Item>,
) -> Vec<Vec<crate::protocol::Item>> {
    let mut chunks: Vec<Vec<crate::protocol::Item>> = Vec::new();
    let mut cur: Vec<crate::protocol::Item> = Vec::new();
    let mut cur_bytes = 0usize;
    for item in items {
        let approx = serde_json::to_vec(&item).map(|v| v.len()).unwrap_or(64);
        // 字节预算：当前块非空且再加一条会超预算 → 先结块。
        if !cur.is_empty() && cur_bytes + approx > REPLAY_CHUNK_BYTES {
            chunks.push(std::mem::take(&mut cur));
            cur_bytes = 0;
        }
        cur.push(item);
        cur_bytes += approx;
        // 条数上限：达到上限即结块（与字节预算互为兜底）。
        if cur.len() >= REPLAY_CHUNK_MAX_ITEMS {
            chunks.push(std::mem::take(&mut cur));
            cur_bytes = 0;
        }
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}
```

- [ ] **Step 4: 运行测试，确认通过**

Run: `cargo test -p yi-agent-app-server --lib -- chunk_items_for_replay`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): add replay chunker by byte budget and count"
```

---

### Task 4: 回放改走批量帧 + 背压（含既有 resume 测试更新与回归测试）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
  - 新增 `write_notification_await`（`write_notification` 之后，约 4171 行）
  - `thread/resume` 回放循环（约 2768-2795 行）
  - 更新既有 resume 测试：`server.rs:10489`、`10584`、`10801`、`10941`（及必要时 `11210/11475/11731/12184`）
  - 新增回归测试 `resume_replays_every_item_for_long_threads`
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: `Notification::ItemsCompleted`（Task 1）、`Broadcaster::broadcast_await`（Task 2）、`chunk_items_for_replay`（Task 3）。
- Produces: 回放期改为「`thread/started` → 若干 `items/completed` → usage →（可选）`turn/completed{Interrupted}` → 响应」；新增测试辅助 `collect_replayed_items`。

- [ ] **Step 1: 写新回归测试（失败）**

在测试模块内新增（放在既有 resume 测试附近）：

```rust
/// 长 thread 的 resume 必须回放**全部** items：这是「重启后长会话丢尾部」的
/// 回归测试。旧实现逐条 try_send,超过 256 格队列即静默丢帧,本测试必失败。
#[tokio::test(flavor = "multi_thread")]
async fn resume_replays_every_item_for_long_threads() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    // 直接向 store 落一轮含 600 个 item 的历史(远超 CLIENT_QUEUE=256)。
    let store = crate::thread_store::ThreadStore::new(dir.path());
    let total = 600usize;
    let items: Vec<crate::protocol::Item> = (0..total)
        .map(|i| crate::protocol::Item::AgentMessage {
            id: format!("item-{i}"),
            text: format!("m{i}"),
        })
        .collect();
    store
        .append_turn(
            &tid,
            &crate::thread_store::TurnLine::Turn {
                items: items.clone(),
                usage: None,
                messages: Vec::new(),
            },
        )
        .unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;

    let replayed = collect_replayed_items(&mut h, 4).await;
    let replayed_ids: std::collections::HashSet<&str> =
        replayed.iter().filter_map(|it| item_id(it)).collect();
    let expected_ids: std::collections::HashSet<String> =
        items.iter().filter_map(item_id_owned).collect();
    let mut missing: Vec<&String> = expected_ids
        .iter()
        .filter(|id| !replayed_ids.contains(id.as_str()))
        .collect();
    missing.sort();
    assert!(
        missing.is_empty(),
        "resume must replay every item; {} missing (e.g. {:?})",
        missing.len(),
        &missing.iter().take(5).collect::<Vec<_>>()
    );
    h.shutdown().await;
}

fn item_id_owned(it: &crate::protocol::Item) -> Option<String> {
    item_id(it).map(|s| s.to_string())
}
```

同时新增两个读取辅助（放在 `read_response` 附近）：

```rust
/// 把一帧回放通知展平为其携带的 items：**兼容**逐条 `item/completed` 与批量
/// `items/completed`；非 item 帧返回空 vec。既有 resume 测试都用它改造。
fn replayed_items_of(v: &serde_json::Value) -> Vec<crate::protocol::Item> {
    match v.get("method").and_then(|m| m.as_str()) {
        Some("item/completed") => {
            vec![serde_json::from_value(v["params"]["item"].clone()).unwrap()]
        }
        Some("items/completed") => v["params"]["items"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|it| serde_json::from_value(it.clone()).unwrap())
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// 读到 id==want 的响应为止,收集期间所有回放 item（经 `replayed_items_of`）。
async fn collect_replayed_items(h: &mut Harness, want: u64) -> Vec<crate::protocol::Item> {
    let mut out = Vec::new();
    for _ in 0..4096 {
        let v = h.read_value().await;
        out.extend(replayed_items_of(&v));
        if v.get("id") == Some(&serde_json::json!(want)) {
            break;
        }
    }
    out
}
```

（`item_id` 已存在于模块内；`Harness::shutdown` 已存在。）

- [ ] **Step 2: 运行测试，确认失败**

Run: `cargo test -p yi-agent-app-server --lib -- resume_replays_every_item_for_long_threads`
Expected: FAIL —— 断言 missing 非空（旧实现丢帧）。这正是实测 bug 的自动化复现。

- [ ] **Step 3: 实现 `write_notification_await`**

在 `write_notification`（约 4171 行）之后加入：

```rust
/// 与 [`write_notification`] 同款序列化与投递键，但走**带背压**的
/// [`crate::broadcast::Broadcaster::broadcast_await`]：本地 reliable 客户端队列
/// 满时等待而非丢帧。用于历史回放这类「必须完整送达」的批量下发。
async fn write_notification_await(
    hub: &crate::broadcast::Broadcaster,
    n: &Notification,
) -> anyhow::Result<()> {
    let frame = serde_json::to_value(NotificationEnvelope::new(n))
        .map_err(|e| anyhow::anyhow!("failed to serialize notification: {e}"))?;
    let key = match n.delivery() {
        crate::protocol::Delivery::Content => n.thread_key(),
        crate::protocol::Delivery::List | crate::protocol::Delivery::Global => None,
    };
    hub.broadcast_await(key, frame)
        .await
        .map_err(|_| anyhow::anyhow!("replay client closed"))?;
    Ok(())
}
```

- [ ] **Step 4: 改造回放循环**

把 `thread/resume` 中（约 2776-2786）的：

```rust
                        for item in loaded.items {
                            write_notification(&hub, &Notification::ItemCompleted {
                                    thread_id: thread_id.clone(),
                                    item,
                                },
                            )
                            .await?;
                        }
```

替换为：

```rust
                        // 回放：分块批量下发（每块一条 `items/completed`），经带背压的
                        // 投递——本地 reliable 客户端队列满时等待而非丢帧。旧实现逐条
                        // try_send,超过 256 格队列即静默丢帧,长会话尾部就此消失。
                        for chunk in chunk_items_for_replay(loaded.items) {
                            if let Err(e) = write_notification_await(
                                &hub,
                                &Notification::ItemsCompleted {
                                    thread_id: thread_id.clone(),
                                    items: chunk,
                                },
                            )
                            .await
                            {
                                eprintln!(
                                    "[app-server] replay aborted for {thread_id}: {e}"
                                );
                                break;
                            }
                        }
```

`thread/started`、usage、`turn/completed{Interrupted}`、响应各帧**不变**。

- [ ] **Step 5: 更新既有 resume 测试以观察批量帧**

原实现逐条发 `item/completed`；改造后回放发批量 `items/completed`。用
`replayed_items_of(&v)`（同时识别两种 method）替换这些测试里逐条匹配
`item/completed` 的写法。各处具体改法：

- **crash partial 回放 `saw_half`（约 10497 与 10593 两处）**：把

  ```rust
  if v.get("method").and_then(|m| m.as_str()) == Some("item/completed")
      && v["params"]["item"]["text"] == "half"
  {
      saw_half = true;
  }
  ```

  替换为

  ```rust
  if replayed_items_of(&v)
      .iter()
      .any(|it| item_id(it).is_some_and(|_| true) && matches!(it, crate::protocol::Item::AgentMessage { text, .. } if text == "half"))
  {
      saw_half = true;
  }
  ```

  （`Item::AgentMessage { text, .. }` 的模式匹配要求模块内已 `use` 或全路径；
  若不便，等价写法：`replayed_items_of(&v).iter().any(|it| matches!(it, crate::protocol::Item::AgentMessage { text, .. } if text == "half"))`。）

- **`thread_resume_replays_history_and_restores_context`（约 10804-10819）**：把
  收集循环里 `Some("item/completed") => { replayed.push(...); item_ids.push(...) }`
  改为对每帧先 `for it in replayed_items_of(&v) { replayed.push(item_type(&it));
  item_ids.push(item_id(&it).unwrap().to_string()); }`，其中
  `item_type`/`item_id` 可复用（`item_id` 已有；type 用 `match` 或
  `serde_json::to_value(it)["type"]`）。断言语义不变。

- **`/clear` 后 resume 不得回放（约 10944-10955）**：把

  ```rust
  if v.get("method").and_then(|m| m.as_str()) == Some("item/completed") {
      replayed.push(v["params"]["item"]["text"].as_str().unwrap_or("").to_string());
  }
  ```

  替换为

  ```rust
  for it in replayed_items_of(&v) {
      if let crate::protocol::Item::UserMessage { text, .. }
      | crate::protocol::Item::AgentMessage { text, .. } = it
      {
          replayed.push(text);
      }
  }
  ```

  断言不变。

- **其余引用 resume 的测试（11210 / 11475 / 11731 / 12184）**：仅断言「有响应 /
  无错误」者无需改动；若断言了回放 item，同样并入 `replayed_items_of`。

- [ ] **Step 6: 运行相关测试，确认通过**

Run: `cargo test -p yi-agent-app-server --lib -- resume_replays_every_item_for_long_threads thread_resume_replays_history resume crashed`
Expected: 全部 PASS。

- [ ] **Step 7: 运行整个 crate 的 lib 测试**

Run: `cargo test -p yi-agent-app-server --lib`
Expected: 全绿（无因回放协议变化而失败的用例）。

- [ ] **Step 8: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "fix(app-server): replay history in batched frames with backpressure

Replaces the per-item try_send replay loop that silently dropped frames once
the 256-deep reliable-client queue filled, truncating long conversations on
reload. Items are now chunked into items/completed frames delivered with
backpressure, so the local client loses nothing."
```

---

### Task 5: 前端消费 `items/completed`

**Files:**
- Modify: `desktop/src/lib/protocol.ts`（`Notification` 联合类型，约 51-97 行）
- Modify: `desktop/src/lib/session.ts`（`apply()` 的 switch，约 195-260 行）
- Test: `desktop/src/lib/session.test.ts`

**Interfaces:**
- Consumes: 服务端 `items/completed` 帧；既有 `Session.upsertItems(items: Item[])`。
- Produces: `Session.apply` 对 `items/completed` 调 `upsertItems`。

- [ ] **Step 1: 写失败测试**

在 `session.test.ts` 内新增：

```ts
it("merges a batched items/completed replay", () => {
  const s = new Session();
  s.apply({
    method: "items/completed",
    params: {
      thread_id: "t1",
      items: [
        { type: "userMessage", id: "user-1", text: "hi" },
        { type: "agentMessage", id: "item-1", text: "hello" },
      ],
    },
  } as never);
  expect(s.items.map((i) => i.id)).toEqual(["user-1", "item-1"]);
});

it("batched replay does not duplicate an already-seen item", () => {
  const s = new Session();
  s.apply({
    method: "item/completed",
    params: { thread_id: "t1", item: { type: "agentMessage", id: "item-1", text: "a" } },
  } as never);
  s.apply({
    method: "items/completed",
    params: {
      thread_id: "t1",
      items: [
        { type: "agentMessage", id: "item-1", text: "a" },
        { type: "agentMessage", id: "item-2", text: "b" },
      ],
    },
  } as never);
  expect(s.items.filter((i) => i.id === "item-1")).toHaveLength(1);
  expect(s.items.map((i) => i.id)).toEqual(["item-1", "item-2"]);
});
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `npm test -- session`（在 `desktop` 下）
Expected: FAIL —— `items/completed` 未被处理，items 为空。

- [ ] **Step 3: 改 `protocol.ts`**

在 `Notification` 联合类型中（`item/completed` 行之后）加：

```ts
  | { method: "items/completed"; params: { thread_id: string; items: Item[] } }
```

- [ ] **Step 4: 改 `session.ts`**

在 `apply()` 的 `switch` 中，`case "item/completed"` 分支之后新增：

```ts
      case "items/completed": {
        // 回放期的批量帧：交给 upsertItems（按 id 去重/就地替换/保持顺序/
        // 推进 lastServerItemId），与逐条 item/completed 等价且幂等。
        this.upsertItems(notification.params.items);
        break;
      }
```

- [ ] **Step 5: 运行测试，确认通过**

Run: `npm test -- session`
Expected: PASS（含新增与既有 session 用例）。

- [ ] **Step 6: 类型检查**

Run: `npx tsc --noEmit`
Expected: 无错误。

- [ ] **Step 7: 提交**

```bash
git add desktop/src/lib/protocol.ts desktop/src/lib/session.ts desktop/src/lib/session.test.ts
git commit -m "feat(desktop): consume batched items/completed replay frames"
```

---

### Task 6: 真实二进制端到端验证 + 文档

**Files:**
- Modify: `docs/project-management/yi-agent-app-server.md`（功能条目）
- Modify: `docs/superpowers/specs/2026-10-03-resume-replay-lossless-design.md`（状态改为「已实现」）

**Interfaces:**
- Consumes: 已构建的 `target/debug/yi-agent` 二进制；真实 `.yi-agent/threads/*.jsonl`。

- [ ] **Step 1: 构建带修复的二进制**

```bash
cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cargo build -p yi-agent
```

- [ ] **Step 2: 写临时探针脚本**

创建 `/tmp/resume_probe.py`（**不入库**；仅在仓库根目录运行）：

```python
import json, subprocess, threading, os, sys, time, glob

BIN = sys.argv[1] if len(sys.argv) > 1 else "yi-agent-rs/target/debug/yi-agent"
WD = os.getcwd()

def probe(target, wait=10.0):
    env = dict(os.environ); env["YI_AGENT_WORKDIR"] = WD
    p = subprocess.Popen([BIN, "app-server", "--listen", "stdio://"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        env=env, cwd=WD, text=True, bufsize=1)
    notifs = []
    def rd():
        for line in p.stdout:
            line = line.strip()
            if line:
                try: notifs.append(json.loads(line))
                except Exception: pass
    threading.Thread(target=rd, daemon=True).start()
    def send(o): p.stdin.write(json.dumps(o) + "\n"); p.stdin.flush()
    send({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}); time.sleep(0.8)
    send({"jsonrpc":"2.0","id":2,"method":"thread/resume","params":{"threadId":target}})
    time.sleep(wait)
    p.terminate()
    try: p.wait(timeout=3)
    except Exception: p.kill()
    got = []
    for o in notifs:
        if o.get("params", {}).get("thread_id") != target: continue
        m = o.get("method")
        if m == "item/completed": got.append(o["params"]["item"].get("id"))
        elif m == "items/completed":
            got += [it.get("id") for it in o["params"]["items"]]
    return got

def jsonl_ids(target):
    out = []
    for line in open(f"{WD}/.yi-agent/threads/{target}.jsonl"):
        line = line.strip()
        if not line: continue
        o = json.loads(line)
        if o.get("type") == "turn":
            out += [it.get("id") for it in o.get("items", [])]
    return out

targets = sys.argv[2:] or [os.path.basename(p)[:-6]
                           for p in sorted(glob.glob(f"{WD}/.yi-agent/threads/*.jsonl"))]
for tid in targets:
    exp = jsonl_ids(tid); got = probe(tid)
    missing = [x for x in exp if x not in set(got)]
    print(f"{tid}: jsonl={len(exp)} replayed={len(got)} missing={len(missing)}"
          + (f" e.g. {missing[:3]}" if missing else ""))
```

- [ ] **Step 3: 对**不同长度**的 thread 验证回放完整**

```bash
# 从仓库根目录
for t in thread-1e0ef9db-a460-42bd-95da-758d59a90a9c \
         thread-2daebf29-098a-4438-bfa0-82486d10ef28 \
         thread-36ebf486-b0d0-47ec-a05e-efea0fd8e122 \
         thread-a414ddb8-383c-4e27-baf4-e9909600bfe4; do
  python3 /tmp/resume_probe.py yi-agent-rs/target/debug/yi-agent "$t"
done
```

Expected: 每一行 `missing=0`。修复前的对照值（用已安装的旧二进制测得）：
`a414ddb8: jsonl=1463 replayed=320 missing=1143`、`36ebf486: jsonl=539 replayed=278 missing=261`。

- [ ] **Step 4: 记录验证证据**

把上一步的命令与输出（最好含一张「修复前 vs 修复后」对照）写进任务报告 / ledger。

- [ ] **Step 5: 更新文档**

- `docs/project-management/yi-agent-app-server.md`：追加一条功能说明（回放分块 + 背压，修复长会话重载丢尾部）。
- spec 文件状态行改为：`状态：已实现`。

- [ ] **Step 6: 提交**

```bash
git add docs/project-management/yi-agent-app-server.md docs/superpowers/specs/2026-10-03-resume-replay-lossless-design.md
git commit -m "docs: record lossless resume replay fix and verify against real binary"
```

---

## 自审（写完计划后对照 spec）

- **Spec 覆盖**：协议新变体→Task 1；背压扇出→Task 2；分块器→Task 3；回放循环改造→Task 4；客户端消费→Task 5；实测验证→Task 6。覆盖完整。
- **占位符**：无 TBD/TODO；每步含可执行代码或明确命令。
- **类型一致**：`ItemsCompleted`（Task 1）在 Task 4/5 一致使用；`broadcast_await`（Task 2）签名与 Task 4 调用一致；`chunk_items_for_replay`（Task 3）入参/返回值与 Task 4 一致；前端 `items/completed` 形状（Task 5）与服务端序列化（Task 1）一致。
- **风险点已显式化**：既有 6 处 resume 测试需观察两种 method（Task 4 Step 5）；单条超 1 MiB 属既有边界，不在本次范围。
