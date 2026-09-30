# 桌面端 Slash 命令实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让桌面 App 输入框支持 `/` 触发的 slash 命令弹窗与执行，功能对齐 TUI 的对应子集（`/help` `/cost` `/model` `/config` `/clear` `/compact`）。

**Architecture:** 六条命令分两类。`/help` `/cost` `/model` 是**纯前端**（数据已在 `Session` 里）；`/config` 走**已有的** `config/read`；`/clear` `/compact` 需要 app-server 新增两个 RPC，且**必须由持有 agent 的 per-thread driver 执行**（`Agent::session()` 只返回 clone，session 的可变访问权在 driver 手里）。前端新增一个命令目录模块 + 弹窗组件，命令输出渲染为不经过 agent 的 `notice` 条目。

**Tech Stack:** Rust 2024 / Tokio（`yi-agent-app-server`）、serde_json；React 19 + TypeScript + Vitest + Testing Library（`desktop/`）。

**设计文档：** `docs/superpowers/specs/2026-10-01-desktop-slash-commands-design.md`

## Global Constraints

- **严禁在 `main` 分支上提交。** 全程在 worktree `.worktrees/feat/desktop-slash-commands`（分支 `feat/desktop-slash-commands`）里工作（执行阶段先建）。
- **提交前必须 `cd yi-agent-rs && cargo fmt --all`**；commit message 用 conventional commits，首行 ≤72 字符，**不要**写 `Co-Authored-By`。
- **不要并行跑 `cargo test`**：同一时刻只跑一个 cargo 命令。跑之前先 `ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent"` 确认没有残留进程。
- **不要跑 `cargo test --workspace`**；按 crate 跑：`cargo test -p yi-agent-app-server`、`cargo test -p yi-agent-core`（如涉及）。
- **TUI 行为零回归**：本计划不改 `yi-agent-rs/crates/yi-agent/`（TUI crate）。
- 命令目录的**单一事实来源**是 `desktop/src/lib/slash.ts`；`/help` 文案必须由它渲染，不得另写一份。
- 错误码复用既有定义，**不新增**：未知 thread = `-32011`，turn 进行中 = `-32012`。
- 前端验证命令统一为 `cd desktop && npx vitest run && npx tsc --noEmit && npm run build`。

---

## 文件结构

**后端（`yi-agent-rs/crates/yi-agent-app-server/src/`）**

| 文件 | 职责 | 动作 |
|---|---|---|
| `session.rs` | `SessionCommand` / `CompactOutcome` 类型 + `ThreadSession.session_tx` | 改 |
| `thread_store.rs` | 新增 `truncate()`：只删 `.jsonl`，保留 `.meta.json` | 改 |
| `server.rs` | driver 的 `session_rx` 分支；`thread/clear`、`thread/compact` 主循环分支；两处 spawn 点传参 | 改 |
| `protocol.rs` | 只读参考，**不改**（新 RPC 无新协议类型） | — |

**前端（`desktop/src/`）**

| 文件 | 职责 | 动作 |
|---|---|---|
| `lib/slash.ts` | 命令目录 + 过滤 + 输入解析 + `/help` 渲染。**唯一事实来源** | 新建 |
| `components/SlashPopup.tsx` | 命令弹窗（纯展示组件） | 新建 |
| `components/MessageInput.tsx` | 弹窗状态机 + 按键路由 + `onSlashCommand` prop | 改 |
| `lib/session.ts` | `NoticeItem` 类型 + `Session.notice()` | 改 |
| `components/ChatView.tsx` | 渲染 `notice` 条目 | 改 |
| `lib/protocol.ts` | 新增两个 RPC 的请求/响应形状（types only，文档用途） | 改 |
| `App.tsx` | 命令分发 `onSlashCommand` + 把 `turnActive` 传给输入框 | 改 |
| `lib/threadStore.ts` | 只读参考（`view(id).info.model`），**不改** | — |

**文档（本仓库根）**

| 文件 | 动作 |
|---|---|
| `docs/project-management/yi-agent-app-server.md` | 新增 2 条 feature + 计数 |
| `docs/project-management/desktop.md` | 新增 1 条 feature + 验证命令计数 |
| `docs/project-management/README.md` | 同步两行计数 |

---

### Task 0: 建 worktree 与工作分支

**Files:** 无（仅 git 操作）

**Interfaces:**
- Consumes: 无
- Produces: 分支 `feat/desktop-slash-commands`，所有后续任务在此提交

- [ ] **Step 1: 建 worktree**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
git worktree add .worktrees/feat/desktop-slash-commands -b feat/desktop-slash-commands
```

预期：`Preparing worktree (new branch 'feat/desktop-slash-commands')`。

- [ ] **Step 2: 确认基线干净**

```bash
cd .worktrees/feat/desktop-slash-commands
git status --short
```

预期：无输出（干净）。后续所有命令都在这个 worktree 根目录执行。

---

### Task 1: `ThreadStore::truncate` —— 只删日志，保留 thread 身份

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`（在 `delete` 之后、`io_err` 之前加方法）
- Test: 同文件 `mod tests`（文件末尾）

**Interfaces:**
- Consumes: 既有 `ThreadStore`（`root: PathBuf`、`meta_lock`、`meta_path(id)`、`log_path(id)`、`valid_id`）
- Produces: `pub fn truncate(&self, id: &str) -> io::Result<bool>` —— 删除 `<id>.jsonl`，保留 `<id>.meta.json`；返回"是否存在过"；id 非法返回 `InvalidInput`

- [ ] **Step 1: 写失败的测试**

在 `thread_store.rs` 的 `mod tests` 里，`append_turn_then_load_returns_items_and_messages` 之后追加：

```rust
    #[test]
    fn truncate_drops_the_log_but_keeps_the_thread_identity() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        s.append_turn(
            "thread-a",
            &turn(
                vec![Item::UserMessage {
                    id: "user-turn-1".into(),
                    text: "hi".into(),
                }],
                vec![Message::user("hi")],
            ),
        )
        .unwrap();

        let existed = s.truncate("thread-a").unwrap();
        assert!(existed, "truncate must report the thread existed");
        // 身份仍在（meta 保留 → list 仍能列出该 thread）。
        assert!(s.exists("thread-a"), "meta must survive truncate");
        assert_eq!(s.list().unwrap().len(), 1, "thread must stay listed");
        // 但对话内容已清空。
        let loaded = s.load("thread-a").unwrap().expect("thread must load");
        assert!(loaded.messages.is_empty(), "messages must be dropped");
        assert!(loaded.items.is_empty(), "items must be dropped");
    }

    #[test]
    fn truncate_rejects_an_invalid_id() {
        let (_d, s) = store();
        let err = s.truncate("../escape").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_store::tests::truncate
```

预期：编译失败，`no method named 'truncate' found for struct 'ThreadStore'`。

- [ ] **Step 3: 实现 `truncate`**

在 `delete` 方法之后插入：

```rust
    /// 清空一个 thread 的**对话记录**，保留它的身份。
    ///
    /// 只删 `.jsonl`，保留 `.meta.json`：thread 仍在 `list()` 里、仍能 `resume`
    /// （回放为空），标题 / cwd / 权限模式不变。这是 `/clear` 的持久化语义——
    /// 若只清内存而不删日志，`resume` 会把旧消息回放回来，用户以为清空了实则没有。
    pub fn truncate(&self, id: &str) -> io::Result<bool> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        // 与 `delete` 一样持锁：避免与并发 `update_meta` 交错。
        let _guard = self
            .meta_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let log = self.log_path(id);
        let existed = log.exists();
        match std::fs::remove_file(&log) {
            Ok(()) => {}
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(existed)
    }
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_store::tests
```

预期：全部通过（含既有的 `create_then_load_round_trips_meta` 等）。

- [ ] **Step 5: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs
git commit -m "feat: add ThreadStore::truncate for clearing a thread's log"
```

---

### Task 2: 给 `ThreadSession` 加 session 命令通道

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/session.rs`
- Test: 同文件 `#[cfg(test)] mod tests`（**本文件当前没有 tests 模块，需新建**）

**Interfaces:**
- Consumes: `tokio::sync::{mpsc, oneshot}`
- Produces:
  - `pub enum SessionCommand { Clear { reply: oneshot::Sender<Result<(), String>> }, Compact { reply: oneshot::Sender<CompactOutcome> } }`
  - `pub enum CompactOutcome { Compacted, NotReduced, Failed(String) }`（`Debug + PartialEq`）
  - `ThreadSession.session_tx: mpsc::Sender<SessionCommand>`（`pub(crate)`，与既有 `prompt_tx` 一致）

- [ ] **Step 1: 写失败的测试**

