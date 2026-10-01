# yi-agent 项目进度总览

## 状态图例
- [x] 已完成（有代码 + 有验证）
- [ ] 未完成
- [-] 已放弃（YAGNI）

## 模块索引

| 模块 | 完成 / 总计 | 详情 |
|---|---|---|
| yi-agent-core | 20 / 21 | [详情](./yi-agent-core.md) |
| yi-agent-llm | 6 / 9 | [详情](./yi-agent-llm.md) |
| yi-agent-tools | 14 / 14 | [详情](./yi-agent-tools.md) |
| yi-agent-skills | 8 / 8 | [详情](./yi-agent-skills.md) |
| yi-agent-tui | 30 / 32 | [详情](./yi-agent-tui.md) |
| yi-agent-run | 10 / 10 | [详情](./yi-agent-run.md) |
| yi-agent-web | 6 / 6 | [详情](./yi-agent-web.md) |
| permission | 9 / 9 | [详情](./permission.md) |
| ci-cd | 11 / 13 | [详情](./ci-cd.md) |
| tooling | 3 / 3 | [详情](./tooling.md) |
| yi-agent-mcp | 1 / 1 | [详情](./yi-agent-mcp.md) |
| yi-agent-store | 4 / 5 | [详情](./yi-agent-store.md) |
| yi-agent-runtime | 11 / 11 | [详情](./yi-agent-runtime.md) |
| yi-agent-subagent | 4 / 4 | [详情](./yi-agent-subagent.md) |
| yi-agent-app-server | 20 / 20 | [详情](./yi-agent-app-server.md) |
| subagent-runtime | 40 / 47 | [详情](./subagent-runtime.md) |
| desktop | 26 / 39 | [详情](./desktop.md) |

## 已知问题

见 [bug-list](../bug-list.md)。

## 维护规则

- 每完成一组需求，**必须**同步更新对应模块文件与本索引的计数（见 CLAUDE.md "项目进度维护"）
- 状态只有三态：`[x]` 已完成、`[ ]` 未完成、`[-]` 已放弃；**不使用** `[~]` 进行中
- 每条 feature 必须带可验证的完成判据（代码位置或可执行命令），不要主观描述
- 新增 crate 时，**同一 PR** 内必须创建对应的模块文件并登记到本索引
