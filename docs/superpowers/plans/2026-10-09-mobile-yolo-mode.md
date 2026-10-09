# 手机端 YOLO 授权 + 模式跨客户端同步 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让手机（Control scope）能切换会话权限模式（YOLO），并让模式变更在所有客户端间实时同步。

**Architecture:** （1）把 `thread/setPermissionMode` 从 admin 门禁移出、按 `Control` 放行（与既有 `thread/setModel` 同档）；（2）新增列表层通知 `thread/permissionModeChanged` 在模式变更时恒推给所有客户端；（3）`thread/resume` / `thread/start` 响应直接携带权威 `permission_mode`，消除对 `thread/listAll` 快照的竞态依赖。

**Tech Stack:** Rust（yi-agent-app-server，tokio / serde_json / tokio-tungstenite 测试）、TypeScript + React + vitest（desktop）。

设计规格：`docs/superpowers/specs/2026-10-09-mobile-yolo-mode-design.md`

## Global Constraints

- 工作目录：worktree `.worktrees/fix-mobile-yolo-mode`（分支 `fix/mobile-yolo-mode`）。**严禁在 `main` 上改代码**。
- **不改** `Scope` 枚举、不引入设备能力位、不把手机设为 Admin。
- **不改** `ADMIN_METHODS` 其余四项：`thread/delete`、`process/kill`、`pair/create`、`device/revoke`。
- 新协议字段/通知必须**向后兼容**：旧客户端忽略未知字段与未知通知。
- Rust 代码在 `yi-agent-rs/` 下改，提交前必须 `cargo fmt --all`。
- Commit message 用 conventional commits，首行 ≤72 字符，**不写** `Co-Authored-By`。
- 测试命令按 crate 跑（不要 `--workspace`）：
  - `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
  - `cd desktop && npx tsc --noEmit && npx vitest run`

---

### Task 1: app-server — `thread/setPermissionMode` 重定档到 `Control`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:2278-2285`（`ADMIN_METHODS`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:3379-3385`（分支开头加门禁）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`tests` 模块内，紧邻 `a_control_client_cannot_delete_a_thread` 附近新增）

**Interfaces:**
- Consumes: 既有 `Scope`（`Observe < Control < Admin`）、`RpcError::insufficient_scope(Scope)`、`Harness::with_scope(Scope)`、测试辅助 `initialize` / `start_thread` / `read_response`。
- Produces: 行为变更 —— Control 客户端调用 `thread/setPermissionMode` 成功；其余四项仍 admin-only。

- [ ] **Step 1: 写失败测试（Control 可切、Observe 被拒）**

在 `server.rs` 的 `tests` 模块里，紧接现有 `a_control_client_cannot_delete_a_thread` 之后插入：

```rust
    /// 手机（Control）必须能切换会话权限模式：设计 §2 的「引导去桌面端授权」
    /// 入口在代码里并不存在，admin-only 会让手机端的 YOLO 切换永远是死路。
    /// 与 `thread/setModel` 同档（Control）。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_control_client_may_switch_permission_mode() {
        let mut h = Harness::with_scope(Scope::Control).await;
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":9,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":"yolo"}}}}"#
        ))
        .await;
        let v = read_response(&mut h, 9).await;
        assert!(
            v["result"].is_object(),
            "Control 客户端必须能切换到 yolo: {v}"
        );
        h.shutdown().await;
    }

    /// 最低档 `Observe` 仍必须被拒：读-only 客户端不得放开沙箱。
    #[tokio::test(flavor = "multi_thread")]
    async fn an_observe_client_cannot_switch_permission_mode() {
        let mut h = Harness::with_scope(Scope::Observe).await;
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":9,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":"yolo"}}}}"#
        ))
        .await;
        let v = read_response(&mut h, 9).await;
        assert_eq!(
            v["error"]["code"], -32014,
            "Observe 客户端必须被拒: {v}"
        );
        h.shutdown().await;
    }
```

- [ ] **Step 2: 运行，确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib a_control_client_may_switch_permission_mode an_observe_client_cannot_switch_permission_mode`
Expected: `a_control_client_may_switch_permission_mode` FAIL（`assert!(v["result"].is_object())` 失败，实得 `-32014` 错误对象）；`an_observe_client_cannot_switch_permission_mode` PASS（Observe 本就被拒）。

- [ ] **Step 3: 从 `ADMIN_METHODS` 移出**

改 `server.rs:2278` 起：

