# 按会话订阅接线（S2）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让手机（远程 ws 客户端）真正按 IM 模式工作——列表层实时、内容层按需订阅、返回冷会话时只读补齐且不打断正在跑的回合。

**Architecture:** 服务端把投递分成三层（全局/列表层恒推，内容层按 `Feed` 过滤），并新增只读 `thread/readItems`；前端仅对 `isRemoteClient()` 维护一个 LRU warm 窗口并调用 `thread/subscribe`，窗口外返回时用 `readItems` 补齐并按 item id 去重。桌面（Tauri/stdio）一行不改。

**Tech Stack:** Rust（edition 2024）、tokio、serde_json、axum（ws）；前端 React + TypeScript（vitest）；测试 `cargo test` / `npx vitest run`。

## Global Constraints

- 严禁在 `main` 直接提交；worktree + 分支 → 改 → 测试 → commit → `git merge --no-ff` → 清理。
- conventional commits，**不写 `Co-Authored-By`**。
- 提交前只对**改动文件**跑 rustfmt（Rust）：`rustfmt --edition 2024 --check --config skip_children=true <file>`；**不要把 repo 级既有 rustfmt 漂移扫进 commit**。
- 门禁：`cargo test -p yi-agent-app-server` 全绿（基线 **280**，不得回归）；`cd desktop && npx vitest run` 全绿（基线 **441**）+ `npx tsc --noEmit`。
- 环境：`export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`；`export CARGO_TARGET_DIR=/tmp/yi-s2`。
- **零回归不变量**：桌面（stdio）实例**永无订阅者**，`Feed::All` 客户端收到的帧序列与改动前逐字节相同。桌面前端**不得**发送 `thread/subscribe` / `thread/readItems`。
- 订阅集合上限 **16**（服务端 `MAX_SUBSCRIPTIONS`）；前端 warm 窗口上限 **8**，天然不超限。
- **列表层恒推**：`thread/started`、`thread/status/updated` 对所有客户端恒推，**不看订阅**；内容层与全局帧语义沿用 S1。
- 参考设计：`docs/superpowers/specs/2026-10-02-thread-subscription-wiring-design.md`；S1 设计：`docs/superpowers/specs/2026-10-02-thread-subscription-filtering-design.md`。

## 文件结构

- `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs` —— 加 `Delivery` 枚举 + `Notification::delivery()`。
- `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` —— `write_notification` 按层选键；新增 `thread/readItems` 分发 + `item_id` 辅助；测试。
- `desktop/src/lib/subscriptionWindow.ts`（新）—— LRU warm 窗口的纯逻辑。
- `desktop/src/lib/session.ts` —— `lastServerItemId` 记录 + `upsertItems`（按 id 去重合并）。
- `desktop/src/App.tsx` —— 仅远程客户端：selectThread 订阅 + 冷会话补齐。
- `README.md` / `README.en.md` —— 记一句列表层实时与手机按需订阅。

---

### Task 1: 服务端：投递分层（列表层恒推）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`（`impl Notification` 附近）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`write_notification`）
- Test: `server.rs` `mod tests`

**Interfaces:**
- Consumes: S1 的 `Notification::thread_key()`、`Broadcaster::broadcast_for`。
- Produces: `pub(crate) enum Delivery { Global, List, Content }`、`Notification::delivery(&self) -> Delivery`。

- [ ] **Step 1: 写失败测试**

在 `server.rs` 的 `mod tests` 末尾追加：

