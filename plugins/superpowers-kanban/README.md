# Superpowers Kanban 插件

## 安装
1. 放入 `superpowers-kanban` 可执行文件（`cargo build -p superpowers-kanban-runner --release`，
   产物名为 `superpowers-kanban`）。
2. 放入 `superpowers-kanban` skill 目录（安装到 `~/.yi-agent/skills/superpowers-kanban/`）。
3. 把 `supervisors/superpowers-kanban.json` 复制到
   `<项目>/.yi-agent/supervisors/superpowers-kanban.json`，并按需改 `command` 为绝对路径。
4. 在 `<项目>/.yi-agent/preferences.json` 写入 `{"superpowers_kanban": true}`
   （或在 TUI/桌面里打开开关）。

## 运行前提
daemon 常驻（`yi-agent daemon start`）。daemon 会按清单与开关拉起 `superpowers-kanban run`
并守护它。

## 卸载
删除 `supervisors/superpowers-kanban.json`（daemon 随即停掉该进程），再删除本目录。
正在 daemon 中运行的会话会照常跑完，不影响主程序。

## 配置
`superpowers-kanban.toml` 放在 `<项目>/.yi-agent/superpowers-kanban/superpowers-kanban.toml`
（示例见本目录）。旧名 `kanban.toml` 仍会被读取（迁移期兼容），但**只读不改**。

## 加入看板
在 TUI 里 `/superpowers-kanban add <spec> <plan>`；桌面端用看板面板的入队动作。
宿主只把投递写进 `<项目>/.yi-agent/superpowers-kanban/inbox/`，插件每 tick 消费并入队。

## 迁移说明（旧名 → 新名）
旧布局仍被**读取**，不会被修改或删除：

| 旧 | 新 |
| --- | --- |
| 开关键 `superpowers_board` | `superpowers_kanban` |
| 状态目录 `.yi-agent/board/` | `.yi-agent/superpowers-kanban/` |
| 日历 `kanban.toml` | `superpowers-kanban.toml` |

确认新位置已就绪后，可自行删除旧文件。