```rust
                const ADMIN_METHODS: [&str; 4] = [
                    "thread/delete",
                    "process/kill",
                    "pair/create",
                    "device/revoke",
                ];
```

并在该常量上方注释里补一句（现有注释已解释「为何入闸」，加一段说明 setPermissionMode 已按 Control 放行）：

```rust
                // 注:`thread/setPermissionMode` 原在此列,现按 `Control` 放行——设计
                // （2026-10-02-mobile-remote-access-design.md §5.4）承诺的「引导去桌面端
                // 授权」入口在代码中并不存在,admin-only 使手机端的 YOLO 切换结构性
                // 不可用。它与 `thread/setModel` 同档:Control 客户端本就能 `turn/start`
                // 并在 YOLO 会话上批准任意工具,单开此门不扩大既有能力面。门禁在下方
                // 方法分支内按 `Control` 施加。
```

- [ ] **Step 4: 在方法分支内按 Control 施加门禁**

在 `server.rs:3379`（`"thread/setPermissionMode" => {`）之后、`let Some(thread_id) = ...` 之前插入：

```rust
                        // 与 `thread/setModel` 同档:Control 起。Observe 读到的是掩码级
                        // 视图,不得放开 OS 沙箱。
                        if client_scope < Scope::Control {
                            write_response(
                                &hub,
                                &client,
                                err_response(id, RpcError::insufficient_scope(Scope::Control)),
                            )
                            .await?;
                            continue;
                        }
```

- [ ] **Step 5: 运行，确认两个用例都通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib a_control_client_may_switch_permission_mode an_observe_client_cannot_switch_permission_mode`
Expected: 两条 PASS。

- [ ] **Step 6: 跑门禁回归，确认其余四项未松动**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib a_control_client_cannot_delete_a_thread a_control_client_cannot_pair_or_revoke an_admin_client_may_delete_a_thread`
Expected: 全 PASS。

- [ ] **Step 7: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-mobile-yolo-mode/yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "fix(app-server): let a control client switch permission mode (yolo)"
```

---

### Task 2: app-server — 新增 `thread/permissionModeChanged` 通知（协议层）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`（`Notification` 枚举、`thread_key`、`delivery`、`use` 与测试模块）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`（`mod tests`）

**Interfaces:**
- Consumes: `crate::thread_store::ThreadMode`（serde lowercase：`normal` / `yolo`）；`NotificationEnvelope::new`；`Delivery`。
- Produces: `Notification::PermissionModeChanged { thread_id: String, mode: crate::thread_store::ThreadMode }`，wire method `thread/permissionModeChanged`，`delivery() == Delivery::List`，`thread_key() == Some(thread_id)`。Task 3 用它在服务端广播。

- [ ] **Step 1: 写失败测试（wire 形状 + 投递层级）**

在 `protocol.rs` 的 `mod tests` 里，紧接 `model_changed_notification_carries_the_method_and_model` 之后插入：

```rust
    /// mode 变更通知的 wire 形状:`thread/permissionModeChanged`,参数 snake_case。
    #[test]
    fn permission_mode_changed_notification_carries_method_and_mode() {
        let n = Notification::PermissionModeChanged {
            thread_id: "t1".into(),
            mode: crate::thread_store::ThreadMode::Yolo,
        };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "thread/permissionModeChanged");
        assert_eq!(v["params"]["thread_id"], "t1");
        assert_eq!(v["params"]["mode"], "yolo");
        // 投递层级:与 `modelChanged` 同为列表层。远程客户端只订阅少量暖会话,
        // 若按内容层过滤,窗口外会话的模式变更永远收不到——「概率性不同步」的成因。
        assert_eq!(n.delivery(), crate::protocol::Delivery::List);
        assert_eq!(n.thread_key(), Some("t1"));
    }
```

- [ ] **Step 2: 运行，确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib permission_mode_changed_notification_carries_method_and_mode`
Expected: 编译 FAIL（`Notification::PermissionModeChanged` 不存在）。

- [ ] **Step 3: 加变体、`thread_key`、`delivery`**

在 `protocol.rs` 的 `ModelChanged` 变体之后插入：

```rust
    /// 该会话的权限模式（自主权）已切换。
    ///
    /// 与 `thread/modelChanged` 并列：模式与模型都是会话元数据，切换后必须让
    /// **所有**客户端（桌面 + 手机）立即刷新各自的 mode chip，否则一端改了
    /// YOLO、另一端停在旧值。它只带模式、不带正文。
    #[serde(rename = "thread/permissionModeChanged")]
    PermissionModeChanged {
        thread_id: String,
        mode: crate::thread_store::ThreadMode,
    },
```

