# 子 Agent 观察入口实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让用户能进入任意子 agent，实时看到它的助手文本、工具调用与结果，并做发消息/取消两项干预；TUI 走 Ctrl+P 第三页签，桌面走子 agent 暂留区。

**Architecture:** 子 worker 的 agent loop 在消费 `AgentEvent` 时顺手把过程交给一个 `TraceAggregator`（写入前聚合：512 字节或 200ms 或结构边界才 flush），聚合结果经 `WorkerHandle` → `Supervisor` → `RuntimeCoordinator::reconcile_worker_events()`（既有 10ms 节奏）落到**独立的 `task_trace_events` 表**（不复用审计 `events` 表）。读取走两个新 IPC：`ReadTaskTrace`（快照回灌）与 `SubscribeTrace`（带游标的实时流），复用既有订阅的帧信封、身份校验与分片写。`tasks` 另加 `thread_id` 列以支持桌面按对话分组。

**Tech Stack:** Rust 2024、Tokio、serde、rusqlite（SQLite）、Unix socket IPC、ratatui（TUI）、React 19 + TypeScript + Vitest（desktop）。

**设计文档：** `docs/superpowers/specs/2026-10-01-subagent-observation-design.md`

## Global Constraints

- **严禁在 `main` 分支上提交。** 全程在 worktree
  `.worktrees/subagent-observation`（分支 `docs/subagent-observation`）里工作。若该 worktree
  已在用（本计划的前置 commit 是设计文档），续用同一分支即可。
- **提交前必须 `cd yi-agent-rs && cargo fmt --all`**；commit message 用 conventional
  commits，首行 ≤72 字符，**不要**写 `Co-Authored-By`。
- **不要并行跑 `cargo test`**：同一时刻只跑一个 cargo 命令。跑之前先
  `ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent"` 确认没有残留进程；残留则 kill。
  优先按 crate 跑（`-p yi-agent-core` / `-p yi-agent-store` / `-p yi-agent` /
  `-p yi-agent-app-server`），不要 `cargo test --workspace`。
- **轨迹不复用审计 `events` 表。** 审计事实（`RuntimeEvent`）必须持久不可删且是封闭枚举；
  轨迹量大、可裁剪、生命周期短。两者混表会互相拖累。见设计 §2.2。
- **默认行为零回归。** 不传 `thread_id`、没有轨迹订阅时，所有既有语义与今天完全一致。
- **前端不改动 `desktop/src-tauri/`**：桌面端是纯帧桥接，新增能力只走 app-server 的 JSON-RPC。
- **保留上限是硬要求**：每任务轨迹 ≤ 2000 行，终态后 24 小时清理。不允许无上限增长。

**仓内既有测试基建（本计划的测试示例引用它们，不要新造）：**

- 建任务：`RuntimeRepository::create_task_with_attempt(&task, &session, &attempt, 1, "running")`
  （`crates/yi-agent-store/src/repository.rs`）。
- 造 id：`RootSessionId::new()`、`yi_agent_core::TaskId::new()`、`yi_agent_core::AttemptId::new()`
  （见 `crates/yi-agent-store/tests/runtime_coordinator.rs:2141`）。
- 起 coordinator 与 worker：见 `tests/runtime_coordinator.rs` 顶部的 `start_with_provider_turn_gate`
  与 `MessageRecordingFactory`。

**常量（本计划各处引用，值先定，实现可调）：**

| 常量 | 值 | 位置 |
| --- | --- | --- |
| `TRACE_TEXT_FLUSH_BYTES` | `512` | `yi-agent-core/src/subagent/trace.rs` |
| `TRACE_TEXT_FLUSH_INTERVAL` | `200ms` | 同上 |
| `TRACE_TOOL_SUMMARY_LIMIT` | `1024` 字节 | 同上 |
| `TRACE_MAX_ROWS_PER_TASK` | `2000` | `yi-agent-store/src/repository.rs` |
| `TRACE_TERMINAL_RETENTION_SECS` | `86_400`（24h） | 同上 |

---

## 文件结构

**新增**

| 文件 | 职责 |
| --- | --- |
| `yi-agent-rs/crates/yi-agent-core/src/subagent/trace.rs` | `TraceFact` 与 `TraceAggregator`（纯逻辑，无 IO） |
| `yi-agent-rs/crates/yi-agent/src/tui/trace.rs` | TUI 轨迹视图状态机（列表 / 详情 / 下钻 / 动作） |
| `desktop/src/lib/subagents.ts` | 前端暂留区状态折叠（通知 → 子 agent 列表与轨迹行） |
| `desktop/src/components/SubagentRail.tsx` | 子 agent 暂留区（列表） |
| `desktop/src/components/SubagentTrace.tsx` | 轨迹详情（摘要 → 完整轨迹两级） |

**修改（按层）**

| 文件 | 改动 |
| --- | --- |
| `yi-agent-store/src/repository.rs` | schema v12（轨迹表 + `thread_id`）、轨迹读写 API、裁剪与清理 |
| `yi-agent-core/src/subagent/worker.rs` | `WorkerHandle::report_trace` / `take_trace_events` |
| `yi-agent-core/src/subagent/supervisor.rs` | `take_worker_trace_events` |
| `yi-agent-core/src/subagent/mod.rs` | 注册 `pub mod trace;` |
| `yi-agent-subagent/src/lib.rs` | worker 循环接入聚合器；spawn 请求带 `thread_id` |
| `yi-agent-store/src/runtime.rs` | reconcile 抽干落库；`thread_id` 继承；清理挂分钟 tick |
| `yi-agent-store/src/ipc.rs` | `ReadTaskTrace` / `SubscribeTrace` 及其客户端 helper |
| `yi-agent/src/tui/app.rs` | Ctrl+P 第三页签接线 |
| `yi-agent-app-server/src/server.rs` | `agent/children/list`、`agent/trace/read`、`agent/trace/watch` |
| `yi-agent-app-server/src/protocol.rs` | 上述方法的请求/响应/通知类型 |
| `desktop/src/lib/protocol.ts`、`desktop/src/App.tsx` | 协议类型与接线 |
| `docs/project-management/*.md`、`docs/bug-list.md` | 进度同步 |

