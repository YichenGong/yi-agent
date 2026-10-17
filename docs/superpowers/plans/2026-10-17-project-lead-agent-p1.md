# 项目组长 Agent P1（最小闭环）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让每个项目目录自动拥有一个「组长 thread」，普通 thread 能用 `ask_lead` 单向投递消息给它，桌面侧栏能识别并点开这个组长对话。

**Architecture:** 组长本质上是一个带 `role: "lead"` 标记的普通 thread，复用既有 `.jsonl` 转录、`.meta.json`、driver、`thread/listAll` 分组与桌面 `ChatView`。`ask_lead` 工具不直连子 agent runtime（daemon 的会话隔离不变量必须保持），而是把消息投给 app-server 主循环维护的**每项目组长信箱**；主循环再把信箱内容转成一次注入组长 thread 的 turn。组长的懒创建发生在 `thread/start` 与 `thread/listAll` 两条入口。

**Tech Stack:** Rust 2024（`yi-agent-app-server`）、TypeScript + React（`desktop/src`）、`cargo test`、`vitest`。

**Spec:** `docs/superpowers/specs/2026-10-17-project-lead-agent-design.md`（本计划只实现其 §11 的 P1）

---

## Global Constraints

- **本期只做 P1。** 不做：后台常驻循环、事件唤醒、前台抢占、阻塞式 `ask_lead`、`lead_memory`、`lead_list_threads` / `lead_read_thread`、权限留痕/撤销、移动端远程专用 UI。这些属 P2/P3，本计划的任何 task 都不得顺手实现。
- **daemon 隔离不变量不得改动。** 不得新增跨 root capability，不得改 `is_descendant_of`，不得动 `two_conversation_roots_do_not_share_children_or_reach_across` 与 `ending_one_conversation_reclaims_only_its_own_root` 这两条测试。
- **一个项目目录至多一个组长。** 判定键是 `ThreadStore::root()` 的父目录（即 `<cwd>/.yi-agent`）的 canonical 路径；同一 cwd 多次调用必须复用同一个组长 thread，绝不重复创建。
- **旧数据零迁移。** `ThreadMeta.role` 用 `#[serde(default)]`，旧 meta 缺字段一律解释为普通 thread（`None`）。
- **`ask_lead` 默认非阻塞。** P1 只实现 `blocking: false` 路径；显式 `blocking: true` 必须返回明确的「P1 未实现」错误文本，而不是静默当作非阻塞。
- **wire 字段缺省即保守。** `ThreadSummary.role` 缺失时前端必须按普通 thread 处理（`?? undefined`），不得假设它是组长。
- **测试基线**：`cargo test -p yi-agent-app-server --lib`；前端 `cd desktop && npx vitest run`。新增用例只增不改既有断言。
- **提交前必须 `cargo fmt --all`**（在 `yi-agent-rs/` 下），commit message 用 conventional commits，不写 `Co-Authored-By`。
- **不要在 `main` 上直接改代码**；执行时应在 worktree 内工作。

## 本仓库测试 API 实况（实现者必读，避免写出不能编译的测试）

`server.rs` 的测试夹具全在 `mod tests`（约 `:10507` 起）。**它们是自由函数 + 行式收发**：

| 设施 | 真实签名 | 说明 |
| --- | --- | --- |
| `Harness::new()` | `fn new() -> Self` | 默认配置；**workdir 是共享的固定路径**，需要隔离时必须用 `with_cfg` |
| `Harness::with_config` | `fn with_config<F>(cfg: RuntimeConfig, build: F, permission_timeout: Duration)` | 需要自定义 `workdir` 时用它 |
| `h.send` | `async fn send(&mut self, line: &str)` | 发一整行 JSON |
| `h.read_value` | `async fn read_value(&mut self) -> serde_json::Value` | 读下一帧（可能是通知） |
| `h.shutdown` | `async fn shutdown(self)` | 收尾 |
| `start_thread` | `async fn start_thread(h: &mut Harness) -> String` | `initialize` + `thread/start`，返回 tid |
| `rpc` | `async fn rpc(h: &mut Harness, id: u64, method: &str, params: Value) -> Value` | 发 + 按 id 收响应 |
| `list_all` | `async fn list_all(h: &mut Harness) -> Value` | 调 `thread/listAll` |
| 常量 | `PERMISSION_TIMEOUT`、`EVENT_WAIT`（30s） | |

**工具测试**：`git_diff_tool.rs` / `theme_tool.rs` 的做法是「句柄 + 工具」直接单测（`#[tokio::test]`，`handle.subscribe()` 后 `tool.call(json!({..}))`）。`ask_lead` 的工具测试照抄这个形状，不必起 app-server。

**工具 trait 真实签名**（`yi-agent-core/src/tool.rs:100`）：

```rust
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn schema(&self) -> Value;
    fn description(&self) -> &str;
    async fn call(&self, args: Value) -> ToolResult;
    fn metadata(&self) -> ToolMetadata { ToolMetadata::default() }
    // 其余方法均有默认实现，不必实现
}
```

---

## File Structure

| 文件 | 职责 | 动作 |
| --- | --- | --- |
| `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs` | `ThreadMeta` 增 `role`；新增「按目录找组长」查询 | Modify |
| `yi-agent-rs/crates/yi-agent-app-server/src/lead.rs` | **新**：组长角色常量、项目键计算、组长查找/懒创建、`LeadInbox` 信箱与 `AskLeadTool` | Create |
| `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs` | 注册 `pub mod lead;` | Modify |
| `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` | `thread/start` 与 `thread/listAll` 触发懒创建；主循环 drain 组长信箱并向组长 thread 投递 turn；`build_runtime_tooling` 注册 `ask_lead`；`thread_summary_json` 带出 `role` | Modify |
| `desktop/src/lib/protocol.ts` | `ThreadSummary.role?: "lead"` | Modify |
| `desktop/src/components/ThreadSidebar.tsx` | 组长条目渲染为固定头像 + 置顶分区 | Modify |
| `desktop/src/lib/leadThread.ts` | **新**：从 `ThreadSummary[]` 中挑选该项目组长的纯函数 | Create |

**分工理由**：把「组长是什么、怎么找、信箱怎么投」全部收进新的 `lead.rs`，使 `server.rs` 只增加接线而不增加业务规则——`server.rs` 已有一万三千余行，任何新规则都应落在独立文件里。

---

## Task 1: `ThreadMeta.role` 与按目录查找组长

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs:28-64`（`ThreadMeta` 结构体）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`（`impl ThreadStore`，新增两个方法）
- Test: 同文件 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: 无（本任务是地基）
- Produces:
  - `pub const LEAD_ROLE: &str = "lead";`
  - `ThreadMeta.role: Option<String>`（`#[serde(default)]`）
  - `ThreadStore::find_lead(&self) -> io::Result<Option<ThreadMeta>>` —— 返回本目录的组长 meta（按 `created_at` 升序取第一个，保证幂等）
  - `ThreadStore::project_key(&self) -> String` —— 组长归属键：`root()` 的父目录 canonical 字符串，失败时回退 `root()` 原样字符串

