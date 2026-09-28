# 并行多 thread 执行 + 每 thread 状态指示 Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 让 desktop app 支持多个 thread 并行运行并可在其间自由切换，同时在侧栏以状态徽标 + 未读注意力点显示每个 thread 的运行 / 待确认状态。

**Architecture:** 后端每 thread 已有独立 driver task（本就并行），本计划只做两件事：(1) 在协议层新增 `ThreadStatus`（`idle|running|awaiting_approval`），由 driver 经共享句柄在状态跃迁点推送 `thread/status/updated` 并写进 `thread/list(:All)`；(2) 前端把单一 `Session` 拆成按 thread 隔离的 `ThreadStore`（会话 / 状态 / 未读 / 审批），把全局 `busy` 闸门下沉到 per-thread，并让「切换视图」与 `thread/resume` 解耦（warm thread 绝不 re-resume，避免打断运行中的 turn）。

**Tech Stack:** Rust（`yi-agent-app-server`：tokio + serde_json + 内联 `#[cfg(test)]` harness）、TypeScript（React 19 + Vite + Vitest + jsdom + @testing-library/react）。

**设计依据：** `docs/plans/2026-09-28-parallel-threads-status-design.md`。

---

## 约定与前置

- **工作树**：所有改动在 worktree 内进行（`git worktree add .worktrees/<name> -b <type>/<name>`）。严禁在 `main` 上直接改。
- **Rust 测试**：只在单个 crate 内跑，避免 OOM / 死锁。跑前 `ps aux | grep cargo` 确认无残留 cargo 进程（见 `CLAUDE.md`）。命令：
  `cargo test -p yi-agent-app-server --lib`
- **Rust 格式**：提交前 `cd yi-agent-rs && cargo fmt --all`。
- **前端测试**：`cd desktop && npx vitest run`；类型检查 `cd desktop && npx tsc --noEmit`。
- **提交规范**：conventional commits，首行 ≤72 字符，**不写** `Co-Authored-By`。
- **行号会随编辑漂移**：下文给出的行号是撰写时的定位参考；改动时以「搜索锚点」为准。

---

## Task 1: 协议新增 `ThreadStatus` 与 `thread/status/updated` 通知

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`

**Step 1: 写失败测试**

在 `protocol.rs` 的 `mod tests`（约 `:220`）内追加：

```rust
    #[test]
    fn thread_status_serializes_snake_case() {
        assert_eq!(serde_json::to_value(ThreadStatus::Idle).unwrap(), serde_json::json!("idle"));
        assert_eq!(
            serde_json::to_value(ThreadStatus::Running).unwrap(),
            serde_json::json!("running")
        );
        assert_eq!(
            serde_json::to_value(ThreadStatus::AwaitingApproval).unwrap(),
            serde_json::json!("awaiting_approval")
        );
    }

    #[test]
    fn thread_status_notification_serializes_with_method_tag() {
        let n = Notification::ThreadStatusUpdated {
            thread_id: "t1".into(),
            status: ThreadStatus::AwaitingApproval,
        };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "thread/status/updated");
        assert_eq!(v["params"]["thread_id"], "t1");
        assert_eq!(v["params"]["status"], "awaiting_approval");
    }
```

**Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_status`
Expected: 编译失败（`ThreadStatus` / `ThreadStatusUpdated` 未定义）。

**Step 3: 实现**

在 `protocol.rs` 中，紧跟 `TurnStatus`（约 `:183`）之后新增：

```rust
/// thread 级实时状态。`failed` 刻意缺席：失败是事件不是状态，失败后 thread
/// 立刻回 `Idle`；"失败未读"由前端从 `turn/completed.params.status` 自行表达。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    Idle,
    Running,
    AwaitingApproval,
}
```

在 `enum Notification`（约 `:107`）的 `TurnStarted` 变体之后插入：

```rust
    #[serde(rename = "thread/status/updated")]
    ThreadStatusUpdated { thread_id: String, status: ThreadStatus },
```

**Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_status`
Expected: PASS（2 passed）。

**Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git -C .. add yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs
git -C .. commit -m "feat(app-server): add ThreadStatus and thread/status/updated"
```

---

## Task 2: `ThreadSession` 增加共享状态句柄

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/session.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（两处构造：`thread/start` 约 `:430`、`thread/resume` 约 `:584`）

**Step 1: 加字段与构造函数**

`session.rs` 顶部（`use std::sync::Arc;` 之后）追加：

```rust
use std::sync::Mutex;

use crate::protocol::ThreadStatus;
```

在 `struct ThreadSession` 内追加字段：

```rust
    /// 该 thread 的实时状态。driver 与主循环共享同一句柄：driver 更新并推送
    /// `thread/status/updated`，主循环在 `thread/list(:All)` 里读取。用共享句柄
    /// 而非 `TurnEvent`，避免扰动 `interrupt_and_wait_for_persist` 的收事件循环。
    pub status: Arc<Mutex<ThreadStatus>>,
```

在文件末尾加：

```rust
impl ThreadSession {
    /// 新建一个共享状态句柄（初值 `Idle`）。
    pub fn new_status() -> Arc<Mutex<ThreadStatus>> {
        Arc::new(Mutex::new(ThreadStatus::Idle))
    }
}
```

**Step 2: 更新两处构造点**

在 `server.rs` 的 `thread/start`（约 `:428`）与 `thread/resume`（约 `:582`）的 `ThreadSession { ... }` 字面量中，各加一行 `status: crate::session::ThreadSession::new_status(),`。

> 注意：`session.rs` 用的是 `std::sync::Mutex`；`server.rs` 已 `use tokio::sync::Mutex`。两处**不要**混用——`server.rs` 里对状态的加锁必须写全 `s.status.lock()` 且**不得跨 `.await` 持锁**。

**Step 3: 编译**

