# 项目组长 Agent P3-B/C 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给组长补两个只读-以外的写工具（`lead_interrupt_thread` 取消、`lead_reject_thread` 打回），走独立反向通道由主循环执行；并修掉组长行在手机上长按会弹出无意义会话菜单的 bug + 加纯 CSS 的 IM 式头像。

**Architecture:** 新增 `lead_control.rs`（纯数据结构 + 通道 + 纯判定函数），组长工具把 `LeadControlCmd` 投进 `LeadControlInbox`（broadcast），主循环 `select!` 消费后动 `threads`——照抄 `lead_inbox` 的先例，守住「`threads` 是主循环独占的 `&mut`」这条硬约束。P3-C 是纯前端改动。

**Tech Stack:** Rust（tokio、async-trait、serde_json）、React + TypeScript（vitest + jsdom）、既有 `yi-agent-app-server` 测试 Harness。

## Global Constraints

- **测试基线**：`cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`（当前 main：**826 passed / 0 failed / 4 ignored**）。新增用例只增不改既有断言。
- **禁止 `--workspace` 全量测试**（会 OOM）；按 crate 跑。
- 跑测试前先 `ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent"` 确认无残留进程。
- **零 core 改动**：本计划只动 `yi-agent-app-server`（后端）与 `desktop/`（前端）。
- **不新增跨 root capability**，不改 daemon 会话隔离模型（上级 spec §10）。
- 提交规范：conventional commits、中文、**不写 `Co-Authored-By`**；提交前 `cd yi-agent-rs && cargo fmt --all`（前端任务跑 `npx prettier --write` 若有配置，否则不动格式）。
- 两个新工具**只对组长注册**（门控用 `crate::lead_memory::is_lead_thread`，唯一权威）。
- 工具元数据：`requires_confirmation: false`（后台无人值守，与 `lead_remember` 同理由）；**不设** `read_only`（它们是写动作）。
- 控制通道 `LeadControlCmd`/`LeadControlInbox` 是**纯进程内结构**，不涉 `serde`、不上 wire。
- 不新增头像图片资源：头像纯 CSS（见 Task 9）。
- 前端测试：`cd desktop && npx vitest run <file>`；`node_modules` 缺失时先 `npm install`。

---

## 文件结构

| 文件 | 职责 | 动作 |
|---|---|---|
| `yi-agent-rs/crates/yi-agent-app-server/src/lead_control.rs` | 控制指令 + 通道 + 目标解析纯函数 | 新建 |
| `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs` | 注册新模块 | 改 1 行 |
| `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` | 通道接线 + 工具注册 + 主循环消费分支 | 改多处 |
| `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs` | `AppState` 透传新通道 | 改 1 处 |
| `yi-agent-rs/crates/yi-agent-app-server/src/ws.rs` | `Attachments` 字面量补字段 | 改 1 处 |
| `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs` | Harness/Attachments 补字段 + 新测试 | 改多处 |
| `desktop/src/lib/sidebarPress.ts` | 组长行不参与长按 | 改 |
| `desktop/src/lib/sidebarPress.test.ts` | 回归测试 | 改 |
| `desktop/src/components/ThreadSidebar.tsx` | 组长行头像 | 改 |
| `desktop/src/components/LeadAvatar.tsx` | 纯 CSS 头像组件 | 新建 |
| `desktop/src/components/ChatView.tsx` | 顶部组长标签 | 改（可选 prop） |

---

### Task 1: `lead_control.rs` —— 指令、通道与目标解析（纯逻辑）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/lead_control.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`（`pub(crate) mod lead_control;`）
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: 无（纯模块）。
- Produces:
  - `pub enum LeadControlKind { Interrupt, Reject }`
  - `pub struct LeadControlCmd { pub project_key: String, pub kind: LeadControlKind, pub thread_id: String, pub reason: String, pub request_id: String }`
  - `#[derive(Clone)] pub struct LeadControlInbox { tx: broadcast::Sender<LeadControlCmd> }`，方法 `new() -> Self`、`subscribe(&self) -> broadcast::Receiver<LeadControlCmd>`、`send(&self, cmd: LeadControlCmd) -> bool`（`true` = 有订阅者）
  - `#[derive(Clone, Default)] pub struct LeadControlReplies { ... }`，方法 `register(&self, request_id: String) -> oneshot::Receiver<Result<String, String>>`、`resolve(&self, request_id: &str, outcome: Result<String, String>) -> bool`
  - `pub enum ControlTarget { Session, NotOpen }`
  - `pub fn classify_target(project_key: &str, cmd_project_key: &str, in_threads: bool) -> ControlTarget`

- [ ] **Step 1: 写失败测试**

在 `lead_control.rs` 末尾：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_from_another_project_is_not_applicable() {
        // project_key 是「本进程正在服务的项目」判定：不同项目一律 NotOpen，
        // 绝不去别的项目目录里找同名 thread。
        assert_eq!(
            classify_target("/a/.yi-agent", "/b/.yi-agent", true),
            ControlTarget::NotOpen
        );
    }

    #[test]
    fn a_matching_project_with_the_session_present_is_a_session() {
        assert_eq!(
            classify_target("/a/.yi-agent", "/a/.yi-agent", true),
            ControlTarget::Session
        );
    }

    #[test]
    fn a_matching_project_without_the_session_is_not_open() {
        assert_eq!(
            classify_target("/a/.yi-agent", "/a/.yi-agent", false),
            ControlTarget::NotOpen
        );
    }

    #[tokio::test]
    async fn a_delivered_command_reaches_the_subscriber() {
        let inbox = LeadControlInbox::new();
        let mut rx = inbox.subscribe();
        let cmd = LeadControlCmd {
            project_key: "/a/.yi-agent".into(),
            kind: LeadControlKind::Reject,
            thread_id: "t1".into(),
            reason: "redo it".into(),
            request_id: "r1".into(),
        };
        assert!(inbox.send(cmd.clone()), "a subscriber must receive it");
        assert_eq!(rx.recv().await.unwrap(), cmd);
    }

    #[tokio::test]
    async fn an_undelivered_command_returns_false() {
        let inbox = LeadControlInbox::new();
        let cmd = LeadControlCmd {
            project_key: "/a/.yi-agent".into(),
            kind: LeadControlKind::Interrupt,
            thread_id: "t1".into(),
            reason: "stop".into(),
            request_id: "r1".into(),
        };
        // 无订阅者：返回 false，调用方据此回一条「主循环未在听」的错误。
        assert!(!inbox.send(cmd));
    }

    #[tokio::test]
    async fn resolving_a_registered_request_hands_back_the_outcome() {
        let replies = LeadControlReplies::default();
        let rx = replies.register("r1".into());
        assert!(replies.resolve("r1", Ok("done".into())));
        assert_eq!(rx.await.unwrap().unwrap(), "done");
    }

    #[tokio::test]
    async fn resolving_an_unknown_request_is_a_noop() {
        let replies = LeadControlReplies::default();
        assert!(!replies.resolve("nope", Ok("x".into())));
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_control`
Expected: 编译失败（`unresolved import` / 类型不存在）。

- [ ] **Step 3: 写最小实现**

在 `lead_control.rs` 顶部：

```rust
//! 组长的**写通道**：把「取消 / 打回」这类针对**别的 thread** 的写动作，
//! 从组长工具（driver 侧）送到主循环（唯一持有 `&mut threads` 的地方）。
//!
//! 为什么不直接写：`threads` 是主循环独占的 `&mut`（上级 spec §6.1），组长工具
//! 在装配期只拿到 `project_key`，天然碰不到它。这与 P2 的后台 tick 同一约束，
//! 故解法同款——一条 broadcast 通道 + 主循环 `select!` 消费。
//!
//! 方向与 `lead::LeadInbox` **相反**：那个是 thread→lead（会话给组长留言），
//! 这个是 lead→thread（组长下指令）。刻意不复用同一条通道：两种语义挤在一起
//! 后续必长歪（见 spec §5.2）。
//!
//! 本模块只放**纯逻辑**：指令结构、通道、目标分类判定。真正的执行在主循环里
//! （它才知道 `threads` 的内容）——这样「同项目/异项目/不在场」的判定可以脱
//! 主循环单测，零 LLM、零 I/O。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{broadcast, oneshot};

/// 组长能对另一个 thread 做的写动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeadControlKind {
    /// 打断目标当前的活跃轮（复用 `turn/interrupt` 语义）。
    Interrupt,
    /// 打回：向目标投递一条「组长指令 + 理由」（非阻塞注入；若目标无活跃轮则起一轮）。
    Reject,
}