- [ ] **Step 1: 写失败测试**

在 `thread_store.rs` 的 `mod tests` 末尾追加：

```rust
    /// 组长标记必须能在落盘后读回,且旧 meta(无 `role`)读取为 `None`。
    #[test]
    fn role_round_trips_and_legacy_meta_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = ThreadStore::new(dir.path());

        let lead = ThreadMeta {
            thread_id: "thread-lead-1".into(),
            cwd: dir.path().display().to_string(),
            model: "m".into(),
            created_at: 1,
            updated_at: 1,
            title: None,
            permission_mode: ThreadMode::Normal,
            pin_seq: None,
            board_project: None,
            card_id: None,
            model_ref: None,
            reasoning_effort: None,
            preserve_thinking: true,
            role: Some(LEAD_ROLE.to_string()),
        };
        store.create(&lead).unwrap();
        store.append_turn("thread-lead-1", &TurnLine::Turn {
            items: Vec::new(), usage: None, messages: Vec::new(),
        }).unwrap();

        let found = store.find_lead().unwrap().expect("lead must be found");
        assert_eq!(found.thread_id, "thread-lead-1");
        assert_eq!(found.role.as_deref(), Some(LEAD_ROLE));

        // 旧 meta:手工写一份不含 `role` 的 JSON,必须读成 None 且不被 find_lead 选中。
        let raw = serde_json::json!({
            "thread_id": "thread-old-1",
            "cwd": dir.path().display().to_string(),
            "model": "m",
            "created_at": 2,
            "updated_at": 2,
            "title": null,
            "permission_mode": "normal",
            "pin_seq": null,
            "board_project": null,
            "card_id": null,
            "model_ref": null,
            "reasoning_effort": null,
            "preserve_thinking": true
        });
        std::fs::write(
            dir.path().join(".yi-agent/threads/thread-old-1.meta.json"),
            serde_json::to_vec_pretty(&raw).unwrap(),
        )
        .unwrap();
        let loaded = store.load("thread-old-1").unwrap().unwrap();
        assert_eq!(loaded.meta.role, None, "legacy meta must read as a plain thread");
        // 组长仍只有那一个。
        assert_eq!(store.find_lead().unwrap().unwrap().thread_id, "thread-lead-1");
    }

    /// 多个组长并存(理论上的异常态)时 `find_lead` 必须稳定返回最早创建的那个,
    /// 使懒创建幂等:并发两次调用不会在第二次又造一个新组长。
    #[test]
    fn find_lead_is_stable_when_more_than_one_exists() {
        let dir = tempfile::tempdir().unwrap();
        let store = ThreadStore::new(dir.path());
        for (id, created) in [("thread-lead-b", 20i64), ("thread-lead-a", 10i64)] {
            store.create(&ThreadMeta {
                thread_id: id.into(),
                cwd: dir.path().display().to_string(),
                model: "m".into(),
                created_at: created,
                updated_at: created,
                title: None,
                permission_mode: ThreadMode::Normal,
                pin_seq: None,
                board_project: None,
                card_id: None,
                model_ref: None,
                reasoning_effort: None,
                preserve_thinking: true,
                role: Some(LEAD_ROLE.to_string()),
            }).unwrap();
            store.append_turn(id, &TurnLine::Turn {
                items: Vec::new(), usage: None, messages: Vec::new(),
            }).unwrap();
        }
        assert_eq!(store.find_lead().unwrap().unwrap().thread_id, "thread-lead-a");
        assert_eq!(store.find_lead().unwrap().unwrap().thread_id, "thread-lead-a");
    }

    /// 项目键必须是 `<cwd>/.yi-agent` 的 canonical 路径:同一 cwd 的两种写法
    /// (含符号链接)必须得到同一个键,否则会造出两个组长。
    #[test]
    fn project_key_is_canonical_and_stable() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();
        let store = ThreadStore::new(&real);
        assert_eq!(store.project_key(), real.join(".yi-agent").display().to_string());
        assert_eq!(store.project_key(), store.project_key());
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_store::tests::role_round_trips thread_store::tests::find_lead_is_stable thread_store::tests::project_key_is`
Expected: 编译失败，报 `ThreadMeta` 无字段 `role` / 无函数 `find_lead` / 无函数 `project_key`。

- [ ] **Step 3: 实现**

在 `thread_store.rs` 顶部（`use` 之后）加常量：

```rust
/// 组长角色的 `ThreadMeta.role` 取值。一个项目目录至多一个。
pub const LEAD_ROLE: &str = "lead";
```

在 `ThreadMeta` 结构体最后（`preserve_thinking` 之后）加字段：

```rust
    /// 会话角色标记。`Some("lead")` = 该会话是所属项目目录的组长;`None` = 普通会话。
    /// 旧 meta 缺字段一律读成 `None`(零迁移)。
    #[serde(default)]
    pub role: Option<String>,
```

在 `impl ThreadStore` 内加两个方法：

```rust
    /// 组长归属键:项目状态目录 `<cwd>/.yi-agent` 的 canonical 字符串。
    ///
    /// 用 canonical 路径而非 cwd 原串,是因为同一个目录可以有多种写法(尾斜杠、
    /// 符号链接、相对路径),原串比较会让同一项目被判成两个项目、造出两个组长。
    /// canonicalize 失败(目录尚未创建)时退回原串:此时也还没有任何会话,
    /// 键取什么都只会被用一次。
    pub fn project_key(&self) -> String {
        let state_dir = self
            .root
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.root.clone());
        state_dir
            .canonicalize()
            .unwrap_or(state_dir)
            .display()
            .to_string()
    }

    /// 本目录的组长,按 `created_at` 升序取第一个。
    ///
    /// 取「最早创建」而非「最新」,是为了让懒创建幂等:并发/重复调用无论如何
    /// 都收敛到同一个组长,而不是每次挑一个不同的。
    pub fn find_lead(&self) -> io::Result<Option<ThreadMeta>> {
        let mut leads: Vec<ThreadMeta> = self
            .list()?
            .into_iter()
            .filter(|meta| meta.role.as_deref() == Some(LEAD_ROLE))
            .collect();
        leads.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(leads.into_iter().next())
    }
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_store::`
Expected: PASS（含既有用例，一个都不许失败）。

- [ ] **Step 5: 编译整个 crate 找出被新字段打断的构造点**

Run: `cd yi-agent-rs && cargo build -p yi-agent-app-server`
Expected: 报 `ThreadMeta` 初始化缺字段 `role` 的**全部**位置。逐一补 `role: None`，**唯一例外**是 `server.rs:7486` 附近那个新建 meta 的地方，本任务也先补 `role: None`（Task 3 才让它在组长路径上传 `Some(LEAD_ROLE)`）。同时检查 `ThreadMeta` 的其它构造点（搜 `ThreadMeta {`），测试夹具也要补。

- [ ] **Step 6: 再跑一次全 lib 测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: PASS。