Run: `cd yi-agent-rs && cargo build -p yi-agent-app-server`
Expected: 编译通过（可能有未使用字段警告，下一步用到）。

**Step 4: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git -C .. add yi-agent-rs/crates/yi-agent-app-server/src/session.rs yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git -C .. commit -m "feat(app-server): give ThreadSession a shared status handle"
```

---

## Task 3: driver 与主循环在跃迁点写状态 + `thread/list(:All)` 带 status

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`

**Step 1: 加一个统一的状态更新 helper**

在 `write_notification`（约 `:936`）附近新增：

```rust
/// 更新共享状态句柄并推送 `thread/status/updated`。
///
/// 加锁是同步的、不跨 `.await`；锁在写通知前即释放。
async fn update_status<W: tokio::io::AsyncWrite + Unpin>(
    writer: &MessageWriter<W>,
    handle: &Mutex<ThreadStatus>,
    thread_id: &str,
    next: ThreadStatus,
) -> anyhow::Result<()> {
    *handle.lock().unwrap() = next;
    write_notification(
        writer,
        &Notification::ThreadStatusUpdated {
            thread_id: thread_id.to_string(),
            status: next,
        },
    )
    .await
}

/// 读某 thread 的当前状态；不在内存（cold thread）一律 `Idle`。
fn thread_status(threads: &HashMap<String, ThreadSession>, thread_id: &str) -> ThreadStatus {
    threads
        .get(thread_id)
        .map(|s| *s.status.lock().unwrap())
        .unwrap_or(ThreadStatus::Idle)
}
```

在 `server.rs` 顶部 `use` 区加入 `crate::protocol::ThreadStatus`（若尚未导入；检查现有 `use crate::protocol::...`）。

**Step 2: `thread/start` 受理时置 `running` 并推送**

在 `turn/start` 分支内，`session.active_turn_id = Some(turn_id.clone());` 所在的**内层作用域**（约 `:813-832`）把返回值改成同时带出状态句柄：

```rust
                        let (prompt_tx, status_handle) = {
                            let Some(session) = threads.get_mut(&thread_id) else {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                                continue;
                            };
                            if session.active_turn_id.is_some() {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::turn_in_progress(&thread_id)),
                                )
                                .await?;
                                continue;
                            }
                            session.active_turn_id = Some(turn_id.clone());
                            (session.prompt_tx.clone(), Arc::clone(&session.status))
                        };
```

然后在写出 `TurnStarted` 通知之后（约 `:842` 之后）插入：

```rust
                        update_status(&writer, &status_handle, &thread_id, ThreadStatus::Running)
                            .await?;
```

**Step 3: `thread/list` / `thread/listAll` 带 `status`**

在 `thread/list`（约 `:304`）与 `thread/listAll`（约 `:342`）的 `json!({...})` 里，各追加一行：

```rust
                                        "status": thread_status(&threads, &m.thread_id),
```

> 借用检查：这两处的闭包在 `threads` 的**不可变借用**下运行（此处无 `&mut threads`），可直接读 `thread_status(&threads, ..)`。

**Step 4: driver 在审批 / 恢复 / 结束时更新状态**

在 `run_thread_driver`（约 `:1039`）签名参数列表末尾追加 `status: Arc<Mutex<ThreadStatus>>,`；并把 `thread/start`（约 `:447`）与 `thread/resume`（约 `:599`）两处 `tokio::spawn(run_thread_driver(...))` 的实参末尾各加一行：

```rust
                            Arc::clone(&store_status),
```

（构造 `ThreadSession` 时把 `new_status()` 的结果先绑给局部量 `let store_status = ThreadSession::new_status();`，既放进 `ThreadSession { status: Arc::clone(&store_status), .. }` 也传给 driver。）

driver 内的改动：

- **`agent.run()` 失败路径**（约 `:1077-1084`，`continue` 之前）：
  ```rust
                let _ = update_status(&writer, &status, &thread_id, ThreadStatus::Idle).await;
  ```
- **审批等待开始**（约 `:1120`，写出反向请求成功之后）：
  ```rust
                            let _ = update_status(
                                &writer,
                                &status,
                                &thread_id,
                                ThreadStatus::AwaitingApproval,
                            )
                            .await;
  ```
- **审批决定返回 / 超时 / 被中断**（约 `:1143`，`let decision = loop { ... };` 之后）：
  ```rust
                            let _ = update_status(&writer, &status, &thread_id, ThreadStatus::Running)
                                .await;
  ```
- **turn 落盘之后、发 `Finished` 之前**（约 `:1216`）：
  ```rust
        let _ = update_status(&writer, &status, &thread_id, ThreadStatus::Idle).await;
  ```

**Step 5: 跑 app-server 全量单测（回归）**

Run: `ps aux | grep cargo`（确认无残留）→ `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: 全绿。**若有个别测试断言了精确的通知帧序列而失败**，说明它假设了「无 status 帧」；逐个改为「循环读到目标 method、忽略其它帧」的写法（参照 `read_until_approval` 的模式），**不要**放宽断言语义。

**Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git -C .. add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git -C .. commit -m "feat(app-server): emit and expose per-thread ThreadStatus"
```

---

## Task 4: 服务端测试 —— 状态跃迁、并行证明、超时离开待审批

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`mod tests`，约 `:1319` 起）

**Step 1: 写失败测试**

在 `mod tests` 末尾追加（复用现有 `Harness` / `build_test_agent` / `build_slow_agent` / `build_permission_agent` / `start_thread` / `read_until_approval` / `read_thread_start_response`）：