在 `thread_key()` 的 match 里，把 `Notification::ModelChanged { thread_id, .. }` 那一行改为并列两行（或在其后新增一行）：

```rust
            | Notification::ModelChanged { thread_id, .. }
            | Notification::PermissionModeChanged { thread_id, .. }
```

在 `delivery()` 的 match 的 `ModelChanged { .. }` 后并列加：

```rust
            | Notification::PermissionModeChanged { .. }
```

（即 `ThreadStarted | ThreadStatusUpdated | ModelChanged | PermissionModeChanged | TurnCompleted => Delivery::List`。）

- [ ] **Step 4: 运行，确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib permission_mode_changed_notification_carries_method_and_mode`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-mobile-yolo-mode/yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs
git commit -m "feat(app-server): add thread/permissionModeChanged list-layer notification"
```

---

### Task 3: app-server — 变更时广播该通知

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:3379-3432`（`thread/setPermissionMode` 分支）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`tests` 模块）

**Interfaces:**
- Consumes: 来自 Task 2 的 `Notification::PermissionModeChanged`；既有 `write_notification`。
- Produces: 任一客户端切换某会话模式后，所有客户端（含未订阅该 thread 的）收到 `thread/permissionModeChanged`。

- [ ] **Step 1: 写失败测试（Change 后本连接收到通知）**

新增到 `server.rs` 的 `tests` 模块（紧接 Task 1 的两个用例）：

```rust
    /// 切换模式必须**广播** `thread/permissionModeChanged`:这是跨客户端同步
    /// （桌面改 → 手机同步、手机改 → 桌面 chip 同步）的唯一通道。
    #[tokio::test(flavor = "multi_thread")]
    async fn switching_permission_mode_broadcasts_the_change() {
        let mut h = Harness::new(); // stdio = Admin,但广播与 scope 无关
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":9,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":"yolo"}}}}"#
        ))
        .await;

        // 响应与通知都在流上,顺序不保证;分别收敛。
        let mut response: Option<serde_json::Value> = None;
        let mut changed: Option<serde_json::Value> = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(9)) {
                response = Some(v);
            } else if v.get("method").and_then(|m| m.as_str())
                == Some("thread/permissionModeChanged")
            {
                changed = Some(v);
            }
            if response.is_some() && changed.is_some() {
                break;
            }
        }
        assert!(response.is_some(), "setPermissionMode 必须有响应");
        let changed = changed.expect("must broadcast thread/permissionModeChanged");
        assert_eq!(changed["params"]["thread_id"].as_str(), Some(tid.as_str()));
        assert_eq!(changed["params"]["mode"].as_str(), Some("yolo"));
        h.shutdown().await;
    }
```

- [ ] **Step 2: 运行，确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib switching_permission_mode_broadcasts_the_change`
Expected: FAIL（`must broadcast thread/permissionModeChanged`）。

- [ ] **Step 3: 在落盘之后广播**

在 `server.rs` 的 `thread/setPermissionMode` 分支里，`match session.store.set_permission_mode(...)` 之后、`write_response(..., ok_response(id, json!({})))` 之前插入：

```rust
                        // 广播给所有客户端(列表层恒推):桌面改 → 手机 chip 同步,
                        // 手机改 → 桌面 chip 同步。与 `setModel`(server.rs 的
                        // ModelChanged 广播)同一手法。
                        let _ = write_notification(
                            &hub,
                            &Notification::PermissionModeChanged {
                                thread_id: thread_id.clone(),
                                mode,
                            },
                        )
                        .await;
```