在 `session.rs` 末尾追加：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// `SessionCommand` 是 driver 与主循环之间的请求/应答桥：reply 通道必须
    /// 能原样把结果带回调用方（clear 的结果、compact 的三态）。
    #[tokio::test]
    async fn session_command_replies_round_trip() {
        let (tx, mut rx) = mpsc::channel::<SessionCommand>(1);
        let (reply, answer) = oneshot::channel();
        tx.send(SessionCommand::Clear { reply }).await.unwrap();
        let command = rx.recv().await.expect("command must arrive");
        match command {
            SessionCommand::Clear { reply } => reply.send(Ok(())).unwrap(),
            SessionCommand::Compact { .. } => panic!("expected Clear"),
        }
        assert_eq!(answer.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn compact_outcome_distinguishes_the_three_states() {
        let (tx, mut rx) = mpsc::channel::<SessionCommand>(1);
        let (reply, answer) = oneshot::channel();
        tx.send(SessionCommand::Compact { reply }).await.unwrap();
        let SessionCommand::Compact { reply } = rx.recv().await.unwrap() else {
            panic!("expected Compact");
        };
        reply.send(CompactOutcome::NotReduced).unwrap();
        assert_eq!(answer.await.unwrap(), CompactOutcome::NotReduced);
        assert_ne!(CompactOutcome::Compacted, CompactOutcome::NotReduced);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib session::tests
```

预期：编译失败，`cannot find type 'SessionCommand'`。

- [ ] **Step 3: 实现类型**

在 `session.rs` 顶部把 `use tokio::sync::mpsc;` 改为 `use tokio::sync::{mpsc, oneshot};`，并在 `InterjectionRequest` 之后插入：

```rust
/// 发给 driver 的会话级命令（清空 / 压缩）。
///
/// 必须走 driver 而不是主循环：`Agent::session()` 只返回 `Session` 的 clone，
/// 修改 session 的可变访问权只存在于持有 agent 的 driver 内部。
pub enum SessionCommand {
    /// 清空该 thread 的上下文，并截断其持久化对话日志。
    Clear {
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// 压缩该 thread 的上下文。
    Compact {
        reply: oneshot::Sender<CompactOutcome>,
    },
}

/// `/compact` 的三种结果。`NotReduced` 是"历史太短、无需压缩"，**不是错误**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactOutcome {
    Compacted,
    NotReduced,
    Failed(String),
}
```

在 `ThreadSession` 里 `interject_tx` 之后加字段：

```rust
    /// 会话级命令（清空 / 压缩）的投递端；与 prompt_tx/interrupt_tx 同类。
    pub(crate) session_tx: mpsc::Sender<SessionCommand>,
```

- [ ] **Step 4: 补齐两处 `ThreadSession` 构造点（否则 crate 编译不过）**

加了结构体字段就必须在所有构造点初始化。`server.rs` 有两处 `ThreadSession { ... }` 字面量（`thread/start` 约 `:641`、`thread/resume` 约 `:810`）。两处都在 `interject_tx,` 之后加一行：

```rust
                                session_tx,
```

并在这两处的 `let (interject_tx, interject_rx) = ...` 附近各加一条通道：

```rust
                        let (session_tx, session_rx) = mpsc::channel::<SessionCommand>(8);
```

`session_rx` 此刻还没被消费会报 `unused variable`（不是错误，但别留）。Task 3 会把它接进 driver；本步先加 `drop(session_rx);` 占位，Task 3 Step 5 移除。

在 `server.rs` 的 `use crate::session::{...}` 里加入 `SessionCommand`：

```rust
use crate::session::{InterjectionRequest, SessionCommand, ThreadSession, TurnPrompt};
```

- [ ] **Step 5: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib session::tests
```

预期：2 个测试通过。同时 `cargo build -p yi-agent-app-server` 应无错误。

- [ ] **Step 6: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/session.rs yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat: add a session-command channel to ThreadSession"
```

---

### Task 3: driver 处理 `session_rx`（clear / compact）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`run_thread_driver` 签名与函数体）
- Test: 本任务无独立单测，由 Task 4 / Task 5 的集成测试覆盖

**Interfaces:**
- Consumes: `SessionCommand`、`CompactOutcome`（Task 2）；`yi_agent_core::compact_session(provider, config, &session)`；`Agent::with_session(Session)`
- Produces: `run_thread_driver` 新增参数 `provider: Arc<dyn yi_agent_core::Provider>`、`config: yi_agent_core::AgentConfig`、`session_rx: mpsc::Receiver<SessionCommand>`（位置见 Step 4）

- [ ] **Step 1: 加 driver 参数**

`run_thread_driver` 签名（约 `:1398`）在 `mut interject_rx: mpsc::Receiver<InterjectionRequest>,` 之后插入：

```rust
    mut session_rx: mpsc::Receiver<SessionCommand>,
```

并在 `status: Arc<std::sync::Mutex<ThreadStatus>>,` 之后插入：

```rust
    // clear / compact 需要它们：compact 要调 provider 生成摘要，两者都要重建 agent。
    provider: Arc<dyn yi_agent_core::Provider>,
    config: yi_agent_core::AgentConfig,
```

- [ ] **Step 2: 抽出"收尾一个 turn"的辅助函数**

driver 里那段"落盘 + 回 Idle + 上报 Finished"（约 `:1636`–`:1661`，从 `let mut items = Vec::with_capacity(...)` 到 `let _ = turn_tx.send(finished_event(...)).await;`）在 clear / compact 后**也要跑一遍**（否则清空/压缩后的状态不会落盘）。把它抽成自由函数，放在 `run_thread_driver` 之前：

```rust
/// 一个 turn 结束后（或被 clear / compact 改动后）的收尾：把当前 session 快照
/// 落盘、置 Idle、上报 Finished。
///
/// clear / compact 之后必须调用它：`/clear` 要落一条空快照（否则 resume 回放的是
/// 旧日志），`/compact` 要把压缩结果写回 `.jsonl`。
#[allow(clippy::too_many_arguments)]
async fn persist_and_finish_turn<W>(
    thread_id: &str,
    turn_id: &str,
    user_prompt: Option<&str>,
    agent: &yi_agent_core::Agent,
    completed_items: Vec<crate::protocol::Item>,
    last_usage: Option<crate::thread_store::TurnUsage>,
    store: &crate::thread_store::ThreadStore,
    writer: &MessageWriter<W>,
    turn_tx: &mpsc::Sender<TurnEvent>,
    status: &Arc<std::sync::Mutex<ThreadStatus>>,
) where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut items = Vec::with_capacity(completed_items.len() + 1);
    if let Some(prompt) = user_prompt {
        // 基线 server 不 emit userMessage，必须在落盘时补齐，否则 resume 会丢用户提问。
        items.push(crate::protocol::Item::UserMessage {
            id: format!("user-{turn_id}"),
            text: prompt.to_string(),
        });
    }
    items.extend(completed_items);

    let record = crate::thread_store::TurnLine::Turn {
        items,
        usage: last_usage,
        messages: agent.session().messages().to_vec(),
    };
    // append 失败则跳过 touch：避免出现"幽灵" thread。
    if let Err(e) = store.append_turn(thread_id, &record) {
        eprintln!("[app-server] failed to persist turn {turn_id} of {thread_id}: {e}");
    } else if let Some(prompt) = user_prompt {
        if let Err(e) = store.touch(thread_id, Some(prompt)) {
            eprintln!("[app-server] failed to update meta for {thread_id}: {e}");
        }
    }

    let _ = update_status(writer, status, thread_id, ThreadStatus::Idle).await;
    let _ = turn_tx.send(finished_event(thread_id, turn_id)).await;
}
```

- [ ] **Step 3: 新增 `session_rx` 分支**

安全规则：**driver 的 agent/session 只在 turn 边界改动。** 这个 `select!` 是内层循环，而内层循环只在"有 turn 在跑"时存在，所以**放在这里**的 clear/compact 一定会跑在半个 turn 上。因此本步的职责是**接住命令**，而不是就地执行——把命令暂存，等本轮 turn 结束后（Step 4 的收尾点）再执行，执行完立即 `continue` 跳过本轮落盘。

先在 driver 开头（`let mut activation_attempted = false;` 之后）加一个暂存位：

```rust
    // 内层 select 期间收到的会话命令。此刻 turn 正在跑，不能改 agent，
    // 暂存到这里，等本轮收尾时执行（见 persist_and_finish_turn 之后的处理）。
    let mut pending_session_command: Option<SessionCommand> = None;
```

在 `select!` 内、`Some(request) = interject_rx.recv() => { ... }` 分支之后追加：

```rust
                Some(command) = session_rx.recv() => {
                    // 本轮已有一个待执行命令时保留先到的那个,后到的直接拒绝,
                    // 避免两端各自 await 一个永远不会有回应的 reply。
                    if pending_session_command.is_some() {
                        match command {
                            SessionCommand::Clear { reply } => {
                                let _ = reply.send(Err("另有一个会话命令待执行".into()));
                            }
                            SessionCommand::Compact { reply } => {
                                let _ = reply.send(CompactOutcome::Failed(
                                    "另有一个会话命令待执行".into(),
                                ));
                            }
                        }
                    } else {
                        pending_session_command = Some(command);
                    }
                }
```

- [ ] **Step 4: 用抽取出的函数替换原来的收尾代码，并执行暂存的会话命令**

把 `select!` 之后的整段收尾（原先 `:1636`–`:1661`）替换为：

```rust
        // 先执行本轮暂存的会话命令（此刻 turn 已结束，改 agent 是安全的），
        // 再落盘——否则会先用压缩前的 session 覆盖日志，再被压缩结果覆盖一次。
        match pending_session_command.take() {
            Some(SessionCommand::Clear { reply }) => {
                agent = agent.with_session(yi_agent_core::Session::new());
                // 截断日志：只清内存而保留 .jsonl 的话，resume 会把旧消息回放
                // 回来，用户以为清空了实则没有。
                let truncate = store.truncate(&thread_id);
                if let Err(e) = &truncate {
                    eprintln!("[app-server] failed to truncate thread log {thread_id}: {e}");
                }
                // 不传 user_prompt → 不 touch meta（清空不改 thread 身份）。
                persist_and_finish_turn(
                    &thread_id,
                    &turn_id,
                    None,
                    &agent,
                    Vec::new(),
                    None,
                    &store,
                    &writer,
                    &turn_tx,
                    &status,
                )
                .await;
                let _ = reply.send(truncate.map(|_| ()));
                continue;
            }
            Some(SessionCommand::Compact { reply }) => {
                let session = agent.session();
                let outcome = match yi_agent_core::compact_session(&provider, &config, &session)
                    .await
                {
                    Ok(Some(compacted)) => {
                        agent = agent.with_session(compacted);
                        CompactOutcome::Compacted
                    }
                    Ok(None) => CompactOutcome::NotReduced,
                    Err(e) => CompactOutcome::Failed(e.to_string()),
                };
                persist_and_finish_turn(
                    &thread_id,
                    &turn_id,
                    None,
                    &agent,
                    Vec::new(),
                    None,
                    &store,
                    &writer,
                    &turn_tx,
                    &status,
                )
                .await;
                let _ = reply.send(outcome);
                continue;
            }
            None => {}
        }

        // 无暂存命令：常规收尾（把本轮结果落盘）。
        persist_and_finish_turn(
            &thread_id,
            &turn_id,
            Some(&user_prompt),
            &agent,
            std::mem::take(&mut completed_items),
            last_usage.take(),
            &store,
            &writer,
            &turn_tx,
            &status,
        )
        .await;
    }
}
```

注意：`persist_and_finish_turn` 只回 Idle / 上报 Finished，**不碰 `active_turn_id`**（那是主循环的职责，见 `:1213`），所以 clear 之后 `turn/start` 不会被误判为 `-32012`。

- [ ] **Step 5: 更新两处 spawn 点**

`thread/start`（约 `:666`）与 `thread/resume`（约 `:829`）的 `tokio::spawn(run_thread_driver(...))` 调用里，在 `interject_rx,` 之后加 `session_rx,`（并删掉 Task 2 Step 4 的 `drop(session_rx);` 占位），在 `Arc::clone(&store_status),` 之后加：

```rust
                            provider,
                            config,
