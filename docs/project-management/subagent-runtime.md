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
- [ ] 手动启动的本地 daemon、SQLite 状态与 IPC 重连 — 验证：`cargo test -p yi-agent-store runtime::tests`
- [ ] 定时任务与保守默认策略 — 验证：`cargo test -p yi-agent-store scheduler::tests`
- [ ] CLI/TUI/Slash-command 任务树观察、帮助和人工干预 — 验证：`cargo test -p yi-agent --bin yi-agent subagent_`