```rust
    /// 读到下一个指定 method 的通知（忽略其它帧）。
    async fn read_until_method(h: &mut Harness, method: &str) -> serde_json::Value {
        for _ in 0..40 {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some(method) {
                return v;
            }
        }
        panic!("expected a {method} notification");
    }

    /// 正常一轮结束后，状态序列为 running → idle。
    #[tokio::test(flavor = "multi_thread")]
    async fn turn_emits_running_then_idle_status() {
        let mut h = Harness::new();
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;

        let running = read_until_method(&mut h, "thread/status/updated").await;
        assert_eq!(running["params"]["thread_id"], tid);
        assert_eq!(running["params"]["status"], "running");

        let idle = loop {
            let v = read_until_method(&mut h, "thread/status/updated").await;
            if v["params"]["status"] == "idle" {
                break v;
            }
        };
        assert_eq!(idle["params"]["thread_id"], tid);
        h.shutdown().await;
    }

    /// 两个 thread 的 turn 可同时在跑：各自的 listAll 状态同为 running。
    #[tokio::test(flavor = "multi_thread")]
    async fn two_threads_run_turns_concurrently() {
        let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;
        let a = read_thread_start_response(&mut h, 2).await;
        h.send(r#"{"jsonrpc":"2.0","id":4,"method":"thread/start","params":{}}"#)
            .await;
        let b = read_thread_start_response(&mut h, 4).await;

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{a}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"turn/start","params":{{"threadId":"{b}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;

        // 两个 turn/started 都应到达（无跨 thread 的 -32012）。
        let mut started = std::collections::HashSet::new();
        for _ in 0..20 {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/started") {
                started.insert(v["params"]["thread_id"].as_str().unwrap().to_string());
            }
            if started.contains(&a) && started.contains(&b) {
                break;
            }
        }
        assert!(started.contains(&a) && started.contains(&b), "both turns must start: {started:?}");

        // 此刻 listAll 里两个 thread 都是 running。
        h.send(r#"{"jsonrpc":"2.0","id":6,"method":"thread/listAll","params":{}}"#)
            .await;
        let all = loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(6)) {
                break v;
            }
        };
        let status_of = |id: &str| -> String {
            all["result"]["groups"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|g| g["threads"].as_array().unwrap())
                .find(|t| t["thread_id"] == id)
                .map(|t| t["status"].as_str().unwrap().to_string())
                .expect("thread must be listed")
        };
        assert_eq!(status_of(&a), "running");
        assert_eq!(status_of(&b), "running");

        // 收尾：中断两个 turn 以便优雅退出。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":7,"method":"turn/interrupt","params":{{"threadId":"{a}"}}}}"#
        ))
        .await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":8,"method":"turn/interrupt","params":{{"threadId":"{b}"}}}}"#
        ))
        .await;
        h.shutdown().await;
    }

    /// 审批超时后状态必须离开 awaiting_approval（前端据此清掉残留审批框）。
    #[tokio::test(flavor = "multi_thread")]
    async fn approval_timeout_leaves_awaiting_status() {
        let mut h = Harness::with_factory(build_permission_agent, Duration::from_millis(150));
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;

        // 应先看到 awaiting_approval。
        let mut saw_awaiting = false;
        // 之后必须离开 awaiting_approval（超时 → 回 running → idle）。
        let mut left_awaiting = false;
        for _ in 0..60 {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("thread/status/updated") {
                match v["params"]["status"].as_str().unwrap() {
                    "awaiting_approval" => saw_awaiting = true,
                    _ if saw_awaiting => {
                        left_awaiting = true;
                        break;
                    }
                    _ => {}
                }
            }
        }
        assert!(saw_awaiting, "must enter awaiting_approval");
        assert!(left_awaiting, "must leave awaiting_approval after timeout");
        h.shutdown().await;
    }

    /// cold thread（未 resume）在 listAll 里状态为 idle。
    #[tokio::test(flavor = "multi_thread")]
    async fn cold_thread_reports_idle() {
        let mut h = Harness::new();
        let tid = start_thread(&mut h).await; // 已 start，但未 resume、未起 turn
        h.send(r#"{"jsonrpc":"2.0","id":9,"method":"thread/listAll","params":{}}"#)
            .await;
        let all = loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(9)) {
                break v;
            }
        };
        let listed = all["result"]["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["threads"].as_array().unwrap())
            .find(|t| t["thread_id"] == tid.as_str())
            .expect("thread must be listed");
        assert_eq!(listed["status"], "idle");
        h.shutdown().await;
    }
```

**Step 2: 跑测试确认失败（在 Task 3 之前跑会失败）**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib status concurrent cold_thread`
Expected: 若 Task 3 已完成则直接 PASS；否则失败。本 Task 应在 Task 3 完成后执行，预期 PASS。

**Step 3: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: 全绿（含新增 4 个）。

**Step 4: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git -C .. add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git -C .. commit -m "test(app-server): cover thread status transitions and parallelism"
```

---

## Task 5: 前端协议类型

**Files:**
- Modify: `desktop/src/lib/protocol.ts`

**Step 1: 加类型**

在 `export type TurnStatus = ...;`（`:41`）之后加：

```ts
/** thread 级实时状态；服务端权威（`thread/status/updated` / `thread/list(:All)`）。 */
export type ThreadStatus = "idle" | "running" | "awaiting_approval";
```

在 `Notification` 联合（`:43`）里追加：

```ts
  | { method: "thread/status/updated"; params: { thread_id: string; status: ThreadStatus } }
```

在 `ThreadSummary`（`:102`）内加：

```ts
  /** 服务端权威状态；旧服务端缺省视为 "idle"。 */
  status?: ThreadStatus;
```

**Step 2: 类型检查**

Run: `cd desktop && npx tsc --noEmit`
Expected: 通过。

**Step 3: 提交**

```bash
git add desktop/src/lib/protocol.ts
git commit -m "feat(desktop): add ThreadStatus wire types"
```

---