```rust
    /// 投递分层：列表层/全局帧无 thread 键（恒推），内容层按 thread 键过滤。
    #[test]
    fn notification_delivery_classifies_list_and_content() {
        use crate::protocol::{Delivery, Notification};
        let status = Notification::ThreadStatusUpdated {
            thread_id: "t2".into(),
            status: crate::protocol::ThreadStatus::Running,
        };
        assert_eq!(status.delivery(), Delivery::List);
        assert_eq!(
            Notification::ThreadStarted {
                thread_id: "t2".into(),
                cwd: "/w".into(),
                model: "m".into()
            }
            .delivery(),
            Delivery::List
        );
        assert_eq!(
            Notification::ItemDelta {
                thread_id: "t2".into(),
                item_id: "i".into(),
                delta: "d".into()
            }
            .delivery(),
            Delivery::Content
        );
        assert_eq!(
            Notification::UiSettingsUpdated { theme: "dark".into() }.delivery(),
            Delivery::Global
        );
        assert_eq!(
            Notification::Error { message: "x".into() }.delivery(),
            Delivery::Global
        );
        assert_eq!(
            Notification::ToolCallApprovalResolved {
                perm_id: "p".into(),
                by: "c".into(),
                decision: "allow_once".into()
            }
            .delivery(),
            Delivery::Global
        );
    }

    /// 订阅 t1 的客户端：仍收到 t2 的 `thread/status/updated`（列表层恒推），
    /// 但收不到 t2 的 `item/delta`（内容层过滤）。
    #[tokio::test(flavor = "multi_thread")]
    async fn list_layer_is_always_delivered_while_content_is_filtered() {
        let hub = crate::broadcast::Broadcaster::new();
        let sub = crate::broadcast::ClientId::ws(uuid::Uuid::from_u128(1));
        let mut rx = hub.register(sub.clone());
        hub.subscribe(&sub, vec!["t1".to_string()]);

        // 列表层：未订阅的 t2 的状态帧也必须到达。
        write_notification(
            &hub,
            &Notification::ThreadStatusUpdated {
                thread_id: "t2".into(),
                status: crate::protocol::ThreadStatus::Running,
            },
        )
        .await
        .unwrap();
        assert_eq!(rx.recv().await.unwrap()["params"]["thread_id"], "t2");

        // 内容层：未订阅的 t2 的 delta 必须被挡下。
        write_notification(
            &hub,
            &Notification::ItemDelta {
                thread_id: "t2".into(),
                item_id: "i".into(),
                delta: "leak".into(),
            },
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_err(), "订阅 t1 不该收到 t2 的内容帧");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-app-server --lib notification_delivery_classifies 2>&1 | tail -20`
Expected: 编译失败（`Delivery`/`delivery` 未定义）。

- [ ] **Step 3: 实现分类 + 接进 `write_notification`**

在 `protocol.rs` 的 `impl Notification` 里加（`thread_key` 旁）：

```rust
/// 该通知的投递层级（S2）：决定 `write_notification` 用哪个广播键。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// 全局帧：主题/错误/审批已处理。恒推。
    Global,
    /// 列表层：会话存在与状态。**恒推**（列表要实时，与订阅无关）。
    List,
    /// 内容层：会话正文流。按 `thread_key()` 过滤。
    Content,
}

impl Notification {
    /// 该通知的投递层级。见 [`Delivery`]。
    pub(crate) fn delivery(&self) -> Delivery {
        match self {
            Notification::ThreadStarted { .. } | Notification::ThreadStatusUpdated { .. } => {
                Delivery::List
            }
            Notification::UiSettingsUpdated { .. }
            | Notification::Error { .. }
            | Notification::ToolCallApprovalResolved { .. } => Delivery::Global,
            _ => Delivery::Content,
        }
    }
}
```

把 `server.rs` 的 `write_notification` 改为：

```rust
async fn write_notification(
    hub: &crate::broadcast::Broadcaster,
    n: &Notification,
) -> anyhow::Result<()> {
    let frame = serde_json::to_value(NotificationEnvelope::new(n))
        .map_err(|e| anyhow::anyhow!("failed to serialize notification: {e}"))?;
    // S2：列表层/全局帧恒推（无键）；内容层按 thread 过滤。无订阅者时等价于全广播。
    let key = match n.delivery() {
        crate::protocol::Delivery::Content => n.thread_key(),
        crate::protocol::Delivery::List | crate::protocol::Delivery::Global => None,
    };
    hub.broadcast_for(key, frame);
    Ok(())
}
```

- [ ] **Step 4: 跑测试确认通过 + 门禁**

Run: `cargo test -p yi-agent-app-server 2>&1 | grep "test result"`
Expected: 全绿（基线 280 + 新增 2，无回归）。

- [ ] **Step 5: fmt + 提交**

```bash
rustfmt --edition 2024 --check --config skip_children=true yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs
rustfmt --edition 2024 --check --config skip_children=true yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git add yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): deliver list-layer notifications regardless of subscription"
```