```

这两处需要在解构 `BuiltAgent` 时**不要**把 `provider` / `config` 丢掉——当前写法是 `let BuiltAgent { agent, decision_tx, catalog, yolo, .. } = activation.built;`，改成：

```rust
                        let BuiltAgent { agent, provider, config, decision_tx, catalog, yolo, .. } =
                            activation.built;
```

- [ ] **Step 6: 编译并跑既有 app-server 测试**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server
```

预期：编译通过，既有测试全绿（本任务不改行为，只加分支）。

- [ ] **Step 7: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat: let the thread driver run clear and compact"
```

---

### Task 4: `thread/clear` RPC

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（主循环 match，在 `thread/delete` 分支之后）
- Test: `server.rs` 的 `mod tests`

**Interfaces:**
- Consumes: `ThreadSession.session_tx`（Task 2）、driver 的 `SessionCommand::Clear` 分支（Task 3）、`require_thread_id`、`err_response`、`ok_response`、`RpcError::{unknown_thread, turn_in_progress}`
- Produces: RPC `thread/clear`，params `{threadId}`，成功 `result: {}`

- [ ] **Step 1: 写失败的测试（清空 + 不复活）**

在 `server.rs` 的 `mod tests` 里，`thread_resume_replays_history_and_restores_context` 之后追加：

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn thread_clear_empties_the_context_and_resume_does_not_revive_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;

        // 先跑一轮，制造可被清空的上下文；必须等本轮真正收尾（日志已 append 且
        // turn/completed 已到）再发 clear——落盘发生在 turn/completed 之后,
        // 早发会被主循环以 -32012（turn 进行中）拒绝。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .await;
        let log = dir
            .path()
            .join(".yi-agent/threads")
            .join(format!("{tid}.jsonl"));
        let mut persisted = false;
        for _ in 0..200 {
            if let Ok(t) = std::fs::read_to_string(&log) {
                if !t.trim().is_empty() {
                    persisted = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(persisted, "turn must be persisted before we clear");
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
                break;
            }
        }

        // clear。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/clear","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let mut cleared = false;
        for _ in 0..8 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(4)) {
                assert!(v.get("error").is_none(), "clear must succeed: {v}");
                cleared = true;
                break;
            }
        }
        assert!(cleared, "thread/clear must respond");

        // 关键回归：resume 不得把旧消息回放回来。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let mut replayed = Vec::new();
        for _ in 0..8 {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("item/completed") {
                replayed.push(v["params"]["item"]["text"].as_str().unwrap_or("").to_string());
            }
            if v.get("id") == Some(&serde_json::json!(5)) {
                assert!(v.get("error").is_none(), "resume must still work: {v}");
                break;
            }
        }
        assert!(
            !replayed.iter().any(|t| t == "hello"),
            "cleared context must not come back on resume: {replayed:?}"
        );

        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_clear_rejects_an_unknown_thread() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/clear","params":{"threadId":"nope"}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["error"]["code"], -32011, "expected unknown thread: {v}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_clear_is_rejected_while_a_turn_is_running() {
        let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;
        // 等到 turn 真正开始（turn/started）再发 clear。
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/started") {
                break;
            }
        }
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/clear","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        // slow provider 期间会持续推 item/delta 通知,必须按 id 找到响应本身。
        let mut rejected = None;
        for _ in 0..8 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(4)) {
                rejected = Some(v);
                break;
            }
        }
        let v = rejected.expect("thread/clear must respond");
        assert_eq!(v["error"]["code"], -32012, "expected turn in progress: {v}");
        h.shutdown().await;
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_clear
```

预期：三个测试失败——`"method not found: thread/clear"`（server 回 `-32601`）。

- [ ] **Step 3: 实现主循环分支**

在 `server.rs` 主循环的 `"thread/delete" => { ... }` 分支之后插入：

```rust
                    "thread/clear" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        let Some(session) = threads.get(&thread_id) else {
                            write_response(
                                &writer,
                                err_response(id, RpcError::unknown_thread(&thread_id)),
                            )
                            .await?;
                            continue;
                        };
                        // 清空跑在半个 turn 上会产出不自洽的历史，直接拒绝。
                        if session.active_turn_id.is_some() {
                            write_response(
                                &writer,
                                err_response(id, RpcError::turn_in_progress(&thread_id)),
                            )
                            .await?;
                            continue;
                        }
                        let (reply_tx, reply_rx) = oneshot::channel();
                        if session
                            .session_tx
                            .send(SessionCommand::Clear { reply: reply_tx })
                            .await
                            .is_err()
                        {
                            write_response(
                                &writer,
                                err_response(id, RpcError::internal("thread driver is gone")),
                            )
                            .await?;
                            continue;
                        }
                        match reply_rx.await {
                            Ok(Ok(())) => {
                                write_response(&writer, ok_response(id, json!({}))).await?
                            }
                            Ok(Err(message)) => write_response(
                                &writer,
                                err_response(id, RpcError::internal(message)),
                            )
                            .await?,
                            Err(_) => write_response(
                                &writer,
                                err_response(id, RpcError::internal("thread driver dropped")),
                            )
                            .await?,
                        }
                    }
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_clear
```

预期：3 个测试通过。

- [ ] **Step 5: 跑整个 crate 的测试，确认无回归**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server
```

预期：全绿。**若 `thread_clear_empties_the_context_and_resume_does_not_revive_it` 卡住**，多半是 clear 之后 driver 的 `break` 让 `stream` 被丢弃、而 `turn_rx`/`active_turn_id` 没被重置——检查 Task 3 Step 3 是否走了 `persist_and_finish_turn`（它负责回 Idle 与上报 Finished），以及主循环收到 Finished 后是否清了 `active_turn_id`（既有逻辑在 `:1213`）。

- [ ] **Step 6: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat: add the thread/clear RPC"
```

---

### Task 5: `thread/compact` RPC

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（主循环 match，紧跟 `thread/clear` 分支）
- Test: `server.rs` 的 `mod tests`（需新增一个 `ErroringProvider`）

**Interfaces:**
- Consumes: `SessionCommand::Compact` / `CompactOutcome`（Task 2）、driver 的 compact 分支（Task 3）、`build_test_agent` / `RecordingProvider`
- Produces: RPC `thread/compact`，params `{threadId}`，成功 `result: {"status":"compacted"|"not_reduced"|"failed"}`（`failed` 时附 `"error"`）

- [ ] **Step 1: 加一个"调用即失败"的测试 provider**

在 `server.rs` 测试模块里 `RecordingProvider` 定义之后追加：

```rust
    /// 每次调用都返回 provider 错误：用于测试 `/compact` 的 `failed` 三态。
    struct ErroringProvider;

    #[async_trait]
    impl yi_agent_core::Provider for ErroringProvider {
        async fn call_stream(
            &self,
            _req: yi_agent_core::provider::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
            yi_agent_core::provider::ProviderError,
        > {
            Err(yi_agent_core::provider::ProviderError::Network(
                "boom".into(),
            ))
        }
    }
```

- [ ] **Step 2: 写失败的测试（三态）**

在 `thread_clear_rejects_an_unknown_thread` 之后追加：