## Task 6: 前端 `ThreadStore`（per-thread 会话 / 状态 / 未读 / 审批）

**Files:**
- Create: `desktop/src/lib/threadStore.ts`
- Test: `desktop/src/lib/threadStore.test.ts`

**Step 1: 写失败测试**

`desktop/src/lib/threadStore.test.ts`：

```ts
import { describe, it, expect } from "vitest";
import { ThreadStore } from "./threadStore";
import type { ApprovalRequest, Notification, ThreadSummary } from "./protocol";

const summary = (id: string, status?: ThreadSummary["status"]): ThreadSummary => ({
  thread_id: id,
  cwd: "/w",
  model: "m",
  created_at: 0,
  updated_at: 0,
  title: null,
  status,
});

const approval = (id: string, threadId: string): ApprovalRequest => ({
  id,
  params: {
    thread_id: threadId,
    turn_id: "u1",
    request_id: 1,
    tool_name: "bash",
    tool_input: {},
    prefix_suggestion: null,
    kind: "Normal",
  },
});

describe("ThreadStore", () => {
  it("routes notifications to the matching thread only", () => {
    const s = new ThreadStore();
    s.applyNotification({
      method: "item/started",
      params: { thread_id: "a", item: { type: "agentMessage", id: "x", text: "" } },
    });
    s.applyNotification({
      method: "item/delta",
      params: { thread_id: "b", item_id: "y", delta: "hi" },
    });
    expect(s.view("a").session.items).toHaveLength(1);
    expect(s.view("b").session.items).toHaveLength(1);
    expect(s.view("a").session.items[0]).toMatchObject({ id: "x" });
    expect(s.view("b").session.items[0]).toMatchObject({ id: "y" });
  });

  it("marks unread on turn/completed for a non-current thread, not the current one", () => {
    const s = new ThreadStore();
    s.select("a");
    const done = (t: string): Notification => ({
      method: "turn/completed",
      params: { thread_id: t, turn_id: "u1", status: "completed" },
    });
    s.applyNotification(done("a"));
    s.applyNotification(done("b"));
    expect(s.view("a").unread).toBe(false);
    expect(s.view("b").unread).toBe(true);
    expect(s.view("b").session.lastStatus).toBe("completed");
  });

  it("clears unread when the thread is selected", () => {
    const s = new ThreadStore();
    s.applyNotification({
      method: "turn/completed",
      params: { thread_id: "b", turn_id: "u1", status: "failed" },
    });
    expect(s.view("b").unread).toBe(true);
    s.select("b");
    expect(s.view("b").unread).toBe(false);
  });

  it("applies thread/status/updated and clears approval once it leaves awaiting", () => {
    const s = new ThreadStore();
    s.setApproval(approval("perm-1", "a"));
    expect(s.view("a").approval).not.toBeNull();
    s.applyNotification({
      method: "thread/status/updated",
      params: { thread_id: "a", status: "awaiting_approval" },
    });
    expect(s.view("a").status).toBe("awaiting_approval");
    expect(s.view("a").approval).not.toBeNull();
    s.applyNotification({
      method: "thread/status/updated",
      params: { thread_id: "a", status: "running" },
    });
    expect(s.view("a").status).toBe("running");
    expect(s.view("a").approval).toBeNull();
  });

  it("seeds status and cwd/model from a listing snapshot", () => {
    const s = new ThreadStore();
    s.seed([summary("a", "running"), summary("b")]);
    expect(s.view("a").status).toBe("running");
    expect(s.view("b").status).toBe("idle");
    expect(s.view("a").info).toEqual({ cwd: "/w", model: "m" });
  });

  it("reports pending approvals for threads other than the current one", () => {
    const s = new ThreadStore();
    s.select("a");
    s.setApproval(approval("perm-1", "a"));
    s.setApproval(approval("perm-2", "b"));
    expect(s.pendingApprovalsElsewhere().map((r) => r.id)).toEqual(["perm-2"]);
  });

  it("routes a thread-less error to the current session", () => {
    const s = new ThreadStore();
    s.select("a");
    s.applyNotification({ method: "error", params: { message: "boom" } });
    expect(s.view("a").session.lastError).toBe("boom");
  });

  it("drop removes all state for a thread", () => {
    const s = new ThreadStore();
    s.select("a");
    s.setApproval(approval("perm-1", "a"));
    s.drop("a");
    expect(s.currentId).toBeNull();
    expect(s.peek("a")).toBeUndefined();
  });
});
```

**Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/threadStore.test.ts`
Expected: FAIL（模块不存在）。

**Step 3: 实现 `threadStore.ts`**

```ts
import { Session } from "./session";
import type { ApprovalRequest, Notification, ThreadStatus, ThreadSummary } from "./protocol";

/** 单个 thread 的客户端视图：会话 + 服务端权威状态 + 未读 + 待处理审批。 */
export interface ThreadView {
  session: Session;
  status: ThreadStatus;
  /** 未被查看时收到过 turn/completed → true；打开即清除。 */
  unread: boolean;
  approval: ApprovalRequest | null;
  info: { cwd: string; model: string } | null;
}

/**
 * 按 thread 隔离的客户端状态机。通知按 `params.thread_id` 路由到各自的
 * `Session`，因此后台 thread 的流式输出照常累积，切回去即最新。
 *
 * 可变实例：调用方在每次变更后自行触发重渲染（沿用 `App.tsx` 的 `force` 模式）。
 */
export class ThreadStore {
  private views = new Map<string, ThreadView>();
  currentId: string | null = null;

  private create(): ThreadView {
    return { session: new Session(), status: "idle", unread: false, approval: null, info: null };
  }

  /** 取（必要时创建）某 thread 的视图。 */
  view(id: string): ThreadView {
    let v = this.views.get(id);
    if (!v) {
      v = this.create();
      this.views.set(id, v);
    }
    return v;
  }

