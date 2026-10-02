# Subagent 调用者上下文 Fork Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `spawn_agent` 支持可选的 `fork: true`，把调用者此刻的完整对话分块经 IPC 送入 daemon，子 worker 以自己的 system prompt 和工具集、以上述历史为前缀继续执行 objective。

**Architecture:** 补三条缺失的接线——(1) core 暴露活的 `Arc<Mutex<Session>>` 句柄并允许预置消息；(2) `yi-agent-subagent` 用 `CallerContext` 活句柄在 spawn 时读取「调用者当前对话」并做协议有效裁剪，再经 `BeginForkUpload`/`AppendForkChunk` 分块上传；(3) daemon 端组装 `Vec<Message>` 后经 `WorkerStart.fork_messages` 交给 worker。默认 `fork:false`，完全走原路径。

**Tech Stack:** Rust 2021/2024 workspace，tokio，serde/serde_json，base64 0.22（分块载荷编码），SQLite（rusqlite，仅既有持久化，fork 载荷本身不落盘）。

**Spec:** `docs/superpowers/specs/2026-10-02-subagent-context-fork-design.md`

## Global Constraints

- **默认不 fork：** `fork` 缺省 `false`；`fork:false` 时首个 provider 请求必须**只有** objective，与今天逐字节一致（回归判据）。
- **不按大小截断历史：** 决策 4。唯一的裁剪是「末尾悬空 `tool_use` 的协议有效性裁剪」（与 `safe_cancel_truncate_len` 同规则）。
- **上限是拒绝、不是截断：** fork 载荷上限经 `YI_AGENT_FORK_MAX_BYTES` 配置，缺省 `32 * 1024 * 1024`（32 MiB）。超限时 `BeginForkUpload` 明确拒绝。
- **不动帧上限：** `MAX_FRAME_BYTES`（`ipc.rs:38`）保持 1 MiB 不变；每个 chunk 必须远小于它（chunk 原始切片 256 KiB）。
- **IPC 向后兼容：** 所有新增 `IpcRequest` 字段一律 `#[serde(default)]`，老客户端省略即行为不变。
- **显式失败：** `fork:true` 但无可用调用者上下文、上传不完整、token 过期/未知/越权——一律返回明确错误，绝不静默回退到无 fork。
- **会话 Arc 稳定不变量：** 同一 thread 的 `Arc<Mutex<Session>>` 在所有 Agent 重建路径（换工具集、compact、`/clear` 之外的普通重建）上保持同一实例；重建必须用 `with_session_arc(agent.session_handle())`，会话内容替换必须用 `Agent::set_session_messages`。
- **提交规范：** 每个 task 结束在 worktree 内 `cd yi-agent-rs && cargo fmt --all`，commit message 用 conventional commits，**不写** `Co-Authored-By` 行。
- **测试命令：** 按 crate 跑，不要 `--workspace` 全量。跑前 `ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent"` 确认无残留进程。

---

## File Structure

| 文件 | 职责 | 改动 |
|---|---|---|
| `yi-agent-rs/crates/yi-agent-core/src/agent.rs` | Agent/Session 基础 | 新增 `session_handle`、`with_session_arc`、`set_session_messages`、`Session::from_messages`；把 `safe_cancel_truncate_len` 提炼出 `fork_prefix_len` 并公开 `forkable_messages` |
| `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs` | worker 启动契约 | `WorkerStart` 增加 `fork_messages: Option<Vec<Message>>` + builder |
| `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs` | 任务树/worker 启动 | 新增 `set_fork_messages`/`fork_messages` 存储，`start_worker_with_provider_turn_gate` 透传 |
| `yi-agent-rs/crates/yi-agent-store/src/runtime.rs` | 协调器 | fork 上传状态机（begin/append/abort/consume）、上限、spawn 消费 token |
| `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` | 线协议与分发 | 3 个新 `IpcRequest` + 2 个新 `IpcResponse`；`SpawnChild`/`SpawnApplicationChild` 增 `fork_token` |
| `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs` | 工具 + worker 工厂 | `CallerContext`；spawn 工具 `fork` 参数 + 上传客户端；worker 工厂预置 fork 会话并绑定自身 caller |
| `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` | 桌面装配 | 绑定 caller；重建/compact 路径改用 `with_session_arc`/`set_session_messages` |
| `yi-agent-rs/crates/yi-agent/src/main.rs`、`src/tui/subagents.rs` | CLI/TUI 装配 | 同上 |
| `docs/project-management/*.md`、`README.md` | 项目进度 | 记录本 feature 与计数 |

---

### Task 1: core 会话句柄与 fork 裁剪

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`（`Session` ~39-80；`Agent` ~466-535；裁剪 ~668-690）
- Modify: `yi-agent-rs/crates/yi-agent-core/src/lib.rs:13-16`（re-export 新符号）
- Test: `yi-agent-core/src/agent.rs` 的 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: 既有 `Session { messages: Vec<Message> }`、`Message`、`has_tool_use`。
- Produces:
  - `Session::from_messages(messages: Vec<Message>) -> Session`
  - `Agent::session_handle(&self) -> Arc<Mutex<Session>>`
  - `Agent::with_session_arc(self, session: Arc<Mutex<Session>>) -> Self`
  - `Agent::set_session_messages(&mut self, messages: Vec<Message>)`
  - `pub fn forkable_messages(session: &Session) -> Vec<Message>`
  - `pub fn fork_prefix_len(session: &Session) -> usize`

- [ ] **Step 1: 写失败测试**

在 `agent.rs` 的 `mod tests` 添加（`safe_prefix_len` 的三条边界 + Arc 稳定性）：

```rust
#[test]
fn fork_prefix_drops_a_trailing_unpaired_tool_use() {
    use crate::message::{ContentBlock, Message};

    let mut session = Session::new();
    session.push(Message::user("hi"));
    session.push(Message::assistant(vec![ContentBlock::Text("working".into())]));
    // 末尾 assistant(tool_use) 尚未配对，fork 必须丢掉它
    session.push(Message::assistant(vec![ContentBlock::ToolUse {
        id: "call-1".into(),
        name: "spawn_agent".into(),
        input: serde_json::json!({"task": "x", "fork": true}),
    }]));

    assert_eq!(fork_prefix_len(&session), 2, "unpaired tool_use must be dropped");
    assert_eq!(forkable_messages(&session).len(), 2);
}