- [ ] **Step 7: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add -A
git commit -m "feat(app-server): 给会话加组长角色标记与按目录查找"
```

---

## Task 2: 组长信箱与 `ask_lead` 工具

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/lead.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`（加 `pub mod lead;`）
- Test: `lead.rs` 内 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: Task 1 的 `LEAD_ROLE`、`ThreadStore::find_lead`、`ThreadStore::project_key`
- Produces:
  - `pub struct LeadEnvelope { pub project_key: String, pub from_thread_id: String, pub text: String }`
  - `pub struct LeadInbox` —— `LeadInbox::new() -> Self`、`inbox.deliver(envelope) -> Result<(), DeliverError>`、`inbox.subscribe() -> tokio::sync::broadcast::Receiver<LeadEnvelope>`、`LeadInbox::RECV_LAG_TOLERATED`（文档常量，不需导出为 type）
  - `pub struct AskLeadTool` —— `AskLeadTool::new(inbox: LeadInbox, project_key: String, from_thread_id: String) -> Self`
  - 工具名：`"ask_lead"`，入参 `{ "message": string, "blocking"?: bool }`

- [ ] **Step 1: 写失败测试**

创建 `lead.rs`，只写文档头与测试（实现留到 Step 3）：

```rust
//! 组长通道:把普通会话的消息投给所属项目的组长。
//!
//! 为什么在此处中转而不直连子 agent runtime:`ThreadRoot` 的 attach key 是
//! `thread:<id>`,daemon 的 `is_descendant_of` 只认自身子树,两个会话的 root
//! 互相不可达(测试 `two_conversation_roots_do_not_share_children_or_reach_across`
//! 刻意钉住了这条不变量)。给会话开一张指向组长 root 的 capability 会破坏它,
//! 因此 thread → 组长的通信一律经 app-server 中转:app-server 本就持有
//! 「thread → 项目 → 组长」的映射。
//!
//! P1 只实现单向投递(`blocking: false`)。阻塞往返的形状见 spec §5.2,属 P2。

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn delivering_reaches_a_subscriber_with_the_sender_identity() {
        let inbox = LeadInbox::new();
        let mut rx = inbox.subscribe();
        let tool = AskLeadTool::new(inbox.clone(), "proj-A".into(), "thread-1".into());

        let result = tool.call(json!({"message": "  需要你裁决 X  "})).await;

        let envelope = rx.try_recv().expect("envelope must be delivered");
        assert_eq!(envelope.project_key, "proj-A");
        assert_eq!(envelope.from_thread_id, "thread-1");
        assert_eq!(envelope.text, "需要你裁决 X", "文本必须 trim");
        assert!(format!("{result:?}").contains("已投递") || format!("{result:?}").contains("delivered"));
    }

    #[tokio::test]
    async fn an_empty_message_is_rejected_and_delivers_nothing() {
        let inbox = LeadInbox::new();
        let mut rx = inbox.subscribe();
        let tool = AskLeadTool::new(inbox.clone(), "proj-A".into(), "thread-1".into());

        let result = tool.call(json!({"message": "   "})).await;

        assert!(rx.try_recv().is_err(), "空消息不得投递");
        // 返回的是错误而非 panic;具体文案由实现决定,只要求它不成功。
        assert!(!format!("{result:?}").contains("已投递"));
    }

    #[tokio::test]
    async fn blocking_is_refused_in_phase_one() {
        let inbox = LeadInbox::new();
        let mut rx = inbox.subscribe();
        let tool = AskLeadTool::new(inbox.clone(), "proj-A".into(), "thread-1".into());

        let result = tool.call(json!({"message": "hi", "blocking": true})).await;

        assert!(rx.try_recv().is_err(), "P1 不得把阻塞请求静默降级为投递");
        let text = format!("{result:?}");
        assert!(text.contains("not implemented") || text.contains("P1"), "got {text}");
    }

    /// 没有订阅者时投递不得报错:组长可能尚未被拉起,消息先落邮箱。
    #[tokio::test]
    async fn delivering_with_no_subscriber_is_not_an_error() {
        let inbox = LeadInbox::new();
        let tool = AskLeadTool::new(inbox.clone(), "proj-A".into(), "thread-1".into());
        let result = tool.call(json!({"message": "hi"})).await;
        assert!(format!("{result:?}").contains("已投递") || format!("{result:?}").contains("delivered"));
    }

    /// 工具必须声明为只读(它不改工作区),否则沙箱读数会误导权限层。
    #[test]
    fn ask_lead_is_read_only() {
        let tool = AskLeadTool::new(LeadInbox::new(), "p".into(), "t".into());
        assert!(tool.metadata().read_only);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead::`
Expected: 编译失败，报 `LeadInbox` / `AskLeadTool` 未定义。

- [ ] **Step 3: 实现**

在 `lead.rs` 的文档头之后、`mod tests` 之前插入：

```rust
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;
use tokio::sync::broadcast;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};

/// 一条投给组长的消息。
///
/// `project_key` 在投递侧就算好并随消息携带:主循环收到后据此把消息路由给
/// 该项目的组长 thread,不必重新反推发送方属于哪个项目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeadEnvelope {
    pub project_key: String,
    pub from_thread_id: String,
    pub text: String,
}

/// 每项目一份的组长信箱。
///
/// 用 `broadcast` 而非 `mpsc`:主循环是唯一消费者,但后者在组长 thread 重启
/// (driver 换新)时需要重建通道、丢掉已在路上的消息;`broadcast` 让订阅者
/// 随到随取,发送侧无须知道谁在听。容量 64 与 `theme_tool` / `git_diff_tool`
/// 的同类通道一致。
#[derive(Clone)]
pub struct LeadInbox {
    tx: broadcast::Sender<LeadEnvelope>,
}

impl LeadInbox {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(64);
        Self { tx }
    }

    /// 投递一条消息。没有订阅者时返回 `Ok`:组长尚未被拉起不是错误,主循环
    /// 稍后会以该项目的组长 thread 为消费者订阅。
    pub fn deliver(&self, envelope: LeadEnvelope) -> Result<(), String> {
        // `send` 只在没有任何接收者时报错;把订阅者数当容量判断是错的,
        // 故这里忽略 Err 的具体原因,只区分「有没有人听」。
        match self.tx.send(envelope.clone()) {
            Ok(_) => Ok(()),
            Err(_) => {
                // 无人订阅:留待主循环订阅后仍能看到的是「后续」消息,
                // 因此这里退化为无操作成功,并在调用侧记录。
                tracing::info!(
                    project_key = %envelope.project_key,
                    from = %envelope.from_thread_id,
                    "lead inbox has no subscriber yet; message dropped"
                );
                Ok(())
            }
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LeadEnvelope> {
        self.tx.subscribe()
    }
}

impl Default for LeadInbox {
    fn default() -> Self {
        Self::new()
    }
}

/// 普通会话用来给组长留言的工具。
///
/// P1 只做单向投递。`blocking: true` 显式拒绝而非降级——静默降级会让模型
/// 以为自己在等一个答复,而 P1 根本不会回信,那比报错更难排查。
pub struct AskLeadTool {
    inbox: LeadInbox,
    project_key: String,
    from_thread_id: String,
}

#[derive(Debug, Deserialize)]
struct AskLeadArgs {
    message: String,
    #[serde(default)]
    blocking: bool,
}

impl AskLeadTool {
    pub fn new(inbox: LeadInbox, project_key: String, from_thread_id: String) -> Self {
        Self { inbox, project_key, from_thread_id }
    }
}

#[async_trait::async_trait]
impl Tool for AskLeadTool {
    fn name(&self) -> &str {
        "ask_lead"
    }

    fn description(&self) -> &str {
        "Send a message to this project's lead agent. The lead coordinates all threads in this project. Delivery is one-way in this version: you will not receive a reply during this turn, so keep working after calling it. Use it to report a blocker, hand over a decision, or flag something the lead must know."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "What the lead should know. Be specific and self-contained; the lead may not have read this conversation."
                },
                "blocking": {
                    "type": "boolean",
                    "description": "Reserved. Waiting for a reply is not implemented in this version; do not set it."
                }
            },
            "required": ["message"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let parsed: AskLeadArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(format!("invalid arguments: {e}")),
        };
        if parsed.blocking {
            return ToolResult::error(
                "ask_lead blocking mode is not implemented in P1; \
                 resend without `blocking` so the message is delivered one-way",
            );
        }
        let text = parsed.message.trim().to_string();
        if text.is_empty() {
            return ToolResult::error("message must not be empty");
        }
        let envelope = LeadEnvelope {
            project_key: self.project_key.clone(),
            from_thread_id: self.from_thread_id.clone(),
            text: text.clone(),
        };
        match self.inbox.deliver(envelope) {
            Ok(()) => ToolResult::text(format!(
                "已投递给你所在项目的组长 ({})。本条为单向投递,本轮不会收到回复;请继续手上的工作。",
                self.project_key
            )),
            Err(e) => ToolResult::error(format!("failed to deliver to lead: {e}")),
        }
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            source: ToolSource::Plugin { name: "lead".to_string() },
            requires_confirmation: false,
            read_only: true,
            version: None,
        }
    }
}
```