```rust
    /// 构造一份「可压缩」的测试 agent 工厂：初始 session 有 3 条消息
    /// （user/assistant/user），压缩后会变成 2 条。
    ///
    /// 注意只放 1 条 user 消息是**不可压缩**的：`plan_compaction` 会把历史里
    /// 所有 user 消息合并成一条，消息数不减少即返回 `None`（→ `not_reduced`）。
    fn compactable_factory(
        session: Option<yi_agent_core::Session>,
        _cwd: &std::path::Path,
        _mode: crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent> {
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(MockProvider);
        let config = yi_agent_core::AgentConfig::default();
        let session = session.unwrap_or_else(|| {
            let mut s = yi_agent_core::Session::new();
            s.replace_messages(vec![
                yi_agent_core::Message::user("first"),
                yi_agent_core::Message::assistant(vec![yi_agent_core::ContentBlock::Text(
                    "reply".into(),
                )]),
                yi_agent_core::Message::user("second"),
            ]);
            s
        });
        Ok(BuiltAgent {
            agent: yi_agent_core::Agent::new(
                provider.clone(),
                Arc::new(yi_agent_core::ToolRegistry::new()),
                config.clone(),
            )
            .with_session(session),
            provider,
            config,
            decision_tx: None,
            decision_rx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        })
    }

    /// 起一个 thread，然后调 `thread/compact`，返回响应的信封。
    async fn compact_thread(h: &mut Harness, tid: &str) -> serde_json::Value {
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":9,"method":"thread/compact","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        for _ in 0..8 {
            let v = h.read_value().await;
            // 跳过沿线可能出现的通知（thread/status/updated 等），只等响应。
            if v.get("id") == Some(&serde_json::json!(9)) {
                return v;
            }
        }
        panic!("thread/compact must respond");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_compact_reports_compacted_when_history_shrinks() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, compactable_factory, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        let v = compact_thread(&mut h, &tid).await;
        assert_eq!(v["result"]["status"], "compacted", "expected compaction: {v}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_compact_reports_not_reduced_for_a_short_history() {
        let mut h = Harness::new(); // build_test_agent 的 session 为空
        let tid = start_thread(&mut h).await;
        let v = compact_thread(&mut h, &tid).await;
        assert_eq!(v["result"]["status"], "not_reduced", "expected no-op: {v}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_compact_reports_failure_without_breaking_the_connection() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let build = |session: Option<yi_agent_core::Session>,
                     _cwd: &std::path::Path,
                     _mode: crate::thread_store::ThreadMode| {
            let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(ErroringProvider);
            let config = yi_agent_core::AgentConfig::default();
            let mut session = session.unwrap_or_default();
            session.replace_messages(vec![
                yi_agent_core::Message::user("first"),
                yi_agent_core::Message::assistant(vec![yi_agent_core::ContentBlock::Text(
                    "reply".into(),
                )]),
                yi_agent_core::Message::user("second"),
            ]);
            Ok(BuiltAgent {
                agent: yi_agent_core::Agent::new(
                    provider.clone(),
                    Arc::new(yi_agent_core::ToolRegistry::new()),
                    config.clone(),
                )
                .with_session(session),
                provider,
                config,
                decision_tx: None,
                decision_rx: None,
                catalog: None,
                yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            })
        };
        let mut h = Harness::with_config(cfg, build, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        let v = compact_thread(&mut h, &tid).await;
        assert_eq!(v["result"]["status"], "failed", "expected failure: {v}");
        assert!(v["result"]["error"].is_string(), "must carry the reason: {v}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_compact_is_rejected_while_a_turn_is_running() {
        let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/started") {
                break;
            }
        }
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/compact","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        // slow provider 期间会持续推 item/delta 通知,必须按 id 找到响应本身。
        let mut rejected = None;
        for _ in 0..8 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(4)) {
                rejected = Some(v);
                break;
            }
        }
        let v = rejected.expect("thread/compact must respond");
        assert_eq!(v["error"]["code"], -32012, "expected turn in progress: {v}");
        h.shutdown().await;
    }
```

- [ ] **Step 3: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_compact
```

预期：`"method not found: thread/compact"`。

- [ ] **Step 4: 实现主循环分支**

紧跟 `thread/clear` 分支之后插入：

```rust
                    "thread/compact" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        let Some(session) = threads.get(&thread_id) else {
                            write_response(
                                &writer,
                                err_response(id, RpcError::unknown_thread(&thread_id)),
                            )
                            .await?;
                            continue;
                        };
                        // 压缩会重写整段历史，turn 进行中不接受。
                        if session.active_turn_id.is_some() {
                            write_response(
                                &writer,
                                err_response(id, RpcError::turn_in_progress(&thread_id)),
                            )
                            .await?;
                            continue;
                        }
                        let (reply_tx, reply_rx) = oneshot::channel();
                        if session
                            .session_tx
                            .send(SessionCommand::Compact { reply: reply_tx })
                            .await
                            .is_err()
                        {
                            write_response(
                                &writer,
                                err_response(id, RpcError::internal("thread driver is gone")),
                            )
                            .await?;
                            continue;
                        }
                        // compact 要调一次 provider 生成摘要，属于长请求；不设短超时。
                        let result = match reply_rx.await {
                            Ok(CompactOutcome::Compacted) => json!({"status": "compacted"}),
                            Ok(CompactOutcome::NotReduced) => json!({"status": "not_reduced"}),
                            Ok(CompactOutcome::Failed(message)) => {
                                json!({"status": "failed", "error": message})
                            }
                            Err(_) => json!({"status": "failed", "error": "thread driver dropped"}),
                        };
                        write_response(&writer, ok_response(id, result)).await?;
                    }
```

并在 `server.rs` 顶部的 `use crate::session::{...}` 里补上 `CompactOutcome`：

```rust
use crate::session::{CompactOutcome, InterjectionRequest, SessionCommand, ThreadSession, TurnPrompt};
```

同时把错误码文档注释更新：`protocol.rs` 的 `-32011` / `-32012` 现在也被 `thread/clear` / `thread/compact` 复用（只改注释，不动逻辑）。

- [ ] **Step 5: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_compact
```

预期：4 个测试通过。

- [ ] **Step 6: 全 crate 回归**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server
```

预期：全绿。

- [ ] **Step 7: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/
git commit -m "feat: add the thread/compact RPC"
```

---

### Task 6: 前端 slash 命令目录与解析

**Files:**
- Create: `desktop/src/lib/slash.ts`
- Test: `desktop/src/lib/slash.test.ts`

**Interfaces:**
- Consumes: 无
- Produces:
  - `export interface SlashCommandSpec { name: string; description: string; usage: string | null; needsArg: boolean }`
  - `export const SLASH_COMMANDS: SlashCommandSpec[]`
  - `export function filterCommands(prefix: string): SlashCommandSpec[]`
  - `export type ParsedSlashInput = { kind: "command"; name: string; args: string | null } | { kind: "path" } | { kind: "none" }`
  - `export function parseSlashInput(text: string): ParsedSlashInput`
  - `export function renderHelp(target?: string | null): string`

- [ ] **Step 1: 写失败的测试**

创建 `desktop/src/lib/slash.test.ts`：

```ts
import { describe, it, expect } from "vitest";
import {
  SLASH_COMMANDS,
  filterCommands,
  parseSlashInput,
  renderHelp,
} from "./slash";

describe("slash command catalog", () => {
  it("lists exactly the six supported commands, in popup order", () => {
    expect(SLASH_COMMANDS.map((c) => c.name)).toEqual([
      "clear",
      "compact",
      "config",
      "cost",
      "help",
      "model",
    ]);
  });

  it("does not offer /quit (closing the window is a system action)", () => {
    expect(SLASH_COMMANDS.find((c) => c.name === "quit")).toBeUndefined();
  });

  it("gives every command a non-empty Chinese description", () => {
    for (const c of SLASH_COMMANDS) {
      expect(c.description.length).toBeGreaterThan(0);
    }
  });

  it("filters by prefix, not fuzzy match", () => {
    expect(filterCommands("co").map((c) => c.name)).toEqual(["compact", "config", "cost"]);
    expect(filterCommands("cost").map((c) => c.name)).toEqual(["cost"]);
    expect(filterCommands("xyz")).toEqual([]);
  });

  it("returns every command for an empty prefix", () => {
    expect(filterCommands("")).toHaveLength(SLASH_COMMANDS.length);
  });
});

describe("parseSlashInput", () => {
  it("parses a bare command with no args", () => {
    expect(parseSlashInput("/cost")).toEqual({ kind: "command", name: "cost", args: null });
  });

  it("parses a command with args, trimmed", () => {
    expect(parseSlashInput("/help   cost  ")).toEqual({
      kind: "command",
      name: "help",
      args: "cost",
    });
  });

  it("leaves plain text alone", () => {
    expect(parseSlashInput("hello")).toEqual({ kind: "none" });
  });

  it("treats a first token with two or more slashes as a path", () => {
    // TUI parity: /Users/me/src is a path, not the /Users command.
    expect(parseSlashInput("/Users/me/src")).toEqual({ kind: "path" });
    expect(parseSlashInput("/tmp/some/dir explain this")).toEqual({ kind: "path" });
  });

  it("still treats a single-slash token as a command", () => {
    expect(parseSlashInput("/tmp")).toEqual({ kind: "command", name: "tmp", args: null });
  });
});

describe("renderHelp", () => {
  it("lists every command with usage and description", () => {
    const text = renderHelp();
    for (const c of SLASH_COMMANDS) {
      expect(text).toContain(`/${c.name}`);
      expect(text).toContain(c.description);
    }
  });

  it("describes one command when given a target", () => {
    expect(renderHelp("help")).toContain("用法");
  });

  it("reports an unknown command", () => {
    expect(renderHelp("nope")).toContain("未知命令");
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/lib/slash.test.ts
```