  /** 只读查询，不创建。 */
  peek(id: string): ThreadView | undefined {
    return this.views.get(id);
  }

  current(): ThreadView | null {
    return this.currentId ? this.view(this.currentId) : null;
  }

  /** 切到某 thread 并清除其未读。 */
  select(id: string): void {
    this.currentId = id;
    this.view(id).unread = false;
  }

  /** 返回当前 thread 的 id 列表（供侧栏派生状态用）。 */
  ids(): string[] {
    return [...this.views.keys()];
  }

  /**
   * 用 `thread/listAll` 快照播种状态与 cwd/model。**不改动会话内容**——
   * 会话只由通知累积。快照覆盖状态是安全的：服务端既是快照也是实时流的权威，
   * 且每次 `turn/completed` 后都会重新拉取快照。
   */
  seed(threads: ThreadSummary[]): void {
    for (const t of threads) {
      const v = this.view(t.thread_id);
      v.status = t.status ?? "idle";
      v.info = { cwd: t.cwd, model: t.model };
    }
  }

  /** 按 `thread_id` 路由一条通知。 */
  applyNotification(n: Notification): void {
    if (n.method === "thread/status/updated") {
      const v = this.view(n.params.thread_id);
      v.status = n.params.status;
      // 离开 awaiting_approval（决定 / 超时 / 中断）即清掉可能残留的审批框，
      // 否则超时后前端会留下一个点不掉的模态。
      if (n.params.status !== "awaiting_approval") v.approval = null;
      return;
    }
    if (n.method === "error") {
      // 无 thread 归属的全局错误归当前 thread。
      this.current()?.session.apply(n);
      return;
    }
    const id = (n.params as { thread_id: string }).thread_id;
    const v = this.view(id);
    v.session.apply(n);
    if (n.method === "thread/started") v.info = { cwd: n.params.cwd, model: n.params.model };
    if (n.method === "turn/completed" && id !== this.currentId) v.unread = true;
  }

  setApproval(r: ApprovalRequest): void {
    this.view(r.params.thread_id).approval = r;
  }

  clearApproval(threadId: string): void {
    const v = this.views.get(threadId);
    if (v) v.approval = null;
  }

  /** 待确认、且不是当前查看的 thread（供全局横幅）。 */
  pendingApprovalsElsewhere(): ApprovalRequest[] {
    const out: ApprovalRequest[] = [];
    for (const [id, v] of this.views) {
      if (v.approval && id !== this.currentId) out.push(v.approval);
    }
    return out;
  }

  /** 删除某 thread 的全部客户端状态。 */
  drop(id: string): void {
    this.views.delete(id);
    if (this.currentId === id) this.currentId = null;
  }
}
```

**Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/threadStore.test.ts`
Expected: PASS（8 passed）。

**Step 5: 提交**

```bash
git add desktop/src/lib/threadStore.ts desktop/src/lib/threadStore.test.ts
git commit -m "feat(desktop): add per-thread ThreadStore"
```

---

## Task 7: `ThreadSidebar` 状态徽标 + 未读点，去掉全局 busy 闸门

**Files:**
- Modify: `desktop/src/components/ThreadSidebar.tsx`
- Test: `desktop/src/components/ThreadSidebar.test.tsx`

**Step 1: 写失败测试**

在 `ThreadSidebar.test.tsx` 的 `renderSidebar` 辅助里，把 `busy: false` 换成新 props：

```tsx
    statuses: new Map(),
    unread: new Map(),
```

并把 `ComponentProps` 导入保持不变。然后追加：

```tsx
describe("ThreadSidebar status", () => {
  it("shows a spinner for a running thread", () => {
    const { container } = renderSidebar({ statuses: new Map([["1", "running"]]) });
    expect(container.querySelector('[aria-label="Running"]')).not.toBeNull();
  });

  it("shows an awaiting-approval badge", () => {
    const { container } = renderSidebar({ statuses: new Map([["1", "awaiting_approval"]]) });
    expect(container.querySelector('[aria-label="Awaiting approval"]')).not.toBeNull();
  });

  it("shows an unread dot and clears it when the thread is current", () => {
    const { container } = renderSidebar({
      unread: new Map([["1", "completed"]]),
      currentId: null,
    });
    expect(container.querySelector('[aria-label="Unread"]')).not.toBeNull();

    const shown = renderSidebar({ unread: new Map([["1", "completed"]]), currentId: "1" });
    expect(shown.container.querySelector('[aria-label="Unread"]')).toBeNull();
  });

  it("allows selecting a thread while another one is running", () => {
    const onSelect = vi.fn();
    const { container } = renderSidebar({
      statuses: new Map([["1", "running"]]),
      onSelect,
    });
    fireEvent.click(screen.getByText("beta-thread"));
    expect(onSelect).toHaveBeenCalledWith("2");
  });
});
```

> 需要 `renderSidebar` 暴露 `screen`；文件已 `import { render, fireEvent, cleanup }`，补 `screen`。

**Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: FAIL（新 props 未实现 / 断言找不到）。

**Step 3: 实现**

`ThreadSidebar.tsx` 改动：

1. 导入类型：`import type { ThreadSummary, ThreadStatus, TurnStatus, Workspace, WorkspaceGroup } from "../lib/protocol";`
2. props 类型：删除 `busy: boolean;`，新增：
   ```tsx
     statuses: Map<string, ThreadStatus>;
     /** thread_id → 最近一轮结束状态；含 key 即有未读点。 */
     unread: Map<string, TurnStatus>;
   ```