- [ ] **Step 4: 运行，确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib switching_permission_mode_broadcasts_the_change`
Expected: PASS。

- [ ] **Step 5: 跨客户端扇出集成测试（stdio=Admin + ws=Control）**

新增到 `server.rs` 的 `tests_merged`（或含 `MergedHarness` 的模块，紧接 `merged_loop_fans_out_stdio_notifications_to_ws_client` 之后）：

```rust
    /// 跨客户端同步的端到端证明:桌面(stdio)改 mode,手机(ws,Control)必须收到
    /// `thread/permissionModeChanged`——修复前没有任何 mode 通知,手机停在旧值。
    #[tokio::test(flavor = "multi_thread")]
    async fn merged_loop_fans_out_permission_mode_change_to_ws_client() {
        let mut h = MergedHarness::new().await;
        let mut ws = h.connect_ws().await;
        crate::server::tests_support::initialize(&mut ws).await;

        // stdio 起一个 thread。
        h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .await;
        let _ = h.read_value().await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;
        let mut thread_id = None;
        for _ in 0..4 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(2)) {
                thread_id = Some(v["result"]["thread_id"].as_str().unwrap().to_string());
                break;
            }
        }
        let thread_id = thread_id.expect("thread/start response");

        // ws 客户端先收到 thread/started(建立它的视图),再等 mode 通知。
        let _ = ws_await_notification(&mut ws, "thread/started").await;

        // stdio 切 yolo。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"thread/setPermissionMode","params":{{"threadId":"{thread_id}","mode":"yolo"}}}}"#
        ))
        .await;

        let notif = ws_await_notification(&mut ws, "thread/permissionModeChanged").await;
        assert_eq!(notif["params"]["thread_id"].as_str(), Some(thread_id.as_str()));
        assert_eq!(notif["params"]["mode"].as_str(), Some("yolo"));
        h.shutdown().await;
    }
```

- [ ] **Step 6: 运行，确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib merged_loop_fans_out_permission_mode_change_to_ws_client`
Expected: PASS。

- [ ] **Step 7: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-mobile-yolo-mode/yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): broadcast permissionModeChanged on mode switch"
```

---

### Task 4: app-server — `thread/resume` / `thread/start` 响应带 `permission_mode`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:2928-2938`（thread/start 响应）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:3194-3205`（thread/resume 响应）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`tests` 模块）

**Interfaces:**
- Consumes: 局部 `mode`（thread/start，恒 `Normal`）、`loaded.meta.permission_mode`（thread/resume，`ThreadMode` 为 `Copy`）。
- Produces: 两个响应新增 `"permission_mode": "normal" | "yolo"`（snake_case）。Task 5/6 的客户端据此直接读权威值，不再依赖 listAll 回读。

- [ ] **Step 1: 写失败测试**

新增到 `server.rs` 的 `tests` 模块：

```rust
    /// `thread/start` 响应必须直接带权威 `permission_mode`（新线程恒 normal）。
    /// 客户端靠它建起 mode chip,不必再回读 listAll(那条路径异步、可失败、可竞态)。
    #[tokio::test(flavor = "multi_thread")]
    async fn thread_start_response_carries_permission_mode() {
        let mut h = Harness::new();
        let tid = start_thread(&mut h).await;
        // `start_thread` 只回 thread_id;这里补一次显式断言:重开一条并读全响应。
        h.send(r#"{"jsonrpc":"2.0","id":20,"method":"thread/start","params":{}}"#)
            .await;
        let mut resp = None;
        for _ in 0..4 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(20)) {
                resp = Some(v);
                break;
            }
        }
        let resp = resp.expect("thread/start response");
        assert_eq!(
            resp["result"]["permission_mode"].as_str(),
            Some("normal"),
            "thread/start must carry permission_mode: {resp}"
        );
        let _ = tid;
        h.shutdown().await;
    }

    /// `thread/resume` 响应必须带该会话**持久化**的模式:手机打开一个电脑上
    /// 已置 yolo 的会话,必须一次请求就看到 yolo,而不是「未知 / 等回读」。
    #[tokio::test(flavor = "multi_thread")]
    async fn thread_resume_response_carries_persisted_permission_mode() {
        let mut h = Harness::new();
        let tid = start_thread(&mut h).await;
        // 切 yolo（走真实 RPC，确保落盘）。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":9,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":"yolo"}}}}"#
        ))
        .await;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(9)) {
                break;
            }
        }
        // 再 resume 同一 thread。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":30,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let mut resp = None;
        for _ in 0..20 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(30)) {
                resp = Some(v);
                break;
            }
        }
        let resp = resp.expect("thread/resume response");
        assert_eq!(
            resp["result"]["permission_mode"].as_str(),
            Some("yolo"),
            "thread/resume must carry the persisted mode: {resp}"
        );
        h.shutdown().await;
    }
```

- [ ] **Step 2: 运行，确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_start_response_carries_permission_mode thread_resume_response_carries_persisted_permission_mode`
Expected: 两条都 FAIL（`permission_mode` 为 `Null`）。

- [ ] **Step 3: 在两处响应里加字段**