在 `lib.rs` 的 `pub mod` 列表里按字母序插入：

```rust
pub mod lead;
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead::`
Expected: PASS（5 个用例）。

- [ ] **Step 5: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add -A
git commit -m "feat(app-server): 组长信箱与 ask_lead 工具(单向投递)"
```

---

## Task 3: 组长懒创建与投递接线

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lead.rs`（新增 `ensure_lead`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
  - `thread/start` 分支（约 `:3485`）之后新增 `thread/lead` 分支
  - `thread/listAll` 分支（约 `:3388`）内触发懒创建
  - 主循环启动处（约 `:2280`）新增组长信箱 drain 任务
- Test: `server.rs` `mod tests`

**Interfaces:**
- Consumes: Task 1 的 `find_lead` / `project_key` / `LEAD_ROLE`；Task 2 的 `LeadInbox` / `LeadEnvelope`
- Produces:
  - `pub async fn ensure_lead(store: &ThreadStore, build: impl FnOnce(&Path) -> anyhow::Result<...>)` —— 见下方实现说明；简化后签名固定为 `ensure_lead_meta(store: &ThreadStore) -> io::Result<ThreadMeta>`，只负责落盘 meta，**不建 agent**（agent 由既有 `thread/resume` 路径按需拉起，避免在 `listAll` 里做重活）
  - JSON-RPC 新方法：`"thread/lead"` → `{ "thread_id": string, "created": bool }`
  - `thread/listAll` 每个 group 的条目新增 `role` 字段（Task 4 才在 wire 上暴露；本任务先保证组长条目**出现在列表里**）

**关键设计约束**：`listAll` 是列表查询，**不得**在它里面创建 agent 或起 driver。懒创建只写 meta（一个 `ThreadStore::create` 调用），代价与一次 `touch` 同量级；agent 在用户真正打开组长对话（`thread/resume`）时才建。

- [ ] **Step 1: 写失败测试**

在 `server.rs` 的 `mod tests` 末尾追加。

**必读**：`Harness::new()` 的 workdir 是**固定的共享路径**
`/tmp/yi-agent-app-server-test`（`tests_support::test_config`），三个测试都用它会互
相污染、且会往真实路径写 meta。必须照 `:8163` 一带既有持久化测试的写法，用
`tempfile::TempDir` + `Harness::with_cfg` 做隔离：

```rust
    /// 建一个 workdir 隔离的 harness,返回 (harness, 该 workdir 的持有者)。
    /// 持有者必须在测试结束前存活,否则 tempdir 被删、meta 无处可写。
    async fn isolated_harness() -> (Harness, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = tests_support::test_config();
        cfg.workdir = dir.path().to_path_buf();
        (Harness::with_cfg(cfg).await, dir)
    }

    /// `thread/lead` 必须幂等:同一 cwd 反复调用只造一个组长,第二次 `created=false`。
    #[tokio::test(flavor = "multi_thread")]
    async fn thread_lead_is_idempotent_per_project() {
        let (mut h, _dir) = isolated_harness().await;
        h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#).await;
        let _ = read_response(&mut h, 1).await;

        let first = rpc(&mut h, 2, "thread/lead", serde_json::json!({})).await;
        let first_id = first["result"]["thread_id"].as_str().expect("id").to_string();
        assert_eq!(first["result"]["created"], serde_json::json!(true));

        let second = rpc(&mut h, 3, "thread/lead", serde_json::json!({})).await;
        assert_eq!(second["result"]["thread_id"].as_str().unwrap(), first_id);
        assert_eq!(second["result"]["created"], serde_json::json!(false));

        h.shutdown().await;
    }

    /// 组长必须出现在 `thread/listAll` 的目录分组里,否则侧栏点不进去。
    #[tokio::test]
    async fn the_lead_appears_in_list_all() {
        let (mut h, _dir) = isolated_harness().await;
        h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#).await;
        let _ = read_response(&mut h, 1).await;
        let lead = rpc(&mut h, 2, "thread/lead", serde_json::json!({})).await;
        let lead_id = lead["result"]["thread_id"].as_str().unwrap().to_string();

        let all = list_all(&mut h).await;
        let found = all["result"]["groups"]
            .as_array()
            .expect("groups")
            .iter()
            .flat_map(|g| g["threads"].as_array().cloned().unwrap_or_default())
            .any(|t| t["thread_id"].as_str() == Some(lead_id.as_str()));
        assert!(found, "lead thread must be listed: {all}");

        h.shutdown().await;
    }

    /// 投递到组长信箱的消息必须转成组长 thread 的一次输入;组长尚未被打开时
    /// 也必须能被拉起(driver 按需建)。
    #[tokio::test]
    async fn a_delivered_message_reaches_the_lead_thread() {
        let (mut h, _dir) = isolated_harness().await;
        h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#).await;
        let _ = read_response(&mut h, 1).await;
        let lead = rpc(&mut h, 2, "thread/lead", serde_json::json!({})).await;
        let lead_id = lead["result"]["thread_id"].as_str().unwrap().to_string();

        // 从这里开始是 Task 3 的实现细节断言:用测试专用入口直接投递,
        // 避免依赖真实模型。入口名固定为 `deliver_lead_message_for_test`。
        deliver_lead_message_for_test(&mut h, _dir.path(), "thread-sender-1", "需要你裁决 X").await;

        // 组长 thread 的 `.jsonl` 必须出现那条消息(以用户消息形式注入)。
        let deadline = std::time::Instant::now() + EVENT_WAIT;
        loop {
            // `_dir` 持有隔离 workdir;它就在本测试的栈上,直接复用它。
            let store = crate::thread_store::ThreadStore::new(_dir.path());
            if let Ok(Some(loaded)) = store.load(&lead_id) {
                if loaded.messages.iter().any(|m| {
                    m.content.iter().any(|b| match b {
                        yi_agent_core::ContentBlock::Text(t) => t.contains("需要你裁决 X"),
                        _ => false,
                    })
                }) {
                    break;
                }
            }
            if std::time::Instant::now() > deadline {
                panic!("the delivered message never reached the lead thread");
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        h.shutdown().await;
    }
```