预期：失败，`Failed to resolve import "./slash"`。

- [ ] **Step 3: 实现 `slash.ts`**

创建 `desktop/src/lib/slash.ts`：

```ts
/**
 * The desktop slash-command catalog — the single source of truth for the popup,
 * the `/help` output, and `App`'s dispatch.
 *
 * Mirrors the "local" subset of the TUI's catalog
 * (`yi-agent-rs/crates/yi-agent/src/tui/slash.rs`). The TUI's 21 daemon-backed
 * commands (`/agents`, `/pause`, …) are deliberately absent: they reach the
 * daemon over a Unix socket, which the frontend never touches.
 */

export interface SlashCommandSpec {
  name: string;
  description: string;
  usage: string | null;
  needsArg: boolean;
}

export const SLASH_COMMANDS: SlashCommandSpec[] = [
  { name: "clear", description: "清空对话上下文", usage: null, needsArg: false },
  { name: "compact", description: "压缩对话历史", usage: null, needsArg: false },
  { name: "config", description: "显示当前配置", usage: null, needsArg: false },
  { name: "cost", description: "显示 token 使用量", usage: null, needsArg: false },
  { name: "help", description: "显示帮助信息", usage: "[command]", needsArg: false },
  { name: "model", description: "显示当前模型", usage: null, needsArg: false },
];

/** Prefix filter, matching the TUI's `CommandPopup::filter` (not fuzzy). */
export function filterCommands(prefix: string): SlashCommandSpec[] {
  const p = prefix.trim();
  if (p === "") return SLASH_COMMANDS;
  return SLASH_COMMANDS.filter((c) => c.name.startsWith(p));
}

export type ParsedSlashInput =
  | { kind: "command"; name: string; args: string | null }
  | { kind: "path" }
  | { kind: "none" };

/**
 * Classify input that starts with `/`.
 *
 * A first token containing two or more slashes is an absolute path and belongs
 * to the agent, not the command popup — the same rule the TUI applies (see its
 * `submit_` / `unknown_slash_command_shows_error` tests): `/tmp` is a command
 * attempt, `/Users/me/src` is a path.
 */
export function parseSlashInput(text: string): ParsedSlashInput {
  const trimmed = text.trim();
  if (!trimmed.startsWith("/")) return { kind: "none" };
  const firstSpace = trimmed.search(/\s/);
  const firstToken = firstSpace === -1 ? trimmed : trimmed.slice(0, firstSpace);
  const slashes = [...firstToken].filter((ch) => ch === "/").length;
  if (slashes >= 2) return { kind: "path" };
  const name = firstToken.slice(1);
  if (name === "") return { kind: "none" };
  const rest = firstSpace === -1 ? "" : trimmed.slice(firstSpace).trim();
  return { kind: "command", name, args: rest === "" ? null : rest };
}

/** Render `/help` output from the same catalog the popup uses. */
export function renderHelp(target?: string | null): string {
  const name = target?.trim().replace(/^\//, "");
  if (name) {
    const command = SLASH_COMMANDS.find((c) => c.name === name);
    if (!command) return `未知命令: /${name}`;
    const usage = command.usage ? ` ${command.usage}` : "";
    return `用法: /${command.name}${usage}\n${command.description}`;
  }
  const lines = SLASH_COMMANDS.map((c) => {
    const usage = c.usage ? ` ${c.usage}` : "";
    return `  /${c.name}${usage} ${c.description}`;
  });
  return ["可用命令:", ...lines].join("\n");
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop && npx vitest run src/lib/slash.test.ts
```

预期：全部通过。

- [ ] **Step 5: 提交**

```bash
cd desktop && npx tsc --noEmit
cd .. && git add desktop/src/lib/slash.ts desktop/src/lib/slash.test.ts
git commit -m "feat: add the desktop slash command catalog"
```

---

### Task 7: `NoticeItem` —— 不经过 agent 的命令输出

**Files:**
- Modify: `desktop/src/lib/session.ts`
- Modify: `desktop/src/components/ChatView.tsx`
- Test: `desktop/src/lib/session.test.ts`（追加用例）

**Interfaces:**
- Consumes: `Item`（`lib/protocol`）
- Produces:
  - `export interface NoticeItem { type: "notice"; id: string; text: string }`
  - `Session.items` 类型变为 `(Item | NoticeItem)[]`
  - `Session.notice(text: string): void`

- [ ] **Step 1: 写失败的测试**

在 `desktop/src/lib/session.test.ts` 末尾（最后一个 `});` 之前）追加：

```ts
  describe("notices", () => {
    it("appends a notice that is not an agent message", () => {
      const s = new Session();
      s.notice("对话已清空");
      expect(s.items).toHaveLength(1);
      const item = s.items[0];
      expect(item.type).toBe("notice");
      expect((item as { text: string }).text).toBe("对话已清空");
    });

    it("gives each notice a distinct id", () => {
      const s = new Session();
      s.notice("a");
      s.notice("b");
      const ids = s.items.map((i) => i.id);
      expect(new Set(ids).size).toBe(2);
    });

    it("drops notices on reset", () => {
      const s = new Session();
      s.notice("x");
      s.reset();
      expect(s.items).toHaveLength(0);
    });
  });
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/lib/session.test.ts
```

预期：编译/断言失败，`s.notice is not a function`。

- [ ] **Step 3: 实现 `NoticeItem` 与 `Session.notice`**

在 `session.ts` 的 import 之后加类型：

```ts
/**
 * A slash-command output line. Not part of the wire protocol: the app-server
 * knows nothing about it. It mirrors the TUI's `HistoryCell::Separator`, which
 * is how the TUI renders command output without pretending the agent said it.
 */
export interface NoticeItem {
  type: "notice";
  id: string;
  text: string;
}
```

把 `items: Item[] = [];` 改为：

```ts
  items: (Item | NoticeItem)[] = [];
```

在 `addUserMessage` 之后加方法：

```ts
  /** Append a command-output line (never sent to the agent). */
  notice(text: string): void {
    this.items.push({ type: "notice", id: nextLocalId(), text });
  }
```

- [ ] **Step 4: 在 `ChatView` 里渲染 notice**

`ChatView.tsx` 的 import 与 `ChatItem` 各改一处：

```ts
import type { NoticeItem } from "../lib/session";
```

`ChatItem` 的参数类型改为 `{ item: Item | NoticeItem }`，并在 `switch` 最前面加一个 case：

```tsx
    case "notice":
      return (
        <div className="my-1 self-center rounded-md bg-neutral-800/60 px-3 py-1 text-xs text-neutral-400">
          {item.text}
        </div>
      );
```

`ChatView` 的 `items` 参数类型与 `lastText` 判定同步更新：

```tsx
export function ChatView({
  items,
  error,
  retrying,
}: {
  items: (Item | NoticeItem)[];
  error?: string | null;
  retrying?: { attempt: number; max: number; cause: RetryCause } | null;
}) {
```

- [ ] **Step 5: 跑测试确认通过**

```bash
cd desktop && npx vitest run src/lib/session.test.ts src/components/ChatView.test.tsx
```

预期：全绿（`ChatView` 既有 3 个用例不回归）。

- [ ] **Step 6: 提交**

```bash
cd desktop && npx tsc --noEmit
cd .. && git add desktop/src/lib/session.ts desktop/src/lib/session.test.ts desktop/src/components/ChatView.tsx
git commit -m "feat: render slash command output as notices"
```

---

### Task 8: `SlashPopup` 组件

**Files:**
- Create: `desktop/src/components/SlashPopup.tsx`
- Test: `desktop/src/components/SlashPopup.test.tsx`

**Interfaces:**
- Consumes: `SlashCommandSpec`（Task 6）
- Produces: `export function SlashPopup({ commands, selected }: { commands: SlashCommandSpec[]; selected: number }): JSX.Element | null` —— `commands` 为空时返回 `null`；每项 `data-testid="slash-option"`，选中项带 `data-selected="true"`

- [ ] **Step 1: 写失败的测试**

创建 `desktop/src/components/SlashPopup.test.tsx`：

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, cleanup } from "@testing-library/react";
import { SlashPopup } from "./SlashPopup";
import { SLASH_COMMANDS } from "../lib/slash";

afterEach(cleanup);

