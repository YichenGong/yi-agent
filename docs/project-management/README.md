# yi-agent 项目进度总览

## 状态图例
- [x] 已完成（有代码 + 有验证）
- [ ] 未完成
- [-] 已放弃（YAGNI）

## 模块索引

| 模块 | 完成 / 总计 | 详情 |
|---|---|---|
| yi-agent-core | 29 / 30 | [详情](./yi-agent-core.md) |
| yi-agent-llm | 12 / 15 | [详情](./yi-agent-llm.md) |
| yi-agent-tools | 28 / 28 | [详情](./yi-agent-tools.md) |
| yi-agent-skills | 9 / 9 | [详情](./yi-agent-skills.md) |
| yi-agent-tui | 47 / 50 | [详情](./yi-agent-tui.md) · [旁问修复 spec](../superpowers/specs/2026-11-05-side-conversation-repair-design.md) |
| yi-agent-run | 11 / 11 | [详情](./yi-agent-run.md) |
| yi-agent-cli | 9 / 9 | [详情](./yi-agent-cli.md) |
| yi-agent-web | 12 / 12 | [详情](./yi-agent-web.md) |
| yi-agent-relay | 2 / 2 | [详情](./yi-agent-relay.md) |
| yi-agent-mcp | 6 / 6 | [详情](./yi-agent-mcp.md) |
| yi-agent-store | 14 / 14 | [详情](./yi-agent-store.md) |
| yi-agent-flags | 9 / 9 | [详情](./yi-agent-flags.md) |
| yi-agent-runtime | 32 / 37 | [详情](./yi-agent-runtime.md) · [运行参数即时生效 spec](../superpowers/specs/2026-11-04-runtime-settings-live-apply-design.md) · [plan](../superpowers/plans/2026-11-04-runtime-settings-live-apply-impl.md) · [validation](../research/2026-11-04-runtime-settings-live-apply-validation.md) · [清单刷新 plan](../superpowers/plans/2026-11-03-supervisor-manifest-describe-bounds-impl.md) |
| yi-agent-power | 12 / 12 | [详情](./yi-agent-power.md) |
| yi-agent-plugins | 86 / 86 | [详情](./yi-agent-plugins.md) |
| yi-agent-subagent | 12 / 12 | [详情](./yi-agent-subagent.md) |
| yi-agent-app-server | 95 / 98 | [详情](./yi-agent-app-server.md) · [Session 完整性修复 spec rev2](../superpowers/specs/2026-11-08-session-completeness-repair-design.md) · [Plan 总入口](../superpowers/plans/2026-11-08-session-completeness-repair-impl.md) |
| subagent-runtime | 70 / 83 | [详情](./subagent-runtime.md) · [App subagent 启动修复 spec（草案）](../superpowers/specs/2026-11-09-app-subagent-startup-repair-design.md) · [plan](../superpowers/plans/2026-11-09-app-subagent-startup-repair-impl.md) · [plan 复审](../superpowers/reviews/2026-11-09-app-subagent-startup-plan-review.md) · [会话委派解耦 spec](../superpowers/specs/2026-11-05-session-subagent-capability-design.md) · [实施计划](../superpowers/plans/2026-11-05-session-subagent-capability-impl.md) · [前台根生命周期 spec](../superpowers/specs/2026-11-03-foreground-root-lifecycle-design.md) · [plan](../superpowers/plans/2026-11-03-foreground-root-lifecycle-impl.md) |
| yi-agent-lifecycle | 23 / 23 | [详情](./yi-agent-lifecycle.md) |
| superpowers-kanban | 19 / 23 | [详情](./superpowers-kanban.md) · [daemon 地址修复 spec rev3](../superpowers/specs/2026-11-09-kanban-daemon-socket-contract-design.md) · [复审](../superpowers/reviews/2026-11-09-kanban-daemon-socket-spec-review.md) · [plan rev2（Task1–8 已执行，正式待确认）](../superpowers/plans/2026-11-09-kanban-daemon-socket-contract-impl.md) · [plan复审](../superpowers/reviews/2026-11-09-kanban-daemon-socket-plan-review.md) · [验收记录](../superpowers/reports/2026-11-09-kanban-daemon-socket-validation.md) · [查询 IPC 修复 spec](../superpowers/specs/2026-11-03-kanban-query-io-design.md) · [会话观测误报 spec](../superpowers/specs/2026-11-03-kanban-stale-observation-design.md) · [plan（已执行，待真机验收）](../superpowers/plans/2026-11-03-kanban-stale-observation-impl.md) |
| desktop | 133 / 156 | [详情](./desktop.md) · [compact 跨会话 Send 隔离修复 spec](../superpowers/specs/2026-11-09-compact-send-isolation-design.md) · [运行参数热更新](../superpowers/specs/2026-11-04-runtime-settings-live-apply-design.md) · [Session 完整性修复 spec rev2](../superpowers/specs/2026-11-08-session-completeness-repair-design.md) · [Plan 总入口](../superpowers/plans/2026-11-08-session-completeness-repair-impl.md) · [子 agent 执行面板设计](../superpowers/specs/2026-11-07-desktop-subagent-trace-redesign-design.md) · [复审](../superpowers/specs/2026-11-07-desktop-subagent-trace-redesign-review.md) · [plan](../superpowers/plans/2026-11-07-desktop-subagent-trace-redesign-impl.md) · [plan 复审](../superpowers/reviews/2026-11-07-desktop-subagent-trace-plan-review.md) · [iOS 识别二维码后黑屏修复 spec](../superpowers/specs/2026-11-07-ios-qr-recognition-black-screen-design.md) · [plan](../superpowers/plans/2026-11-07-ios-qr-recognition-black-screen-impl.md) · [审查记录](../superpowers/reports/2026-11-07-ios-qr-recognition-spec-review.md) · [plan 审查记录](../superpowers/reports/2026-11-07-ios-qr-recognition-plan-review.md) · [managed 生产接入 spec](../superpowers/specs/2026-11-05-managed-runtime-production-integration-design.md) · [接入 plan](../superpowers/plans/2026-11-05-managed-runtime-production-integration-impl.md) · [运行中插话设计](../superpowers/specs/2026-11-03-desktop-mid-turn-input-design.md) · [搜索框清除按钮修复 spec](../superpowers/specs/2026-11-05-sidebar-search-clear-design.md) · [plan](../superpowers/plans/2026-11-05-sidebar-search-clear-impl.md) · [旁问修复 spec](../superpowers/specs/2026-11-05-side-conversation-repair-design.md) · [修复 plan](../superpowers/plans/2026-11-05-side-conversation-repair-impl.md) · [转录窗口化 spec](../superpowers/specs/2026-11-06-transcript-window-design.md) · [plan](../superpowers/plans/2026-11-06-transcript-window.md) |
| permission | 9 / 9 | [详情](./permission.md) |
| ci-cd | 41 / 43 | [详情](./ci-cd.md) |
| tooling | 15 / 15 | [详情](./tooling.md) |
| magpie-agent-yi-agent | 5 / 5 | [详情](./magpie-agent-yi-agent.md) |
| remote-ssh | 31 / 34 | [详情](./remote-ssh.md) |
临时旁问（三端）：新需求见[一次性对话快照旁问 spec](../superpowers/specs/2026-11-09-side-snapshot-lifecycle-design.md)（规格已复核，[plan](../superpowers/plans/2026-11-09-side-snapshot-lifecycle-impl.md) Task 1–12 已实施、Task 13 自动检查通过；[审查记录](../superpowers/reviews/2026-11-09-side-snapshot-lifecycle-spec-review.md)、[验收记录](../superpowers/reviews/2026-11-09-side-snapshot-lifecycle-acceptance.md)）：固定当前对话快照、允许追问、退出销毁、再进入新环境；对应待办分别登记在 runtime、app-server、desktop 和 TUI。旧[设计基线](../superpowers/specs/2026-11-03-side-conversation-design.md)、[实施计划](../superpowers/plans/2026-11-03-side-conversation-impl.md)及[修复 spec](../superpowers/specs/2026-11-05-side-conversation-repair-design.md)/[修复 plan](../superpowers/plans/2026-11-05-side-conversation-repair-impl.md)保留为历史记录，其中“返回保留/后台续聊/文件及网络只读工具”的行为与验收拟由新规格覆盖；真实联合验收仍未完成。

本轮同步复核以上四模块计数：统计模块文件中的 checklist 条目（包括子项及已放弃项），排除明确指向其他主条目的“不重复计数”交叉引用；运行参数即时生效的主条目保留计数。已完成数变化只校正索引滞后，未将本次旁问待办标为完成。

## 已知问题

见 [bug-list](../bug-list.md)。

## 维护规则

- 每完成一组需求，**必须**同步更新对应模块文件与本索引的计数（见 CLAUDE.md "项目进度维护"）
- 状态只有三态：`[x]` 已完成、`[ ]` 未完成、`[-]` 已放弃；**不使用** `[~]` 进行中
- 每条 feature 必须带可验证的完成判据（代码位置或可执行命令），不要主观描述
- 新增 crate 时，**同一 PR** 内必须创建对应的模块文件并登记到本索引