**注意**：上面第三个测试只要求一个测试专用设施 `deliver_lead_message_for_test`
（`Harness` 上的 `async fn deliver_lead_message_for_test(h: &mut Harness, workdir: &Path, from_thread_id: &str, text: &str)`，
见 (d)-5）。**不要**给 `Harness`
新增 `workdir()` 访问器——隔离 workdir 由测试自己经 `isolated_harness()` 持有，不需要从
harness 反查。

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_lead_is_idempotent a_delivered_message_reaches the_lead_appears_in_list_all`
Expected: 编译失败（`thread/lead` 方法不存在、`deliver_lead_message_for_test` 未定义）。

- [ ] **Step 3: 实现**

**(a) `lead.rs` 增加懒创建**：

```rust
use std::path::Path;

use crate::thread_store::{LEAD_ROLE, ThreadMeta, ThreadStore};

/// 确保本目录存在组长 meta,返回它。
///
/// 只写 meta,不建 agent、不起 driver:调用点之一是 `thread/listAll`,那是纯
/// 列表查询,在上面做重活会让侧栏每次刷新都付一次装配成本。真正的 agent 由
/// 用户打开组长对话时的 `thread/resume` 建。
///
/// `created` 区分本次调用是否真的建了:`ThreadMeta` 不带该信息,故一并返回,
/// 让 `thread/lead` 能如实回答调用方。
pub fn ensure_lead_meta(store: &ThreadStore, cwd: &Path) -> std::io::Result<(ThreadMeta, bool)> {
    if let Some(existing) = store.find_lead()? {
        return Ok((existing, false));
    }
    // 竞态说明:两个请求同时走到这里会各造一个 meta,但它们是**同一个**
    // thread_id 之外的两种可能 id,因此会短暂出现两个组长。`find_lead` 取
    // created_at 最早者,故此后所有读取都收敛到同一个;`thread/lead` 的响应
    // 也以 find_lead 的结果为准(见 server 的实现),使调用方观察不到分歧。
    let now = chrono::Utc::now().timestamp_millis();
    let meta = ThreadMeta {
        thread_id: format!("thread-lead-{}", uuid::Uuid::new_v4()),
        cwd: cwd.display().to_string(),
        model: String::new(), // 空 = 跟随全局默认,与「损坏重建」同款兜底
        created_at: now,
        updated_at: now,
        title: Some("组长".to_string()),
        permission_mode: crate::thread_store::ThreadMode::Normal,
        pin_seq: None,
        board_project: None,
        card_id: None,
        model_ref: None,
        reasoning_effort: None,
        preserve_thinking: true,
        role: Some(LEAD_ROLE.to_string()),
    };
    store.create(&meta)?;
    Ok((meta, true))
}
```

**(b) `server.rs` 新增 `thread/lead` 分支**（放在 `"thread/start" => { ... }` 分支**之后**、同级）：

```rust
                    "thread/lead" => {
                        // 组长归属当前请求的项目根。与 `thread/start` 同源,保证
                        // 「用户在哪个项目里问组长」与「组长管哪个项目」一致。
                        let cwd = match resolve_thread_cwd(&req.params, &cfg, &hub, &client, id.clone()).await? {
                            Some(c) => c,
                            None => continue,
                        };
                        let store = crate::thread_store::ThreadStore::new(Path::new(&cwd));
                        match crate::lead::ensure_lead_meta(&store, Path::new(&cwd)) {
                            Ok((meta, created)) => {
                                write_response(
                                    &hub,
                                    &client,
                                    ok_response(id, json!({
                                        "thread_id": meta.thread_id,
                                        "created": created,
                                    })),
                                )
                                .await?;
                            }
                            Err(e) => {
                                write_response(&hub, &client, err_response(id, RpcError::internal(e.to_string()))).await?;
                            }
                        }
                    }
```

**(c) `thread/listAll` 触发懒创建**：在它遍历 `dirs` 的循环体内、为每个存在的目录建组之前插入。**必须容错**：列表查询不能因为一个目录写不进 meta 而整体失败。

```rust
                        // 每个被列举的项目目录都确保有组长:侧栏的分组头要用到它。
                        // 失败只记日志——列表查询不得因单个目录不可写而整体失败。
                        for dir in &dirs {
                            if !Path::new(dir).is_dir() {
                                continue;
                            }
                            let store = crate::thread_store::ThreadStore::new(Path::new(dir));
                            if let Err(e) = crate::lead::ensure_lead_meta(&store, Path::new(dir)) {
                                eprintln!("[app-server] thread/listAll could not ensure a lead for {dir}: {e}");
                            }
                        }
```

**(d) 主循环接线（无帧开 thread + 投递 prompt，照抄 `ServeLauncher`）**

**这是本任务最需要照抄现成模式的地方**，不要自己发明。仓库里已有「从主循环开一个
thread 并送首条 prompt、且不发任何 RPC 帧」的既定实现：`ServeLauncher::launch_inner`
（`server.rs:7232` 起）。组长投递走同一套内核。

架构上有一个**硬约束**必须先理解：`threads` 是主循环独占的 `&mut HashMap`
（`server.rs:2247`），所以信箱的消费**不能**放进 `tokio::spawn`（那需要 `'static`
或共享可变状态）。正确做法是在主循环的 `tokio::select!` 里加一条分支，并把「开
thread + 投递」实现为 `ServeLauncher` 上的一个方法——它已经借好了 `threads` /
`pending_activation` / `process_watches` / `thread_roots` 等全部局部量。

因此本任务**不新增** `pump_lead_inbox` 函数，而是：

1. 创建信箱并把它接进 `ServeLauncher`：

```rust
    // 组长信箱:投递 → 组长 thread 的一次输入。进程内多项目各走自己的
    // project_key,故一条通道即可。
    let lead_inbox = crate::lead::LeadInbox::new();
    let mut lead_rx = lead_inbox.subscribe();
```

2. 给 `ServeLauncher` 加字段（与既有字段同层）：

