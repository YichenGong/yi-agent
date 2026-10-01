# Spec 规划：Superpowers Kanban（插件化收尾）

日期：2026-10-01
状态：设计已逐项确认，待写实施计划

本轮讨论把「看板可折叠」这一条小需求，追成了一个**架构收尾**：让看板真正成为**可独立安装/卸载的插件**，并把命名统一到 `superpowers-kanban`。原因是调查发现主程序里仍留有看板实现（crate `yi-agent-board-ui` + app-server 的 4 条 `board/*` RPC），即"解耦"目前只是单向的。

## 已确认的决策（全部经用户逐项拍板）

| # | 决策 | 选择 |
| --- | --- | --- |
| 1 | CLI 归属 | 插件二进制，不进主程序 |
| 2 | 二进制名 | `superpowers-kanban`（同时提供 `run` 与 `add\|list\|on\|off`） |
| 3 | skill 分发 | 随插件分发，**手工安装**（不写脚本），INSTALL.md 指导模型 |
| 4 | skill 职责 | **只管入队**（spec/plan 必须已存在） |
| 5 | 桌面端 | **要有**建卡入口 |
| 6 | 命名 | 统一 `superpowers-kanban`，**含落盘键**，带兼容读取 + 迁移 |
| 7 | 改名深度 | 含 crate / 模块 / 组件名 |
| 8 | 解耦方向 | **B**：插件通过 IPC 暴露状态，主程序彻底不认识看板 |
| 9 | 插件接查询方式 | 插件自监听 socket，daemon 转发 |

## 拆分：5 份 spec（按依赖顺序）

| 顺序 | spec | 内容 | 风险 |
| --- | --- | --- | --- |
| 1 | `superpowers-kanban-rename` | 命名统一 + 兼容读取 + 迁移 | 中（有数据） |
| 2 | `superpowers-kanban-plugin-surface` | 插件二进制（run + add/list/on/off）、skill、INSTALL.md | 低 |
| 3 | `superpowers-kanban-desktop-enqueue` | 桌面端建卡入口 | 低 |
| 4 | `superpowers-kanban-decoupling` | 通用 `plugin/query` IPC；删除 `yi-agent-board-ui`；TUI/桌面改道 | **高**（动核心 IPC） |
| 5 | `board-collapsible`（已有草稿） | 可折叠面板；命名对齐 | 低 |

实施顺序建议 1 → 2 → 5 → 3 → 4（先立命名地基，快速见效项居中，最重的解耦最后，且解耦会改写 3 的落点）。

## 明确不做（本轮）

- 不写自动化安装脚本（用户选定手工 + INSTALL.md）。
- 不让 skill 承担 spec/plan 的生成（职责单一，复用既有 `brainstorming` / `writing-plans`）。
- 不为改名重写历史提交或旧文档正文（旧文档在 `docs/superpowers/` 下保留原样，仅新文档用新名）。

### 本次新发现、留待后续（Spec 2 实施期记录）

- **三个入队入口的校验强度不一致。** 同为"投递一张卡"，行为目前不同：

  | 入口 | 是否先校验 spec/plan | 失败表现 |
  | --- | --- | --- |
  | CLI `superpowers-kanban add` | 是（`validate_promotion`） | 立刻非零退出码 + 具名错误 |
  | TUI `/superpowers-kanban add` | 否 | 静默投递，等下一个 tick 由 runner 移进 `inbox/rejected/` |
  | 桌面端入队 | 否 | 同上 |

  CLI 是新的、校验最严；TUI 与桌面端是既有实现。**这不算 bug，但算不一致**：
  同一个错别字，命令行当场报错，界面上要等一个 tick（默认 60 秒）才在
  `inbox/rejected/` 里看到。要不要统一（让 TUI/桌面端也先校验，或反过来让 CLI
  只负责投递、校验一律交给 runner）尚未决定，故本轮不改。

  影响面：仅错误反馈的**时机**，不影响正确性——runner 的校验始终是最终防线。
  若决定统一，落点是 Spec 4（解耦）顺手改，因为那时投递路径会整体搬到插件侧。

- **`inbox/rejected/` 没有读路径。** 卡片被拒后只落一个原因文件，TUI/桌面/CLI 都不展示它。
  用户此时只能 `ls` 文件系统。本轮未加，因为需要先决定「拒绝原因算不算看板状态」——
  若算，它要进整体状态模型；若不算，它只是个日志。留待 Spec 4 一并定。

- **`list` 不显示 `board.json` 里非 queued 卡片之外的 `AwaitingMerge` 合并候选关系。**
  当前只打印每张卡自己的状态，不表达"这几张卡来自同一个 spec/plan、可以合并"。
  合并决定权在用户，但界面还没把候选关系摆出来。属于 Spec 3（桌面建卡）的界面范畴。