#[test]
fn fork_prefix_keeps_a_paired_tool_round_trip() {
    use crate::message::{ContentBlock, Message};

    let mut session = Session::new();
    session.push(Message::user("hi"));
    session.push(Message::assistant(vec![ContentBlock::ToolUse {
        id: "call-1".into(),
        name: "bash".into(),
        input: serde_json::json!({"command": "ls"}),
    }]));
    session.push(Message::tool_results(vec![ContentBlock::ToolResult {
        tool_use_id: "call-1".into(),
        content: vec![ContentBlock::Text("ok".into())],
        is_error: false,
    }]));

    assert_eq!(fork_prefix_len(&session), 3, "a paired round-trip is complete history");
}

#[test]
fn session_handle_stays_the_same_across_with_session_arc() {
    use crate::message::Message;

    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(vec![])),
        Arc::new(crate::ToolRegistry::new()),
        AgentConfig::default(),
    );
    let handle = agent.session_handle();
    handle.lock().unwrap().push(Message::user("kept"));

    let rebuilt = agent.with_session_arc(handle.clone());
    assert!(
        Arc::ptr_eq(&handle, &rebuilt.session_handle()),
        "with_session_arc must reuse the caller's Arc, not mint a new one"
    );
    assert_eq!(rebuilt.session().messages().len(), 1);
}
```

如果 `ScriptedProvider` 在该文件的 `mod tests` 内不可直接复用（例如需要构造参数），实现者可在同一 `mod tests` 内建一个最小 provider：`call_stream` 返回 `futures::stream::iter([ProviderEvent::Stop { reason: StopReason::EndTurn }]).boxed()`。

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib fork_prefix_drops_a_trailing_unpaired_tool_use`
Expected: 编译失败（`fork_prefix_len` / `session_handle` / `with_session_arc` 未定义）。

- [ ] **Step 3: 实现**

`Session` 增加：

```rust
pub fn from_messages(messages: Vec<Message>) -> Self {
    Self { messages, last_input_tokens: None }
}
```
（`Session` 目前有私有字段 `last_input_tokens`，同文件内可直接构造；若已有 `replace_messages` 则 `from_messages` 内部复用它。）

把 `agent.rs:678` 的裁剪提炼为公开函数，并让 `safe_cancel_truncate_len` 复用它：

```rust
/// How many leading messages of `session` form a provider-valid transcript:
/// everything, minus a trailing assistant message whose `tool_use` has no
/// matching `tool_result` (which the provider would reject).
pub fn fork_prefix_len(session: &Session) -> usize {
    let messages = session.messages();
    match messages.last() {
        Some(last) if last.role == Role::Assistant && has_tool_use(last) => messages.len() - 1,
        _ => messages.len(),
    }
}

/// The messages safe to seed a forked child session with.
pub fn forkable_messages(session: &Session) -> Vec<Message> {
    session.messages()[..fork_prefix_len(session)].to_vec()
}

fn safe_cancel_truncate_len(session: &Session) -> usize {
    fork_prefix_len(session)
}
```

`Agent` 增加：

```rust
/// The live session handle, so a tool can read the caller's *current*
/// conversation at call time. Rebuilds must reuse this Arc (see
/// `with_session_arc`) or a held handle goes stale.
pub fn session_handle(&self) -> Arc<Mutex<Session>> {
    Arc::clone(&self.session)
}

/// Rebuild the agent around an existing session handle, preserving identity.
pub fn with_session_arc(self, session: Arc<Mutex<Session>>) -> Self {
    Self { session, ..self }
}

/// Replace the session's contents in place, keeping the Arc (and therefore any
/// held handle) valid. Used by compaction instead of swapping sessions.
pub fn set_session_messages(&mut self, messages: Vec<Message>) {
    self.session.lock().unwrap().replace_messages(messages);
}
```

`lib.rs` re-export 增加 `forkable_messages`、`fork_prefix_len`。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib agent::tests`
Expected: PASS（含既有 `safe_cancel_truncate_len` 相关测试，证明提炼未改变 cancel 语义）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-core/src/agent.rs yi-agent-rs/crates/yi-agent-core/src/lib.rs
git commit -m "feat(core): expose a live session handle and forkable-history trimming"
```

---