describe("SlashPopup", () => {
  it("renders nothing when there are no matches", () => {
    const { container } = render(<SlashPopup commands={[]} selected={0} />);
    expect(container).toBeEmptyDOMElement();
  });

  it("renders every command with its usage and description", () => {
    render(<SlashPopup commands={SLASH_COMMANDS} selected={0} />);
    expect(screen.getAllByTestId("slash-option")).toHaveLength(SLASH_COMMANDS.length);
    const selected = screen
      .getAllByTestId("slash-option")
      .filter((el) => el.getAttribute("data-selected") === "true");
    expect(selected).toHaveLength(1);
    // /help 是目录里的第 5 条（index 4）。
    expect(selected[0].textContent).toContain("/help [command]");
    expect(selected[0].textContent).toContain("显示帮助信息");
  });

  it("marks exactly the selected option", () => {
    render(<SlashPopup commands={SLASH_COMMANDS} selected={2} />);
    const selected = screen
      .getAllByTestId("slash-option")
      .filter((el) => el.getAttribute("data-selected") === "true");
    expect(selected).toHaveLength(1);
    expect(selected[0].textContent).toContain("/config");
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/components/SlashPopup.test.tsx
```

预期：失败，无法解析 `./SlashPopup`。

- [ ] **Step 3: 实现组件**

创建 `desktop/src/components/SlashPopup.tsx`：

```tsx
import type { SlashCommandSpec } from "../lib/slash";

/**
 * The command list shown above the input while the user types `/`.
 *
 * Presentational only: the parent owns filtering and the selected index. A
 * plain list of divs (not `role="listbox"`) because the textarea keeps DOM
 * focus — the popup is a visual hint driven by keyboard events on the textarea.
 */
export function SlashPopup({
  commands,
  selected,
}: {
  commands: SlashCommandSpec[];
  selected: number;
}) {
  if (commands.length === 0) return null;
  return (
    <div
      data-testid="slash-popup"
      className="absolute bottom-full left-0 mb-1 max-h-64 w-full overflow-y-auto rounded-md border border-neutral-700 bg-neutral-900 py-1 shadow-lg"
    >
      {commands.map((c, i) => (
        <div
          key={c.name}
          data-testid="slash-option"
          data-selected={i === selected ? "true" : "false"}
          className={
            i === selected
              ? "flex items-baseline gap-2 bg-neutral-700 px-3 py-1 text-sm"
              : "flex items-baseline gap-2 px-3 py-1 text-sm"
          }
        >
          <span className="font-mono text-neutral-100">
            /{c.name}
            {c.usage ? ` ${c.usage}` : ""}
          </span>
          <span className="truncate text-xs text-neutral-400">{c.description}</span>
        </div>
      ))}
    </div>
  );
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop && npx vitest run src/components/SlashPopup.test.tsx
```

预期：3 个用例通过。

- [ ] **Step 5: 提交**

```bash
cd desktop && npx tsc --noEmit
cd .. && git add desktop/src/components/SlashPopup.tsx desktop/src/components/SlashPopup.test.tsx
git commit -m "feat: add the slash command popup component"
```

---

### Task 9: 输入框的弹窗状态机

**Files:**
- Modify: `desktop/src/components/MessageInput.tsx`
- Test: `desktop/src/components/MessageInput.test.tsx`（追加用例）

**Interfaces:**
- Consumes: `filterCommands` / `SLASH_COMMANDS` / `SlashCommandSpec`（Task 6）、`SlashPopup`（Task 8）
- Produces: `MessageInput` 新增两个 props：
  - `onSlashCommand: (name: string, args: string | null) => void`
  - `turnActive` 含义扩展：为 `true` 时输入禁用（发命令被拒，见设计 §5）
  - 弹窗项 `data-testid="slash-option"`，输入框保持 `role="textbox"`

- [ ] **Step 1: 写失败的测试**

先更新既有测试的 helper（新增两个 props），在 `MessageInput.test.tsx` 的 `renderInput` 里加：

```ts
    onSlashCommand: vi.fn(),
```

然后在 `describe("MessageInput", ...)` 末尾追加：

```tsx
  describe("slash commands", () => {
    it("opens the popup on a leading slash and filters as you type", () => {
      renderInput();
      const textarea = screen.getByRole("textbox");
      fireEvent.change(textarea, { target: { value: "/" } });
      expect(screen.getByTestId("slash-popup")).toBeTruthy();

      fireEvent.change(textarea, { target: { value: "/co" } });
      const options = screen.getAllByTestId("slash-option");
      expect(options.map((o) => o.textContent)).toEqual([
        expect.stringContaining("/compact"),
        expect.stringContaining("/config"),
        expect.stringContaining("/cost"),
      ]);
    });

    it("closes the popup once a space starts the argument list", () => {
      renderInput();
      const textarea = screen.getByRole("textbox");
      fireEvent.change(textarea, { target: { value: "/help" } });
      expect(screen.getByTestId("slash-popup")).toBeTruthy();

      fireEvent.change(textarea, { target: { value: "/help co" } });
      expect(screen.queryByTestId("slash-popup")).toBeNull();
    });

    it("moves the selection with arrow keys without sending", () => {
      const onSend = vi.fn(async () => true);
      renderInput({ onSend });
      const textarea = screen.getByRole("textbox");
      fireEvent.change(textarea, { target: { value: "/" } });

      fireEvent.keyDown(textarea, { key: "ArrowDown" });
      const selected = screen
        .getAllByTestId("slash-option")
        .filter((el) => el.getAttribute("data-selected") === "true");
      expect(selected[0].textContent).toContain("/compact");
      expect(onSend).not.toHaveBeenCalled();
    });

    it("completes the selected command on Tab", () => {
      renderInput();
      const textarea = screen.getByRole("textbox") as HTMLTextAreaElement;
      fireEvent.change(textarea, { target: { value: "/he" } });
      fireEvent.keyDown(textarea, { key: "Tab" });
      expect(textarea.value).toBe("/help ");
      expect(screen.queryByTestId("slash-popup")).toBeNull();
    });

    it("runs the selected command on Enter instead of sending the text", () => {
      const onSend = vi.fn(async () => true);
      const onSlashCommand = vi.fn();
      renderInput({ onSend, onSlashCommand });
      const textarea = screen.getByRole("textbox");
      fireEvent.change(textarea, { target: { value: "/cos" } });
      fireEvent.keyDown(textarea, { key: "Enter" });

      expect(onSlashCommand).toHaveBeenCalledWith("cost", null);
      expect(onSend).not.toHaveBeenCalled();
    });

    it("passes arguments through and runs a single-match command on Enter", () => {
      const onSlashCommand = vi.fn();
      renderInput({ onSlashCommand });
      const textarea = screen.getByRole("textbox");
      fireEvent.change(textarea, { target: { value: "/help cost" } });
      fireEvent.keyDown(textarea, { key: "Enter" });
      expect(onSlashCommand).toHaveBeenCalledWith("help", "cost");
    });

    it("reports an unknown command on Enter without sending", () => {
      const onSend = vi.fn(async () => true);
      const onSlashCommand = vi.fn();
      renderInput({ onSend, onSlashCommand });
      const textarea = screen.getByRole("textbox");
      fireEvent.change(textarea, { target: { value: "/nope" } });
      fireEvent.keyDown(textarea, { key: "Enter" });
      expect(onSlashCommand).toHaveBeenCalledWith("nope", null);
      expect(onSend).not.toHaveBeenCalled();
    });

    it("sends a two-slash path to the agent instead of treating it as a command", () => {
      const onSend = vi.fn(async () => true);
      const onSlashCommand = vi.fn();
      renderInput({ onSend, onSlashCommand });
      const textarea = screen.getByRole("textbox");
      fireEvent.change(textarea, { target: { value: "/Users/me/src" } });
      expect(screen.queryByTestId("slash-popup")).toBeNull();
      fireEvent.keyDown(textarea, { key: "Enter" });
      expect(onSlashCommand).not.toHaveBeenCalled();
      expect(onSend).toHaveBeenCalledWith("/Users/me/src");
    });

    it("dismisses the popup on Escape while keeping the text", () => {
      renderInput();
      const textarea = screen.getByRole("textbox") as HTMLTextAreaElement;
      fireEvent.change(textarea, { target: { value: "/he" } });
      fireEvent.keyDown(textarea, { key: "Escape" });
      expect(screen.queryByTestId("slash-popup")).toBeNull();
      expect(textarea.value).toBe("/he");
    });
  });
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/components/MessageInput.test.tsx
```

预期：新用例失败（无 `slash-popup`、`onSlashCommand` 未被调用）。

- [ ] **Step 3: 实现弹窗状态机**

`MessageInput.tsx` 完整改为：

```tsx
import { useEffect, useRef, useState } from "react";
import { ModeChip } from "./ModeChip";
import { SlashPopup } from "./SlashPopup";
import { filterCommands, parseSlashInput } from "../lib/slash";
import { isImeEnter, useImeGuard } from "../lib/imeEnter";
import type { ThreadMode } from "../lib/threadPermissionMode";

export function MessageInput({
  turnActive,
  onSend,
  onInterrupt,
  mode,
  onModeChange,
  onSlashCommand,
}: {
  turnActive: boolean;
  onSend: (text: string) => Promise<boolean>;
  onInterrupt: () => void;
  mode: ThreadMode | null;
  onModeChange: (mode: ThreadMode) => void;
  onSlashCommand: (name: string, args: string | null) => void;
}) {
  const [text, setText] = useState("");
  const [sending, setSending] = useState(false);
  // Latch: true while the popup should be offered. Escape clears it so a later
  // keystroke cannot silently re-open a popup the user just dismissed.
  const [popupOpen, setPopupOpen] = useState(false);
  const [selected, setSelected] = useState(0);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  // Enter that confirms an IME candidate must not send the message; see
  // `isImeCompositionKey` for why keyCode 229 is the load-bearing check here.
  const ime = useImeGuard();

  // The popup shows only while the caret sits in the command name (before the
  // first space); `/help co` is argument mode.
  const parsed = parseSlashInput(text);
  const showPopup =
    popupOpen && parsed.kind === "command" && !text.trim().includes(" ");
  const options = showPopup ? filterCommands(parsed.kind === "command" ? parsed.name : "") : [];

  useEffect(() => {
    setSelected(0);
  }, [text]);

  const handleSend = async () => {
    if (!text.trim() || sending) return;
    setSending(true);
    try {
      const ok = await onSend(text);
      // Only discard the draft once the send was actually accepted; otherwise
      // the user's text would be lost on a rejected turn/start.
      if (ok) setText("");
    } finally {
      setSending(false);
    }
  };

  /** Run the slash command the input currently names (or report it unknown). */
  const runSlash = (name: string, args: string | null) => {
    onSlashCommand(name, args);
    setText("");
    setPopupOpen(false);
  };

  return (
    <div className="relative flex items-end gap-2 border-t border-neutral-800 bg-neutral-900 p-3">
      {showPopup && <SlashPopup commands={options} selected={selected} />}
      <textarea
        ref={inputRef}
        value={text}
        onChange={(e) => {
          const next = e.target.value;
          setText(next);
          // Re-arm the popup whenever the text still looks like a command name.
          setPopupOpen(parseSlashInput(next).kind === "command");
        }}
        onCompositionStart={ime.onCompositionStart}
        onCompositionEnd={ime.onCompositionEnd}
        onBlur={ime.resetComposition}
        onKeyDown={(e) => {
          if (isImeEnter(e, ime.composing.current)) return;
          if (showPopup && options.length > 0) {
            if (e.key === "ArrowDown") {
              e.preventDefault();
              setSelected((i) => (i + 1) % options.length);
              return;
            }
            if (e.key === "ArrowUp") {
              e.preventDefault();
              setSelected((i) => (i - 1 + options.length) % options.length);
              return;
            }
            if (e.key === "Tab") {
              e.preventDefault();
              const picked = options[Math.min(selected, options.length - 1)];
              setText(`/${picked.name} `);
              setPopupOpen(false);
              inputRef.current?.focus();
              return;
            }
            if (e.key === "Escape") {
              e.preventDefault();
              setPopupOpen(false);
              return;
            }
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              const picked = options[Math.min(selected, options.length - 1)];
              runSlash(picked.name, null);
              return;
            }
            return; // 弹窗开启时吞掉其余按键,不作文本处理
          }
          if (e.key === "Escape" && popupOpen) {
            e.preventDefault();
            setPopupOpen(false);
            return;
          }
          if (e.key === "Enter" && !e.shiftKey) {
            // Confirming a candidate with Enter is the IME's key, not the user's:
            // let the composition land in the box and wait for the next Enter.
            e.preventDefault();
            if (parsed.kind === "command") {
              // Enter with no popup match still executes: unknown commands and
              // commands with arguments belong here, not to the agent.
              runSlash(parsed.name, parsed.args);
              return;
            }
            if (parsed.kind === "path") {
              // Two-slash first token is a path (TUI parity) — falls through.
            } else if (turnActive) {
              onInterrupt();
              return;
            }
            void handleSend();
          }
        }}
        disabled={sending || turnActive}
        rows={3}
        placeholder="Type a message… (Enter to send, Shift+Enter for newline)"
        className="flex-1 resize-none rounded-md border border-neutral-700 bg-neutral-950 px-3 py-2 text-sm text-neutral-100 placeholder:text-neutral-600 focus:border-neutral-500 focus:outline-none disabled:opacity-50"
      />
      <ModeChip mode={mode} onChange={onModeChange} disabled={mode === null} />
      <button
        type="button"
        onClick={turnActive ? onInterrupt : () => void handleSend()}
        disabled={sending}
        className={
          turnActive
            ? "rounded-md bg-red-600 px-4 py-2 text-sm font-medium text-white hover:bg-red-500 disabled:opacity-50"
            : "rounded-md bg-blue-600 px-4 py-2 text-sm font-medium text-white hover:bg-blue-500 disabled:opacity-50"
        }
      >
        {turnActive ? "Stop" : "Send"}
      </button>
    </div>
  );
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop && npx vitest run src/components/MessageInput.test.tsx
```

预期：既有 8 个 + 新增 8 个全部通过。

- [ ] **Step 5: 提交**

```bash
cd desktop && npx tsc --noEmit
cd .. && git add desktop/src/components/MessageInput.tsx desktop/src/components/MessageInput.test.tsx
git commit -m "feat: drive the slash popup from the message input"
```

---

### Task 10: 命令分发（App 侧）

**Files:**
- Modify: `desktop/src/lib/protocol.ts`（只加类型与注释，无运行时逻辑）
- Modify: `desktop/src/App.tsx`
- Test: `desktop/src/App.test.tsx`（追加用例）

**Interfaces:**
- Consumes: `SLASH_COMMANDS` / `renderHelp`（Task 6）、`Session.notice`（Task 7）、`MessageInput.onSlashCommand`（Task 9）、`estimateCost` / `formatCost`（`lib/pricing`）
- Produces: `App` 内部的 `onSlashCommand(name, args)`；对 `/clear` 发 `thread/clear`、对 `/compact` 发 `thread/compact`

- [ ] **Step 1: 写失败的测试**

在 `desktop/src/App.test.tsx` 末尾追加：

```tsx
describe("App slash commands", () => {
  it("renders /help output as a notice without touching the agent", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "/help" } });
    fireEvent.keyDown(textarea, { key: "Enter" });

    await screen.findByText(/可用命令:/);
    expect(
      clients[0].requests.some((r) => r.method === "turn/start"),
    ).toBe(false);
  });

  it("sends thread/clear and empties the transcript on /clear", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    // 先造一条消息,让 clear 有东西可清。
    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "hello" } });
    fireEvent.keyDown(textarea, { key: "Enter" });
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(true),
    );
    await screen.findByText("hello");

    fireEvent.change(textarea, { target: { value: "/clear" } });
    fireEvent.keyDown(textarea, { key: "Enter" });

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/clear")).toBe(true),
    );
    await waitFor(() => expect(screen.queryByText("hello")).toBeNull());
    await screen.findByText(/对话已清空/);
  });

  it("surfaces a rejected /clear without clearing the transcript", async () => {
    state.rejectCode = { "thread/clear": -32012 };
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "hello" } });
    fireEvent.keyDown(textarea, { key: "Enter" });
    await screen.findByText("hello");

    fireEvent.change(textarea, { target: { value: "/clear" } });
    fireEvent.keyDown(textarea, { key: "Enter" });

    await screen.findByText(/清空失败|-32012/);
    expect(screen.getByText("hello")).toBeTruthy();
  });

  it("sends thread/compact and reports the server's verdict", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "/compact" } });
    fireEvent.keyDown(textarea, { key: "Enter" });

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/compact")).toBe(true),
    );
    // 默认 mock 返回 {},没有 status 字段 → 视为 unknown,提示"未知结果"。
    await screen.findByText(/压缩/);
  });
});
```

同时给 `App.test.tsx` 的 RPC mock 加上两个方法的响应，在 `if (method === "thread/setPermissionMode") { ... }` 之后插入：

```ts
      if (method === "thread/clear") return {};
      if (method === "thread/compact") return { status: "compacted" };