/// 一条投给主循环的写指令。
///
/// 纯进程内结构（非 wire 类型，故与 `serde` 无关），与 `lead::LeadEnvelope` 同款。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeadControlCmd {
    /// 发起这条指令的组长所属的项目键。
    pub project_key: String,
    pub kind: LeadControlKind,
    /// 目标 thread（必须在同一个项目 store 内）。
    pub thread_id: String,
    /// 给用户/目标的理由——打回会把它带进目标转录。
    pub reason: String,
    /// 结果回填 id：工具侧登记一个 oneshot，主循环执行完按它回填。
    pub request_id: String,
}

/// 组长的写通道（发送端）。
///
/// 用 `broadcast` 而非 `mpsc`：与 `LeadInbox` 同一理由——主循环是唯一消费者，
/// 但订阅者（主循环）可能晚于发送方建立，`broadcast` 让「有没有人听」成为
/// 一次可判定的返回值（`send` 返回订阅者数是否为 0），而不是静默丢弃。
#[derive(Clone)]
pub struct LeadControlInbox {
    tx: broadcast::Sender<LeadControlCmd>,
}

impl LeadControlInbox {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(64);
        Self { tx }
    }

    /// 投递一条指令。返回 `true` 表示至少有一个订阅者（主循环在听）。
    ///
    /// 与 `LeadInbox::deliver` 的差别：那个把「无人听」退化成 `Ok(())`（组长尚未
    /// 拉起不算错），这个**必须如实返回 false**——主循环不在听意味着这条写动作
    /// 根本不会发生，静默成功会让组长以为已经打回/取消了。调用方据此回错。
    pub fn send(&self, cmd: LeadControlCmd) -> bool {
        self.tx.send(cmd).is_ok()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LeadControlCmd> {
        self.tx.subscribe()
    }
}

impl Default for LeadControlInbox {
    fn default() -> Self {
        Self::new()
    }
}

/// 写结果的登记表。形状照抄 `lead::LeadReplies`：投递方登记 `oneshot` 后 await，
/// 主循环执行完回填 `Ok(执行摘要)` 或 `Err(失败原因)`。
///
/// 结果是 `Result` 而非裸 `String`：写动作可能失败（目标无活跃轮、目标不在场），
/// 失败必须让组长看到，不能伪装成成功。
#[derive(Clone, Default)]
pub struct LeadControlReplies {
    waiting: Arc<Mutex<HashMap<String, oneshot::Sender<Result<String, String>>>>>,
}

impl LeadControlReplies {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一次等待，返回接收端。等待方超时后 drop 掉接收端即可（`resolve` 会安全失败）。
    pub fn register(&self, request_id: String) -> oneshot::Receiver<Result<String, String>> {
        let (tx, rx) = oneshot::channel();
        self.waiting
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(request_id, tx);
        rx
    }

    /// 回填一次。无人等待（已超时 / 从未登记）返回 `false`，不 panic。
    pub fn resolve(&self, request_id: &str, outcome: Result<String, String>) -> bool {
        let sender = self
            .waiting
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(request_id);
        match sender {
            Some(tx) => tx.send(outcome).is_ok(),
            None => false,
        }
    }
}

/// 一条指令的目标在**本进程**里能不能落地。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlTarget {
    /// 目标 thread 就在本项目的 `threads` 里，可以执行。
    Session,
    /// 目标不可达：项目键不符，或该 thread 不在场的冷会话。
    NotOpen,
}

/// 判定指令目标是否可执行。
///
/// `project_key` = 指令发起方（组长）的项目键，`cmd_project_key` = 主循环当前正在
/// 服务的项目键，`in_threads` = 目标是否在当前 `threads` 里。
///
/// 项目键必须**完全相等**：不同项目即使有同名 thread，也绝不能互达（跨项目写是
/// 隔离不变量的红线）。冷会话（不在 `threads`）一律 `NotOpen`——给别的 thread
/// 冷启动一轮需要复刻 `deliver_to_lead` 的整套恢复逻辑，超出本设计（spec §5.4）。
pub fn classify_target(project_key: &str, cmd_project_key: &str, in_threads: bool) -> ControlTarget {
    if project_key != cmd_project_key {
        return ControlTarget::NotOpen;
    }
    if !in_threads {
        return ControlTarget::NotOpen;
    }
    ControlTarget::Session
}
```

在 `lib.rs` 里加一行（放在 `lead_memory` 声明附近）：

```rust
pub(crate) mod lead_control;
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_control`
Expected: PASS（7 passed）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/lead_control.rs yi-agent-rs/crates/yi-agent-app-server/src/lib.rs
git commit -m "feat(app-server): 组长写通道 lead_control（指令 + 通道 + 目标判定）"
```

---