---

### Task 2: 服务端：只读 `thread/readItems`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（RPC 分发 `"thread/subscribe"` 之后；`store_lookup` 附近加 `item_id`）
- Test: `server.rs` `mod tests`

**Interfaces:**
- Consumes: S1/S2 `thread/subscribe`、`store_lookup`、`require_thread_id`、`thread_store::LoadedThread`。
- Produces: RPC `thread/readItems`：请求 `{ threadId, afterItemId? }` → 响应 `{ items: Item[] }`；未知 thread → `-32011`。

- [ ] **Step 1: 写失败测试**

在 `server.rs` 的 `mod tests` 追加：

```rust
    /// `thread/readItems` 返回已落盘的 items；`afterItemId` 只返回其后的部分。
    #[tokio::test(flavor = "multi_thread")]
    async fn read_items_returns_persisted_items_and_slices_after_id() {
        let mut h = Harness::new();
        let tid = start_thread(&mut h).await;
        // 跑完一轮，产生至少一条已落盘的 item（user + agent 消息）。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;
        // 读到 turn/completed 为止，确认落盘完成。
        for _ in 0..16 {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
                break;
            }
        }

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/readItems","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let v = read_response(&mut h, 4).await;
        let items = v["result"]["items"].as_array().expect("items array");
        assert!(!items.is_empty(), "readItems 必须返回已落盘的 items: {v}");
        let first_id = items[0]["id"].as_str().unwrap().to_string();

        // afterItemId = 第一条 → 不包含第一条。
        let req = serde_json::json!({
            "jsonrpc":"2.0","id":5,"method":"thread/readItems",
            "params":{"threadId": tid, "afterItemId": first_id}
        });
        h.send(&req.to_string()).await;
        let v = read_response(&mut h, 5).await;
        let after = v["result"]["items"].as_array().unwrap();
        assert!(
            after.iter().all(|i| i["id"].as_str() != Some(first_id.as_str())),
            "afterItemId 之后不该再含该 id: {v}"
        );

        // 未知 thread → unknown_thread(-32011)。
        h.send(r#"{"jsonrpc":"2.0","id":6,"method":"thread/readItems","params":{"threadId":"nope"}}"#)
            .await;
        let v = read_response(&mut h, 6).await;
        assert_eq!(v["error"]["code"], -32011, "未知 thread 必须报错: {v}");
        h.shutdown().await;
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-app-server --lib read_items_returns 2>&1 | tail -20`
Expected: 失败（`method not found`）。

- [ ] **Step 3: 实现**

在 `server.rs` 分发里 `"thread/subscribe"` 分支**之后**（同一层 `match method.as_str()`）加：

```rust
                    "thread/readItems" => {
                        // 只读补齐：读已落盘的 items，**不 resume、不重建 agent、
                        // 不中断正在跑的回合**（这正是它相对 thread/resume 的意义）。
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        let after =
                            req.params.get("afterItemId").and_then(|v| v.as_str()).map(str::to_string);
                        let store = store_lookup(&threads, &workspaces, &cfg, &thread_id);
                        let loaded = match store.load(&thread_id) {
                            Ok(Some(l)) => l,
                            Ok(None) => {
                                write_response(
                                    &hub,
                                    &client,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                                continue;
                            }
                            Err(e) => {
                                write_response(
                                    &hub,
                                    &client,
                                    err_response(id, RpcError::internal(e.to_string())),
                                )
                                .await?;
                                continue;
                            }
                        };
                        let items: Vec<crate::protocol::Item> = match after {
                            Some(aid) => match loaded
                                .items
                                .iter()
                                .position(|it| item_id(it) == Some(aid.as_str()))
                            {
                                // 找到锚点：只给它之后的部分。
                                Some(i) => loaded.items.into_iter().skip(i + 1).collect(),
                                // 锚点缺失（可能被 compact 丢弃）：返回全部，由客户端按 id 去重。
                                None => loaded.items,
                            },
                            None => loaded.items,
                        };
                        write_response(&hub, &client, ok_response(id, json!({ "items": items })))
                            .await?;
                    }
```

在 `store_lookup` 函数旁加辅助：