**依赖顺序：** Task 1–3 独立 → Task 4 依赖 3 → Task 5 依赖 3、4 → Task 6 依赖 2、4 →
Task 7 独立 → Task 8 依赖 2 → Task 9 依赖 8 → Task 10、11 依赖 9 → Task 12 依赖 9 →
Task 13、14 依赖 12 → Task 15 最后。

---

## Phase 1：数据层

### Task 1: schema v12——轨迹表与 `thread_id` 列

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs`
- Test: 同文件 `mod tests`（本仓惯例）

**Interfaces:**
- Produces: 表 `task_trace_events(id, task_id, kind, payload_json, created_at)`、索引
  `task_trace_task_id_idx`；`tasks.thread_id TEXT`（nullable）；`LATEST_SCHEMA_VERSION = 12`。

- [ ] **Step 1: 写失败测试**

在 `repository.rs` 的 `mod tests` 里新增。本任务只断言**结构**（读写 API 属 Task 2），
所以测试直接查 schema，不依赖尚未存在的 API：

```rust
#[test]
fn schema_twelve_adds_the_trace_table_and_thread_id() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime.sqlite");
    let repository = RuntimeRepository::open(&path).unwrap();

    let connection = repository.connection_for_test();
    let table_exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'task_trace_events')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(table_exists);

    let has_thread_id: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('tasks') WHERE name = 'thread_id')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(has_thread_id);
    assert_eq!(LATEST_SCHEMA_VERSION, 12);
}
```

> `connection_for_test` 是本任务需要加的一个**仅测试可见**访问器
> （`#[cfg(test)] pub(crate) fn connection_for_test(&self) -> &Connection`），
> 供迁移断言直接查 schema。若仓内已有等价访问器，复用之，不要另造。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --lib schema_twelve_adds_the_trace_table_and_thread_id`
Expected: FAIL（`task_trace_events` 不存在 / `LATEST_SCHEMA_VERSION == 11`）

- [ ] **Step 3: 实现迁移**

`LATEST_SCHEMA_VERSION` 由 `11` 改为 `12`（`repository.rs:18`）。在 `migrate` 末尾追加
（仿 `if current_version < 11` 块，`repository.rs:4391`）：

```rust
if current_version < 12 {
    let transaction = connection.unchecked_transaction()?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_trace_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            task_id TEXT NOT NULL REFERENCES tasks(id),
            kind TEXT NOT NULL,
            payload_json TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX IF NOT EXISTS task_trace_task_id_idx ON task_trace_events(task_id, id);",
    )?;
    let has_column = transaction.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM pragma_table_info('tasks') WHERE name = 'thread_id'
         )",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if !has_column {
        transaction.execute_batch("ALTER TABLE tasks ADD COLUMN thread_id TEXT;")?;
    }
    transaction.execute("INSERT INTO schema_migrations (version) VALUES (12)", [])?;
    transaction.commit()?;
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --lib migrate`
Expected: PASS（含既有迁移测试全绿）

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-store/src/repository.rs
git commit -m "feat(store): add task_trace_events table and tasks.thread_id (schema v12)"
```

---

### Task 2: 轨迹读写 API、每任务裁剪、终态清理

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs`
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: Task 1 的表与列。
- Produces（供 Task 6、8 使用）：
  - `RuntimeRepository::append_trace(&mut self, task: &TaskId, fact: &TraceFact) -> Result<(), RepositoryError>`
  - `RuntimeRepository::trace_after(&self, task: &TaskId, after_id: i64) -> Result<Vec<PersistedTraceRow>, RepositoryError>`
  - `RuntimeRepository::trace_high_water(&self, task: &TaskId) -> Result<i64, RepositoryError>`
  - `RuntimeRepository::trace_row_count(&self, task: &TaskId) -> Result<i64, RepositoryError>`
  - `RuntimeRepository::trim_trace(&mut self, task: &TaskId) -> Result<(), RepositoryError>`（裁到 2000 行）
  - `RuntimeRepository::prune_terminal_traces(&mut self, now: DateTime<Utc>) -> Result<usize, RepositoryError>`
  - `RuntimeRepository::set_task_thread_id(&mut self, task: &TaskId, thread_id: &str) -> Result<(), RepositoryError>`
  - `RuntimeRepository::task_thread_id(&self, task: &TaskId) -> Result<Option<String>, RepositoryError>`
  - `pub struct PersistedTraceRow { pub id: i64, pub task_id: TaskId, pub kind: String, pub payload_json: String }`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn trace_rows_are_trimmed_to_the_per_task_ceiling() {
    let directory = tempfile::tempdir().unwrap();
    let mut repository = RuntimeRepository::open(&directory.path().join("runtime.sqlite")).unwrap();
    let (root, task, attempt) = seed_running_task(&mut repository);

    for index in 0..(TRACE_MAX_ROWS_PER_TASK + 5) {
        repository
            .append_trace(&task, &yi_agent_core::subagent::trace::TraceFact::StateNote { note: format!("step {index}") })
            .unwrap();
    }

    assert_eq!(repository.trace_row_count(&task).unwrap(), TRACE_MAX_ROWS_PER_TASK as i64);
    let rows = repository.trace_after(&task, 0).unwrap();
    // The newest rows survive; the oldest are gone.
    assert_eq!(rows.first().unwrap().payload_json.contains("step 5"), true);
    let _ = (session, attempt);
}

#[test]
fn terminal_traces_are_pruned_after_the_retention_window() {
    let directory = tempfile::tempdir().unwrap();
    let mut repository = RuntimeRepository::open(&directory.path().join("runtime.sqlite")).unwrap();
    let (root, task, attempt) = seed_running_task(&mut repository);
    repository.append_trace(&task, &yi_agent_core::subagent::trace::TraceFact::StateNote { note: "done".into() }).unwrap();

    let removed = repository
        .prune_terminal_traces(Utc::now() + chrono::Duration::seconds(TRACE_TERMINAL_RETENTION_SECS + 1))
        .unwrap();
    assert_eq!(removed, 1);
    assert_eq!(repository.trace_row_count(&task).unwrap(), 0);
    let _ = (session, attempt);
}
```

