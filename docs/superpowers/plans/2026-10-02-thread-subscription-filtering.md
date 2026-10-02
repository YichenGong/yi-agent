# 按会话订阅过滤（S1）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让客户端能"只收我在意的会话"，并在已订阅时把逐字流合并，减少外网手机的流量与噪音。

**Architecture:** `Broadcaster` 给每个客户端加一份 `Feed`（默认 `All`＝全收，`Only(set)`＝只收集合内 thread）。通知经 `broadcast_for(thread_key, frame)` 路由：内容通知带 `thread_id`，全局帧（主题/错误/审批已处理）带 `None` 恒放行。逐字流 `item/delta` 在**有订阅者时**按 thread 攒批再发（本地缓冲 + 100ms tick + 非 delta 屏障 flush），无订阅者时逐字节不变。

**Tech Stack:** Rust（edition 2024）、tokio、serde_json、axum/tokio-tungstenite（ws）；测试 `cargo test`。

## Global Constraints

- 严禁在 `main` 直接提交；worktree + 分支 → 改 → 测试 → commit → `git merge --no-ff` → 清理。
- conventional commits，**不写 `Co-Authored-By`**。
- 提交前只对**改动文件**跑 rustfmt：`rustfmt --edition 2024 --check --config skip_children=true <file>`；**不要把 repo 级既有 rustfmt 漂移扫进 commit**。
- 门禁：`cargo test -p yi-agent-app-server` 全绿（基线 **272**，不得回归）；`cd desktop && npx vitest run` 全绿（基线 **439**）+ `npx tsc --noEmit`。
- 环境：`export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`；`export CARGO_TARGET_DIR=/tmp/yi-s1`。
- **零回归不变量**：`Feed::All` 且**无订阅者连接**时，客户端收到的帧序列与改动前逐字节相同（桌面走 stdio，永远无订阅者，故桌面不受影响）。
- 订阅集合上限 **16**；`[]`＝收起所有内容通知；全局帧恒放行。

## 文件结构

- `yi-agent-rs/crates/yi-agent-app-server/src/broadcast.rs` —— 加 `Feed`、`subscribe`、`has_subscribed_clients`、`broadcast_for`；`broadcast` 变薄封装。
- `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` —— `thread/subscribe` RPC；`write_notification` 改为按 thread 键路由；审批请求改为按 thread 键路由；turn 驱动里加 delta 合并。
- `yi-agent-rs/crates/yi-agent-app-server/src/ws.rs` —— 集成测试（双客户端订阅）。不改生产代码。
- `docs/superpowers/specs/2026-10-02-thread-subscription-filtering-design.md` —— 精确化不变量措辞。
- `README.md` / `README.en.md` / `docs/relay-deploy.md` —— 记一句新能力。

---

