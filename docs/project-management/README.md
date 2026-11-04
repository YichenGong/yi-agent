# yi-agent 项目进度总览

## 状态图例
- [x] 已完成（有代码 + 有验证）
- [ ] 未完成
- [-] 已放弃（YAGNI）

## 模块索引

| 模块 | 完成 / 总计 | 详情 |
|---|---|---|
| yi-agent-core | 27 / 28 | [详情](./yi-agent-core.md) |
| yi-agent-llm | 12 / 15 | [详情](./yi-agent-llm.md) |
| yi-agent-tools | 24 / 24 | [详情](./yi-agent-tools.md) |
| yi-agent-skills | 9 / 9 | [详情](./yi-agent-skills.md) |
| yi-agent-tui | 46 / 47 | [详情](./yi-agent-tui.md) |
| yi-agent-run | 11 / 11 | [详情](./yi-agent-run.md) |
| yi-agent-cli | 9 / 9 | [详情](./yi-agent-cli.md) |
| yi-agent-web | 12 / 12 | [详情](./yi-agent-web.md) |
| yi-agent-relay | 2 / 2 | [详情](./yi-agent-relay.md) |
| yi-agent-mcp | 6 / 6 | [详情](./yi-agent-mcp.md) |
| yi-agent-store | 13 / 14 | [详情](./yi-agent-store.md) |
| yi-agent-flags | 9 / 9 | [详情](./yi-agent-flags.md) |
| yi-agent-runtime | 27 / 28 | [详情](./yi-agent-runtime.md) · [清单刷新 plan](../superpowers/plans/2026-11-03-supervisor-manifest-describe-bounds-impl.md) |
| yi-agent-power | 12 / 12 | [详情](./yi-agent-power.md) |
| yi-agent-plugins | 86 / 86 | [详情](./yi-agent-plugins.md) |
| yi-agent-subagent | 12 / 12 | [详情](./yi-agent-subagent.md) |
| yi-agent-app-server | 85 / 85 | [详情](./yi-agent-app-server.md) |
| subagent-runtime | 62 / 74 | [详情](./subagent-runtime.md) · [前台根生命周期 spec](../superpowers/specs/2026-11-03-foreground-root-lifecycle-design.md) · [plan](../superpowers/plans/2026-11-03-foreground-root-lifecycle-impl.md) |
| yi-agent-lifecycle | 19 / 19 | [详情](./yi-agent-lifecycle.md) |
| superpowers-kanban | 19 / 22 | [详情](./superpowers-kanban.md) · [查询 IPC 修复 spec](../superpowers/specs/2026-11-03-kanban-query-io-design.md) · [会话观测误报 spec](../superpowers/specs/2026-11-03-kanban-stale-observation-design.md) · [plan（已执行，待真机验收）](../superpowers/plans/2026-11-03-kanban-stale-observation-impl.md) |
| desktop | 121 / 132 | [详情](./desktop.md) · [运行中插话设计](../superpowers/specs/2026-11-03-desktop-mid-turn-input-design.md) |
| permission | 9 / 9 | [详情](./permission.md) |
| ci-cd | 41 / 43 | [详情](./ci-cd.md) |
| tooling | 11 / 11 | [详情](./tooling.md) |
| magpie-agent | 5 / 5 | [详情](./magpie-agent.md) |
| remote-ssh | 31 / 34 | [详情](./remote-ssh.md) |
临时旁问（三端，功能未实现）：[设计基线](../superpowers/specs/2026-11-03-side-conversation-design.md) · [实施计划](../superpowers/plans/2026-11-03-side-conversation-impl.md)。计划细分21个任务，不重复增加六个模块的需求总数。>>>>>>> kanban/2026-11-02-kanban-independent-release-design-2026-11-02-kanban-independent-release

## 已知问题

见 [bug-list](../bug-list.md)。

## 维护规则

- 每完成一组需求，**必须**同步更新对应模块文件与本索引的计数（见 CLAUDE.md "项目进度维护"）
- 状态只有三态：`[x]` 已完成、`[ ]` 未完成、`[-]` 已放弃；**不使用** `[~]` 进行中
- 每条 feature 必须带可验证的完成判据（代码位置或可执行命令），不要主观描述
- 新增 crate 时，**同一 PR** 内必须创建对应的模块文件并登记到本索引