```rust
    /// 组长信箱:普通会话经 `ask_lead` 投递的消息从这里被消费。
    lead_inbox: &'a crate::lead::LeadInbox,
    /// 该 launcher 对应项目的组长 thread id 与 cwd,由消费方在调用前解析。
    /// 用 owned 值而非从 `threads` 反查:组长可能尚未被打开,不在 `threads` 里。
    lead_target: LeadTarget,
```

其中：

```rust
/// 一次组长投递的目标:该项目当前应承载消息的组长 thread。
struct LeadTarget {
    project_key: String,
    cwd: String,
    thread_id: String,
}
```

3. 在 `ServeLauncher` 上加方法（形状与 `launch_inner` 一致，**照它的顺序写**）：

```rust
    /// 把一条投递给组长的消息转成组长 thread 的一次 turn。
    ///
    /// 与 `launch_inner` 的三点差异,每一点都是刻意的:
    /// - 复用**已存在的**组长 thread id(不新铸 uuid),因此重复投递落在同一个会话;
    /// - `mode` 用该会话落盘的 `permission_mode`,而非看板的 Yolo 强制值;
    /// - 不设 `board_project` / `card_id`,组长不是看板卡。
    async fn deliver_to_lead(&mut self, envelope: &crate::lead::LeadEnvelope) -> anyhow::Result<String> {
        let target = &self.lead_target;
        let built = (self.build_agent)(
            None,
            Path::new(&target.cwd),
            crate::thread_store::ThreadMode::Normal,
        )?;
        let model = built.agent.config().model.clone();

        // 组长已在本进程内打开时,直接往它已有的 driver 投递,不必重建。
        if let Some(session) = self.threads.get(&target.thread_id) {
            let turn_id = format!("turn-{}", uuid::Uuid::new_v4());
            let text = format!("[来自会话 {}] {}", envelope.from_thread_id, envelope.text);
            session
                .prompt_tx
                .send(TurnPrompt {
                    turn_id,
                    prompt: text.clone(),
                    activate: self
                        .pending_activation
                        .get(&target.thread_id)
                        .cloned()
                        .flatten(),
                    attachments: Vec::new(),
                    image_blocks: Vec::new(),
                    image_refs: Vec::new(),
                    display_text: text,
                })
                .await
                .map_err(|_| anyhow::anyhow!("lead driver is gone"))?;
            return Ok(target.thread_id.clone());
        }

        // 冷启动:走与 `launch_inner` 相同的无帧内核。
        let hub = Arc::clone(self.hub);
        let client = crate::broadcast::ClientId::local();
        start_thread_core(
            self.threads,
            self.pending_activation,
            self.process_watches,
            self.runtimes,
            self.thread_roots,
            self.models_path,
            self.cfg,
            &target.cwd,
            crate::thread_store::ThreadMode::Normal,
            target.thread_id.clone(),
            built,
            &hub,
            &client,
            self.turn_tx,
            self.pending,
            self.permission_timeout,
            self.perm_seq,
            self.theme,
            self.git_diff,
            self.workspaces,
            model,
            None,
            None,
            None,
        )
        .await?;

        let params = json!({
            "threadId": target.thread_id,
            "input": [{ "type": "text",
                        "text": format!("[来自会话 {}] {}", envelope.from_thread_id, envelope.text) }],
        });
        let prepared = prepare_turn_core(self.threads, self.pending_activation, self.uploads, &params).await?;
        prepared
            .prompt_tx
            .send(TurnPrompt {
                turn_id: prepared.turn_id,
                prompt: prepared.prompt,
                activate: prepared.activate,
                attachments: prepared.attachments,
                image_blocks: prepared.image_blocks,
                image_refs: prepared.image_refs,
                display_text: prepared.display_text,
            })
            .await
            .map_err(|_| anyhow::anyhow!("lead driver is gone before its first prompt"))?;
        Ok(target.thread_id.clone())
    }
```

**注意 `start_thread_core` 会 `create` meta**（`server.rs:7483` 附近）。组长 meta 在
Task 3(a) 已由 `ensure_lead_meta` 建好，此处第二次 `create` 会**覆盖**它——因此必须
把 `start_thread_core` 里的 `if let Err(e) = thread_store.create(&meta)` 改为
**先探测存在**：

```rust
    // 组长 meta 可能已由 `ensure_lead_meta` 落盘:此时不得覆盖,否则会把它
    // 的 `role: "lead"` 抹掉(本函数的 meta 不带 role)。
    let already = thread_store
        .load(&thread_id)
        .ok()
        .flatten()
        .is_some();
    if !already {
        if let Err(e) = thread_store.create(&meta) {
            eprintln!("[app-server] failed to create thread meta for {thread_id}: {e}");
        }
    }
```

4. 主循环 `select!` 新增分支（放在既有的 `card_ticker.tick()` 分支旁）：

```rust
            envelope = lead_rx.recv() => {
                let Ok(envelope) = envelope else { continue };
                // 按 project_key 找到该项目的组长。找不到就跳过并记日志:
                // 一条投递失效不该影响主循环处理其它请求。
                let target = match resolve_lead_target(&envelope.project_key, &cfg) {
                    Ok(target) => target,
                    Err(error) => {
                        eprintln!("[app-server] could not resolve a lead for {}: {error}", envelope.project_key);
                        continue;
                    }
                };
                let mut launcher = ServeLauncher {
                    threads,
                    pending_activation,
                    process_watches,
                    uploads,
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
                    lead_target: target,
                };
                if let Err(error) = launcher.deliver_to_lead(&envelope).await {
                    eprintln!("[app-server] lead delivery failed: {error}");
                }
            }
```

`resolve_lead_target` 是 `server.rs` 内的自由函数：

```rust
/// 由项目键反推该项目当前应承载消息的组长。
///
/// 项目键本身就是 `<cwd>/.yi-agent` 的 canonical 路径(见 `ThreadStore::project_key`),
/// 故 cwd 可由它去掉末段得到。组长 meta 不存在时当场补建——用户可能直接让某个
/// 会话给组长留言,而从未打开过组长对话。
fn resolve_lead_target(project_key: &str, cfg: &RuntimeConfig) -> anyhow::Result<LeadTarget> {
    let state_dir = Path::new(project_key);
    let cwd = state_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("project key has no parent: {project_key}"))?;
    let store = crate::thread_store::ThreadStore::new(cwd);
    let (meta, _created) = crate::lead::ensure_lead_meta(&store, cwd)?;
    Ok(LeadTarget {
        project_key: project_key.to_string(),
        cwd: cwd.display().to_string(),
        thread_id: meta.thread_id,
    })
}
```

**借用检查提示**：主循环里 `threads` 等局部量此刻不再被别的分支借用（`select!` 一次只
跑一个分支），因此照 `card_ticker.tick()` 分支里构造 `ServeLauncher` 的写法即可。

5. 测试专用入口：