```rust
/// 一条 `Item` 的稳定 id（用于 `thread/readItems` 的 `afterItemId` 切片与前端去重）。
fn item_id(item: &crate::protocol::Item) -> Option<&str> {
    match item {
        crate::protocol::Item::UserMessage { id, .. }
        | crate::protocol::Item::AgentMessage { id, .. }
        | crate::protocol::Item::ToolCall { id, .. }
        | crate::protocol::Item::UserInterjection { id, .. } => Some(id),
    }
}
```

- [ ] **Step 4: 跑测试确认通过 + 门禁**

Run: `cargo test -p yi-agent-app-server 2>&1 | grep "test result"`
Expected: 全绿（280 + 3，无回归）。

- [ ] **Step 5: fmt + 提交**

```bash
rustfmt --edition 2024 --check --config skip_children=true yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): add read-only thread/readItems RPC"
```

---

### Task 3: 前端：warm 订阅窗口（纯逻辑）

**Files:**
- Create: `desktop/src/lib/subscriptionWindow.ts`
- Test: `desktop/src/lib/subscriptionWindow.test.ts`

**Interfaces:**
- Produces: `SUBSCRIPTION_WINDOW_SIZE`（=8）、`class SubscriptionWindow { touch(id): string[]; has(id): boolean; current(): string[] }`。

- [ ] **Step 1: 写失败测试**

`desktop/src/lib/subscriptionWindow.test.ts`：

```ts
import { describe, expect, it } from "vitest";
import { SUBSCRIPTION_WINDOW_SIZE, SubscriptionWindow } from "./subscriptionWindow";

describe("SubscriptionWindow", () => {
  it("keeps the most recent ids, most-recent first", () => {
    const w = new SubscriptionWindow();
    expect(w.touch("a")).toEqual(["a"]);
    expect(w.touch("b")).toEqual(["b", "a"]);
    expect(w.touch("a")).toEqual(["a", "b"]); // 重访移到最前
  });

  it("caps at the window size, evicting the least recent", () => {
    const w = new SubscriptionWindow();
    for (let i = 0; i < SUBSCRIPTION_WINDOW_SIZE + 3; i++) w.touch(`t${i}`);
    const ids = w.current();
    expect(ids).toHaveLength(SUBSCRIPTION_WINDOW_SIZE);
    expect(ids[0]).toBe(`t${SUBSCRIPTION_WINDOW_SIZE + 2}`); // 最新在最前
    expect(ids).not.toContain("t0"); // 最旧的被淘汰
  });

  it("never exceeds the server cap of 16", () => {
    expect(SUBSCRIPTION_WINDOW_SIZE).toBeLessThanOrEqual(16);
  });

  it("reports membership and returns copies", () => {
    const w = new SubscriptionWindow();
    w.touch("a");
    expect(w.has("a")).toBe(true);
    expect(w.has("z")).toBe(false);
    const snap = w.current();
    snap.push("x");
    expect(w.has("x")).toBe(false); // 返回的是副本，外部改动不影响内部
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/subscriptionWindow.test.ts 2>&1 | tail -15`
Expected: 失败（模块不存在）。

- [ ] **Step 3: 实现**

`desktop/src/lib/subscriptionWindow.ts`：

```ts
/**
 * 最近打开的会话（LRU），用作 `thread/subscribe` 的集合。
 *
 * IM 式内容层的核心：订阅集合 = 当前会话 + 最近用过的至多 K 个。切回窗口内
 * 的会话无需重新拉取（其通知一直在累积）；窗口外的会话返回时用
 * `thread/readItems` 补齐。上限固定为 8，天然落在服务端 `MAX_SUBSCRIPTIONS`(16)
 * 之内。
 */
export const SUBSCRIPTION_WINDOW_SIZE = 8;

export class SubscriptionWindow {
  private ids: string[] = [];

  constructor(private readonly max: number = SUBSCRIPTION_WINDOW_SIZE) {}

  /** 把 `id` 移到窗口最前（重访亦然），返回当前集合的快照。 */
  touch(id: string): string[] {
    this.ids = [id, ...this.ids.filter((x) => x !== id)].slice(0, this.max);
    return [...this.ids];
  }

  has(id: string): boolean {
    return this.ids.includes(id);
  }

  current(): string[] {
    return [...this.ids];
  }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/subscriptionWindow.test.ts 2>&1 | tail -15`