`thread/start`（`server.rs:2933` 附近的 `ok_response`）：

```rust
                                    "thread_id": thread_id,
                                    "cwd": cwd,
                                    "model": model,
                                    "model_ref": model_ref,
                                    "permission_mode": mode,
```

`thread/resume`（`server.rs:3198` 附近的 `ok_response`）：

```rust
                                    "thread_id": thread_id,
                                    "cwd": cwd,
                                    "model": model,
                                    "model_ref": loaded.meta.model_ref,
                                    "permission_mode": loaded.meta.permission_mode,
```

- [ ] **Step 4: 运行，确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_start_response_carries_permission_mode thread_resume_response_carries_persisted_permission_mode`
Expected: 两条 PASS。

- [ ] **Step 5: 全 crate 回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: 全绿（注意既有 `list_exposes_permission_mode`、`set_permission_mode_toggles_and_persists`、`notification_delivery_classifies_list_and_content`、`thread_status_notification_*` 等仍 PASS）。

- [ ] **Step 6: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-mobile-yolo-mode/yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): carry permission_mode in thread start/resume responses"
```

---

### Task 5: desktop — 消费 `thread/permissionModeChanged`

**Files:**
- Modify: `desktop/src/lib/protocol.ts:60-`（`Notification` union）
- Modify: `desktop/src/lib/threadStore.ts`（`applyNotification`）
- Test: `desktop/src/lib/threadStore.test.ts`

**Interfaces:**
- Consumes: 服务端 `thread/permissionModeChanged { thread_id, mode }`；`ThreadMode = "normal" | "yolo"`。
- Produces: `ThreadStore.applyNotification` 对该方法写 `view(id).mode = n.params.mode`。

- [ ] **Step 1: 写失败测试**

在 `desktop/src/lib/threadStore.test.ts` 的 `describe` 内、`thread/modelChanged` 相关用例之后插入（该文件用 `summary(id)` 造 `ThreadSummary`）：

```ts
  it("writes the target thread's mode on thread/permissionModeChanged", () => {
    const s = new ThreadStore();
    s.seed([summary("a"), summary("b")]);
    s.applyNotification({
      method: "thread/permissionModeChanged",
      params: { thread_id: "a", mode: "yolo" },
    });
    expect(s.view("a").mode).toBe("yolo");
    // 只动被点名的那个。
    expect(s.view("b").mode).toBeNull();
  });

  it("does not crash on thread/permissionModeChanged for an unknown thread", () => {
    const s = new ThreadStore();
    s.applyNotification({
      method: "thread/permissionModeChanged",
      params: { thread_id: "ghost", mode: "normal" },
    });
    expect(s.view("ghost").mode).toBe("normal");
  });
```

- [ ] **Step 2: 运行，确认失败**

Run: `cd desktop && npx vitest run src/lib/threadStore.test.ts`
Expected: FAIL —— TypeScript/运行时不认 `method: "thread/permissionModeChanged"`（`Notification` union 未含它；`applyNotification` 会把它当内容帧塞进 `session.apply`，`view("a").mode` 仍为 `null`）。

- [ ] **Step 3: 扩展 union 与 `applyNotification`**

`desktop/src/lib/protocol.ts` 在 `| { method: "thread/modelChanged"; params: { thread_id: string; model: string } }` 之后加：

```ts
  | {
      method: "thread/permissionModeChanged";
      params: { thread_id: string; mode: ThreadMode };
    }
```

`desktop/src/lib/threadStore.ts` 的 `applyNotification` 里，在 `thread/modelChanged` 分支之后加：

```ts
    if (n.method === "thread/permissionModeChanged") {
      // 跨客户端同步通道：另一个客户端（如桌面）切换了该会话的权限模式，
      // 服务端广播到这里。mode 是服务端权威值，直接采纳。
      this.view(n.params.thread_id).mode = n.params.mode;
      return;
    }
```

（放在 `if (n.method === "error")` 之前即可。）

- [ ] **Step 4: 运行，确认通过**

