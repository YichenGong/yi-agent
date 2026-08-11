# subagent-runtime

## 模块说明

跨项目的多 Agent 运行时设计模块。它为根 Agent、子 Agent 和孙 Agent 提供
任务树、监督、mailbox、资源调度、worktree 交付、daemon 持久化与用户控制面。
完整设计见
[Subagent Runtime Architecture Design](../superpowers/specs/2026-08-09-subagent-architecture-design.md)。

## 范围边界

**做什么：**
- 两层递归的任务树和受监督 Agent 生命周期
- `spawn_agent`、`wait_agent`、`send_message` 工具
- worktree 隔离、commit 交付与逐层审核集成
- 跨项目资源租约、手动启动 daemon、持久化和用户控制 API

**不做什么：**
- depth 2 以下的继续递归
- 未经审核的自动合并到目标分支
- 默认网络监听、跨机器协调或自动启动 daemon

## Features

- [ ] 任务、attempt 与两层父子状态机 — 验证：`cargo test -p yi-agent-core subagent::task::tests`
- [ ] AgentSupervisor 与结构化 mailbox — 验证：`cargo test -p yi-agent-core subagent::supervisor::tests`
- [ ] `spawn_agent` / `wait_agent` / `send_message` 工具 — 验证：`cargo test -p yi-agent-core subagent::tools::tests`
- [ ] DelegationContract、提示词装配与权限下放 — 验证：`cargo test -p yi-agent-core subagent::authority::tests`
- [ ] 通用资源租约与 16 个 resident subagent 公平调度 — 验证：`cargo test -p yi-agent-core subagent::scheduler::tests`
- [ ] Git worktree 基线、提交交付和逐层集成 — 验证：`cargo test -p yi-agent-tools subagent_worktree`
- [x] 手动启动的本地 daemon、SQLite 状态与 IPC 重连 — `/agents` 通过紧凑任务摘要避免传输 worktree 元数据；订阅快照部分写入失败后关闭连接，避免向半帧追加 error JSON；代码：`yi-agent-rs/crates/yi-agent-store/src/ipc.rs`、`yi-agent-rs/crates/yi-agent/src/tui/app.rs`，验证：`cargo test -p yi-agent-store --test runtime_ipc daemon_lists_compact_task_summaries && cargo test -p yi-agent --bin yi-agent agents_summary_reads_daemon_task_snapshot`
- [x] 定时任务与保守默认策略 — 五字段 Cron、原子发生记录、离线 skip/catch-up-once、重叠 skip、daemon 分钟 tick 与 IPC 位于 `yi-agent-store`；自然语言仅生成需 `--confirm` 的本地预览，验证：`cargo test -p yi-agent-store --test scheduler && cargo test -p yi-agent-store --test runtime_coordinator && cargo test -p yi-agent-store --test runtime_ipc && cargo test -p yi-agent --bin yi-agent schedule_intent`
- [x] TUI-first delivery 审查 MVP — `/review` 读取 daemon delivery JSON，`/accept`、`/rework`、`/reject` 走预览 token 后确认的受控审查路径，代码：`yi-agent-rs/crates/yi-agent/src/tui/app.rs`，验证：`cargo test -p yi-agent --bin yi-agent review_control && cargo test -p yi-agent --bin yi-agent control_previews_then_confirms_review_from_tui`
- [x] TUI-first 任务观察 MVP — `/agents` 默认显示当前 session 的子任务树并排除前台 root，`--all` 显示 runtime 全部记录、`--active` 显示全局非终止任务，两个诊断视图会标注 root，并以 Markdown 列表显示状态；`/events` 读取任务事件、`/mailbox` 读取未消费 mailbox、`/diff` 读取 delivery evidence，代码：`yi-agent-rs/crates/yi-agent/src/tui/app.rs`、`yi-agent-rs/crates/yi-agent-store/src/ipc.rs`，验证：`cargo test -p yi-agent-store --test runtime_ipc daemon_lists_compact_task_summaries && cargo test -p yi-agent --bin yi-agent all_agents_summary_marks_root_tasks && cargo test -p yi-agent --bin yi-agent events_control_reads_task_events_from_daemon && cargo test -p yi-agent --bin yi-agent mailbox_control_reads_messages_without_consuming_them && cargo test -p yi-agent --bin yi-agent diff_control_reads_delivery_evidence_from_daemon`
- [x] Sub-agent 文本结果回传 — worker 聚合 `AgentEvent::AssistantText`，无代码变更时以 `completed_no_changes` 收口，将文本报告写入 `attempts.terminal_json` 并通过 `wait_agent` / IPC `reports` 返回，代码：`yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`、`yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs`、`yi-agent-rs/crates/yi-agent-store/src/runtime.rs`，验证：`cargo test -p yi-agent-core --test subagent_supervisor wait_agent_returns_completed_child_reports && cargo test -p yi-agent-store --test runtime_ipc daemon_wait_agent_returns_completed_child_reports && cargo test -p yi-agent-store --test runtime_ipc daemon_wait_agent_keeps_completed_child_reports_after_restart && cargo test -p yi-agent --bin yi-agent daemon_worker_reports_text_completion_without_a_workspace_delivery`
- [x] TUI wait_agent 有界等待 — daemon IPC 支持可选 `timeout_ms`，TUI `wait_agent` 默认 2 分钟超时并返回 `status: "timeout"` 与当前 children，避免子任务未终止时界面无限等待，代码：`yi-agent-rs/crates/yi-agent-store/src/ipc.rs`、`yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`，验证：`cargo test -p yi-agent-store --test runtime_ipc daemon_wait_agent_times_out_instead_of_waiting_forever`
- [x] wait_agent 跨进程结果精确性 — `any` 只返回已终止 child，`all` 超时时保留已完成 child 的 `reports`，worker 文本完成会及时唤醒等待者，已终止历史 child 不再占 direct-child spawn 名额，超时路径不绕过 application capability，IPC/TUI 错误带可读原因，且一个等待中的 IPC client 不阻塞其他 client，代码：`yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs`、`yi-agent-rs/crates/yi-agent-store/src/ipc.rs`、`yi-agent-rs/crates/yi-agent-store/src/runtime.rs`，验证：`cargo test -p yi-agent-core --test subagent_supervisor wait_ && cargo test -p yi-agent-core --test subagent_supervisor spawn && cargo test -p yi-agent-store --test runtime_ipc application_root_ && cargo test -p yi-agent-store --test runtime_ipc daemon_`
- [ ] CLI/TUI/Slash-command 任务树观察、帮助和人工干预 — 验证：`cargo test -p yi-agent --bin yi-agent subagent_`
- [x] Worker retry watchdog 记账 — `WorkerWatchdogEvent::ToolRetry` 经 `RuntimeCoordinator::reconcile_worker_events` 持久化至 `WatchdogUsage::tool_retries`；验证：`cargo test -p yi-agent-store --test runtime_coordinator coordinator_persists_worker_usage_and_meaningful_progress`