```

并把 `state.rejectCode` 的类型注释更新为"任意 method 都可强制报错"（既有实现已支持）。

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/App.test.tsx
```

预期：新用例失败（`onSlashCommand` 未接线，`/help` 文本被当作普通消息发送）。

- [ ] **Step 3: 在 `protocol.ts` 记录两个新方法的形状**

在 `protocol.ts` 末尾追加（纯文档类型，不参与运行）：

```ts
/**
 * `thread/clear` 的参数与结果。清空该 thread 的 agent 上下文并截断持久化日志，
 * 保留 thread 身份（标题 / cwd / 模型）。
 */
export interface ThreadClearParams {
  threadId: string;
}
export type ThreadClearResult = Record<string, never>;

/**
 * `thread/compact` 的参数与结果。`status` 三态：
 * - `compacted`   —— 已压缩
 * - `not_reduced` —— 历史太短，无需压缩（不是错误）
 * - `failed`      —— 压缩失败，`error` 带原因
 */
export interface ThreadCompactParams {
  threadId: string;
}
export interface ThreadCompactResult {
  status: "compacted" | "not_reduced" | "failed";
  error?: string;
}
```

- [ ] **Step 4: 在 `App.tsx` 实现分发**

在 `App.tsx` 顶部加 import（用不到的导出别写进来，`noUnusedLocals` 会报错）：

```ts
import { renderHelp } from "./lib/slash";
import { estimateCost, formatCost } from "./lib/pricing";
```

在 `send` 之前加 `onSlashCommand`（`config/read` 用 `clientRef`，与其它 RPC 一致）：