（测试里的 `seed_running_task` 按仓内既有基建拼出：`create_task_with_attempt(&task, &session, &attempt, 1, "running")`
再走既有状态推进把任务置为终态 `completed_no_changes`——清理只针对终态任务。若既有测试已有
等价 helper，复用之，不要另造。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --lib trace_rows_are_trimmed`
Expected: FAIL（`append_trace` 未定义）

- [ ] **Step 3: 实现**

要点：

- `append_trace` 在一个事务里：`INSERT INTO task_trace_events(task_id, kind, payload_json)`
  → 若该任务行数 > `TRACE_MAX_ROWS_PER_TASK`，删除最旧的 `rowid` 直到等于上限。
- `trace_after`：`SELECT id, task_id, kind, payload_json FROM task_trace_events
  WHERE task_id = ?1 AND id > ?2 ORDER BY id`。
- `prune_terminal_traces`：只删「所属任务处于终态」且
  `created_at < now - TRACE_TERMINAL_RETENTION_SECS` 的行；返回删除行数。终态判定复用
  既有的 `is_terminal_state`（`runtime.rs:3369` 附近有同类集合，若不在 repository 可见范围，
  在 repository 内按同一组状态名实现一个私有 helper 并在测试里钉住）。
- `set_task_thread_id` / `task_thread_id` 走已有的 `tasks` 表；未知任务返回
  `RepositoryError::TaskNotFound`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --lib trace`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-store/src/repository.rs
git commit -m "feat(store): trace row read/write with per-task trim and terminal prune"
```

---

### Task 3: `TraceFact` 与 `TraceAggregator`

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-core/src/subagent/trace.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/mod.rs`（加 `pub mod trace;`）
- Test: `trace.rs` 内 `mod tests`

**Interfaces:**
- Produces（供 Task 4、5、6 使用）：
  - `pub enum TraceFact { AssistantText { text: String }, ToolCall { name: String, summary: String }, ToolResult { name: String, is_error: bool, summary: String }, StateNote { note: String } }`（`Serialize` / `Deserialize`，`#[serde(tag = "type", rename_all = "snake_case")]`）
  - `pub struct TraceAggregator`，方法：
    - `new() -> Self`
    - `push_text(&mut self, fragment: &str, now: Instant) -> Vec<TraceFact>`
    - `push_tool_call(&mut self, name: &str, summary: &str, now: Instant) -> Vec<TraceFact>`
    - `push_tool_result(&mut self, name: &str, is_error: bool, summary: &str, now: Instant) -> Vec<TraceFact>`
    - `push_state_note(&mut self, note: &str, now: Instant) -> Vec<TraceFact>`
    - `due(&mut self, now: Instant) -> Vec<TraceFact>`
    - `flush(&mut self, now: Instant) -> Vec<TraceFact>`
  - 常量 `TRACE_TEXT_FLUSH_BYTES`、`TRACE_TEXT_FLUSH_INTERVAL`、`TRACE_TOOL_SUMMARY_LIMIT`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn text_fragments_are_merged_until_the_byte_threshold() {
    let mut aggregator = TraceAggregator::new();
    let now = Instant::now();
    assert!(aggregator.push_text("hello ", now).is_empty());
    assert!(aggregator.push_text("world", now).is_empty());
    let facts = aggregator.push_text(&"x".repeat(TRACE_TEXT_FLUSH_BYTES), now);
    assert_eq!(facts, vec![TraceFact::AssistantText { text: "hello worldxxx".into() }]);
}

#[test]
fn a_tool_call_flushes_buffered_text_first() {
    let mut aggregator = TraceAggregator::new();
    let now = Instant::now();
    aggregator.push_text("thinking out loud", now);
    let facts = aggregator.push_tool_call("bash", "cargo test", now);
    assert_eq!(facts.len(), 2);
    assert_eq!(facts[0], TraceFact::AssistantText { text: "thinking out loud".into() });
    assert_eq!(facts[1], TraceFact::ToolCall { name: "bash".into(), summary: "cargo test".into() });
}

#[test]
fn buffered_text_flushes_on_the_time_window() {
    let mut aggregator = TraceAggregator::new();
    let start = Instant::now();
    aggregator.push_text("slow stream", start);
    assert!(aggregator.due(start + TRACE_TEXT_FLUSH_INTERVAL / 2).is_empty());
    let facts = aggregator.due(start + TRACE_TEXT_FLUSH_INTERVAL);
    assert_eq!(facts, vec![TraceFact::AssistantText { text: "slow stream".into() }]);
}

#[test]
fn tool_summaries_are_truncated_to_the_limit() {
    let mut aggregator = TraceAggregator::new();
    let now = Instant::now();
    let facts = aggregator.push_tool_result("bash", false, &"y".repeat(TRACE_TOOL_SUMMARY_LIMIT * 2), now);
    let TraceFact::ToolResult { summary, .. } = &facts[0] else { panic!("expected tool result") };
    assert!(summary.len() <= TRACE_TOOL_SUMMARY_LIMIT);
}

