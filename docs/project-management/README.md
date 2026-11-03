# yi-agent 项目进度总览

## 状态图例
- [x] 已完成（有代码 + 有验证）
- [ ] 未完成
- [-] 已放弃（YAGNI）

## 模块索引

| 模块 | 完成 / 总计 | 详情 |
|---|---|---|
| yi-agent-core | 23 / 25 | [详情](./yi-agent-core.md) |
| yi-agent-llm | 12 / 15 | [详情](./yi-agent-llm.md) |
| yi-agent-tools | 19 / 20 | [详情](./yi-agent-tools.md) |
| yi-agent-skills | 9 / 9 | [详情](./yi-agent-skills.md) |
| yi-agent-tui | 44 / 46 | [详情](./yi-agent-tui.md) |
| yi-agent-run | 10 / 10 | [详情](./yi-agent-run.md) |
| yi-agent-cli | 7 / 7 | [详情](./yi-agent-cli.md) |
| yi-agent-web | 12 / 12 | [详情](./yi-agent-web.md) |
| yi-agent-relay | 2 / 2 | [详情](./yi-agent-relay.md) |
| permission | 9 / 9 | [详情](./permission.md) |
| ci-cd | 36 / 36 | [详情](./ci-cd.md) |
| tooling | 11 / 11 | [详情](./tooling.md) |
| yi-agent-mcp | 5 / 5 | [详情](./yi-agent-mcp.md) |
| yi-agent-store | 8 / 9 | [详情](./yi-agent-store.md) |
| yi-agent-flags | 9 / 9 | [详情](./yi-agent-flags.md) |
| yi-agent-runtime | 24 / 27 | [详情](./yi-agent-runtime.md) |
| yi-agent-power | 8 / 12 | [详情](./yi-agent-power.md) |
| yi-agent-subagent | 10 / 10 | [详情](./yi-agent-subagent.md) |
| yi-agent-app-server | 81 / 82 | [详情](./yi-agent-app-server.md) |
| subagent-runtime | 45 / 53 | [详情](./subagent-runtime.md) |
| superpowers-kanban | 19 / 20 | [详情](./superpowers-kanban.md) |
| desktop | 116 / 127 | [详情](./desktop.md) · [运行中插话设计](../superpowers/specs/2026-11-03-desktop-mid-turn-input-design.md) |
| magpie-agent | 5 / 5 | [详情](./magpie-agent.md) |
| remote-ssh | 31 / 34 | [详情](./remote-ssh.md) |

临时旁问（三端，功能未实现）：[设计基线](../superpowers/specs/2026-11-03-side-conversation-design.md) · [实施计划](../superpowers/plans/2026-11-03-side-conversation-impl.md)。计划细分21个任务，不重复增加六个模块的需求总数。

## 已知问题

见 [bug-list](../bug-list.md)。

## 维护规则

- 每完成一组需求，**必须**同步更新对应模块文件与本索引的计数（见 CLAUDE.md "项目进度维护"）
- 状态只有三态：`[x]` 已完成、`[ ]` 未完成、`[-]` 已放弃；**不使用** `[~]` 进行中
- 每条 feature 必须带可验证的完成判据（代码位置或可执行命令），不要主观描述
- 新增 crate 时，**同一 PR** 内必须创建对应的模块文件并登记到本索引