Run: `cd desktop && npx vitest run src/lib/threadStore.test.ts`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-mobile-yolo-mode
git add desktop/src/lib/protocol.ts desktop/src/lib/threadStore.ts desktop/src/lib/threadStore.test.ts
git commit -m "feat(desktop): apply thread/permissionModeChanged to the thread store"
```

---

### Task 6: desktop — resume/start 响应直接驱动 mode（消除快照竞态）

**Files:**
- Modify: `desktop/src/lib/threadPermissionMode.ts`（新增解析辅助）
- Modify: `desktop/src/App.tsx`（`selectThread` 的 resume 路径、`newThread` 的 start 路径）
- Test: `desktop/src/lib/threadPermissionMode.test.ts`、`desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: resume/start 响应中的 `permission_mode?: ThreadMode`（Task 4 产物）。
- Produces: `permissionModeFromResponse(r: unknown): ThreadMode | null` —— 响应含合法 mode 返回该值，缺失/非法返回 `null`（未知，勿当 normal）。

- [ ] **Step 1: 写失败测试（纯函数）**

在 `desktop/src/lib/threadPermissionMode.test.ts` 末尾追加：

```ts
import { permissionModeFromResponse } from "./threadPermissionMode";

describe("permissionModeFromResponse", () => {
  it("reads a valid mode off a thread response", () => {
    expect(permissionModeFromResponse({ thread_id: "t", permission_mode: "yolo" })).toBe("yolo");
    expect(permissionModeFromResponse({ thread_id: "t", permission_mode: "normal" })).toBe("normal");
  });
  it("returns null for a missing or unknown mode (never guesses normal)", () => {
    expect(permissionModeFromResponse({ thread_id: "t" })).toBeNull();
    expect(permissionModeFromResponse({ thread_id: "t", permission_mode: "wat" })).toBeNull();
    expect(permissionModeFromResponse(null)).toBeNull();
  });
});
```

- [ ] **Step 2: 运行，确认失败**

Run: `cd desktop && npx vitest run src/lib/threadPermissionMode.test.ts`
Expected: FAIL（`permissionModeFromResponse` 未导出/不存在）。

- [ ] **Step 3: 实现纯函数**

`desktop/src/lib/threadPermissionMode.ts` 末尾追加：

```ts
/**
 * 从 `thread/resume` / `thread/start` 的响应里读权威权限模式。
 *
 * 响应现在直接带 `permission_mode`（此前只能从 `thread/listAll` 快照回读，
 * 那条路径异步、可失败、可与另一端写入竞态——「手机上打开会话时 YOLO 概率性
 * 没同步过来」的成因）。缺失或非法一律返回 `null`：未知，**不得**回退成 normal。
 */
export function permissionModeFromResponse(r: unknown): ThreadMode | null {
  const mode = (r as { permission_mode?: unknown } | null | undefined)?.permission_mode;
  return mode === "normal" || mode === "yolo" ? mode : null;
}
```

- [ ] **Step 4: 运行，确认纯函数通过**

Run: `cd desktop && npx vitest run src/lib/threadPermissionMode.test.ts`
Expected: PASS。

- [ ] **Step 5: 写失败测试（App：resume 响应即决定 chip）**

在 `desktop/src/App.test.tsx` 里，先让 mock 的 `thread/resume` 回带 `permission_mode`（修改既有 mock 分支）：

```ts
      if (method === "thread/resume") {
        const { threadId } = params as { threadId: string };
        return {
          thread_id: threadId,
          cwd: "/w",
          model: "m",
          permission_mode: seeds.find((s) => s.thread_id === threadId)?.permission_mode,
        };
      }
```

再新增用例（沿用文件已有的 `modeTrigger()`、`seeds` 与渲染 helper；若该文件用 `state.seeds` 摆数据则照其既有风格）：

```ts
  it("shows YOLO on resume from the response field, without waiting for a listAll re-read", async () => {
    // 该会话在「电脑上」已是 yolo；手机打开它。
    state.seeds = [{ thread_id: "t1", cwd: "/w", model: "m", permission_mode: "yolo" }];
    // 让 listAll 回读**失败**，证明 chip 不再依赖它（修复前这条会停在未知/Normal）。
    state.listFails = true;
    render(<App />);
    await waitFor(() => expect(modeTrigger().textContent).toContain("YOLO"));
  });
```

若该测试文件的 mock 尚无 `state.listFails`，则在它的 mock 状态对象里加 `listFails: false`，并在 `thread/listAll` 分支开头加：

```ts
      if (method === "thread/listAll") {
        if (state.listFails) throw { code: -32603, message: "list failed" };
        // ...既有实现...
      }
```

- [ ] **Step 6: 运行，确认失败**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "shows YOLO on resume from the response field"`
Expected: FAIL（chip 停在 Mode/Normal：listAll 抛错 → `refreshThreads` 返回 `null` → 不回读，而 resume 响应未被使用）。

- [ ] **Step 7: 改 `selectThread` 的 resume 路径**

`desktop/src/App.tsx` 的 `selectThread` 里，把

```ts
      await c.request("thread/resume", { threadId: id });