3. 删除 props 解构里的 `busy`，加入 `statuses, unread`。
4. `renderThread`（约 `:134`）内：
   - 删掉 `onClick` 里的 `if (busy || editingId) return;` 与 `busy`（保留 `editingId` 与 `e.detail > 1` 判断）。
   - 删掉 `className` 中的 `${busy ? "cursor-not-allowed opacity-60" : "cursor-pointer"}`，改为静态 `cursor-pointer`。
   - 删掉双击重命名处的 `if (busy) return;`。
   - 删掉 Delete 按钮的 `disabled={busy}`。
   - 计算 `const st = statuses.get(t.thread_id) ?? "idle";` 与 `const un = unread.get(t.thread_id);`。
   - 标题行改为：
     ```tsx
             <div className="min-w-0 flex-1">
               <div className="flex items-center gap-1.5">
                 {st === "running" && (
                   <span
                     aria-label="Running"
                     className="size-3 shrink-0 animate-spin rounded-full border-2 border-neutral-600 border-t-neutral-300"
                   />
                 )}
                 {st === "awaiting_approval" && (
                   <span
                     aria-label="Awaiting approval"
                     className="size-2 shrink-0 rounded-full bg-amber-400"
                   />
                 )}
                 <div
                   className="truncate"
                   title={t.title ?? t.thread_id}
                   onDoubleClick={() => {
                     setEditingId(t.thread_id);
                     setDraft(t.title ?? "");
                   }}
                 >
                   {t.title ?? "(untitled)"}
                 </div>
               </div>
               <div className="text-xs text-neutral-600">{relativeTime(t.updated_at)}</div>
             </div>
             {un && (
               <span
                 aria-label="Unread"
                 title="Unread result"
                 className={`size-2 shrink-0 rounded-full ${unreadDotClass(un)}`}
               />
             )}
     ```
   - 文件内加辅助：
     ```tsx
     function unreadDotClass(status: TurnStatus): string {
       if (status === "failed") return "bg-red-400";
       if (status === "interrupted") return "bg-neutral-400";
       return "bg-blue-400";
     }
     ```
5. 删除其余 `busy` 引用：New thread 按钮 `disabled={busy}`（约 `:216`）、组头 `onContextMenu`/`onKeyDown` 的 `if (busy) return;`（约 `:277,288`）、"New thread here"/"Remove from list" 的 `disabled={busy}`（约 `:324,336`）。

**Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: PASS（含新增 4 个）。

**Step 5: 提交**

```bash
git add desktop/src/components/ThreadSidebar.tsx desktop/src/components/ThreadSidebar.test.tsx
git commit -m "feat(desktop): sidebar status badge, unread dot, no global gate"
```

---

## Task 8: 全局审批横幅组件

**Files:**
- Create: `desktop/src/components/ApprovalBanner.tsx`
- Test: `desktop/src/components/ApprovalBanner.test.tsx`

**Step 1: 写失败测试**

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, fireEvent, cleanup } from "@testing-library/react";
import { ApprovalBanner } from "./ApprovalBanner";
import type { ApprovalRequest } from "../lib/protocol";

afterEach(cleanup);

const req = (id: string, threadId: string, tool: string): ApprovalRequest => ({
  id,
  params: {
    thread_id: threadId,
    turn_id: "u1",
    request_id: 1,
    tool_name: tool,
    tool_input: {},
    prefix_suggestion: null,
    kind: "Normal",
  },
});

describe("ApprovalBanner", () => {
  it("lists background approvals and jumps to a thread", () => {
    const onJump = vi.fn();
    render(
      <ApprovalBanner items={[req("p1", "t1", "bash")]} onJump={onJump} onDismiss={vi.fn()} />,
    );
    expect(screen.getByText(/bash/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /jump/i }));
    expect(onJump).toHaveBeenCalledWith("t1");
  });

  it("calls onDismiss", () => {
    const onDismiss = vi.fn();
    render(
      <ApprovalBanner items={[req("p1", "t1", "bash")]} onJump={vi.fn()} onDismiss={onDismiss} />,
    );
    fireEvent.click(screen.getByRole("button", { name: /dismiss/i }));
    expect(onDismiss).toHaveBeenCalledTimes(1);
  });
});
```

**Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/ApprovalBanner.test.tsx`
Expected: FAIL（模块不存在）。

**Step 3: 实现**

`ApprovalBanner.tsx`：

```tsx
import type { ApprovalRequest } from "../lib/protocol";

/**
 * 顶部非模态横幅：提示"某个后台 thread 正在等确认"。只提示、不抢焦点；
 * 关闭后由调用方记住，直到**新的**审批到达才再弹。
 */
export function ApprovalBanner({
  items,
  onJump,
  onDismiss,
}: {
  items: ApprovalRequest[];
  onJump: (threadId: string) => void;
  onDismiss: () => void;
}) {
  if (items.length === 0) return null;
  return (
    <div className="flex items-center gap-3 border-b border-amber-900/60 bg-amber-950/60 px-3 py-2 text-xs text-amber-100">
      <span>
        {items.length === 1
          ? `Thread needs approval: ${items[0].params.tool_name}`
          : `${items.length} threads need approval`}
      </span>
      {items.map((r) => (
        <button
          key={r.id}
          type="button"
          onClick={() => onJump(r.params.thread_id)}
          className="rounded bg-amber-800/70 px-2 py-0.5 hover:bg-amber-700/70"
        >
          Jump ({r.params.tool_name})
        </button>
      ))}
      <button
        type="button"
        onClick={onDismiss}
        aria-label="Dismiss"
        className="ml-auto rounded px-1 text-amber-300 hover:text-amber-100"
      >
        ✕
      </button>
    </div>
  );
}
```

**Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/ApprovalBanner.test.tsx`
Expected: PASS（2 passed）。

**Step 5: 提交**

```bash
git add desktop/src/components/ApprovalBanner.tsx desktop/src/components/ApprovalBanner.test.tsx
git commit -m "feat(desktop): add background approval banner"
```

---

## Task 9: `App.tsx` 接线 —— per-thread 会话、切换与 resume 解耦、审批表、去 inert

**Files:**
- Modify: `desktop/src/App.tsx`
- Test: `desktop/src/App.test.tsx`

**Step 1: 更新测试 mock 与新增测试**

`App.test.tsx` 的 RpcClient mock 需能回调通知与审批。将 `vi.hoisted` 的 `state` 增加：

```ts
    notifHandlers: [] as Array<(n: unknown) => void>,
    approvalHandlers: [] as Array<(r: unknown) => void>,
```

`onNotification(cb) { state.notifHandlers.push(cb); return () => {}; }`
`onApproval(cb) { state.approvalHandlers.push(cb); return () => {}; }`

`beforeEach` 里清空：`state.notifHandlers.length = 0; state.approvalHandlers.length = 0;`。

mock 的 `thread/start` 需返回递增的 thread_id（新增测试要用）：

```ts
      if (method === "thread/start") {
        const id = `new-${this.requests.length}`;
        return { thread_id: id, cwd: "/w", model: "m" };
      }
```

`thread/listAll` 返回的 thread 增补 `status: "idle"` 字段。

新增测试：

```tsx
describe("App parallel threads", () => {
  it("does not re-resume a warm thread when switching back to it", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    render(<App />);
    await waitFor(() => expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true));

    fireEvent.click(screen.getByText("two")); // 冷 thread → resume 一次
    await waitFor(() =>
      expect(clients[0].requests.filter((r) => r.method === "thread/resume")).toHaveLength(2),
    );
    fireEvent.click(screen.getByText("one")); // warm → 不再 resume
    await new Promise((r) => setTimeout(r, 0));
    expect(clients[0].requests.filter((r) => r.method === "thread/resume")).toHaveLength(2);
  });

  it("does not lose a background thread's timeline when switching", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    render(<App />);
    await waitFor(() => expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true));
    fireEvent.click(screen.getByText("two"));
    await waitFor(() =>
      expect(clients[0].requests.filter((r) => r.method === "thread/resume")).toHaveLength(2),
    );

    // 后台 t1 流式输出（当前看的是 t2）。
    const notify = state.notifHandlers[0];
    notify({ method: "item/delta", params: { thread_id: "t1", item_id: "a1", delta: "bg" } });

    fireEvent.click(screen.getByText("one")); // 切回 t1（warm，不 resume）
    await waitFor(() => expect(screen.getByText("bg")).toBeTruthy());
  });

  it("shows a banner for a background approval and jumps on click", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    render(<App />);
    await waitFor(() => expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true));
    fireEvent.click(screen.getByText("two")); // 当前 = t2
    await waitFor(() =>
      expect(clients[0].requests.filter((r) => r.method === "thread/resume")).toHaveLength(2),
    );

    state.approvalHandlers[0]({
      id: "perm-1",
      params: {
        thread_id: "t1",
        turn_id: "u1",
        request_id: 1,
        tool_name: "bash",
        tool_input: {},
        prefix_suggestion: null,
        kind: "Normal",
      },
    });

    await waitFor(() => expect(screen.getByRole("button", { name: /jump/i })).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: /jump/i }));
    // 跳到 t1 后显示其审批模态。
    await waitFor(() => expect(screen.getByText(/bash/)).toBeTruthy());
  });
});
```

> 现有 7 个 App 测试把"单 session / 自动 resume 首个 thread / busy 禁用"当前提；重写 `App.tsx` 后按新行为逐个校正，**不要**为了让旧断言通过而保留旧闸门。

**Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/App.test.tsx`
Expected: FAIL（新行为未实现）。

**Step 3: 实现 `App.tsx`**

要点（按此重写，保留现有 RPC 方法签名与 `refreshThreads`/`refreshWorkspaces`/`addWorkspace` 等辅助）：

1. 用 `const storeRef = useRef(new ThreadStore()); const store = storeRef.current;` 取代 `const [session] = useState(() => new Session());`。
2. 新增 `warm = useRef(new Set<string>())`、`inFlightResume = useRef(new Set<string>())`、`dismissedApprovals = useRef(new Set<string>())`。
3. `currentId` 保留为 state；`current = currentId ? store.view(currentId) : null` 每次渲染计算。
4. `refreshThreads()`：`setGroups(r.groups)` 后调用 `store.seed(r.groups.flatMap((g) => g.threads))`。返回 `r.groups` 不变。
5. **`selectThread(id)`**：
   ```tsx
   const selectThread = async (id: string) => {
     store.select(id);
     setCurrentId(id);
     force((v) => v + 1);
     if (warm.current.has(id) || inFlightResume.current.has(id)) return; // warm → 只切视图
     const c = clientRef.current;
     if (!c) return;
     inFlightResume.current.add(id);
     try {
       await c.request("thread/resume", { threadId: id });
       warm.current.add(id);
       const gs = await refreshThreads();
       if (gs !== null && id === store.currentId) setMode(modeForThread(gs, id));
     } catch (e) {
       store.view(id).session.lastError = formatError(e);
       force((v) => v + 1);
     } finally {
       inFlightResume.current.delete(id);
     }
   };
   ```