```rust
    /// 测试专用:直接往该 harness 的组长信箱投递,不经过真实模型。
    ///
    /// 需要一个「与主循环共享的信箱句柄」——因此 `Harness` 必须持有它(见下)。
    /// `project_key` 由调用方给出的隔离 workdir 求得,不从 `Harness` 反查。
    pub(crate) async fn deliver_lead_message_for_test(
        h: &mut Harness,
        workdir: &std::path::Path,
        from_thread_id: &str,
        text: &str,
    ) {
        h.lead_inbox
            .deliver(crate::lead::LeadEnvelope {
                project_key: crate::thread_store::ThreadStore::new(workdir).project_key(),
                from_thread_id: from_thread_id.to_string(),
                text: text.to_string(),
            })
            .expect("deliver");
    }
```

`Harness` 需要新增字段 `lead_inbox: crate::lead::LeadInbox`（在 `with_config` 里建一份，
并把同一个 clone 传进 `serve` 的主循环初始化；按既有 `theme` / `git_diff` 句柄的注入
方式对齐，**不要**新造一套注入机制）。

因此第三个测试的调用点要补 workdir 参数，且 `_dir` 必须仍在该测试作用域内：

```rust
        deliver_lead_message_for_test(&mut h, _dir.path(), "thread-sender-1", "需要你裁决 X").await;
```

**(e) `build_runtime_tooling` 注册 `ask_lead`**：在 `register_attached_root_tools_in_thread` 调用（约 `:861`）之后：

```rust
    // 普通会话用它给组长留言。`from_thread_id` 绑在装配期,与
    // `register_attached_root_tools_in_thread` 的 thread marker 同理由:
    // 模型看不到自己的 thread id,只能由装配期注入。
    registry.register(Arc::new(crate::lead::AskLeadTool::new(
        lead_inbox.clone(),
        project_key.clone(),
        thread_id.to_string(),
    )));
```

为此 `build_runtime_tooling` 需要新增两个参数：`lead_inbox: crate::lead::LeadInbox` 与 `project_key: String`。调用点在 `:330` 与测试的 `:9614` / `:9691` / `:9740` / `:9795`，全部要补。`project_key` 由调用方从 `ThreadStore::new(Path::new(cwd)).project_key()` 求出。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: PASS。若第三个测试因「组长 driver 未建」而超时，检查是否真的走了 resume 路径（而不是只写了 meta）；这是本任务最容易做错的地方。

- [ ] **Step 5: 验证不变量未被破坏**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc two_conversation_roots_do_not_share_children_or_reach_across ending_one_conversation_reclaims_only_its_own_root`
Expected: PASS（本任务不得改动 daemon 隔离语义）。
若该环境无法创建 Unix socket，记录为「未在本机验证」并在 PR 描述中写明，不要谎报通过。

- [ ] **Step 6: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add -A
git commit -m "feat(app-server): 组长懒创建与信箱投递接线"
```

---

## Task 4: wire 暴露 `role` 与桌面端组长入口

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`thread_summary_json`，约 `:5943`；`thread/lead` 已存在，本任务只补列表字段）
- Modify: `desktop/src/lib/protocol.ts:255-277`
- Create: `desktop/src/lib/leadThread.ts`
- Modify: `desktop/src/components/ThreadSidebar.tsx`
- Test: `desktop/src/lib/leadThread.test.ts`（新）、`desktop/src/components/ThreadSidebar.test.tsx`、`yi-agent-rs/crates/yi-agent-app-server/src/server.rs` 的 `mod tests`

**Interfaces:**
- Consumes: Task 1 的 `ThreadMeta.role`；Task 3 的 `thread/lead`
- Produces:
  - wire：`thread/list`、`thread/listAll` 的每个条目新增 `"role"?: "lead"`（`None` 时**不发送**该 key，保持旧客户端字节兼容）
  - `desktop/src/lib/leadThread.ts`：`export function findLeadThread(threads: ThreadSummary[]): ThreadSummary | null`
  - `ThreadSidebar` 把组长渲染在列表顶部固定分区

- [ ] **Step 1: 写失败的 Rust 测试**

在 `server.rs` 的 `mod tests` 末尾追加：

```rust
    /// `role` 必须在列表条目上暴露;普通会话不得带该 key(旧客户端字节兼容)。
    #[tokio::test]
    async fn list_all_exposes_the_lead_role_only_for_the_lead() {
        let (mut h, _dir) = isolated_harness().await;
        h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#).await;
        let _ = read_response(&mut h, 1).await;
        let start = start_thread(&mut h).await;
        let lead = rpc(&mut h, 5, "thread/lead", serde_json::json!({})).await;
        let lead_id = lead["result"]["thread_id"].as_str().unwrap().to_string();

        let all = list_all(&mut h).await;
        // 朴素收集:`.leak()` 会永久泄漏内存,只为借出一个 `&Value`;克隆更清楚。
        let entries: Vec<serde_json::Value> = all["result"]["groups"]
            .as_array()
            .expect("groups")
            .iter()
            .flat_map(|g| g["threads"].as_array().cloned().unwrap_or_default())
            .collect();

        let lead_entry = entries.iter().find(|t| t["thread_id"].as_str() == Some(lead_id.as_str())).expect("lead entry");
        assert_eq!(lead_entry["role"], serde_json::json!("lead"));

        let plain_entry = entries.iter().find(|t| t["thread_id"].as_str() == Some(start.as_str())).expect("plain entry");
        assert!(
            plain_entry.get("role").is_none(),
            "a plain thread must not carry a role key: {plain_entry}"
        );

        h.shutdown().await;
    }
```

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib list_all_exposes_the_lead_role`
Expected: FAIL（`role` 为 `null` 或缺失）。

- [ ] **Step 3: 实现 Rust 侧**

`thread_summary_json` 改为：

```rust
fn thread_summary_json(
    m: &crate::thread_store::ThreadMeta,
    threads: &HashMap<String, ThreadSession>,
) -> serde_json::Value {
    let mut value = json!({
        "thread_id": m.thread_id,
        "cwd": m.cwd,
        "model": m.model,
        "model_ref": m.model_ref,
        "created_at": m.created_at,
        "updated_at": m.updated_at,
        "title": m.title,
        "permission_mode": m.permission_mode,
        "pinned": m.pin_seq.is_some(),
        "board_project": m.board_project,
        "card_id": m.card_id,
        "status": thread_status(threads, &m.thread_id),
    });
    // 只在确实是组长时发送该 key:普通会话多一个 `"role": null` 会让旧客户端
    // 的字节比对与快照测试失效,而我们只需要一个新值。
    if m.role.as_deref() == Some(crate::thread_store::LEAD_ROLE) {
        value["role"] = json!("lead");
    }
    value
}
```

- [ ] **Step 4: 运行确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: PASS。

- [ ] **Step 5: 写失败的前端测试**

创建 `desktop/src/lib/leadThread.test.ts`：

```typescript
import { describe, expect, it } from "vitest";
import { findLeadThread } from "./leadThread";
import type { ThreadSummary } from "./protocol";

function thread(over: Partial<ThreadSummary>): ThreadSummary {
  return {
    thread_id: "t1",
    cwd: "/p",
    model: "m",
    created_at: 1,
    updated_at: 1,
    title: null,
    ...over,
  };
}

