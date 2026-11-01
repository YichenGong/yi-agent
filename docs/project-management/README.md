# yi-agent 项目进度总览

## 状态图例
- [x] 已完成（有代码 + 有验证）
- [ ] 未完成
- [-] 已放弃（YAGNI）

## 模块索引

| 模块 | 完成 / 总计 | 详情 |
|---|---|---|
| yi-agent-core | 23 / 24 | [详情](./yi-agent-core.md) |
| yi-agent-llm | 12 / 15 | [详情](./yi-agent-llm.md) |
| yi-agent-tools | 19 / 19 | [详情](./yi-agent-tools.md) |
| yi-agent-skills | 9 / 9 | [详情](./yi-agent-skills.md) |
| yi-agent-tui | 40 / 41 | [详情](./yi-agent-tui.md) |
| yi-agent-run | 10 / 10 | [详情](./yi-agent-run.md) |
| yi-agent-cli | 7 / 7 | [详情](./yi-agent-cli.md) |
| yi-agent-web | 11 / 11 | [详情](./yi-agent-web.md) |
| permission | 9 / 9 | [详情](./permission.md) |
| ci-cd | 21 / 23 | [详情](./ci-cd.md) |
| tooling | 9 / 9 | [详情](./tooling.md) |
| yi-agent-mcp | 5 / 5 | [详情](./yi-agent-mcp.md) |
| yi-agent-store | 8 / 9 | [详情](./yi-agent-store.md) |
| yi-agent-flags | 9 / 9 | [详情](./yi-agent-flags.md) |
| yi-agent-runtime | 24 / 24 | [详情](./yi-agent-runtime.md) |
| yi-agent-subagent | 10 / 10 | [详情](./yi-agent-subagent.md) |
| yi-agent-app-server | 75 / 75 | [详情](./yi-agent-app-server.md) |
| subagent-runtime | 45 / 53 | [详情](./subagent-runtime.md) |
| superpowers-kanban | 18 / 18 | [详情](./superpowers-kanban.md) |
| desktop | 105 / 116 | [详情](./desktop.md) |
| magpie-agent | 5 / 5 | [详情](./magpie-agent.md) |

## 已知问题

见 [bug-list](../bug-list.md)。

## 维护规则

- 每完成一组需求，**必须**同步更新对应模块文件与本索引的计数（见 CLAUDE.md "项目进度维护"）
- 状态只有三态：`[x]` 已完成、`[ ]` 未完成、`[-]` 已放弃；**不使用** `[~]` 进行中
- 每条 feature 必须带可验证的完成判据（代码位置或可执行命令），不要主观描述
- 新增 crate 时，**同一 PR** 内必须创建对应的模块文件并登记到本索引