### Task 1: `Broadcaster` 的订阅过滤

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/broadcast.rs`
- Test: 同文件 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: 既有 `ClientId`、`Client`、`Broadcaster::{register,register_reliable,unregister,broadcast}`。
- Produces:
  - `pub const MAX_SUBSCRIPTIONS: usize = 16;`
  - `pub fn subscribe(&self, id: &ClientId, thread_ids: Vec<String>)`
  - `pub fn has_subscribed_clients(&self) -> bool`
  - `pub fn broadcast_for(&self, key: Option<&str>, frame: Value)`
  - `pub fn broadcast(&self, frame: Value)`（改为 `self.broadcast_for(None, frame)`）

- [ ] **Step 1: 写失败测试**

在 `broadcast.rs` 的 `mod tests` 末尾追加：

```rust
    /// `Feed::All`（默认）收全部；`None` 键（全局帧）对谁都放行。
    #[tokio::test]
    async fn a_default_client_receives_everything() {
        let hub = Broadcaster::new();
        let mut rx = hub.register(ClientId::ws(uuid::Uuid::nil()));
        hub.broadcast_for(Some("t1"), serde_json::json!({"n": 1}));
        hub.broadcast_for(None, serde_json::json!({"n": 2}));
        assert_eq!(rx.recv().await.unwrap()["n"], 1);
        assert_eq!(rx.recv().await.unwrap()["n"], 2);
    }

    /// 订阅后只收集合内 thread 的内容通知；全局帧仍放行。
    #[tokio::test]
    async fn a_subscribed_client_only_gets_its_threads() {
        let hub = Broadcaster::new();
        let id = ClientId::ws(uuid::Uuid::nil());
        let mut rx = hub.register(id.clone());
        hub.subscribe(&id, vec!["t1".to_string()]);

        hub.broadcast_for(Some("t1"), serde_json::json!({"n": 1})); // 命中
        hub.broadcast_for(Some("t2"), serde_json::json!({"n": 2})); // 未命中 → 丢
        hub.broadcast_for(None, serde_json::json!({"n": 3}));       // 全局 → 放行

        assert_eq!(rx.recv().await.unwrap()["n"], 1);
        assert_eq!(rx.recv().await.unwrap()["n"], 3);

        // 未命中那一帧确实没进来：队列里没有 n=2。
        assert!(rx.try_recv().is_err());
    }

    /// 订阅（`Only`）与未订阅（`All`）混在一起时各取所需。
    #[tokio::test]
    async fn subscription_narrows_one_client_without_affecting_another() {
        let hub = Broadcaster::new();
        let sub_id = ClientId::ws(uuid::Uuid::from_u128(1));
        let all_id = ClientId::local();
        let mut sub = hub.register(sub_id.clone());
        let mut all = hub.register(all_id.clone());
        hub.subscribe(&sub_id, vec!["t1".to_string()]);

        hub.broadcast_for(Some("t2"), serde_json::json!({"n": 9}));
        assert_eq!(all.recv().await.unwrap()["n"], 9, "All 客户端必须收到 t2");
        assert!(sub.try_recv().is_err(), "订阅 t1 的客户端不该收到 t2");
        assert!(hub.has_subscribed_clients());
    }

    /// 空订阅＝只收全局帧。
    #[tokio::test]
    async fn an_empty_subscription_receives_only_global_frames() {
        let hub = Broadcaster::new();
        let id = ClientId::ws(uuid::Uuid::nil());
        let mut rx = hub.register(id.clone());
        hub.subscribe(&id, vec![]);
        hub.broadcast_for(Some("t1"), serde_json::json!({"n": 1}));
        hub.broadcast_for(None, serde_json::json!({"n": 2}));
        assert_eq!(rx.recv().await.unwrap()["n"], 2);
        assert!(rx.try_recv().is_err());
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-app-server --lib broadcast 2>&1 | tail -20`
Expected: 编译失败（`subscribe`/`broadcast_for`/`has_subscribed_clients` 未定义）。

- [ ] **Step 3: 实现**

在 `broadcast.rs`：`use std::collections::{HashMap, HashSet};`（现在是 `HashMap`，加 `HashSet`）。在 `CLIENT_QUEUE` 旁加：

```rust
/// 一个客户端可订阅的 thread 上限（防止"订阅全部"退化成"全收"）。
pub const MAX_SUBSCRIPTIONS: usize = 16;

/// 一个客户端的订阅状态。
#[derive(Default)]
enum Feed {
    /// 未订阅：收全部内容通知（今天的桌面行为）。
    #[default]
    All,
    /// 已订阅：只收这些 thread 的内容通知（可为空集）。
    Only(HashSet<String>),
}

impl Feed {
    /// `key`＝帧所属 thread（`None`＝全局帧，恒放行）。
    fn accepts(&self, key: Option<&str>) -> bool {
        match (self, key) {
            (Feed::All, _) => true,
            (Feed::Only(_), None) => true,
            (Feed::Only(set), Some(t)) => set.contains(t),
        }
    }
}
```

`struct Client` 加字段 `feed: Feed`；`register_inner` 里 `Client { tx, reliable, feed: Feed::default() }`。

在 `impl Broadcaster` 加：

```rust
    /// 设置该客户端的订阅集合（**整体替换**）。未注册则无操作。
    pub fn subscribe(&self, id: &ClientId, thread_ids: Vec<String>) {
        let mut guard = self.clients.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(client) = guard.get_mut(id) {
            client.feed = Feed::Only(thread_ids.into_iter().collect());
        }
    }

    /// 是否存在已订阅的客户端（决定逐字流是否合并）。
    pub fn has_subscribed_clients(&self) -> bool {
        self.clients
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .any(|c| matches!(c.feed, Feed::Only(_)))
    }
```

把 `broadcast` 改为薄封装，并新增 `broadcast_for`：

```rust
    /// 广播给所有客户端（无 thread 键，恒放行）。等价于 `broadcast_for(None, …)`。
    pub fn broadcast(&self, frame: Value) {
        self.broadcast_for(None, frame);
    }

    /// 按 thread 键广播：`Feed::All` 与订阅命中的客户端收到，其余跳过。
    ///
    /// 背压/可靠客户端语义与旧 `broadcast` 完全一致（见其文档）。
    pub fn broadcast_for(&self, key: Option<&str>, frame: Value) {
        let mut guard = self.clients.lock().unwrap_or_else(|p| p.into_inner());
        guard.retain(|_, client| {
            if !client.feed.accepts(key) {
                return true; // 未命中：跳过投递，但保留登记。
            }
            if client.reliable {
                let _ = client.tx.try_send(frame.clone());
                true
            } else {
                client.tx.try_send(frame.clone()).is_ok()
            }
        });
    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p yi-agent-app-server --lib broadcast 2>&1 | tail -20`
Expected: 全绿（既有 broadcast 测试 + 新增 4 个）。

- [ ] **Step 5: 跑门禁 + fmt + 提交**

```bash
cargo test -p yi-agent-app-server 2>&1 | grep "test result"
rustfmt --edition 2024 --check --config skip_children=true yi-agent-rs/crates/yi-agent-app-server/src/broadcast.rs
git add yi-agent-rs/crates/yi-agent-app-server/src/broadcast.rs
git commit -m "feat(app-server): add per-client feed filtering to the broadcaster"
```

---

### Task 2: `thread/subscribe` RPC

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（RPC 分发处，`device/revoke` 附近）

**Interfaces:**
- Consumes: Task 1 的 `Broadcaster::{subscribe, MAX_SUBSCRIPTIONS}`。
- Produces: RPC `thread/subscribe`：请求 `{threadIds: string[]}` → 响应 `{subscribed: string[]}`；错误 `-32602`。

- [ ] **Step 1: 写失败测试**

在 `server.rs` 的 `mod tests` 里（`a_control_client_can_redeem_a_code_with_pair_redeem` 附近）追加：

```rust
    /// `thread/subscribe` 整体替换订阅集合并回显；超上限报 invalid_params。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_can_subscribe_to_a_thread_set() {
        let mut h = Harness::with_scope(Scope::Control).await;
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":9,"method":"thread/subscribe","params":{"threadIds":["t1","t2"]}}"#)
            .await;
        let v = read_response(&mut h, 9).await;
        assert_eq!(v["result"]["subscribed"].as_array().unwrap().len(), 2);
        assert!(h.hub().has_subscribed_clients(), "订阅后 hub 必须认得");

        // 超过 16 → invalid_params。
        let many: Vec<String> = (0..17).map(|i| format!("t{i}")).collect();
        let req = serde_json::json!({"jsonrpc":"2.0","id":10,"method":"thread/subscribe","params":{"threadIds":many}});
        h.send(&req.to_string()).await;
        let v = read_response(&mut h, 10).await;
        assert_eq!(v["error"]["code"], -32602, "超上限必须报错: {v}");
        h.shutdown().await;
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-app-server --lib a_client_can_subscribe 2>&1 | tail -20`
Expected: 失败（`method not found`，且 `h.hub()` 可能未定义 → 见 Step 3）。

- [ ] **Step 3: 实现**

在 `server.rs` 的 `"device/revoke" => {` 分支**之后**（同一层 `match method.as_str()`）加：

```rust
                    "thread/subscribe" => {
                        // 整体替换该客户端的订阅集合。**不在 ADMIN_METHODS 内**：
                        // 任何已初始化客户端都能收窄自己的 feed。未调用过的客户端
                        // 保持 `Feed::All`（桌面行为不变）。
                        let ids: Option<Vec<String>> = req
                            .params
                            .get("threadIds")
                            .and_then(|v| v.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(str::to_string))
                                    .collect()
                            });
                        let Some(ids) = ids else {
                            write_response(
                                &hub,
                                &client,
                                err_response(id, RpcError::invalid_params("missing threadIds")),
                            )
                            .await?;
                            continue;
                        };
                        if ids.len() > crate::broadcast::MAX_SUBSCRIPTIONS {
                            write_response(
                                &hub,
                                &client,
                                err_response(
                                    id,
                                    RpcError::invalid_params(format!(
                                        "at most {} threadIds",
                                        crate::broadcast::MAX_SUBSCRIPTIONS
                                    )),
                                ),
                            )
                            .await?;
                            continue;
                        }
                        hub.subscribe(&client, ids.clone());
                        write_response(
                            &hub,
                            &client,
                            ok_response(id, json!({ "subscribed": ids })),
                        )
                        .await?;
                    }
```

在测试 harness (`impl Harness`) 里，`pub(crate) fn pairing(&self) -> Arc<PairingState>` **旁**加：

```rust
        pub(crate) fn hub(&self) -> Arc<crate::broadcast::Broadcaster> {
            Arc::clone(&self.hub)
        }
```

（若 `Harness` 未把 `hub` 存成字段，则按 `with_config_and_scope` 里 `serve_scoped` 收到的同一个 `Arc` 补存一个 `hub` 字段。）

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p yi-agent-app-server --lib a_client_can_subscribe 2>&1 | tail -20`
Expected: PASS。

- [ ] **Step 5: 门禁 + fmt + 提交**

```bash
cargo test -p yi-agent-app-server 2>&1 | grep "test result"
rustfmt --edition 2024 --check --config skip_children=true yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): add thread/subscribe RPC"
```

---

### Task 3: 通知按 thread 键路由

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`write_notification`、审批请求广播点、`Notification` 加键方法）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/ws.rs`（双客户端集成）

**Interfaces:**
- Consumes: Task 1 `broadcast_for`；Task 2 `thread/subscribe`。
- Produces: `impl Notification { fn thread_key(&self) -> Option<&str> }`（crate 内可见）。

- [ ] **Step 1: 写失败测试**

在 `server.rs` 的 `mod tests` 追加（用 `Harness::hub()` 直发帧，避免驱动两个真实
thread 的重成本；ws 层"订阅确实传到 hub"由 Task 2 的测试覆盖）：

```rust
    /// 通知按 thread 键路由：订阅 t1 的客户端只收 t1，未订阅的收全部。
    #[tokio::test(flavor = "multi_thread")]
    async fn notifications_are_routed_by_thread_key() {
        let hub = crate::broadcast::Broadcaster::new();
        let sub = crate::broadcast::ClientId::ws(uuid::Uuid::from_u128(1));
        let all = crate::broadcast::ClientId::local();
        let mut sub_rx = hub.register(sub.clone());
        let mut all_rx = hub.register(all.clone());
        hub.subscribe(&sub, vec!["t1".to_string()]);

        let content_t1 = serde_json::json!({"method":"turn/started","params":{"thread_id":"t1"}});
        let content_t2 = serde_json::json!({"method":"turn/started","params":{"thread_id":"t2"}});
        hub.broadcast_for(Some("t1"), content_t1);
        hub.broadcast_for(Some("t2"), content_t2);

        assert_eq!(sub_rx.recv().await.unwrap()["params"]["thread_id"], "t1");
        assert!(sub_rx.try_recv().is_err(), "订阅 t1 不该收到 t2");
        assert_eq!(all_rx.recv().await.unwrap()["params"]["thread_id"], "t1");
        assert_eq!(all_rx.recv().await.unwrap()["params"]["thread_id"], "t2");
    }

    /// `Notification::thread_key` 与协议字段一致（全局帧无键）。
    #[test]
    fn notification_thread_key_matches_the_wire() {
        use crate::protocol::Notification;
        assert_eq!(
            Notification::TurnStarted { thread_id: "t1".into(), turn_id: "x".into() }.thread_key(),
            Some("t1")
        );
        assert_eq!(Notification::UiSettingsUpdated { theme: "dark".into() }.thread_key(), None);
        assert_eq!(Notification::Error { message: "x".into() }.thread_key(), None);
        assert_eq!(
            Notification::ToolCallApprovalResolved {
                perm_id: "p".into(), by: "c".into(), decision: "allow_once".into()
            }
            .thread_key(),
            None
        );
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-app-server --lib notification 2>&1 | tail -20`
Expected: 失败（`thread_key` 未定义）。

- [ ] **Step 3: 实现**

在 `protocol.rs` 的 `impl Notification`（没有就新建）加：

```rust
impl Notification {
    /// 该通知所属的 thread；`None`＝全局帧（主题/错误/审批已处理），恒放行。
    pub(crate) fn thread_key(&self) -> Option<&str> {
        match self {
            Notification::ThreadStarted { thread_id, .. }
            | Notification::TurnStarted { thread_id, .. }
            | Notification::ThreadStatusUpdated { thread_id, .. }
            | Notification::ItemStarted { thread_id, .. }
            | Notification::ItemDelta { thread_id, .. }
            | Notification::ItemCompleted { thread_id, .. }
            | Notification::TurnCompleted { thread_id, .. }
            | Notification::InterjectionsReturned { thread_id, .. }
            | Notification::TurnRetry { thread_id, .. }
            | Notification::TokenUsage { thread_id, .. }
            | Notification::AgentTraceEvent { thread_id, .. }
            | Notification::AgentChildrenUpdated { thread_id, .. }
            | Notification::ProcessUpdated { thread_id, .. } => Some(thread_id),
            Notification::ToolCallApprovalResolved { .. }
            | Notification::UiSettingsUpdated { .. }
            | Notification::Error { .. } => None,
        }
    }
}
```

`server.rs` 的 `write_notification` 改一行：

```rust
    // 内容通知按 thread 过滤；全局帧（None）恒放行。无订阅者时等价于全广播。
    hub.broadcast_for(n.thread_key(), frame);
```

审批请求广播点（`requestApproval` 那段 `hub.broadcast(frame);`）改为：

```rust
                            // 只推给"订阅了该 thread"的客户端 + 全收的客户端（桌面）：
                            // 没打开这个会话的手机不该被它的审批打断。
                            hub.broadcast_for(Some(&thread_id), frame);
```

- [ ] **Step 4: 跑测试确认通过 + 门禁**

Run: `cargo test -p yi-agent-app-server 2>&1 | grep "test result"`
Expected: 全绿（272 + 新增，无回归）。

- [ ] **Step 5: fmt + 提交**

```bash
rustfmt --edition 2024 --check --config skip_children=true yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs
rustfmt --edition 2024 --check --config skip_children=true yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git add yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): route notifications by thread and scope approvals"
```

---

### Task 4: 逐字流按订阅合并

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（turn 驱动的 translate 循环）
- Test: `server.rs` `mod tests`

**Interfaces:**
- Consumes: `Broadcaster::has_subscribed_clients()`（Task 1）。
- Produces: 无新公开签名；`item/delta` 在**有订阅者时**按 thread 攒批。

- [ ] **Step 1: 写失败测试**

在 `server.rs` `mod tests` 追加（测合并器的**纯逻辑**，不碰 I/O；`push` 返回"此刻该
发出去的"若干条）：

```rust
    /// 合并器：同一 item_id 的相邻 delta 拼接；跨 item_id 先刷旧的。
    #[test]
    fn delta_coalescer_concat_within_an_item_and_flush_across_items() {
        let mut c = DeltaCoalescer::default();
        // 同一 item 连续追加不立即发。
        assert!(c.push("t1", "i1", "Hel").is_empty());
        assert!(c.push("t1", "i1", "lo").is_empty());
        // 取出即 "Hello"。
        assert_eq!(c.take("t1"), Some(("i1".to_string(), "Hello".to_string())));
        assert_eq!(c.take("t1"), None, "取走后为空");

        // 跨 item_id：追新的之前先把旧的返回（顺序优先）。
        assert!(c.push("t1", "i1", "a").is_empty());
        assert_eq!(c.push("t1", "i2", "b"), vec![("i1".to_string(), "a".to_string())]);
        assert_eq!(c.take("t1"), Some(("i2".to_string(), "b".to_string())));

        // 超过 4KB 自动刷出。
        let big = "x".repeat(DeltaCoalescer::FLUSH_BYTES);
        let flushed = c.push("t1", "i3", &big);
        assert_eq!(flushed.len(), 1, "超上限必须立即返回: {flushed:?}");
        assert_eq!(c.take("t1"), None);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-app-server --lib delta_coalescer 2>&1 | tail -20`
Expected: 失败（`DeltaCoalescer` 未定义）。

- [ ] **Step 3: 实现合并器 + 接进驱动**

在 `server.rs`（`write_notification` 附近）加纯数据结构：

```rust
/// 逐字流合并器：按 thread 攒 `(item_id, text)`。跨 item_id 不合并（顺序优先）。
#[derive(Default)]
struct DeltaCoalescer {
    /// thread → 当前正在累加的 (item_id, text)。
    pending: std::collections::HashMap<String, (String, String)>,
}

impl DeltaCoalescer {
    const FLUSH_BYTES: usize = 4096;

    /// 追加一段 delta，返回**此刻应当立即发出去的**若干条（跨 item 或超限时）。
    fn push(&mut self, thread: &str, item_id: &str, delta: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        match self.pending.get_mut(thread) {
            Some((pid, text)) if pid == item_id => text.push_str(delta),
            _ => {
                if let Some(prev) = self.pending.remove(thread) {
                    out.push(prev);
                }
                self.pending
                    .insert(thread.to_string(), (item_id.to_string(), delta.to_string()));
            }
        }
        if self.pending.get(thread).map(|(_, t)| t.len()).unwrap_or(0) >= Self::FLUSH_BYTES {
            if let Some(p) = self.pending.remove(thread) {
                out.push(p);
            }
        }
        out
    }

    /// 取出并清空该 thread 的待发（屏障/tick 调用）。
    fn take(&mut self, thread: &str) -> Option<(String, String)> {
        self.pending.remove(thread)
    }

    /// 取出全部待发（tick 用）。
    fn take_all(&mut self) -> Vec<(String, (String, String))> {
        self.pending.drain().collect()
    }
}
```

在 turn 驱动（`Some(e) => { for n in translator.on_event(e) { … } }`）里：

- `loop {` **之前**加：

```rust
                            let mut coalescer = DeltaCoalescer::default();
                            let mut delta_tick = tokio::time::interval(Duration::from_millis(100));
                            delta_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
```

- 把"逐个 `write_notification`"改为（**保留既有** `completed_items.push` / `TokenUsage`
  侧收集与错误处理不变，只替换写出的那一步）：

```rust
                            for n in translator.on_event(e) {
                                // …既有 completed_items / last_usage 侧收集原样保留…

                                // 逐字流：仅当存在订阅者时合并；否则走原来的直发。
                                let mut to_send: Vec<crate::protocol::Notification> = Vec::new();
                                match &n {
                                    crate::protocol::Notification::ItemDelta { thread_id: t, item_id, delta, .. }
                                        if hub.has_subscribed_clients() =>
                                    {
                                        for (pid, text) in coalescer.push(t, item_id, delta) {
                                            to_send.push(crate::protocol::Notification::ItemDelta {
                                                thread_id: t.clone(), item_id: pid, delta: text,
                                            });
                                        }
                                    }
                                    _ => {
                                        // 屏障：非 delta 通知前先刷该 thread 的待发。
                                        if let crate::protocol::Notification::ItemDelta { thread_id: t, .. } = &n {
                                            if let Some((pid, text)) = coalescer.take(t) {
                                                to_send.push(crate::protocol::Notification::ItemDelta {
                                                    thread_id: t.clone(), item_id: pid, delta: text,
                                                });
                                            }
                                        }
                                        to_send.push(n.clone());
                                    }
                                }
                                for out in to_send {
                                    if write_notification(&hub, &out).await.is_err() {
                                        // 既有失败处理（上报 Finished 后结束本轮）原样。
                                        let _ = turn_tx.send(finished_event(&thread_id, &turn_id)).await;
                                        return;
                                    }
                                }
                            }
```

- 在 `tokio::select!` 里加一个 tick 分支（与 `ev = stream.next()` 并列）：

```rust
                    _ = delta_tick.tick() => {
                        for (t, (pid, text)) in coalescer.take_all() {
                            let _ = write_notification(&hub, &crate::protocol::Notification::ItemDelta {
                                thread_id: t, item_id: pid, delta: text,
                            }).await;
                        }
                    }
```

- [ ] **Step 4: 跑测试确认通过 + 门禁**

Run: `cargo test -p yi-agent-app-server 2>&1 | grep "test result"`
Expected: 全绿，无回归。

- [ ] **Step 5: fmt + 提交**

```bash
rustfmt --edition 2024 --check --config skip_children=true yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): coalesce item/delta for subscribed clients"
```

---

### Task 5: 文档与 spec 措辞精确化

**Files:**
- Modify: `docs/superpowers/specs/2026-10-02-thread-subscription-filtering-design.md`（§3.1 不变量措辞）
- Modify: `README.md`、`README.en.md`、`docs/relay-deploy.md`

**Interfaces:** 无代码。

- [ ] **Step 1: 精确化 spec §3.1 不变量**

把"`Feed::All` 客户端收到的字节流与今天逐字节相同"改为：

> `Feed::All` 且**当前没有已订阅客户端连接**时，字节流与今天逐字节相同。桌面走 stdio
> 实例（永无订阅者），故桌面**始终**不受影响；仅在同时存在订阅者的 ws 实例上，delta
> 才会对所有客户端合并。

- [ ] **Step 2: README 记一句新能力**

`README.md` 远程连接段追加一句：

> 客户端可调 `thread/subscribe {threadIds}` **只收自己关心的会话**（未调用者仍全收）；
> 已订阅时逐字输出会合并降频。见 [设计](docs/superpowers/specs/2026-10-02-thread-subscription-filtering-design.md)。

`README.en.md` 对应英文一句。

- [ ] **Step 3: relay-deploy 记一句**

`docs/relay-deploy.md` §六"Tier 1.1 已落地"补一条：按 thread 订阅过滤（`thread/subscribe`）。

- [ ] **Step 4: 提交**

```bash
git add -A
git commit -m "docs: document thread-scoped subscription filtering"
```

---

## Self-Review

**Spec coverage**
- G1 订阅（整体替换、上限 16、`[]`、未订阅不变）→ Task 1 + Task 2。
- 全局帧恒放行 → Task 1（`Feed::accepts(None)=true`）+ Task 3（`thread_key` 映射）。
- 审批按 thread 过滤 → Task 3。
- G2 delta 合并（100ms/4KB、同 item 合并、屏障 flush）→ Task 4。
- G3 列表不推送 → 无需代码（服务端本就不推列表；文档 Task 5 提一句）。
- 零回归 → 每个 Task 的门禁 + `Feed::All` 快路径。

**Placeholder scan**：无 TBD/TODO；每个代码步都有实际代码。

**Type consistency**：`Feed`、`MAX_SUBSCRIPTIONS`、`subscribe`、`has_subscribed_clients`、`broadcast_for`、`thread_key`、`DeltaCoalescer::{push,take,take_all}` 在各 Task 间名称一致。

**已知偏差（Task 4 Step 1 内已标注）**：合并器的单测以 `push` 返回值为准（跨 item 边走边发），`drain` 仅示意；实现者按 Step 1 修正版断言。