describe("findLeadThread", () => {
  it("returns the lead thread when one is present", () => {
    const lead = thread({ thread_id: "lead-1", role: "lead" });
    expect(findLeadThread([thread({ thread_id: "a" }), lead])?.thread_id).toBe("lead-1");
  });

  it("returns null when no thread carries the lead role", () => {
    expect(findLeadThread([thread({})])).toBeNull();
  });

  it("returns null on an empty list", () => {
    expect(findLeadThread([])).toBeNull();
  });

  it("ignores an unknown role value (旧服务端/未来取值)", () => {
    expect(findLeadThread([thread({ role: "auditor" as never })])).toBeNull();
  });

  it("returns the earliest created lead when more than one exists", () => {
    const older = thread({ thread_id: "lead-old", role: "lead", created_at: 10 });
    const newer = thread({ thread_id: "lead-new", role: "lead", created_at: 20 });
    expect(findLeadThread([newer, older])?.thread_id).toBe("lead-old");
  });
});
```

- [ ] **Step 6: 运行确认失败**

Run: `cd desktop && npx vitest run src/lib/leadThread.test.ts`
Expected: FAIL（模块不存在）。

- [ ] **Step 7: 实现前端纯函数**

创建 `desktop/src/lib/leadThread.ts`：

```typescript
import type { ThreadSummary } from "./protocol";

/**
 * Pick this project's lead thread out of a flat thread list.
 *
 * Mirrors the server rule (`ThreadStore::find_lead`): the lead is the
 * `role === "lead"` thread with the smallest `created_at`, so a transient
 * duplicate converges on the same answer here and on the server. An unknown
 * `role` value is ignored rather than treated as a lead — a future role must
 * not silently take over the sidebar's lead slot.
 */
export function findLeadThread(threads: ThreadSummary[]): ThreadSummary | null {
  let best: ThreadSummary | null = null;
  for (const thread of threads) {
    if (thread.role !== "lead") continue;
    if (best === null || thread.created_at < best.created_at) best = thread;
  }
  return best;
}
```

在 `protocol.ts` 的 `ThreadSummary` 末尾（`model_ref` 之后）加：

```typescript
  /**
   * 会话角色。`"lead"` = 该会话是所属项目的组长。缺省 = 普通会话
   * （旧服务端不发送此字段）。未知取值必须忽略，不得当成组长。
   */
  role?: string;
```

- [ ] **Step 8: 运行确认通过**

Run: `cd desktop && npx vitest run src/lib/leadThread.test.ts`
Expected: PASS（5 个用例）。

- [ ] **Step 9: 侧栏渲染组长分区**

在 `ThreadSidebar.tsx` 中：把 `findLeadThread` 用在传入的当前项目 thread 列表上，若命中则在该组的 `Pinned` 分区**之前**渲染一个组长条目（固定头像 + 标题「组长」）。测试追加到 `ThreadSidebar.test.tsx`：

```typescript
  it("renders the lead entry above the pinned section", () => {
    const lead: ThreadSummary = {
      thread_id: "lead-1", cwd: "/p", model: "m", created_at: 1, updated_at: 9,
      title: "组长", role: "lead",
    };
    const pinned: ThreadSummary = {
      thread_id: "pin-1", cwd: "/p", model: "m", created_at: 2, updated_at: 8,
      title: "pinned", pinned: true,
    };
    render(<ThreadSidebar /* 该测试的既有必填 props */ threads={[pinned, lead]} />);
    const leadRow = screen.getByText("组长");
    const pinnedRow = screen.getByText("pinned");
    expect(leadRow.compareDocumentPosition(pinnedRow) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });
```

**注意**：`render(<ThreadSidebar ... />)` 的 props 必须照抄该文件里既有测试的写法（它有多个必填回调）。不要发明 prop 名。

- [ ] **Step 10: 运行前端测试与类型检查**

Run: `cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx && npx tsc --noEmit`
Expected: PASS，且无类型错误。

- [ ] **Step 11: 端到端验证懒创建（手工，一次）**

```bash
cd yi-agent-rs && cargo build -p yi-agent --bin yi-agent
TMP=$(mktemp -d) && cd "$TMP" && git init -q .
# 起 app-server 并问一次组长(用真实二进制,验证 wire 层)
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"thread/lead","params":{}}' \
  | <worktree>/yi-agent-rs/target/debug/yi-agent app-server 2>/dev/null | head -40
ls -1 "$TMP/.yi-agent/threads/"
```

Expected: 输出含 `"created":true` 与一个 `thread-lead-*.meta.json`；再次运行同一命令时该次响应为 `"created":false`，且**不新增** meta 文件（证明幂等）。

- [ ] **Step 12: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add -A
git commit -m "feat: 暴露组长角色并在桌面侧栏给出组长入口"
```

---

## Self-Review

**Spec 覆盖（对照 spec §11 的 P1：lead thread 存在 + 自动归属 + 单向 `ask_lead` + 桌面能点开 lead 并对话）**

| spec 要求 | 落点 |
| --- | --- |
| 组长是一个特殊 thread（决策 9） | Task 1 `role` 字段；Task 3 懒创建 |
| 自动归属、零认领（决策 8） | Task 1 `project_key` + Task 3 `thread/lead` / `listAll` 懒创建 |
| 单向 `ask_lead`（决策 4 的非阻塞半边） | Task 2 |
| 桌面能点开组长并对话（决策 7） | Task 4（点击走既有 `thread/resume`，无需新代码） |
| 通信经 app-server，不动 daemon 隔离（§4） | Task 3 的 `pump_lead_inbox`；Task 3 Step 5 回归验证 |
| 组长不占 `MAX_DIRECT_CHILDREN`（决策 12） | 组长的子任务走它自己的 `ThreadRoot`，与普通 thread 同构，无需额外代码 |

**P1 明确不覆盖**（属 P2/P3，已写入 Global Constraints）：后台循环、事件唤醒、前台抢占、阻塞式 `ask_lead`、`lead_memory`、lead 的列表/转录读取工具、权限留痕、移动端专用 UI。

**已知待决**：Task 3 的 `pump_lead_inbox` 需要「打开一个 thread 并投递首条 turn」的可复用逻辑。若 `start_thread_core` 的借用结构不允许提取，实现者应内联 `thread/resume` 的写法并在 commit message 里说明原因——不要为了提取而重构 `thread/resume`（那是无关改动，会放大回归面）。

**类型一致性核对**：`LeadEnvelope` 字段名（`project_key` / `from_thread_id` / `text`）在 Task 2 定义、Task 3 消费，一致；`LeadInbox::subscribe() -> broadcast::Receiver<LeadEnvelope>` 在两处一致；`ensure_lead_meta(store, cwd) -> io::Result<(ThreadMeta, bool)>` 在 Task 3 定义与使用一致；`findLeadThread(threads) -> ThreadSummary | null` 在 Task 4 定义与测试一致；wire 的 `role` 值 `"lead"` 与 `LEAD_ROLE` 常量一致。