6. **`newThread(cwd?)`**：`thread/start` → `warm.current.add(id)` → `store.view(id).info = {cwd, model}` → `store.select(id)` → `setCurrentId(id)` → `refreshThreads()` → `setMode(modeForThread(...))`。无 busy 守卫。
7. `send` / `interrupt` 改为作用于 `currentId`（`store.view(currentId)`），乐观用户消息与回滚沿用现有逻辑但落到该 view 的 session。
8. `deleteThread(id)`：请求成功后 `store.drop(id)`；若 `id === currentId` 则 `setCurrentId(null)`；删除 `warm`/`dismissedApprovals` 中该 id；`refreshThreads()`。无 busy 守卫。
9. `renameThread` / `onBrowse` / `onNew` / `addWorkspace` / `removeWorkspace` / `setThreadMode`：去掉 `session.turnActive || resuming.current` 守卫（`onBrowse` 除外可保留无守卫）。`setThreadMode` 仍只作用于当前 thread。
10. 通知接线：
    ```tsx
    client.onNotification((n) => {
      store.applyNotification(n);
      force((v) => v + 1);
      if (n.method === "turn/completed") void refreshThreads();
    });
    client.onApproval((r) => {
      store.setApproval(r);
      force((v) => v + 1);
    });
    ```
11. 首屏：`initialize` → `refreshWorkspaces` → `thread/listAll`（`setGroups` + `store.seed`）→ 若有首个 thread 则 `selectThread(first)` → `setStatus("connected")`。
12. 渲染派生量：
    ```tsx
    const statuses = new Map<string, ThreadStatus>();
    const unread = new Map<string, TurnStatus>();
    for (const g of groups)
      for (const t of g.threads) {
        const v = store.peek(t.thread_id);
        if (!v) continue;
        statuses.set(t.thread_id, v.status);
        if (v.unread && t.thread_id !== currentId)
          unread.set(t.thread_id, v.session.lastStatus ?? "completed");
      }
    const approval = current?.approval ?? null;
    const bannerItems = store
      .pendingApprovalsElsewhere()
      .filter((r) => !dismissedApprovals.current.has(r.id));
    ```
13. JSX：
    - 外层 `<div>` 的 `inert={approval !== null}` **删除**（改为只把模态罩在对话区；侧栏保持可点）。
    - `<ThreadSidebar statuses={statuses} unread={unread} currentId={currentId} onSelect={selectThread} ... />`（不再传 `busy`）。
    - `<StatusBar cwd={current?.info?.cwd ?? null} model={current?.info?.model ?? null} status={status} usage={current?.session.usage ?? null} />`
    - `<ChatView items={current?.session.items ?? []} error={current?.session.lastError ?? null} retrying={current?.session.retrying ?? null} />`
    - `<MessageInput turnActive={current?.session.turnActive ?? false} onSend={send} onInterrupt={interrupt} mode={mode} onModeChange={setThreadMode} />`
    - 横幅渲染在对话列顶部（`StatusBar` 之上）：
      ```tsx
      <ApprovalBanner
        items={bannerItems}
        onJump={(id) => void selectThread(id)}
        onDismiss={() => {
          for (const r of bannerItems) dismissedApprovals.current.add(r.id);
          force((v) => v + 1);
        }}
      />
      ```
    - 模态：`{approval && <ApprovalDialog key={approval.id} request={approval} onDecide={...} />}`，`onDecide` 内 `store.clearApproval(approval.params.thread_id)` + `force`。
14. 导入补：`ThreadStore`、`ApprovalBanner`、`ThreadStatus`、`TurnStatus`；删除不再使用的 `Session` 导入。

**Step 4: 跑测试 + 类型检查**

Run: `cd desktop && npx vitest run src/App.test.tsx && npx tsc --noEmit`
Expected: PASS。

**Step 5: 提交**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): parallel per-thread sessions and approvals"
```

---

## Task 10: 全量验证

**Step 1: 前端**

Run: `cd desktop && npx vitest run && npx tsc --noEmit && npm run build`
Expected: 全绿。

**Step 2: 后端**

Run: `ps aux | grep cargo`（确认无残留）→ `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: 全绿。

**Step 3: 格式**

Run: `cd yi-agent-rs && cargo fmt --all --check`
Expected: 无差异。

---

## Task 11: 文档同步（CLAUDE.md 要求）

**Files:**
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/yi-agent-app-server.md`
- Modify: `docs/superpowers/specs/2026-09-26-desktop-gui-p1-persistence-design.md`
- Modify: `README.md`（若模块索引计数变化）

**Step 1: `desktop.md`**

- 把 `:71` 的 `- [ ] 多 thread 标签页 — 判据：...` 改为 `- [x]`，判据换成可验证形式，例如：
  `可同时运行多个 thread 并在 Sidebar 间自由切换（warm thread 不 re-resume，`desktop/src/App.tsx` `selectThread`）；判据：起两个 turn 后两 thread 状态徽标同时为 running。`
- 在 P1 段落补一条 `[x]`：每 thread 状态徽标 + 未读注意力点（判据：`desktop/src/components/ThreadSidebar.tsx` 渲染 `aria-label="Running"` / `"Awaiting approval"` / `"Unread"`）。
- 更新"验证命令"段的前端单测计数（现 103 → 新数）。

**Step 2: `yi-agent-app-server.md`**

登记 `ThreadStatus`（`protocol.rs`）、`thread/status/updated` 通知、`thread/list(:All)` 的 `status` 字段。

**Step 3: 修订旧约束**

`docs/superpowers/specs/2026-09-26-desktop-gui-p1-persistence-design.md` 约 `:209` 的
"**`turnActive` 期间禁用侧栏切换/删除**…" 与 "切换 thread 不并发（一次一个）。" 两行，
改为指向本设计（说明约束已由并行多 thread 特性取代）。

**Step 4: `README.md`**

若模块索引表的"完成 / 总计"计数因新增 `[x]` 条目而变化，同步更新。

**Step 5: 提交**

```bash
git add docs/project-management/desktop.md docs/project-management/yi-agent-app-server.md docs/superpowers/specs/2026-09-26-desktop-gui-p1-persistence-design.md README.md
git commit -m "docs: record parallel threads + per-thread status"
```

---

## 收尾

全部通过后，按 `superpowers:finishing-a-development-branch` 用 `git merge --no-ff` 合回 `main`，删除分支与 worktree。