#[test]
fn an_empty_buffer_never_emits_an_empty_text_fact() {
    let mut aggregator = TraceAggregator::new();
    let now = Instant::now();
    assert!(aggregator.flush(now).is_empty());
    assert!(aggregator.push_state_note("running", now).iter().all(|fact| !matches!(fact, TraceFact::AssistantText { .. })));
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib subagent::trace`
Expected: FAIL（模块不存在）

- [ ] **Step 3: 实现**

`TraceAggregator` 只持有 `pending: String` 与 `pending_since: Option<Instant>`：

- `push_text`：追加到 `pending`，首次追加时记录 `now`。若
  `pending.len() >= TRACE_TEXT_FLUSH_BYTES` → flush 成一条 `AssistantText`。
- `push_tool_call` / `push_tool_result` / `push_state_note`：先
  `let mut facts = self.flush(now);` 再 push 自己的事实（保证「边界先 flush 文本」）。
  工具摘要按 `TRACE_TOOL_SUMMARY_LIMIT` 截断（按字节，且不切裂 UTF-8：
  用 `char_indices` 找边界）。
- `due(now)`：仅当 `pending_since` 早于 `now - TRACE_TEXT_FLUSH_INTERVAL` 时 flush。
- `flush`：`pending` 非空才产出一条 `AssistantText`，产出后清空并复位 `pending_since`。
  **空缓冲绝不产出空事实。**

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib subagent::trace`
Expected: PASS（5 例）

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src/subagent/trace.rs crates/yi-agent-core/src/subagent/mod.rs
git commit -m "feat(core): trace facts with write-side aggregation"
```

---

## Phase 2：把源头接到存储

### Task 4: `WorkerHandle` / `Supervisor` 的轨迹抽干

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs`（仿 `watchdog_events`：`worker.rs:231` / `:334` / `:433`）
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs`（仿 `take_worker_watchdog_events`：`supervisor.rs:804`）
- Test: `yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs`

**Interfaces:**
- Consumes: Task 3 的 `TraceFact`。
- Produces（供 Task 5、6 使用）：
  - `WorkerHandle::report_trace(&self, fact: TraceFact)`
  - `WorkerHandle::take_trace_events(&self) -> Vec<TraceFact>`
  - `AgentSupervisor::take_worker_trace_events(&self) -> Vec<(TaskId, TraceFact)>`

- [ ] **Step 1: 写失败测试**

在 `subagent_supervisor.rs` 里（仿既有 watchdog 测试的形状）：

```rust
#[test]
fn supervisor_drains_trace_facts_keyed_by_task() {
    let mut supervisor = /* 既有 helper：起一个 supervisor 与一个 child */;
    let handle = /* 该 child 的 WorkerHandle clone */;
    handle.report_trace(yi_agent_core::subagent::trace::TraceFact::StateNote { note: "running".into() });

    let drained = supervisor.take_worker_trace_events();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].1, yi_agent_core::subagent::trace::TraceFact::StateNote { note: "running".into() });
    // Draining is destructive: a second call yields nothing.
    assert!(supervisor.take_worker_trace_events().is_empty());
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --test subagent_supervisor supervisor_drains_trace_facts_keyed_by_task`
Expected: FAIL（`report_trace` 未定义）

- [ ] **Step 3: 实现**

照抄 `watchdog_events` 的三处形状：`WorkerHandle` 加一个
`trace_events: Arc<Mutex<Vec<TraceFact>>>` 字段与构造、`report_trace` 推入、
`take_trace_events` 用 `std::mem::take` 抽干；`AgentSupervisor` 加
`take_worker_trace_events`，结构与 `take_worker_watchdog_events` 完全一致
（`workers.iter().flat_map(...)`）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --test subagent_supervisor`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src/subagent/worker.rs crates/yi-agent-core/src/subagent/supervisor.rs crates/yi-agent-core/tests/subagent_supervisor.rs
git commit -m "feat(core): drain worker trace facts through the supervisor"
```

---

### Task 5: worker 循环接入聚合器

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`（`AgentEvent` 消费循环：`lib.rs:653-791`）
- Test: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs` 内 `mod tests`

**Interfaces:**
- Consumes: Task 3 `TraceAggregator`、Task 4 `WorkerHandle::report_trace`。
- Produces: 轨迹事实从此在子 worker 跑动时持续产出（Task 6 负责落库）。

- [ ] **Step 1: 写失败测试**

本仓既有的 worker 循环测试用假 provider 驱动（见 `lib.rs` 内既有 `worker_...` 测试）。
新增：

```rust
#[tokio::test]
async fn a_child_transcript_reaches_the_trace_buffer() {
    // 用假 provider 产出：一段 AssistantText → 一次 ToolCall+ToolResult → 结束。
    // 断言 drain 出的轨迹里同时存在 AssistantText、ToolCall、ToolResult 三类事实，
    // 且 AssistantText 的分片已被合并（不是逐片一条）。
    let facts = run_child_and_collect_trace().await;
    assert!(facts.iter().any(|f| matches!(f, TraceFact::AssistantText { .. })));
    assert!(facts.iter().any(|f| matches!(f, TraceFact::ToolCall { .. })));
    assert!(facts.iter().any(|f| matches!(f, TraceFact::ToolResult { .. })));
    assert!(facts.iter().filter(|f| matches!(f, TraceFact::AssistantText { .. })).count() < 3);
}

