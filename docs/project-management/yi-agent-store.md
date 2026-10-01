# yi-agent-store

## 模块说明

yi-agent 的会话持久化 crate。目标是保存和恢复 `yi-agent-core::Session`，使会话可跨进程继续。

## 范围边界

**做什么：**
- 本地会话历史的保存与读取
- 基于已恢复 `Session` 重建 agent
- 存储后端与数据格式的封装

**不做什么：**
- 不在本 crate 执行 Agent 循环
- 不将会话上传至远程服务

## Features

- [x] schema v12：`task_trace_events` 表与 `tasks.thread_id` — 轨迹独立成表（不复用审计 `events`），每行是 `(task_id, kind, payload_json)` + 自增 `id` 作游标；`tasks.thread_id`（nullable）标记任务所属对话，与轨迹表同批迁移（v11 -> v12），`pragma_table_info` 探针保证幂等。代码：`yi-agent-rs/crates/yi-agent-store/src/repository.rs`（迁移）、`repository.rs:69`（`PersistedTraceRow`），验证：`cargo test -p yi-agent-store --lib schema_twelve_adds_the_trace_table_and_thread_id`、`cargo test -p yi-agent-store --lib a_version_eleven_database_gains_the_trace_table_and_thread_id`
- [x] 轨迹写入、读取、裁剪与终态清理 — `append_trace` 在同一事务内把该任务裁到 `TRACE_MAX_ROWS_PER_TASK`（2000）行；`trace_after(task, after_id)` 按游标读、`trace_high_water(task)` 报水位；`trim_trace` 重裁已有轨迹，`prune_terminal_traces(now)` 按 `TRACE_TERMINAL_RETENTION_SECS`（86400）清理终态任务的行，由 daemon 分钟 tick 调用。代码：`yi-agent-rs/crates/yi-agent-store/src/repository.rs:24`、`repository.rs:30`、`repository.rs:3114`、`repository.rs:3135`、`repository.rs:3172`、`repository.rs:3194`、`repository.rs:3211`，验证：`cargo test -p yi-agent-store --lib trace`
- [x] 轨迹 IPC：快照读取与带游标订阅 — `IpcRequest::ReadTaskTrace` -> `IpcResponse::TaskTrace { task_id, high_water_id, rows }`；`IpcRequest::SubscribeTrace { task_ids, after_id, kinds }` 首帧回 `TraceSubscription(TraceSnapshot)`、之后逐行 `TraceEvent(IpcTraceRow)`，`task_ids` 为空返回 `Validation`；快照的水位取自**实际返回的最后一行**（0 表示无行），避免"先采样水位"造成的重复推送或永久丢行。客户端 helper `read_task_trace` / `subscribe_trace` 支持 `set_nonblocking` + `try_row`（UI 每帧轮询用）。代码：`yi-agent-rs/crates/yi-agent-store/src/ipc.rs:333`、`ipc.rs:346`、`ipc.rs:458`、`ipc.rs:469`、`ipc.rs:636`、`ipc.rs:645`、`ipc.rs:1153`、`ipc.rs:1182`、`ipc.rs:1229`、`ipc.rs:1240`，验证：`cargo test -p yi-agent-store --lib trace_subscription_tests`；集成用例 `cargo test -p yi-agent-store --test runtime_ipc trace` 需要能创建 Unix socket 的环境
- [x] 任务摘要携带父任务与对话标记 — `task_summaries` 的四个分支都多取 `parent_id` 与 `thread_id`，透传到 `IpcTaskSummary`，让"某任务的直接子任务"与"某对话的全部子任务"各自一次查询可得，无需二次请求。代码：`yi-agent-rs/crates/yi-agent-store/src/repository.rs:353`（`PersistedTaskSummary`）、`repository.rs:3915`（`task_summaries`）、`yi-agent-rs/crates/yi-agent-store/src/ipc.rs`（`IpcTaskSummary`），验证：`cargo test -p yi-agent-store --test runtime_ipc daemon_lists_compact_task_summaries`（需能创建 Unix socket 的环境）
- [ ] Session 持久化与恢复 — 当前仅有 `crates/yi-agent-store/src/lib.rs` crate 骨架，尚无存储后端或 save/load API