### Task 2: 两个组长写工具

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lead_control.rs`（加两个 `Tool` 实现，放在 `mod tests` 之前）
- Test: 同文件 `mod tests` 追加

**Interfaces:**
- Consumes: Task 1 的 `LeadControlInbox` / `LeadControlCmd` / `LeadControlKind` / `LeadControlReplies`；`crate::lead_memory::lead_memory_metadata`（**注意**：当前是私有 `fn`，本 Task 第一步改成 `pub(crate) fn`）。
- Produces:
  - `pub struct LeadInterruptTool { inbox: LeadControlInbox, replies: LeadControlReplies, project_key: String }`，`new(inbox, replies, project_key) -> Self`
  - `pub struct LeadRejectTool { inbox: LeadControlInbox, replies: LeadControlReplies, project_key: String }`，`new(...) -> Self`
  - 两者 `impl Tool`，`name()` 分别 `"lead_interrupt_thread"` / `"lead_reject_thread"`。

- [ ] **Step 1: 写失败测试**

在 `lead_control.rs` 的 `mod tests` 里追加：

```rust
    use yi_agent_core::Tool;

    #[tokio::test]
    async fn interrupt_reports_an_empty_reason_as_an_error() {
        let tool = LeadInterruptTool::new(
            LeadControlInbox::new(),
            LeadControlReplies::default(),
            "/a/.yi-agent".into(),
        );
        let out = tool
            .call(serde_json::json!({ "thread_id": "t1", "reason": "   " }))
            .await;
        assert!(out.is_error, "a blank reason must be rejected");
    }

    #[tokio::test]
    async fn interrupt_fails_when_no_one_is_listening() {
        // 无订阅者 = 主循环不在听：绝不能报成功。
        let tool = LeadInterruptTool::new(
            LeadControlInbox::new(),
            LeadControlReplies::default(),
            "/a/.yi-agent".into(),
        );
        let out = tool
            .call(serde_json::json!({ "thread_id": "t1", "reason": "stop" }))
            .await;
        assert!(out.is_error, "no subscriber must be an error, got {out:?}");
    }

    #[tokio::test]
    async fn interrupt_returns_the_main_loop_outcome() {
        let inbox = LeadControlInbox::new();
        let replies = LeadControlReplies::default();
        let mut rx = inbox.subscribe();
        let tool = LeadInterruptTool::new(inbox, replies.clone(), "/a/.yi-agent".into());
        // 扮演主循环：收到指令后回填失败（目标无活跃轮）。
        let outcome = tokio::spawn(async move {
            let cmd = rx.recv().await.unwrap();
            assert_eq!(cmd.kind, LeadControlKind::Interrupt);
            assert_eq!(cmd.thread_id, "t1");
            replies.resolve(&cmd.request_id, Err("目标没有活跃轮".into()));
        });
        let out = tool
            .call(serde_json::json!({ "thread_id": "t1", "reason": "stop" }))
            .await;
        outcome.await.unwrap();
        assert!(out.is_error, "a failed interruption must surface as an error");
    }

    #[tokio::test]
    async fn reject_carries_the_reason_to_the_main_loop() {
        let inbox = LeadControlInbox::new();
        let replies = LeadControlReplies::default();
        let mut rx = inbox.subscribe();
        let tool = LeadRejectTool::new(inbox, replies.clone(), "/a/.yi-agent".into());
        let outcome = tokio::spawn(async move {
            let cmd = rx.recv().await.unwrap();
            assert_eq!(cmd.kind, LeadControlKind::Reject);
            assert_eq!(cmd.reason, "redo with tests");
            replies.resolve(&cmd.request_id, Ok("已打回".into()));
        });
        let out = tool
            .call(serde_json::json!({ "thread_id": "t2", "reason": "redo with tests" }))
            .await;
        outcome.await.unwrap();
        assert!(!out.is_error, "a successful reject must not be an error: {out:?}");
        assert!(
            format!("{out:?}").contains("已打回") || serde_json::to_string(&out.content).unwrap().contains("已打回"),
            "the outcome must reach the tool result: {out:?}"
        );
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_control`
Expected: 编译失败（`LeadInterruptTool` 未定义）。

- [ ] **Step 3: 写最小实现**

先改 `lead_memory.rs:110` 的 `fn lead_memory_metadata()` → `pub(crate) fn lead_memory_metadata()`。

在 `lead_control.rs` 的 `mod tests` 之前追加：

```rust
use serde::Deserialize;
use serde_json::Value;
use yi_agent_core::{Tool, ToolMetadata, ToolResult};

/// 两个写工具共用的参数形状。
#[derive(Deserialize)]
struct LeadControlArgs {
    thread_id: String,
    #[serde(default)]
    reason: String,
}

/// 两个写工具共用的执行骨架：校验 → 投递 → 等主循环回填结果。
///
/// 抽出来是为了让「校验 + 投递 + 有界等待 + 错误归一」只有一份；两个工具的差别
/// 仅在 `kind` 与文案。
async fn run_control(
    inbox: &LeadControlInbox,
    replies: &LeadControlReplies,
    project_key: &str,
    kind: LeadControlKind,
    args: Value,
) -> ToolResult {
    let parsed: LeadControlArgs = match serde_json::from_value(args) {
        Ok(a) => a,
        Err(e) => return ToolResult::error(format!("invalid arguments: {e}")),
    };
    let thread_id = parsed.thread_id.trim().to_string();
    if thread_id.is_empty() {
        return ToolResult::error("thread_id must not be empty");
    }
    let reason = parsed.reason.trim().to_string();
    if reason.is_empty() {
        return ToolResult::error("reason must not be empty");
    }
    let request_id = format!("lead-ctl-{}", uuid::Uuid::new_v4());
    let rx = replies.register(request_id.clone());
    let delivered = inbox.send(LeadControlCmd {
        project_key: project_key.to_string(),
        kind,
        thread_id,
        reason,
        request_id,
    });
    if !delivered {
        // 主循环不在听：这条写动作不会发生，如实报错（不静默成功）。
        return ToolResult::error(
            "组长写通道当前无人接收（主循环未在运行），本条指令未下达。",
        );
    }
    // 有界等待：主循环是本进程内一步操作，毫秒级；给 5s 兜底，避免主循环卡死时
    // 工具永久挂住（与 `ask_lead blocking` 的有界收口同一原则）。
    match tokio::time::timeout(std::time::Duration::from_secs(5), rx).await {
        Ok(Ok(Ok(summary))) => ToolResult::text(summary),
        Ok(Ok(Err(reason))) => ToolResult::error(format!("指令未能执行：{reason}")),
        Ok(Err(_)) => ToolResult::error("组长写通道已关闭。"),
        Err(_) => ToolResult::error("等待主循环执行超时（5s）。"),
    }
}

/// 两个写工具的元数据：与 `lead_remember` 同源（`ToolSource::Plugin { name: "lead" }`），
/// `requires_confirmation: false`（后台无人值守时若需批准，功能就废了，spec §6）。
/// **不设** `read_only`——它们是写动作。
fn lead_control_metadata() -> ToolMetadata {
    crate::lead_memory::lead_memory_metadata()
}

/// 打断目标 thread 的当前活跃轮。**只由组长使用**（注册点见 `build_runtime_tooling`）。
pub struct LeadInterruptTool {
    inbox: LeadControlInbox,
    replies: LeadControlReplies,
    project_key: String,
}

impl LeadInterruptTool {
    pub fn new(inbox: LeadControlInbox, replies: LeadControlReplies, project_key: String) -> Self {
        Self {
            inbox,
            replies,
            project_key,
        }
    }
}

#[async_trait::async_trait]
impl Tool for LeadInterruptTool {
    fn name(&self) -> &str {
        "lead_interrupt_thread"
    }

    fn description(&self) -> &str {
        "Interrupt the current in-flight turn of another thread in this project (same effect as the user pressing Esc). Fails if that thread has no active turn — it never silently no-ops. Use it to stop a thread that has gone off track mid-turn; to correct a thread without stopping it, use lead_reject_thread instead."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "thread_id": { "type": "string", "description": "The thread whose current turn to interrupt (must be in this project)." },
                "reason": { "type": "string", "description": "Why you are interrupting. Recorded so the user can audit it." }
            },
            "required": ["thread_id", "reason"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        run_control(
            &self.inbox,
            &self.replies,
            &self.project_key,
            LeadControlKind::Interrupt,
            args,
        )
        .await
    }

    fn metadata(&self) -> ToolMetadata {
        lead_control_metadata()
    }
}

/// 打回目标 thread：投递一条「组长指令 + 理由」。**只由组长使用**。
pub struct LeadRejectTool {
    inbox: LeadControlInbox,
    replies: LeadControlReplies,
    project_key: String,
}

impl LeadRejectTool {
    pub fn new(inbox: LeadControlInbox, replies: LeadControlReplies, project_key: String) -> Self {
        Self {
            inbox,
            replies,
            project_key,
        }
    }
}

#[async_trait::async_trait]
impl Tool for LeadRejectTool {
    fn name(&self) -> &str {
        "lead_reject_thread"
    }

    fn description(&self) -> &str {
        "Send a correcting instruction to another thread in this project. If that thread is mid-turn the instruction is folded into the current turn (like a user interjection); if it is idle a fresh turn is started so the correction takes effect. To retract an earlier rejection, send another one that says so — the transcript is the ledger. Use it to steer a thread that is heading the wrong way."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "thread_id": { "type": "string", "description": "The thread to reject (must be in this project)." },
                "reason": { "type": "string", "description": "The correcting instruction plus why. It lands verbatim in that thread's transcript, so write it for the thread to act on." }
            },
            "required": ["thread_id", "reason"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        run_control(
            &self.inbox,
            &self.replies,
            &self.project_key,
            LeadControlKind::Reject,
            args,
        )
        .await
    }

    fn metadata(&self) -> ToolMetadata {
        lead_control_metadata()
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_control`
Expected: PASS（Task 1 的 7 + 本 Task 4 = 11 passed）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/lead_control.rs yi-agent-rs/crates/yi-agent-app-server/src/lead_memory.rs
git commit -m "feat(app-server): 组长写工具 lead_interrupt_thread / lead_reject_thread"
```

---

### Task 3: 主循环执行写指令

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
- Test: 同文件逻辑靠 Task 5 的集成测试覆盖（本 Task 只接线，不新增单测）

**Interfaces:**
- Consumes: Task 1/2 的 `LeadControlInbox` / `LeadControlCmd` / `LeadControlKind` / `LeadControlReplies` / `classify_target` / `ControlTarget`；`ServeLauncher` 的既有 `threads`/`build_agent` 等字段。
- Produces: `ServeLauncher` 新字段 `lead_control: &'a crate::lead_control::LeadControlInbox`；`pub(crate) async fn apply_lead_control(&mut self, cmd: LeadControlCmd) -> Result<String, String>`（在 `impl<F> ServeLauncher<'_, F>` 内）。

- [ ] **Step 1: 给 ServeLauncher 加字段**

在 `struct ServeLauncher`（`server.rs:5012`）末尾、`lead_target` 字段之后加：

```rust
    /// 组长写通道：工具侧把「取消 / 打回」投进这里，主循环消费它去动 `threads`。
    /// 与 `lead_inbox` 相对（那个是 thread→lead，这个是 lead→thread），同一实例、
    /// 同一注入理由——自造一份就投进没人听的通道。
    lead_control: &'a crate::lead_control::LeadControlInbox,
```

- [ ] **Step 2: 实现执行逻辑**

在 `impl<F> ServeLauncher<'_, F>`（`server.rs:5113`）里、`prepare_and_deliver` 之前加：

```rust
    /// 执行一条组长写指令。
    ///
    /// 只有主循环能调：它持有 `&mut self.threads`。工具侧永远够不到这条路径
    /// （它们只往 `lead_control` 投递），这就是「守住 §6.1 独占」的落点。
    async fn apply_lead_control(
        &mut self,
        cmd: crate::lead_control::LeadControlCmd,
    ) -> Result<String, String> {
        use crate::lead_control::{classify_target, ControlTarget, LeadControlKind};

        // 目标 thread 所属的项目键：从它的 store 现取（每个 thread 有自己的
        // `cwd` → store，`threads` 里可能有多个项目的 thread）。取不到 = 目标不在场。
        let target_project_key = self
            .threads
            .get(&cmd.thread_id)
            .map(|s| s.store.project_key());
        let target = classify_target(
            &cmd.project_key,
            target_project_key.as_deref().unwrap_or(""),
            target_project_key.is_some(),
        );
        if target == ControlTarget::NotOpen {
            return Err(format!(
                "目标会话 {} 当前未打开（或不属于本项目），无法下达指令",
                cmd.thread_id
            ));
        }

        match cmd.kind {
            LeadControlKind::Interrupt => {
                let Some(session) = self.threads.get(&cmd.thread_id) else {
                    return Err("目标会话不在场".to_string());
                };
                let Some(turn_id) = session.active_turn_id.clone() else {
                    // 无活跃轮：报错，不静默 no-op（spec §3.2）。
                    return Err("目标当前没有活跃轮，无法打断".to_string());
                };
                // 与 `turn/interrupt` 逐字同源：幂等 level 信号 + try_send 不阻塞。
                let _ = session.interrupt_tx.try_send(turn_id);
                Ok(format!("已打断 {} 的当前轮", cmd.thread_id))
            }
            LeadControlKind::Reject => {
                let text = format!("[组长打回] {}", cmd.reason);
                // 有活跃轮 → 走 interjection（与用户中途打字同一路径）；
                // 无活跃轮 → 起一轮（spec §3.1）。
                if let Some(session) = self.threads.get(&cmd.thread_id) {
                    if let Some(turn_id) = session.active_turn_id.clone() {
                        let interjection_id =
                            format!("interject-{}-{}", turn_id, uuid::Uuid::new_v4());
                        if session
                            .interject_tx
                            .send(crate::session::InterjectionRequest {
                                turn_id,
                                interjection_id,
                                text,
                            })
                            .await
                            .is_err()
                        {
                            return Err("目标 driver 已退出".to_string());
                        }
                        return Ok(format!("已把打回指令并入 {} 的当前轮", cmd.thread_id));
                    }
                }
                // 无活跃轮（或目标刚转为 idle）：起一轮，走 prepare_and_deliver。
                self.prepare_and_deliver(&cmd.thread_id, &text)
                    .await
                    .map(|tid| format!("已向 {tid} 投递打回指令（起了新的一轮）"))
                    .map_err(|e| e.to_string())
            }
        }
    }
```

> **实现者注意**：这段用 `self.threads.get(&cmd.thread_id).map(|s| s.store.project_key())` 取目标项目键——每个 `ThreadSession` 自带 `store`（`session.rs` 的 `Arc<ThreadStore>`），而 `ThreadStore::project_key()`（`thread_store.rs:418`）给出该 thread 所属项目。**这是必要的**：`serve()` 的 `threads` 里可能同时有**多个项目**的 thread（侧栏跨 workspace），不能假定"在 `threads` 里就是本项目"。`classify_target` 要求项目键完全相等，跨项目目标一律 `NotOpen`（daemon 隔离不变量）。

- [ ] **Step 3: 主循环接线**

在 `serve()` 里、`lead_rx` 订阅处（`server.rs:2451`）之后加：

```rust
    // 组长写通道的消费端。与 `lead_rx` 同一理由：订阅要在投递之前建立，
    // 且消费必须留在本循环的 `select!`（`threads` 是本函数独占的 `&mut`）。
    let mut lead_control_rx = lead_control.subscribe();
```

在 `select!` 里、`envelope = lead_rx.recv()` 分支之后加一个新分支：

```rust
            cmd = lead_control_rx.recv() => {
                let Ok(cmd) = cmd else { continue };
                // 照着 `lead_rx` 分支的形状构造 launcher（每个分支都得自带一份，
                // 因为 `threads` 是 `&mut`，不能跨分支共享）。
                let mut launcher = ServeLauncher {
                    threads: &mut threads,
                    pending_activation: &mut pending_activation,
                    process_watches: &mut process_watches,
                    uploads: &mut uploads,
                    runtimes: &runtimes,
                    thread_roots: &thread_roots,
                    models_path: &models_path,
                    cfg: &cfg,
                    hub: &hub,
                    turn_tx: &turn_tx,
                    pending: &pending,
                    permission_timeout,
                    perm_seq: &perm_seq,
                    theme: &theme,
                    git_diff: &git_diff_handle,
                    workspaces: &workspaces,
                    build_agent: &build_agent,
                    lead_inbox: &lead_inbox,
                    lead_replies: &lead_replies,
                    lead_control: &lead_control,
                    // 写指令不投递给组长，故没有 lead target。
                    lead_target: None,
                };
                let request_id = cmd.request_id.clone();
                let outcome = launcher.apply_lead_control(cmd).await;
                // 回填结果：工具侧按 request_id 收口。无人等待（已超时）是安全的。
                lead_control_replies.resolve(&request_id, outcome);
            }
```

- [ ] **Step 4: 把通道透传进 serve 的 Attachments 与全部 launcher 构造点**

在 `Attachments` 结构体（`server.rs:227` 一带）加字段：

```rust
    /// 组长写通道:组长工具投递「取消 / 打回」,主循环消费。与 `lead_inbox` 相对、
    /// 同一注入理由——测试要在主循环之外拿同一个实例投递。
    pub(crate) lead_control: crate::lead_control::LeadControlInbox,
    /// 组长写结果登记表:工具侧登记 `request_id`,主循环执行完回填。
    pub(crate) lead_control_replies: crate::lead_control::LeadControlReplies,
```

在 `serve()` 的形参解构（`server.rs:2371` 一带）加：

```rust
        lead_control,
        lead_control_replies,
```

把所有 `ServeLauncher { ... lead_replies: ... }` 构造点补上两个新字段：
`lead_control: &lead_control,`（在 `serve` 内，用局部借用）。共 3 处：`driver_tick` 调用前的分支（`2664` 一带）、`lead_ticker.tick()` 分支（`3003`）、`lead_rx` 分支（`3055`）。每处在 `lead_replies: &lead_replies,` 之后加：

```rust
                        lead_control: &lead_control,
```

（`lead_rx` 与 `lead_ticker` 分支里已经 `lead_target: Some(...)`，保持不变。）

在三处生产 `Attachments { .. }` 字面量（`server.rs:1544`、`1628`、`7396`）与 `ws.rs:135` 补：

```rust
            lead_control: crate::lead_control::LeadControlInbox::new(),
            lead_control_replies: crate::lead_control::LeadControlReplies::new(),
```

在 `rpc_tests.rs` 的 `Harness` 结构体（`1588` 一带）加同名字段，并在 Harness 构造的 `Attachments { .. }`（`1806` 一带）里加：

```rust
                lead_control: lead_control.clone(),
                lead_control_replies: lead_control_replies.clone(),
```

并在 Harness 的构造入口（与 `lead_inbox`/`lead_replies` 同处，`rpc_tests.rs:146` 等）创建局部量：

```rust
    let lead_control = crate::lead_control::LeadControlInbox::new();
    let lead_control_replies = crate::lead_control::LeadControlReplies::new();
```

在 `rpc/mod.rs` 的 `AppState`（`348` 一带）加：

```rust
    /// 组长写通道与结果登记表。`thread/start` 与 `thread/resume` 各把前者交给
    /// `start_thread_core` / `attach_delegation`（转交给组长写工具）。与
    /// [`AppState::lead_inbox`] 同一理由，按 `&` 借用。
    pub(crate) lead_control: &'a crate::lead_control::LeadControlInbox,
    pub(crate) lead_control_replies: &'a crate::lead_control::LeadControlReplies,
```

并在 `AppState { .. }` 的构造点（`server.rs` 里 `serve` 内注入 AppState 处）补齐这两个字段。

- [ ] **Step 5: 编译并跑既有测试**

Run: `cd yi-agent-rs && cargo build -p yi-agent-app-server 2>&1 | tail -20`
Expected: 编译通过（若报缺字段，按编译器提示补齐所有 `ServeLauncher`/`Attachments`/`AppState` 字面量）。

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: PASS（826 + 11 = 837 passed；新分支未接工具，尚无新增用例）。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/
git commit -m "feat(app-server): 主循环消费组长写指令并回填结果"
```

---

### Task 4: 注册两个写工具（仅组长可见）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`build_runtime_tooling` 的 `if is_lead { .. }` 分支 + 形参）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`（门控测试）

**Interfaces:**
- Consumes: Task 2 的 `LeadInterruptTool` / `LeadRejectTool`；Task 3 透传的通道。
- Produces: `build_runtime_tooling` 新形参 `lead_control: crate::lead_control::LeadControlInbox, lead_control_replies: crate::lead_control::LeadControlReplies`。

- [ ] **Step 1: 写失败测试（门控）**

在 `rpc_tests.rs` 里，紧跟 `only_the_lead_sees_the_memory_tools`（`422`）之后加一个同形状测试：

```rust
/// 两个写工具只给组长：普通会话装配后注册表里绝不能有它们。
#[test]
fn only_the_lead_sees_the_control_tools() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let mut cfg = test_config();
    cfg.workdir = repo.path().to_path_buf();

    let attached = Arc::new(
        yi_agent_subagent::attach::attach_project_runtime(
            &cfg,
            runtime.path().to_path_buf(),
            &isolated_models_path(runtime.path()),
        )
        .expect("a clean git repo must attach"),
    );
    let binding = RuntimeBinding::managed(
        &cfg,
        runtime.path().to_path_buf(),
        isolated_models_path(runtime.path()),
        Arc::clone(&attached),
    );
    let root = ThreadRoot::from_handle(binding, attached.attached_root.clone());

    // 普通会话（无 role: lead 的 meta）——两个写工具都不该出现。
    let names = build_runtime_tooling(
        &cfg,
        &root,
        "thread-plain",
        yi_agent_core::autonomy::YoloSwitch::new(false),
        test_theme(),
        crate::git_diff_tool::GitDiffHandle::new(),
        Vec::new(),
        "proj-test".to_string(),
        crate::lead::LeadInbox::new(),
        crate::lead::LeadReplies::new(),
        crate::lead_control::LeadControlInbox::new(),
        crate::lead_control::LeadControlReplies::new(),
        None,
    )
    .expect("tooling")
    .registry
    .names();

    assert!(
        !names.contains(&"lead_interrupt_thread".to_string())
            && !names.contains(&"lead_reject_thread".to_string()),
        "a plain thread must not see the control tools, got {names:?}"
    );

    // 组长（role: lead）——两个写工具都必须出现。用**生产的组长落盘路径**
    // 造 meta，而不是手搓 role 字段（与 `only_the_lead_sees_the_memory_tools` 同一手法）。
    let store = crate::thread_store::ThreadStore::new(&cfg.workdir);
    let (lead_meta, _created) = crate::lead::ensure_lead_meta(&store, &project_root)
        .expect("the production lead path must persist a lead meta");

    let lead_names = build_runtime_tooling(
        &cfg,
        &root,
        &lead_meta.thread_id,
        yi_agent_core::autonomy::YoloSwitch::new(false),
        test_theme(),
        crate::git_diff_tool::GitDiffHandle::new(),
        Vec::new(),
        "proj-test".to_string(),
        crate::lead::LeadInbox::new(),
        crate::lead::LeadReplies::new(),
        crate::lead_control::LeadControlInbox::new(),
        crate::lead_control::LeadControlReplies::new(),
        None,
    )
    .expect("tooling")
    .registry
    .names();

    assert!(
        lead_names.contains(&"lead_interrupt_thread".to_string())
            && lead_names.contains(&"lead_reject_thread".to_string()),
        "the lead must see both control tools, got {lead_names:?}"
    );
}
```

> **实现者注意**：`ensure_lead_meta` 要求 `project_root` 是有效项目目录（非 kanban worktree）；
> 本测试的 `project_root`（`repo.path().canonicalize()`）满足。`lead_meta.thread_id` 是
> 生产路径铸出的组长 id，直接用它调 `build_runtime_tooling`（`is_lead_thread` 会读到刚落的 role）。
> 若 `ensure_lead_meta` 的签名与上面不符，按编译器提示调整。

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib only_the_lead_sees_the_control_tools`
Expected: 编译失败（`build_runtime_tooling` 参数不匹配）。

- [ ] **Step 3: 改 build_runtime_tooling 形参与注册**

在 `build_runtime_tooling`（`server.rs:949`）形参表 `lead_replies` 之后加：

```rust
    // 组长写通道与结果登记表:组长的两个写工具经它们把「取消 / 打回」送到主循环。
    // 与 `lead_inbox`/`lead_replies` 同一注入理由(装配期绑,否则工具投进没人听的通道)。
    lead_control: crate::lead_control::LeadControlInbox,
    lead_control_replies: crate::lead_control::LeadControlReplies,
```

在 `if is_lead { .. }` 分支（`server.rs:1069`）里、两个账本工具注册之后加：

```rust
        // 两个写工具:组长对「项目里别的 thread」的写能力(spec §3)。
        // 与账本工具同门控(唯一权威 = 落盘 role),普通会话与子 worker 看不到。
        registry.register(Arc::new(crate::lead_control::LeadInterruptTool::new(
            lead_control.clone(),
            lead_control_replies.clone(),
            project_key.clone(),
        )));
        registry.register(Arc::new(crate::lead_control::LeadRejectTool::new(
            lead_control,
            lead_control_replies,
            project_key,
        )));
```

> **注意借用顺序**：`project_key` 在上面已经被 `AskLeadTool::new(...)` move 过一次吗？不会——`if !is_lead` 分支里才用 `project_key`。但本分支里 `project_key.clone()` 再 move 是对的；若编译报 `project_key` 已被移动，检查是否在更早处 move 了它，并改为克隆。

在 `attach_delegation`（`server.rs:423` 的 `build_runtime_tooling(...)` 调用）补两个实参：

```rust
        lead_control.clone(),
        lead_control_replies.clone(),
```

并给 `attach_delegation` 形参（`server.rs:351`）加：

```rust
    lead_control: &crate::lead_control::LeadControlInbox,
    lead_control_replies: &crate::lead_control::LeadControlReplies,
```

在 `start_thread_core`（`server.rs:5643`）形参加同样两项并按名透传。在其 3 个调用点（`server.rs:5150`、`5420`、`rpc/thread.rs:289`）补实参。

在 `rpc_tests.rs` 的 `build_runtime_tooling(...)` 全部调用点（`137/219/273/336/505/642/718/14415/14918/16348/16370/16430/16499`）在 `lead_replies` 实参之后加：

```rust
        crate::lead_control::LeadControlInbox::new(),
        crate::lead_control::LeadControlReplies::new(),
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib only_the_lead_sees_the_control_tools`
Expected: PASS。

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: PASS（837 + 1 = 838 passed）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/
git commit -m "feat(app-server): 组长写工具只对组长注册（门控测试）"
```

---

### Task 5: 集成验证（打回 / 取消 / 冷会话）

**Files:**
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`

**Interfaces:**
- Consumes: Task 3 的 `apply_lead_control`、Task 4 的注册；既有 Harness。

- [ ] **Step 1: 写集成测试**

在 `rpc_tests.rs` 末尾加：

```rust
/// 打回到 idle 目标：起一轮，目标转录里出现打回文本。
#[tokio::test(flavor = "multi_thread")]
async fn a_reject_to_an_idle_thread_starts_a_turn_that_carries_the_reason() {
    let (mut h, dir) = isolated_harness().await;
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let _ = read_response(&mut h, 1).await;
    let target = start_thread(&mut h).await;

    // 扮演组长：直接往共享的写通道投递一条 Reject（绕过组长工具本身，
    // 单测主循环的执行路径——工具侧已由 lead_control 的单测覆盖）。
    let project_key = crate::thread_store::ThreadStore::new(dir.path()).project_key();
    let request_id = "ctl-1".to_string();
    let rx = h.lead_control_replies.register(request_id.clone());
    assert!(
        h.lead_control.send(crate::lead_control::LeadControlCmd {
            project_key,
            kind: crate::lead_control::LeadControlKind::Reject,
            thread_id: target.clone(),
            reason: "redo with tests".to_string(),
            request_id,
        }),
        "the main loop must be subscribed"
    );

    let outcome = tokio::time::timeout(EVENT_WAIT, rx)
        .await
        .expect("the main loop must answer")
        .expect("the replies table hands back an outcome");
    assert!(outcome.is_ok(), "a reject to a live thread must succeed: {outcome:?}");

    // 目标转录里出现打回文本（经注入落盘为新轮的用户条目）。
    let read = rpc(
        &mut h,
        2,
        "thread/readItems",
        serde_json::json!({ "threadId": target }),
    )
    .await;
    assert!(
        read.to_string().contains("redo with tests"),
        "the rejection must land in the target transcript: {read}"
    );

    h.shutdown().await;
}

/// 打回到不在场的冷会话：回错，不起轮。
#[tokio::test(flavor = "multi_thread")]
async fn a_reject_to_an_unknown_thread_is_an_error() {
    let (mut h, dir) = isolated_harness().await;
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let _ = read_response(&mut h, 1).await;
    let project_key = crate::thread_store::ThreadStore::new(dir.path()).project_key();
    let request_id = "ctl-2".to_string();
    let rx = h.lead_control_replies.register(request_id.clone());
    assert!(h.lead_control.send(crate::lead_control::LeadControlCmd {
        project_key,
        kind: crate::lead_control::LeadControlKind::Reject,
        thread_id: "thread-does-not-exist".to_string(),
        reason: "x".to_string(),
        request_id,
    }));
    let outcome = tokio::time::timeout(EVENT_WAIT, rx)
        .await
        .expect("the main loop must answer")
        .expect("an outcome");
    assert!(outcome.is_err(), "an unreachable target must be an error");
    h.shutdown().await;
}

/// 打断无活跃轮的目标：回错（不静默 no-op）。
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupt_without_an_active_turn_is_an_error() {
    let (mut h, dir) = isolated_harness().await;
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let _ = read_response(&mut h, 1).await;
    let target = start_thread(&mut h).await;
    let project_key = crate::thread_store::ThreadStore::new(dir.path()).project_key();
    let request_id = "ctl-3".to_string();
    let rx = h.lead_control_replies.register(request_id.clone());
    assert!(h.lead_control.send(crate::lead_control::LeadControlCmd {
        project_key,
        kind: crate::lead_control::LeadControlKind::Interrupt,
        thread_id: target,
        reason: "stop".to_string(),
        request_id,
    }));
    let outcome = tokio::time::timeout(EVENT_WAIT, rx)
        .await
        .expect("the main loop must answer")
        .expect("an outcome");
    assert!(outcome.is_err(), "no active turn must be an error, not a silent no-op");
    h.shutdown().await;
}
```

> **实现者注意**：`Harness` 需要与 `lead_inbox`/`lead_replies` 并列的公开字段 `lead_control: crate::lead_control::LeadControlInbox` 与 `lead_control_replies: crate::lead_control::LeadControlReplies`（在 Task 3 Step 4 已要求加）。**不要**新增 `project_key()`/`read_items()` 辅助方法——照上面用既有的 `rpc(..)` + `ThreadStore::new(dir.path()).project_key()`。`start_thread`、`read_response`、`EVENT_WAIT`、`isolated_harness` 都是本文件既有 helper。

- [ ] **Step 2: 运行测试确认真实行为**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib "a_reject_to_an_idle_thread" "a_reject_to_an_unknown_thread" "an_interrupt_without_an_active_turn"`
Expected: PASS（3 passed）。若失败，按断言定位（多半是注入文本的前缀或落盘时机）。

- [ ] **Step 3: 全量回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: PASS（838 + 3 = 841 passed）。

- [ ] **Step 4: 跑 daemon 隔离不变量**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc -- two_conversation_roots_do_not_share_children_or_reach_across ending_one_conversation_reclaims_only_its_own_root
```
Expected: PASS（2 passed）。新通道是纯进程内，不新增跨 root capability。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs
git commit -m "test(app-server): 组长写指令的集成验证（打回/取消/冷会话）"
```

---

### Task 6: 修组长行长按菜单（P3-C ①）

**Files:**
- Modify: `desktop/src/lib/sidebarPress.ts`
- Test: `desktop/src/lib/sidebarPress.test.ts`

**Interfaces:**
- Consumes: 无。
- Produces: `resolveSidebarPressTarget` 对组长行（带 `data-lead-row`）返回 `null`；新导出常量 `LEAD_ROW_ATTR = "data-lead-row"`。

- [ ] **Step 1: 写失败测试**

在 `sidebarPress.test.ts` 的 `tree()` 里加一条组长行，并加一个用例：

```ts
// 在 tree() 的 innerHTML 里加：
//   <div data-lead-row="" data-thread-row="lead-1"><span id="lead-text">lead</span></div>
// 并返回 leadRow / leadText 两个查询结果。
```

用例：

```ts
  it("returns null for a press on the lead row (no session menu)", () => {
    const { leadText } = tree();
    expect(resolveSidebarPressTarget(leadText)).toBeNull();
  });

  it("still resolves a press inside a normal thread row to that row", () => {
    tree();
    expect(resolveSidebarPressTarget(document.querySelector("#r-text"))).toBe(
      document.querySelector("[data-thread-row]:not([data-lead-row])"),
    );
  });
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd desktop && npx vitest run src/lib/sidebarPress.test.ts`
Expected: FAIL（组长行被判定为会话行，返回了元素而非 `null`）。

- [ ] **Step 3: 改实现**

在 `sidebarPress.ts` 加常量并在判定里**先**排除组长行：

```ts
/** 组长行的标记属性：组长是项目级固定入口，不参与会话长按菜单。 */
export const LEAD_ROW_ATTR = "data-lead-row";
```

`resolveSidebarPressTarget` 里，在取 `row` 之前插入：

```ts
  // 组长行先排除：它是项目级固定入口，重命名/置顶/删除对它都没有意义。
  // 必须排在 `data-thread-row` 判定**之前**——组长行两者都带。
  if (target.closest(`[${LEAD_ROW_ATTR}]`)) return null;
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd desktop && npx vitest run src/lib/sidebarPress.test.ts`
Expected: PASS（全部用例）。

- [ ] **Step 5: 同步更新 ThreadSidebar 的注释**

在 `ThreadSidebar.tsx:772` 一带，把「带 `data-thread-row` 以便移动端长按与既有样式规则照旧生效」这句改成如实描述新行为：

```
  // 固定入口，置顶与删除对它没有意义。仍带 `data-thread-row`（选中逻辑与样式
  // 规则要用），但 `resolveSidebarPressTarget` 会因 `data-lead-row` 先返回 null，
  // 故手机长按组长行**不会**弹出会话菜单。
```

- [ ] **Step 6: 提交**

```bash
git add desktop/src/lib/sidebarPress.ts desktop/src/lib/sidebarPress.test.ts desktop/src/components/ThreadSidebar.tsx
git commit -m "fix(desktop): 组长行不参与移动端长按菜单"
```

---

### Task 7: 纯 CSS 组长头像（P3-C ②）

**Files:**
- Create: `desktop/src/components/LeadAvatar.tsx`
- Modify: `desktop/src/components/ThreadSidebar.tsx`（组长行左侧）
- Modify: `desktop/src/components/ChatView.tsx`（顶部标签，可选 prop）
- Test: `desktop/src/components/ThreadSidebar.test.tsx`

**Interfaces:**
- Consumes: 无。
- Produces: `export function LeadAvatar({ size }: { size?: number })`。

- [ ] **Step 1: 写失败测试**

在 `ThreadSidebar.test.tsx` 加：

```tsx
  it("renders a lead avatar on the lead row", () => {
    // render ThreadSidebar with a thread whose role === "lead"（沿用本文件既有
    // 的 fixture 手法），断言 `[data-lead-row]` 内出现 `[data-lead-avatar]`。
    // 断言用 querySelector 而非快照：头像样式不属于契约，存在性才是。
  });
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: FAIL（找不到 `[data-lead-avatar]`）。

- [ ] **Step 3: 写头像组件**

`LeadAvatar.tsx`：

```tsx
/**
 * 组长的 IM 式头像。**纯 CSS**：不引入图片资源（避免二进制资源与打包路径问题，
 * 见 spec §7.2）。用一个带首字母/图标的圆点，配 `data-lead-avatar` 供测试与
 * 后续样式挂钩。
 */
export function LeadAvatar({ size = 20 }: { size?: number }) {
  return (
    <span
      data-lead-avatar=""
      aria-hidden="true"
      className="inline-flex shrink-0 items-center justify-center rounded-full bg-accent text-[10px] font-semibold text-accent-fg"
      style={{ width: size, height: size }}
    >
      L
    </span>
  );
}
```

> **实现者注意**：`bg-accent` / `text-accent-fg` 若不在本项目的 Tailwind 主题里，用一个既有的高对比配色类替换（查 `desktop/tailwind.config.*` 或既有组件里的配色类）。目标是「与普通会话行视觉可区分」。

- [ ] **Step 4: 组长行接入头像**

在 `ThreadSidebar.tsx` 的 `renderLeadRow` 里，`<div className="min-w-0 flex-1">` 之前加 `<LeadAvatar />`；在文件顶部加 `import { LeadAvatar } from "./LeadAvatar";`。

`ChatView.tsx`：加可选 prop `isLead?: boolean`，为真时在顶部渲染 `<LeadAvatar size={16} />` 与文字「组长」。默认 `false`，既有调用点零改动。`App.tsx` 在渲染 `ChatView` 处按当前会话 `role === "lead"` 传入。（若 `ChatView` 顶部本就无标题栏，则只做侧栏头像，`ChatView` 改动可省——以既有布局为准，不硬造标题栏。）

- [ ] **Step 5: 运行测试确认通过**

Run: `cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: PASS。

- [ ] **Step 6: 提交**

```bash
git add desktop/src/components/LeadAvatar.tsx desktop/src/components/ThreadSidebar.tsx desktop/src/components/ThreadSidebar.test.tsx desktop/src/components/ChatView.tsx desktop/src/App.tsx
git commit -m "feat(desktop): 组长纯 CSS 头像（IM 式视觉区隔）"
```

---

### Task 8: 收口——全量验证与文档对齐

**Files:**
- Modify: `docs/superpowers/specs/2026-10-18-project-lead-agent-p3-control-design.md`（状态行改「已实现」）
- Modify: `docs/superpowers/specs/2026-10-17-project-lead-agent-design.md`（§11 的 P3 状态行）
- Modify: `docs/project-management/yi-agent-app-server.md`（新增 P3-B 条目）
- Modify: `README.md` 与 `docs/project-management/README.md`（计数各自 +1）

- [ ] **Step 1: 跑全量后端测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: PASS（841 passed；含本计划新增 15 条：Task1 7 + Task2 4 + Task4 1 + Task5 3）。

- [ ] **Step 2: 跑 daemon 不变量 + fmt**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc -- two_conversation_roots_do_not_share_children_or_reach_across ending_one_conversation_reclaims_only_its_own_root
cd yi-agent-rs && cargo fmt --all -- --check
```
Expected: 2 passed；fmt exit 0。

- [ ] **Step 3: 跑前端测试**

Run: `cd desktop && npx vitest run`
Expected: PASS（既有 + Task 6/7 新增）。

- [ ] **Step 4: 更新文档**

后端模块文件里新增 P3-B 条目（放 P2 条目之后）：

```
- [x] 项目组长 Agent（P3-B 写动作）—— `lead_interrupt_thread`（取消，复用
  `turn/interrupt`）+ `lead_reject_thread`（打回，非阻塞注入；idle 目标起一轮）。
  新增纯逻辑模块 `src/lead_control.rs`（指令 + 反向通道 `LeadControlInbox` +
  目标分类 `classify_target`）；主循环新 `select!` 分支消费并回填结果；只对组长
  注册（`is_lead_thread` 门控，`src/server.rs:1069` 一带）。留痕=两处转录，
  撤销=追加式 supersede。已知限制：无循环防护（spec §4.1）、冷会话不可下达指令
  （§5.4）。验证：`cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_control`
  —— [设计](../superpowers/specs/2026-10-18-project-lead-agent-p3-control-design.md)、
  [计划](../superpowers/plans/2026-10-18-project-lead-agent-p3-bc.md)
```

前端模块文件（`docs/project-management/desktop.md`）加 P3-C 条目：

```
- [x] 组长行移动端缺口 —— ①`resolveSidebarPressTarget` 先排除 `data-lead-row`，
  组长行不再弹会话长按菜单；②纯 CSS `LeadAvatar` 做 IM 式视觉区隔。
  验证：`cd desktop && npx vitest run src/lib/sidebarPress.test.ts`。
```

把本 spec 的状态行改「待实现」→「已实现」，上级 spec §11 的 P3 行改成
「P3-A/P3-B/P3-C 已实现」。

两个 README 的 `yi-agent-app-server` 计数**各自按实际值 +1**（先读现值，不要假定）。

- [ ] **Step 5: 提交**

```bash
git add docs/ README.md
git commit -m "docs: 项目组长 P3-B/C 实现状态与模块条目"
```

---

## Self-Review

**Spec coverage:**
- §3 工具清单（`lead_interrupt_thread`/`lead_reject_thread`）→ Task 2。
- §3.1 打回非阻塞注入（interject / 起轮）→ Task 3。
- §3.2 取消复用 `turn/interrupt`、无活跃轮报错 → Task 3。
- §3.3 追加式撤销 → 由工具描述承载（Task 2 的 `description`），无需代码。
- §3.4 纯转录留痕 → Task 5 断言目标转录含文本。
- §4 偏差（打回起轮、循环防护不做）→ 记在 spec，Task 3 实现起轮。
- §5 控制路径（独立通道 + 主循环消费）→ Task 1、3。
- §5.3 结果回填（oneshot）→ Task 1（`LeadControlReplies`）、Task 3。
- §5.4 冷 thread 报错 → Task 3、Task 5。
- §6 门控 → Task 4。
- §7.1 长按菜单 → Task 6。
- §7.2 头像 → Task 7。
- §7.3 `created_by` 不做 → 无需任务（范围外）。
- §8 测试 → Task 1/2/4/5/6/7。

**Placeholder scan:** 无 `TBD`/`TODO`/悬空占位。Task 3 Step 2 的项目键判定曾写作占位，已改为可直接落地的 `session.store.project_key()`（`thread_store.rs:418`）。Task 5 的测试一度用了不存在的 `Harness::start()`/`project_key()`/`read_items()`，已改为文件内既有的 `isolated_harness()`/`rpc()`/`read_response()`/`EVENT_WAIT`。

**Type consistency:** `LeadControlCmd` 字段（`project_key`/`kind`/`thread_id`/`reason`/`request_id`）在 Task 1 定义、Task 2 构造、Task 3 消费、Task 5 测试构造，名称一致。`LeadControlReplies::resolve` 签名（`Result<String,String>`）在 Task 1/3 一致。`classify_target` 三参数在 Task 1/3 一致。`build_runtime_tooling` 新形参名（`lead_control`/`lead_control_replies`）在 Task 3/4 一致。