#[tokio::test]
async fn a_terminal_state_always_ends_the_trace_with_a_note() {
    let facts = run_child_and_collect_trace().await;
    assert!(matches!(facts.last(), Some(TraceFact::StateNote { .. })));
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib a_child_transcript_reaches_the_trace_buffer`
Expected: FAIL（无轨迹产出）

- [ ] **Step 3: 实现**

在 `lib.rs:653` 那个 `match event` 里按类型喂给聚合器，并把返回的事实转交 reporter：

- `Some(AgentEvent::AssistantText(text))` → `aggregator.push_text(&text, Instant::now())`
  （**同时**保留既有 `assistant_report.push_str(&text)`，报告语义不变）。
- `ToolCall` 分支（当前 loop 未显式匹配；如需工具名与入参，从 `AgentEvent::ToolCall`
  取 `name` 与入参摘要）→ `aggregator.push_tool_call(...)`。
- `Some(AgentEvent::ToolResult { result, .. })` → `aggregator.push_tool_result(..., result.is_error, ...)`。
- 每个 `loop` 迭代开头插入一次 `aggregator.due(Instant::now())`，让时间窗口在**没有事件**
  的长间隔里也能 flush。
- **终态**：在每个 `break 'run` 之前调用 `aggregator.flush(Instant::now())` 并推一条
  `StateNote`（内容取终态原因：`completed` / `budget_exhausted` / `cancelled` / `paused` /
  `failed: <原因>`）。**flush 必须发生在 `reporter.report_*` 之前**，否则最后一段文本会
  排在终态之后。
- 把每条产出的事实经 `reporter.report_trace(fact)`（即 Task 4 的 `WorkerHandle`）入队。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib`
Expected: PASS（含既有 worker 测试）

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-subagent/src/lib.rs
git commit -m "feat(subagent): record child transcript as trace facts"
```

---

### Task 6: reconcile 落库 + 分钟 tick 清理

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`（`reconcile_worker_events`：`runtime.rs:2854`）
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`（分钟 tick 分支：`ipc.rs:752` 附近）
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`

**Interfaces:**
- Consumes: Task 2 `append_trace` / `prune_terminal_traces`，Task 4 `take_worker_trace_events`。
- Produces: 轨迹行在子 worker 跑动时持续落库。

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn reconcile_persists_worker_trace_facts() {
    let harness = /* 既有 helper：起 coordinator + 一个 child */;
    let handle = /* 该 child 的 WorkerHandle clone */;
    handle.report_trace(yi_agent_core::subagent::trace::TraceFact::StateNote { note: "running".into() });

    harness.coordinator.reconcile_worker_events().await.unwrap();

    let rows = harness.repository.trace_after(&harness.child_task, 0).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].payload_json.contains("running"));
}

#[tokio::test]
async fn a_reconciled_trace_survives_a_reopen() {
    // 落库后重开 repository，轨迹仍在（证明它是持久的，不是内存态）。
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator reconcile_persists_worker_trace_facts`
Expected: FAIL（轨迹未落库）

- [ ] **Step 3: 实现**

- 在 `reconcile_worker_events` 里，与既有
  `watchdog_updates.extend(supervisor.take_worker_watchdog_events())`（`runtime.rs:2868`）
  并列，加 `trace_updates.extend(supervisor.take_worker_trace_events())`；循环结束后用
  一把 repository 锁批量 `append_trace`。
- **顺序**：轨迹先于状态转移写入（先 append 全部 trace，再走既有的状态推进逻辑），保证
  「终态 note」排在终态事件之前可读。
- 在 daemon 分钟 tick（`ipc.rs:752` 的 `minute != last_schedule_minute` 分支，与
  `evaluate_schedules` 并列）调用 `coordinator.prune_terminal_traces(Utc::now())`，失败只
  `tracing::warn!`，绝不影响主循环。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator`
Expected: PASS（含既有用例）

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-store/src/runtime.rs crates/yi-agent-store/src/ipc.rs
git commit -m "feat(store): persist worker trace facts on reconcile and prune on the minute tick"
```

---

### Task 7: `thread_id` 贯穿与继承

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`（`SpawnApplicationChild`：`ipc.rs:190`）
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`（`spawn_child_with_objective`：`runtime.rs:1153`）
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`（委派工具构造 spawn 请求处）
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`、`tests/runtime_coordinator.rs`

**Interfaces:**
- Consumes: Task 1/2 的 `tasks.thread_id` 与 `task_thread_id` / `set_task_thread_id`。
- Produces: `SpawnApplicationChild { …, thread_id: Option<String> }`（`#[serde(default)]`）；
  子任务行带 `thread_id`；孙任务**继承**父的 `thread_id`。

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn a_child_records_the_thread_that_spawned_it() {
    // 经 IPC: SpawnApplicationChild { thread_id: Some("thread-a") }
    // 断言：daemon 返回的 child 任务，task_thread_id == Some("thread-a")
}

#[tokio::test]
async fn a_grandchild_inherits_its_parents_thread() {
    // 父带 thread-a、父再 spawn 一个不传 thread_id 的孙任务
    // 断言：孙任务的 thread_id == Some("thread-a")
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc a_child_records_the_thread`
Expected: FAIL（无 `thread_id` 字段）

- [ ] **Step 3: 实现**

- IPC 请求加 `#[serde(default)] thread_id: Option<String>`（老客户端缺失即 `None`，行为不变）。
- `spawn_child_with_objective` 增参 `thread_id: Option<String>`；解析规则：
  `thread_id.or_else(|| repository.task_thread_id(parent).ok().flatten())`——**显式值优先，
  否则继承父**。解析结果经 Task 2 的 `set_task_thread_id` 写入子任务行（与建任务同一事务，
  避免出现「任务已建但标记缺失」的中间态）。
- `yi-agent-subagent` 的委派工具构造 spawn 请求时透传调用方给的 `thread_id`（TUI 传 `None`）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc && cargo test -p yi-agent-store --test runtime_coordinator`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-store/src/ipc.rs crates/yi-agent-store/src/runtime.rs crates/yi-agent-subagent/src/lib.rs
git commit -m "feat(store): carry a conversation marker onto child tasks and inherit it"
```

---

## Phase 3：IPC 观察面

### Task 8: `ReadTaskTrace` 与 `SubscribeTrace`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`

**Interfaces:**
- Consumes: Task 2 `trace_after` / `trace_high_water`。
- Produces（供 Task 9、12 使用）：
  - `IpcRequest::ReadTaskTrace { task_id: String }`
  - `IpcResponse::TaskTrace { task_id: String, high_water_id: i64, rows: Vec<IpcTraceRow> }`
  - `IpcRequest::SubscribeTrace { after_id: i64, filters: TraceFilters }`
  - `IpcResponse::TraceSubscription(TraceSnapshot)`（首帧）与 `IpcResponse::TraceEvent(IpcTraceRow)`
  - `pub struct IpcTraceRow { pub event_id: i64, pub task_id: String, pub kind: String, pub payload_json: String }`
  - `pub struct TraceFilters { pub task_ids: Vec<String>, pub kinds: Vec<String> }`（空 = 不过滤，语义同既有 `SubscriptionFilters`）

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn daemon_reads_a_task_trace_snapshot() {
    // 造一个 child，报告两条 trace，经 IPC: ReadTaskTrace
    // 断言：TaskTrace.rows.len() == 2 且 high_water_id == 第二行 id
}

#[tokio::test]
async fn a_trace_subscription_replays_then_streams_without_gaps_or_duplicates() {
    // 1) 先写 2 行；2) SubscribeTrace { after_id: 0 } 收到首帧 TraceSubscription（含 2 行）
    //    + 2 条 TraceEvent；3) 再写 1 行，收到第 3 条 TraceEvent；
    // 4) 断言 event_id 严格递增、无重复、无缺失。
}

#[tokio::test]
async fn a_trace_subscription_can_be_kind_filtered() {
    // filters.kinds == ["tool_call"]：assistant_text 行被过滤掉。
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc daemon_reads_a_task_trace_snapshot`
Expected: FAIL（请求变体不存在）

- [ ] **Step 3: 实现**

- `ReadTaskTrace` 走既有的请求/响应路径（`send_request` + `handle_client` 的 match 加一臂），
  返回快照 + 高水位 id。
- `SubscribeTrace` **照搬** `SubscribeEvents` 的全套形状：`handle_client` 里对应的流式分支、
  首帧 `subscription_snapshot` 等价的 trace 版本、producer 循环每轮
  `reconcile_worker_events()` 后改读 `trace_after`（对每个被订阅的 task）并按 `TraceFilters`
  过滤、逐帧写入时带上 `event_id`、`Overflowed` 即关连接。
- **帧身份校验照旧**：客户端读每帧时校验 `envelope.event_id` 与载荷一致（同 `ipc.rs:652`），
  不一致即报错——这是「不丢不重」保证的一部分，不要省。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-store/src/ipc.rs
git commit -m "feat(store): read and subscribe to task traces over IPC"
```

---

### Task 9: 客户端轨迹订阅 helper

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`（仿 `subscribe_with_filters`：`ipc.rs:1022`）
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`

**Interfaces:**
- Consumes: Task 8 的请求/响应类型。
- Produces（供 Task 10、11、12 使用）：
  - `yi_agent_store::ipc::read_task_trace(socket_path, task_id) -> Result<TraceSnapshot, IpcError>`
  - `yi_agent_store::ipc::subscribe_trace(socket_path, after_id, filters) -> Result<TraceSubscription, IpcError>`
  - `pub struct TraceSubscription`，方法 `next_row(&mut self) -> Result<Option<IpcTraceRow>, IpcError>`（`None` = 流结束）

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn a_trace_subscription_rejects_a_frame_for_another_request() {
    // 起一个假服务端，故意写一条 event_id 与载荷不匹配的帧；
    // 断言 next_row 返回 Err 而不是把错帧当成正常行交给调用方。
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc a_trace_subscription_rejects_a_frame_for_another_request`
Expected: FAIL（`TraceSubscription` 未定义）

- [ ] **Step 3: 实现**

`TraceSubscription` 与既有 `Subscription` 同形：持 `BufReader<UnixStream>` 与 `request_id`；
`next_row` 读一帧、校验信封的 `protocol_version` / `request_id` / `event_id` 一致性，
首帧（`TraceSubscription`）消费掉并返回 `Ok(None)`（或按需并入），其余返回 `Ok(Some(row))`。
连接关闭（EOF）返回 `Ok(None)`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-store/src/ipc.rs
git commit -m "feat(store): client helpers for reading and following a task trace"
```

---

## Phase 4：TUI 入口

### Task 10: Ctrl+P 第三页签「子 agent」

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`（`RuntimePopup`：`app.rs:707`，`switch_tab`：`app.rs:722`）
- Create: `yi-agent-rs/crates/yi-agent/src/tui/trace.rs`
- Test: `trace.rs` 内 `mod tests` + `app.rs` 内 `mod tests`

**Interfaces:**
- Consumes: Task 9 `read_task_trace` / `subscribe_trace`。
- Produces: `tui/trace.rs` 内 `TracePopup` 状态机（`List` / `Detail`）与
  `RuntimeTab::Agents`，供 Task 11 扩展。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn the_runtime_popup_cycles_bash_processes_and_agents() {
    // RuntimeTab::BashTasks.next() == Processes, Processes.next() == Agents, Agents.next() == BashTasks
}

#[test]
fn opening_the_agents_tab_lists_the_current_roots_children() {
    // 给一个假的子任务列表，断言列表项数量与顺序（按 created_at 升序、排除 root）
}

#[test]
fn enter_on_a_list_row_opens_that_agents_detail() {
    // 断言状态从 List 变为 Detail 且绑定的 task_id 正确
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent the_runtime_popup_cycles_bash_processes_and_agents`
Expected: FAIL（`RuntimeTab::Agents` 不存在）

- [ ] **Step 3: 实现**

- `RuntimeTab` 加 `Agents`；`RuntimePopup` 加 `Agents(TracePopup)`；`switch_tab` /
  `switch_runtime_tab` 的 match 补全（注意 `app.rs` 里 `switch_tab` 与
  `switch_runtime_tab_for_test` 两处都要改，否则测试路径与生产路径分叉）。
- 列表数据源复用既有 `/agents` 的取数（当前 root 的直接子任务，排除前台 root）。
- 渲染与键处理仿 `process_popup.rs`：`Up`/`Down` 移动、`Enter` 进详情、`Esc` 退回列表、
  再 `Esc`（或 `Tab`）退出弹窗。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/trace.rs crates/yi-agent/src/tui/mod.rs crates/yi-agent/src/tui/app.rs
git commit -m "feat(tui): subagent tab in the runtime popup"
```

---

### Task 11: 只读轨迹视图、下钻与两个动作

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/trace.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`
- Test: `yi-agent-rs/crates/yi-agent/src/tui/trace.rs` 内 `mod tests`

**Interfaces:**
- Consumes: Task 9 的订阅、Task 5 产生的轨迹事实。
- Produces: 轨迹详情视图（文本流 + 工具卡 + 状态行）、逐级下钻、发消息、取消。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn a_trace_row_becomes_a_history_cell() {
    // assistant_text → HistoryCell::AssistantMessage（或既有等价的助手文本 cell）
    // tool_call     → HistoryCell::ToolCall
    // tool_result   → HistoryCell::ToolResult
    // state_note    → HistoryCell::Separator
}

#[test]
fn streaming_text_rows_merge_into_one_cell() {
    // 连续两条 assistant_text 行合并显示为一段，而不是两段
}

#[test]
fn esc_from_a_child_detail_returns_to_its_parent_agent() {
    // 从孙任务详情 Esc 回到父任务详情，而不是直接回到列表
}

#[test]
fn opening_a_detail_opens_exactly_one_trace_subscription() {
    // 断言：进入详情建立 1 条订阅；退回列表后该订阅被丢弃（不再消费）
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent a_trace_row_becomes_a_history_cell`
Expected: FAIL

- [ ] **Step 3: 实现**

- 打开详情：`read_task_trace` 回灌历史 → 以 `high_water_id` 建 `subscribe_trace`（全 kind）→
  在事件循环里 `next_row` 拉取并转成 `HistoryCell` 追加；`Esc`/退出即丢弃订阅。
- 渲染复用 `HistoryState`（含滚动与缓存），**不要**另写一套渲染器。
- 下钻：详情页若该任务有直接子任务，`Enter`（或 `Tab`）展开子任务列表，可再进入；
  `Esc` 逐级返回（维护一个进入栈，栈空则回列表）。
- 两个动作：`m` 进入发消息输入（回车提交，走既有 `SendUserMessage`）；`k` 触发取消，
  **沿用既有预览 token + 二次确认**路径（复用 `/cancel` 的实现，不要绕过确认）。
- `/agent <task_id>` 保留并改为直接打开同一详情视图。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/trace.rs crates/yi-agent/src/tui/app.rs
git commit -m "feat(tui): read-only subagent trace view with drill-down, message and cancel"
```

---

## Phase 5：桌面端入口

### Task 12: app-server 的 `agent/*` 方法与对话标记

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（attach 处：`server.rs:135`）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` 内 `mod tests`

**Interfaces:**
- Consumes: Task 9 的客户端 helper、Task 7 的 `thread_id`。
- Produces：
  - `agent/children/list { threadId }` → `{ children: [{ taskId, objective, state, lastStep }] }`
  - `agent/trace/read { threadId, taskId }` → `{ rows: [...], highWaterId }`
  - `agent/trace/watch { threadId, taskId }` / `agent/trace/unwatch { threadId }`
  - 通知 `agent/trace/event { threadId, taskId, row }` 与 `agent/children/updated { threadId, children }`

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn a_git_project_spawns_children_tagged_with_the_thread() {
    // 起 thread → 经 RPC 触发一次 spawn → agent/children/list 只列出该 thread 的子 agent
}

#[tokio::test]
async fn trace_watch_streams_rows_for_the_watched_task_only() {
    // watch(task A) → 只有 A 的行经 agent/trace/event 推出；unwatch 后不再推
}

#[tokio::test]
async fn watching_another_task_replaces_the_previous_watch() {
    // watch(A) 后 watch(B)：A 的流被关闭，不会同时推两条
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server trace_watch_streams_rows_for_the_watched_task_only`
Expected: FAIL（方法不存在）

- [ ] **Step 3: 实现**

- 协议类型加进 `protocol.rs`（请求/响应/通知），命名遵循既有 `thread/` `turn/` 风格。
- **对话标记**：`spawn_agent` 由模型调用，模型不知道 threadId。因此在
  `build_runtime_tooling`（`server.rs:167`）装配该 thread 的工具时，把该 thread 的 id
  绑定进委派工具的 spawn 请求（工具闭包捕获 threadId），随 `SpawnApplicationChild` 送出。
  **一个 cwd 多 thread 共享 root 的情况下，标记由装配期捕获决定，不靠事后推断。**
- **轻量列表**：`agent/children/list` 用该 thread 的 threadId 过滤任务行。实时性靠
  `agent/children/updated`：app-server 为该 thread 建一条**kind 过滤为
  `tool_call` + `state_note` 的 `SubscribeTrace`**（文本流是唯一的高 volume 部分，被排除；
  工具用既有订阅的 filters 语义），据此更新列表并推通知。过期/失效时重建订阅。
- **详情流**：`agent/trace/watch` 每 thread 至多一个受关注任务；换任务即关闭旧订阅、开新订阅
  （用一个受控线程 + 控制通道实现，形状仿 app-server 既有的 per-thread driver）。
  行经 `agent/trace/event` 推出。
- **降级**：attach 失败或 daemon 不可达时，`agent/*` 一律返回空列表/错误码，
  **turn 照常跑**（沿用既有「失败只 warn」的降级姿态）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: PASS（含既有 157+ 用例）

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/protocol.rs crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): expose subagent children and live traces per thread"
```

---

### Task 13: 桌面端暂留区（列表 + 状态）

**Files:**
- Modify: `desktop/src/lib/protocol.ts`
- Create: `desktop/src/lib/subagents.ts`
- Create: `desktop/src/components/SubagentRail.tsx`
- Modify: `desktop/src/App.tsx`（布局：`App.tsx:382-420`）
- Test: `desktop/src/lib/subagents.test.ts`、`desktop/src/components/SubagentRail.test.tsx`

**Interfaces:**
- Consumes: Task 12 的 `agent/children/list`、`agent/children/updated`。
- Produces: 暂留区组件与 `SubagentView` 状态折叠（`{ taskId, objective, state, lastStep }[]`）。

- [ ] **Step 1: 写失败测试**

```ts
// subagents.test.ts
it("folds children/updated notifications into the rail list", () => {
  // 一条 updated 通知 → 列表内容与顺序符合预期
});
it("keeps a terminal child in the list but marks it finished", () => {
  // state 为终态的子 agent 仍在列表中，且标记为已结束（不再有「当前步骤」）
});
```

```tsx
// SubagentRail.test.tsx
it("renders one card per child with its objective and status", () => {...});
it("shows an empty state when the thread has no children", () => {...});
it("invokes onOpen with the task id when a card is clicked", () => {...});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/subagents.test.ts src/components/SubagentRail.test.tsx`
Expected: FAIL（文件不存在）

- [ ] **Step 3: 实现**

- `protocol.ts` 加请求/响应/通知类型（types only，遵循既有文件风格）。
- `subagents.ts` 折叠通知为列表（纯函数，便于测试）。
- `SubagentRail.tsx`：按当前 thread 的子 agent 列表，点卡片回调 `onOpen(taskId)`；
  空态显示「暂无子 agent」。
- `App.tsx`：主对话列旁加可折叠的暂留区（与主对话并列、可切换，不是弹窗）；thread 切换时
  重新 `agent/children/list`；`agent/children/updated` 就地更新。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/subagents.test.ts src/components/SubagentRail.test.tsx`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd desktop && npx tsc --noEmit
git add desktop/src
git commit -m "feat(desktop): subagent rail beside the conversation"
```

---

### Task 14: 桌面端轨迹详情（两级）与两个动作

**Files:**
- Create: `desktop/src/components/SubagentTrace.tsx`
- Modify: `desktop/src/lib/subagents.ts`（轨迹行折叠）
- Modify: `desktop/src/App.tsx`
- Test: `desktop/src/components/SubagentTrace.test.tsx`、`desktop/src/lib/subagents.test.ts`

**Interfaces:**
- Consumes: Task 12 的 `agent/trace/read` / `watch` / `unwatch`、`agent/trace/event`。
- Produces: 详情视图（摘要 → 完整轨迹）、下钻、发消息、取消。

- [ ] **Step 1: 写失败测试**

```tsx
it("opens on the summary level and expands to the full trace on demand", () => {...});
it("appends streamed trace rows without re-rendering finalised ones", () => {...});
it("sends a message to the child and cancels it through the confirmation path", () => {...});
```

```ts
it("merges consecutive assistant_text rows into one block", () => {...});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/SubagentTrace.test.tsx`
Expected: FAIL

- [ ] **Step 3: 实现**

- 打开详情：`agent/trace/read` 回灌 → `agent/trace/watch` 订阅 → `agent/trace/event` 追加；
  关闭即 `unwatch`（**同一时刻只有一个 watch**，避免多路流）。
- 两级视图：默认摘要（状态、目标、最近步骤、已产生的报告），展开才渲染完整轨迹
  （复用既有 `ToolCallCard`、`MarkdownText` 与 `AgentMessage` 的记忆化范式，避免流式期间
  重渲染定稿内容——这是既有性能修复踩过的坑，桌面端必须沿用）。
- 下钻：详情内若有子任务，列出并可继续进入；带返回栈。
- 两个动作：发消息（走既有 `send`/interject 路径的适当形态）与取消（**必须走既有确认路径**，
  不得直接取消）。
- 子 agent 的权限：**不新增审批入口**（沙箱边界不是交互式审批）；轨迹里出现的失败工具结果
  就是既有 `ToolCallCard` 的失败态。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run && npx tsc --noEmit`
Expected: PASS（含既有 170 用例）

- [ ] **Step 5: 提交**

```bash
cd desktop && npm run build
git add desktop/src
git commit -m "feat(desktop): subagent trace detail with drill-down, message and cancel"
```

---

## Phase 6：文档同步

### Task 15: 进度文档

**Files:**
- Modify: `docs/project-management/subagent-runtime.md`
- Modify: `docs/project-management/yi-agent-tui.md`
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/yi-agent-app-server.md`
- Modify: `docs/project-management/yi-agent-store.md`
- Modify: `docs/bug-list.md`
- Modify: `docs/project-management/README.md`（模块索引计数）

**Interfaces:** 无（纯文档）。

- [ ] **Step 1: 更新各模块 Features**

每条新能力带**可验证判据**（代码位置或可执行命令）。至少覆盖：

- `subagent-runtime.md`：轨迹记录与订阅、`thread_id` 分组、TUI 观察入口、桌面观察入口
  （每条给 `cargo test` / `npx vitest` 命令）。
- `yi-agent-tui.md`：Ctrl+P 第三页签 + 轨迹视图 + 下钻 + 两个动作。
- `yi-agent-app-server.md`：`agent/children/list`、`agent/trace/read`、`agent/trace/watch`。
- `yi-agent-store.md`：schema v12、轨迹表、裁剪与清理、两个新 IPC。
- `desktop.md`：把 P3 的 `[ ] 子 agent 任务树可视化` 更新为已交付（若仅部分交付，写明剩余）。

- [ ] **Step 2: 关闭 bug-list 对应条目**

`docs/bug-list.md` 中「目前没有路径进入 subagent 内部看它的内容与进度…」一条改为 `[x]`，
并按既有风格补上修复说明、代码位置与验证命令，同时注明**未做**的部分（reasoning 展示）
另立新条目。

- [ ] **Step 3: 同步 README 计数**

更新 `docs/project-management/README.md` 模块索引表里受影响模块的「完成 / 总计」。

- [ ] **Step 4: 提交**

```bash
git add docs
git commit -m "docs: record the subagent observation entry points"
```

---

## Self-Review

**Spec 覆盖检查**

| 设计文档章节 | 对应任务 |
| --- | --- |
| §2.1 记录层（源头 + 聚合） | Task 3、5 |
| §2.2 存储层（独立表、上限、清理、10ms 节奏） | Task 1、2、6 |
| §2.3 传输层（两个 IPC、续看、按需订阅） | Task 8、9 |
| §2.4 对话标记与继承 | Task 1、7、12 |
| §2.5 呈现层（TUI 页签/轨迹/动作；桌面暂留区/详情/动作） | Task 10、11、13、14 |
| §3 不做项 | 全局约束 + Task 11/14 中「不新增审批入口」「走确认路径」 |
| §4 验证策略 | 各任务 Step 1 的测试 + Task 15 |
| §5 落地分期 | Phase 1–4 的划分 |

**一致性检查（类型与命名）**

- `TraceFact` 在 Task 3 定义，Task 4/5/6 使用同一名字与变体名（`AssistantText` /
  `ToolCall` / `ToolResult` / `StateNote`）。Task 2 的 `append_trace` 引用
  `TraceFact` 住在 `yi-agent-core`（`yi_agent_core::subagent::trace::TraceFact`），
  store 通过 `yi_agent_core` 引用它；Task 1/2 的示例已按此路径写。
- IPC 类型名在 Task 8 定义（`IpcTraceRow` / `TraceFilters` / `TaskTrace` / `TraceEvent` /
  `TraceSubscription`），Task 9、12 复用同一组名字。
- 常量值集中在 Global Constraints 表，Task 2/3 引用同一组名字。

**占位符扫描**：无 `TBD` / `TODO` / 「类似 Task N」。UI 任务的测试以「断言行为」的
具体断言写出（非「写测试」空话）。

**已知需实现者补全的细节**：Task 1 的 `connection_for_test` 访问器、Task 2 测试里的
`seed_running_task` helper、Task 5 的
`run_child_and_collect_trace` helper、Task 12 的受控 watch 线程——三处都给了明确的
形状与断言要求，实现时按仓内既有等价 helper 落地，不要新造抽象。