Expected: PASS（4/4）。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/subscriptionWindow.ts desktop/src/lib/subscriptionWindow.test.ts
git commit -m "feat(desktop): add subscription window for remote clients"
```

---

### Task 4: 前端：会话视图的 id 记录与去重合并

**Files:**
- Modify: `desktop/src/lib/session.ts`
- Test: `desktop/src/lib/session.test.ts`（若不存在则新建）

**Interfaces:**
- Consumes：`Item`（`./protocol`）。
- Produces：`Session.lastServerItemId: string | null`；`Session.upsertItems(items: Item[]): void`。

- [ ] **Step 1: 写失败测试**

先确认 `desktop/src/lib/session.test.ts` 是否存在：`ls desktop/src/lib/session.test.ts`。
- 若不存在，新建并在文件顶部写 `import { describe, expect, it } from "vitest";` 与 `import { Session } from "./session";` 与 `import type { Item } from "./protocol";`。
- 若存在，在文件末尾追加下面的 `describe` 块。

```ts
describe("Session.readItems merge", () => {
  const user = (id: string, text: string): Item => ({ type: "userMessage", id, text });
  const agent = (id: string, text: string): Item => ({ type: "agentMessage", id, text });

  it("tracks the last server item id for item/started, item/completed and item/delta", () => {
    const s = new Session();
    s.apply({ method: "item/completed", params: { thread_id: "t", item: user("u1", "hi") } } as never);
    expect(s.lastServerItemId).toBe("u1");
    s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "a1", delta: "x" } } as never);
    expect(s.lastServerItemId).toBe("a1");
  });

  it("upsertItems dedupes by id and appends genuinely new items", () => {
    const s = new Session();
    s.apply({ method: "item/completed", params: { thread_id: "t", item: user("u1", "hi") } } as never);
    s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "a1", delta: "hello" } } as never);
    // 补齐返回了重叠的 u1/a1 与一条新的 u2：不得产生重复气泡。
    s.upsertItems([user("u1", "hi"), agent("a1", "hello"), user("u2", "again")]);
    const ids = s.items.map((i) => i.id);
    expect(ids.filter((x) => x === "u1")).toHaveLength(1);
    expect(ids.filter((x) => x === "a1")).toHaveLength(1);
    expect(ids).toContain("u2");
  });

  it("reset clears the recorded server item id", () => {
    const s = new Session();
    s.apply({ method: "item/completed", params: { thread_id: "t", item: user("u1", "hi") } } as never);
    s.reset();
    expect(s.lastServerItemId).toBeNull();
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/session.test.ts 2>&1 | tail -20`
Expected: 失败（`lastServerItemId`/`upsertItems` 未定义）。

- [ ] **Step 3: 实现**

在 `desktop/src/lib/session.ts` 的 `Session` 类里：

1. 字段区（`items` 旁）加：

```ts
  /**
   * 最近一条来自**服务端**的 item id（`item/started|completed|delta`）。
   * `thread/readItems` 用它作为 `afterItemId` 补齐窗口外错过的内容。
   */
  lastServerItemId: string | null = null;
```

2. `reset()` 里加一行 `this.lastServerItemId = null;`。

3. 在 `apply` 的 `case "item/started": case "item/completed":` 分支里，`const item: Item = ...` 之后加 `this.lastServerItemId = item.id;`；
   在 `case "item/delta":` 分支里，取到 `item_id` 后加 `this.lastServerItemId = item_id;`。

4. 新增方法：

```ts
  /**
   * 把一批服务端 item 并入时间线，**按 id 去重**（`thread/readItems` 补齐时，
   * 与已到的实时帧可能有重叠）。同 id 已存在则就地替换，否则追加。
   */
  upsertItems(items: Item[]): void {
    for (const raw of items) {
      const item: Item =
        raw.type === "user_interjection"
          ? { type: "userMessage", id: raw.id, text: raw.text }
          : raw;
      const index = this.items.findIndex((i) => i.id === item.id);
      if (index >= 0) this.items[index] = item;
      else this.items.push(item);
    }
    if (items.length > 0) this.lastServerItemId = items[items.length - 1].id;
  }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/session.test.ts 2>&1 | tail -15`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/session.ts desktop/src/lib/session.test.ts
git commit -m "feat(desktop): track last server item id and upsert (dedupe) read items"
```

---

### Task 5: 前端：远程客户端订阅与冷会话只读补齐

**Files:**
- Modify: `desktop/src/App.tsx`（`selectThread` 附近）
- Test: `desktop/src/App.test.tsx`

**Interfaces:**
- Consumes：`isRemoteClient()`（`./lib/platform`）、`SubscriptionWindow`（`./lib/subscriptionWindow`）、`Session::upsertItems`/`lastServerItemId`。
- Produces：远程客户端在 `selectThread` 时发送 `thread/subscribe`（集合=窗口）；对**正在跑**的冷会话改发 `thread/readItems` 而不是 `thread/resume`。桌面**不发**这两个方法。

- [ ] **Step 1: 写失败测试**

在 `desktop/src/App.test.tsx` 的 `describe` 内追加（沿用既有 harness：`clients[0].requests`、`state.threads`、`state.platformUa`、`localStorage`）：

```tsx
  it("subscribes the warm window on the remote client, and never on desktop", async () => {
    // 远程：写入 remote 配置 → isRemoteClient() 为真。
    state.platformUa = IPHONE_UA;
    localStorage.setItem("yi-agent.remote", JSON.stringify({ url: "wss://relay/ws", token: "t" }));
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/subscribe")).toBe(true),
    );
    const sub = clients[0].requests.find((r) => r.method === "thread/subscribe")!;
    expect((sub.params as { threadIds: string[] }).threadIds).toContain("t1");
  });

  it("does not subscribe from the desktop build", async () => {
    state.platformUa = ""; // 桌面
    render(<App />);
    await waitFor(() => expect(clients[0].requests.some((r) => r.method === "thread/listAll")).toBe(true));
    expect(clients[0].requests.some((r) => r.method === "thread/subscribe")).toBe(false);
    expect(clients[0].requests.some((r) => r.method === "thread/readItems")).toBe(false);
  });

  it("backfills with readItems (not resume) when returning to a running cold thread", async () => {
    state.platformUa = IPHONE_UA;
    localStorage.setItem("yi-agent.remote", JSON.stringify({ url: "wss://relay/ws", token: "t" }));
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    state.listStatus = "running"; // listAll 报告 t1/t2 都在跑
    render(<App />);
    await waitFor(() => expect(clients[0].requests.some((r) => r.method === "thread/subscribe")).toBe(true));
    // 切到一个正在跑的冷会话：必须走 readItems，且**不** resume（避免打断回合）。
    const beforeResume = clients[0].requests.filter((r) => r.method === "thread/resume").length;
    await act(async () => {
      fireEvent.click(screen.getByText("two"));
    });
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/readItems")).toBe(true),
    );
    const afterResume = clients[0].requests.filter((r) => r.method === "thread/resume").length;
    expect(afterResume).toBe(beforeResume); // 没有新增 resume
  });
```

（若 `fireEvent`/`act` 未在文件顶部导入，按文件既有导入风格补齐：`import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";`。若点击会话的定位方式与文件既有用例不同，沿用该文件里"点击侧栏会话"的既有写法。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/App.test.tsx 2>&1 | tail -25`
Expected: 失败（未发送 `thread/subscribe`）。

- [ ] **Step 3: 实现**

在 `App.tsx`：

1. 顶部导入加：

```tsx
import { SubscriptionWindow } from "./lib/subscriptionWindow";
```

（`isRemoteClient` 已在 `App.tsx` 从 `./lib/platform` 导入；若未导入则加 `isRemoteClient`。）

2. 在组件内 refs 区加：

```tsx
  // 远程客户端的 warm 订阅窗口（桌面不使用）。
  const subWindow = useRef(new SubscriptionWindow());
```

3. 在 `selectThread` 里，**紧接 `void refreshSubagents(id);` 之后、`if (warm.current.has(id) || inFlightResume.current.has(id)) return;` 之前**，插入远程分支（这样 `refreshSubagents` 仍会执行，只是跳过后续的 resume 路径）：

```tsx
    // 远程客户端（iOS）：IM 式订阅。列表层状态由服务端恒推，这里只更新内容层。
    if (isRemoteClient()) {
      const inWindow = subWindow.current.has(id);
      const win = subWindow.current.touch(id);
      const c0 = clientRef.current;
      if (c0) {
        if (!inWindow && running) {
          const after = store.peek(id)?.session.lastServerItemId ?? null;
          void (async () => {
            try {
              const r = await c0.request<{ items: import("./lib/protocol").Item[] }>(
                "thread/readItems",
                after ? { threadId: id, afterItemId: after } : { threadId: id },
              );
              const v = store.peek(id);
              if (v) {
                v.session.upsertItems(r.items);
                force((n) => n + 1);
              }
            } catch {
              // 保留已有内容；不因补齐失败清空视图。
            }
          })();
        }
        void c0.request("thread/subscribe", { threadIds: win }).catch(() => {});
      }
      // 窗口内/正在跑的会话不再走下面的 resume 路径。
      if (inWindow || running) return;
    }
```

> 注意：`running` 必须在上面的 `if (c0)` **之前**声明（`const running = ...`），`if (c0)` 只包住网络调用；早退 `if (inWindow || running) return;` 用同一个 `running`。

4. 桌面路径**不改**：`isRemoteClient()` 为假时整段跳过，后续 `warm`/`resume` 逻辑保持原样。

- [ ] **Step 4: 跑测试确认通过 + 门禁**

Run: `cd desktop && npx vitest run 2>&1 | tail -8 && npx tsc --noEmit && echo TSC_CLEAN`
Expected: 全绿（基线 441 + 新增 3），tsc 干净。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): subscribe warm window and backfill running threads remotely"
```

---

### Task 6: 文档

**Files:**
- Modify: `README.md`、`README.en.md`

**Interfaces:** 无代码。

- [ ] **Step 1: README.md 记一句**

在"远程连接（实验性）"段里，`thread/subscribe` 那句之后追加：

> 手机端按 IM 模式工作：**列表层**（`thread/started`、`thread/status/updated`）实时推给
> 所有设备，**内容层**只推你打开过（最近 8 个）的会话；返回很久没看的会话时用只读
> `thread/readItems` 补齐，**不会打断**它正在跑的回合。见
> [接线设计](docs/superpowers/specs/2026-10-02-thread-subscription-wiring-design.md)。

- [ ] **Step 2: README.en.md 对应英文一句**

在 Remote access 段的订阅那句之后追加：

> On the phone the client works like an IM app: the **list layer**
> (`thread/started`, `thread/status/updated`) streams to every device, while
> the **content layer** only streams the sessions you opened (the most recent
> 8); returning to an older session backfills it with the read-only
> `thread/readItems` — without interrupting a turn that is still running. See
> the [wiring design](docs/superpowers/specs/2026-10-02-thread-subscription-wiring-design.md).

- [ ] **Step 3: 提交**

```bash
git add README.md README.en.md
git commit -m "docs: describe IM-style list/content split for remote clients"
```

---

## Self-Review

**Spec coverage**
- G1 列表层恒推 → Task 1（`Delivery` + `write_notification`）。
- G2 内容层按需（前端接线）→ Task 3 + Task 5。
- G3 warm 窗口 K=8 → Task 3（窗口）+ Task 5（订阅集合）。
- G4 只读补齐 `thread/readItems` → Task 2（服务端）+ Task 4（去重）+ Task 5（远程补齐）。
- G5 零回归 → Task 5 的桌面分支不改 + 两处门禁。
- §5 文档 → Task 6。

**Placeholder scan**：无 TBD/TODO；每个代码步给出实际代码与命令。

**Type consistency**：`Delivery`、`Notification::delivery`、`thread/readItems`、`item_id`、`SubscriptionWindow`（`touch/has/current`、`SUBSCRIPTION_WINDOW_SIZE`）、`Session::lastServerItemId`/`upsertItems` 在各 Task 间名称一致。

**已知偏差（Task 5 内已标注）**：`running` 变量需在 `if (c0)` 之外声明；实现者按 Step 3 的注意项调整作用域。