```tsx
  /**
   * Run a slash command. Commands never reach the agent: their output is a
   * `notice` on the current session, mirroring the TUI's Separator cells.
   */
  const onSlashCommand = async (name: string, args: string | null) => {
    const id = store.currentId;
    const c = clientRef.current;
    if (!id || !c) return;
    const view = store.view(id);
    const session = view.session;
    switch (name) {
      case "help":
        session.notice(renderHelp(args));
        break;
      case "cost": {
        const u = session.usage;
        if (!u) {
          session.notice("暂无用量数据");
          break;
        }
        const cost = estimateCost(u);
        session.notice(
          [
            `model: ${u.model}`,
            `input: ${u.input}  output: ${u.output}`,
            `cache read: ${u.cacheRead}  cache write: ${u.cacheWrite}`,
            `估算成本: ${formatCost(cost)}`,
          ].join("\n"),
        );
        break;
      }
      case "model":
        session.notice(`当前模型: ${view.info?.model ?? "未知"}`);
        break;
      case "config":
        try {
          const cfg = await c.request<Record<string, unknown>>("config/read", {});
          session.notice(
            Object.entries(cfg)
              .map(([k, v]) => `${k}: ${String(v)}`)
              .join("\n"),
          );
        } catch (e) {
          session.notice(`读取配置失败: ${formatError(e)}`);
        }
        break;
      case "clear":
        try {
          await c.request("thread/clear", { threadId: id });
          session.reset();
          session.notice("对话已清空");
        } catch (e) {
          // 失败绝不清界面:否则会出现"看着清了、服务端还记得"的假象。
          session.notice(`清空失败: ${formatError(e)}`);
        }
        break;
      case "compact":
        try {
          const r = await c.request<{ status?: string; error?: string }>("thread/compact", {
            threadId: id,
          });
          if (r.status === "compacted") session.notice("对话已压缩");
          else if (r.status === "not_reduced") session.notice("无需压缩：历史太短");
          else session.notice(`压缩失败: ${r.error ?? "未知结果"}`);
        } catch (e) {
          session.notice(`压缩失败: ${formatError(e)}`);
        }
        break;
      default:
        session.notice(`未知命令: /${name}\n${renderHelp()}`);
        break;
    }
    force((v) => v + 1);
  };
```

把 `MessageInput` 的调用改为传入新 prop：

```tsx
          <MessageInput
            turnActive={current?.session.turnActive ?? false}
            onSend={send}
            onInterrupt={interrupt}
            mode={current?.mode ?? null}
            onModeChange={setThreadMode}
            onSlashCommand={(name, args) => void onSlashCommand(name, args)}
          />
```

（`parseSlashInput` 目前只在 `MessageInput` 里用到；若 `App.tsx` 未直接使用它，就从 import 里去掉，避免 `noUnusedLocals` 报错。）

- [ ] **Step 5: 跑测试确认通过**

```bash
cd desktop && npx vitest run src/App.test.tsx
```

预期：既有 19 个 + 新增 4 个全部通过。

- [ ] **Step 6: 全前端回归 + 类型检查 + 构建**

```bash
cd desktop && npx vitest run && npx tsc --noEmit && npm run build
```

预期：全绿。

- [ ] **Step 7: 提交**

```bash
cd .. && git add desktop/src/lib/protocol.ts desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat: dispatch desktop slash commands in the app"
```

---

### Task 11: 同步项目进度文档

**Files:**
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/yi-agent-app-server.md`
- Modify: `docs/project-management/README.md`

**Interfaces:**
- Consumes: 前十个任务的完成判据
- Produces: 文档计数与实际一致

- [ ] **Step 1: 在 `desktop.md` 的 Features 列表末尾（子 Agent 委派那条之后）加一条**

```markdown
- [x] Slash 命令（`/help` `/cost` `/model` `/config` `/clear` `/compact`）— 输入框 `/` 触发弹窗（↑↓ 选择 / Tab 补全 / Enter 执行 / Esc 关闭 / 空格进入参数），命令输出渲染为不经过 agent 的 `notice` 条目；`/clear` 走 `thread/clear`（服务端重建空 session + 截断日志），`/compact` 走 `thread/compact`；验证 `cd desktop && npx vitest run src/lib/slash.test.ts src/components/SlashPopup.test.tsx src/components/MessageInput.test.tsx src/lib/session.test.ts src/App.test.tsx` — [设计](../superpowers/specs/2026-10-01-desktop-slash-commands-design.md)
```

并把该文件末尾「验证命令」里的前端单测总数从 `170 个前端单测` 更新为实际跑出来的数字（跑 `npx vitest run` 看输出），同时把新增的 5 个测试文件补进那份明细列表。

- [ ] **Step 2: 在 `yi-agent-app-server.md` 加两条 feature**

```markdown
- [x] `thread/clear` RPC —— 清空该 thread 的 agent 上下文并截断其 `.jsonl` 对话日志（保留 `.meta.json`，thread 身份/标题/cwd/模式不变），由 per-thread driver 执行（`Agent::session()` 只返回 clone，session 的可变访问权在 driver 内）；turn 进行中回 `-32012`，未知 thread 回 `-32011`；验证 `cd yi-agent-rs && cargo test -p yi-agent-app-server thread_clear`
- [x] `thread/compact` RPC —— 调 `yi_agent_core::compact_session` 原地替换 session，并以 `{"status":"compacted"|"not_reduced"|"failed"}` 区分三态（`not_reduced` 是"历史太短"而非错误）；turn 进行中回 `-32012`；验证 `cd yi-agent-rs && cargo test -p yi-agent-app-server thread_compact`
```

- [ ] **Step 3: 更新 `README.md` 索引计数**

把 `yi-agent-app-server | 17 / 17` 与 `desktop | 23 / 37` 改为加完条目后的实际数字（app-server +2；desktop 的"完成/总计"里完成数 +1，保持总分母不变）。

- [ ] **Step 4: 提交**

```bash
git add docs/project-management/
git commit -m "docs: record the desktop slash commands in the project board"
```

---

### Task 12: 合并回 main

**Files:** 无

**Interfaces:**
- Consumes: 分支 `feat/desktop-slash-commands` 上的全部提交
- Produces: `main` 上的合并提交

- [ ] **Step 1: 全量验证（在 worktree 内）**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/desktop-slash-commands
cd yi-agent-rs && cargo test -p yi-agent-app-server
cd ../desktop && npx vitest run && npx tsc --noEmit && npm run build
```

预期：Rust 与前端全绿。**别并行跑别的 cargo 命令。**

- [ ] **Step 2: 回归 TUI（本计划不该碰它，但确认一下）**

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent
```

预期：全绿（证明 TUI 的 slash 行为未受影响）。

- [ ] **Step 3: 合并**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
git checkout main
git merge --no-ff feat/desktop-slash-commands -m "merge: add slash commands to the desktop app"
git worktree remove .worktrees/feat/desktop-slash-commands
git branch -d feat/desktop-slash-commands
```

- [ ] **Step 4: 确认 main 干净且含合并提交**

```bash
git log --oneline -3
git status --short
```

预期：最新一条是 `merge: add slash commands to the desktop app`，工作区干净。

- [ ] **Step 5: 手工冒烟（可选，但设计 §12 要求最终在真机上验一次）**

```bash
cd desktop && npm run sidecar && npm run tauri dev
```

逐项确认：输入 `/` 弹窗出现；`/help` 输出命令表；`/cost` 在有过一轮对话后显示用量；`/clear` 后消息列表清空；`/compact` 在长会话里提示已压缩；两个命令在 turn 运行时被拒并提示。

---

## 自查

**Spec 覆盖：**

| Spec 章节 | 落在哪个任务 |
|---|---|
| §2 命令集（6 个，无 `/quit`） | Task 6（目录）+ Task 10（分发） |
| §3.1 必须走 driver | Task 3 |
| §3.2 传输（`session_tx`） | Task 2 |
| §3.3 `thread/clear` | Task 1（truncate）+ Task 3 + Task 4 |
| §3.4 `thread/compact` | Task 3 + Task 5 |
| §3.5 协议文档同步 | Task 10 Step 3 |
| §4.1 `lib/slash.ts` | Task 6 |
| §4.2 `SlashPopup.tsx` | Task 8 |
| §4.3 `MessageInput` 状态机 | Task 9 |
| §4.4 `Session.notice` | Task 7 |
| §4.5 `ChatView` 渲染 | Task 7 |
| §4.6 `App` 分发 | Task 10 |
| §5 错误处理 | Task 4/5（错误码）+ Task 10（notice 文案） |
| §6 测试策略 | 各任务的 Step 2/4；`clear` 不复活回归在 Task 4 Step 1 |
| §7 非目标 | 未建任务（有意） |
| §8 风险 | Task 1（保留 meta）、Task 4 Step 5（driver 收尾） |

**占位符扫描：** 无 TBD / TODO；每个代码步骤都带完整代码。

**类型一致性：** `SessionCommand` / `CompactOutcome`（Task 2）在 Task 3/4/5 中名称一致；`SlashCommandSpec` / `filterCommands` / `parseSlashInput`（Task 6）在 Task 8/9/10 中签名一致；`NoticeItem` / `Session.notice`（Task 7）在 Task 10 中一致；`MessageInput.onSlashCommand`（Task 9）在 Task 10 的 JSX 里一致。

**已知的实现期风险（Task 4 Step 5 已写进排查提示）：** driver 在 `session_rx` 分支里 `break` 出内层循环后，依赖既有主循环的 `Finished` 处理把 `active_turn_id` 清空（`:1213`）——若该路径未覆盖，`thread/clear` 之后的 `turn/start` 会被误判为 `-32012`；Task 4 的 resume 回归用例会先暴露它。