### Task 2: core worker 启动契约与 supervisor 透传

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs:68-197`（`WorkerStart`）
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs`（存储 + `start_worker_with_provider_turn_gate` ~582-626）
- Test: 两文件各自的 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: Task 1 的 `Message`（`yi_agent_core::Message`）。
- Produces:
  - `WorkerStart.fork_messages: Option<Vec<Message>>`、`with_fork_messages`、`maybe_with_fork_messages`
  - `AgentSupervisor::set_fork_messages(&mut self, task: &TaskId, messages: Vec<Message>)`
  - `AgentSupervisor::fork_messages(&self, task: &TaskId) -> Option<Vec<Message>>`（私有或 pub，供 start 内部读取）

- [ ] **Step 1: 写失败测试**

`worker.rs` 的 tests：

```rust
#[test]
fn worker_start_carries_fork_messages_and_defaults_to_none() {
    let default = WorkerStart::new(TaskId::new(), AttemptId::new(), RootSessionId::new());
    assert!(default.fork_messages.is_none(), "fork is opt-in");

    let forked = WorkerStart::new(TaskId::new(), AttemptId::new(), RootSessionId::new())
        .with_fork_messages(vec![Message::user("parent said")]);
    assert_eq!(forked.fork_messages.unwrap().len(), 1);
}
```

`supervisor.rs` 的 tests（仿 `set_inherited_sandbox` 用例）：

```rust
#[test]
fn supervisor_remembers_fork_messages_per_task() {
    let mut supervisor = AgentSupervisor::new_with_objective(RootSessionId::new(), "obj".into());
    let child = supervisor
        .spawn_with_objective(
            supervisor.root_task_id().clone(),
            crate::subagent::worker::SpawnRequest::new(
                "child".into(),
                ChildWriteMode::ReadOnly,
                None,
            ),
        )
        .unwrap();

    supervisor.set_fork_messages(&child, vec![Message::user("inherited")]);
    assert_eq!(supervisor.fork_messages(&child).unwrap().len(), 1);
    assert!(supervisor.fork_messages(&supervisor.root_task_id().clone()).is_none());
}
```
（若 `root_task_id()` 名称不同，用该文件既有测试里获取 root id 的写法。）

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib worker_start_carries_fork_messages`
Expected: FAIL（`fork_messages` 字段/`with_fork_messages` 不存在）。

- [ ] **Step 3: 实现**

`worker.rs`：`WorkerStart` 增加字段（`use crate::message::Message;`）：

```rust
/// The caller's conversation, when the parent asked for a fork. The worker
/// seeds its session with these messages before adding `objective`.
pub fork_messages: Option<Vec<Message>>,
```
`new()` 初始化为 `None`；新增 builder：

```rust
pub fn with_fork_messages(mut self, messages: Vec<Message>) -> Self {
    self.fork_messages = Some(messages);
    self
}

pub fn maybe_with_fork_messages(mut self, messages: Option<Vec<Message>>) -> Self {
    self.fork_messages = messages;
    self
}
```

`supervisor.rs`：仿 `inherited_sandbox` 增加存储字段 `fork_messages: Mutex<HashMap<TaskId, Vec<Message>>>`（在 `AgentSupervisor` 构造处初始化空表），加 `set_fork_messages` / `fork_messages`，并在 `start_worker_with_provider_turn_gate`（`supervisor.rs:603-621`）的 builder 链追加：

```rust
.maybe_with_fork_messages(self.fork_messages(task_id))
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib subagent::`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs
git commit -m "feat(core): let a worker start from a forked conversation prefix"
```

---

### Task 3: store 协调器的分块上传状态机

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`（`RuntimeCoordinator` ~154-175；`open`；新增方法）
- Modify: `yi-agent-rs/crates/yi-agent-store/Cargo.toml`（`base64 = "0.22"`）
- Test: `yi-agent-store/tests/subagent_fork_upload.rs`（新建）

**Interfaces:**
- Consumes: 无（自足）。
- Produces:
  - `pub const DEFAULT_FORK_MAX_BYTES: u64 = 32 * 1024 * 1024;`
  - `RuntimeCoordinator::begin_fork_upload(&self, session: &RootSessionId, caller: &TaskId, capability: &str, total_bytes: u64) -> Result<String, RuntimeCoordinatorError>`（返回 `fork_token`）
  - `RuntimeCoordinator::append_fork_chunk(&self, token: &str, seq: u64, data: &str) -> Result<u64, RuntimeCoordinatorError>`（返回已收字节数）
  - `RuntimeCoordinator::abort_fork_upload(&self, token: &str) -> Result<(), RuntimeCoordinatorError>`
  - `RuntimeCoordinator::take_fork_messages(&self, token: &str, session: &RootSessionId, caller: &TaskId) -> Result<Vec<Message>, RuntimeCoordinatorError>`
  - `RuntimeCoordinator::with_fork_max_bytes(self, bytes: u64) -> Self`（测试用）

**上限定死：** `open()` 读环境变量 `YI_AGENT_FORK_MAX_BYTES`（解析失败则用 `DEFAULT_FORK_MAX_BYTES`），存入新字段 `fork_max_bytes: u64`。

- [ ] **Step 1: 写失败测试**

新建 `yi-agent-store/tests/subagent_fork_upload.rs`（用与 `tests/subagent_runtime_e2e.rs` 相同的 coordinator 搭建方式）：

```rust
// 目标：完整上传 -> take 成功；不完整 -> 拒绝；重复 seq -> 拒绝；超上限 begin -> 拒绝；abort 后 token 失效。

