# Spec 1：命名统一为 superpowers-kanban（含迁移）

日期：2026-10-01
状态：设计待评审

## 1. 目标

把看板相关的命名统一到 `superpowers-kanban`，消除"同一能力三个名字"（`kanban` / `board` / `superpowers-board`）的混乱，并保证**已有数据不丢**。

## 2. 改名映射

### 2.1 用户可见面与接线

| 类别 | 现状 | 改为 |
| --- | --- | --- |
| 插件目录 | `plugins/superpowers-board/` | `plugins/superpowers-kanban/` |
| 插件二进制 | `board-runner` | `superpowers-kanban` |
| supervisor 清单文件 | `supervisors/superpowers-board.json` | `supervisors/superpowers-kanban.json` |
| 清单 `name` | `superpowers-board` | `superpowers-kanban` |
| 清单 `switch_key` | `superpowers_board` | `superpowers_kanban` |
| TUI 命令 | `/kanban` | `/superpowers-kanban` |
| app-server RPC | `board/list`、`board/enqueue`、`board/switch/read`、`board/switch/write` | `superpowers-kanban/*`（见 §4 注） |

### 2.2 落盘状态（含迁移）

| 类别 | 现状 | 改为 |
| --- | --- | --- |
| 开关键（`preferences.json`） | `superpowers_board` | `superpowers_kanban` |
| 状态目录 | `<项目>/.yi-agent/board/` | `<项目>/.yi-agent/superpowers-kanban/` |
| 看板文件 | `board.json` | 不变（目录已承载身份） |
| 投递目录 | `inbox/` | 不变 |
| 日历配置 | `kanban.toml` | `superpowers-kanban.toml` |

### 2.3 代码内部标识

| 类别 | 现状 | 改为 |
| --- | --- | --- |
| 插件 crate | `board-core` / `board-ipc` / `board-runner` | `superpowers-kanban-core` / `-ipc` / `-runner` |
| 主程序 crate | `yi-agent-board-ui` | **不改**（见 §5） |
| TUI 模块 | `src/tui/board.rs` | `src/tui/superpowers_kanban.rs` |
| 桌面组件 | `BoardView.tsx` / `SettingsPanel.tsx` / `boardState.ts` / `boardSwitch.ts` | `SuperpowersKanbanView.tsx` / `SuperpowersKanbanSettings.tsx` / `superpowersKanbanState.ts` / `superpowersKanbanSwitch.ts` |

## 3. 迁移语义（非破坏性）

- **读**：先查新键 / 新目录；不存在则**回退**读旧键 `superpowers_board` / 旧目录 `.yi-agent/board` / 旧文件 `kanban.toml`。
- **写**：一律写**新**位置；**绝不修改、绝不删除**旧文件。
- **冲突**：新键存在时新键胜出，不再看旧键。
- 旧文件由用户确认后手工删除；README 写明这一点。

## 4. 兼容边界

- **TUI 命令**：`/kanban` 作为**别名**保留一段过渡期（打印一行"已更名为 /superpowers-kanban"），避免肌肉记忆直接失效；新名为主。
- **RPC**：`board/*` 四条的处置取决于 Spec 4。**若 Spec 4 先行**，这批 RPC 会被通用 `plugin/query` 取代，本 spec 就不必改它们；**若本 spec 先行**，则先改名为 `superpowers-kanban/*`，Spec 4 再替换。实施顺序（见 overview）选的是本 spec 先行，故此处按"先改名、后替换"执行，且不保留 `board/*` 别名（桌面端与 app-server 同仓同版本，无外部消费者）。
- **清单文件名**：改名后旧清单文件名不再被扫描；迁移时用户在 README 指引下重命名（或由 `superpowers-kanban` 的 on/off 重建）。此点需在实施计划中给出明确步骤。

## 5. 为什么不改 `yi-agent-board-ui`

`yi-agent-board-ui` 是**主程序**的 crate（`yi-agent-rs/crates/`，被 app-server 与 TUI 依赖），承载宿主侧"读状态 / 写投递 / 读写开关"。把它改名成插件品牌，等于把插件身份焊进主程序，与"主流程干净、插件解耦"相悖。它保持中性名 `board-ui`；Spec 4 落地后该 crate 将被整体删除。

## 6. 测试重点

1. 只有旧键 → 仍解析为 enabled；只有新键 → enabled；两者并存冲突 → 新胜。
2. 只有旧目录（含 `board.json` 与 `inbox/*.json`）→ 仍读得到；写入落到新目录。
3. 旧 `kanban.toml` 仍被读取；新 `superpowers-kanban.toml` 优先。
4. `/superpowers-kanban` 与其 `/kanban` 别名等价。
5. 改名后 supervisor 仍能按清单拉起插件进程（`switch_key` 生效）。