```

改为读取响应并先落权威值：

```ts
      const resumed = await c.request<{ permission_mode?: ThreadMode }>("thread/resume", {
        threadId: id,
      });
```

并把紧随其后的回写块改为「响应优先、listAll 兜底」：

```ts
      const view = store.peek(id);
      if (!view) return;
      warm.current.add(id);
      // 响应直接带权威 mode：一次请求即定，不受 listAll 快照的竞态/失败影响。
      const fromResponse = permissionModeFromResponse(resumed);
      if (fromResponse !== null) view.mode = fromResponse;
      // listAll 回读仅作兜底（旧服务端不带该字段时），失败即保持已有值。
      const gs = await refreshThreads();
      if (gs !== null) view.mode = modeForThread(gs, pinned, id) ?? view.mode;
      force((v) => v + 1);
```

（注意：`?? view.mode` 保证 listAll 缺该 thread 时不把已知值打回 null。）

- [ ] **Step 8: 改 `newThread` 的 start 路径**

`desktop/src/App.tsx` 的 `newThread` 里：

```ts
      const t = await c.request<{
        thread_id: string;
        cwd: string;
        model: string;
        permission_mode?: ThreadMode;
      }>("thread/start", threadStartParams(cwd));
```

把

```ts
      store.view(t.thread_id).mode = "normal";
```

改为

```ts
      // 新会话恒 normal，但以响应字段为准（服务端是权威），缺失时回退 normal。
      store.view(t.thread_id).mode = permissionModeFromResponse(t) ?? "normal";
```

并把该函数末尾的 listAll 回写块同样加 `.filter(Boolean)` 语义：

```ts
      const gs = await refreshThreads();
      if (gs !== null) {
        const v = store.view(t.thread_id);
        v.mode = modeForThread(gs, pinned, t.thread_id) ?? v.mode;
        force((n) => n + 1);
      }
```

- [ ] **Step 9: 在 App.tsx 导入新辅助**

在 `desktop/src/App.tsx:65` 的导入行改为：

```ts
import {
  permissionModeFromResponse,
  setPermissionModeParams,
  type ThreadMode,
} from "./lib/threadPermissionMode";
```

- [ ] **Step 10: 运行，确认通过 + 类型检查**

Run: `cd desktop && npx tsc --noEmit && npx vitest run src/App.test.tsx src/lib/threadPermissionMode.test.ts`
Expected: 全 PASS（含既有 `requests thread/setPermissionMode with the right params` 等）。

- [ ] **Step 11: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-mobile-yolo-mode
git add desktop/src/App.tsx desktop/src/lib/threadPermissionMode.ts desktop/src/lib/threadPermissionMode.test.ts desktop/src/App.test.tsx
git commit -m "fix(desktop): take permission mode from resume/start responses, not a list snapshot"
```

---

### Task 7: 文档 + bug-list 同步

**Files:**
- Modify: `docs/superpowers/specs/2026-10-02-mobile-remote-access-design.md:310`（危险操作表述）
- Modify: `docs/relay-deploy.md:622-624` 与 `:295` 一带（admin 清单）
- Modify: `docs/bug-list.md`（新增两条 `[x]`）
- Modify: `docs/project-management/yi-agent-app-server.md`（如该文件列了 scope/setPermissionMode 行为）

**Interfaces:** 无（纯文档）。

- [ ] **Step 1: 改 `mobile-remote-access-design.md` §5.4 表格行**

把「危险操作」那行改为（保留 delete/process.kill 为 admin，setPermissionMode 移出）：

```markdown
| 危险操作 | `admin` 类（`thread/delete`、`process/kill`）在 `control` 下返回 `-32014`。`thread/setPermissionMode`（切 YOLO）按 `control` 放行（2026-10-09 修订：原设计承诺的「桌面端授权」入口从未实现，admin-only 使手机端 YOLO 结构性不可用；它与 `thread/setModel` 同档，Control 本就能 `turn/start` 并批准任意工具） |
```

- [ ] **Step 2: 改 `relay-deploy.md`**

把两处把 `thread/setPermissionMode` 列入 admin 的表述（`:295` 的清单、`:622` 的「admin 操作只在桌面可发」）改为不再包含它，并注明 2026-10-09 修订与理由（手机可切 YOLO）。