#[test]
fn a_complete_upload_is_accepted_and_consumed_once() {
    // 1. 建 coordinator + session + 取得 root 的 capability（沿用既有测试 helper）
    // 2. 构造两条消息，序列化 JSON -> 字节 -> base64 文本
    let payload = serde_json::to_string(&vec![Message::user("parent"), Message::user("more")]).unwrap();
    let encoded = base64::engine::general_purpose::STANDARD.encode(payload.as_bytes());
    // 3. total 用原始字节数
    let token = coordinator
        .begin_fork_upload(&session, &root, &capability, payload.len() as u64)
        .unwrap();
    // 4. 一次 append（测试用小载荷）
    let received = coordinator.append_fork_chunk(&token, 0, &encoded).unwrap();
    assert_eq!(received, payload.len() as u64);
    // 5. take
    let messages = coordinator.take_fork_messages(&token, &session, &root).unwrap();
    assert_eq!(messages.len(), 2);
    // 6. 一次性：再次 take 必须失败
    assert!(coordinator.take_fork_messages(&token, &session, &root).is_err());
}

#[test]
fn an_incomplete_upload_is_rejected() {
    // begin(total=10) 后只 append 4 字节的 base64，take 必须 Err
}

#[test]
fn a_duplicate_or_out_of_order_seq_is_rejected() {
    // append seq=1 而当前已收 0 个 chunk -> Err
}

#[test]
fn a_total_over_the_cap_is_rejected_at_begin() {
    // with_fork_max_bytes(4) 后 begin(total=5) -> Err
}

#[test]
fn an_aborted_upload_has_no_live_token() {
    // begin -> abort -> append/take 皆 Err
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test subagent_fork_upload`
Expected: 编译失败（方法未定义）。

- [ ] **Step 3: 实现**

`runtime.rs` 增加：

```rust
pub const DEFAULT_FORK_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// One in-flight fork payload. Bytes arrive base64-encoded, one chunk per
/// request, and are only decoded once the expected length has arrived.
struct ForkUpload {
    session_id: RootSessionId,
    caller_task_id: TaskId,
    expected_bytes: u64,
    next_seq: u64,
    received: Vec<u8>,
}
```

`RuntimeCoordinator` 增加字段 `fork_uploads: Mutex<HashMap<String, ForkUpload>>` 与 `fork_max_bytes: u64`（`open()` 里从 env 读取，默认 `DEFAULT_FORK_MAX_BYTES`）。方法：

- `begin_fork_upload`：`self.authorize_application_root(session, caller, capability)?`；`total_bytes > self.fork_max_bytes` → `Err(Spawn(...))` 或新的 `RuntimeCoordinatorError::ForkTooLarge { total: u64, max: u64 }`；`Uuid::new_v4().to_string()` 作 token 存入。
- `append_fork_chunk`：取 token；`seq != next_seq` → Err；base64 解码（失败 → Err）；`received.len() + chunk.len() > expected_bytes` → Err（防越界）；追加并 `next_seq += 1`；返回 `received.len() as u64`。
- `abort_fork_upload`：`remove`，未知 token 返回 Err。
- `take_fork_messages`：`remove` token；校验 `session_id`/`caller_task_id` 匹配、`received.len() == expected_bytes`；`serde_json::from_slice::<Vec<Message>>` → `Ok`。

新增错误变体（`runtime.rs:55-80` 的 `RuntimeCoordinatorError`）：

```rust
#[error("fork payload of {total} bytes exceeds the {max}-byte limit")]
ForkTooLarge { total: u64, max: u64 },
#[error("fork upload rejected: {0}")]
ForkUpload(String),
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test subagent_fork_upload`
Expected: 全部 PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-store/src/runtime.rs yi-agent-rs/crates/yi-agent-store/Cargo.toml yi-agent-rs/crates/yi-agent-store/tests/subagent_fork_upload.rs
git commit -m "feat(store): add a chunked fork-payload upload state machine"
```

---

### Task 4: store IPC 协议与 spawn 消费 fork token

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`（`IpcRequest` ~165-225；`IpcResponse` ~429+；dispatch ~3101）
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`（`spawn_child_with_objective` 1288-1391、`spawn_child_and_admit` 1409、`spawn_application_child` 1107）
- Test: `yi-agent-store/tests/subagent_fork_ipc.rs`（新建）+ `ipc.rs` 的 `mod tests`

**Interfaces:**
- Consumes: Task 3 的 coordinator 方法与 `take_fork_messages`；Task 2 的 `supervisor.set_fork_messages`。
- Produces:
  - `IpcRequest::BeginForkUpload { session_id, caller_task_id, capability, total_bytes: u64 }`
  - `IpcRequest::AppendForkChunk { fork_token: String, seq: u64, data: String }`
  - `IpcRequest::AbortForkUpload { fork_token: String }`
  - `IpcResponse::ForkUploadStarted { fork_token: String }`
  - `IpcResponse::ForkChunkAccepted { received: u64 }`
  - `IpcResponse::ForkUploadAborted`
  - `SpawnChild.fork_token: Option<String>`、`SpawnApplicationChild.fork_token: Option<String>`
  - `spawn_child_and_admit` / `spawn_child_with_objective` / `spawn_application_child` 末尾新增参数 `fork_token: Option<String>`

- [ ] **Step 1: 写失败测试**

`ipc.rs` 的 tests 加协议兼容断言：

```rust
#[test]
fn fork_upload_requests_round_trip_over_the_wire() {
    let request = IpcRequest::AppendForkChunk {
        fork_token: "t".into(),
        seq: 0,
        data: "aGk=".into(),
    };
    let encoded = serde_json::to_string(&request).unwrap();
    assert_eq!(serde_json::from_str::<IpcRequest>(&encoded).unwrap(), request);
}

#[test]
fn an_older_spawn_request_without_fork_token_still_deserializes() {
    let json = r#"{"type":"SpawnApplicationChild","session_id":"s","parent_task_id":"p",
        "capability":"c","objective":"o"}"#;
    let parsed: IpcRequest = serde_json::from_str(json).unwrap();
    assert!(matches!(
        parsed,
        IpcRequest::SpawnApplicationChild { fork_token: None, .. }
    ));
}
```

新建 `yi-agent-store/tests/subagent_fork_ipc.rs`：起真实 daemon（沿用 `tests/runtime_ipc.rs` 的 `Daemon::start` 方式），走完整链路：`BeginForkUpload` → `AppendForkChunk` → `SpawnApplicationChild { fork_token }`，断言返回 `TaskSpawned`；再断言 `SpawnApplicationChild { fork_token: Some("bogus") }` 被拒。

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --lib fork_upload_requests_round_trip`
Expected: FAIL（变体不存在）。

- [ ] **Step 3: 实现**

`ipc.rs`：按上面 Produces 增 `IpcRequest`/`IpcResponse` 变体（`fork_token` 一律 `#[serde(default)]`）。dispatch（`ipc.rs:3101` 的 `match request`）新增三臂，调用 `coordinator.begin_fork_upload/append_fork_chunk/abort_fork_upload`，并把错误经既有 `IpcError`/`error_response` 映射（`ForkTooLarge`/`ForkUpload` → `IpcErrorCode::Validation`）。`SpawnChild`/`SpawnApplicationChild` 两臂把 `fork_token` 透传给 coordinator 的 spawn 方法。

`runtime.rs`：三个 spawn 方法末尾加 `fork_token: Option<String>`。在 `spawn_child_with_objective` 中，对 `thread_id`/`inherited_sandbox` 既有的「先解析、后落库」处（1314-1352）并行处理 fork：

```rust
let fork_messages = match fork_token.as_deref() {
    Some(token) => Some(self.take_fork_messages(token, session, parent)?),
    None => None,
};
```
然后在该方法内、`supervisor` 被锁定的区块里（`set_inherited_sandbox` 附近，1350-1352）写入：

```rust
if let Some(fork) = fork_messages {
    supervisor.set_fork_messages(&child, fork);
}
```
`spawn_child_and_admit` 与 `spawn_application_child` 仅透传新参数。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --lib ipc:: && cargo test -p yi-agent-store --test subagent_fork_ipc`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-store/src/ipc.rs yi-agent-rs/crates/yi-agent-store/src/runtime.rs yi-agent-rs/crates/yi-agent-store/tests/subagent_fork_ipc.rs
git commit -m "feat(store): carry a forked context through spawn over IPC"
```

---

### Task 5: subagent 的 CallerContext 活句柄与注册接线

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`（工具结构体 ~1450+、~1815+；`register_*` ~1270-1340）
- Test: `yi-agent-subagent/src/lib.rs` 的 `mod tests`

**Interfaces:**
- Consumes: Task 1 的 `Agent::session_handle`、`forkable_messages`。
- Produces:
  - `pub struct CallerContext`（`Clone`，内部 `Arc<Mutex<Option<Arc<Mutex<Session>>>>>`）
  - `CallerContext::unbound() -> Self`、`CallerContext::new(Arc<Mutex<Session>>) -> Self`
  - `CallerContext::bind(&self, Arc<Mutex<Session>>)`
  - `CallerContext::snapshot(&self) -> Option<Vec<Message>>`（返回 `forkable_messages`）
  - `register_attached_root_tools_in_thread(..., caller: CallerContext)` 与 `register_application_subagent_tools_in_thread(..., caller: CallerContext)` 增加最后参数；其余三个薄封装 `register_attached_root_tools` / `register_application_subagent_tools` 增加同名参数

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn a_bound_caller_context_snapshots_the_live_session() {
    let session = Arc::new(Mutex::new(Session::new()));
    let caller = CallerContext::new(Arc::clone(&session));
    session.lock().unwrap().push(Message::user("parent history"));

    let snapshot = caller.snapshot().expect("bound context yields a snapshot");
    assert_eq!(snapshot.len(), 1);

    // 活句柄：绑定后新增的消息，下一次 snapshot 必须看得到
    session.lock().unwrap().push(Message::user("later"));
    assert_eq!(caller.snapshot().unwrap().len(), 2);
}

#[test]
fn an_unbound_caller_context_has_no_snapshot() {
    assert!(CallerContext::unbound().snapshot().is_none());
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib caller_context`
Expected: FAIL（类型未定义）。

- [ ] **Step 3: 实现**

```rust
use std::sync::Mutex;

/// The live conversation of whoever is calling a delegation tool.
///
/// Held by the tools and re-resolved on every call, so a rebuilt Agent (new
/// tool registry, post-compaction session) never leaves a tool reading a stale
/// transcript. `bind` is called by the host right after it builds the Agent.
#[derive(Clone, Default)]
pub struct CallerContext {
    session: Arc<Mutex<Option<Arc<Mutex<Session>>>>>,
}

impl CallerContext {
    pub fn unbound() -> Self { Self::default() }

    pub fn new(session: Arc<Mutex<Session>>) -> Self {
        Self { session: Arc::new(Mutex::new(Some(session))) }
    }

    pub fn bind(&self, session: Arc<Mutex<Session>>) {
        *self.session.lock().unwrap() = Some(session);
    }

    /// The caller's provider-valid transcript, or `None` when unbound.
    pub fn snapshot(&self) -> Option<Vec<Message>> {
        let guard = self.session.lock().unwrap();
        guard
            .as_ref()
            .map(|session| yi_agent_core::forkable_messages(&session.lock().unwrap()))
    }
}
```

两个 spawn 工具结构体（`DaemonApplicationSpawnAgentTool`、`DaemonSpawnAgentTool`）增加 `caller: CallerContext` 字段；`register_*_in_thread` 增加 `caller: CallerContext` 参数并传入两个 spawn 工具（其余四个工具不需要）。薄封装同样加参。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib`
Expected: PASS（既有 `application_tools_share_one_root` 等需按新签名补 `CallerContext::unbound()`）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-subagent/src/lib.rs
git commit -m "feat(subagent): add a live caller-context handle for delegation tools"
```

---

### Task 6: subagent 的 `fork` 参数与分块上传客户端

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`（两个 spawn 工具的 schema 与 `call`）
- Modify: `yi-agent-rs/crates/yi-agent-subagent/Cargo.toml`（`base64 = "0.22"`）
- Test: `yi-agent-subagent/src/lib.rs` 的 `mod tests`

**Interfaces:**
- Consumes: Task 4 的 IPC 变体；Task 5 的 `CallerContext`。
- Produces:
  - 两个 spawn 工具 schema 含 `fork`（`boolean`，`default: false`）
  - 内部 helper `fn fork_upload(...) -> Result<Option<String/*fork_token*/>, ToolResult>`：读取 `snapshot()`，序列化、切块（原始 256 KiB/块）、base64、逐块 `AppendForkChunk`，返回 token

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn both_spawn_schemas_offer_fork_and_default_to_false() {
    for schema in [application_spawn_schema(), core_spawn_schema()] {
        assert_eq!(schema["properties"]["fork"]["type"], "boolean");
        assert_eq!(schema["properties"]["fork"]["default"], false);
        assert_eq!(schema["required"], json!(["task"]), "fork must stay optional");
    }
}

#[tokio::test]
async fn fork_requested_without_a_bound_caller_fails_explicitly() {
    let tool = /* DaemonSpawnAgentTool with caller: CallerContext::unbound(), root: test_root(...) */;
    let result = tool.call(json!({"task": "do it", "fork": true})).await;
    assert!(result.is_error);
    assert!(matches!(
        result.content.as_slice(),
        [yi_agent_core::ContentBlock::Text(text)]
            if text.contains("fork requested but no caller context available")
    ));
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib fork_requested_without`
Expected: FAIL（`fork` 未处理）。

- [ ] **Step 3: 实现**

schema 追加（两个副本都要）：

```json
"fork": {
  "type": "boolean",
  "default": false,
  "description": "Fork the caller's current conversation into the child so it inherits context. Off by default: pass true only when the child genuinely needs the history."
}
```

`call` 中，在解析 `task`/`mode`/`model`/`workdir`/`sandbox` 之后、发起 spawn 之前：

```rust
let fork = args.get("fork").and_then(Value::as_bool).unwrap_or(false);
let fork_token = if fork {
    match upload_forked_context(&handle.socket_path, &handle, &self.caller) {
        Ok(token) => Some(token),
        Err(error) => return error,
    }
} else {
    None
};
```

`upload_forked_context`：

```rust
const FORK_CHUNK_BYTES: usize = 256 * 1024;

fn upload_forked_context(
    socket: &Path,
    handle: &RuntimeHandle,
    caller: &CallerContext,
) -> Result<String, ToolResult> {
    let Some(messages) = caller.snapshot() else {
        return Err(ToolResult::error("fork requested but no caller context available"));
    };
    let payload = serde_json::to_vec(&messages)
        .map_err(|error| ToolResult::error(format!("could not encode forked context: {error}")))?;
    let total = payload.len() as u64;
    let token = match send_request(socket, IpcRequest::BeginForkUpload {
        session_id: handle.session_id.clone(),
        caller_task_id: handle.task_id.clone(),
        capability: handle.capability.clone(),
        total_bytes: total,
    }) {
        Ok(IpcResponse::ForkUploadStarted { fork_token }) => fork_token,
        Ok(other) => return Err(ToolResult::error(format_ipc_rejection("fork upload", &other))),
        Err(error) => return Err(ToolResult::error(format!("daemon is unavailable: {error}"))),
    };
    for (seq, chunk) in payload.chunks(FORK_CHUNK_BYTES).enumerate() {
        let data = base64::engine::general_purpose::STANDARD.encode(chunk);
        match send_request(socket, IpcRequest::AppendForkChunk {
            fork_token: token.clone(),
            seq: seq as u64,
            data,
        }) {
            Ok(IpcResponse::ForkChunkAccepted { .. }) => {}
            Ok(other) => return Err(ToolResult::error(format_ipc_rejection("fork chunk", &other))),
            Err(error) => {
                let _ = send_request(socket, IpcRequest::AbortForkUpload { fork_token: token.clone() });
                return Err(ToolResult::error(format!("daemon is unavailable: {error}")));
            }
        }
    }
    Ok(token)
}
```

把 `fork_token` 填入 `SpawnChild` / `SpawnApplicationChild` 请求。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-subagent/src/lib.rs yi-agent-rs/crates/yi-agent-subagent/Cargo.toml
git commit -m "feat(subagent): upload the caller's context when fork is requested"
```

---

### Task 7: subagent worker 预置 fork 会话并绑定自身 caller

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`（worker 工厂 `start_with_provider_turn_gate` ~536-670）
- Test: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs` 的 `mod tests`

**Interfaces:**
- Consumes: Task 2 的 `WorkerStart.fork_messages`；Task 1 的 `Session::from_messages`、`Agent::session_handle`、`with_session_arc`；Task 5 的 `CallerContext`。
- Produces: 无新公开符号（行为变更）。

- [ ] **Step 1: 写失败测试**

仿既有 `daemon_worker_reports_text_completion_without_a_workspace_delivery`，用 `RecordingProvider` 捕获首个请求：

```rust
#[tokio::test]
async fn a_forked_worker_seeds_its_first_request_with_the_parent_history() {
    let directory = TempDir::new().unwrap();
    let provider = Arc::new(RecordingProvider::default());
    let factory = DaemonAgentWorkerFactory::new(
        provider.clone(),
        Arc::new(ToolRegistry::new()),
        AgentConfig::default(),
        directory.path().join("runtime.sock"),
    )
    .with_workspace(directory.path().to_path_buf());
    let request = WorkerStart::new(TaskId::new(), AttemptId::new(), RootSessionId::new())
        .with_objective("now continue the task")
        .with_workspace(worker_workspace(directory.path()))
        .with_workspace_mode(ChildWriteMode::ReadOnly)
        .with_fork_messages(vec![
            yi_agent_core::Message::user("earlier the user asked for X"),
        ]);
    let handle = factory.start(request).await.unwrap();
    wait_until(|| !provider.requests.lock().unwrap().is_empty(), "the forked worker to call the provider").await;

    let requests = provider.requests.lock().unwrap();
    let texts: Vec<&str> = requests[0]
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            yi_agent_core::ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert!(texts.iter().any(|t| t.contains("earlier the user asked for X")), "parent history must be a prefix: {texts:?}");
    assert!(texts.iter().any(|t| t.contains("now continue the task")), "objective must follow: {texts:?}");
    handle.cancel();
}
```

并加一条回归：不带 `fork_messages` 时首个请求**不含**父历史（既有测试已覆盖「只有 objective」，此处断言 `requests[0].messages.len() == 1` 即可）。

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib a_forked_worker_seeds`
Expected: FAIL（尚无预置逻辑，父历史缺失）。

- [ ] **Step 3: 实现**

在 `start_with_provider_turn_gate` 解构 `let fork_messages = request.fork_messages;` 并移入线程闭包。在 `Agent::new(...)` 之后（`lib.rs:646`）预置：

```rust
let mut agent = Agent::new(provider, worker_tools, config);
if let Some(fork) = fork_messages {
    agent = agent.with_session(yi_agent_core::Session::from_messages(fork));
}
```

同时把子 worker 自己的委派工具绑定到其实时会话（子 agent fork 自己时用）：把 `CallerContext` 在闭包外创建、`Arc` 传入工具，闭包内 Agent 建好后绑定：

```rust
let worker_caller = CallerContext::unbound();
// 构造 worker_root 时把 worker_caller.clone() 交给两个 spawn 工具
...
let mut agent = Agent::new(provider, worker_tools, config);
if let Some(fork) = fork_messages {
    agent = agent.with_session(yi_agent_core::Session::from_messages(fork));
}
worker_caller.bind(agent.session_handle());
```

**顺序必须如此**：先 `with_session`（决定会话内容）再 `bind`，否则句柄指向替换掉的旧 Arc。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-subagent/src/lib.rs
git commit -m "feat(subagent): start a forked worker from the caller's transcript"
```

---

### Task 8: app-server 绑定 caller 并保持会话 Arc 稳定

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`build_runtime_tooling` 696-731、`RuntimeTooling` 结构、`rebuild_thread_agent_with_theme` 772-810、`wrap_for_delegation` 811+、compact 3599/3625、`apply_session` 3242）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` 的 `mod tests`

**Interfaces:**
- Consumes: Task 5 的 `CallerContext`；Task 1 的 `with_session_arc`/`set_session_messages`/`session_handle`。
- Produces: `RuntimeTooling` 增加 `caller: CallerContext` 字段。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn rebuilding_a_thread_agent_keeps_one_session_handle() {
    // 用 test fixture 建一个 built agent（含委派工具与 caller），记录 handle A；
    // 走 wrap_for_delegation / rebuild_thread_agent_with_theme 重建；
    // 断言重建后 handle 与 A Arc::ptr_eq。
    // 并断言 tooling.caller.snapshot() 在重建后仍能看到重建前写入的消息。
}

#[test]
fn compaction_replaces_messages_without_changing_the_handle() {
    // 建 agent，记录 handle；调用 set_session_messages(compacted)；
    // 断言 handle 未变且 snapshot 反映新内容。
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib rebuilding_a_thread_agent_keeps`
Expected: FAIL。

- [ ] **Step 3: 实现**

- `RuntimeTooling` 增 `caller: CallerContext`；`build_runtime_tooling` 里 `let caller = CallerContext::unbound();` 并在注册委派工具时传入 `caller.clone()`，返回它。
- 首次构建 agent 后调用 `tooling.caller.bind(agent.session_handle())`。
- 所有重建路径（`wrap_for_delegation` 811、`rebuild_thread_agent_with_theme` 777-778、809、`apply_session` 3242）把 `with_session(...)` 换成 `agent.with_session_arc(agent.session_handle())`（或复用外部保存的 handle）。
- compact 路径（3599 的 `/clear`-style 与 3625）改用 `agent.set_session_messages(new_messages)`，不再 `with_session(Session::new())` / `with_session(compacted)`。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: PASS（含既有委派与 compact 测试）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): bind fork context to the live thread session"
```

---

### Task 9: TUI/headless 绑定 caller 并保持会话 Arc 稳定

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/subagents.rs`（注册调用 150/159 + 单测）
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`（773/773/885 注册点；1612-1768 重建点；1504 构建点）
- Test: `yi-agent/src/tui/subagents.rs` 与 `main.rs` 的既有测试

**Interfaces:**
- Consumes: Task 5 的 `CallerContext`；Task 1 的 `with_session_arc`/`set_session_messages`/`session_handle`。
- Produces: 无新公开符号。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn tui_registers_delegation_tools_bound_to_a_caller_context() {
    // 建一个 CallerContext::unbound()，注册工具，bind 一个 Session handle，
    // 断言 snapshot 可见（证明注册时传入了同一个 caller）。
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib tui_registers_delegation_tools_bound`
Expected: FAIL。

- [ ] **Step 3: 实现**

- `register_attached_root_tools` / `..._in_thread` 调用点传入 `CallerContext`（TUI 进程级单例 caller，在首次构建 agent 后 `bind`）。
- `main.rs` 的重建点（1634-1687、1760-1768）把 `with_session(...)` 换成 `with_session_arc(...)`；headless（`yi-agent run` 路径，`build_headless_root_tools`）同样处理。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent/src/tui/subagents.rs yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "feat(cli): bind fork context to the live session in the TUI and headless paths"
```

---

### Task 10: 端到端 fork 测试与项目进度文档

**Files:**
- Test: `yi-agent-rs/crates/yi-agent/tests/subagent_fork_e2e.rs`（新建，mock provider）
- Modify: `docs/project-management/yi-agent-subagent.md`、`docs/project-management/yi-agent-store.md`、`docs/project-management/yi-agent-core.md`
- Modify: `README.md`（模块索引计数，若计数变化）

**Interfaces:**
- Consumes: 全部前序 task。
- Produces: 无。

- [ ] **Step 1: 写端到端测试**

建一个可观测 provider 的 daemon（用 `Daemon::start_with_factory` 注入 RecordingProvider），在本进程内构造调用者 session（含一条历史消息），经 subagent 的注册工具调用 `spawn_agent{fork:true}`，等待 worker 首个 provider 请求，断言其 messages 含父历史 + objective。再跑一次 `fork:false`（或省略）断言首个请求只有 objective。

- [ ] **Step 2: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --test subagent_fork_e2e`
Expected: PASS。若该测试因需要真实 socket 环境而受限，退化为在 `yi-agent-subagent` 与 `yi-agent-store` 的集成测试中覆盖同一条链路，并在测试文件顶部注明原因。

- [ ] **Step 3: 更新项目进度文档**

- `docs/project-management/yi-agent-subagent.md` 的 Features 段新增本 feature（带可验证判据：`cargo test -p yi-agent-subagent --lib`、`cargo test -p yi-agent --test subagent_fork_e2e`），状态用 `[x]`。
- `docs/project-management/yi-agent-store.md` 记录 fork 上传协议（`cargo test -p yi-agent-store --test subagent_fork_upload`）。
- `docs/project-management/yi-agent-core.md` 记录会话句柄/裁剪原语。
- 若 README 的「完成 / 总计」计数变化，同步更新。

- [ ] **Step 4: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add -A
git commit -m "test(subagent): prove fork end to end and record project progress"
```

---

## Self-Review

**1. Spec coverage：**
- 决策 1/2（完整复制、显式可选）→ Task 6 的 `fork` 默认 false + Task 7 预置完整前缀。
- 决策 3（fork 直接调用者）→ Task 5 `CallerContext` + Task 7 worker 绑定自身 caller（子 agent fork 自己）。
- 决策 4（全量不按大小截断）+ 内存保护 → Task 3 的 32 MiB 上限「拒绝而非截断」；裁剪仅在 Task 1 的协议有效性裁剪。
- 决策 5（`fork` 布尔参数）→ Task 6 schema。
- 决策 6（现有 socket 分块上传）→ Task 3/4/6。
- 决策 7（活句柄）→ Task 5 + Task 8/9 的 Arc 稳定不变量。
- 决策 8/9（落点、末端裁剪）→ Task 1 `forkable_messages` + Task 7 顺序（前缀 → initial user messages → objective）。
- 错误处理表 → Task 3/6 的显式错误分支。
- 测试策略 → Task 1/3/4/6/7/8/9/10 各自判据。

**2. Placeholder scan：** 无 TBD/TODO；每个实现步骤给出可编译的签名与关键代码；测试步骤给出可运行命令与期望。

**3. Type consistency：** 全程统一使用 `fork_prefix_len`/`forkable_messages`、`CallerContext::snapshot`、`with_session_arc`、`set_session_messages`、`WorkerStart.fork_messages`、`fork_token: Option<String>`、`begin_fork_upload`/`append_fork_chunk`/`abort_fork_upload`/`take_fork_messages`；Task 4 的 spawn 参数顺序在多处一致。

**已知需实现者按现场校准的点（非占位符，均有明确规则）：** 各 `mod tests` 里测试 provider/fixture 的既有名称；`AgentSupervisor` 存储字段的既有风格；`ipc.rs` dispatch 的确切行号。