- [ ] **Step 3: bug-list 追加两条 `[x]`**

在 `docs/bug-list.md` 顶部已修复区追加：

```markdown
- [x] 手机端开启 new thread 后切换 mode 到 YOLO 报「权限不足」（根因：`thread/setPermissionMode` 在 `server.rs` 的 `ADMIN_METHODS` 内按 `Scope::Admin` 门禁，而手机（直连 ws / 经中继）恒为 `Scope::Control`、只有桌面 stdio 是 Admin，故必然 `-32014`；设计承诺的「引导去桌面端授权」入口代码里并不存在。修复：把它移出 `ADMIN_METHODS`，在方法分支内按 `Control` 放行（与 `thread/setModel` 同档），Observe 仍被拒。验证：`cargo test -p yi-agent-app-server --lib a_control_client_may_switch_permission_mode an_observe_client_cannot_switch_permission_mode`（修复前第一条 `-32014`），`cargo test -p yi-agent-app-server --lib a_control_client_cannot_delete_a_thread a_control_client_cannot_pair_or_revoke`（其余四项 admin 门禁不变）。doc 同步：`mobile-remote-access-design.md` §5.4、`relay-deploy.md`）
- [x] 手机上打开会话时电脑端已置的 YOLO 状态概率性不显示（根因：`thread/resume` / `thread/start` 响应不带 `permission_mode`，客户端只能从 `thread/listAll` 快照回读，而该回读异步、可失败（`refreshThreads` 失败返回 `null`）、且 `selectThread` 的 `inFlightResume` 守卫会让并发打开跳过回读，桌面在手机之后改 YOLO 时手机上现存会话永不更新。修复：新增列表层通知 `thread/permissionModeChanged`（`protocol.rs`，`delivery()==List` 恒推——与 `thread/modelChanged` 同理，避免远程订阅窗口过滤）在 `thread/setPermissionMode` 落盘后广播；并让 `thread/resume`/`thread/start` 响应直接携带权威 `permission_mode`，客户端响应优先、listAll 仅兜底。验证：`cargo test -p yi-agent-app-server --lib permission_mode_changed_notification_carries_method_and_mode switching_permission_mode_broadcasts_the_change merged_loop_fans_out_permission_mode_change_to_ws_client thread_start_response_carries_permission_mode thread_resume_response_carries_persisted_permission_mode`、`cd desktop && npx vitest run src/lib/threadStore.test.ts src/App.test.tsx src/lib/threadPermissionMode.test.ts`）
```

- [ ] **Step 4: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-mobile-yolo-mode
git add docs/
git commit -m "docs: record mobile yolo authorization and mode-sync fixes"
```

---

### Task 8: 收尾验证（合并前）

- [ ] **Step 1: 全量 app-server 测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: 全绿。

- [ ] **Step 2: 前端全量 + 类型**

Run: `cd desktop && npx tsc --noEmit && npx vitest run`
Expected: 全绿。

- [ ] **Step 3: 格式化与构建**

Run: `cd yi-agent-rs && cargo fmt --all --check` 与 `cd desktop && npm run build`
Expected: 均通过。

- [ ] **Step 4: 按 finishing-a-development-branch 合并回 main**

在 `main` 上执行 `git merge --no-ff fix/mobile-yolo-mode`，随后删除分支与 worktree。

---

## Self-Review

**Spec coverage：**
- §1.1/§2 方案 A → Task 1（重定档）。
- §3.2.1 新通知 → Task 2；§3.2.2 广播点 → Task 3；§3.2.3 响应带 mode → Task 4。
- §3.3 客户端 → Task 5（通知消费）、Task 6（响应驱动 + 兜底）。
- §4 测试 → 各 Task 内嵌；§5 验证命令 → Task 8。
- §2「同步修正文档」→ Task 7。

**Placeholder scan：** 无 TBD/TODO；每个代码步骤都给了实际代码块。

**Type consistency：** `Notification::PermissionModeChanged { thread_id, mode: ThreadMode }` 在 Task 2 定义、Task 3 使用；前端 `thread/permissionModeChanged { thread_id, mode }` 在 Task 5 定义、Task 6/Task 5 测试使用；`permissionModeFromResponse` 在 Task 6 Step 3 定义、Step 7/8 使用（同文件同签名）。`mode`/`loaded.meta.permission_mode` 均为 `ThreadMode`（`Copy`），可在 `start_thread_core` 之后继续读。
